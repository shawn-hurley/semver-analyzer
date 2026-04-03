//! TypeScript SD (Source-Level Diff) pipeline implementation.
//!
//! Orchestrates the v2 source-level analysis in two phases:
//!
//! **Phase A — Diff (scoped to changed files):**
//! 1. Find changed `.tsx` source files via `git diff --name-only`
//! 2. Read component source at both refs via `git show`
//! 3. Extract `ComponentSourceProfile`s and diff them → `SourceLevelChange`
//!
//! **Phase B — Full to-version (all component files):**
//! 4. Enumerate ALL component `.tsx` files at the to-ref via `git ls-tree`
//! 5. Extract profiles for all components in the to-version
//! 6. Build composition trees for ALL families → `CompositionTree`
//! 7. Diff trees (for changed families only) → `CompositionChange`
//! 8. Generate conformance checks from ALL to-version trees → `ConformanceCheck`
//!
//! This separation ensures conformance rules cover the entire new API
//! (not just families with changes), while migration rules are scoped
//! to actual diffs.
//!
//! All analysis is deterministic — no LLM, no confidence scores.

use crate::composition::build_composition_tree;
use crate::source_profile::{self, diff::diff_profiles};

use crate::sd_types::{
    ComponentSourceProfile, CompositionChange, CompositionChangeType, CompositionTree,
    ConformanceCheck, ConformanceCheckType, SdPipelineResult,
};

use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use tracing::{debug, info, info_span, warn};

