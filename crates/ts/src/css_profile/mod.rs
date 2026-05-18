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

use anyhow::{Context, Result};
use lightningcss::properties::display::{Display, DisplayKeyword};
use lightningcss::properties::Property;
use lightningcss::rules::CssRule;
use lightningcss::selector::{Combinator, Component, Selector};
use lightningcss::stylesheet::{ParserOptions, StyleSheet};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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

    /// Direct-child relationships from CSS `>` (child) combinators.
    /// `.block__parent > .block__child` → child is a direct child of parent.
    /// Stronger signal than descendant nesting.
    pub direct_child_nesting: Vec<(String, String)>,

    /// Descendant relationships from CSS space (descendant) combinators.
    /// `.block__parent .block__child` → child is somewhere inside parent.
    /// Weaker signal — proves ancestor-descendant but not direct parent-child.
    pub descendant_nesting: Vec<(String, String)>,

    /// Sibling relationships from `~` or `+` combinators.
    pub sibling_relationships: Vec<(String, String)>,

    /// Layout container → child relationships inferred from CSS display model.
    ///
    /// When BEM elements share a CSS rule (are CSS siblings) and one of them
    /// is a layout container (has `flex-wrap`, `gap`, or is `display: grid`
    /// with layout properties), the container wraps the others.
    ///
    /// Example: `.toolbar__content-section, .toolbar__group, .toolbar__item`
    /// share a rule. `content-section` has `flex-wrap: wrap` → it's the
    /// container. So `(content-section, group)` and `(content-section, item)`.
    pub layout_children: Vec<(String, String)>,
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

    /// Whether this element defines a grid layout via `grid-template-columns`
    /// or `grid-template-rows`. This makes it a grid **container** (as opposed
    /// to a grid child which has `grid-column`/`grid-row`).
    pub has_grid_template: bool,

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
                    file = %css_path,
                    "CSS profile extracted"
                );
                // Merge into existing profile if we've already seen this block
                // (multiple CSS files per component directory)
                if let Some(existing) = profiles.get_mut(&profile.block) {
                    merge_css_profile(existing, profile);
                } else {
                    profiles.insert(profile.block.clone(), profile);
                }
            }
            Err(e) => {
                warn!(file = %css_path, %e, "failed to parse CSS");
            }
        }
    }

    info!(profiles = profiles.len(), "CSS profiles extracted");
    Ok(profiles)
}

// ── CSS class inventory extraction ──────────────────────────────────────

/// Extract a complete inventory of CSS class selector names from a git ref.
///
/// Unlike `extract_css_profiles` which builds structural BEM profiles, this
/// function collects **all** CSS class names (component, utility, layout,
/// modifier) into a flat `HashSet<String>`. Used to detect "dead" classes:
/// v5 classes where a naive prefix swap produces a v6 class that doesn't
/// actually exist in the target CSS distribution.
pub fn extract_css_class_inventory(repo: &Path, git_ref: &str) -> Result<HashSet<String>> {
    let css_files = find_component_css_files(repo, git_ref)?;

    // Also discover utility and layout CSS files (not just components)
    let all_css_files = find_all_css_files(repo, git_ref)?;

    let mut classes = HashSet::new();

    let process_source = |source: &str, classes: &mut HashSet<String>| {
        let Ok(stylesheet) = StyleSheet::parse(source, ParserOptions::default()) else {
            return;
        };
        for rule in &stylesheet.rules.0 {
            collect_classes_from_rule(rule, classes);
        }
    };

    // Process component CSS files
    for (_component_dir, css_path) in &css_files {
        if let Some(source) = crate::git_utils::read_git_file(repo, git_ref, css_path) {
            process_source(&source, &mut classes);
        }
    }

    // Process non-component CSS files (utilities, layouts, etc.)
    for css_path in &all_css_files {
        if let Some(source) = crate::git_utils::read_git_file(repo, git_ref, css_path) {
            process_source(&source, &mut classes);
        }
    }

    info!(
        classes = classes.len(),
        git_ref = git_ref,
        "CSS class inventory extracted"
    );
    Ok(classes)
}

/// Extract a complete CSS class inventory from a filesystem directory.
///
/// Walks all subdirectories looking for `.css` files and collects every
/// CSS class selector name. Companion to `extract_css_class_inventory`
/// for use with built worktrees.
pub fn extract_css_class_inventory_from_dir(dir: &Path) -> Result<HashSet<String>> {
    let mut classes = HashSet::new();

    // Walk the entire directory tree for CSS files
    fn walk_dir(dir: &Path, classes: &mut HashSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip node_modules, .git, etc.
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if name.starts_with('.') || name == "node_modules" {
                    continue;
                }
                walk_dir(&path, classes);
            } else if path.extension().is_some_and(|e| e == "css") {
                let fname = path.file_name().unwrap_or_default().to_string_lossy();
                if fname.contains(".min.") || fname.contains(".map") {
                    continue;
                }
                if let Ok(source) = std::fs::read_to_string(&path) {
                    if let Ok(stylesheet) = StyleSheet::parse(&source, ParserOptions::default()) {
                        for rule in &stylesheet.rules.0 {
                            collect_classes_from_rule(rule, classes);
                        }
                    }
                }
            }
        }
    }

    walk_dir(dir, &mut classes);

    info!(
        classes = classes.len(),
        dir = %dir.display(),
        "CSS class inventory extracted from dir"
    );
    Ok(classes)
}

/// Recursively collect all CSS class selector names from a CSS rule.
fn collect_classes_from_rule(rule: &CssRule, classes: &mut HashSet<String>) {
    match rule {
        CssRule::Style(style_rule) => {
            for selector in style_rule.selectors.0.iter() {
                collect_classes_from_selector(selector, classes);
            }
        }
        CssRule::Media(m) => {
            for r in &m.rules.0 {
                collect_classes_from_rule(r, classes);
            }
        }
        CssRule::Supports(s) => {
            for r in &s.rules.0 {
                collect_classes_from_rule(r, classes);
            }
        }
        CssRule::LayerBlock(l) => {
            for r in &l.rules.0 {
                collect_classes_from_rule(r, classes);
            }
        }
        _ => {}
    }
}

/// Extract class names from a single selector, including pseudo-class functions.
fn collect_classes_from_selector(selector: &Selector, classes: &mut HashSet<String>) {
    for component in selector.iter() {
        match component {
            Component::Class(name) => {
                classes.insert(name.as_ref().to_string());
            }
            Component::Negation(selectors) => {
                for sel in selectors.iter() {
                    collect_classes_from_selector(sel, classes);
                }
            }
            Component::Is(selectors) | Component::Where(selectors) => {
                for sel in selectors.iter() {
                    collect_classes_from_selector(sel, classes);
                }
            }
            _ => {}
        }
    }
}

/// Find ALL CSS files in the repo at a given ref (not just components).
///
/// Discovers utility, layout, and other non-component CSS files that
/// `find_component_css_files` skips.
fn find_all_css_files(repo: &Path, git_ref: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args([
            "-C",
            &repo.to_string_lossy(),
            "ls-tree",
            "-r",
            "--name-only",
            git_ref,
        ])
        .output()
        .context("Failed to run 'git ls-tree' for CSS file discovery")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git ls-tree failed for CSS file discovery at ref {}: {}",
            git_ref,
            stderr
        );
    }

    let mut files = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let path = line.trim();
        if !path.ends_with(".css") {
            continue;
        }
        // Skip minified, sourcemap, example, and test files
        if path.contains(".min.css")
            || path.contains(".map")
            || path.contains("/examples/")
            || path.contains("/test")
        {
            continue;
        }
        // Skip files already covered by find_component_css_files
        if path.contains("/components/") {
            continue;
        }
        files.push(path.to_string());
    }

    Ok(files)
}

