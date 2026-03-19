//! Prompt templates for LLM-based behavioral analysis.
//!
//! Each prompt is designed to produce structured JSON output matching
//! the `FunctionSpec` or `BreakingVerdict` schemas. Template-guided
//! generation reduces hallucination (Preguss finding: ~30% → ~11-19%).

use semver_analyzer_core::{ChangedFunction, EvidenceSource, FunctionSpec, TestDiff};

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
    evidence: &EvidenceSource,
) -> String {
    let evidence_desc = format_evidence(evidence);

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
            if f.old_signature.is_empty() {
                "(added)"
            } else {
                &f.old_signature
            },
            if f.new_signature.is_empty() {
                "(removed)"
            } else {
                &f.new_signature
            },
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

    format!(
        r#"Analyze this file diff for breaking changes.

## File: `{file_path}`

## Changed functions in this file:
{func_section}

## Git diff:
```diff
{diff}
```

## Task:
Identify TWO categories of breaking changes:

### A. Behavioral breaking changes
Changes that alter the OBSERVABLE BEHAVIOR of exported functions/components.
For each, assign a `category` from: `dom_structure`, `css_class`, `css_variable`,
`accessibility`, `default_value`, `logic_change`, `data_attribute`, `render_output`.

1. **DOM/render changes** (category: `dom_structure`): Changed element types
   (e.g., `<header>` → `<div>`), added/removed wrapper elements, altered
   component nesting structure, children wrapping changes
2. **CSS changes** (category: `css_class`): Class name renames
   (e.g., pf-v5-* → pf-v6-*), removed CSS classes, changed class
   application logic, modifier classes no longer applied
3. **CSS variable changes** (category: `css_variable`): Renamed or removed
   CSS custom properties (e.g., --pf-v5-* → --pf-v6-*)
4. **Accessibility changes** (category: `accessibility`): Added/removed/changed
   ARIA attributes (aria-label, aria-labelledby, aria-describedby, aria-hidden),
   changed `role` attributes, keyboard navigation changes, focus management
   changes, tab order changes (tabIndex additions/removals)
5. **Default value changes** (category: `default_value`): Changed default
   prop values that alter behavior
6. **Logic changes** (category: `logic_change`): Changed conditional logic,
   removed code paths, altered return values for same inputs, changed event
   handler types, removed or changed event emissions
7. **Data attribute changes** (category: `data_attribute`): Changed
   data-ouia-component-type, data-testid, or other data-* attributes
8. **Other render output** (category: `render_output`): Any other change
   to what is visually rendered that doesn't fit above

### B. API type-level breaking changes
Changes to type signatures that static .d.ts analysis may miss:
1. **Interface `extends` changed**: e.g., props now extends `CheckboxProps`
   instead of `React.HTMLProps<HTMLInputElement>` — changes available props
2. **Prop optionality changed**: prop went from optional to required or
   vice versa
3. **Enum/union members removed or renamed**: e.g., variant value
   'light300' replaced with 'secondary'
4. **Type narrowed or widened**: e.g., `string | null` → `string`
5. **Default value changed**: e.g., closeBtnAriaLabel default changed
   from 'close' to dynamic value
6. **Prop migration**: When a prop is removed and its functionality moved
   to a child/sibling component, include `removal_disposition`

## What to EXCLUDE:
- New additions (new props, new functions, new enum members)
- Internal refactoring that doesn't change observable behavior
- Comment-only changes
- Import reorganization
- Changes already obvious from type signature removal/addition

## Output format:
Return ONLY a JSON object:

```json
{{
  "breaking_behavioral_changes": [
    {{
      "symbol": "<ComponentName or functionName>",
      "kind": "class",
      "category": "<dom_structure|css_class|css_variable|accessibility|default_value|logic_change|data_attribute|render_output>",
      "description": "<what changed and why it breaks consumers>",
      "is_internal_only": false
    }}
  ],
  "breaking_api_changes": [
    {{
      "symbol": "<InterfaceName.propName or TypeName>",
      "change": "<signature_changed|type_changed|default_changed|removed>",
      "description": "<what changed in the type signature>",
      "removal_disposition": null,
      "renders_element": null
    }}
  ]
}}
```

Rules:
- For behavioral: use "class" for React components, "function" for others
- For behavioral: ALWAYS include a "category" from the list above
- For behavioral: set `is_internal_only` to true when the change only
  affects internal rendering and does NOT require consumer code changes
  (e.g., internal component now passes a prop differently). Set false
  when consumers must update their code.
- For API: use "InterfaceName.propName" format for property changes
- For API removals: include `removal_disposition` when you can determine
  where the prop's functionality went:
  - `{{"type": "moved_to_child", "target_component": "ChildName", "mechanism": "prop"}}` —
    prop moved to a named prop on a child component (e.g., title → ModalHeader.title)
  - `{{"type": "moved_to_child", "target_component": "ChildName", "mechanism": "children"}}` —
    prop value should now be passed as children of the child component
    (e.g., actions → <ModalFooter>{{actions}}</ModalFooter>)
  - `{{"type": "replaced_by_prop", "new_prop": "newPropName"}}` —
    replaced by a different prop on the same component
  - `{{"type": "made_automatic"}}` — functionality is now inferred automatically
  - `{{"type": "truly_removed"}}` — removed with no replacement
  - `null` if you cannot determine the disposition
- For API removals of components: include `renders_element` with the HTML
  element the component renders (e.g., "ol", "ul", "div", "footer") when
  the component is being replaced by a generic component that needs an
  explicit element type. Set null if not applicable.
- Keep descriptions specific and actionable
- Only include changes that would break existing consumers
- Use empty arrays for categories with no changes
- Respond with ONLY the JSON object inside a ```json fenced block."#,
        file_path = file_path,
        func_section = func_section,
        diff = diff_truncated,
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

/// Format evidence for the propagation check prompt.
fn format_evidence(evidence: &EvidenceSource) -> String {
    match evidence {
        EvidenceSource::TestDelta { test_diff } => {
            let mut desc = String::from("Test assertions changed:\n");
            for line in &test_diff.removed_assertions {
                desc.push_str(&format!("  - {}\n", line));
            }
            for line in &test_diff.added_assertions {
                desc.push_str(&format!("  + {}\n", line));
            }
            desc
        }
        EvidenceSource::LlmWithTestContext { spec_old, spec_new }
        | EvidenceSource::LlmOnly { spec_old, spec_new } => {
            format!(
                "Old spec: {}\nNew spec: {}",
                serde_json::to_string(spec_old).unwrap_or_default(),
                serde_json::to_string(spec_new).unwrap_or_default()
            )
        }
        EvidenceSource::JsxDiff { change_description } => {
            format!("JSX render output change: {}", change_description)
        }
    }
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
            old_body: "{ return <div>old</div>; }".into(),
            new_body: "{ return <section>new</section>; }".into(),
            old_signature: "function Modal(props: ModalProps): JSX.Element".into(),
            new_signature: "function Modal(props: ModalProps): JSX.Element".into(),
        }];

        let prompt = build_file_behavioral_prompt(
            "src/Modal.tsx",
            "- <div>old</div>\n+ <section>new</section>",
            &funcs,
        );
        assert!(prompt.contains("Modal"));
        assert!(prompt.contains("src/Modal.tsx"));
        assert!(prompt.contains("<div>old</div>"));
        assert!(prompt.contains("breaking_behavioral_changes"));
        assert!(prompt.contains("exported"));
    }

    #[test]
    fn propagation_prompt_includes_callee_info() {
        let evidence = EvidenceSource::TestDelta {
            test_diff: TestDiff {
                test_file: PathBuf::from("test.ts"),
                removed_assertions: vec!["expect(x).toBe(5)".into()],
                added_assertions: vec!["expect(x).toBe(10)".into()],
                has_assertion_changes: true,
                full_diff: String::new(),
            },
        };

        let prompt = build_propagation_check_prompt(
            "{ return helper() + 1; }",
            "function main(): number",
            "helper",
            &evidence,
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
      "component": "the component whose parent changed",
      "old_parent": "the previous parent component (null if newly added)",
      "new_parent": "the new parent component (null if removed from nesting)",
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