/// Run the full SD pipeline for a TypeScript/React project.
///
/// Phase A: diff changed files for source-level changes.
/// Phase B: extract full to-version profiles, build composition trees
/// for all families, generate conformance checks.
///
/// If `css_profiles` is provided (from a dependency CSS repo), they're
/// used to enrich composition trees with grid layout nesting.
pub fn run_sd(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    css_profiles: Option<&HashMap<String, crate::css_profile::CssBlockProfile>>,
) -> Result<SdPipelineResult> {
    let _span = info_span!("sd_pipeline", %from_ref, %to_ref).entered();

    // ════════════════════════════════════════════════════════════════
    // Phase A: Diff — scoped to changed files
    // ════════════════════════════════════════════════════════════════

    let changed_files = find_changed_component_files(repo, from_ref, to_ref)?;
    info!(count = changed_files.len(), "changed component files found");

    let mut old_profiles: HashMap<String, ComponentSourceProfile> = HashMap::new();
    let mut all_source_changes = Vec::new();

    // Extract profiles at both refs for changed files, diff them
    for file_info in &changed_files {
        let old_source = read_git_file(repo, from_ref, &file_info.path);
        let new_source = read_git_file(repo, to_ref, &file_info.path);

        if let Some(ref source) = old_source {
            let profile =
                source_profile::extract_profile(&file_info.component_name, &file_info.path, source);
            // When a component exists in both main and deprecated paths,
            // prefer the main (non-deprecated) version.
            let is_deprecated = file_info.path.contains("/deprecated/");
            if let Some(existing) = old_profiles.get(&file_info.component_name) {
                let existing_is_deprecated = existing.file.contains("/deprecated/");
                if existing_is_deprecated && !is_deprecated {
                    old_profiles.insert(file_info.component_name.clone(), profile);
                }
            } else {
                old_profiles.insert(file_info.component_name.clone(), profile);
            }
        }

        // new_source profiles are populated in Phase B (full extraction)
        // but we need them here for diffing, so extract inline
        if let (Some(old_src), Some(new_src)) = (&old_source, &new_source) {
            let old_p = source_profile::extract_profile(
                &file_info.component_name,
                &file_info.path,
                old_src,
            );
            let new_p = source_profile::extract_profile(
                &file_info.component_name,
                &file_info.path,
                new_src,
            );

            let changes = diff_profiles(&old_p, &new_p);
            if !changes.is_empty() {
                debug!(
                    component = %file_info.component_name,
                    changes = changes.len(),
                    "source-level changes detected"
                );
            }
            all_source_changes.extend(changes);
        }
    }

    info!(
        total_changes = all_source_changes.len(),
        "Phase A complete: source-level diff"
    );

    // ════════════════════════════════════════════════════════════════
    // Phase B: Full to-version extraction
    // ════════════════════════════════════════════════════════════════

    // Find ALL component .tsx files at the to-ref
    let all_to_files = find_all_component_files(repo, to_ref)?;
    info!(
        count = all_to_files.len(),
        "all component files in to-version"
    );

    // Extract profiles for all to-version components.
    // When a component exists in both main and deprecated paths (e.g., Modal),
    // the main (non-deprecated) version takes priority — it represents the
    // canonical v6 API surface that consumers should migrate to.
    let mut new_profiles: HashMap<String, ComponentSourceProfile> = HashMap::new();
    for file_info in &all_to_files {
        if let Some(source) = read_git_file(repo, to_ref, &file_info.path) {
            let profile = source_profile::extract_profile(
                &file_info.component_name,
                &file_info.path,
                &source,
            );
            let is_deprecated = file_info.path.contains("/deprecated/");
            if let Some(existing) = new_profiles.get(&file_info.component_name) {
                let existing_is_deprecated = existing.file.contains("/deprecated/");
                // Main path wins over deprecated path
                if existing_is_deprecated && !is_deprecated {
                    new_profiles.insert(file_info.component_name.clone(), profile);
                }
                // else: keep the existing (non-deprecated or first-seen)
            } else {
                new_profiles.insert(file_info.component_name.clone(), profile);
            }
        }
    }

    info!(
        new_profiles = new_profiles.len(),
        "to-version profiles extracted"
    );

    // Group ALL to-version files by family
    let all_families = group_by_family(&all_to_files);
    // Track which families had changes (for composition diffing)
    let changed_families: HashSet<String> = changed_files
        .iter()
        .filter_map(|f| f.family.clone())
        .collect();

    // ── B1: Build all to-version composition trees ──────────────────
    //
    // For each family, build the tree with ALL component files in the
    // directory (including internal/non-exported ones like ModalBox,
    // ModalContent). This lets us trace rendering chains through
    // internal components. Afterwards, collapse non-exported nodes.
    let mut composition_trees = Vec::new();
    let mut family_exports_map: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (family_name, family_files) in &all_families {
        let new_exports = read_family_exports_from_dir(repo, to_ref, family_name, family_files);

        // Collect ALL family member names (including internal components)
        let all_member_names: Vec<String> = family_files
            .iter()
            .map(|f| f.component_name.clone())
            .collect();

        // Collect profiles for ALL members (not just exports)
        let all_family_profiles = collect_family_profiles(&new_profiles, &all_member_names);

        // Build tree with all members, using exports[0] as root
        // Pass all member names so the builder sees the internal components
        let mut all_members_for_tree = new_exports.clone();
        for name in &all_member_names {
            if !all_members_for_tree.contains(name) {
                all_members_for_tree.push(name.clone());
            }
        }

        let full_tree = build_composition_tree(&all_family_profiles, &all_members_for_tree);

        if let Some(mut tree) = full_tree {
            // Collapse non-exported nodes: remove internal components
            // and transfer their edges to their parents.
            let exports_set: HashSet<&str> = new_exports.iter().map(|s| s.as_str()).collect();
            collapse_internal_nodes(&mut tree, &exports_set);
            composition_trees.push(tree);
        }

        family_exports_map.insert(family_name.clone(), new_exports);
    }

    // ── B2: Delegation projection ───────────────────────────────────
    //
    // Wrapper families (e.g., Dropdown wraps Menu) have sparse trees
    // because they lack BEM tokens. Project edges from the delegate
    // family's tree using `extends_props` (e.g., DropdownListProps
    // extends MenuListProps → DropdownList maps to MenuList).
    // This must happen BEFORE diffing so composition diffs see the
    // projected edges.

    let trees_snapshot: Vec<CompositionTree> = composition_trees.clone();
    project_delegate_trees(&mut composition_trees, &new_profiles, &trees_snapshot);

    // ── B2.5: CSS grid nesting enrichment ─────────────────────────────
    //
    // If CSS profiles are available (from a dependency CSS repo), use
    // grid layout signals to refine nesting:
    //   - Elements with grid-column → direct children of block grid
    //   - Elements WITHOUT grid-column → must be nested inside a grid item
    //   - Variable child refs (--block__main--toggle--...) → explicit containment
    if let Some(css_profs) = css_profiles {
        enrich_trees_with_css(&mut composition_trees, css_profs, &new_profiles);
    }

    // ── B3: Composition diff + conformance checks ───────────────────
    //
    // Now that trees have full edges (including projected ones), diff
    // changed families and generate conformance checks from all trees.
    let mut composition_changes = Vec::new();
    let mut conformance_checks = Vec::new();

    for tree in &composition_trees {
        let family_name = &tree.root;

        // Conformance checks from ALL to-version trees
        let checks = generate_conformance_checks(family_name, tree);
        conformance_checks.extend(checks);

        // Composition diff only for families with changes
        if changed_families.contains(family_name) {
            if let Some(family_files) = all_families.get(family_name) {
                let new_exports = family_exports_map
                    .get(family_name)
                    .cloned()
                    .unwrap_or_default();
                let old_exports =
                    read_family_exports_from_dir(repo, from_ref, family_name, family_files);
                let old_family_profiles =
                    extract_family_profiles_at_ref(repo, from_ref, &old_exports, family_files);
                let old_tree = build_composition_tree(&old_family_profiles, &old_exports);

                let changes = diff_composition_trees(
                    family_name,
                    old_tree.as_ref(),
                    tree,
                    &old_exports,
                    &new_exports,
                );
                composition_changes.extend(changes);
            }
        }
    }

    info!(
        composition_trees = composition_trees.len(),
        composition_changes = composition_changes.len(),
        conformance_checks = conformance_checks.len(),
        "Phase B complete: composition analysis"
    );

    // Build serializable prop maps for child→prop detection
    let old_component_props: HashMap<String, BTreeSet<String>> = old_profiles
        .iter()
        .map(|(name, profile)| (name.clone(), profile.all_props.clone()))
        .collect();
    let new_component_props: HashMap<String, BTreeSet<String>> = new_profiles
        .iter()
        .map(|(name, profile)| (name.clone(), profile.all_props.clone()))
        .collect();
    let new_component_prop_types: HashMap<String, BTreeMap<String, String>> = new_profiles
        .iter()
        .filter(|(_, profile)| !profile.prop_types.is_empty())
        .map(|(name, profile)| (name.clone(), profile.prop_types.clone()))
        .collect();

    // Build component→package maps for both versions.
    // Used for detecting deprecated↔main migrations.
    let old_component_packages: HashMap<String, String> = old_profiles
        .iter()
        .filter_map(|(name, profile)| {
            resolve_component_package(&profile.file).map(|pkg| (name.clone(), pkg))
        })
        .collect();

    let component_packages: HashMap<String, String> = new_profiles
        .iter()
        .filter_map(|(name, profile)| {
            resolve_component_package(&profile.file).map(|pkg| (name.clone(), pkg))
        })
        .collect();

    Ok(SdPipelineResult {
        source_level_changes: all_source_changes,
        composition_trees,
        composition_changes,
        conformance_checks,
        component_packages,
        old_component_packages,
        old_component_props,
        new_component_props,
        new_component_prop_types,
        dep_repo_packages: HashMap::new(), // populated by orchestrator from --dep-repo
        old_profiles,
        new_profiles,
    })
}

// ── Internal types ──────────────────────────────────────────────────────

/// A component source file with extracted metadata.
#[derive(Debug, Clone)]
struct ComponentFile {
    /// Relative path to the .tsx file.
    path: String,
    /// Component name derived from the filename (e.g., "Dropdown").
    component_name: String,
    /// Family directory name (e.g., "Dropdown" from ".../components/Dropdown/...").
    family: Option<String>,
}

// ── File discovery ──────────────────────────────────────────────────────

