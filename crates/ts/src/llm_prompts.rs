//! TypeScript/React-specific LLM prompt builders.
//!
//! These prompts contain language-specific terminology (JSX, React context,
//! CSS custom properties, component hierarchy) and belong in the TS crate,
//! not in the language-agnostic LLM crate. The LLM crate provides generic
//! infrastructure (run command, parse JSON); the prompts are language-specific.

/// Build a prompt for component hierarchy inference.
///
/// Uses React/JSX-specific terminology: JSX children, props, component
/// families, internal rendering, barrel file exports.
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

/// Build a prompt for CSS property suffix rename inference.
///
/// Identifies CSS physical-to-logical property renames encoded as
/// PascalCase suffixes in CSS custom property names.
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
mod tests {
    use super::*;

    #[test]
    fn hierarchy_prompt_without_related() {
        let prompt = build_hierarchy_inference_prompt("Dropdown", "source code here", None);
        assert!(prompt.contains("Dropdown"));
        assert!(prompt.contains("source code here"));
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
