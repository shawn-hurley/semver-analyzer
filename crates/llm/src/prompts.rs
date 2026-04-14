//! Prompt templates for LLM-based behavioral analysis.
//!
//! Each prompt is designed to produce structured JSON output matching
//! the `FunctionSpec` or `BreakingVerdict` schemas. Template-guided
//! generation reduces hallucination (Preguss finding: ~30% → ~11-19%).

use semver_analyzer_core::{ChangedFunction, FunctionSpec, LlmCategoryDefinition, TestDiff};

/// JSON schema template for `FunctionSpec`.
///
/// Included in every spec inference prompt so the LLM knows the exact
/// structure to produce.
const FUNCTION_SPEC_SCHEMA: &str = r#"{
  "preconditions": [
    {
      "parameter": "parameter name",
      "condition": "what is checked (e.g., must be non-empty string)",
      "on_violation": "what happens (e.g., throws TypeError)"
    }
  ],
  "postconditions": [
    {
      "condition": "when this output is produced",
      "returns": "what is returned/resolved"
    }
  ],
  "error_behavior": [
    {
      "trigger": "what causes the error",
      "error_type": "error class name (e.g., TypeError)",
      "message_pattern": "optional: error message substring"
    }
  ],
  "side_effects": [
    {
      "target": "what external state is changed",
      "action": "what is done (e.g., inserts row, emits event)",
      "condition": "optional: when this occurs"
    }
  ],
  "notes": ["any behavioral nuances that don't fit above"]
}"#;

/// Build a prompt for spec inference from function body alone.
pub fn build_spec_inference_prompt(function_body: &str, signature: &str) -> String {
    format!(
        r#"Analyze this function and produce a behavioral specification as a JSON object.

## Function signature:
```
{signature}
```

## Function body:
```
{function_body}
```

## Task:
Describe what this function guarantees — its preconditions (input validation),
postconditions (what it returns for given inputs), error behavior (what errors
it throws and when), side effects (external state changes), and any behavioral
notes that don't fit the structured fields.

## Output format:
Return ONLY a JSON object matching this schema (no other text):

```json
{FUNCTION_SPEC_SCHEMA}
```

Rules:
- Use empty arrays [] for categories with no entries
- Be specific and concrete in descriptions
- For preconditions, list actual parameter validation checks in the code
- For postconditions, describe what the function returns under different conditions
- For error_behavior, only list errors the code explicitly throws/rejects
- For side_effects, only list observable external state changes
- For notes, capture any behavioral nuances not covered above

Respond with ONLY the JSON object inside a ```json fenced block."#,
        signature = signature,
        function_body = function_body,
        FUNCTION_SPEC_SCHEMA = FUNCTION_SPEC_SCHEMA,
    )
}

/// Build a prompt for spec inference with test context.
pub fn build_spec_inference_with_test_prompt(
    function_body: &str,
    signature: &str,
    test_context: &TestDiff,
) -> String {
    let test_section = format_test_context(test_context);

    format!(
        r#"Analyze this function and produce a behavioral specification as a JSON object.

## Function signature:
```
{signature}
```

## Function body:
```
{function_body}
```

## Associated test diff:
The following test file was changed alongside this function. The test assertions
provide concrete examples of expected behavior — use them to ground your analysis.

{test_section}

## Task:
Describe what this function guarantees — its preconditions (input validation),
postconditions (what it returns for given inputs), error behavior (what errors
it throws and when), side effects (external state changes), and any behavioral
notes that don't fit the structured fields.

Pay special attention to the test assertions — they encode the developer's
explicit expectations of how this function should behave.

## Output format:
Return ONLY a JSON object matching this schema (no other text):

```json
{FUNCTION_SPEC_SCHEMA}
```

Rules:
- Use empty arrays [] for categories with no entries
- Be specific and concrete in descriptions
- Ground your analysis in the actual code and test assertions

Respond with ONLY the JSON object inside a ```json fenced block."#,
        signature = signature,
        function_body = function_body,
        test_section = test_section,
        FUNCTION_SPEC_SCHEMA = FUNCTION_SPEC_SCHEMA,
    )
}

