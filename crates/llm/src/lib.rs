//! LLM-based behavioral analysis for the semver-analyzer.
//!
//! This crate implements the `BehaviorAnalyzer` trait from `semver-analyzer-core`.
//! It provides:
//!
//! 1. **Agent-agnostic LLM invocation** via `--llm-command` (goose, opencode, etc.)
//! 2. **Template-constrained spec inference** — prompts that produce `FunctionSpec` JSON
//! 3. **Tier 1 structural spec comparison** — mechanical comparison without LLM
//! 4. **Tier 2 LLM fallback** — for ambiguous `notes` diffs and fuzzy matches
//!
//! ## Usage
//!
//! ```rust,ignore
//! use semver_analyzer_llm::LlmBehaviorAnalyzer;
//!
//! let analyzer = LlmBehaviorAnalyzer::new("goose run --no-session -q -t");
//! let spec = analyzer.infer_spec(&function_body, &signature)?;
//! ```

pub mod invoke;
mod prompts;
mod spec_compare;

use anyhow::Result;
pub use invoke::{
    FileApiChange, FileBehavioralChange, LlmConstantRenamePattern, LlmInterfaceRenameMapping,
    LlmSuffixRename,
};
use semver_analyzer_core::{
    BehaviorAnalyzer, BreakingVerdict, ChangedFunction, FunctionSpec, LlmCategoryDefinition,
    TestDiff,
};

/// LLM-based implementation of `BehaviorAnalyzer`.
///
/// Uses an external command (e.g., `goose run`, `opencode run`) to invoke
/// an LLM for spec inference. The command receives a prompt as its final
/// argument and is expected to return a response on stdout.
pub struct LlmBehaviorAnalyzer {
    /// The command template for invoking the LLM.
    /// The prompt is appended as the final argument.
    /// e.g., "goose run --no-session -q -t" or "opencode run"
    llm_command: String,

    /// Timeout in seconds for each LLM invocation.
    timeout_secs: u64,
}

impl LlmBehaviorAnalyzer {
    /// Create a new LLM analyzer with the given command.
    pub fn new(llm_command: &str) -> Self {
        Self {
            llm_command: llm_command.to_string(),
            timeout_secs: 120,
        }
    }

    /// Set the timeout for LLM invocations.
    pub fn with_timeout(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    /// Run an LLM command with debug logging.
    fn run_llm(&self, prompt: &str) -> Result<String> {
        tracing::debug!(prompt_bytes = prompt.len(), "sending LLM prompt");
        let result = invoke::run_llm_command(&self.llm_command, prompt, self.timeout_secs);
        match &result {
            Ok(response) => {
                tracing::debug!(
                    response_bytes = response.len(),
                    response_tail = %&response[response.floor_char_boundary(response.len().saturating_sub(200))..],
                    "LLM response received"
                );
            }
            Err(e) => {
                tracing::debug!(%e, "LLM command failed");
            }
        }
        result
    }
}

impl LlmBehaviorAnalyzer {
    /// Analyze a single file's diff for breaking changes (behavioral + API type-level).
    ///
    /// This is the file-level approach: one LLM call per file instead of
    /// 2+ calls per function. The prompt includes the git diff and the
    /// list of changed function signatures.
    ///
    /// The `categories` parameter provides language-specific behavioral change
    /// categories for the LLM prompt. Pass `&lang.llm_categories()` from the
    /// `Language` trait implementation.
    ///
    /// Returns (behavioral_changes, api_changes).
    pub fn analyze_file_diff(
        &self,
        file_path: &str,
        diff_content: &str,
        changed_functions: &[ChangedFunction],
        test_diff: Option<&str>,
        categories: &[LlmCategoryDefinition],
    ) -> Result<(Vec<FileBehavioralChange>, Vec<FileApiChange>)> {
        let prompt = prompts::build_file_behavioral_prompt(
            file_path,
            diff_content,
            changed_functions,
            test_diff,
            categories,
        );
        let response = self.run_llm(&prompt)?;
        invoke::parse_file_behavioral_response(&response)
    }

