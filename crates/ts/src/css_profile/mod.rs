//! CSS profile extraction from a dependency repository.
//!
//! Parses CSS files (e.g., from `@patternfly/patternfly`) to extract
//! structural information that enriches React component composition trees:
//!
//! 1. **Grid/flex nesting**: Elements with `grid-column` are direct grid
//!    children; elements without must be nested inside a grid item.
//!    Elements that switch between `display: contents` and `display: flex`
//!    are mode-switching containers whose children get promoted/demoted.
//!
//! 2. **`:has()` selectors**: Explicit containment proof
//!    (e.g., `.masthead__main:has(.masthead__toggle)` → toggle inside main)
//!
//! 3. **Descendant selectors**: Nesting relationships between BEM elements
//!    (e.g., `.menu__breadcrumb .menu__content` → content inside breadcrumb)

use anyhow::Result;
use lightningcss::properties::display::{Display, DisplayKeyword};
use lightningcss::properties::Property;
use lightningcss::rules::CssRule;
use lightningcss::selector::{Combinator, Component, Selector};
use lightningcss::stylesheet::{ParserOptions, StyleSheet};
use lightningcss::traits::ParseWithOptions;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::process::Command;
use tracing::{debug, info, warn};

/// CSS-level profile for a BEM block (one per component CSS file).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CssBlockProfile {
    /// BEM block name (e.g., "masthead", "menu", "modal-box").
    pub block: String,

    /// BEM elements found in the CSS with their layout properties.
    pub elements: BTreeMap<String, CssElementInfo>,

    /// Containment relationships from `:has()` selectors.
    /// Key: parent element, Value: child element that must be inside it.
    pub has_containment: Vec<(String, String)>,

    /// Descendant relationships from CSS descendant combinators.
    /// `parent__element child__element` → parent contains child.
    pub descendant_nesting: Vec<(String, String)>,

    /// Sibling relationships from `~` or `+` combinators.
    pub sibling_relationships: Vec<(String, String)>,
}

/// Layout-relevant CSS properties for a BEM element.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CssElementInfo {
    /// Whether this element has a `grid-column` assignment.
    pub has_grid_column: bool,

    /// Whether this element's grid-column reverts to `initial`/`unset`/`revert`
    /// in some mode. This means the element is inside a mode-switching
    /// container and only participates in the grid when the container
    /// has `display: contents`.
    pub grid_column_reverts: bool,

    /// Whether this element has a `grid-row` assignment.
    pub has_grid_row: bool,

    /// Display values seen (may vary by mode/breakpoint).
    pub display_values: BTreeSet<String>,

    /// Whether this element switches between `display: contents` and
    /// another display value (flex/grid). This indicates it's a
    /// mode-switching container.
    pub is_mode_switcher: bool,

    /// Whether this element has `flex-shrink: 0` — a rigid container
    /// that maintains dimensions for its content (like a brand/logo area).
    pub flex_shrink_zero: bool,

    /// Whether this element has `flex-wrap: wrap` — a flexible layout
    /// container for wrapping content (like a toolbar area).
    pub flex_wrap: bool,

    /// Whether this element has explicit sizing (width, max-width,
    /// max-height) — indicates it's a leaf content element.
    pub has_sizing: bool,

    /// CSS custom property names that reference this element in a
    /// multi-element path (e.g., `--masthead__main--toggle--content`
    /// tells us toggle and content relate to main).
    pub variable_child_refs: BTreeSet<String>,
}