/// Find changed component .tsx files between two refs via `git diff`.
fn find_changed_component_files(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
) -> Result<Vec<ComponentFile>> {
    let output = Command::new("git")
        .args([
            "-C",
            &repo.to_string_lossy(),
            "diff",
            "--name-only",
            &format!("{}..{}", from_ref, to_ref),
            "--",
            "*.tsx",
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff --name-only failed: {}", stderr);
    }

    Ok(parse_component_file_list(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Find ALL component .tsx files at a specific ref via `git ls-tree`.
///
/// `git ls-tree` doesn't support glob pathspecs, so we enumerate all
/// files and filter to `.tsx` in Rust.
fn find_all_component_files(repo: &Path, git_ref: &str) -> Result<Vec<ComponentFile>> {
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(%stderr, "git ls-tree failed, falling back to empty");
        return Ok(Vec::new());
    }

    // Filter to .tsx files in Rust (git ls-tree doesn't support globs)
    let all_output = String::from_utf8_lossy(&output.stdout);
    let tsx_only: String = all_output
        .lines()
        .filter(|line| line.ends_with(".tsx"))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(parse_component_file_list(&tsx_only))
}

/// Parse a newline-separated file list into ComponentFile entries.
fn parse_component_file_list(output: &str) -> Vec<ComponentFile> {
    output
        .lines()
        .filter_map(|line| {
            let path = line.trim().to_string();
            if path.is_empty() || should_exclude_from_sd(&path) {
                return None;
            }
            let component_name = extract_component_name(&path)?;
            let family = extract_family_from_path(&path);
            Some(ComponentFile {
                path,
                component_name,
                family,
            })
        })
        .collect()
}

/// Whether a file should be excluded from SD analysis.
fn should_exclude_from_sd(path: &str) -> bool {
    // Test files and mocks
    path.contains(".test.") || path.contains(".spec.")
    || path.contains("__tests__") || path.contains("__mocks__")
    // Index/barrel files
    || path.ends_with("/index.tsx") || path == "index.tsx"
    // Build output
    || path.contains("/dist/") || path.starts_with("dist/")
    // Declaration files
    || path.ends_with(".d.ts") || path.ends_with(".d.tsx")
    // Demo/example files
    || path.contains("/examples/") || path.contains("/demos/")
    // Figma code connect files
    || path.contains(".figma.")
}

/// Extract the component name from a .tsx filename.
///
/// Convention: `Dropdown.tsx` → "Dropdown"
/// Only returns names that start with uppercase (React component convention).
fn extract_component_name(path: &str) -> Option<String> {
    let filename = path.rsplit('/').next()?;
    let stem = filename.strip_suffix(".tsx")?;

    // Must start with uppercase (React component convention)
    if stem.starts_with(|c: char| c.is_ascii_uppercase()) {
        Some(stem.to_string())
    } else {
        None
    }
}

/// Extract the component family directory name from a file path.
///
/// e.g., "packages/react-core/src/components/Masthead/Masthead.tsx" → "Masthead"
fn extract_family_from_path(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == "components" && i + 1 < parts.len() && i + 2 < parts.len() {
            return Some(parts[i + 1].to_string());
        }
    }
    None
}

// ── Family / profile helpers ────────────────────────────────────────────

/// Group files by their family directory.
fn group_by_family(files: &[ComponentFile]) -> BTreeMap<String, Vec<&ComponentFile>> {
    let mut groups: BTreeMap<String, Vec<&ComponentFile>> = BTreeMap::new();
    for file in files {
        if let Some(ref family) = file.family {
            groups.entry(family.clone()).or_default().push(file);
        }
    }
    groups
}

/// Collect profiles from an existing profile map for a given family's exports.
fn collect_family_profiles(
    all_profiles: &HashMap<String, crate::sd_types::ComponentSourceProfile>,
    family_exports: &[String],
) -> HashMap<String, crate::sd_types::ComponentSourceProfile> {
    family_exports
        .iter()
        .filter_map(|name| all_profiles.get(name).map(|p| (name.clone(), p.clone())))
        .collect()
}

/// Extract profiles for family members at a specific git ref by reading source.
fn extract_family_profiles_at_ref(
    repo: &Path,
    git_ref: &str,
    family_exports: &[String],
    family_files: &[&ComponentFile],
) -> HashMap<String, crate::sd_types::ComponentSourceProfile> {
    let mut profiles = HashMap::new();

    let family_dir = family_files
        .first()
        .and_then(|f| f.path.rsplit_once('/').map(|(dir, _)| dir.to_string()))
        .unwrap_or_default();

    for component_name in family_exports {
        let file_path = format!("{}/{}.tsx", family_dir, component_name);
        if let Some(source) = read_git_file(repo, git_ref, &file_path) {
            let profile = source_profile::extract_profile(component_name, &file_path, &source);
            profiles.insert(component_name.clone(), profile);
        }
    }

    profiles
}

/// Read family exports from the index file at a given ref.
///
/// Determines the family directory from the file list, reads `index.ts`
/// or `index.tsx`, and parses re-exported component names.
fn read_family_exports_from_dir(
    repo: &Path,
    git_ref: &str,
    family: &str,
    family_files: &[&ComponentFile],
) -> Vec<String> {
    let family_dir = family_files
        .first()
        .and_then(|f| f.path.rsplit_once('/').map(|(dir, _)| dir.to_string()))
        .unwrap_or_default();

    // Try index.ts first, then index.tsx
    for index_name in &["index.ts", "index.tsx"] {
        let index_path = format!("{}/{}", family_dir, index_name);
        if let Some(content) = read_git_file(repo, git_ref, &index_path) {
            let exports = parse_index_exports(&content, family);
            if !exports.is_empty() {
                return exports;
            }
        }
    }

    // Fallback: use component names from the file list
    let mut names: Vec<String> = family_files
        .iter()
        .map(|f| f.component_name.clone())
        .collect();
    if let Some(pos) = names.iter().position(|n| n == family) {
        names.swap(0, pos);
    }
    names
}

/// Parse re-exports from an index.ts file.
///
/// Handles patterns like:
/// - `export { Dropdown } from './Dropdown';`
/// - `export { default as Dropdown } from './Dropdown';`
/// - `export * from './Dropdown';` (derives name from path)
fn parse_index_exports(content: &str, family: &str) -> Vec<String> {
    let mut exports = Vec::new();
    let mut seen = HashSet::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("export") {
            continue;
        }

        // `export * from './Dropdown'` → derive component name from path
        if trimmed.starts_with("export *") || trimmed.starts_with("export type *") {
            if let Some(path) = extract_from_path(trimmed) {
                let name = path.strip_prefix("./").unwrap_or(&path).to_string();
                if name.starts_with(|c: char| c.is_ascii_uppercase()) && seen.insert(name.clone()) {
                    exports.push(name);
                }
            }
            continue;
        }

        // `export { X, Y as Z } from './...'`
        if let Some(brace_start) = trimmed.find('{') {
            if let Some(brace_end) = trimmed.find('}') {
                let names_str = &trimmed[brace_start + 1..brace_end];
                for part in names_str.split(',') {
                    let part = part.trim();
                    let name = if let Some((_before, after)) = part.split_once(" as ") {
                        after.trim().to_string()
                    } else {
                        part.to_string()
                    };
                    if name.starts_with(|c: char| c.is_ascii_uppercase())
                        && !name.ends_with("Props")
                        && seen.insert(name.clone())
                    {
                        exports.push(name);
                    }
                }
            }
        }
    }

    // Put the family-matching component first (it's the root)
    if let Some(pos) = exports.iter().position(|n| n == family) {
        exports.swap(0, pos);
    }

    exports
}

/// Extract the `from '...'` path from an export statement.
fn extract_from_path(line: &str) -> Option<String> {
    let from_idx = line.find("from ")?;
    let after_from = &line[from_idx + 5..];
    let quote = after_from.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let end = after_from[1..].find(quote)?;
    Some(after_from[1..1 + end].to_string())
}

// ── CSS grid nesting enrichment ─────────────────────────────────────────

/// Enrich composition trees with CSS grid layout nesting.
///
/// For each tree, find the matching CSS profile (by block name) and use
/// grid layout signals to move edges from flat (root → all) to nested
/// (root → grid-items, grid-items → non-grid-items).
///
/// Algorithm:
/// 1. Match CSS profile to tree via the BEM block name
/// 2. Identify grid items (elements with `grid-column`) → direct children of root
/// 3. Identify non-grid elements → must be nested inside a grid item
/// 4. Use `variable_child_refs` to determine which grid item contains which non-grid element
/// 5. For unresolved non-grid elements, assign to the nearest flex container
fn enrich_trees_with_css(
    trees: &mut [CompositionTree],
    css_profiles: &HashMap<String, crate::css_profile::CssBlockProfile>,
    react_profiles: &HashMap<String, crate::sd_types::ComponentSourceProfile>,
) {
    for tree in trees.iter_mut() {
        // Match CSS profile by finding the BEM block used by the root or
        // its children. Try the root's bem_block first, then the dominant
        // block among family members.
        let css_profile = find_matching_css_profile(tree, react_profiles, css_profiles);
        let Some(css_prof) = css_profile else {
            continue;
        };

        debug!(
            family = %tree.root,
            css_block = %css_prof.block,
            elements = css_prof.elements.len(),
            "enriching tree with CSS grid layout"
        );

        // Map family member names → their BEM element name (kebab-case)
        // e.g., "MastheadMain" → "main", "MastheadBrand" → "brand"
        let root_lower = tree.root.to_lowercase();
        let member_to_element: HashMap<&str, String> = tree
            .family_members
            .iter()
            .filter_map(|name| {
                let lower = name.to_lowercase();
                if lower == root_lower {
                    return None; // Root maps to "" (block itself)
                }
                // Strip the root prefix to get the element suffix
                let suffix = lower.strip_prefix(&root_lower)?;
                // Convert to kebab-case for CSS matching
                // "main" → "main", "brand" → "brand"
                // For multi-word: "expandablecontent" → need to match "expandable-content"
                Some((name.as_str(), suffix.to_string()))
            })
            .collect();

        // Reverse map: element → member name
        let element_to_member: HashMap<&str, &str> = member_to_element
            .iter()
            .map(|(member, element)| (element.as_str(), *member))
            .collect();

        // Helper: look up CSS element info with kebab fallback
        let lookup_css_el = |element: &str| -> Option<&crate::css_profile::CssElementInfo> {
            css_prof.elements.get(element).or_else(|| {
                css_prof
                    .elements
                    .iter()
                    .find(|(k, _)| k.replace('-', "") == element)
                    .map(|(_, v)| v)
            })
        };

        // Classify members into three categories:
        //
        // 1. stable_grid: has grid-column that NEVER reverts → direct child of root
        // 2. promoted_grid: has grid-column but reverts in some mode → inside mode-switcher
        // 3. non_grid: no grid-column at all → inside some container
        let mut stable_grid: HashSet<&str> = HashSet::new();
        let mut promoted_grid: HashSet<&str> = HashSet::new();
        let mut non_grid: HashSet<&str> = HashSet::new();

        for (member, element) in &member_to_element {
            if let Some(info) = lookup_css_el(element) {
                if info.has_grid_column {
                    if info.grid_column_reverts {
                        promoted_grid.insert(member);
                    } else {
                        stable_grid.insert(member);
                    }
                } else {
                    non_grid.insert(member);
                }
            }
        }

        if stable_grid.is_empty() && promoted_grid.is_empty() && non_grid.is_empty() {
            continue;
        }

        // Find the mode-switching container (display: contents ↔ flex)
        let mode_switcher: Option<&str> = member_to_element
            .iter()
            .find(|(_, element)| {
                lookup_css_el(element).map_or(false, |info| {
                    info.is_mode_switcher
                        || (info.display_values.contains("var") && info.has_grid_column)
                })
            })
            .map(|(member, _)| *member);

        debug!(
            family = %tree.root,
            stable_grid = ?stable_grid,
            promoted_grid = ?promoted_grid,
            non_grid = ?non_grid,
            mode_switcher = ?mode_switcher,
            "CSS grid classification"
        );

        // Move promoted_grid items under the mode-switcher (skip self)
        if let Some(switcher) = mode_switcher {
            for &member in &promoted_grid {
                if member == switcher {
                    continue; // Don't move the mode-switcher under itself
                }
                tree.edges
                    .retain(|e| !(e.parent == tree.root && e.child == member));
                if !tree
                    .edges
                    .iter()
                    .any(|e| e.parent == switcher && e.child == member)
                {
                    tree.edges.push(crate::sd_types::CompositionEdge {
                        parent: switcher.to_string(),
                        child: member.to_string(),
                        relationship: crate::sd_types::ChildRelationship::DirectChild,
                        required: false,
                        bem_evidence: Some(format!(
                            "CSS grid nesting: {} grid-column reverts in some mode → inside {} (mode-switcher)",
                            member, switcher
                        )),
                    });
                }
            }
        }

        // Move non-grid items using variable_child_refs from the mode-switcher
        if let Some(switcher) = mode_switcher {
            let switcher_element = &member_to_element[switcher];
            if let Some(info) = lookup_css_el(switcher_element) {
                for child_ref in &info.variable_child_refs {
                    let child_member = element_to_member.get(child_ref.as_str()).or_else(|| {
                        let no_hyphen = child_ref.replace('-', "");
                        element_to_member
                            .iter()
                            .find(|(k, _)| k.replace('-', "") == no_hyphen)
                            .map(|(_, v)| v)
                    });

                    if let Some(child) = child_member {
                        // Only move non-grid items via var refs
                        // (stable_grid items stay as direct children of root)
                        if !non_grid.contains(child) {
                            continue;
                        }
                        tree.edges
                            .retain(|e| !(e.parent == tree.root && e.child == *child));
                        if !tree
                            .edges
                            .iter()
                            .any(|e| e.parent == switcher && e.child == *child)
                        {
                            tree.edges.push(crate::sd_types::CompositionEdge {
                                parent: switcher.to_string(),
                                child: child.to_string(),
                                relationship: crate::sd_types::ChildRelationship::DirectChild,
                                required: false,
                                bem_evidence: Some(format!(
                                    "CSS grid nesting: {} (no grid-column) → {} (var ref --{}__{}--{})",
                                    child, switcher, css_prof.block, switcher_element, child_ref
                                )),
                            });
                        }
                    }
                }
            }
        }

        // For remaining non-grid items not yet assigned, find their container.
        // A non-grid item must be inside SOME flex/grid container that IS a
        // grid item or promoted_grid item. Among the containers, pick the one
        // that is itself a flex container (display: flex).
        let unassigned: Vec<&str> = non_grid
            .iter()
            .filter(|member| {
                !tree
                    .edges
                    .iter()
                    .any(|e| e.child == **member && e.parent != tree.root)
            })
            .copied()
            .collect();

        if !unassigned.is_empty() {
            // For each unassigned non-grid item, find which flex container
            // it belongs to. Prefer the promoted_grid container (e.g., brand
            // is a flex container inside main) over stable_grid containers
            // (e.g., content is a flex container at root level).
            //
            // Heuristic: a non-grid item likely belongs in a flex container
            // whose BEM element name is a prefix of the non-grid item's
            // element name (e.g., "logo" could go in "brand" or "content",
            // but if we can't determine, pick the most specific container).
            let all_flex_containers: Vec<(&str, &str)> = member_to_element
                .iter()
                .filter(|(member, element)| {
                    **member != tree.root
                        && mode_switcher.map_or(true, |s| s != **member)
                        && lookup_css_el(element)
                            .map_or(false, |info| info.display_values.contains("flex"))
                })
                .map(|(member, element)| (*member, element.as_str()))
                .collect();

            for &member in &unassigned {
                let member_element = &member_to_element[member];

                // Try to find a container whose variable_child_refs include this element
                let via_var_ref = all_flex_containers.iter().find(|(_, el)| {
                    lookup_css_el(el).map_or(false, |info| {
                        info.variable_child_refs.contains(member_element.as_str())
                    })
                });

                let container = if let Some((c, _)) = via_var_ref {
                    Some(*c)
                } else if all_flex_containers.len() == 1 {
                    Some(all_flex_containers[0].0)
                } else {
                    // Multiple flex containers — use sizing heuristic:
                    // A sized non-grid element (width/max-height) goes in
                    // the rigid flex container (flex-shrink: 0), not in
                    // the wrapping one (flex-wrap: wrap).
                    let child_has_sizing =
                        lookup_css_el(member_element).map_or(false, |info| info.has_sizing);

                    if child_has_sizing {
                        // Find the rigid (non-wrapping) flex container
                        all_flex_containers
                            .iter()
                            .find(|(_, el)| {
                                lookup_css_el(el)
                                    .map_or(false, |info| info.flex_shrink_zero && !info.flex_wrap)
                            })
                            .map(|(c, _)| *c)
                    } else {
                        // Non-sized element → prefer the wrapping container
                        all_flex_containers
                            .iter()
                            .find(|(_, el)| lookup_css_el(el).map_or(false, |info| info.flex_wrap))
                            .map(|(c, _)| *c)
                    }
                };

                if let Some(container) = container {
                    tree.edges
                        .retain(|e| !(e.parent == tree.root && e.child == member));
                    if !tree
                        .edges
                        .iter()
                        .any(|e| e.parent == container && e.child == member)
                    {
                        tree.edges
                            .push(crate::sd_types::CompositionEdge {
                                parent: container.to_string(),
                                child: member.to_string(),
                                relationship:
                                    crate::sd_types::ChildRelationship::DirectChild,
                                required: false,
                                bem_evidence: Some(format!(
                                "CSS grid nesting: {} (no grid-column) inside {} (flex container)",
                                member, container
                            )),
                            });
                    }
                }
            }
        }
    }
}

/// Find the CSS profile matching a composition tree's component family.
fn find_matching_css_profile<'a>(
    tree: &CompositionTree,
    react_profiles: &HashMap<String, crate::sd_types::ComponentSourceProfile>,
    css_profiles: &'a HashMap<String, crate::css_profile::CssBlockProfile>,
) -> Option<&'a crate::css_profile::CssBlockProfile> {
    // Try matching by the root component's bem_block
    if let Some(root_profile) = react_profiles.get(&tree.root) {
        if let Some(ref block) = root_profile.bem_block {
            if let Some(css_prof) = css_profiles.get(block) {
                return Some(css_prof);
            }
        }
    }

    // Try matching by the dominant bem_block among family members
    let mut block_counts: HashMap<&str, usize> = HashMap::new();
    for member in &tree.family_members {
        if let Some(profile) = react_profiles.get(member) {
            if let Some(ref block) = profile.bem_block {
                *block_counts.entry(block.as_str()).or_default() += 1;
            }
        }
    }

    let dominant = block_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(block, _)| block)?;

    css_profiles.get(dominant)
}

