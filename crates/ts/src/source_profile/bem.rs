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
                let token = first.to_string();
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
/// If the component's primary token is an element of the parent block
/// (starts with parent block name), it's a BEM element relationship.
/// If it has its own independent block, it's an independent block.
pub fn classify_bem_relationship(
    child_block: Option<&str>,
    child_tokens: &BTreeSet<String>,
    parent_block: &str,
) -> BemRelationship {
    // Check if any of the child's tokens are elements of the parent block
    for token in child_tokens {
        if token.starts_with(parent_block) {
            let suffix = &token[parent_block.len()..];
            if suffix.starts_with(|c: char| c.is_uppercase()) {
                return BemRelationship::Element {
                    element_name: lowercase_first(suffix),
                };
            }
        }
    }

    // If the child has its own block, it's independent
    if let Some(block) = child_block {
        if block != parent_block {
            return BemRelationship::Independent {
                block_name: block.to_string(),
            };
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
}
