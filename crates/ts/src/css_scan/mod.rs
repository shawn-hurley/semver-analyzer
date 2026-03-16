//! CSS variable and class prefix scanner.
//!
//! Scans function bodies (old and new) for CSS custom property references
//! (`--pf-v5-*`, `--pf-v6-*`, etc.) and CSS class prefixes (`pf-v5-c-*`,
//! `pf-v6-c-*`), reporting renames and removals as `JsxChange` entries.
//!
//! This is deterministic — no LLM involved. It complements the JSX differ
//! by catching CSS variable changes that are invisible to DOM/attribute diffing.

use regex::Regex;
use semver_analyzer_core::{BehavioralCategory, JsxChange};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::LazyLock;

/// Regex matching CSS custom properties (e.g., `--pf-v5-global--Color--100`).
static CSS_VAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"--[a-zA-Z][a-zA-Z0-9_-]*(?:--[a-zA-Z0-9_-]+)+").unwrap());

/// Regex matching CSS class name patterns (e.g., `pf-v5-c-button`, `pf-m-primary`).
static CSS_CLASS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bpf-(?:v\d+-)(?:[a-z]-)?[a-z][a-z0-9-]+").unwrap());

/// Scan old and new function bodies for CSS variable and class prefix changes.
///
/// Returns `JsxChange` entries for:
/// - CSS custom properties that were removed or renamed
/// - CSS class prefixes that changed (e.g., `pf-v5-c-*` → `pf-v6-c-*`)
pub fn diff_css_references(
    old_body: &str,
    new_body: &str,
    symbol: &str,
    file: &Path,
) -> Vec<JsxChange> {
    let mut changes = Vec::new();

    // CSS custom properties
    let old_vars = extract_matches(&CSS_VAR_RE, old_body);
    let new_vars = extract_matches(&CSS_VAR_RE, new_body);

    for var in old_vars.difference(&new_vars) {
        changes.push(JsxChange {
            symbol: symbol.to_string(),
            file: file.to_path_buf(),
            category: BehavioralCategory::CssVariable,
            description: format!("CSS variable '{}' removed from source", var),
            before: Some(var.clone()),
            after: None,
        });
    }

    for var in new_vars.difference(&old_vars) {
        changes.push(JsxChange {
            symbol: symbol.to_string(),
            file: file.to_path_buf(),
            category: BehavioralCategory::CssVariable,
            description: format!("CSS variable '{}' added to source", var),
            before: None,
            after: Some(var.clone()),
        });
    }

    // CSS class prefixes (PatternFly-style)
    let old_classes = extract_matches(&CSS_CLASS_RE, old_body);
    let new_classes = extract_matches(&CSS_CLASS_RE, new_body);

    for class in old_classes.difference(&new_classes) {
        changes.push(JsxChange {
            symbol: symbol.to_string(),
            file: file.to_path_buf(),
            category: BehavioralCategory::CssClass,
            description: format!("CSS class prefix '{}' removed from source", class),
            before: Some(class.clone()),
            after: None,
        });
    }

    for class in new_classes.difference(&old_classes) {
        changes.push(JsxChange {
            symbol: symbol.to_string(),
            file: file.to_path_buf(),
            category: BehavioralCategory::CssClass,
            description: format!("CSS class prefix '{}' added to source", class),
            before: None,
            after: Some(class.clone()),
        });
    }

    changes
}

/// Returns true if the body contains CSS variable or versioned class references.
pub fn body_contains_css_refs(body: &str) -> bool {
    CSS_VAR_RE.is_match(body) || CSS_CLASS_RE.is_match(body)
}

/// Extract all unique matches of a regex from text.
fn extract_matches(re: &Regex, text: &str) -> BTreeSet<String> {
    re.find_iter(text).map(|m| m.as_str().to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_css_var_removed() {
        let old = r#"const color = "var(--pf-v5-global--Color--100)";"#;
        let new = r#"const color = "var(--pf-v6-global--Color--100)";"#;
        let changes = diff_css_references(old, new, "MyComponent", &PathBuf::from("test.tsx"));

        // Should have CSS variable changes (removed old, added new).
        // May also have CSS class prefix changes since pf-v5-global matches the class regex.
        let var_changes: Vec<_> = changes
            .iter()
            .filter(|c| c.category == BehavioralCategory::CssVariable)
            .collect();
        assert_eq!(var_changes.len(), 2); // old removed + new added
        let removed: Vec<_> = var_changes
            .iter()
            .filter(|c| c.description.contains("removed"))
            .collect();
        let added: Vec<_> = var_changes
            .iter()
            .filter(|c| c.description.contains("added"))
            .collect();
        assert_eq!(removed.len(), 1);
        assert_eq!(added.len(), 1);
        assert!(removed[0]
            .before
            .as_ref()
            .unwrap()
            .contains("--pf-v5-global--Color--100"));
        assert!(added[0]
            .after
            .as_ref()
            .unwrap()
            .contains("--pf-v6-global--Color--100"));
    }

    #[test]
    fn test_css_class_prefix_changed() {
        let old = r#"className="pf-v5-c-button pf-v5-c-button--primary""#;
        let new = r#"className="pf-v6-c-button pf-v6-c-button--primary""#;
        let changes = diff_css_references(old, new, "Button", &PathBuf::from("test.tsx"));

        let removed: Vec<_> = changes
            .iter()
            .filter(|c| c.category == BehavioralCategory::CssClass && c.before.is_some())
            .collect();
        assert!(!removed.is_empty());
        assert!(removed
            .iter()
            .any(|c| c.before.as_ref().unwrap().starts_with("pf-v5-")));
    }

    #[test]
    fn test_no_css_refs_returns_empty() {
        let old = "const x = 42;";
        let new = "const x = 43;";
        let changes = diff_css_references(old, new, "foo", &PathBuf::from("test.ts"));
        assert!(changes.is_empty());
    }

    #[test]
    fn test_body_contains_css_refs() {
        assert!(body_contains_css_refs(r#"var(--pf-v5-global--Color--100)"#));
        assert!(body_contains_css_refs(r#"className="pf-v5-c-button""#));
        assert!(!body_contains_css_refs("const x = 42;"));
    }

    #[test]
    fn test_css_var_unchanged() {
        let body = r#"const color = "var(--pf-v5-global--Color--100)";"#;
        let changes = diff_css_references(body, body, "Comp", &PathBuf::from("test.tsx"));
        assert!(changes.is_empty());
    }

    #[test]
    fn test_multiple_vars_mixed() {
        let old = r#"
            const a = "var(--pf-v5-global--Color--100)";
            const b = "var(--pf-v5-global--spacer--md)";
            const c = "var(--pf-v5-global--FontSize--sm)";
        "#;
        let new = r#"
            const a = "var(--pf-v6-global--Color--100)";
            const b = "var(--pf-v5-global--spacer--md)";
        "#;
        let changes = diff_css_references(old, new, "Comp", &PathBuf::from("test.tsx"));

        // --pf-v5-global--Color--100 removed, --pf-v6-global--Color--100 added
        // --pf-v5-global--FontSize--sm removed (was in old but not new)
        // --pf-v5-global--spacer--md unchanged
        let removed: Vec<_> = changes
            .iter()
            .filter(|c| c.description.contains("removed"))
            .collect();
        assert!(removed.len() >= 2);
    }
}