// ── Internal node collapsing ────────────────────────────────────────────

/// Collapse non-exported nodes from a composition tree.
///
/// Internal components (like ModalBox, ModalContent) form the rendering
/// chain between exported parent and exported children, but consumers
/// never see them. This function:
///
/// 1. Finds edges where an internal node is an intermediary
///    (e.g., Modal → ModalContent (internal) → ModalBox (internal))
/// 2. Removes the internal nodes from `family_members`
/// 3. For each internal node, transfers its child edges to its parent(s)
///    (e.g., if A → Internal → B, creates A → B)
/// 4. Removes edges that reference internal nodes
fn collapse_internal_nodes(tree: &mut CompositionTree, exports: &HashSet<&str>) {
    // Find internal (non-exported) nodes
    let internal_nodes: HashSet<String> = tree
        .family_members
        .iter()
        .filter(|name| !exports.contains(name.as_str()))
        .cloned()
        .collect();

    if internal_nodes.is_empty() {
        return;
    }

    // Build adjacency: parent → [children]
    let mut parent_to_children: HashMap<String, Vec<String>> = HashMap::new();
    for edge in &tree.edges {
        parent_to_children
            .entry(edge.parent.clone())
            .or_default()
            .push(edge.child.clone());
    }

    // Iteratively collapse internal nodes. Each pass resolves one level
    // of internal chain (e.g., first pass: ModalBox → ModalBody becomes
    // ModalContent → ModalBody; second pass: ModalContent → ModalBody
    // becomes Modal → ModalBody).
    //
    // We iterate until no more edges reference internal nodes.
    loop {
        let mut new_edges = Vec::new();
        let mut made_progress = false;

        for internal in &internal_nodes {
            // Find parents of this internal node (may include other internals)
            let parents: Vec<String> = tree
                .edges
                .iter()
                .filter(|e| e.child == *internal)
                .map(|e| e.parent.clone())
                .collect();

            // Find children of this internal node
            let children: Vec<(String, crate::sd_types::CompositionEdge)> = tree
                .edges
                .iter()
                .filter(|e| e.parent == *internal)
                .map(|e| (e.child.clone(), e.clone()))
                .collect();

            if !parents.is_empty() && !children.is_empty() {
                made_progress = true;
            }

            for parent in &parents {
                for (child, original_edge) in &children {
                    if parent == child {
                        continue;
                    }
                    new_edges.push(crate::sd_types::CompositionEdge {
                        parent: parent.clone(),
                        child: child.clone(),
                        relationship: original_edge.relationship.clone(),
                        required: original_edge.required,
                        bem_evidence: Some(format!(
                            "Collapsed through internal {}: {} → {} → {}",
                            internal, parent, internal, child
                        )),
                    });
                }
            }
        }

        // Remove edges that have an internal node as parent OR child
        tree.edges
            .retain(|e| !internal_nodes.contains(&e.parent) && !internal_nodes.contains(&e.child));

        // Add transitive edges (some may still reference internals — next iteration handles)
        tree.edges.extend(new_edges);

        if !made_progress {
            break;
        }

        // Check if any edges still reference internal nodes
        let still_has_internal = tree
            .edges
            .iter()
            .any(|e| internal_nodes.contains(&e.parent) || internal_nodes.contains(&e.child));
        if !still_has_internal {
            break;
        }
    }

    // Deduplicate edges
    let mut seen = HashSet::new();
    tree.edges
        .retain(|e| seen.insert((e.parent.clone(), e.child.clone())));

    // Remove internal nodes from family_members
    tree.family_members
        .retain(|name| !internal_nodes.contains(name));
}