    /// Infer constant rename patterns from sampled removed/added constant names.
    pub fn infer_constant_renames(
        &self,
        removed_sample: &[&str],
        added_sample: &[&str],
        package_name: &str,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<Vec<LlmConstantRenamePattern>> {
        let prompt = prompts::build_constant_rename_prompt(
            removed_sample,
            added_sample,
            package_name,
            from_ref,
            to_ref,
        );
        let response = self.run_llm(&prompt)?;
        invoke::parse_constant_rename_response(&response)
    }

    /// Run a pre-built prompt and parse the response as a component hierarchy.
    ///
    /// The prompt text is language-specific and should be provided by the
    /// `Language` implementation. The LLM crate only handles execution and
    /// JSON parsing of the response.
    pub fn infer_hierarchy_from_prompt(
        &self,
        prompt: &str,
    ) -> Result<std::collections::HashMap<String, Vec<semver_analyzer_core::ExpectedChild>>> {
        let response = self.run_llm(prompt)?;
        invoke::parse_hierarchy_response(&response)
    }

    /// Run a pre-built prompt and parse the response as suffix rename pairs.
    ///
    /// The prompt text is language-specific (e.g., CSS logical property
    /// renames for TypeScript). The LLM crate only handles execution and
    /// JSON parsing of the response.
    pub fn infer_suffix_renames_from_prompt(
        &self,
        prompt: &str,
    ) -> Result<Vec<invoke::LlmSuffixRename>> {
        let response = self.run_llm(prompt)?;
        invoke::parse_suffix_rename_response(&response)
    }

    /// Infer interface/component rename mappings from removed/added interface data.
    pub fn infer_interface_renames(
        &self,
        removed: &[(&str, &[String])],
        added: &[(&str, &[String])],
        package_name: &str,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<Vec<LlmInterfaceRenameMapping>> {
        let prompt =
            prompts::build_interface_rename_prompt(removed, added, package_name, from_ref, to_ref);
        let response = self.run_llm(&prompt)?;
        invoke::parse_interface_rename_response(&response)
    }
}

impl BehaviorAnalyzer for LlmBehaviorAnalyzer {
    fn infer_spec(&self, function_body: &str, signature: &str) -> Result<FunctionSpec> {
        let prompt = prompts::build_spec_inference_prompt(function_body, signature);
        let response = self.run_llm(&prompt)?;
        invoke::parse_function_spec(&response)
    }

    fn infer_spec_with_test_context(
        &self,
        function_body: &str,
        signature: &str,
        test_context: &TestDiff,
    ) -> Result<FunctionSpec> {
        let prompt =
            prompts::build_spec_inference_with_test_prompt(function_body, signature, test_context);
        let response = self.run_llm(&prompt)?;
        invoke::parse_function_spec(&response)
    }

    fn specs_are_breaking(
        &self,
        old: &FunctionSpec,
        new: &FunctionSpec,
    ) -> Result<BreakingVerdict> {
        // Tier 1: Structural comparison (no LLM)
        let tier1 = spec_compare::structural_compare(old, new);

        if tier1.is_breaking || tier1.confidence >= 0.80 {
            return Ok(tier1);
        }

        // Tier 2: LLM fallback for notes diffs and ambiguous cases
        if !old.notes.is_empty() || !new.notes.is_empty() {
            let prompt = prompts::build_spec_comparison_prompt(old, new);
            let response = self.run_llm(&prompt)?;
            return invoke::parse_breaking_verdict(&response);
        }

        // No breaking changes detected
        Ok(tier1)
    }

    fn check_propagation(
        &self,
        caller_body: &str,
        caller_signature: &str,
        callee_name: &str,
        evidence_description: &str,
    ) -> Result<bool> {
        let prompt = prompts::build_propagation_check_prompt(
            caller_body,
            caller_signature,
            callee_name,
            evidence_description,
        );
        let response = self.run_llm(&prompt)?;
        invoke::parse_propagation_result(&response)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn response_tail_handles_multibyte_boundary() {
        // Verifies that floor_char_boundary prevents panics when the
        // 200-byte-from-end offset lands inside a multi-byte UTF-8
        // character (e.g. '→' = 3 bytes E2 86 92).
        let arrow = "→";
        // 198 bytes of ASCII + 3-byte arrow = 201 bytes total
        // saturating_sub(200) = byte 1, which is mid-character
        let response = format!("{}{}", arrow, "x".repeat(198));
        assert_eq!(response.len(), 201);
        let start = response.floor_char_boundary(response.len().saturating_sub(200));
        let tail = &response[start..];
        // floor_char_boundary rounds down to 0 (start of '→'), so we get the full string
        assert_eq!(tail, response);
    }
}