/// Extract CSS profiles from a dependency repo's component CSS files.
///
/// Reads CSS files at `components/*/` paths in the repo at the given ref,
/// parses them with lightningcss, and extracts structural information.
pub fn extract_css_profiles(
    repo: &Path,
    git_ref: &str,
) -> Result<HashMap<String, CssBlockProfile>> {
    let mut profiles = HashMap::new();

    // Find all component CSS files at the ref
    let css_files = find_component_css_files(repo, git_ref)?;
    info!(count = css_files.len(), "component CSS files found");

    for (component_dir, css_path) in &css_files {
        let Some(source) = read_git_file(repo, git_ref, css_path) else {
            continue;
        };

        match extract_css_block_profile(&source, component_dir) {
            Ok(profile) => {
                debug!(
                    block = %profile.block,
                    elements = profile.elements.len(),
                    has_rules = profile.has_containment.len(),
                    "CSS profile extracted"
                );
                profiles.insert(profile.block.clone(), profile);
            }
            Err(e) => {
                warn!(file = %css_path, %e, "failed to parse CSS");
            }
        }
    }

    info!(profiles = profiles.len(), "CSS profiles extracted");
    Ok(profiles)
}

/// Extract CSS profiles from a filesystem directory of compiled CSS.
///
/// Walks `dir/components/*/` looking for `.css` files. This is the typical
/// layout of an npm package (e.g., `@patternfly/patternfly/components/`).
pub fn extract_css_profiles_from_dir(dir: &Path) -> Result<HashMap<String, CssBlockProfile>> {
    let mut profiles = HashMap::new();

    let components_dir = if dir.join("components").exists() {
        dir.join("components")
    } else {
        dir.to_path_buf()
    };

    if !components_dir.exists() {
        warn!(path = %components_dir.display(), "CSS components directory not found");
        return Ok(profiles);
    }

    for entry in std::fs::read_dir(&components_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let component_dir_name = entry.file_name().to_string_lossy().to_string();

        // Find the main CSS file in this component directory
        for css_entry in std::fs::read_dir(entry.path())? {
            let css_entry = css_entry?;
            let css_path = css_entry.path();
            if !css_path.extension().map_or(false, |e| e == "css") {
                continue;
            }
            // Skip minified, sourcemaps, examples
            let fname = css_path.file_name().unwrap_or_default().to_string_lossy();
            if fname.contains(".min.") || fname.contains(".map") {
                continue;
            }

            match std::fs::read_to_string(&css_path) {
                Ok(source) => match extract_css_block_profile(&source, &component_dir_name) {
                    Ok(profile) => {
                        debug!(
                            block = %profile.block,
                            elements = profile.elements.len(),
                            file = %css_path.display(),
                            "CSS profile extracted from dir"
                        );
                        profiles.insert(profile.block.clone(), profile);
                        break; // One CSS file per component dir
                    }
                    Err(e) => {
                        warn!(file = %css_path.display(), %e, "failed to parse CSS");
                    }
                },
                Err(e) => {
                    warn!(file = %css_path.display(), %e, "failed to read CSS file");
                }
            }
        }
    }

    info!(profiles = profiles.len(), "CSS profiles extracted from dir");
    Ok(profiles)
}

/// Find component CSS files in the dependency repo.
///
/// Generic approach: find all `.css` files under a `components/` directory
/// (at any depth). Each CSS file in a component directory is treated as
/// a potential block definition. The component directory name is used as
/// a hint but the actual block class is detected from the CSS itself.
///
/// Also supports compiled output directories (`dist/`) where CSS files
/// may be placed after a build step.
fn find_component_css_files(repo: &Path, git_ref: &str) -> Result<Vec<(String, String)>> {
    let output = Command::new("git")
        .args([
            "-C",
            &repo.to_string_lossy(),
            "ls-tree",
            "-r",
            "--name-only",
            git_ref,
        ])
        .output()?;

    if !output.status.success() {
        anyhow::bail!("git ls-tree failed");
    }

    let mut seen_dirs = std::collections::HashSet::new();
    let files: Vec<(String, String)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let path = line.trim();
            if !path.ends_with(".css") {
                return None;
            }
            // Skip minified, sourcemap, example, and test files
            if path.contains(".min.css")
                || path.contains(".map")
                || path.contains("/examples/")
                || path.contains("/test")
            {
                return None;
            }

            // Look for CSS files under a components/ or dist/components/ directory
            let parts: Vec<&str> = path.split('/').collect();
            let comp_idx = parts.iter().position(|&p| p == "components")?;
            if comp_idx + 2 >= parts.len() {
                return None;
            }
            let component_dir = parts[comp_idx + 1].to_string();

            // Only take one CSS file per component directory (the main one)
            if !seen_dirs.insert(component_dir.clone()) {
                return None;
            }

            Some((component_dir, path.to_string()))
        })
        .collect();

    Ok(files)
}