// ── Delegation projection ───────────────────────────────────────────────

/// Project edges from delegate family trees onto wrapper family trees.
///
/// A wrapper family (e.g., Dropdown) wraps another family (e.g., Menu).
/// Each wrapper component extends the corresponding delegate's Props:
///   DropdownProps extends MenuProps
///   DropdownListProps extends MenuListProps
///   DropdownItemProps extends MenuItemProps
///
/// If Menu's tree has edges like Menu → MenuList → MenuItem, this function
/// projects them as Dropdown → DropdownList → DropdownItem.
fn project_delegate_trees(
    trees: &mut Vec<CompositionTree>,
    all_profiles: &HashMap<String, crate::sd_types::ComponentSourceProfile>,
    all_trees: &[CompositionTree],
) {
    // Build a lookup: component name → which tree it belongs to
    let mut component_to_tree: HashMap<&str, usize> = HashMap::new();
    for (i, tree) in all_trees.iter().enumerate() {
        for member in &tree.family_members {
            component_to_tree.insert(member.as_str(), i);
        }
    }

    for tree in trees.iter_mut() {
        // Skip trees that already have meaningful edges
        // (more than just internal rendering edges)
        let non_internal_edges = tree
            .edges
            .iter()
            .filter(|e| {
                e.relationship != crate::sd_types::ChildRelationship::Internal
            })
            .count();
        if non_internal_edges > 0 {
            continue;
        }

        // Build the wrapping map: wrapper_component → delegate_component
        // by matching `extends_props` to components in other families.
        //
        // e.g., DropdownList.extends_props = ["MenuListProps"]
        //       → strip "Props" suffix → "MenuList"
        //       → if "MenuList" exists in another family's tree → map DropdownList → MenuList
        let mut wrapper_to_delegate: HashMap<String, String> = HashMap::new();
        let mut delegate_tree_idx: Option<usize> = None;

        for member in &tree.family_members {
            let Some(profile) = all_profiles.get(member) else {
                continue;
            };

            for ext in &profile.extends_props {
                // Strip "Props" suffix to get the component name
                let delegate_name = ext.strip_suffix("Props").unwrap_or(ext).to_string();

                if let Some(&tree_idx) = component_to_tree.get(delegate_name.as_str()) {
                    // Skip self-family references
                    if tree.family_members.contains(&delegate_name) {
                        continue;
                    }
                    wrapper_to_delegate.insert(member.clone(), delegate_name);
                    delegate_tree_idx = Some(tree_idx);
                }
            }
        }

        if wrapper_to_delegate.is_empty() {
            continue;
        }

        let Some(dt_idx) = delegate_tree_idx else {
            continue;
        };
        let delegate_tree = &all_trees[dt_idx];

        // Build reverse map: delegate → wrapper
        let mut delegate_to_wrapper: HashMap<&str, &str> = HashMap::new();
        for (wrapper, delegate) in &wrapper_to_delegate {
            delegate_to_wrapper.insert(delegate.as_str(), wrapper.as_str());
        }

        debug!(
            family = %tree.root,
            delegate_family = %delegate_tree.root,
            mappings = ?wrapper_to_delegate,
            "projecting delegate tree edges"
        );

        // Project edges from the delegate tree
        for edge in &delegate_tree.edges {
            let Some(wrapper_parent) = delegate_to_wrapper.get(edge.parent.as_str()) else {
                continue;
            };
            let Some(wrapper_child) = delegate_to_wrapper.get(edge.child.as_str()) else {
                continue;
            };

            // Check we don't already have this edge
            let already_exists = tree
                .edges
                .iter()
                .any(|e| e.parent == *wrapper_parent && e.child == *wrapper_child);
            if already_exists {
                continue;
            }

            tree.edges
                .push(crate::sd_types::CompositionEdge {
                    parent: wrapper_parent.to_string(),
                    child: wrapper_child.to_string(),
                    relationship: edge.relationship.clone(),
                    required: edge.required,
                    bem_evidence: Some(format!(
                        "Projected from {} tree: {} extends {}, {} extends {}",
                        delegate_tree.root, wrapper_parent, edge.parent, wrapper_child, edge.child,
                    )),
                });
        }
    }
}

