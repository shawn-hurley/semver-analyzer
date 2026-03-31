//! LLM command invocation and response parsing.
//!
//! Handles running external LLM commands and extracting structured JSON
//! from their free-text output. Uses multiple strategies for JSON extraction
//! to handle different LLM output formats.

use anyhow::{Context, Result};
use regex::Regex;
use semver_analyzer_core::{BreakingVerdict, ExpectedChild, FunctionSpec, RemovalDisposition};
use serde::Deserialize;
use std::process::Command;
use std::sync::LazyLock;

/// Run an LLM command with the given prompt and return the output.
///
/// The command string is split on whitespace and the prompt is appended
/// as the final argument. The command is expected to return a response
/// on stdout.
///
/// Examples:
/// - `"goose run --no-session -q -t"` → `goose run --no-session -q -t "<prompt>"`
/// - `"opencode run"` → `opencode run "<prompt>"`
pub fn run_llm_command(command: &str, prompt: &str, timeout_secs: u64) -> Result<String> {
    let parts: Vec<&str> = command.split_whitespace().collect();
    if parts.is_empty() {
        anyhow::bail!("Empty LLM command");
    }

    let program = parts[0];
    let args = &parts[1..];

    let mut child = Command::new(program)
        .args(args)
        .arg(prompt)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to execute LLM command: {}", command))?;

    // Wait with timeout
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let start = std::time::Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Process finished
                let output = child.wait_with_output()?;
                if !status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!(
                        "LLM command failed (exit code {:?}): {}",
                        status.code(),
                        stderr
                    );
                }
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                if stdout.trim().is_empty() {
                    anyhow::bail!("LLM command returned empty output");
                }
                // Goose truncates large stdout and writes the full output
                // to a temp file, e.g.:
                //   ... (57 more lines → /tmp/goose-xxx.txt)
                // Detect this and read the temp file to get the full response.
                return Ok(resolve_goose_overflow(&stdout));
            }
            Ok(None) => {
                // Still running
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    anyhow::bail!("LLM command timed out after {} seconds", timeout_secs);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                anyhow::bail!("Error waiting for LLM command: {}", e);
            }
        }
    }
}

/// Goose CLI truncates large outputs to stdout and saves the full text to a
/// temp file. The truncation marker looks like:
///   `... (57 more lines → /var/folders/.../goose-XxYz.txt)`
/// This function detects the marker, reads the overflow file, and reconstructs
/// the full response by replacing the truncation line with the file contents.
fn resolve_goose_overflow(stdout: &str) -> String {
    // Pattern: "... (N more lines → <path>)"
    // The marker is always on a line by itself, near the end of stdout.
    static OVERFLOW_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^\.\.\. \(\d+ more lines → (.+)\)\s*$").unwrap());

    let Some(caps) = OVERFLOW_RE.captures(stdout) else {
        return stdout.to_string();
    };

    let overflow_path = caps.get(1).unwrap().as_str();
    let overflow_content = match std::fs::read_to_string(overflow_path) {
        Ok(content) => content,
        Err(e) => {
            tracing::warn!(
                path = overflow_path,
                %e,
                "failed to read goose overflow file, using truncated stdout"
            );
            return stdout.to_string();
        }
    };

    // The overflow file contains the FULL response (without code fences).
    // Use it directly since it's complete.
    tracing::debug!(
        path = overflow_path,
        overflow_lines = overflow_content.lines().count(),
        "resolved goose overflow file"
    );
    overflow_content
}

/// Parse a `FunctionSpec` from LLM output.
///
/// Tries multiple strategies:
/// 1. Fenced JSON block (```json ... ```)
/// 2. Raw JSON object ({ ... })
/// 3. JSON embedded in prose text
pub fn parse_function_spec(response: &str) -> Result<FunctionSpec> {
    let json_str = extract_json(response).context("Could not extract JSON from LLM response")?;

    serde_json::from_str(&json_str).with_context(|| {
        format!(
            "Failed to parse FunctionSpec from JSON. Extracted:\n{}",
            truncate(&json_str, 500)
        )
    })
}

