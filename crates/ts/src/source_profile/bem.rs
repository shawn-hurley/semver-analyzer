//! BEM (Block-Element-Modifier) token parsing from `styles.*` references.
//!
//! PatternFly's `@patternfly/react-styles` generates JS class maps from CSS.
//! The `formatClassName` function (in `generateClassMaps.mjs`) strips the
//! `pf-(v6-)?(c-|l-|m-|u-|is-|has-)?` prefix and camelCases the rest:
//!
//! ```text
//! pf-v6-c-menu            → styles.menu           (Block)
//! pf-v6-c-menu__list      → styles.menuList        (Element: menu__list)
//! pf-v6-c-menu__list-item → styles.menuListItem    (Element: menu__list-item)
//! pf-m-expanded           → styles.modifiers.expanded (Modifier)
//! ```
//!
//! This module extracts `styles.*` references from source code and parses
//! the BEM structure: block name, element names, and modifier names.

use std::collections::BTreeSet;

/// Parsed BEM structure from a set of `styles.*` tokens.
#[derive(Debug, Clone, Default)]
pub struct BemStructure {
    /// The primary block name (e.g., "menu", "modalBox", "dropdown").
    pub block: Option<String>,

    /// Element names (the part after `__` in BEM, converted to camelCase
    /// by the class map generator).
    /// e.g., for block "menu": { "list", "listItem", "itemMain" }
    pub elements: BTreeSet<String>,

    /// Modifier names (from `styles.modifiers.*`).
    /// e.g., { "expanded", "disabled", "plain" }
    pub modifiers: BTreeSet<String>,

    /// Raw `styles.*` token names (excluding `modifiers`).
    /// e.g., { "menu", "menuList", "menuListItem" }
    pub raw_tokens: BTreeSet<String>,
}

/// Extract `styles.*` token references from a source string.
///
/// Scans for patterns like `styles.menuList` and `styles.modifiers.expanded`.
/// Returns all unique token paths found.
pub fn extract_style_tokens(source: &str) -> Vec<StyleToken> {
    let mut tokens = Vec::new();
    let mut seen = BTreeSet::new();

    // Match `styles.xxx` and `styles.modifiers.xxx` patterns.
    // We search for identifier chains starting with a known styles variable.
    //
    // All comparisons use byte-level matching on ASCII patterns. The
    // "styles." prefix and identifier characters are pure ASCII, so byte
    // offsets are safe for comparison. We only slice `source` (as &str)
    // at positions that are guaranteed to be ASCII (and thus char
    // boundaries).
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Look for "styles." prefix (all ASCII, safe to compare as bytes)
        if i + 7 <= len && &bytes[i..i + 7] == b"styles." {
            // Check that this isn't in the middle of a word (e.g., "myStyles.xxx")
            if i > 0 && is_ident_char(bytes[i - 1]) {
                i += 1;
                continue;
            }

            let start = i + 7;
            if start >= len || !is_ident_start(bytes[start]) {
                i += 1;
                continue;
            }

            // Read the first identifier (ASCII-only, so byte offsets = char offsets)
            let first_end = read_ident(bytes, start);
            // Safety: start..first_end spans only ASCII identifier chars
            let first = &source[start..first_end];

            if first == "modifiers" {
                // styles.modifiers.xxx
                if first_end < len && bytes[first_end] == b'.' {
                    let mod_start = first_end + 1;
                    if mod_start < len && is_ident_start(bytes[mod_start]) {
                        let mod_end = read_ident(bytes, mod_start);
                        let modifier = source[mod_start..mod_end].to_string();
                        let key = format!("modifiers.{modifier}");
                        if seen.insert(key) {
                            tokens.push(StyleToken::Modifier(modifier));
                        }
                    }
                }
            } else {
                // styles.xxx (a block or element token)
                //
                // Template literal detection: in patterns like
                // `${styles.form}__alert`, the component constructs a BEM
                // element class via string interpolation. The actual rendered
                // class is `pf-v6-c-form__alert`, NOT `pf-v6-c-form` (the
                // root). Record the composed camelCase token (e.g.,
                // `formAlert`) so the CSS element map correctly maps this
                // component to its BEM element instead of the root.
                let token = if first_end < len
                    && bytes[first_end] == b'}'
                    && first_end + 3 <= len
                    && bytes[first_end + 1] == b'_'
                    && bytes[first_end + 2] == b'_'
                {
                    // Read the BEM element suffix after `}__`
                    let suffix_start = first_end + 3;
                    // BEM suffixes can contain hyphens (kebab-case), so read
                    // until we hit a non-ident, non-hyphen character.
                    let mut suffix_end = suffix_start;
                    while suffix_end < len
                        && (is_ident_char(bytes[suffix_end]) || bytes[suffix_end] == b'-')
                    {
                        suffix_end += 1;
                    }
                    if suffix_end > suffix_start {
                        let suffix = &source[suffix_start..suffix_end];
                        // Convert kebab-case suffix to camelCase with leading
                        // uppercase: "helper-text" → "HelperText"
                        let camel_suffix = super::kebab_to_camel_case(&capitalize_first(suffix));
                        format!("{first}{camel_suffix}")
                    } else {
                        first.to_string()
                    }
                } else {
                    first.to_string()
                };
                if seen.insert(token.clone()) {
                    tokens.push(StyleToken::ClassToken(token));
                }
            }

            i = first_end;
        } else {
            i += 1;
        }
    }

    tokens
}