/// Build a prompt for comparing two specs (Tier 2 LLM fallback).
pub fn build_spec_comparison_prompt(old: &FunctionSpec, new: &FunctionSpec) -> String {
    let old_json = serde_json::to_string_pretty(old).unwrap_or_else(|_| "{}".to_string());
    let new_json = serde_json::to_string_pretty(new).unwrap_or_else(|_| "{}".to_string());

    format!(
        r#"Compare these two behavioral specifications for the SAME function at two
different versions. Determine if the changes are breaking.

## Old version spec (v1):
```json
{old_json}
```

## New version spec (v2):
```json
{new_json}
```

## Breaking change criteria:
A change is breaking if:
- Preconditions are TIGHTENED (function accepts less input than before)
- Postconditions are WEAKENED (function guarantees less output than before)
- Error types changed (callers catching specific errors will break)
- New errors added for inputs that previously succeeded
- Side effects removed or changed (consumers depending on them will break)

A change is NOT breaking if:
- Preconditions are RELAXED (function accepts more input)
- Postconditions are STRENGTHENED (function guarantees more)
- Error cases removed (function is more permissive)
- New side effects added (unless they cause observable issues)

## Output format:
Return ONLY a JSON object:

```json
{{
  "is_breaking": true/false,
  "reasons": ["list of specific breaking changes found"],
  "confidence": 0.0-1.0
}}
```

Respond with ONLY the JSON object inside a ```json fenced block."#,
        old_json = old_json,
        new_json = new_json,
    )
}

/// Build a prompt for checking whether a behavioral break propagates
/// through a caller.
pub fn build_propagation_check_prompt(
    caller_body: &str,
    caller_signature: &str,
    callee_name: &str,
    evidence_description: &str,
) -> String {
    let evidence_desc = evidence_description;

    format!(
        r#"A behavioral change was detected in the function `{callee_name}`.
Determine whether this change PROPAGATES through the following caller function,
or whether the caller ABSORBS it.

## Caller signature:
```
{caller_signature}
```

## Caller body:
```
{caller_body}
```

## Behavioral change in `{callee_name}`:
{evidence_desc}

## Does the caller propagate this change?

The caller ABSORBS the change (does NOT propagate) if it:
- Ignores the callee's return value
- Catches and handles the callee's new error behavior
- Only calls the callee on code paths that don't trigger the change
- Applies its own validation that masks the change

The caller PROPAGATES the change if:
- It passes through the callee's return value to its own callers
- It doesn't handle the callee's new error cases
- The behavioral change affects the caller's observable output

## Output format:
Return ONLY a JSON object:

```json
{{
  "propagates": true/false,
  "reasoning": "brief explanation"
}}
```

Respond with ONLY the JSON object inside a ```json fenced block."#,
        callee_name = callee_name,
        caller_signature = caller_signature,
        caller_body = caller_body,
        evidence_desc = evidence_desc,
    )
}

// ── File-level behavioral analysis ──────────────────────────────────────