/// Extract a CSS block profile from a CSS source string.
///
/// The block class is detected automatically from the CSS — we find the
/// first standalone class selector used as a top-level rule (the BEM block).
/// No prefix knowledge is required.
fn extract_css_block_profile(source: &str, _component_dir: &str) -> Result<CssBlockProfile> {
    // Parse with lightningcss
    let stylesheet = StyleSheet::parse(source, ParserOptions::default())
        .map_err(|e| anyhow::anyhow!("CSS parse error: {}", e))?;

    // Step 1: Detect the block class from the first standalone selector
    let block_class = detect_block_class(&stylesheet)
        .ok_or_else(|| anyhow::anyhow!("could not detect block class from CSS"))?;

    // Derive the camelCase block name for the profile
    // e.g., "pf-v6-c-masthead" → strip known prefixes → "masthead"
    //        "pf-v6-c-modal-box" → "modalBox"
    //        "my-component" → "myComponent" (generic)
    let block_name = derive_block_name(&block_class);

    let mut profile = CssBlockProfile {
        block: block_name,
        ..Default::default()
    };

    // Step 2: Walk all rules using the detected block class as prefix
    for rule in &stylesheet.rules.0 {
        extract_from_rule(rule, &block_class, &mut profile);
    }

    // Step 3: Detect mode-switchers (display: contents ↔ flex/grid)
    for (_name, info) in &mut profile.elements {
        let has_contents = info.display_values.contains("contents");
        let has_flex_or_grid =
            info.display_values.contains("flex") || info.display_values.contains("grid");
        // Also count "var" as potential mode-switcher (var-driven display)
        let has_var_display = info.display_values.contains("var");
        info.is_mode_switcher = has_contents && has_flex_or_grid
            || has_var_display && (has_contents || has_flex_or_grid);
    }

    // Step 4: Extract variable child refs from CSS custom property names
    // Variable prefix = "--" + block_class (e.g., "--pf-v6-c-masthead")
    extract_variable_nesting(source, &block_class, &mut profile);

    // Step 5: Detect elements whose grid-column reverts in some mode.
    // Pattern: --{block}--m-*__{element}--GridColumn: initial/unset/revert
    detect_grid_column_reverts(source, &block_class, &mut profile);

    Ok(profile)
}