/// Parse a `BreakingVerdict` from LLM output.
pub fn parse_breaking_verdict(response: &str) -> Result<BreakingVerdict> {
    let json_str =
        extract_json(response).context("Could not extract JSON from LLM response for verdict")?;

    serde_json::from_str(&json_str).with_context(|| {
        format!(
            "Failed to parse BreakingVerdict from JSON. Extracted:\n{}",
            truncate(&json_str, 500)
        )
    })
}

/// Parse a boolean propagation result from LLM output.
///
/// Looks for clear yes/no signals. Defaults to `true` (conservative:
/// assume propagation) if the response is ambiguous.
pub fn parse_propagation_result(response: &str) -> Result<bool> {
    let lower = response.to_lowercase();

    // Try to parse as JSON first
    if let Some(json_str) = extract_json(response) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json_str) {
            if let Some(propagates) = val.get("propagates").and_then(|v| v.as_bool()) {
                return Ok(propagates);
            }
            if let Some(propagates) = val.get("is_breaking").and_then(|v| v.as_bool()) {
                return Ok(propagates);
            }
        }
    }

    // Heuristic text matching
    if lower.contains("does not propagate")
        || lower.contains("does not affect")
        || lower.contains("absorbs the change")
        || lower.contains("masks the change")
        || lower.contains("no propagation")
    {
        return Ok(false);
    }

    if lower.contains("propagates")
        || lower.contains("is affected")
        || lower.contains("breaks the caller")
        || lower.contains("yes, the change propagates")
    {
        return Ok(true);
    }

    // Conservative default: assume propagation
    Ok(true)
}

// ── File-level behavioral change parsing ────────────────────────────────

/// A single behavioral change from the file-level LLM response.
#[derive(Debug, Clone, Deserialize)]
pub struct FileBehavioralChange {
    pub symbol: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Sub-category: dom_structure, css_class, css_variable, accessibility,
    /// default_value, logic_change, data_attribute, render_output.
    #[serde(default)]
    pub category: Option<String>,
    pub description: String,
    /// Whether this change only affects internal rendering and does NOT
    /// require consumer code changes.
    #[serde(default)]
    pub is_internal_only: Option<bool>,
}

fn default_kind() -> String {
    "class".to_string()
}

/// A single API type-level change from the file-level LLM response.
#[derive(Debug, Clone, Deserialize)]
pub struct FileApiChange {
    pub symbol: String,
    #[serde(default = "default_change")]
    pub change: String,
    pub description: String,
    /// Why a removed prop was removed and where its functionality went.
    #[serde(default)]
    pub removal_disposition: Option<RemovalDisposition>,
    /// HTML element the component renders (e.g., "ol", "div").
    #[serde(default)]
    pub renders_element: Option<String>,
}

fn default_change() -> String {
    "signature_changed".to_string()
}

/// Parsed response from the file-level analysis prompt.
#[derive(Debug, Clone, Deserialize)]
pub struct FileBehavioralResponse {
    #[serde(default)]
    pub breaking_behavioral_changes: Vec<FileBehavioralChange>,
    #[serde(default)]
    pub breaking_api_changes: Vec<FileApiChange>,
}

/// Parse file-level changes (behavioral + API) from LLM output.
pub fn parse_file_behavioral_response(
    response: &str,
) -> Result<(Vec<FileBehavioralChange>, Vec<FileApiChange>)> {
    let json_str = extract_json(response)
        .context("Could not extract JSON from LLM response for file analysis")?;

    let parsed: FileBehavioralResponse = serde_json::from_str(&json_str).with_context(|| {
        format!(
            "Failed to parse FileBehavioralResponse from JSON. Extracted:\n{}",
            truncate(&json_str, 500)
        )
    })?;

    Ok((
        parsed.breaking_behavioral_changes,
        parsed.breaking_api_changes,
    ))
}

// ── JSON Extraction ─────────────────────────────────────────────────────

/// Regex for fenced JSON blocks.
static FENCED_JSON_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"```(?:json)?\s*\n([\s\S]*?)\n```").unwrap());