// ── Composition tree diffing ────────────────────────────────────────────

/// Diff old and new composition trees to produce `CompositionChange` entries.
fn diff_composition_trees(
    family: &str,
    old_tree: Option<&CompositionTree>,
    new_tree: &CompositionTree,
    old_exports: &[String],
    new_exports: &[String],
) -> Vec<CompositionChange> {
    let mut changes = Vec::new();
    let old_exports_set: HashSet<&str> = old_exports.iter().map(|s| s.as_str()).collect();
    let new_exports_set: HashSet<&str> = new_exports.iter().map(|s| s.as_str()).collect();

    // Detect added/removed family members
    for name in &new_exports_set {
        if !old_exports_set.contains(name) {
            changes.push(CompositionChange {
                family: family.to_string(),
                change_type: CompositionChangeType::FamilyMemberAdded {
                    member: name.to_string(),
                },
                description: format!("{} is a new component in the {} family", name, family),
                before_pattern: None,
                after_pattern: None,
            });
        }
    }
    for name in &old_exports_set {
        if !new_exports_set.contains(name) {
            changes.push(CompositionChange {
                family: family.to_string(),
                change_type: CompositionChangeType::FamilyMemberRemoved {
                    member: name.to_string(),
                },
                description: format!("{} was removed from the {} family", name, family),
                before_pattern: None,
                after_pattern: None,
            });
        }
    }

    // Build edge maps for easy comparison
    let old_edges = old_tree.map(|t| build_edge_map(t)).unwrap_or_default();
    let new_edges = build_edge_map(new_tree);

    // Find new required children (edges in new but not in old)
    for ((parent, child), edge) in &new_edges {
        if !old_edges.contains_key(&(parent.clone(), child.clone())) {
            changes.push(CompositionChange {
                family: family.to_string(),
                change_type: CompositionChangeType::NewRequiredChild {
                    parent: parent.clone(),
                    new_child: child.clone(),
                    wraps: vec![],
                },
                description: format!(
                    "{} now expects {} as a child component{}",
                    parent,
                    child,
                    if edge.required { " (required)" } else { "" }
                ),
                before_pattern: None,
                after_pattern: Some(format!("<{}>\n  <{} />\n</{}>", parent, child, parent)),
            });
        }
    }

    changes
}