/// Build a prompt for file-level behavioral breaking change analysis.
///
/// Instead of per-function spec inference (2+ LLM calls per function),
/// this sends the git diff for a file and the list of changed function
/// signatures in one shot — 1 LLM call per file.
pub fn build_file_behavioral_prompt(
    file_path: &str,
    diff_content: &str,
    changed_functions: &[ChangedFunction],
    test_diff: Option<&str>,
    categories: &[LlmCategoryDefinition],
) -> String {
    let mut func_list = String::new();
    for f in changed_functions {
        func_list.push_str(&format!(
            "- `{}` ({})\n  Old: `{}`\n  New: `{}`\n",
            f.name,
            if f.visibility == semver_analyzer_core::Visibility::Exported {
                "exported"
            } else {
                "internal"
            },
            f.old_signature.as_deref().unwrap_or("(added)"),
            f.new_signature.as_deref().unwrap_or("(removed)"),
        ));
    }

    // Truncate very large diffs to avoid exceeding context limits
    let diff_truncated = if diff_content.len() > 15000 {
        format!(
            "{}\n\n... [diff truncated, {} bytes total] ...",
            &diff_content[..15000],
            diff_content.len()
        )
    } else {
        diff_content.to_string()
    };

    let func_section = if func_list.is_empty() {
        "(No function body changes detected — analyze the diff for type-level and behavioral changes)".to_string()
    } else {
        func_list
    };

    let test_diff_section = if let Some(td) = test_diff {
        let truncated_td = if td.len() > 8_000 { &td[..8_000] } else { td };
        format!(
            "\n## Associated test diff:\n\
             The following diff shows how this component's tests/examples changed,\n\
             revealing expected usage pattern changes:\n\
             ```diff\n{}\n```\n",
            truncated_td
        )
    } else {
        String::new()
    };

    // Build the category section dynamically from language definitions
    let category_section = build_category_section(categories);
    let category_enum = build_category_enum(categories);

    format!(
        r#"Analyze this file diff for breaking changes.

## File: `{file_path}`

## Changed functions in this file:
{func_section}

## Git diff:
```diff
{diff}
```
{test_diff_section}
## Task:
Identify breaking changes in these categories:

### A. Behavioral breaking changes
Changes that alter the OBSERVABLE BEHAVIOR of exported functions/components.
{category_section}
### B. API type-level breaking changes
Changes to type signatures that static analysis may miss:
1. **Interface/class `extends` changed**: changes available members
2. **Member optionality changed**: member went from optional to required or
   vice versa
3. **Enum/union members removed or renamed**: e.g., variant value
   removed or replaced
4. **Type narrowed or widened**: e.g., `string | null` → `string`
5. **Default value changed**: default changed in a way that alters behavior
6. **Member migration**: When a member is removed and its functionality moved
   to a child/sibling type, include `removal_disposition`

## What to EXCLUDE:
- New additions (new members, new functions, new enum variants)
- Internal refactoring that doesn't change observable behavior
- Comment-only changes
- Import reorganization
- Changes already obvious from type signature removal/addition

## Output format:
Return ONLY a JSON object:

```json
{{{{
  "breaking_behavioral_changes": [
    {{{{
      "symbol": "<TypeName or functionName>",
      "kind": "class",
      "category": "{category_enum}",
      "description": "<what changed and why it breaks consumers>",
      "is_internal_only": false
    }}}}
  ],
  "breaking_api_changes": [
    {{{{
      "symbol": "<TypeName.memberName or TypeName>",
      "change": "<signature_changed|type_changed|default_changed|removed>",
      "description": "<what changed in the type signature>",
      "removal_disposition": null,
      "renders_element": null
    }}}}
  ],
  "composition_pattern_changes": [
    {{{{
      "component": "<symbol whose container changed>",
      "old_parent": "<previous container/parent or null>",
      "new_parent": "<new container/parent or null>",
      "description": "<what nesting changed>"
    }}}}
  ]
}}}}
```

Rules:
- For behavioral: use "class" for components/types, "function" for others
- For behavioral: ALWAYS include a "category" from the list above
- For behavioral: set `is_internal_only` to true when the change only
  affects internal rendering and does NOT require consumer code changes.
  Set false when consumers must update their code.
- For API: use "TypeName.memberName" format for member changes
- For API removals: include `removal_disposition` when you can determine
  where the member's functionality went:
  - `{{{{"type": "moved_to_related_type", "target_type": "ChildName", "mechanism": "prop"}}}}` —
    member moved to a named member on a child/related type
  - `{{{{"type": "moved_to_related_type", "target_type": "ChildName", "mechanism": "children"}}}}` —
    member value should now be passed as children of the child type
   - `{{{{"type": "replaced_by_member", "new_member": "newMemberName"}}}}` —
     replaced by a different member on the SAME type. Rules:
     * `new_member` MUST be an exact member name that was ADDED to the same type in the diff
     * The new member must serve the same purpose
     * If the types are fundamentally different, use `truly_removed` instead
     * If unsure which member replaced it, use `null` instead
     * Do NOT guess — if you cannot find a clear 1:1 replacement, use `null`
   - `{{{{"type": "made_automatic"}}}}` — functionality is now inferred automatically
   - `{{{{"type": "truly_removed"}}}}` — removed with no replacement
   - `null` if you cannot determine the disposition
- For API removals of components: include `renders_element` with the HTML
  element the component renders when applicable. Set null if not applicable.
- For composition: include when nesting structure changed. Use empty array
  if no nesting changes.
- Keep descriptions specific and actionable
- Only include changes that would break existing consumers
- Use empty arrays for categories with no changes
- Respond with ONLY the JSON object inside a ```json fenced block."#,
        file_path = file_path,
        func_section = func_section,
        diff = diff_truncated,
        test_diff_section = test_diff_section,
        category_section = category_section,
        category_enum = category_enum,
    )
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Format test context for inclusion in a prompt.
fn format_test_context(test_diff: &TestDiff) -> String {
    let mut parts = Vec::new();

    parts.push(format!("Test file: {}", test_diff.test_file.display()));

    if !test_diff.removed_assertions.is_empty() {
        parts.push("Removed assertions:".to_string());
        for line in &test_diff.removed_assertions {
            parts.push(format!("  - {}", line));
        }
    }

    if !test_diff.added_assertions.is_empty() {
        parts.push("Added assertions:".to_string());
        for line in &test_diff.added_assertions {
            parts.push(format!("  + {}", line));
        }
    }

    if !test_diff.full_diff.is_empty() {
        parts.push("Full diff:".to_string());
        parts.push(format!("```diff\n{}\n```", test_diff.full_diff));
    }

    parts.join("\n")
}

/// Build the numbered category list for the behavioral section of the prompt.
///
/// Produces text like:
/// ```text
/// For each, assign a `category` from: `dom_structure`, `css_class`, ...
///
/// 1. **DOM/render changes** (category: `dom_structure`): Changed element types...
/// 2. **CSS changes** (category: `css_class`): Class name renames...
/// ```
fn build_category_section(categories: &[LlmCategoryDefinition]) -> String {
    if categories.is_empty() {
        return String::new();
    }

    let id_list: Vec<String> = categories.iter().map(|c| format!("`{}`", c.id)).collect();
    let mut section = format!(
        "For each, assign a `category` from: {}.\n\n",
        id_list.join(", ")
    );

    for (i, cat) in categories.iter().enumerate() {
        section.push_str(&format!(
            "{}. **{}** (category: `{}`): {}\n",
            i + 1,
            cat.label,
            cat.id,
            cat.description
        ));
    }

    section
}

/// Build the category enum string for the JSON schema in the prompt.
///
/// Produces: `<dom_structure|css_class|css_variable|...>`
fn build_category_enum(categories: &[LlmCategoryDefinition]) -> String {
    if categories.is_empty() {
        return "<category>".to_string();
    }
    let ids: Vec<&str> = categories.iter().map(|c| c.id.as_str()).collect();
    format!("<{}>", ids.join("|"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn spec_inference_prompt_contains_signature() {
        let prompt =
            build_spec_inference_prompt("{ return x + 1; }", "function add(x: number): number");
        assert!(prompt.contains("function add(x: number): number"));
        assert!(prompt.contains("return x + 1"));
        assert!(prompt.contains("preconditions"));
        assert!(prompt.contains("postconditions"));
    }

    #[test]
    fn spec_inference_prompt_includes_schema() {
        let prompt = build_spec_inference_prompt("{ return 1; }", "function f(): number");
        assert!(prompt.contains("parameter"));
        assert!(prompt.contains("condition"));
        assert!(prompt.contains("on_violation"));
        assert!(prompt.contains("error_type"));
        assert!(prompt.contains("side_effects"));
    }

    #[test]
    fn test_context_prompt_includes_assertions() {
        let test_diff = TestDiff {
            test_file: PathBuf::from("test.ts"),
            removed_assertions: vec!["expect(result).toBe(5)".into()],
            added_assertions: vec!["expect(result).toBe(10)".into()],
            has_assertion_changes: true,
            full_diff: String::new(),
        };

        let prompt = build_spec_inference_with_test_prompt(
            "{ return x + 1; }",
            "function add(x: number): number",
            &test_diff,
        );
        assert!(prompt.contains("expect(result).toBe(5)"));
        assert!(prompt.contains("expect(result).toBe(10)"));
        assert!(prompt.contains("test assertions"));
    }

    #[test]
    fn comparison_prompt_includes_both_specs() {
        let old = FunctionSpec {
            preconditions: vec![],
            postconditions: vec![semver_analyzer_core::Postcondition {
                condition: "always".into(),
                returns: "5".into(),
            }],
            error_behavior: vec![],
            side_effects: vec![],
            notes: vec![],
        };
        let new = FunctionSpec {
            preconditions: vec![],
            postconditions: vec![semver_analyzer_core::Postcondition {
                condition: "always".into(),
                returns: "10".into(),
            }],
            error_behavior: vec![],
            side_effects: vec![],
            notes: vec![],
        };

        let prompt = build_spec_comparison_prompt(&old, &new);
        assert!(prompt.contains("\"returns\": \"5\""));
        assert!(prompt.contains("\"returns\": \"10\""));
        assert!(prompt.contains("breaking"));
    }

    #[test]
    fn file_behavioral_prompt_includes_diff_and_functions() {
        let funcs = vec![ChangedFunction {
            qualified_name: "src/Modal.tsx::Modal".into(),
            name: "Modal".into(),
            file: std::path::PathBuf::from("src/Modal.tsx"),
            line: 10,
            kind: semver_analyzer_core::SymbolKind::Function,
            visibility: semver_analyzer_core::Visibility::Exported,
            old_body: Some("{ return <div>old</div>; }".into()),
            new_body: Some("{ return <section>new</section>; }".into()),
            old_signature: Some("function Modal(props: ModalProps): JSX.Element".into()),
            new_signature: Some("function Modal(props: ModalProps): JSX.Element".into()),
        }];

        let categories = vec![
            LlmCategoryDefinition {
                id: "dom_structure".into(),
                label: "DOM/render changes".into(),
                description: "Changed element types".into(),
            },
            LlmCategoryDefinition {
                id: "css_class".into(),
                label: "CSS changes".into(),
                description: "Class name renames".into(),
            },
        ];
        let prompt = build_file_behavioral_prompt(
            "src/Modal.tsx",
            "- <div>old</div>\n+ <section>new</section>",
            &funcs,
            None,
            &categories,
        );
        assert!(prompt.contains("Modal"));
        assert!(prompt.contains("src/Modal.tsx"));
        assert!(prompt.contains("<div>old</div>"));
        assert!(prompt.contains("breaking_behavioral_changes"));
        assert!(prompt.contains("exported"));
        // Categories appear in the prompt
        assert!(prompt.contains("`dom_structure`"));
        assert!(prompt.contains("`css_class`"));
        assert!(prompt.contains("DOM/render changes"));
        assert!(prompt.contains("<dom_structure|css_class>"));
    }

    #[test]
    fn propagation_prompt_includes_callee_info() {
        let evidence_desc =
            "Test assertions changed:\n  - expect(x).toBe(5)\n  + expect(x).toBe(10)\n";

        let prompt = build_propagation_check_prompt(
            "{ return helper() + 1; }",
            "function main(): number",
            "helper",
            evidence_desc,
        );
        assert!(prompt.contains("helper"));
        assert!(prompt.contains("return helper() + 1"));
        assert!(prompt.contains("propagates"));
    }
}

// ── Composition pattern analysis prompt ───────────────────────────────

/// Build the prompt for analyzing test/example diffs to detect composition
/// pattern changes (JSX nesting restructuring).
///
/// Given the diff of a test or example file, asks the LLM to identify
/// components whose parent-child nesting relationship changed.
pub fn build_composition_pattern_prompt(file_path: &str, diff_content: &str) -> String {
    // Truncate large diffs
    let diff_truncated = if diff_content.len() > 15_000 {
        &diff_content[..15_000]
    } else {
        diff_content
    };

    format!(
        r#"Analyze this diff of a component library's test/example file to identify changes in JSX component nesting structure.

## File: {file_path}

```diff
{diff_content}
```

Identify components whose **parent component changed** between the old and new code. This includes:
- A component that was a direct child of component A but is now a child of component B
- A component that gained a new wrapper component
- A component that was removed from a wrapper and is now a direct child of a higher-level component
- JSX props that changed from children pattern to named prop pattern (e.g., `<Button><Icon /></Button>` → `<Button icon={{<Icon />}} />`)

Return ONLY a JSON object inside a ```json fenced block:
```json
{{
  "composition_changes": [
    {{
      "component": "the symbol whose container changed",
      "old_parent": "the previous container/parent (null if newly added)",
      "new_parent": "the new container/parent (null if removed from nesting)",
      "description": "brief description of the nesting change"
    }}
  ]
}}
```

Rules:
- Only include changes where the JSX nesting structure actually changed
- Ignore CSS class changes, prop value changes, or text content changes
- Focus on structural parent-child relationships between React components
- If no composition changes are found, return {{"composition_changes": []}}
- Return the component names without angle brackets (e.g., "MastheadToggle" not "<MastheadToggle>")"#,
        file_path = file_path,
        diff_content = diff_truncated,
    )
}

// ── Rename inference prompts ──────────────────────────────────────────

/// Build the prompt for constant rename pattern inference (Call 1).
///
/// Given samples of removed and added constant names from a package,
/// asks the LLM to identify systematic regex-based rename patterns.
pub fn build_constant_rename_prompt(
    removed_sample: &[&str],
    added_sample: &[&str],
    package_name: &str,
    from_ref: &str,
    to_ref: &str,
) -> String {
    let removed_list = removed_sample
        .iter()
        .map(|s| format!("  {}", s))
        .collect::<Vec<_>>()
        .join("\n");
    let added_list = added_sample
        .iter()
        .map(|s| format!("  {}", s))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"These exported constants were removed from {package_name} between {from_ref} and {to_ref}:
{removed_list}

These exported constants were added:
{added_list}

Identify ALL systematic naming patterns that map removed constant names to added constant names.

Return ONLY a JSON array of regex substitution rules inside a ```json fenced block:
```json
[
  {{"match": "regex pattern matching removed names", "replace": "replacement using capture groups"}}
]
```

Rules:
- Use capture groups to generalize patterns (e.g., "(.*)Top$" not just "c_alert_PaddingTop")
- Each pattern should match multiple constants, not just one
- Order from most specific to least specific
- Only include patterns where applying the substitution to a removed name produces a name in the added list
- Do not include identity patterns where match and replace produce the same string"#,
        package_name = package_name,
        from_ref = from_ref,
        to_ref = to_ref,
        removed_list = removed_list,
        added_list = added_list,
    )
}

/// Build the prompt for interface/component rename mapping inference (Call 2).
///
/// Given removed and added interfaces with their member lists,
/// asks the LLM to identify which removed interfaces map to which added ones.
pub fn build_interface_rename_prompt(
    removed: &[(&str, &[String])], // (name, member_names)
    added: &[(&str, &[String])],   // (name, member_names)
    package_name: &str,
    from_ref: &str,
    to_ref: &str,
) -> String {
    let removed_list = removed
        .iter()
        .map(|(name, members)| {
            if members.is_empty() {
                format!("  {} (no members)", name)
            } else {
                format!("  {} (members: {})", name, members.join(", "))
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let added_list = added
        .iter()
        .map(|(name, members)| {
            if members.is_empty() {
                format!("  {} (no members)", name)
            } else {
                format!("  {} (members: {})", name, members.join(", "))
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"These interfaces/components were removed from {package_name} between {from_ref} and {to_ref}:
{removed_list}

These interfaces/components were added:
{added_list}

Identify which removed interfaces map to which added interfaces (renames/replacements).
Consider:
- Name similarity (e.g., TextProps → ContentProps)
- Member overlap (same prop names appearing in both)
- Typo corrections (e.g., FormFiledGroup → FormFieldGroup)
- Functional equivalence (component that does the same thing under a new name)

Return ONLY a JSON array of mappings inside a ```json fenced block:
```json
[
  {{"old_name": "removed name", "new_name": "added name", "confidence": "high|medium|low", "reason": "brief explanation"}}
]
```

Rules:
- Only include mappings where the added interface is a clear replacement for the removed one
- Set confidence to "high" for clear renames/typo fixes, "medium" for functional replacements with different names, "low" for uncertain matches
- If a removed interface has no replacement in the added list, omit it
- Return an empty array if no mappings can be determined"#,
        package_name = package_name,
        from_ref = from_ref,
        to_ref = to_ref,
        removed_list = removed_list,
        added_list = added_list,
    )
}

/// Build a prompt to infer the component hierarchy for a component family.
///
/// The LLM receives the full source code of all files in a component family
/// directory and determines the expected parent-child composition structure
/// for consumers of the library.
///
/// Props are NOT included in the prompt — they come from the AST surface.
/// The LLM only needs to determine the hierarchy (what goes inside what).
pub fn build_hierarchy_inference_prompt(
    family_name: &str,
    files_content: &str,
    related_components: Option<&str>,
) -> String {
    let related_section = related_components
        .map(|rc| {
            format!(
                r#"

## Related components from other families

These components share React context with this family. If a related
component is designed to be placed INSIDE a family component, include
it as an expected child of that family component. Match by naming
(e.g., a "ToggleButton" goes inside a "Toggle" container).

Do NOT include a related component if the family component goes inside
IT (reversed direction).

{rc}"#
            )
        })
        .unwrap_or_default();

    format!(
        r#"You are a JSON-only API. Do NOT output any explanation, reasoning, or markdown headers.
Output ONLY a single ```json fenced code block.

Analyze the `{family_name}` component family and determine the expected parent-child composition hierarchy.

## Source files:
{files_content}{related_section}

## Rules:
- For each **exported** component, list what other components from this family (or related components listed above) that CONSUMERS provide as direct JSX children
- A child is "required" if the parent needs it to function, "optional" otherwise
- CRITICAL: If a parent component WRAPS children in another component internally (e.g., `return <div><SomeWrapper>{{children}}</SomeWrapper></div>` or `{{hasWrapper ? <SomeWrapper>{{children}}</SomeWrapper> : children}}`), that wrapper is an INTERNAL implementation detail. Do NOT list it as an expected child. The consumer passes `children` to the parent and the parent handles the wrapping automatically.
- This rule applies even if the wrapper component is exported from index.ts.
- CRITICAL: If a component is received via a NAMED PROP (e.g., `header`, `icon`, `toggle`, `footer`) and rendered internally, it is NOT a direct JSX child. Set mechanism to "prop" and specify the prop name. Only set mechanism to "child" for components that consumers place directly between opening and closing JSX tags: `<Parent><Child /></Parent>`.
  Example prop-passed: `<FormFieldGroup header={{<FormFieldGroupHeader />}} />` → mechanism: "prop", propName: "header"
  Example child-passed: `<Modal><ModalBody>...</ModalBody></Modal>` → mechanism: "child"
- Only include components that consumers must explicitly add in their JSX (as children or prop values)
- Exclude: internally-rendered components, base components from other families, HTML elements, the component itself

## Output format — respond with ONLY this JSON, no other text:
```json
{{
  "components": {{
    "<ComponentName>": {{
      "expected_children": [
        {{ "name": "<ChildComponentName>", "required": true, "mechanism": "child" }},
        {{ "name": "<PropPassedComponent>", "required": false, "mechanism": "prop", "propName": "header" }}
      ]
    }}
  }}
}}
```"#,
        family_name = family_name,
        files_content = files_content,
    )
}

// ── CSS Suffix Rename Inference Prompt ───────────────────────────────────

/// Build a prompt for LLM inference of CSS property suffix renames.
///
/// Given two sets of suffixes (removed from old version, added in new version),
/// the LLM identifies which removed suffixes are CSS physical property names
/// that were renamed to their logical equivalents in the new version.
pub fn build_suffix_rename_prompt(removed_suffixes: &[&str], added_suffixes: &[&str]) -> String {
    let removed_list = removed_suffixes.join(", ");
    let added_list = added_suffixes.join(", ");

    format!(
        r#"A CSS design system library changed its CSS custom property naming between versions. The library encodes CSS property names as PascalCase suffixes in variable names (e.g., `--pf-c-button--PaddingTop` uses suffix `PaddingTop` for the CSS property `padding-top`).

Between versions, some suffixes were removed and new ones were added. Your task is to identify which removed suffixes were **renamed** to new suffixes — specifically, CSS physical property names that were replaced with their CSS Logical Properties equivalents.

## CSS Logical Properties background

CSS Logical Properties replace physical direction words with flow-relative ones:
- `top` → `block-start`, `bottom` → `block-end`
- `left` → `inline-start`, `right` → `inline-end`
- Position properties like `top`/`left` → `inset-block-start`/`inset-inline-start`

## Removed suffixes (from old version):
{removed}

## Added suffixes (in new version):
{added}

## Task:
Identify pairs where a removed suffix is the PascalCase form of a CSS physical property and the corresponding added suffix is its CSS logical property equivalent.

Only include pairs where you are confident the rename is a CSS physical→logical property change. Do NOT include pairs that are unrelated property changes (e.g., Color→FontWeight is NOT a logical property rename).

## Output format:
Return ONLY a JSON object inside a ```json fenced block:
```json
{{
  "renames": [
    {{ "from": "PaddingTop", "to": "PaddingBlockStart" }}
  ]
}}
```

Rules:
- Only include CSS physical→logical property renames
- The "from" must be a removed suffix, the "to" must be an added suffix
- If no valid renames are found, return `{{"renames": []}}`"#,
        removed = removed_list,
        added = added_list,
    )
}

#[cfg(test)]
mod hierarchy_tests {
    use super::*;

    #[test]
    fn hierarchy_prompt_without_related() {
        let prompt = build_hierarchy_inference_prompt("Dropdown", "source code here", None);
        assert!(prompt.contains("Dropdown"));
        assert!(prompt.contains("source code here"));
        // Should NOT have the related components section header
        assert!(
            !prompt.contains("Related components from other families"),
            "Prompt should not include related section when None",
        );
    }

    #[test]
    fn hierarchy_prompt_with_related() {
        let related =
            "--- Related: Page/PageToggleButton.tsx ---\nexport interface PageToggleButtonProps {}";
        let prompt = build_hierarchy_inference_prompt("Masthead", "masthead source", Some(related));
        assert!(prompt.contains("Masthead"));
        assert!(prompt.contains("Related components from other families"));
        assert!(prompt.contains("PageToggleButtonProps"));
    }
}