/// A parsed style token reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StyleToken {
    /// A class token (e.g., "menu", "menuList", "menuItemMain").
    ClassToken(String),
    /// A modifier token (e.g., "expanded", "disabled").
    Modifier(String),
}

/// Extract the BEM block name from the `styles` import path.
///
/// Pattern: `import styles from '@patternfly/react-styles/css/components/Menu/menu'`
/// → block = `"menu"` (last path segment)
///
/// Returns `None` if no matching import is found.
pub fn extract_bem_block_from_import(source: &str) -> Option<String> {
    for line in source.lines() {
        let trimmed = line.trim();

        // Match: import styles from '...' or import styles from "..."
        // Also matches: import xxxStyles from '...'
        if !trimmed.starts_with("import ") {
            continue;
        }

        // Must import from @patternfly/react-styles/css/
        if !trimmed.contains("@patternfly/react-styles/css/") {
            continue;
        }

        // Must be the main `styles` binding (not e.g., `type` imports)
        // Check for `import styles from` or `import xxxStyles from`
        let after_import = trimmed.strip_prefix("import ")?;
        let binding = after_import.split_whitespace().next()?;

        // Accept `styles` or anything ending in `Styles` (e.g., `breadcrumbStyles`)
        // but only use the primary `styles` binding for the block name
        if binding != "styles" {
            continue;
        }

        // Extract the path: last segment before the quote
        let quote_start = trimmed.rfind('\'')?;
        let path = &trimmed[..quote_start];
        let block = path.rsplit('/').next()?;

        return Some(block.to_string());
    }

    None
}

/// Parse BEM structure from a set of style tokens.
///
/// If `block_override` is provided (from the styles import path), use it
/// as the authoritative block name instead of guessing from the shortest
/// token. This is the ground truth — the import path maps directly to the
/// CSS file that defines the BEM block.
///
/// Example with block_override = Some("menu"):
/// ```text
/// tokens: ["menu", "menuList", "menuListItem", "divider"]
/// block:  "menu"
/// elements: ["list", "listItem"]
/// (divider is NOT an element — doesn't start with "menu" + uppercase)
/// ```
pub fn parse_bem_structure(tokens: &[StyleToken], block_override: Option<&str>) -> BemStructure {
    let mut structure = BemStructure::default();

    // Collect class tokens and modifiers separately
    let mut class_tokens: Vec<&str> = Vec::new();
    for token in tokens {
        match token {
            StyleToken::ClassToken(name) => {
                structure.raw_tokens.insert(name.clone());
                class_tokens.push(name);
            }
            StyleToken::Modifier(name) => {
                structure.modifiers.insert(name.clone());
            }
        }
    }

    if class_tokens.is_empty() {
        return structure;
    }

    // Determine block name: prefer import-derived override, fall back to
    // shortest-token heuristic.
    let block = if let Some(override_block) = block_override {
        override_block.to_string()
    } else {
        class_tokens.sort_by_key(|t| t.len());
        class_tokens[0].to_string()
    };

    structure.block = Some(block.clone());

    // Elements are tokens that start with the block name, with the
    // remaining suffix starting with an uppercase letter (camelCase join)
    for token in &class_tokens {
        if *token == block {
            continue;
        }
        if token.starts_with(&block) {
            let suffix = &token[block.len()..];
            if suffix.starts_with(|c: char| c.is_uppercase()) {
                let element = lowercase_first(suffix);
                structure.elements.insert(element);
            }
        }
    }

    structure
}