/// Build a lookup map from (parent, child) to the edge for a composition tree.
fn build_edge_map(
    tree: &CompositionTree,
) -> HashMap<(String, String), &crate::sd_types::CompositionEdge> {
    tree.edges
        .iter()
        .map(|e| ((e.parent.clone(), e.child.clone()), e))
        .collect()
}

// ── Conformance check generation ────────────────────────────────────────

/// Generate conformance checks from a composition tree.
///
/// Each edge in the tree becomes a conformance check that validates
/// consumer JSX structure.
fn generate_conformance_checks(family: &str, tree: &CompositionTree) -> Vec<ConformanceCheck> {
    let mut checks = Vec::new();

    // Build parent lookup: child → [parent]
    let mut child_to_parents: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &tree.edges {
        child_to_parents
            .entry(edge.child.as_str())
            .or_default()
            .push(edge.parent.as_str());
    }

    for edge in &tree.edges {
        // Skip internal edges (not consumer-facing)
        if edge.relationship == crate::sd_types::ChildRelationship::Internal {
            continue;
        }

        // MissingChild: parent should contain this required child
        if edge.required {
            checks.push(ConformanceCheck {
                family: family.to_string(),
                check_type: ConformanceCheckType::MissingChild {
                    parent: edge.parent.clone(),
                    expected_child: edge.child.clone(),
                },
                description: format!(
                    "{} should contain a {} child component",
                    edge.parent, edge.child
                ),
                correct_example: Some(format!(
                    "<{}>\n  <{} />\n</{}>",
                    edge.parent, edge.child, edge.parent
                )),
            });
        }

        // InvalidDirectChild: child should not be a direct child of grandparent
        if let Some(grandparents) = child_to_parents.get(edge.parent.as_str()) {
            for grandparent in grandparents {
                checks.push(ConformanceCheck {
                    family: family.to_string(),
                    check_type: ConformanceCheckType::InvalidDirectChild {
                        parent: grandparent.to_string(),
                        child: edge.child.clone(),
                        expected_parent: edge.parent.clone(),
                    },
                    description: format!(
                        "{} should be inside {}, not directly inside {}",
                        edge.child, edge.parent, grandparent
                    ),
                    correct_example: Some(format!(
                        "<{}>\n  <{}>\n    <{} />\n  </{}>\n</{}>",
                        grandparent, edge.parent, edge.child, edge.parent, grandparent
                    )),
                });
            }
        }
    }

    checks
}

// ── Package resolution ──────────────────────────────────────────────────