/// Regex for standalone JSON objects.
static JSON_OBJECT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{[\s\S]*\}").unwrap());

/// Extract JSON from LLM output using multiple strategies.
///
/// Strategy order (first match wins):
/// 1. Fenced JSON block: ```json\n{...}\n```
/// 2. Last fenced block (if multiple)
/// 3. Largest `{...}` substring
fn extract_json(text: &str) -> Option<String> {
    // Strategy 1: Fenced JSON block
    let fenced_matches: Vec<_> = FENCED_JSON_RE
        .captures_iter(text)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().trim().to_string()))
        .collect();

    if let Some(last) = fenced_matches.last() {
        return Some(last.clone());
    }

    // Strategy 2: Find the largest JSON object in the text
    let mut best: Option<String> = None;
    let mut best_len = 0;

    for mat in JSON_OBJECT_RE.find_iter(text) {
        let candidate = mat.as_str();
        // Validate it's actually parseable JSON
        if serde_json::from_str::<serde_json::Value>(candidate).is_ok()
            && candidate.len() > best_len
        {
            best = Some(candidate.to_string());
            best_len = candidate.len();
        }
    }

    if best.is_some() {
        return best;
    }

    // Strategy 3: Try to find a JSON object by brace matching
    if let Some(start) = text.find('{') {
        let mut depth = 0;
        let mut in_string = false;
        let mut escape = false;

        for (i, ch) in text[start..].char_indices() {
            if escape {
                escape = false;
                continue;
            }
            match ch {
                '\\' if in_string => escape = true,
                '"' => in_string = !in_string,
                '{' if !in_string => depth += 1,
                '}' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        let json_str = &text[start..start + i + 1];
                        if serde_json::from_str::<serde_json::Value>(json_str).is_ok() {
                            return Some(json_str.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    None
}

/// Truncate a string for error messages.
fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..max_len]
    }
}

// ── Composition pattern response parsing ──────────────────────────────

/// Response from composition pattern analysis of test/example diffs.
#[derive(Debug, Clone, Deserialize)]
pub struct CompositionPatternResponse {
    #[serde(default)]
    pub composition_changes: Vec<LlmCompositionChange>,
}

/// A single composition pattern change from the LLM.
#[derive(Debug, Clone, Deserialize)]
pub struct LlmCompositionChange {
    pub component: String,
    pub old_parent: Option<String>,
    pub new_parent: Option<String>,
    #[serde(default)]
    pub description: String,
}

/// Parse the LLM response for composition pattern analysis (standalone call).
pub fn parse_composition_pattern_response(response: &str) -> Result<Vec<LlmCompositionChange>> {
    let json_str =
        extract_json(response).with_context(|| "No JSON found in composition pattern response")?;
    let parsed: CompositionPatternResponse =
        serde_json::from_str(&json_str).with_context(|| {
            format!(
                "Failed to parse composition pattern response: {}",
                truncate(&json_str, 200)
            )
        })?;
    Ok(parsed.composition_changes)
}

/// Extract composition_pattern_changes from a file behavioral response.
///
/// This is a best-effort extraction -- the field may not be present if the
/// LLM didn't include it (backward compat with older models).
pub fn parse_composition_from_file_response(response: &str) -> Result<Vec<LlmCompositionChange>> {
    let json_str = match extract_json(response) {
        Some(s) => s,
        None => return Ok(vec![]),
    };
    // Try parsing the full response structure with composition field
    #[derive(Deserialize)]
    struct WithComposition {
        #[serde(default)]
        composition_pattern_changes: Vec<LlmCompositionChange>,
    }
    let parsed: WithComposition = serde_json::from_str(&json_str).unwrap_or(WithComposition {
        composition_pattern_changes: vec![],
    });
    Ok(parsed.composition_pattern_changes)
}

// ── Rename inference response parsing ─────────────────────────────────

/// A single constant rename pattern from the LLM response.
#[derive(Debug, Clone, Deserialize)]
pub struct LlmConstantRenamePattern {
    #[serde(alias = "match")]
    pub match_regex: String,
    pub replace: String,
}

/// A single interface rename mapping from the LLM response.
#[derive(Debug, Clone, Deserialize)]
pub struct LlmInterfaceRenameMapping {
    pub old_name: String,
    pub new_name: String,
    #[serde(default = "default_confidence")]
    pub confidence: String,
    #[serde(default)]
    pub reason: String,
}

fn default_confidence() -> String {
    "medium".to_string()
}

/// Parse the LLM response for constant rename pattern inference.
/// Returns a list of regex match/replace patterns.
pub fn parse_constant_rename_response(response: &str) -> Result<Vec<LlmConstantRenamePattern>> {
    let json_str = extract_json(response)
        .with_context(|| "No JSON found in constant rename inference response")?;
    let patterns: Vec<LlmConstantRenamePattern> =
        serde_json::from_str(&json_str).with_context(|| {
            format!(
                "Failed to parse constant rename patterns: {}",
                truncate(&json_str, 200)
            )
        })?;
    Ok(patterns)
}

/// Parse the LLM response for interface rename mapping inference.
/// Returns a list of old_name → new_name mappings.
pub fn parse_interface_rename_response(response: &str) -> Result<Vec<LlmInterfaceRenameMapping>> {
    let json_str = extract_json(response)
        .with_context(|| "No JSON found in interface rename inference response")?;
    let mappings: Vec<LlmInterfaceRenameMapping> =
        serde_json::from_str(&json_str).with_context(|| {
            format!(
                "Failed to parse interface rename mappings: {}",
                truncate(&json_str, 200)
            )
        })?;
    Ok(mappings)
}

// ── Hierarchy inference response parsing ─────────────────────────────

/// The top-level LLM hierarchy response.
#[derive(Debug, Clone, Deserialize)]
pub struct LlmHierarchyResponse {
    pub components: std::collections::HashMap<String, LlmComponentHierarchy>,
}

/// A single component's hierarchy entry from the LLM.
#[derive(Debug, Clone, Deserialize)]
pub struct LlmComponentHierarchy {
    #[serde(default)]
    pub expected_children: Vec<ExpectedChild>,
}

// ── CSS Suffix Rename Response Parsing ───────────────────────────────────

/// A single suffix rename from the LLM response.
#[derive(Debug, Clone, Deserialize)]
pub struct LlmSuffixRename {
    pub from: String,
    pub to: String,
}

/// The top-level LLM suffix rename response.
#[derive(Debug, Clone, Deserialize)]
struct LlmSuffixRenameResponse {
    pub renames: Vec<LlmSuffixRename>,
}

/// Parse the LLM response for CSS suffix rename inference.
/// Returns a list of (old_suffix, new_suffix) pairs.
pub fn parse_suffix_rename_response(response: &str) -> Result<Vec<LlmSuffixRename>> {
    let json_str = extract_json(response)
        .with_context(|| "No JSON found in suffix rename inference response")?;
    let parsed: LlmSuffixRenameResponse = serde_json::from_str(&json_str).with_context(|| {
        format!(
            "Failed to parse suffix rename response: {}",
            truncate(&json_str, 300)
        )
    })?;
    Ok(parsed.renames)
}

/// Parse the LLM response for hierarchy inference.
/// Returns a map of component name → expected children.
pub fn parse_hierarchy_response(
    response: &str,
) -> Result<std::collections::HashMap<String, Vec<ExpectedChild>>> {
    let json_str =
        extract_json(response).with_context(|| "No JSON found in hierarchy inference response")?;
    let parsed: LlmHierarchyResponse = serde_json::from_str(&json_str).with_context(|| {
        format!(
            "Failed to parse hierarchy response: {}",
            truncate(&json_str, 300)
        )
    })?;
    Ok(parsed
        .components
        .into_iter()
        .map(|(name, h)| (name, h.expected_children))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── JSON extraction tests ───────────────────────────────────────

    #[test]
    fn extract_fenced_json() {
        let input = r#"Here is the spec:

```json
{
  "preconditions": [],
  "postconditions": [{"condition": "always", "returns": "42"}],
  "error_behavior": [],
  "side_effects": [],
  "notes": []
}
```

That's the spec."#;

        let json = extract_json(input).unwrap();
        let spec: FunctionSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec.postconditions.len(), 1);
        assert_eq!(spec.postconditions[0].returns, "42");
    }

    #[test]
    fn extract_raw_json() {
        let input = r#"The function spec is: {"preconditions": [], "postconditions": [], "error_behavior": [], "side_effects": [], "notes": ["simple function"]}"#;

        let json = extract_json(input).unwrap();
        let spec: FunctionSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec.notes.len(), 1);
    }

    #[test]
    fn extract_json_with_prose() {
        let input = r#"After analyzing the function, I found:

{
  "preconditions": [
    {"parameter": "email", "condition": "must be non-empty", "on_violation": "throws TypeError"}
  ],
  "postconditions": [],
  "error_behavior": [],
  "side_effects": [],
  "notes": []
}

The function validates email addresses."#;

        let json = extract_json(input).unwrap();
        let spec: FunctionSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec.preconditions.len(), 1);
        assert_eq!(spec.preconditions[0].parameter, "email");
    }

    #[test]
    fn extract_json_prefers_fenced() {
        let input = r#"Small json: {"notes": ["wrong"]}

```json
{"preconditions": [], "postconditions": [], "error_behavior": [], "side_effects": [], "notes": ["correct"]}
```"#;

        let json = extract_json(input).unwrap();
        let spec: FunctionSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec.notes, vec!["correct"]);
    }

    #[test]
    fn extract_json_returns_none_for_no_json() {
        assert!(extract_json("No JSON here at all").is_none());
        assert!(extract_json("").is_none());
    }

    // ── FunctionSpec parsing tests ──────────────────────────────────

    #[test]
    fn parse_spec_from_fenced_block() {
        let response = r#"```json
{
  "preconditions": [],
  "postconditions": [{"condition": "valid input", "returns": "processed string"}],
  "error_behavior": [{"trigger": "empty input", "error_type": "Error"}],
  "side_effects": [],
  "notes": []
}
```"#;

        let spec = parse_function_spec(response).unwrap();
        assert_eq!(spec.postconditions.len(), 1);
        assert_eq!(spec.error_behavior.len(), 1);
    }

    // ── Propagation result parsing ──────────────────────────────────

    #[test]
    fn parse_propagation_json() {
        let response = r#"{"propagates": false}"#;
        assert!(!parse_propagation_result(response).unwrap());

        let response = r#"{"propagates": true}"#;
        assert!(parse_propagation_result(response).unwrap());
    }

    #[test]
    fn parse_propagation_text() {
        assert!(!parse_propagation_result("The caller does not propagate the change").unwrap());
        assert!(!parse_propagation_result("It absorbs the change").unwrap());
        assert!(parse_propagation_result("The change propagates to the caller").unwrap());
    }

    #[test]
    fn parse_propagation_default_conservative() {
        // Ambiguous response defaults to true (conservative)
        assert!(parse_propagation_result("I'm not sure about this one").unwrap());
    }

    // ── BreakingVerdict parsing ─────────────────────────────────────

    // ── File behavioral response parsing ────────────────────────────

    #[test]
    fn parse_file_behavioral_empty() {
        let response = r#"```json
{"breaking_behavioral_changes": [], "breaking_api_changes": []}
```"#;
        let (beh, api) = parse_file_behavioral_response(response).unwrap();
        assert!(beh.is_empty());
        assert!(api.is_empty());
    }

    #[test]
    fn parse_file_behavioral_with_changes() {
        let response = r#"```json
{
  "breaking_behavioral_changes": [
    {
      "symbol": "Modal",
      "kind": "class",
      "description": "Component now renders a <section> instead of <div>"
    },
    {
      "symbol": "closeModal",
      "kind": "function",
      "description": "No longer emits 'beforeClose' event"
    }
  ],
  "breaking_api_changes": [
    {
      "symbol": "ModalProps.size",
      "change": "type_changed",
      "description": "Type narrowed from string to union"
    }
  ]
}
```"#;
        let (beh, api) = parse_file_behavioral_response(response).unwrap();
        assert_eq!(beh.len(), 2);
        assert_eq!(beh[0].symbol, "Modal");
        assert_eq!(beh[0].kind, "class");
        assert!(beh[0].description.contains("section"));
        assert_eq!(beh[1].symbol, "closeModal");
        assert_eq!(beh[1].kind, "function");
        assert_eq!(api.len(), 1);
        assert_eq!(api[0].symbol, "ModalProps.size");
        assert_eq!(api[0].change, "type_changed");
    }

    #[test]
    fn parse_file_behavioral_default_kind() {
        let response =
            r#"{"breaking_behavioral_changes": [{"symbol": "Foo", "description": "changed"}]}"#;
        let (beh, _api) = parse_file_behavioral_response(response).unwrap();
        assert_eq!(beh[0].kind, "class");
    }

    #[test]
    fn parse_file_no_api_field_ok() {
        // Old-format response without breaking_api_changes should still work
        let response = r#"{"breaking_behavioral_changes": []}"#;
        let (beh, api) = parse_file_behavioral_response(response).unwrap();
        assert!(beh.is_empty());
        assert!(api.is_empty());
    }

    // ── Tier 1 structured field parsing ─────────────────────────────

    #[test]
    fn parse_removal_disposition_moved_to_child() {
        let response = r#"```json
{
  "breaking_behavioral_changes": [],
  "breaking_api_changes": [
    {
      "symbol": "ModalProps.actions",
      "change": "removed",
      "description": "actions prop removed, pass as children of ModalFooter",
      "removal_disposition": {
        "type": "moved_to_related_type",
        "target_type": "ModalFooter",
        "mechanism": "children"
      }
    }
  ]
}
```"#;
        let (_beh, api) = parse_file_behavioral_response(response).unwrap();
        assert_eq!(api.len(), 1);
        assert_eq!(api[0].symbol, "ModalProps.actions");
        let disp = api[0]
            .removal_disposition
            .as_ref()
            .expect("Should have disposition");
        match disp {
            RemovalDisposition::MovedToRelatedType {
                target_type,
                mechanism,
            } => {
                assert_eq!(target_type, "ModalFooter");
                assert_eq!(mechanism, "children");
            }
            _ => panic!("Expected MovedToRelatedType, got {:?}", disp),
        }
    }

    #[test]
    fn parse_removal_disposition_moved_to_child_as_prop() {
        let response = r#"```json
{
  "breaking_behavioral_changes": [],
  "breaking_api_changes": [
    {
      "symbol": "ModalProps.title",
      "change": "removed",
      "description": "title prop moved to ModalHeader",
      "removal_disposition": {
        "type": "moved_to_related_type",
        "target_type": "ModalHeader",
        "mechanism": "prop"
      }
    }
  ]
}
```"#;
        let (_beh, api) = parse_file_behavioral_response(response).unwrap();
        let disp = api[0].removal_disposition.as_ref().unwrap();
        match disp {
            RemovalDisposition::MovedToRelatedType {
                target_type,
                mechanism,
            } => {
                assert_eq!(target_type, "ModalHeader");
                assert_eq!(mechanism, "prop");
            }
            _ => panic!("Expected MovedToRelatedType, got {:?}", disp),
        }
    }

    #[test]
    fn parse_removal_disposition_replaced_by_member() {
        let response = r#"```json
{
  "breaking_behavioral_changes": [],
  "breaking_api_changes": [
    {
      "symbol": "ButtonProps.isFlat",
      "change": "removed",
      "description": "isFlat replaced by isPlain",
      "removal_disposition": {
        "type": "replaced_by_member",
        "new_member": "isPlain"
      }
    }
  ]
}
```"#;
        let (_beh, api) = parse_file_behavioral_response(response).unwrap();
        let disp = api[0].removal_disposition.as_ref().unwrap();
        match disp {
            RemovalDisposition::ReplacedByMember { new_member } => {
                assert_eq!(new_member, "isPlain");
            }
            _ => panic!("Expected ReplacedByMember, got {:?}", disp),
        }
    }

    #[test]
    fn parse_removal_disposition_truly_removed() {
        let response = r#"```json
{
  "breaking_behavioral_changes": [],
  "breaking_api_changes": [
    {
      "symbol": "ModalProps.showClose",
      "change": "removed",
      "description": "showClose removed, close button now controlled by onClose presence",
      "removal_disposition": {"type": "truly_removed"}
    }
  ]
}
```"#;
        let (_beh, api) = parse_file_behavioral_response(response).unwrap();
        let disp = api[0].removal_disposition.as_ref().unwrap();
        assert!(matches!(disp, RemovalDisposition::TrulyRemoved));
    }

    #[test]
    fn parse_removal_disposition_made_automatic() {
        let response = r#"```json
{
  "breaking_behavioral_changes": [],
  "breaking_api_changes": [
    {
      "symbol": "SelectProps.isDynamic",
      "change": "removed",
      "description": "isDynamic now inferred automatically",
      "removal_disposition": {"type": "made_automatic"}
    }
  ]
}
```"#;
        let (_beh, api) = parse_file_behavioral_response(response).unwrap();
        let disp = api[0].removal_disposition.as_ref().unwrap();
        assert!(matches!(disp, RemovalDisposition::MadeAutomatic));
    }

    #[test]
    fn parse_removal_disposition_null_is_none() {
        let response = r#"```json
{
  "breaking_behavioral_changes": [],
  "breaking_api_changes": [
    {
      "symbol": "FooProps.bar",
      "change": "removed",
      "description": "bar removed",
      "removal_disposition": null
    }
  ]
}
```"#;
        let (_beh, api) = parse_file_behavioral_response(response).unwrap();
        assert!(api[0].removal_disposition.is_none());
    }

    #[test]
    fn parse_removal_disposition_missing_is_none() {
        // Backward compat: old responses without the field should still parse
        let response = r#"```json
{
  "breaking_behavioral_changes": [],
  "breaking_api_changes": [
    {
      "symbol": "FooProps.bar",
      "change": "removed",
      "description": "bar removed"
    }
  ]
}
```"#;
        let (_beh, api) = parse_file_behavioral_response(response).unwrap();
        assert!(api[0].removal_disposition.is_none());
    }

    #[test]
    fn parse_is_internal_only() {
        let response = r#"```json
{
  "breaking_behavioral_changes": [
    {
      "symbol": "ClipboardCopyButton",
      "kind": "class",
      "category": "render_output",
      "description": "CopyIcon now passed via icon prop internally",
      "is_internal_only": true
    },
    {
      "symbol": "Modal",
      "kind": "class",
      "category": "dom_structure",
      "description": "Modal no longer renders ModalBoxBody wrapper",
      "is_internal_only": false
    }
  ],
  "breaking_api_changes": []
}
```"#;
        let (beh, _api) = parse_file_behavioral_response(response).unwrap();
        assert_eq!(beh.len(), 2);
        assert_eq!(beh[0].is_internal_only, Some(true));
        assert_eq!(beh[1].is_internal_only, Some(false));
    }

    #[test]
    fn parse_is_internal_only_missing_is_none() {
        let response = r#"```json
{
  "breaking_behavioral_changes": [
    {"symbol": "Foo", "kind": "class", "description": "changed"}
  ],
  "breaking_api_changes": []
}
```"#;
        let (beh, _api) = parse_file_behavioral_response(response).unwrap();
        assert!(beh[0].is_internal_only.is_none());
    }

    #[test]
    fn parse_renders_element() {
        let response = r#"```json
{
  "breaking_behavioral_changes": [],
  "breaking_api_changes": [
    {
      "symbol": "TextList",
      "change": "removed",
      "description": "TextList removed, use Content instead",
      "renders_element": "ol"
    }
  ]
}
```"#;
        let (_beh, api) = parse_file_behavioral_response(response).unwrap();
        assert_eq!(api[0].renders_element.as_deref(), Some("ol"));
    }

    #[test]
    fn parse_renders_element_null_is_none() {
        let response = r#"```json
{
  "breaking_behavioral_changes": [],
  "breaking_api_changes": [
    {
      "symbol": "ModalProps.title",
      "change": "removed",
      "description": "title removed",
      "renders_element": null
    }
  ]
}
```"#;
        let (_beh, api) = parse_file_behavioral_response(response).unwrap();
        assert!(api[0].renders_element.is_none());
    }

    #[test]
    fn parse_verdict_from_json() {
        let response = r#"```json
{
  "is_breaking": true,
  "reasons": ["postcondition weakened"],
  "confidence": 0.75
}
```"#;
        let verdict = parse_breaking_verdict(response).unwrap();
        assert!(verdict.is_breaking);
        assert_eq!(verdict.reasons.len(), 1);
        assert!((verdict.confidence - 0.75).abs() < f64::EPSILON);
    }

    // ── Hierarchy inference tests ───────────────────────────────────

    #[test]
    fn parse_hierarchy_response_dropdown_family() {
        let response = r#"```json
{
  "components": {
    "Dropdown": {
      "expected_children": [
        { "name": "DropdownList", "required": true },
        { "name": "DropdownGroup", "required": false }
      ]
    },
    "DropdownList": {
      "expected_children": [
        { "name": "DropdownItem", "required": true }
      ]
    },
    "DropdownGroup": {
      "expected_children": [
        { "name": "DropdownItem", "required": true }
      ]
    },
    "DropdownItem": {
      "expected_children": []
    }
  }
}
```"#;
        let result = parse_hierarchy_response(response).unwrap();
        assert_eq!(result.len(), 4);

        let dropdown = &result["Dropdown"];
        assert_eq!(dropdown.len(), 2);
        assert_eq!(dropdown[0].name, "DropdownList");
        assert!(dropdown[0].required);
        assert_eq!(dropdown[1].name, "DropdownGroup");
        assert!(!dropdown[1].required);

        let list = &result["DropdownList"];
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "DropdownItem");

        let item = &result["DropdownItem"];
        assert!(item.is_empty());
    }

    #[test]
    fn parse_hierarchy_response_modal_family() {
        let response = r#"```json
{
  "components": {
    "Modal": {
      "expected_children": [
        { "name": "ModalHeader", "required": false },
        { "name": "ModalBody", "required": true },
        { "name": "ModalFooter", "required": false }
      ]
    },
    "ModalHeader": { "expected_children": [] },
    "ModalBody": { "expected_children": [] },
    "ModalFooter": { "expected_children": [] }
  }
}
```"#;
        let result = parse_hierarchy_response(response).unwrap();
        assert_eq!(result.len(), 4);

        let modal = &result["Modal"];
        assert_eq!(modal.len(), 3);
        assert!(!modal[0].required); // ModalHeader optional
        assert!(modal[1].required); // ModalBody required
        assert!(!modal[2].required); // ModalFooter optional
    }

    #[test]
    fn parse_hierarchy_response_empty_components() {
        let response = r#"```json
{
  "components": {
    "Badge": {
      "expected_children": []
    }
  }
}
```"#;
        let result = parse_hierarchy_response(response).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result["Badge"].is_empty());
    }

    // ── Suffix rename response parsing tests ───────────────────────

    #[test]
    fn parse_suffix_rename_response_valid() {
        let response = r#"```json
{
  "renames": [
    { "from": "PaddingTop", "to": "PaddingBlockStart" },
    { "from": "MarginLeft", "to": "MarginInlineStart" }
  ]
}
```"#;
        let result = parse_suffix_rename_response(response).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].from, "PaddingTop");
        assert_eq!(result[0].to, "PaddingBlockStart");
        assert_eq!(result[1].from, "MarginLeft");
        assert_eq!(result[1].to, "MarginInlineStart");
    }

    #[test]
    fn parse_suffix_rename_response_empty() {
        let response = r#"```json
{ "renames": [] }
```"#;
        let result = parse_suffix_rename_response(response).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parse_suffix_rename_response_no_json() {
        let response = "I couldn't find any renames.";
        let result = parse_suffix_rename_response(response);
        assert!(result.is_err());
    }
}