/// Extract CSS profiles from a filesystem directory of compiled CSS.
///
/// Walks `dir/components/*/` looking for `.css` files. This is the typical
/// layout of an npm package (e.g., `@patternfly/patternfly/components/`).
pub fn extract_css_profiles_from_dir(dir: &Path) -> Result<HashMap<String, CssBlockProfile>> {
    let mut profiles = HashMap::new();

    // Try common component directory locations:
    // - dir/components/ (npm package layout)
    // - dir/dist/components/ (PatternFly CSS repo after gulp compileSrcSASS)
    // - dir/src/patternfly/components/ (PatternFly CSS repo source layout — only .scss)
    // - dir/ (flat layout)
    let components_dir = if dir.join("components").exists() {
        dir.join("components")
    } else if dir.join("dist/components").exists() {
        dir.join("dist/components")
    } else if dir.join("src/patternfly/components").exists() {
        dir.join("src/patternfly/components")
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

        // Read ALL CSS files in this component directory — each may contribute
        // structural signals to the same block profile.
        for css_entry in std::fs::read_dir(entry.path())? {
            let css_entry = css_entry?;
            let css_path = css_entry.path();
            if css_path.extension().is_none_or(|e| e != "css") {
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
                        if let Some(existing) = profiles.get_mut(&profile.block) {
                            merge_css_profile(existing, profile);
                        } else {
                            profiles.insert(profile.block.clone(), profile);
                        }
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
        .output()
        .context("Failed to run 'git ls-tree' for CSS file discovery")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git ls-tree failed for CSS file discovery at ref {}: {}",
            git_ref,
            stderr
        );
    }

    // Collect ALL CSS files per component directory — each one may contribute
    // structural signals to the same block profile.
    let mut candidates: HashMap<String, Vec<String>> = HashMap::new();

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let path = line.trim();
        if !path.ends_with(".css") {
            continue;
        }
        // Skip minified, sourcemap, example, and test files
        if path.contains(".min.css")
            || path.contains(".map")
            || path.contains("/examples/")
            || path.contains("/test")
        {
            continue;
        }

        // Look for CSS files under a components/ or dist/components/ directory
        let parts: Vec<&str> = path.split('/').collect();
        let Some(comp_idx) = parts.iter().position(|&p| p == "components") else {
            continue;
        };
        if comp_idx + 2 >= parts.len() {
            continue;
        }
        let component_dir = parts[comp_idx + 1].to_string();

        candidates
            .entry(component_dir)
            .or_default()
            .push(path.to_string());
    }

    // Flatten: return all CSS files, sorted so the main file (matching dir name)
    // comes first per directory. The caller reads all of them and merges profiles.
    let mut files = Vec::new();
    for (dir, mut paths) in candidates {
        let expected_stem = pascal_to_kebab(&dir);
        let expected_name = format!("{}.css", expected_stem);

        // Sort: main file first, then by name length (shorter = more primary)
        paths.sort_by(|a, b| {
            let a_name = a.rsplit('/').next().unwrap_or(a);
            let b_name = b.rsplit('/').next().unwrap_or(b);
            let a_match = a_name == expected_name;
            let b_match = b_name == expected_name;
            b_match
                .cmp(&a_match)
                .then_with(|| a_name.len().cmp(&b_name.len()))
        });

        for path in paths {
            files.push((dir.clone(), path));
        }
    }

    Ok(files)
}

/// Convert PascalCase to kebab-case.
/// e.g., "DescriptionList" → "description-list"
fn pascal_to_kebab(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            result.push('-');
        }
        result.push(ch.to_ascii_lowercase());
    }
    result
}

/// Merge a secondary CSS profile into an existing one.
///
/// Combines elements, nesting relationships, and containment data from
/// multiple CSS files for the same component (e.g., `description-list.css`
/// and `description-list-order.css`).
fn merge_css_profile(existing: &mut CssBlockProfile, other: CssBlockProfile) {
    // Merge elements: combine layout properties
    for (name, info) in other.elements {
        let entry = existing.elements.entry(name).or_default();
        entry.has_grid_column |= info.has_grid_column;
        entry.grid_column_reverts |= info.grid_column_reverts;
        entry.has_grid_row |= info.has_grid_row;
        entry.has_grid_template |= info.has_grid_template;
        entry.display_values.extend(info.display_values);
        entry.is_mode_switcher |= info.is_mode_switcher;
        entry.flex_shrink_zero |= info.flex_shrink_zero;
        entry.flex_wrap |= info.flex_wrap;
        entry.has_sizing |= info.has_sizing;
        entry.variable_child_refs.extend(info.variable_child_refs);
    }

    // Merge relationships (deduplicate)
    for pair in other.has_containment {
        if !existing.has_containment.contains(&pair) {
            existing.has_containment.push(pair);
        }
    }
    for pair in other.direct_child_nesting {
        if !existing.direct_child_nesting.contains(&pair) {
            existing.direct_child_nesting.push(pair);
        }
    }
    for pair in other.descendant_nesting {
        if !existing.descendant_nesting.contains(&pair) {
            existing.descendant_nesting.push(pair);
        }
    }
    for pair in other.sibling_relationships {
        if !existing.sibling_relationships.contains(&pair) {
            existing.sibling_relationships.push(pair);
        }
    }
    for pair in other.layout_children {
        if !existing.layout_children.contains(&pair) {
            existing.layout_children.push(pair);
        }
    }
}