/// Resolve npm package name from a file path.
///
/// "packages/react-core/src/components/Modal/Modal.tsx" → "@patternfly/react-core"
/// "packages/react-core/src/deprecated/components/Modal/Modal.tsx" → "@patternfly/react-core/deprecated"
fn resolve_component_package(file_path: &str) -> Option<String> {
    let parts: Vec<&str> = file_path.split('/').collect();
    let pkg_idx = parts.iter().position(|&p| p == "packages")?;
    let pkg_dir = parts.get(pkg_idx + 1)?;
    let mut pkg_name = format!("@patternfly/{}", pkg_dir);

    if parts.contains(&"deprecated") {
        pkg_name.push_str("/deprecated");
    } else if parts.contains(&"next") {
        pkg_name.push_str("/next");
    }

    Some(pkg_name)
}

// ── Git helpers ─────────────────────────────────────────────────────────

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
    fn test_extract_component_name() {
        assert_eq!(
            extract_component_name("packages/react-core/src/components/Dropdown/Dropdown.tsx"),
            Some("Dropdown".to_string())
        );
        assert_eq!(
            extract_component_name("packages/react-core/src/components/Modal/ModalHeader.tsx"),
            Some("ModalHeader".to_string())
        );
        assert_eq!(
            extract_component_name("packages/react-core/src/helpers/util.tsx"),
            None
        );
        assert_eq!(
            extract_component_name("packages/react-core/src/components/Dropdown/Dropdown.ts"),
            None
        );
    }

    #[test]
    fn test_extract_family_from_path() {
        assert_eq!(
            extract_family_from_path("packages/react-core/src/components/Dropdown/Dropdown.tsx"),
            Some("Dropdown".to_string())
        );
        assert_eq!(
            extract_family_from_path("packages/react-core/src/components/Modal/ModalHeader.tsx"),
            Some("Modal".to_string())
        );
        assert_eq!(extract_family_from_path("src/helpers/util.tsx"), None);
    }

    #[test]
    fn test_should_exclude_from_sd() {
        assert!(should_exclude_from_sd(
            "src/components/Dropdown/Dropdown.test.tsx"
        ));
        assert!(should_exclude_from_sd(
            "src/components/Dropdown/Dropdown.spec.tsx"
        ));
        assert!(should_exclude_from_sd(
            "src/components/Dropdown/__tests__/Dropdown.tsx"
        ));
        assert!(should_exclude_from_sd("src/components/Dropdown/index.tsx"));
        assert!(should_exclude_from_sd("dist/components/Dropdown.tsx"));
        assert!(should_exclude_from_sd(
            "src/components/Dropdown/examples/Basic.tsx"
        ));
        assert!(!should_exclude_from_sd(
            "src/components/Dropdown/Dropdown.tsx"
        ));
    }

    #[test]
    fn test_parse_index_exports() {
        let content = r#"
export { Dropdown } from './Dropdown';
export { DropdownItem } from './DropdownItem';
export { DropdownList } from './DropdownList';
export type { DropdownProps } from './Dropdown';
"#;
        let exports = parse_index_exports(content, "Dropdown");
        assert_eq!(exports, vec!["Dropdown", "DropdownItem", "DropdownList"]);
    }

    #[test]
    fn test_parse_index_exports_star() {
        let content = r#"
export * from './Modal';
export * from './ModalHeader';
export * from './ModalBody';
export * from './ModalFooter';
"#;
        let exports = parse_index_exports(content, "Modal");
        assert_eq!(
            exports,
            vec!["Modal", "ModalHeader", "ModalBody", "ModalFooter"]
        );
    }

    #[test]
    fn test_parse_index_exports_default_as() {
        let content = r#"
export { default as Dropdown } from './Dropdown';
export { default as DropdownItem } from './DropdownItem';
"#;
        let exports = parse_index_exports(content, "Dropdown");
        assert_eq!(exports, vec!["Dropdown", "DropdownItem"]);
    }

    #[test]
    fn test_parse_index_exports_family_first() {
        let content = r#"
export { DropdownItem } from './DropdownItem';
export { Dropdown } from './Dropdown';
export { DropdownList } from './DropdownList';
"#;
        let exports = parse_index_exports(content, "Dropdown");
        assert_eq!(exports[0], "Dropdown");
        assert!(exports.contains(&"DropdownItem".to_string()));
        assert!(exports.contains(&"DropdownList".to_string()));
    }

    #[test]
    fn test_extract_from_path() {
        assert_eq!(
            extract_from_path("export { Dropdown } from './Dropdown';"),
            Some("./Dropdown".to_string())
        );
        assert_eq!(
            extract_from_path("export * from \"./Modal\";"),
            Some("./Modal".to_string())
        );
        assert_eq!(extract_from_path("export { Dropdown };"), None);
    }

    #[test]
    fn test_generate_conformance_checks() {
        use crate::sd_types::{ChildRelationship, CompositionEdge};

        let tree = CompositionTree {
            root: "Dropdown".to_string(),
            family_members: vec![
                "Dropdown".to_string(),
                "DropdownList".to_string(),
                "DropdownItem".to_string(),
            ],
            edges: vec![
                CompositionEdge {
                    parent: "Dropdown".to_string(),
                    child: "DropdownList".to_string(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: None,
                },
                CompositionEdge {
                    parent: "DropdownList".to_string(),
                    child: "DropdownItem".to_string(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                },
            ],
        };

        let checks = generate_conformance_checks("Dropdown", &tree);

        assert!(checks.iter().any(|c| matches!(
            &c.check_type,
            ConformanceCheckType::MissingChild {
                parent,
                expected_child
            } if parent == "Dropdown" && expected_child == "DropdownList"
        )));

        assert!(checks.iter().any(|c| matches!(
            &c.check_type,
            ConformanceCheckType::InvalidDirectChild {
                parent,
                child,
                expected_parent
            } if parent == "Dropdown" && child == "DropdownItem" && expected_parent == "DropdownList"
        )));
    }

    #[test]
    fn test_parse_component_file_list() {
        let output = "packages/react-core/src/components/Modal/Modal.tsx\n\
                       packages/react-core/src/components/Modal/ModalHeader.tsx\n\
                       packages/react-core/src/helpers/util.tsx\n\
                       packages/react-core/src/components/Modal/Modal.test.tsx\n\
                       packages/react-core/src/components/Modal/index.tsx\n";

        let files = parse_component_file_list(output);
        assert_eq!(files.len(), 2); // Only Modal.tsx and ModalHeader.tsx
        assert_eq!(files[0].component_name, "Modal");
        assert_eq!(files[1].component_name, "ModalHeader");
        assert_eq!(files[0].family, Some("Modal".to_string()));
    }
}