/// Detect the BEM block class from the stylesheet.
///
/// Finds the first standalone class selector (`.something { ... }`) that
/// doesn't contain `__` (element) or start with a modifier pattern.
/// This is the block class — no prefix knowledge required.
fn detect_block_class(stylesheet: &StyleSheet) -> Option<String> {
    for rule in &stylesheet.rules.0 {
        if let CssRule::Style(style_rule) = rule {
            for selector in style_rule.selectors.0.iter() {
                // Look for a simple selector with just one class component
                let components: Vec<&Component> = selector.iter().collect();

                // A standalone block selector has exactly one class component
                // (or one class + modifier), no combinators to other blocks
                if let Some(Component::Class(class_name)) = components.first() {
                    let name = class_name.as_ref();
                    // Skip modifiers, pseudo-elements, etc.
                    if name.contains("__") || name.starts_with("pf-m-") {
                        continue;
                    }
                    // This looks like a block class
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

/// Derive a camelCase block name from a CSS class string.
///
/// Strips common prefixes (pf-v5-c-, pf-v6-c-, etc.) and converts
/// kebab-case to camelCase. For unknown prefixes, converts the whole
/// class to camelCase.
fn derive_block_name(block_class: &str) -> String {
    // Try stripping known PF prefixes
    let stripped = block_class
        .strip_prefix("pf-v6-c-")
        .or_else(|| block_class.strip_prefix("pf-v5-c-"))
        .or_else(|| block_class.strip_prefix("pf-c-"))
        // Generic: strip any prefix matching X-X-X- pattern (vendor-version-type-)
        .unwrap_or(block_class);

    kebab_to_camel(stripped)
}

/// Walk a CSS rule, extracting layout properties and selector relationships.
fn extract_from_rule(rule: &CssRule, class_prefix: &str, profile: &mut CssBlockProfile) {
    match rule {
        CssRule::Style(style_rule) => {
            // Extract selector relationships
            for selector in style_rule.selectors.0.iter() {
                extract_selector_relationships(selector, class_prefix, profile);
            }

            // Extract layout properties per element
            for selector in style_rule.selectors.0.iter() {
                if let Some(element_name) = extract_element_from_selector(selector, class_prefix) {
                    let info = profile
                        .elements
                        .entry(element_name)
                        .or_insert_with(CssElementInfo::default);

                    for property in style_rule.declarations.declarations.iter() {
                        match property {
                            Property::GridColumn(..) => {
                                info.has_grid_column = true;
                            }
                            Property::GridRow(..) => {
                                info.has_grid_row = true;
                            }
                            Property::Display(display) => {
                                let display_str = match display {
                                    Display::Keyword(DisplayKeyword::None) => "none",
                                    Display::Pair(pair) => {
                                        let s = format!("{:?}", pair);
                                        if s.contains("Flex") {
                                            "flex"
                                        } else if s.contains("Grid") {
                                            "grid"
                                        } else if s.contains("Contents") {
                                            "contents"
                                        } else {
                                            "other"
                                        }
                                    }
                                    _ => "other",
                                };
                                info.display_values.insert(display_str.to_string());
                            }
                            // Flex properties
                            Property::FlexShrink(shrink, _) => {
                                if *shrink == 0.0 {
                                    info.flex_shrink_zero = true;
                                }
                            }
                            Property::FlexWrap(wrap, _) => {
                                let s = format!("{:?}", wrap);
                                if s.contains("Wrap") && !s.contains("NoWrap") {
                                    info.flex_wrap = true;
                                }
                            }
                            // Sizing properties
                            Property::Width(..)
                            | Property::MaxWidth(..)
                            | Property::MaxHeight(..) => {
                                info.has_sizing = true;
                            }
                            // PF uses `grid-column: var(...)` and `display: var(...)`
                            // which lightningcss represents as Unparsed properties
                            Property::Unparsed(unparsed) => {
                                let prop_name = unparsed.property_id.name();
                                if prop_name == "grid-column"
                                    || prop_name == "grid-column-start"
                                    || prop_name == "grid-column-end"
                                {
                                    info.has_grid_column = true;
                                } else if prop_name == "grid-row"
                                    || prop_name == "grid-row-start"
                                    || prop_name == "grid-row-end"
                                {
                                    info.has_grid_row = true;
                                } else if prop_name == "display" {
                                    info.display_values.insert("var".to_string());
                                } else if prop_name == "width"
                                    || prop_name == "max-width"
                                    || prop_name == "max-height"
                                {
                                    info.has_sizing = true;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        CssRule::Media(media_rule) => {
            for inner_rule in &media_rule.rules.0 {
                extract_from_rule(inner_rule, class_prefix, profile);
            }
        }
        _ => {}
    }
}

/// Extract the BEM element name from a selector's last class component.
///
/// `.pf-v6-c-masthead__main` → Some("main")
/// `.pf-v6-c-masthead` → Some("") (block itself)
/// `.pf-v6-c-button` → None (different block)
fn extract_element_from_selector(selector: &Selector, class_prefix: &str) -> Option<String> {
    // Walk selector components to find the last class matching our prefix
    for component in selector.iter() {
        if let Component::Class(class_name) = component {
            let name = class_name.as_ref();
            if let Some(suffix) = name.strip_prefix(class_prefix) {
                if suffix.is_empty() {
                    return Some(String::new()); // Block itself
                }
                if let Some(element) = suffix.strip_prefix("__") {
                    // Strip any modifier (e.g., `__header.pf-m-help` → just "header")
                    let element = element.split('.').next().unwrap_or(element);
                    return Some(element.to_string());
                }
            }
        }
    }
    None
}

/// Extract containment and nesting from selector combinators.
fn extract_selector_relationships(
    selector: &Selector,
    class_prefix: &str,
    profile: &mut CssBlockProfile,
) {
    let components: Vec<&Component> = selector.iter().collect();

    // Look for :has() pseudo-class
    for component in &components {
        if let Component::Has(has_selectors) = component {
            // Find the parent (the element before :has())
            if let Some(parent_el) = extract_element_from_selector(selector, class_prefix) {
                for has_selector in has_selectors.iter() {
                    if let Some(child_el) =
                        extract_element_from_selector(has_selector, class_prefix)
                    {
                        if !parent_el.is_empty() && !child_el.is_empty() {
                            profile.has_containment.push((parent_el.clone(), child_el));
                        }
                    }
                }
            }
        }
    }

    // Look for descendant combinator (space) and sibling combinator (~)
    // Selectors iterate right-to-left, so we need to track pairs
    let mut prev_element: Option<String> = None;
    let mut prev_combinator: Option<Combinator> = None;

    for component in selector.iter() {
        match component {
            Component::Combinator(comb) => {
                prev_combinator = Some(*comb);
            }
            Component::Class(class_name) => {
                let name = class_name.as_ref();
                if let Some(suffix) = name.strip_prefix(class_prefix) {
                    let element = if suffix.is_empty() {
                        String::new()
                    } else if let Some(el) = suffix.strip_prefix("__") {
                        el.to_string()
                    } else {
                        continue;
                    };

                    if let (Some(child_el), Some(combinator)) = (&prev_element, &prev_combinator) {
                        if !element.is_empty() && !child_el.is_empty() {
                            match combinator {
                                Combinator::Descendant | Combinator::Child => {
                                    // parent (current) → child (prev)
                                    // Note: selector iterates right-to-left
                                    profile
                                        .descendant_nesting
                                        .push((element.clone(), child_el.clone()));
                                }
                                Combinator::NextSibling | Combinator::LaterSibling => {
                                    profile
                                        .sibling_relationships
                                        .push((element.clone(), child_el.clone()));
                                }
                                _ => {}
                            }
                        }
                    }

                    prev_element = Some(element);
                    prev_combinator = None;
                }
            }
            _ => {}
        }
    }
}

/// Extract nesting hints from CSS custom property names.
///
/// PF variable naming: `--pf-v6-c-{block}__{element}--{child}--{property}`
/// The `{element}--{child}` path encodes that `child` is related to `element`.
fn extract_variable_nesting(source: &str, class_prefix: &str, profile: &mut CssBlockProfile) {
    let var_prefix = format!("--{}", class_prefix);

    for line in source.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with(&var_prefix) {
            continue;
        }

        // Extract the variable name (up to the colon)
        let var_name = trimmed.split(':').next().unwrap_or(trimmed).trim();

        // Parse: --pf-v6-c-{block}__{element}--{child}--...
        if let Some(after_prefix) = var_name.strip_prefix(&var_prefix) {
            if let Some(element_part) = after_prefix.strip_prefix("__") {
                // Split on '--' to find element and child refs
                // e.g., "main--toggle--content--GridColumn"
                // → parent: "main", children: ["toggle", "content"]
                // Properties start with uppercase and are skipped
                let parts: Vec<&str> = element_part.split("--").collect();
                if parts.len() >= 2 {
                    let parent_element = parts[0];

                    let info = profile
                        .elements
                        .entry(parent_element.to_string())
                        .or_insert_with(CssElementInfo::default);

                    for child_ref in &parts[1..] {
                        // Stop at property names (start with uppercase)
                        if child_ref.chars().next().map_or(true, |c| c.is_uppercase()) {
                            break;
                        }
                        // Skip modifier markers
                        if *child_ref == "m" {
                            break;
                        }
                        info.variable_child_refs.insert(child_ref.to_string());
                    }
                }
            }
        }
    }
}

/// Detect elements whose grid-column variable reverts in some display mode.
///
/// Scans for CSS variable definitions matching:
///   `--{block}--m-display-{mode}__{element}--GridColumn: initial`
///
/// When an element's GridColumn is `initial`/`unset`/`revert` in some mode,
/// it means the element is promoted from inside a `display: contents`
/// container in another mode, and belongs inside that container.
fn detect_grid_column_reverts(source: &str, block_class: &str, profile: &mut CssBlockProfile) {
    let var_prefix = format!("--{}", block_class);

    for line in source.lines() {
        let trimmed = line.trim();

        // Look for variable definitions like:
        //   --pf-v6-c-masthead--m-display-inline__brand--GridColumn: initial;
        if !trimmed.starts_with(&var_prefix) {
            continue;
        }

        // Must contain GridColumn (or Order — Order: initial also indicates containment)
        if !trimmed.contains("GridColumn") && !trimmed.contains("Order") {
            continue;
        }

        // Check if the value is initial/unset/revert
        if let Some(colon_idx) = trimmed.find(':') {
            let value = trimmed[colon_idx + 1..].trim().trim_end_matches(';').trim();
            let is_revert = value == "initial"
                || value == "unset"
                || value == "revert"
                || value.starts_with("var(") && value.contains("initial");

            if !is_revert {
                continue;
            }

            // Extract the element name from the variable
            // Pattern: --block--m-*__{element}--GridColumn
            let var_name = &trimmed[..colon_idx].trim();
            if let Some(dunder_idx) = var_name.rfind("__") {
                let after_dunder = &var_name[dunder_idx + 2..];
                // Element name is before the --GridColumn/--Order part
                if let Some(prop_idx) = after_dunder.find("--") {
                    let element = &after_dunder[..prop_idx];
                    if !element.is_empty() {
                        let info = profile
                            .elements
                            .entry(element.to_string())
                            .or_insert_with(CssElementInfo::default);
                        info.grid_column_reverts = true;
                    }
                }
            }
        }
    }
}

/// Convert kebab-case to camelCase.
fn kebab_to_camel(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut capitalize_next = false;
    for ch in s.chars() {
        if ch == '-' {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(ch.to_ascii_uppercase());
            capitalize_next = false;
        } else {
            result.push(ch);
        }
    }
    result
}

/// Read a file from a git ref.
fn read_git_file(repo: &Path, git_ref: &str, file_path: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["show", &format!("{}:{}", git_ref, file_path)])
        .current_dir(repo)
        .output()
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_element_from_class() {
        assert_eq!(
            extract_element_from_class("pf-v6-c-masthead__main", "pf-v6-c-masthead"),
            Some("main".to_string())
        );
        assert_eq!(
            extract_element_from_class("pf-v6-c-masthead", "pf-v6-c-masthead"),
            Some(String::new())
        );
        assert_eq!(
            extract_element_from_class("pf-v6-c-button", "pf-v6-c-masthead"),
            None
        );
    }

    #[test]
    fn test_kebab_to_camel() {
        assert_eq!(kebab_to_camel("modal-box"), "modalBox");
        assert_eq!(kebab_to_camel("masthead"), "masthead");
        assert_eq!(kebab_to_camel("about-modal-box"), "aboutModalBox");
    }

    #[test]
    fn test_variable_nesting_extraction() {
        let source = r#"
.pf-v6-c-masthead {
  --pf-v6-c-masthead__main--toggle--content--GridColumn: 2;
  --pf-v6-c-masthead__main--Display: contents;
  --pf-v6-c-masthead__brand--GridColumn: -1 / 1;
  --pf-v6-c-masthead__logo--MaxHeight: 2rem;
}
"#;
        let mut profile = CssBlockProfile::default();
        extract_variable_nesting(source, "pf-v6-c-masthead", &mut profile);

        // __main has child refs: toggle, content (from main--toggle--content)
        let main_info = profile.elements.get("main").unwrap();
        assert!(
            main_info.variable_child_refs.contains("toggle"),
            "Expected 'toggle' in main's child refs: {:?}",
            main_info.variable_child_refs
        );
        assert!(main_info.variable_child_refs.contains("content"));
    }
}

/// Helper for tests — extract element name from a class string directly.
#[cfg(test)]
fn extract_element_from_class(class: &str, prefix: &str) -> Option<String> {
    if let Some(suffix) = class.strip_prefix(prefix) {
        if suffix.is_empty() {
            return Some(String::new());
        }
        if let Some(element) = suffix.strip_prefix("__") {
            return Some(element.to_string());
        }
    }
    None
}

/// Parse a CSS string and extract a block profile (public for testing).
#[cfg(test)]
pub fn parse_css_for_test(source: &str, component_dir: &str) -> Result<CssBlockProfile> {
    extract_css_block_profile(source, component_dir)
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    #[test]
    fn test_parse_real_masthead_css() {
        let source = std::fs::read_to_string("/tmp/package/components/Masthead/masthead.css")
            .expect("Need /tmp/package/components/Masthead/masthead.css - run: cd /tmp && curl -sL https://registry.npmjs.org/@patternfly/patternfly/-/patternfly-6.0.0.tgz | tar xzf -");

        let profile = extract_css_block_profile(&source, "Masthead").unwrap();

        println!("Block: {}", profile.block);
        println!(
            "Elements: {:?}",
            profile.elements.keys().collect::<Vec<_>>()
        );
        println!("\nElement details:");
        for (name, info) in &profile.elements {
            println!(
                "  {}: grid_col={}, display={:?}, mode_switch={}, var_children={:?}",
                name,
                info.has_grid_column,
                info.display_values,
                info.is_mode_switcher,
                info.variable_child_refs
            );
        }
        println!("\n:has() containment: {:?}", profile.has_containment);
        println!("Descendant nesting: {:?}", profile.descendant_nesting);
        println!("Sibling relationships: {:?}", profile.sibling_relationships);

        // Verify key structural signals
        // __main should be a mode-switcher or have variable display
        let main = profile.elements.get("main").expect("should have __main");
        assert!(
            !main.display_values.is_empty(),
            "main should have display values: {:?}",
            main
        );

        // __main should have toggle and content as variable child refs
        assert!(
            main.variable_child_refs.contains("toggle"),
            "main should reference toggle: {:?}",
            main.variable_child_refs
        );

        // __brand should have grid-column
        let brand = profile.elements.get("brand").expect("should have __brand");
        assert!(brand.has_grid_column, "brand should have grid-column");

        // __toggle should NOT have grid-column
        let toggle = profile.elements.get("toggle");
        if let Some(t) = toggle {
            assert!(!t.has_grid_column, "toggle should NOT have grid-column");
        }

        // __logo should NOT have grid-column
        let logo = profile.elements.get("logo");
        if let Some(l) = logo {
            assert!(!l.has_grid_column, "logo should NOT have grid-column");
        }

        // Variable child refs prove toggle relates to main
        // (from --pf-v6-c-masthead__main--toggle--content--GridColumn)
        assert!(
            main.variable_child_refs.contains("toggle"),
            "variable refs should show toggle inside main"
        );
        assert!(
            main.variable_child_refs.contains("content"),
            "variable refs should show content relates to main"
        );

        // Now verify the nesting inference:
        // toggle: no grid-column, main has var_child_ref "toggle" → toggle inside main
        // logo: no grid-column, brand is flex → logo inside brand
        // brand: has grid-column → direct grid child (but var_child_ref from main → also inside main)
        // content: has grid-column → direct grid child, sibling of main
    }
}