/// Map a rendered component to its BEM relationship based on the
/// component's own style tokens vs the parent's block name.
///
/// If the child has its own distinct BEM block (extracted from its own
/// CSS import), it is always classified as `Independent` — even if its
/// camelCase tokens happen to share a prefix with the parent block.
/// This prevents false positives from naming collisions like:
///   - `label-group` → camelCase `labelGroup` → looks like element `Group` of block `label`
///   - `alert-group` → camelCase `alertGroup` → looks like element `Group` of block `alert`
///   - `menu-toggle` → camelCase `menuToggle` → looks like element `Toggle` of block `menu`
///
/// If the child does NOT have its own block (shares the parent's CSS file),
/// token prefix matching determines whether it's a BEM element.
pub fn classify_bem_relationship(
    child_block: Option<&str>,
    child_tokens: &BTreeSet<String>,
    parent_block: &str,
) -> BemRelationship {
    // If the child has its own distinct BEM block (from its own CSS import),
    // it's an independent component regardless of token name coincidences.
    // A component that imports its own stylesheet is by definition a separate
    // BEM block, not an element of another block.
    if let Some(block) = child_block {
        if block != parent_block {
            return BemRelationship::Independent {
                block_name: block.to_string(),
            };
        }
    }

    // Only do token prefix matching when the child shares the parent's BEM
    // block (or has no block of its own). In this case, tokens that start
    // with the parent block name followed by an uppercase letter are BEM
    // elements (e.g., `menuList` is element `list` of block `menu`).
    for token in child_tokens {
        if let Some(suffix) = token.strip_prefix(parent_block) {
            if suffix.starts_with(|c: char| c.is_uppercase()) {
                return BemRelationship::Element {
                    element_name: lowercase_first(suffix),
                };
            }
        }
    }

    BemRelationship::Unknown
}

/// BEM relationship between a child component and a parent component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BemRelationship {
    /// Child is a BEM element of the parent's block.
    /// e.g., MenuList (menuList) is element "list" of Menu (menu).
    Element { element_name: String },
    /// Child has its own independent BEM block.
    /// e.g., MenuToggle (menuToggle) is independent from Dropdown.
    Independent { block_name: String },
    /// Couldn't determine relationship from BEM tokens.
    Unknown,
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn read_ident(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() && is_ident_char(bytes[end]) {
        end += 1;
    }
    end
}

fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_lowercase().to_string() + chars.as_str(),
    }
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_style_tokens() {
        let source = r#"
            <div className={css(styles.menu, isPlain && styles.modifiers.plain)}>
                <ul className={css(styles.menuList)}>
                    <li className={css(styles.menuListItem, styles.modifiers.disabled)}>
                        <span className={css(styles.menuItemMain)} />
                    </li>
                </ul>
            </div>
        "#;

        let tokens = extract_style_tokens(source);
        assert!(tokens.contains(&StyleToken::ClassToken("menu".into())));
        assert!(tokens.contains(&StyleToken::ClassToken("menuList".into())));
        assert!(tokens.contains(&StyleToken::ClassToken("menuListItem".into())));
        assert!(tokens.contains(&StyleToken::ClassToken("menuItemMain".into())));
        assert!(tokens.contains(&StyleToken::Modifier("plain".into())));
        assert!(tokens.contains(&StyleToken::Modifier("disabled".into())));
    }

    #[test]
    fn test_parse_bem_structure() {
        let tokens = vec![
            StyleToken::ClassToken("menu".into()),
            StyleToken::ClassToken("menuList".into()),
            StyleToken::ClassToken("menuListItem".into()),
            StyleToken::ClassToken("menuItemMain".into()),
            StyleToken::Modifier("expanded".into()),
            StyleToken::Modifier("disabled".into()),
        ];

        let bem = parse_bem_structure(&tokens, None);
        assert_eq!(bem.block, Some("menu".into()));
        assert!(bem.elements.contains("list"));
        assert!(bem.elements.contains("listItem"));
        assert!(bem.elements.contains("itemMain"));
        assert!(bem.modifiers.contains("expanded"));
        assert!(bem.modifiers.contains("disabled"));
    }

    #[test]
    fn test_parse_bem_modal_box() {
        let tokens = vec![
            StyleToken::ClassToken("modalBox".into()),
            StyleToken::ClassToken("modalBoxBody".into()),
            StyleToken::ClassToken("modalBoxHeader".into()),
            StyleToken::ClassToken("modalBoxFooter".into()),
            StyleToken::ClassToken("modalBoxDescription".into()),
            StyleToken::ClassToken("modalBoxClose".into()),
        ];

        let bem = parse_bem_structure(&tokens, None);
        assert_eq!(bem.block, Some("modalBox".into()));
        assert!(bem.elements.contains("body"));
        assert!(bem.elements.contains("header"));
        assert!(bem.elements.contains("footer"));
        assert!(bem.elements.contains("description"));
        assert!(bem.elements.contains("close"));
    }

    #[test]
    fn test_classify_bem_element() {
        let child_tokens: BTreeSet<String> = vec!["menuList".to_string()].into_iter().collect();

        let rel = classify_bem_relationship(None, &child_tokens, "menu");
        assert_eq!(
            rel,
            BemRelationship::Element {
                element_name: "list".into()
            }
        );
    }

    #[test]
    fn test_extract_style_tokens_with_utf8() {
        // Regression test: source containing multi-byte UTF-8 characters
        // (e.g., ©) must not cause a panic when scanning for "styles."
        let source = r#"
/**
 * © Copyright 2024 Company
 * Licensed under Apache License
 */
import styles from './component.css';

const Component = () => (
    <div className={styles.menu}>
        <span className={styles.menuList} />
    </div>
);
"#;
        let tokens = extract_style_tokens(source);
        assert!(tokens.contains(&StyleToken::ClassToken("menu".into())));
        assert!(tokens.contains(&StyleToken::ClassToken("menuList".into())));
    }

    #[test]
    fn test_classify_bem_independent() {
        let child_tokens: BTreeSet<String> = vec!["menuToggle".to_string()].into_iter().collect();

        // menuToggle starts with "menu" so this would look like an element.
        // But if the child has its own block "menuToggle", it's independent.
        let rel = classify_bem_relationship(Some("menuToggle"), &child_tokens, "dropdown");
        assert_eq!(
            rel,
            BemRelationship::Independent {
                block_name: "menuToggle".into()
            }
        );
    }

    // ── BEM collision regression tests ──────────────────────────────────
    //
    // These tests verify that components with their own distinct BEM blocks
    // are classified as Independent even when their camelCase tokens happen
    // to share a prefix with the parent's block name.

    #[test]
    fn test_label_labelgroup_collision_returns_independent() {
        // LabelGroup imports its own CSS: label-group.css → block "label-group"
        // Its tokens like "labelGroup", "labelGroupList" share prefix "label"
        // with the Label block. Without the fix, "labelGroup" stripped of
        // "label" gives "Group" (uppercase) → falsely classified as Element.
        let child_tokens: BTreeSet<String> = vec![
            "labelGroup".to_string(),
            "labelGroupLabel".to_string(),
            "labelGroupList".to_string(),
            "labelGroupListItem".to_string(),
            "labelGroupClose".to_string(),
            "labelGroupMain".to_string(),
        ]
        .into_iter()
        .collect();

        let rel = classify_bem_relationship(Some("label-group"), &child_tokens, "label");
        assert_eq!(
            rel,
            BemRelationship::Independent {
                block_name: "label-group".into()
            },
            "LabelGroup has its own BEM block 'label-group' — must be Independent, not Element"
        );
    }

    #[test]
    fn test_alert_alertgroup_collision_returns_independent() {
        // AlertGroup imports its own CSS: alert-group.css → block "alert-group"
        // Same collision pattern as Label/LabelGroup.
        let child_tokens: BTreeSet<String> =
            vec!["alertGroup".to_string(), "alertGroupItem".to_string()]
                .into_iter()
                .collect();

        let rel = classify_bem_relationship(Some("alert-group"), &child_tokens, "alert");
        assert_eq!(
            rel,
            BemRelationship::Independent {
                block_name: "alert-group".into()
            },
            "AlertGroup has its own BEM block 'alert-group' — must be Independent, not Element"
        );
    }

    #[test]
    fn test_menu_menutoggle_collision_returns_independent() {
        // MenuToggle imports its own CSS: menu-toggle.css → block "menu-toggle"
        // "menuToggle" stripped of "menu" gives "Toggle" (uppercase) → collision.
        let child_tokens: BTreeSet<String> = vec![
            "menuToggle".to_string(),
            "menuToggleIcon".to_string(),
            "menuToggleCount".to_string(),
        ]
        .into_iter()
        .collect();

        let rel = classify_bem_relationship(Some("menu-toggle"), &child_tokens, "menu");
        assert_eq!(
            rel,
            BemRelationship::Independent {
                block_name: "menu-toggle".into()
            },
            "MenuToggle has its own BEM block 'menu-toggle' — must be Independent, not Element"
        );
    }

    #[test]
    fn test_form_formcontrol_collision_returns_independent() {
        // FormControl imports its own CSS: form-control.css → block "form-control"
        let child_tokens: BTreeSet<String> =
            vec!["formControl".to_string(), "formControlIcon".to_string()]
                .into_iter()
                .collect();

        let rel = classify_bem_relationship(Some("form-control"), &child_tokens, "form");
        assert_eq!(
            rel,
            BemRelationship::Independent {
                block_name: "form-control".into()
            },
            "FormControl has its own BEM block 'form-control' — must be Independent, not Element"
        );
    }

    // ── Ensure true BEM elements still work ─────────────────────────────

    #[test]
    fn test_true_bem_element_same_block() {
        // MenuList shares the "menu" block (imports from menu.css, not its own CSS).
        // child_block is None because it uses the parent's CSS file.
        // Token "menuList" → element "list" of block "menu". This is correct.
        let child_tokens: BTreeSet<String> =
            vec!["menuList".to_string(), "menuListItem".to_string()]
                .into_iter()
                .collect();

        let rel = classify_bem_relationship(None, &child_tokens, "menu");
        assert_eq!(
            rel,
            BemRelationship::Element {
                element_name: "list".into()
            },
            "MenuList without its own block should be classified as BEM element of menu"
        );
    }

    #[test]
    fn test_true_bem_element_with_same_block_name() {
        // When child_block == parent_block, the child shares the parent's CSS.
        // Token prefix matching should still identify elements.
        let child_tokens: BTreeSet<String> = vec!["toolbarGroup".to_string()].into_iter().collect();

        let rel = classify_bem_relationship(Some("toolbar"), &child_tokens, "toolbar");
        assert_eq!(
            rel,
            BemRelationship::Element {
                element_name: "group".into()
            },
            "ToolbarGroup with same block as parent should be a BEM element"
        );
    }

    #[test]
    fn test_no_matching_tokens_no_block_returns_unknown() {
        // Child has no tokens matching parent block and no own block.
        let child_tokens: BTreeSet<String> = vec!["divider".to_string()].into_iter().collect();

        let rel = classify_bem_relationship(None, &child_tokens, "menu");
        assert_eq!(
            rel,
            BemRelationship::Unknown,
            "Unrelated tokens with no own block should be Unknown"
        );
    }

    #[test]
    fn test_independent_block_no_token_collision() {
        // Child has its own block and tokens that DON'T share prefix with parent.
        // This should be Independent regardless (no collision to worry about).
        let child_tokens: BTreeSet<String> =
            vec!["pagination".to_string(), "paginationNav".to_string()]
                .into_iter()
                .collect();

        let rel = classify_bem_relationship(Some("pagination"), &child_tokens, "table");
        assert_eq!(
            rel,
            BemRelationship::Independent {
                block_name: "pagination".into()
            },
            "Component with own block unrelated to parent should be Independent"
        );
    }

    #[test]
    fn test_empty_tokens_with_own_block_returns_independent() {
        // Edge case: child has a block but no tokens at all.
        let child_tokens: BTreeSet<String> = BTreeSet::new();

        let rel = classify_bem_relationship(Some("badge"), &child_tokens, "button");
        assert_eq!(
            rel,
            BemRelationship::Independent {
                block_name: "badge".into()
            },
            "Component with own block but no tokens should still be Independent"
        );
    }

    #[test]
    fn test_empty_tokens_no_block_returns_unknown() {
        let child_tokens: BTreeSet<String> = BTreeSet::new();

        let rel = classify_bem_relationship(None, &child_tokens, "button");
        assert_eq!(
            rel,
            BemRelationship::Unknown,
            "No tokens and no block should be Unknown"
        );
    }

    // ── Template literal BEM token extraction ───────────────────────

    #[test]
    fn template_literal_composes_bem_element() {
        // `${styles.form}__alert` → "formAlert", not "form"
        let source = r#"<div className={css(`${styles.form}__alert`, className)}>"#;
        let tokens = extract_style_tokens(source);
        assert!(
            tokens.contains(&StyleToken::ClassToken("formAlert".into())),
            "Expected 'formAlert' from template literal. Got: {:?}",
            tokens
        );
        assert!(
            !tokens.contains(&StyleToken::ClassToken("form".into())),
            "Should NOT record bare 'form' when used in template literal"
        );
    }

    #[test]
    fn template_literal_kebab_suffix() {
        // `${styles.fileUpload}__helper-text` → "fileUploadHelperText"
        let source = r#"<div className={css(`${styles.fileUpload}__helper-text`)}>"#;
        let tokens = extract_style_tokens(source);
        assert!(
            tokens.contains(&StyleToken::ClassToken("fileUploadHelperText".into())),
            "Expected 'fileUploadHelperText' from kebab suffix. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn direct_styles_ref_unchanged() {
        // css(styles.form, className) → "form" (no template literal)
        let source = r#"<div className={css(styles.form, className)}>"#;
        let tokens = extract_style_tokens(source);
        assert!(
            tokens.contains(&StyleToken::ClassToken("form".into())),
            "Direct styles.form should still record 'form'. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn both_direct_and_template_in_same_file() {
        // A file using styles.form directly AND ${styles.form}__alert
        let source = r#"
            <form className={css(styles.form, className)}>
                <div className={css(`${styles.form}__alert`)}>
        "#;
        let tokens = extract_style_tokens(source);
        assert!(
            tokens.contains(&StyleToken::ClassToken("form".into())),
            "Direct use should record 'form'. Got: {:?}",
            tokens
        );
        assert!(
            tokens.contains(&StyleToken::ClassToken("formAlert".into())),
            "Template use should record 'formAlert'. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn template_literal_single_word_suffix() {
        // `${styles.emptyState}__header` → "emptyStateHeader"
        let source = r#"<div className={css(`${styles.emptyState}__header`)}>"#;
        let tokens = extract_style_tokens(source);
        assert!(
            tokens.contains(&StyleToken::ClassToken("emptyStateHeader".into())),
            "Expected 'emptyStateHeader'. Got: {:?}",
            tokens
        );
    }
}