/// Extract a CSS block profile from a CSS source string.
///
/// The block class is detected automatically from the CSS — we find the
/// first standalone class selector used as a top-level rule (the BEM block).
/// No prefix knowledge is required.
fn extract_css_block_profile(source: &str, _component_dir: &str) -> Result<CssBlockProfile> {
    // Parse with lightningcss
    let stylesheet = StyleSheet::parse(
        source,
        ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        },
    )
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

    // Step 2: Walk all rules using the detected block class as prefix.
    // Also collect selector groups (multi-element rules) for layout inference.
    let mut selector_groups: Vec<BTreeSet<String>> = Vec::new();
    for rule in &stylesheet.rules.0 {
        extract_from_rule(rule, &block_class, &mut profile, &mut selector_groups);
    }

    // Step 2b: Resolve display values from CSS variable definitions.
    // When elements use `display: var(...)`, the actual values (contents,
    // flex, etc.) are defined in modifier-scoped variables. Resolve them
    // before the mode-switcher check so it sees the real display values.
    resolve_display_var_values(source, &block_class, &mut profile);

    // Step 3: Detect mode-switchers (display: contents ↔ flex/grid)
    for info in profile.elements.values_mut() {
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

    // Step 6: Infer layout container → child relationships from CSS display model.
    //
    // When BEM elements share a CSS rule (selector_groups), they're at the same
    // "CSS level" — they receive the same display/visibility treatment. Among
    // these siblings, elements with `flex-wrap` (or `display: grid` + grid
    // properties) are layout containers; the others are layout children.
    //
    // Example: `.toolbar__content-section, .toolbar__group, .toolbar__item`
    // share a rule. `content-section` has `flex-wrap: wrap` in another rule →
    // container. `group` and `item` don't → they're flex children of
    // `content-section`.
    infer_layout_children(&mut profile, &selector_groups);

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
///
/// `selector_groups` collects sets of BEM elements that appear together in
/// the same CSS rule's selector list (CSS siblings). After all rules are
/// processed, these groups are used to infer layout container → child
/// relationships.
fn extract_from_rule(
    rule: &CssRule,
    class_prefix: &str,
    profile: &mut CssBlockProfile,
    selector_groups: &mut Vec<BTreeSet<String>>,
) {
    match rule {
        CssRule::Style(style_rule) => {
            // Extract selector relationships
            for selector in style_rule.selectors.0.iter() {
                extract_selector_relationships(selector, class_prefix, profile);
            }

            // Collect BEM elements from ALL selectors in this rule.
            // When multiple elements share a rule, they're CSS siblings
            // (same display behavior, same modifiers, same layout level).
            let mut rule_elements = BTreeSet::new();
            for selector in style_rule.selectors.0.iter() {
                if let Some(element_name) = extract_element_from_selector(selector, class_prefix) {
                    if !element_name.is_empty() {
                        rule_elements.insert(element_name);
                    }
                }
            }
            if rule_elements.len() > 1 {
                // Multiple BEM elements share this rule → CSS siblings
                if !selector_groups.contains(&rule_elements) {
                    selector_groups.push(rule_elements);
                }
            }

            // Extract layout properties per element
            for selector in style_rule.selectors.0.iter() {
                if let Some(element_name) = extract_element_from_selector(selector, class_prefix) {
                    let info = profile.elements.entry(element_name).or_default();

                    for property in style_rule.declarations.declarations.iter() {
                        match property {
                            Property::GridColumn(..) => {
                                info.has_grid_column = true;
                            }
                            Property::GridRow(..) => {
                                info.has_grid_row = true;
                            }
                            Property::GridTemplateColumns(..) => {
                                info.has_grid_template = true;
                            }
                            Property::GridTemplateRows(..) => {
                                info.has_grid_template = true;
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
                            Property::FlexShrink(shrink, _) if *shrink == 0.0 => {
                                info.flex_shrink_zero = true;
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
                                } else if prop_name == "grid-template-columns"
                                    || prop_name == "grid-template-rows"
                                {
                                    info.has_grid_template = true;
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
                extract_from_rule(inner_rule, class_prefix, profile, selector_groups);
            }
        }
        _ => {}
    }
}

/// Infer layout container → child relationships from CSS display model.
///
/// Uses two signals:
/// 1. **Shared selector groups**: BEM elements that appear together in the
///    same CSS rule are "CSS siblings" — designed for the same layout level.
/// 2. **Container properties**: Among CSS siblings, elements with `flex-wrap`
///    are flex containers; elements with `display: grid` + grid properties
///    are grid containers.
///
/// When a group has exactly one container and other siblings, those siblings
/// are layout children of the container.
fn infer_layout_children(profile: &mut CssBlockProfile, selector_groups: &[BTreeSet<String>]) {
    let mut seen = std::collections::HashSet::new();

    // Determine which elements are layout containers.
    // A container has flex-wrap (flex container) or display:grid + grid
    // template/columns (grid container). The block root ("") is always
    // a container and is excluded from child inference.
    let is_container = |el: &str| -> bool {
        if el.is_empty() {
            return true; // block root is always a container
        }
        let Some(info) = profile.elements.get(el) else {
            return false;
        };
        // Flex container: has flex-wrap
        if info.flex_wrap {
            return true;
        }
        // Grid container: has display:grid AND defines a grid template
        if info.display_values.contains("grid") && info.has_grid_template {
            return true;
        }
        false
    };

    for group in selector_groups {
        // Find containers and non-containers within this group
        let containers: Vec<&String> = group.iter().filter(|el| is_container(el)).collect();
        let children: Vec<&String> = group.iter().filter(|el| !is_container(el)).collect();

        // If no containers in this group, skip — these elements are all
        // at the same level with no clear parent.
        if containers.is_empty() || children.is_empty() {
            continue;
        }

        // For each container, record its children.
        // Prefer the most specific container (longest element name) if
        // multiple containers exist in the same group.
        let best_container = containers.iter().max_by_key(|c| c.len()).unwrap();

        for child in &children {
            let pair = (best_container.to_string(), child.to_string());
            if seen.insert(pair.clone()) {
                debug!(
                    container = %pair.0,
                    child = %pair.1,
                    block = %profile.block,
                    "CSS layout container → child inferred"
                );
                profile.layout_children.push(pair);
            }
        }
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
                        // Include root-level :has() — parent_el == "" means
                        // the block root itself. This is critical for detecting
                        // that a child is a direct root child (not nested inside
                        // a sibling element).
                        if !child_el.is_empty() {
                            profile.has_containment.push((parent_el.clone(), child_el));
                        }
                    }
                }
            }
        }
    }

    // Look for descendant combinator (space) and sibling combinator (~)
    // Selectors iterate right-to-left, so we need to track pairs.
    //
    // IMPORTANT: `selector.iter()` silently drops `Component::Combinator`
    // items — the `SelectorIter` consumes them internally and never yields
    // them. We must use `iter_raw_match_order()` which yields ALL
    // components including combinators in right-to-left order.
    let mut prev_element: Option<String> = None;
    let mut prev_combinator: Option<Combinator> = None;

    for component in selector.iter_raw_match_order() {
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
                        // Allow root ("") as parent or child — e.g.,
                        // `.drawer > .drawer__main` is root→main.
                        // Only skip self-referencing root (both empty).
                        if !(element.is_empty() && child_el.is_empty()) {
                            match combinator {
                                Combinator::Child => {
                                    // Direct child: parent > child
                                    // Note: selector iterates right-to-left
                                    profile
                                        .direct_child_nesting
                                        .push((element.clone(), child_el.clone()));
                                }
                                Combinator::Descendant => {
                                    // Descendant: parent child (space)
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
                        .or_default();

                    for child_ref in &parts[1..] {
                        // Stop at property names (start with uppercase)
                        if child_ref.chars().next().is_none_or(|c| c.is_uppercase()) {
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

/// Resolve display values from CSS custom property definitions.
///
/// When an element uses `display: var(--block__element--Display)`, the
/// actual display values (`contents`, `flex`, etc.) are set via CSS
/// variable definitions in modifier contexts:
///
///   `--{block}--m-display-stack__{element}--Display: contents;`
///   `--{block}--m-display-inline__{element}--Display: flex;`
///
/// This function scans for these definitions and adds the resolved values
/// to the element's `display_values` set, enabling accurate
/// `is_mode_switcher` detection.
fn resolve_display_var_values(source: &str, block_class: &str, profile: &mut CssBlockProfile) {
    let var_prefix = format!("--{}", block_class);

    for line in source.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with(&var_prefix) {
            continue;
        }

        // Must contain --Display
        if !trimmed.contains("--Display") {
            continue;
        }

        // Extract the value after the colon
        let Some(colon_idx) = trimmed.find(':') else {
            continue;
        };
        let value = trimmed[colon_idx + 1..].trim().trim_end_matches(';').trim();

        // Only interested in concrete display values, not var() references
        if value.starts_with("var(") {
            continue;
        }

        // Extract the element name from the variable
        // Pattern: --block--m-*__{element}--Display or --block__{element}--Display
        let var_name = trimmed[..colon_idx].trim();
        if let Some(dunder_idx) = var_name.rfind("__") {
            let after_dunder = &var_name[dunder_idx + 2..];
            // Element name is before the --Display part
            if let Some(prop_idx) = after_dunder.find("--Display") {
                let element = &after_dunder[..prop_idx];
                if !element.is_empty() {
                    let info = profile.elements.entry(element.to_string()).or_default();
                    info.display_values.insert(value.to_string());
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
                        let info = profile.elements.entry(element.to_string()).or_default();
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

use crate::git_utils::read_git_file;
use crate::sd_types::{CssModifierEffect, CssModifierMap, ComponentCssModifiers};

// ── CSS Modifier Extraction ─────────────────────────────────────────────

/// Extract CSS modifier declarations for all components in a CSS directory.
///
/// For each component CSS file, finds all `.pf-m-*` modifier rules and
/// captures their declarations: custom property overrides (e.g.,
/// `--pf-v6-c-label--BackgroundColor: var(...)`) and direct CSS properties
/// (e.g., `display: flex`). Shorthand properties are expanded to longhands.
///
/// Returns: BEM block name → { modifier class → CssModifierEffect }
pub fn extract_component_css_modifiers_from_dir(
    dir: &Path,
) -> Result<ComponentCssModifiers> {
    let mut all_modifiers = ComponentCssModifiers::new();

    // Same directory resolution as extract_css_profiles_from_dir
    let components_dir = if dir.join("components").exists() {
        dir.join("components")
    } else if dir.join("dist/components").exists() {
        dir.join("dist/components")
    } else if dir.join("src/patternfly/components").exists() {
        dir.join("src/patternfly/components")
    } else {
        dir.to_path_buf()
    };

    if !components_dir.exists() {
        warn!(path = %components_dir.display(), "CSS components directory not found for modifier extraction");
        return Ok(all_modifiers);
    }

    for entry in std::fs::read_dir(&components_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        for css_entry in std::fs::read_dir(entry.path())? {
            let css_entry = css_entry?;
            let css_path = css_entry.path();
            if css_path.extension().is_none_or(|e| e != "css") {
                continue;
            }
            let fname = css_path.file_name().unwrap_or_default().to_string_lossy();
            if fname.contains(".min.") || fname.contains(".map") {
                continue;
            }

            match std::fs::read_to_string(&css_path) {
                Ok(source) => {
                    match extract_css_modifier_declarations(&source) {
                        Ok(modifiers) => {
                            for (block, modifier_map) in modifiers {
                                let existing = all_modifiers.entry(block).or_default();
                                for (class, effect) in modifier_map {
                                    let entry = existing.entry(class).or_default();
                                    // Merge: later declarations override earlier ones
                                    entry.custom_property_overrides.extend(
                                        effect.custom_property_overrides,
                                    );
                                    entry.direct_properties.extend(effect.direct_properties);
                                }
                            }
                        }
                        Err(e) => {
                            debug!(file = %css_path.display(), %e, "failed to parse CSS for modifiers");
                        }
                    }
                }
                Err(e) => {
                    debug!(file = %css_path.display(), %e, "failed to read CSS file for modifiers");
                }
            }
        }
    }

    info!(
        blocks = all_modifiers.len(),
        total_modifiers = all_modifiers.values().map(|m| m.len()).sum::<usize>(),
        "CSS modifier declarations extracted"
    );
    Ok(all_modifiers)
}

/// Extract modifier declarations from a single CSS source string.
///
/// Finds the block class, then walks all rules looking for selectors
/// containing `.pf-m-*` classes. For each modifier rule, captures
/// custom property overrides and direct CSS property declarations.
fn extract_css_modifier_declarations(
    source: &str,
) -> Result<ComponentCssModifiers> {
    let stylesheet = StyleSheet::parse(
        source,
        ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("CSS parse error: {}", e))?;

    let block_class = match detect_block_class(&stylesheet) {
        Some(b) => b,
        None => return Ok(ComponentCssModifiers::new()),
    };

    let block_name = derive_block_name(&block_class);
    let mut modifier_map = CssModifierMap::new();

    for rule in &stylesheet.rules.0 {
        extract_modifiers_from_rule(rule, &mut modifier_map);
    }

    if modifier_map.is_empty() {
        return Ok(ComponentCssModifiers::new());
    }

    let mut result = ComponentCssModifiers::new();
    result.insert(block_name, modifier_map);
    Ok(result)
}

/// Recursively walk CSS rules, extracting modifier declarations.
fn extract_modifiers_from_rule(
    rule: &CssRule,
    modifiers: &mut CssModifierMap,
) {
    match rule {
        CssRule::Style(style_rule) => {
            // Check if any selector contains a modifier class
            let mut found_modifiers = Vec::new();
            for selector in style_rule.selectors.0.iter() {
                for class_name in extract_modifier_classes(selector) {
                    found_modifiers.push(class_name);
                }
            }

            if found_modifiers.is_empty() {
                return;
            }

            // Collect all declarations from this rule
            let mut effect = CssModifierEffect::default();
            for property in style_rule
                .declarations
                .declarations
                .iter()
                .chain(style_rule.declarations.important_declarations.iter())
            {
                collect_declaration(property, &mut effect);
            }

            if effect.custom_property_overrides.is_empty()
                && effect.direct_properties.is_empty()
            {
                return;
            }

            // Attribute to each modifier found in the selectors
            for modifier_class in found_modifiers {
                let entry = modifiers.entry(modifier_class).or_default();
                entry
                    .custom_property_overrides
                    .extend(effect.custom_property_overrides.clone());
                entry
                    .direct_properties
                    .extend(effect.direct_properties.clone());
            }
        }
        CssRule::Media(media_rule) => {
            for inner in &media_rule.rules.0 {
                extract_modifiers_from_rule(inner, modifiers);
            }
        }
        CssRule::Supports(supports_rule) => {
            for inner in &supports_rule.rules.0 {
                extract_modifiers_from_rule(inner, modifiers);
            }
        }
        _ => {}
    }
}

/// Extract all modifier class names from a selector.
///
/// A modifier class is any class matching common modifier patterns:
/// - `pf-m-*` (PatternFly BEM modifier)
/// - `is-*` (generic state modifier)
/// - `has-*` (generic state modifier)
///
/// Returns all matching class names from the selector.
fn extract_modifier_classes(selector: &Selector) -> Vec<String> {
    let mut classes = Vec::new();
    for component in selector.iter() {
        if let Component::Class(class_name) = component {
            let name = class_name.as_ref();
            if name.starts_with("pf-m-") || name.starts_with("is-") || name.starts_with("has-") {
                classes.push(name.to_string());
            }
        }
    }
    classes
}

/// Collect a single CSS property declaration into a CssModifierEffect.
fn collect_declaration(property: &Property, effect: &mut CssModifierEffect) {
    use lightningcss::printer::PrinterOptions;

    let prop_name = property.property_id().name().to_string();

    // Serialize the value using lightningcss's public API
    let value_str = match property.value_to_css_string(PrinterOptions::default()) {
        Ok(v) => v.trim().to_string(),
        Err(_) => return,
    };

    if prop_name.starts_with("--") {
        // CSS custom property override (e.g., --pf-v6-c-label--BackgroundColor)
        effect
            .custom_property_overrides
            .insert(prop_name, value_str);
    } else {
        // Direct CSS property (e.g., display, overflow, border)
        effect.direct_properties.insert(prop_name, value_str);
    }
}

// ── CSS Variable Resolution ─────────────────────────────────────────────

/// A map from CSS custom property name to its resolved terminal value.
/// Terminal values are concrete (hex colors, pixels, etc.) — no `var()`.
pub type CssVariableResolutionMap = HashMap<String, String>;

/// Build a resolution map from CSS custom property names to their terminal
/// values by collecting all custom property definitions from CSS files and
/// iteratively resolving `var()` chains.
///
/// Parses ALL CSS files in the directory (including global token files like
/// `patternfly-base.css` which contain `:root` definitions with palette and
/// semantic tokens). Component CSS files contribute component-level tokens.
///
/// Returns: variable name (with `--` prefix) → resolved terminal value.
pub fn build_css_variable_resolution_map_from_dir(
    dir: &Path,
) -> Result<CssVariableResolutionMap> {
    let mut definitions: HashMap<String, String> = HashMap::new();

    // Walk ALL CSS files recursively to find custom property definitions
    collect_css_definitions_recursive(dir, &mut definitions)?;

    info!(
        raw_definitions = definitions.len(),
        "Collected CSS custom property definitions for resolution"
    );

    // Iteratively resolve var() references
    resolve_var_chains(&mut definitions);

    // Keep only entries with terminal values (no remaining var() references)
    let resolved: CssVariableResolutionMap = definitions
        .into_iter()
        .filter(|(_, v)| !v.contains("var("))
        .collect();

    info!(
        resolved = resolved.len(),
        "CSS variable resolution map built"
    );

    Ok(resolved)
}

/// Recursively walk a directory and collect all CSS custom property definitions.
fn collect_css_definitions_recursive(
    dir: &Path,
    definitions: &mut HashMap<String, String>,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            collect_css_definitions_recursive(&path, definitions)?;
        } else if path.extension().is_some_and(|e| e == "css") {
            let fname = path.file_name().unwrap_or_default().to_string_lossy();
            if fname.contains(".min.") || fname.contains(".map") {
                continue;
            }

            if let Ok(source) = std::fs::read_to_string(&path) {
                collect_definitions_from_css(&source, definitions);
            }
        }
    }

    Ok(())
}

/// Parse a CSS source string and collect all custom property definitions.
///
/// Extracts `--name: value` declarations from all rule blocks (including
/// `:root`, component rules, modifier rules, etc.).
fn collect_definitions_from_css(
    source: &str,
    definitions: &mut HashMap<String, String>,
) {
    let stylesheet = match StyleSheet::parse(source, ParserOptions::default()) {
        Ok(s) => s,
        Err(_) => return,
    };

    for rule in &stylesheet.rules.0 {
        collect_definitions_from_rule(rule, definitions);
    }
}

/// Recursively walk CSS rules collecting custom property definitions.
fn collect_definitions_from_rule(
    rule: &CssRule,
    definitions: &mut HashMap<String, String>,
) {
    match rule {
        CssRule::Style(style_rule) => {
            for property in style_rule
                .declarations
                .declarations
                .iter()
                .chain(style_rule.declarations.important_declarations.iter())
            {
                let prop_name = property.property_id().name().to_string();
                if prop_name.starts_with("--") {
                    if let Ok(value_str) = property.value_to_css_string(
                        lightningcss::printer::PrinterOptions::default(),
                    ) {
                        let value = value_str.trim().to_string();
                        // Only insert if not already defined (first definition wins —
                        // `:root` definitions come before component overrides)
                        definitions.entry(prop_name).or_insert(value);
                    }
                }
            }
        }
        CssRule::Media(media_rule) => {
            for inner in &media_rule.rules.0 {
                collect_definitions_from_rule(inner, definitions);
            }
        }
        CssRule::Supports(supports_rule) => {
            for inner in &supports_rule.rules.0 {
                collect_definitions_from_rule(inner, definitions);
            }
        }
        CssRule::LayerBlock(layer_rule) => {
            for inner in &layer_rule.rules.0 {
                collect_definitions_from_rule(inner, definitions);
            }
        }
        _ => {}
    }
}

/// Iteratively resolve `var()` references in a definitions map.
///
/// For each definition whose value is `var(--name)` or `var(--name, fallback)`,
/// replaces it with the referenced definition's value. Repeats until no more
/// resolutions are possible (handles multi-level chains).
///
/// Max 10 iterations to prevent infinite loops from circular references.
fn resolve_var_chains(definitions: &mut HashMap<String, String>) {
    for iteration in 0..10 {
        let mut resolved_count = 0;

        let snapshot: Vec<(String, String)> = definitions
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        for (name, value) in &snapshot {
            if !value.contains("var(") {
                continue;
            }

            // Try to resolve the var() reference
            if let Some(resolved) = resolve_single_var(value, definitions) {
                if resolved != *value {
                    definitions.insert(name.clone(), resolved);
                    resolved_count += 1;
                }
            }
        }

        if resolved_count == 0 {
            debug!(
                iterations = iteration + 1,
                "CSS variable resolution converged"
            );
            break;
        }
    }
}

/// Resolve a single value that may contain `var()` references.
///
/// Handles:
/// - `var(--name)` → looks up `--name` in definitions
/// - `var(--name, fallback)` → uses fallback if `--name` not found
/// - Multiple `var()` references in one value (e.g., border shorthand)
/// - Nested `var()` (var inside fallback)
fn resolve_single_var(
    value: &str,
    definitions: &HashMap<String, String>,
) -> Option<String> {
    // Simple case: value is exactly `var(--name)` or `var(--name, fallback)`
    let trimmed = value.trim();

    if !trimmed.contains("var(") {
        return Some(trimmed.to_string());
    }

    // Find all var() references and try to resolve them
    let mut result = trimmed.to_string();
    let mut changed = false;

    // Iteratively replace var() references (innermost first)
    for _ in 0..5 {
        let mut new_result = String::new();
        let mut remaining = result.as_str();
        let mut round_changed = false;

        while let Some(var_start) = remaining.find("var(") {
            new_result.push_str(&remaining[..var_start]);
            let after_var = &remaining[var_start + 4..];

            // Find matching closing paren (handle nested parens)
            if let Some(content_end) = find_matching_paren(after_var) {
                let content = &after_var[..content_end];

                // Parse var name and optional fallback
                let (var_name, fallback) = if let Some(comma_pos) = find_comma_outside_parens(content) {
                    let name = content[..comma_pos].trim();
                    let fb = content[comma_pos + 1..].trim();
                    (name, Some(fb))
                } else {
                    (content.trim(), None)
                };

                // Try to resolve
                if let Some(resolved_value) = definitions.get(var_name) {
                    if !resolved_value.contains("var(") {
                        // Fully resolved — inline the terminal value
                        new_result.push_str(resolved_value);
                        round_changed = true;
                    } else {
                        // Partially resolved — keep var() for next iteration
                        new_result.push_str(&format!("var({}", content));
                        new_result.push(')');
                    }
                } else if let Some(fb) = fallback {
                    // Name not found, use fallback
                    new_result.push_str(fb);
                    round_changed = true;
                } else {
                    // Unresolvable — keep as-is
                    new_result.push_str(&format!("var({}", content));
                    new_result.push(')');
                }

                remaining = &after_var[content_end + 1..];
            } else {
                // Malformed var() — keep as-is
                new_result.push_str("var(");
                remaining = after_var;
            }
        }
        new_result.push_str(remaining);

        if !round_changed {
            break;
        }
        changed = true;
        result = new_result;
    }

    if changed {
        Some(result)
    } else {
        None
    }
}

/// Find the position of the matching closing parenthesis.
fn find_matching_paren(s: &str) -> Option<usize> {
    let mut depth = 1;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Find the first comma that's not inside parentheses.
fn find_comma_outside_parens(s: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Resolve the `CssModifierEffect.custom_property_overrides` values using
/// a variable resolution map, populating `resolved_overrides`.
///
/// For each override `--token: var(--ref)`, looks up `--ref` in the
/// resolution map. If resolved, stores the terminal value in
/// `resolved_overrides` under the same key.
pub fn resolve_modifier_effects(
    modifiers: &mut crate::sd_types::ComponentCssModifiers,
    resolution_map: &CssVariableResolutionMap,
) {
    for (_block, modifier_map) in modifiers.iter_mut() {
        for (_class, effect) in modifier_map.iter_mut() {
            for (token_name, raw_value) in &effect.custom_property_overrides {
                // Try to resolve the raw value (which is typically var(--something))
                if let Some(resolved) = resolve_single_var(raw_value, resolution_map) {
                    if !resolved.contains("var(") {
                        effect
                            .resolved_overrides
                            .insert(token_name.clone(), resolved);
                    }
                } else if !raw_value.contains("var(") {
                    // Already a terminal value (e.g., "transparent", "0")
                    effect
                        .resolved_overrides
                        .insert(token_name.clone(), raw_value.clone());
                }
            }
        }
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
    #[ignore] // Requires external file: /tmp/package/components/Masthead/masthead.css
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

#[cfg(test)]
mod selector_relationship_tests {
    use super::*;

    /// Parse CSS and return the extracted profile for selector relationship tests.
    fn profile_from_css(css: &str) -> CssBlockProfile {
        extract_css_block_profile(css, "test").unwrap()
    }

    #[test]
    fn test_descendant_combinator_extracts_nesting() {
        let css = r#"
            .pf-v6-c-toolbar { display: flex; }
            .pf-v6-c-toolbar__group .pf-v6-c-toolbar__item { flex: 1; }
        "#;
        let profile = profile_from_css(css);
        assert!(
            profile
                .descendant_nesting
                .contains(&("group".to_string(), "item".to_string())),
            "Should extract group → item from descendant selector. Got: {:?}",
            profile.descendant_nesting
        );
    }

    #[test]
    fn test_child_combinator_extracts_direct_child_nesting() {
        let css = r#"
            .pf-v6-c-drawer { display: flex; }
            .pf-v6-c-drawer__content > .pf-v6-c-drawer__body { overflow: auto; }
        "#;
        let profile = profile_from_css(css);
        assert!(
            profile
                .direct_child_nesting
                .contains(&("content".to_string(), "body".to_string())),
            "Should extract content → body from child combinator into direct_child_nesting. Got: {:?}",
            profile.direct_child_nesting
        );
        assert!(
            !profile
                .descendant_nesting
                .contains(&("content".to_string(), "body".to_string())),
            "Child combinator should NOT go into descendant_nesting. Got: {:?}",
            profile.descendant_nesting
        );
    }

    #[test]
    fn test_sibling_combinator_extracts_relationship() {
        let css = r#"
            .pf-v6-c-card { display: flex; }
            .pf-v6-c-card__actions + .pf-v6-c-card__title { margin: 0; }
        "#;
        let profile = profile_from_css(css);
        assert!(
            profile
                .sibling_relationships
                .contains(&("actions".to_string(), "title".to_string())),
            "Should extract actions ~ title from sibling selector. Got: {:?}",
            profile.sibling_relationships
        );
    }

    #[test]
    fn test_later_sibling_combinator() {
        let css = r#"
            .pf-v6-c-data-list { display: flex; }
            .pf-v6-c-data-list__cell ~ .pf-v6-c-data-list__cell { border: 0; }
        "#;
        let profile = profile_from_css(css);
        assert!(
            profile
                .sibling_relationships
                .contains(&("cell".to_string(), "cell".to_string())),
            "Should extract cell ~ cell from later-sibling selector. Got: {:?}",
            profile.sibling_relationships
        );
    }

    #[test]
    fn test_multiple_descendant_selectors() {
        let css = r#"
            .pf-v6-c-toolbar { display: flex; }
            .pf-v6-c-toolbar__expandable-content .pf-v6-c-toolbar__group { flex: 1; }
            .pf-v6-c-toolbar__expandable-content .pf-v6-c-toolbar__item { flex: 0; }
        "#;
        let profile = profile_from_css(css);
        assert!(
            profile
                .descendant_nesting
                .contains(&("expandable-content".to_string(), "group".to_string())),
            "Should extract expandable-content → group. Got: {:?}",
            profile.descendant_nesting
        );
        assert!(
            profile
                .descendant_nesting
                .contains(&("expandable-content".to_string(), "item".to_string())),
            "Should extract expandable-content → item. Got: {:?}",
            profile.descendant_nesting
        );
    }

    #[test]
    fn test_modifier_on_parent_still_extracts_element() {
        // Selectors like .toolbar__group:where(.pf-m-toggle-group) .toolbar__item
        // should extract group → item (the :where() modifier doesn't change the element)
        let css = r#"
            .pf-v6-c-toolbar { display: flex; }
            .pf-v6-c-toolbar__group:where(.pf-m-toggle-group) .pf-v6-c-toolbar__item { display: none; }
        "#;
        let profile = profile_from_css(css);
        assert!(
            profile
                .descendant_nesting
                .contains(&("group".to_string(), "item".to_string())),
            "Should extract group → item even with :where() modifier. Got: {:?}",
            profile.descendant_nesting
        );
    }

    #[test]
    fn test_no_nesting_without_combinators() {
        // A selector with just one class shouldn't produce nesting
        let css = r#"
            .pf-v6-c-card { display: flex; }
            .pf-v6-c-card__header { padding: 0; }
            .pf-v6-c-card__title { font-weight: bold; }
        "#;
        let profile = profile_from_css(css);
        assert!(
            profile.descendant_nesting.is_empty(),
            "Should have no descendant nesting for single-class selectors. Got: {:?}",
            profile.descendant_nesting
        );
    }

    #[test]
    fn test_layout_container_flex_wrap() {
        // Toolbar-like pattern: content-section, group, and item share a rule.
        // content-section has flex-wrap: wrap → it's the container.
        let css = r#"
            .pf-v6-c-toolbar { display: grid; }
            .pf-v6-c-toolbar__content-section,
            .pf-v6-c-toolbar__group,
            .pf-v6-c-toolbar__item {
                display: flex;
            }
            .pf-v6-c-toolbar__content-section {
                flex-wrap: wrap;
                row-gap: 8px;
                column-gap: 16px;
            }
        "#;
        let profile = profile_from_css(css);
        assert!(
            profile
                .layout_children
                .contains(&("content-section".to_string(), "group".to_string())),
            "content-section should be container of group. Got: {:?}",
            profile.layout_children
        );
        assert!(
            profile
                .layout_children
                .contains(&("content-section".to_string(), "item".to_string())),
            "content-section should be container of item. Got: {:?}",
            profile.layout_children
        );
    }

    #[test]
    fn test_layout_container_no_false_positives() {
        // Elements that share a rule but NONE is a container → no layout_children
        let css = r#"
            .pf-v6-c-card { display: flex; }
            .pf-v6-c-card__header,
            .pf-v6-c-card__body,
            .pf-v6-c-card__footer {
                padding: 16px;
            }
        "#;
        let profile = profile_from_css(css);
        assert!(
            profile.layout_children.is_empty(),
            "No container in shared rule → no layout_children. Got: {:?}",
            profile.layout_children
        );
    }

    #[test]
    fn test_layout_container_description_list_grid() {
        // DescriptionList-like: root is grid with template, group is also grid
        // with template → group is a grid container. term and description are
        // grid children of group.
        let css = r#"
            .pf-v6-c-description-list {
                display: grid;
                grid-template-columns: 1fr;
            }
            .pf-v6-c-description-list__group {
                display: grid;
                grid-template-rows: auto 1fr;
                grid-column: 1;
            }
            .pf-v6-c-description-list__group,
            .pf-v6-c-description-list__term,
            .pf-v6-c-description-list__description {
                padding: 8px;
            }
        "#;
        let profile = profile_from_css(css);
        assert!(
            profile
                .layout_children
                .contains(&("group".to_string(), "term".to_string())),
            "group should be container of term. Got: {:?}",
            profile.layout_children
        );
        assert!(
            profile
                .layout_children
                .contains(&("group".to_string(), "description".to_string())),
            "group should be container of description. Got: {:?}",
            profile.layout_children
        );
    }

    #[test]
    fn test_layout_container_empty_state() {
        // EmptyState-like: footer has flex-wrap, actions is a sibling
        let css = r#"
            .pf-v6-c-empty-state { display: flex; }
            .pf-v6-c-empty-state__footer {
                display: flex;
                flex-wrap: wrap;
                gap: 16px;
            }
            .pf-v6-c-empty-state__footer,
            .pf-v6-c-empty-state__actions {
                display: flex;
            }
        "#;
        let profile = profile_from_css(css);
        assert!(
            profile
                .layout_children
                .contains(&("footer".to_string(), "actions".to_string())),
            "footer should be container of actions. Got: {:?}",
            profile.layout_children
        );
    }

    #[test]
    fn test_card_header_title_nesting() {
        // Real-world Card CSS pattern
        let css = r#"
            .pf-v6-c-card { display: flex; }
            .pf-v6-c-card__header .pf-v6-c-card__title { padding: 0; }
        "#;
        let profile = profile_from_css(css);
        assert!(
            profile
                .descendant_nesting
                .contains(&("header".to_string(), "title".to_string())),
            "Card: header → title. Got: {:?}",
            profile.descendant_nesting
        );
    }

    #[test]
    fn test_grid_template_detection() {
        let css = r#"
            .pf-v6-c-description-list {
                display: grid;
                grid-template-columns: repeat(1, 1fr);
            }
            .pf-v6-c-description-list__group {
                display: grid;
                grid-template-rows: auto 1fr;
                grid-column: 1;
            }
        "#;
        let profile = profile_from_css(css);

        // Root should be a grid container (has grid-template)
        let root = profile.elements.get("").unwrap();
        assert!(root.has_grid_template, "Root should have grid-template");
        assert!(!root.has_grid_column, "Root should NOT have grid-column");

        // Group should be both a grid container AND a grid child
        let group = profile.elements.get("group").unwrap();
        assert!(group.has_grid_template, "Group should have grid-template");
        assert!(
            group.has_grid_column,
            "Group should have grid-column (it's a child of root grid)"
        );
    }

    #[test]
    fn test_direct_child_vs_descendant_separation() {
        let css = r#"
            .pf-v6-c-drawer { display: flex; }
            .pf-v6-c-drawer__content > .pf-v6-c-drawer__body { padding: 0; }
            .pf-v6-c-drawer__panel .pf-v6-c-drawer__head { display: grid; }
        "#;
        let profile = profile_from_css(css);

        // content > body should be in direct_child_nesting only
        assert!(
            profile
                .direct_child_nesting
                .contains(&("content".to_string(), "body".to_string())),
            "content > body should be direct child. Got direct: {:?}",
            profile.direct_child_nesting
        );
        assert!(
            !profile
                .descendant_nesting
                .contains(&("content".to_string(), "body".to_string())),
            "content > body should NOT be descendant"
        );

        // panel head should be in descendant_nesting only
        assert!(
            profile
                .descendant_nesting
                .contains(&("panel".to_string(), "head".to_string())),
            "panel head should be descendant. Got descendant: {:?}",
            profile.descendant_nesting
        );
        assert!(
            !profile
                .direct_child_nesting
                .contains(&("panel".to_string(), "head".to_string())),
            "panel head should NOT be direct child"
        );
    }

    #[test]
    fn test_grid_template_from_unparsed_var() {
        // PF often uses var() for grid-template-columns which gets parsed as Unparsed
        let css = r#"
            .pf-v6-c-toolbar {
                display: grid;
                grid-template-columns: var(--pf-v6-c-toolbar--GridTemplateColumns);
            }
        "#;
        let profile = profile_from_css(css);
        let root = profile.elements.get("").unwrap();
        assert!(
            root.has_grid_template,
            "Should detect grid-template from unparsed var(). Got: {:?}",
            root
        );
    }

    // ── CSS Modifier Extraction Tests ────────────────────────────────────

    fn modifiers_from_css(css: &str) -> ComponentCssModifiers {
        extract_css_modifier_declarations(css).unwrap()
    }

    /// Real PF v5 Label .pf-m-cyan modifier (compiled CSS equivalent).
    /// From v5.4.0:src/patternfly/components/Label/label.scss, .pf-m-cyan block.
    /// The SCSS #{$label} resolves to pf-v5-c-label after compilation.
    #[test]
    fn test_modifier_extraction_label_v5_cyan() {
        let css = r#"
            .pf-v5-c-label {
                background-color: var(--pf-v5-c-label--BackgroundColor);
            }
            .pf-v5-c-label.pf-m-cyan {
                --pf-v5-c-label--BackgroundColor: var(--pf-v5-c-label--m-cyan--BackgroundColor);
                --pf-v5-c-label__icon--Color: var(--pf-v5-c-label--m-cyan__icon--Color);
                --pf-v5-c-label__content--Color: var(--pf-v5-c-label--m-cyan__content--Color);
                --pf-v5-c-label__content--before--BorderColor: var(--pf-v5-c-label--m-cyan__content--before--BorderColor);
                --pf-v5-c-label__content--link--hover--before--BorderColor: var(--pf-v5-c-label--m-cyan__content--link--hover--before--BorderColor);
                --pf-v5-c-label__content--link--focus--before--BorderColor: var(--pf-v5-c-label--m-cyan__content--link--focus--before--BorderColor);
                --pf-v5-c-label--m-outline__content--Color: var(--pf-v5-c-label--m-outline--m-cyan__content--Color);
                --pf-v5-c-label--m-outline__content--before--BorderColor: var(--pf-v5-c-label--m-outline--m-cyan__content--before--BorderColor);
                --pf-v5-c-label--m-outline__content--link--hover--before--BorderColor: var(--pf-v5-c-label--m-outline--m-cyan__content--link--hover--before--BorderColor);
                --pf-v5-c-label--m-outline__content--link--focus--before--BorderColor: var(--pf-v5-c-label--m-outline--m-cyan__content--link--focus--before--BorderColor);
                --pf-v5-c-label--m-editable__content--before--BorderColor: var(--pf-v5-c-label--m-cyan__content--before--BorderColor);
                --pf-v5-c-label--m-editable__content--hover--before--BorderColor: var(--pf-v5-c-label--m-cyan__content--before--BorderColor);
                --pf-v5-c-label--m-editable__content--focus--before--BorderColor: var(--pf-v5-c-label--m-cyan__content--before--BorderColor);
            }
        "#;

        let all = modifiers_from_css(css);
        let label_mods = all.get("label").expect("should have 'label' block");
        let cyan = label_mods.get("pf-m-cyan").expect("should have pf-m-cyan");

        // v5 cyan has 13 custom property overrides
        assert_eq!(
            cyan.custom_property_overrides.len(),
            13,
            "v5 pf-m-cyan should have 13 custom property overrides. Got: {:?}",
            cyan.custom_property_overrides.keys().collect::<Vec<_>>()
        );

        // Verify specific override keys
        assert!(cyan.custom_property_overrides.contains_key("--pf-v5-c-label--BackgroundColor"));
        assert!(cyan.custom_property_overrides.contains_key("--pf-v5-c-label__icon--Color"));
        assert!(cyan
            .custom_property_overrides
            .contains_key("--pf-v5-c-label__content--Color"));

        // No direct properties — modifier only overrides custom properties
        assert!(
            cyan.direct_properties.is_empty(),
            "pf-m-cyan should have no direct properties"
        );
    }

    /// Real PF v6 Label .pf-m-teal modifier (compiled CSS equivalent).
    /// From v6.4.0:src/patternfly/components/Label/label.scss, .pf-m-teal block.
    #[test]
    fn test_modifier_extraction_label_v6_teal() {
        let css = r#"
            .pf-v6-c-label {
                background-color: var(--pf-v6-c-label--BackgroundColor);
            }
            .pf-v6-c-label.pf-m-teal {
                --pf-v6-c-label--BackgroundColor: var(--pf-v6-c-label--m-teal--BackgroundColor);
                --pf-v6-c-label--Color: var(--pf-v6-c-label--m-teal--Color);
                --pf-v6-c-label__icon--Color: var(--pf-v6-c-label--m-teal__icon--Color);
                --pf-v6-c-label--m-clickable--hover--BackgroundColor: var(--pf-v6-c-label--m-teal--m-clickable--hover--BackgroundColor);
                --pf-v6-c-label--m-clickable--hover--Color: var(--pf-v6-c-label--m-teal--m-clickable--hover--Color);
                --pf-v6-c-label--m-clickable--hover__icon--Color: var(--pf-v6-c-label--m-teal--m-clickable--hover__icon--Color);
                --pf-v6-c-label--m-outline--BorderColor: var(--pf-v6-c-label--m-teal--m-outline--BorderColor);
                --pf-v6-c-label--m-outline--m-clickable--hover--BorderColor: var(--pf-v6-c-label--m-teal--m-outline--m-clickable--hover--BorderColor);
            }
        "#;

        let all = modifiers_from_css(css);
        let label_mods = all.get("label").expect("should have 'label' block");
        let teal = label_mods.get("pf-m-teal").expect("should have pf-m-teal");

        // v6 teal has 8 custom property overrides (simplified from v5's 13)
        assert_eq!(
            teal.custom_property_overrides.len(),
            8,
            "v6 pf-m-teal should have 8 custom property overrides. Got: {:?}",
            teal.custom_property_overrides.keys().collect::<Vec<_>>()
        );

        assert!(teal
            .custom_property_overrides
            .contains_key("--pf-v6-c-label--BackgroundColor"));
        assert!(teal
            .custom_property_overrides
            .contains_key("--pf-v6-c-label--Color"));
    }

    /// Real PF v5 Nav with tertiary modifier — includes direct CSS properties
    /// (display, position, overflow) from the shared horizontal layout block.
    /// From v5.4.0:src/patternfly/components/Nav/nav.scss.
    #[test]
    fn test_modifier_extraction_nav_v5_tertiary_with_direct_props() {
        let css = r#"
            .pf-v5-c-nav {
                display: block;
            }
            .pf-v5-c-nav.pf-m-horizontal,
            .pf-v5-c-nav.pf-m-tertiary,
            .pf-v5-c-nav.pf-m-horizontal-subnav {
                display: flex;
                overflow: hidden;
            }
            .pf-v5-c-nav.pf-m-tertiary {
                --pf-v5-c-nav__link--PaddingTop: var(--pf-v5-c-nav--m-tertiary__link--PaddingTop);
                --pf-v5-c-nav__link--PaddingRight: var(--pf-v5-c-nav--m-tertiary__link--PaddingRight);
                --pf-v5-c-nav__link--PaddingBottom: var(--pf-v5-c-nav--m-tertiary__link--PaddingBottom);
                --pf-v5-c-nav__link--PaddingLeft: var(--pf-v5-c-nav--m-tertiary__link--PaddingLeft);
                --pf-v5-c-nav__link--Color: var(--pf-v5-c-nav--m-tertiary__link--Color);
                --pf-v5-c-nav__link--BackgroundColor: transparent;
            }
        "#;

        let all = modifiers_from_css(css);
        let nav_mods = all.get("nav").expect("should have 'nav' block");
        let tertiary = nav_mods.get("pf-m-tertiary").expect("should have pf-m-tertiary");

        // Should have custom property overrides from the tertiary-specific block
        assert!(
            tertiary.custom_property_overrides.len() >= 6,
            "tertiary should have at least 6 custom property overrides. Got: {:?}",
            tertiary.custom_property_overrides
        );

        // Should have direct properties from the shared horizontal block
        assert!(
            tertiary.direct_properties.contains_key("display"),
            "tertiary should have 'display' from shared horizontal rule. Got: {:?}",
            tertiary.direct_properties
        );
        assert!(
            tertiary.direct_properties.contains_key("overflow"),
            "tertiary should have 'overflow' from shared horizontal rule"
        );

        // horizontal-subnav should also have the same direct properties
        let hs = nav_mods
            .get("pf-m-horizontal-subnav")
            .expect("should have pf-m-horizontal-subnav");
        assert!(
            hs.direct_properties.contains_key("display"),
            "horizontal-subnav should share display from combined selector"
        );
    }

    /// Real PF v5 Drawer .pf-m-light-200 and .pf-m-no-background modifiers.
    /// Each overrides only one custom property (BackgroundColor).
    /// From v5.4.0:src/patternfly/components/Drawer/drawer.scss.
    #[test]
    fn test_modifier_extraction_drawer_v5_variants() {
        let css = r#"
            .pf-v5-c-drawer__content {
                background-color: var(--pf-v5-c-drawer__content--BackgroundColor);
            }
            .pf-v5-c-drawer__content.pf-m-no-background {
                --pf-v5-c-drawer__content--BackgroundColor: transparent;
            }
            .pf-v5-c-drawer__content.pf-m-light-200 {
                --pf-v5-c-drawer__content--BackgroundColor: var(--pf-v5-c-drawer__content--m-light-200--BackgroundColor);
            }
        "#;

        let all = modifiers_from_css(css);
        // Without the parent .pf-v5-c-drawer block class in the CSS,
        // detect_block_class can't find the block (it skips classes with "__").
        // In real compiled CSS, the drawer.css starts with .pf-v5-c-drawer {},
        // so the block is always detectable. This test verifies that when the
        // block class is missing, extraction degrades gracefully.
        assert!(
            all.is_empty(),
            "Without block class, no modifiers should be extracted"
        );
    }

    /// Drawer test with proper block class included.
    #[test]
    fn test_modifier_extraction_drawer_with_block() {
        let css = r#"
            .pf-v5-c-drawer {
                display: flex;
            }
            .pf-v5-c-drawer__content {
                background-color: var(--pf-v5-c-drawer__content--BackgroundColor);
            }
            .pf-v5-c-drawer__content.pf-m-no-background {
                --pf-v5-c-drawer__content--BackgroundColor: transparent;
            }
            .pf-v5-c-drawer__content.pf-m-light-200 {
                --pf-v5-c-drawer__content--BackgroundColor: var(--pf-v5-c-drawer__content--m-light-200--BackgroundColor);
            }
        "#;

        let all = modifiers_from_css(css);
        let drawer_mods = all.get("drawer").expect("should have 'drawer' block");

        let nobg = drawer_mods
            .get("pf-m-no-background")
            .expect("should have pf-m-no-background");
        assert_eq!(
            nobg.custom_property_overrides.len(),
            1,
            "no-background should override 1 custom property"
        );
        // Value should be "transparent" (a terminal value, not var())
        assert_eq!(
            nobg.custom_property_overrides
                .get("--pf-v5-c-drawer__content--BackgroundColor")
                .map(|v| v.as_str()),
            Some("transparent"),
            "no-background should set BackgroundColor to transparent"
        );

        let light200 = drawer_mods
            .get("pf-m-light-200")
            .expect("should have pf-m-light-200");
        assert_eq!(
            light200.custom_property_overrides.len(),
            1,
            "light-200 should override 1 custom property"
        );
    }

    /// Modifier inside @media query should still be extracted.
    #[test]
    fn test_modifier_extraction_inside_media_query() {
        let css = r#"
            .pf-v6-c-nav {
                display: block;
            }
            @media (min-width: 768px) {
                .pf-v6-c-nav.pf-m-horizontal {
                    --pf-v6-c-nav--BackgroundColor: var(--pf-v6-c-nav--m-horizontal--BackgroundColor);
                    display: flex;
                }
            }
        "#;

        let all = modifiers_from_css(css);
        let nav_mods = all.get("nav").expect("should have 'nav' block");
        let horizontal = nav_mods
            .get("pf-m-horizontal")
            .expect("should have pf-m-horizontal");

        assert!(
            horizontal
                .custom_property_overrides
                .contains_key("--pf-v6-c-nav--BackgroundColor"),
            "Should extract custom property from inside @media"
        );
        assert!(
            horizontal.direct_properties.contains_key("display"),
            "Should extract display from inside @media"
        );
    }

    /// Non-modifier rules should not appear in the modifier map.
    #[test]
    fn test_modifier_extraction_non_modifier_ignored() {
        let css = r#"
            .pf-v6-c-label {
                background-color: var(--pf-v6-c-label--BackgroundColor);
                color: var(--pf-v6-c-label--Color);
            }
            .pf-v6-c-label__icon {
                color: var(--pf-v6-c-label__icon--Color);
            }
        "#;

        let all = modifiers_from_css(css);
        // Should have a label block but NO modifiers
        if let Some(label_mods) = all.get("label") {
            assert!(
                label_mods.is_empty(),
                "Should have no modifiers for base/element rules only"
            );
        }
    }

    /// Multiple modifiers on the same component should each have their
    /// own entry with correct declarations.
    #[test]
    fn test_modifier_extraction_multiple_colors() {
        let css = r#"
            .pf-v6-c-label {
                background-color: var(--pf-v6-c-label--BackgroundColor);
            }
            .pf-v6-c-label.pf-m-blue {
                --pf-v6-c-label--BackgroundColor: var(--pf-v6-c-label--m-blue--BackgroundColor);
                --pf-v6-c-label--Color: var(--pf-v6-c-label--m-blue--Color);
                --pf-v6-c-label__icon--Color: var(--pf-v6-c-label--m-blue__icon--Color);
            }
            .pf-v6-c-label.pf-m-teal {
                --pf-v6-c-label--BackgroundColor: var(--pf-v6-c-label--m-teal--BackgroundColor);
                --pf-v6-c-label--Color: var(--pf-v6-c-label--m-teal--Color);
                --pf-v6-c-label__icon--Color: var(--pf-v6-c-label--m-teal__icon--Color);
            }
            .pf-v6-c-label.pf-m-yellow {
                --pf-v6-c-label--BackgroundColor: var(--pf-v6-c-label--m-yellow--BackgroundColor);
                --pf-v6-c-label--Color: var(--pf-v6-c-label--m-yellow--Color);
                --pf-v6-c-label__icon--Color: var(--pf-v6-c-label--m-yellow__icon--Color);
            }
        "#;

        let all = modifiers_from_css(css);
        let label_mods = all.get("label").expect("should have 'label' block");

        assert_eq!(
            label_mods.len(),
            3,
            "Should have 3 color modifiers. Got: {:?}",
            label_mods.keys().collect::<Vec<_>>()
        );

        // All three should have the same structure (3 overrides each)
        for (name, effect) in label_mods {
            assert_eq!(
                effect.custom_property_overrides.len(),
                3,
                "{} should have 3 custom property overrides",
                name
            );
        }
    }

    /// Generic modifier patterns (is-*, has-*) should also be extracted.
    #[test]
    fn test_modifier_extraction_generic_patterns() {
        let css = r#"
            .my-component {
                color: var(--my-component--Color);
            }
            .my-component.is-active {
                --my-component--Color: var(--my-component--is-active--Color);
            }
            .my-component.has-icon {
                --my-component--PaddingLeft: var(--my-component--has-icon--PaddingLeft);
            }
        "#;

        let all = modifiers_from_css(css);
        // Block name derived from "my-component" → "myComponent"
        let comp_mods = all.get("myComponent").expect("should have 'myComponent' block");

        assert!(
            comp_mods.contains_key("is-active"),
            "Should extract is-active modifier"
        );
        assert!(
            comp_mods.contains_key("has-icon"),
            "Should extract has-icon modifier"
        );
    }

    // ── CSS Variable Resolution Tests ────────────────────────────────

    /// Terminal values (no var()) are preserved as-is.
    #[test]
    fn test_resolution_terminal_values() {
        let css = r#"
            :root {
                --pf-t--color--teal--10: #daf2f2;
                --pf-t--color--teal--20: #b9e5e5;
                --pf-t--color--blue--50: #0066cc;
            }
        "#;
        let mut defs = HashMap::new();
        collect_definitions_from_css(css, &mut defs);
        resolve_var_chains(&mut defs);

        // lightningcss may minify hex colors (e.g., #0066cc → #06c)
        // so we compare case-insensitively and accept both forms
        let teal10 = defs.get("--pf-t--color--teal--10").unwrap();
        assert!(teal10 == "#daf2f2" || teal10 == "#DAF2F2", "teal-10: {}", teal10);
        let teal20 = defs.get("--pf-t--color--teal--20").unwrap();
        assert!(teal20 == "#b9e5e5" || teal20 == "#B9E5E5", "teal-20: {}", teal20);
        let blue50 = defs.get("--pf-t--color--blue--50").unwrap();
        assert!(blue50 == "#0066cc" || blue50 == "#06c", "blue-50: {}", blue50);
    }

    /// One level of var() indirection: semantic → palette.
    /// Real PF v6: --pf-t--global--color--nonstatus--teal--100: var(--pf-t--color--teal--20)
    #[test]
    fn test_resolution_one_level_chain() {
        let css = r#"
            :root {
                --pf-t--color--teal--20: #b9e5e5;
                --pf-t--global--color--nonstatus--teal--100: var(--pf-t--color--teal--20);
            }
        "#;
        let mut defs = HashMap::new();
        collect_definitions_from_css(css, &mut defs);
        resolve_var_chains(&mut defs);

        assert_eq!(
            defs.get("--pf-t--global--color--nonstatus--teal--100").map(|s| s.as_str()),
            Some("#b9e5e5"),
            "Semantic token should resolve to palette hex"
        );
    }

    /// Multi-level chain matching real PF v6 architecture:
    /// palette → semantic numbered → semantic default → component modifier token.
    #[test]
    fn test_resolution_multi_level_chain() {
        let css = r#"
            :root {
                --pf-t--color--teal--20: #b9e5e5;
                --pf-t--global--color--nonstatus--teal--100: var(--pf-t--color--teal--20);
                --pf-t--global--color--nonstatus--teal--default: var(--pf-t--global--color--nonstatus--teal--100);
            }
            .pf-v6-c-label {
                --pf-v6-c-label--m-teal--BackgroundColor: var(--pf-t--global--color--nonstatus--teal--default);
            }
        "#;
        let mut defs = HashMap::new();
        collect_definitions_from_css(css, &mut defs);
        resolve_var_chains(&mut defs);

        // Full chain: label modifier token → semantic default → semantic 100 → palette → hex
        assert_eq!(
            defs.get("--pf-v6-c-label--m-teal--BackgroundColor").map(|s| s.as_str()),
            Some("#b9e5e5"),
            "Label modifier token should resolve through 3 levels to palette hex"
        );
    }

    /// Fallback value used when referenced variable doesn't exist.
    #[test]
    fn test_resolution_with_fallback() {
        let css = r#"
            :root {
                --my-color: var(--missing-var, red);
            }
        "#;
        let mut defs = HashMap::new();
        collect_definitions_from_css(css, &mut defs);
        resolve_var_chains(&mut defs);

        assert_eq!(
            defs.get("--my-color").map(|s| s.as_str()),
            Some("red"),
            "Should use fallback when referenced var doesn't exist"
        );
    }

    /// Terminal values like "transparent" and "0" pass through unchanged.
    #[test]
    fn test_resolution_transparent_terminal() {
        let css = r#"
            .pf-v5-c-drawer__content.pf-m-no-background {
                --pf-v5-c-drawer__content--BackgroundColor: transparent;
            }
        "#;
        let mut defs = HashMap::new();
        collect_definitions_from_css(css, &mut defs);
        resolve_var_chains(&mut defs);

        assert_eq!(
            defs.get("--pf-v5-c-drawer__content--BackgroundColor").map(|s| s.as_str()),
            Some("transparent"),
            "Terminal 'transparent' should pass through unchanged"
        );
    }

    /// resolve_modifier_effects enriches CssModifierEffect with resolved values.
    #[test]
    fn test_resolve_modifier_effects_enrichment() {
        use crate::sd_types::{CssModifierEffect, ComponentCssModifiers};

        // Build a resolution map (simulating the global token chain)
        let mut resolution_map = CssVariableResolutionMap::new();
        resolution_map.insert("--pf-v6-c-label--m-teal--BackgroundColor".into(), "#b9e5e5".into());
        resolution_map.insert("--pf-v6-c-label--m-teal--Color".into(), "#003333".into());
        resolution_map.insert("--pf-v6-c-label--m-yellow--BackgroundColor".into(), "#fff4cc".into());
        resolution_map.insert("--pf-v6-c-label--m-yellow--Color".into(), "#54330b".into());

        let mut modifiers = ComponentCssModifiers::new();
        let mut label_mods = HashMap::new();

        let mut teal = CssModifierEffect::default();
        teal.custom_property_overrides.insert(
            "--pf-v6-c-label--BackgroundColor".into(),
            "var(--pf-v6-c-label--m-teal--BackgroundColor)".into(),
        );
        teal.custom_property_overrides.insert(
            "--pf-v6-c-label--Color".into(),
            "var(--pf-v6-c-label--m-teal--Color)".into(),
        );
        label_mods.insert("pf-m-teal".into(), teal);

        let mut yellow = CssModifierEffect::default();
        yellow.custom_property_overrides.insert(
            "--pf-v6-c-label--BackgroundColor".into(),
            "var(--pf-v6-c-label--m-yellow--BackgroundColor)".into(),
        );
        yellow.custom_property_overrides.insert(
            "--pf-v6-c-label--Color".into(),
            "var(--pf-v6-c-label--m-yellow--Color)".into(),
        );
        label_mods.insert("pf-m-yellow".into(), yellow);

        modifiers.insert("label".into(), label_mods);

        // Resolve
        resolve_modifier_effects(&mut modifiers, &resolution_map);

        let teal = &modifiers["label"]["pf-m-teal"];
        assert_eq!(
            teal.resolved_overrides.get("--pf-v6-c-label--BackgroundColor").map(|s| s.as_str()),
            Some("#b9e5e5"),
            "teal BackgroundColor should resolve to #b9e5e5"
        );
        assert_eq!(
            teal.resolved_overrides.get("--pf-v6-c-label--Color").map(|s| s.as_str()),
            Some("#003333"),
            "teal Color should resolve to #003333"
        );

        let yellow = &modifiers["label"]["pf-m-yellow"];
        assert_eq!(
            yellow.resolved_overrides.get("--pf-v6-c-label--BackgroundColor").map(|s| s.as_str()),
            Some("#fff4cc"),
            "yellow BackgroundColor should resolve to #fff4cc"
        );
    }
}
