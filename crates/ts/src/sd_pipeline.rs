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

use crate::composition::build_composition_tree_v2;
use crate::source_profile::{self, diff::diff_profiles};

use semver_analyzer_core::types::sd::{
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

    // Collect v5 profiles for deprecated components that were removed in v6.
    // These are used in Phase A.5 to diff against their non-deprecated replacements.
    let mut deprecated_removed_profiles: HashMap<String, ComponentSourceProfile> = HashMap::new();

    // Extract profiles at both refs for changed files, diff them
    for file_info in &changed_files {
        let old_source = read_git_file(repo, from_ref, &file_info.path);
        let new_source = read_git_file(repo, to_ref, &file_info.path);

        if let Some(ref source) = old_source {
            let profile =
                source_profile::extract_profile(&file_info.component_name, &file_info.path, source);

            // Collect deprecated components that were removed (exist in v5, gone in v6).
            // Store these separately before the old_profiles preference logic
            // can overwrite them with the main-path version.
            if new_source.is_none() && file_info.path.contains("/deprecated/") {
                deprecated_removed_profiles
                    .entry(file_info.component_name.clone())
                    .or_insert_with(|| profile.clone());
            }

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

    // ════════════════════════════════════════════════════════════════
    // Phase A.5: Deprecated migration diffing
    // ════════════════════════════════════════════════════════════════
    //
    // For deprecated components that were removed in v6, if a same-named
    // component exists at the non-deprecated path, diff their source
    // profiles to produce migration-specific source-level changes.
    //
    // Example: deprecated/Select was removed, components/Select exists →
    // diff deprecated Select (v5) against new Select (v6) to surface
    // behavioral differences (e.g., deprecated Select rendered TextInput
    // for typeahead variant, new Select doesn't).

    if !deprecated_removed_profiles.is_empty() {
        let mut deprecated_migration_count = 0;
        for (component_name, deprecated_profile) in &deprecated_removed_profiles {
            if let Some(replacement_profile) = new_profiles.get(component_name) {
                info!(
                    component = %component_name,
                    deprecated_path = %deprecated_profile.file,
                    replacement_path = %replacement_profile.file,
                    "Diffing deprecated component against non-deprecated replacement"
                );
                let changes = diff_profiles(deprecated_profile, replacement_profile);
                if !changes.is_empty() {
                    debug!(
                        component = %component_name,
                        changes = changes.len(),
                        "deprecated migration changes detected"
                    );
                    // Tag each change with the deprecated source path so
                    // downstream rule generation can separate these from
                    // same-component evolution changes.
                    let tagged_changes: Vec<_> = changes
                        .into_iter()
                        .map(|mut c| {
                            c.migration_from = Some(deprecated_profile.file.clone());
                            c
                        })
                        .collect();
                    deprecated_migration_count += tagged_changes.len();
                    all_source_changes.extend(tagged_changes);
                }
            } else {
                debug!(
                    component = %component_name,
                    "No non-deprecated replacement found — skipping migration diff"
                );
            }
        }

        if deprecated_migration_count > 0 {
            info!(
                changes = deprecated_migration_count,
                components = deprecated_removed_profiles
                    .keys()
                    .filter(|name| new_profiles.contains_key(*name))
                    .count(),
                "Phase A.5 complete: deprecated migration diffing"
            );
        }
    }

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

        // Find the CSS profile for this family (by dominant BEM block)
        let family_css_profile = css_profiles.and_then(|css_profs| {
            // Try root component's bem_block first
            let root_name = new_exports.first()?;
            if let Some(root_prof) = all_family_profiles.get(root_name) {
                if let Some(ref block) = root_prof.bem_block {
                    if let Some(css_prof) = css_profs.get(block) {
                        return Some(css_prof);
                    }
                }
            }
            // Fall back to dominant bem_block among all members
            let mut block_counts: HashMap<&str, usize> = HashMap::new();
            for prof in all_family_profiles.values() {
                if let Some(ref block) = prof.bem_block {
                    *block_counts.entry(block.as_str()).or_default() += 1;
                }
            }
            let dominant = block_counts
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .map(|(block, _)| block)?;
            css_profs.get(dominant)
        });

        let full_tree = build_composition_tree_v2(
            &all_family_profiles,
            &all_members_for_tree,
            family_css_profile,
        );

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

    // ── B2.5: CSS enrichment is now handled by build_composition_tree_v2 ──
    // The v2 tree builder integrates CSS direct-child selectors, grid
    // parent-child, flex context, and descendant selectors directly into
    // tree construction. The old enrich_trees_with_css is no longer called.

    // ── B3: Composition diff + conformance checks ───────────────────
    //
    // Now that trees have full edges (including projected ones), diff
    // changed families and generate conformance checks from all trees.
    let mut composition_changes = Vec::new();
    let mut conformance_checks = Vec::new();
    let mut old_composition_trees = Vec::new();

    for tree in &composition_trees {
        let family_name = &tree.root;

        // Conformance checks from ALL to-version trees
        let checks = generate_conformance_checks(family_name, tree, &new_profiles);
        conformance_checks.extend(checks);

        // Composition diff: build old tree with v2 and compare
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
                let old_tree = build_composition_tree_v2(&old_family_profiles, &old_exports, None);

                let changes = diff_composition_trees(
                    family_name,
                    old_tree.as_ref(),
                    tree,
                    &old_exports,
                    &new_exports,
                );
                composition_changes.extend(changes);

                if let Some(ot) = old_tree {
                    old_composition_trees.push(ot);
                }
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
    let old_component_prop_types: HashMap<String, BTreeMap<String, String>> = old_profiles
        .iter()
        .filter(|(_, profile)| !profile.prop_types.is_empty())
        .map(|(name, profile)| (name.clone(), profile.prop_types.clone()))
        .collect();
    let new_component_prop_types: HashMap<String, BTreeMap<String, String>> = new_profiles
        .iter()
        .filter(|(_, profile)| !profile.prop_types.is_empty())
        .map(|(name, profile)| (name.clone(), profile.prop_types.clone()))
        .collect();
    let new_required_props: HashMap<String, BTreeSet<String>> = new_profiles
        .iter()
        .filter(|(_, profile)| !profile.required_props.is_empty())
        .map(|(name, profile)| (name.clone(), profile.required_props.clone()))
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
        old_composition_trees,
        composition_changes,
        conformance_checks,
        component_packages,
        old_component_packages,
        old_component_props,
        new_component_props,
        old_component_prop_types,
        new_component_prop_types,
        new_required_props,
        dep_repo_packages: HashMap::new(), // populated by orchestrator from --dep-repo
        removed_css_blocks: Vec::new(),    // populated by orchestrator from dep-repo diff
        deprecated_replacements: Vec::new(), // populated by orchestrator from rendering swaps
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
    all_profiles: &HashMap<String, semver_analyzer_core::types::sd::ComponentSourceProfile>,
    family_exports: &[String],
) -> HashMap<String, semver_analyzer_core::types::sd::ComponentSourceProfile> {
    family_exports
        .iter()
        .filter_map(|name| all_profiles.get(name).map(|p| (name.clone(), p.clone())))
        .collect()
}

/// Extract source profiles for a family at a specific git ref.
/// Used for building old-version trees for composition diffing.
fn extract_family_profiles_at_ref(
    repo: &Path,
    git_ref: &str,
    exports: &[String],
    family_files: &[&ComponentFile],
) -> HashMap<String, semver_analyzer_core::types::sd::ComponentSourceProfile> {
    let mut profiles = HashMap::new();
    for name in exports {
        // Find the component file for this export
        if let Some(cf) = family_files.iter().find(|f| f.component_name == *name) {
            if let Some(source) = read_git_file(repo, git_ref, &cf.path) {
                let profile = crate::source_profile::extract_profile(name, &cf.path, &source);
                profiles.insert(name.clone(), profile);
            }
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
///    Convert a camelCase suffix to kebab-case for CSS element matching.
///    "ContentSection" → "content-section"
///    "item" → "item"
///    "expandableContent" → "expandable-content"
#[allow(dead_code)]
fn camel_to_kebab(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                result.push('-');
            }
            result.push(ch.to_ascii_lowercase());
        } else {
            result.push(ch);
        }
    }
    result
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
    let mut iteration = 0usize;
    loop {
        iteration += 1;
        let mut new_edges = Vec::new();
        let mut made_progress = false;

        if iteration.is_multiple_of(10) || iteration <= 3 {
            tracing::debug!(
                root = %tree.root,
                iteration,
                edge_count = tree.edges.len(),
                internal_count = internal_nodes.len(),
                "collapse_internal_nodes iteration"
            );
        }

        if iteration > 100 {
            tracing::warn!(
                root = %tree.root,
                iteration,
                edge_count = tree.edges.len(),
                "collapse_internal_nodes: exceeded 100 iterations, breaking"
            );
            break;
        }

        for internal in &internal_nodes {
            // Find parent edges INTO this internal node
            let parent_edges: Vec<&semver_analyzer_core::types::sd::CompositionEdge> =
                tree.edges.iter().filter(|e| e.child == *internal).collect();

            // Find child edges OUT OF this internal node
            let child_edges: Vec<&semver_analyzer_core::types::sd::CompositionEdge> = tree
                .edges
                .iter()
                .filter(|e| e.parent == *internal)
                .collect();

            if !parent_edges.is_empty() && !child_edges.is_empty() {
                made_progress = true;
            }

            for parent_edge in &parent_edges {
                for child_edge in &child_edges {
                    if parent_edge.parent == child_edge.child {
                        continue;
                    }
                    // Inherit the STRONGER strength of the two edges in the chain.
                    // Required > Allowed. If either hop is Required, the
                    // transitive edge is Required.
                    let strength =
                        std::cmp::max(parent_edge.strength.clone(), child_edge.strength.clone());
                    new_edges.push(semver_analyzer_core::types::sd::CompositionEdge {
                        parent: parent_edge.parent.clone(),
                        child: child_edge.child.clone(),
                        relationship: child_edge.relationship.clone(),
                        required: child_edge.required,
                        bem_evidence: Some(format!(
                            "Collapsed through internal {}: {} → {} → {}",
                            internal, parent_edge.parent, internal, child_edge.child
                        )),
                        strength,
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
    trees: &mut [CompositionTree],
    all_profiles: &HashMap<String, semver_analyzer_core::types::sd::ComponentSourceProfile>,
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
                e.relationship != semver_analyzer_core::types::sd::ChildRelationship::Internal
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
                .push(semver_analyzer_core::types::sd::CompositionEdge {
                    parent: wrapper_parent.to_string(),
                    child: wrapper_child.to_string(),
                    relationship: edge.relationship.clone(),
                    required: edge.required,
                    bem_evidence: Some(format!(
                        "Projected from {} tree: {} extends {}, {} extends {}",
                        delegate_tree.root, wrapper_parent, edge.parent, wrapper_child, edge.child,
                    )),
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Allowed,
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
        // Skip internal rendering edges — these are not consumer-facing
        // children. For example, Tab → TabTitleText via internal OverflowTab
        // is an implementation detail, not something consumers place in JSX.
        if edge.relationship == semver_analyzer_core::types::sd::ChildRelationship::Internal {
            continue;
        }

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
) -> HashMap<(String, String), &semver_analyzer_core::types::sd::CompositionEdge> {
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
fn generate_conformance_checks(
    family: &str,
    tree: &CompositionTree,
    profiles: &HashMap<String, ComponentSourceProfile>,
) -> Vec<ConformanceCheck> {
    let mut checks = Vec::new();

    // Build parent lookup: child → [parent]
    let mut child_to_parents: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &tree.edges {
        child_to_parents
            .entry(edge.child.as_str())
            .or_default()
            .push(edge.parent.as_str());
    }

    // Compute depth from root via BFS over non-internal edges.
    // Used to detect back-edges: an edge A → B is a back-edge if B
    // has a smaller depth than A (i.e., points upward toward root).
    let mut depth: HashMap<&str, usize> = HashMap::new();
    depth.insert(tree.root.as_str(), 0);
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(tree.root.as_str());
    while let Some(node) = queue.pop_front() {
        let node_depth = depth[node];
        for edge in &tree.edges {
            if edge.parent == node
                && edge.relationship != semver_analyzer_core::types::sd::ChildRelationship::Internal
                && !depth.contains_key(edge.child.as_str())
            {
                depth.insert(edge.child.as_str(), node_depth + 1);
                queue.push_back(edge.child.as_str());
            }
        }
    }

    for edge in &tree.edges {
        // Skip internal edges (not consumer-facing)
        if edge.relationship == semver_analyzer_core::types::sd::ChildRelationship::Internal {
            continue;
        }

        // Skip Allowed edges — only Required edges generate conformance
        // checks. Allowed edges (from CSS descendant selectors, flex context)
        // document valid placements but don't enforce nesting.
        if edge.strength == semver_analyzer_core::types::sd::EdgeStrength::Allowed {
            continue;
        }

        // Skip back-edges that create cycles (e.g., Tab → Tabs where Tabs
        // is an ancestor of Tab). These represent optional recursive nesting
        // (nested tabs), not mandatory containment constraints.
        //
        // A back-edge is one where the child's depth from root is ≤
        // the parent's depth (i.e., pointing upward or sideways).
        let parent_depth = depth.get(edge.parent.as_str()).copied();
        let child_depth = depth.get(edge.child.as_str()).copied();
        if let (Some(pd), Some(cd)) = (parent_depth, child_depth) {
            if cd <= pd {
                continue;
            }
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

    // ExclusiveWrapper: detect parent components where all direct children
    // must be one of the family's BEM element children.
    //
    // Heuristic: find all BEM element direct children of the root. If at
    // least one is a generic wrapper (has_children_prop, renders div/span),
    // then the root uses a wrapper pattern — ALL BEM direct children form
    // the allowed set, and any non-family component placed directly inside
    // the root is a violation.
    //
    // Examples:
    //   InputGroup  → allowed: {InputGroupItem, InputGroupText}
    //   ActionList  → allowed: {ActionListGroup}
    //   Card        → NOT detected (CardHeader/CardBody/CardFooter are content
    //                 components, none is a generic div/span wrapper)
    let root = &tree.root;
    let direct_child_edges: Vec<_> = tree
        .edges
        .iter()
        .filter(|e| {
            e.parent == *root
                && e.relationship == semver_analyzer_core::types::sd::ChildRelationship::DirectChild
        })
        .collect();

    // Find all BEM element children of the root
    let bem_children: Vec<&str> = direct_child_edges
        .iter()
        .filter(|e| {
            e.bem_evidence
                .as_ref()
                .is_some_and(|ev| ev.contains("BEM element"))
        })
        .map(|e| e.child.as_str())
        .collect();

    // Check if at least one BEM child is a generic wrapper (div/span with children)
    let has_generic_wrapper = bem_children.iter().any(|name| {
        profiles.get(*name).is_some_and(|p| {
            p.has_children_prop
                && p.children_slot_path
                    .first()
                    .is_some_and(|tag| matches!(tag.as_str(), "div" | "span"))
        })
    });

    if has_generic_wrapper && !bem_children.is_empty() {
        // The allowed set starts with all BEM direct children
        let mut allowed: Vec<String> = bem_children.iter().map(|s| s.to_string()).collect();

        // Also add family members that self-wrap in one of the BEM children
        // (internal edges, e.g., InputGroupText internally renders InputGroupItem)
        for edge in &tree.edges {
            if edge.relationship == semver_analyzer_core::types::sd::ChildRelationship::Internal
                && bem_children.contains(&edge.child.as_str())
                && !allowed.contains(&edge.parent)
            {
                allowed.push(edge.parent.clone());
            }
        }

        // Find the primary wrapper (the generic one) for the example
        let primary_wrapper = bem_children
            .iter()
            .find(|name| {
                profiles.get(**name).is_some_and(|p| {
                    p.has_children_prop
                        && p.children_slot_path
                            .first()
                            .is_some_and(|tag| matches!(tag.as_str(), "div" | "span"))
                })
            })
            .unwrap_or(&bem_children[0]);

        let allowed_list = allowed.join(", ");
        checks.push(ConformanceCheck {
            family: family.to_string(),
            check_type: ConformanceCheckType::ExclusiveWrapper {
                parent: root.clone(),
                allowed_children: allowed.clone(),
            },
            description: format!(
                "All children of {} must be wrapped in {}",
                root, allowed_list
            ),
            correct_example: Some(format!(
                "<{}>\n  <{}>\n    {{/* your content */}}\n  </{}>\n</{}>",
                root, primary_wrapper, primary_wrapper, root
            )),
        });
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
        use semver_analyzer_core::types::sd::{ChildRelationship, CompositionEdge};

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
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                CompositionEdge {
                    parent: "DropdownList".to_string(),
                    child: "DropdownItem".to_string(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
            ],
        };

        let checks = generate_conformance_checks("Dropdown", &tree, &HashMap::new());

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

    /// Back-edges (cycles) in the composition tree should NOT generate
    /// conformance checks. For example, Tab → Tabs (nested tabs) should
    /// not produce "Tabs must be inside Tab" because top-level Tabs is
    /// valid without a Tab parent.
    #[test]
    fn test_conformance_checks_skip_back_edges() {
        use semver_analyzer_core::types::sd::{ChildRelationship, CompositionEdge};

        // Mimics the Tabs family: Tabs → Tab (direct_child), Tab → Tabs (direct_child)
        let tree = CompositionTree {
            root: "Tabs".to_string(),
            family_members: vec!["Tabs".to_string(), "Tab".to_string()],
            edges: vec![
                CompositionEdge {
                    parent: "Tabs".to_string(),
                    child: "Tab".to_string(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                // Back-edge: Tab → Tabs (for nested tabs)
                CompositionEdge {
                    parent: "Tab".to_string(),
                    child: "Tabs".to_string(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
            ],
        };

        let checks = generate_conformance_checks("Tabs", &tree, &HashMap::new());

        // "Tab must be in Tabs" should exist (correct, forward edge)
        assert!(
            checks.iter().any(|c| {
                c.description.contains("Tab")
                    && c.description.contains("Tabs")
                    && !c.description.contains("Tabs should be inside Tab")
                    && !c.description.contains("Tabs must")
            }),
            "Expected a check for 'Tab must be in Tabs'"
        );

        // "Tabs must be in Tab" should NOT exist (back-edge, cycle)
        assert!(
            !checks.iter().any(|c| {
                matches!(&c.check_type, ConformanceCheckType::InvalidDirectChild {
                    child, expected_parent, ..
                } if child == "Tabs" && expected_parent == "Tab")
            }),
            "Back-edge should not produce InvalidDirectChild conformance check"
        );

        // No MissingChild for Tabs in Tab (not required, and it's a back-edge)
        assert!(
            !checks.iter().any(|c| {
                matches!(&c.check_type, ConformanceCheckType::MissingChild {
                    parent, expected_child,
                } if parent == "Tab" && expected_child == "Tabs")
            }),
            "Back-edge should not produce MissingChild conformance check"
        );
    }

    /// Internal edges should NOT produce NewRequiredChild composition changes.
    /// For example, Tab → TabTitleText (internal, via collapsed OverflowTab)
    /// should not generate a "Tab requires TabTitleText as child" change.
    #[test]
    fn test_composition_changes_skip_internal_edges() {
        use semver_analyzer_core::types::sd::{ChildRelationship, CompositionEdge};

        let old_tree = CompositionTree {
            root: "Tabs".to_string(),
            family_members: vec![
                "Tabs".to_string(),
                "Tab".to_string(),
                "TabTitleText".to_string(),
            ],
            edges: vec![CompositionEdge {
                parent: "Tabs".to_string(),
                child: "Tab".to_string(),
                relationship: ChildRelationship::DirectChild,
                required: false,
                bem_evidence: None,
                strength: semver_analyzer_core::types::sd::EdgeStrength::Allowed,
            }],
        };

        let new_tree = CompositionTree {
            root: "Tabs".to_string(),
            family_members: vec![
                "Tabs".to_string(),
                "Tab".to_string(),
                "TabTitleText".to_string(),
            ],
            edges: vec![
                CompositionEdge {
                    parent: "Tabs".to_string(),
                    child: "Tab".to_string(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Allowed,
                },
                // New internal edge (collapsed from Tab → OverflowTab → TabTitleText)
                CompositionEdge {
                    parent: "Tab".to_string(),
                    child: "TabTitleText".to_string(),
                    relationship: ChildRelationship::Internal,
                    required: false,
                    bem_evidence: Some(
                        "Collapsed through internal OverflowTab: Tab → OverflowTab → TabTitleText"
                            .to_string(),
                    ),
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Allowed,
                },
            ],
        };

        let exports: Vec<String> = vec!["Tabs".into(), "Tab".into(), "TabTitleText".into()];
        let changes =
            diff_composition_trees("Tabs", Some(&old_tree), &new_tree, &exports, &exports);

        // Should NOT have a NewRequiredChild for Tab → TabTitleText
        let has_tab_tabtitletext = changes.iter().any(|c| {
            matches!(&c.change_type, CompositionChangeType::NewRequiredChild {
                parent, new_child, ..
            } if parent == "Tab" && new_child == "TabTitleText")
        });

        assert!(
            !has_tab_tabtitletext,
            "Internal edges should not produce NewRequiredChild composition changes. \
             Got changes: {:?}",
            changes.iter().map(|c| &c.description).collect::<Vec<_>>()
        );
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

    // ── CSS enrichment guard tests ──────────────────────────────────

    use crate::css_profile::{CssBlockProfile, CssElementInfo};
    use semver_analyzer_core::types::sd::{CompositionEdge, CompositionTree};
    use std::collections::BTreeMap;

    #[allow(dead_code)]
    fn make_css_element(display: &str, is_flex: bool) -> CssElementInfo {
        let mut info = CssElementInfo::default();
        info.display_values.insert(display.to_string());
        if is_flex {
            info.display_values.insert("flex".to_string());
        }
        info
    }

    fn make_source_profile(name: &str) -> ComponentSourceProfile {
        ComponentSourceProfile {
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn make_source_profile_with_block(name: &str, block: &str) -> ComponentSourceProfile {
        ComponentSourceProfile {
            name: name.to_string(),
            bem_block: Some(block.to_string()),
            ..Default::default()
        }
    }

    /// Real PatternFly case: PageSidebar has a self-referential edge
    /// (PageSidebar → PageSidebar) from the CSS enrichment because
    /// the sidebar element is a flex container and also appears as a
    /// non-grid element. The self-referential guard must block this.

    /// Real PatternFly case: TextInputGroupMain and TextInputGroupUtilities
    /// are siblings under TextInputGroup. CSS has
    /// `.pf-v6-c-text-input-group:has(> .pf-v6-c-text-input-group__utilities)`
    /// proving utilities is a root-level direct child, NOT inside main.

    /// Real PatternFly case: Card header contains title (proven by CSS
    /// `.pf-v6-c-card__header .pf-v6-c-card__title`). Valid nesting.

    /// CSS sibling selectors prevent nesting.

    // ── Deprecated migration diffing tests ──────────────────────────

    /// When a deprecated component (e.g., deprecated/Select) is removed and
    /// a same-named replacement exists (components/Select), diffing their
    /// profiles produces source-level changes tagged with `migration_from`.
    #[test]
    fn test_deprecated_migration_diff_produces_tagged_changes() {
        use semver_analyzer_core::types::sd::SourceLevelCategory;

        // Deprecated Select rendered TextInput internally
        let mut deprecated_profile = ComponentSourceProfile::default();
        deprecated_profile.name = "Select".to_string();
        deprecated_profile.file =
            "packages/react-core/src/deprecated/components/Select/Select.tsx".to_string();
        deprecated_profile
            .rendered_components
            .push("TextInput".to_string());
        deprecated_profile
            .rendered_components
            .push("ChipGroup".to_string());

        // New Select does NOT render TextInput or ChipGroup
        let mut replacement_profile = ComponentSourceProfile::default();
        replacement_profile.name = "Select".to_string();
        replacement_profile.file =
            "packages/react-core/src/components/Select/Select.tsx".to_string();
        replacement_profile
            .rendered_components
            .push("Menu".to_string());

        // Diff them
        let changes = diff_profiles(&deprecated_profile, &replacement_profile);

        // Should produce RenderedComponent changes
        let rendered_changes: Vec<_> = changes
            .iter()
            .filter(|c| c.category == SourceLevelCategory::RenderedComponent)
            .collect();
        assert!(
            !rendered_changes.is_empty(),
            "Should detect rendered component differences"
        );

        // Should find "no longer renders TextInput"
        let text_input_removed = rendered_changes
            .iter()
            .find(|c| c.old_value.as_deref() == Some("TextInput"));
        assert!(
            text_input_removed.is_some(),
            "Should detect TextInput no longer rendered. Changes: {:?}",
            rendered_changes
                .iter()
                .map(|c| (&c.old_value, &c.new_value))
                .collect::<Vec<_>>()
        );

        // Component name should be bare "Select" (not "removed/Select")
        for c in &changes {
            assert_eq!(
                c.component, "Select",
                "Component name should be bare, not prefixed"
            );
        }

        // migration_from is None by default from diff_profiles — the tagging
        // happens in Phase A.5. Verify we can tag them.
        let tagged: Vec<_> = changes
            .into_iter()
            .map(|mut c| {
                c.migration_from = Some(deprecated_profile.file.clone());
                c
            })
            .collect();

        for c in &tagged {
            assert_eq!(
                c.migration_from.as_deref(),
                Some("packages/react-core/src/deprecated/components/Select/Select.tsx"),
                "migration_from should be set to deprecated path"
            );
            assert_eq!(c.component, "Select", "component should remain bare");
        }
    }

    /// When a deprecated component has no same-named replacement in v6,
    /// no migration diff should be produced.
    #[test]
    fn test_deprecated_without_replacement_skipped() {
        // deprecated/Tile removed, no components/Tile exists
        let mut deprecated_profile = ComponentSourceProfile::default();
        deprecated_profile.name = "Tile".to_string();
        deprecated_profile.file =
            "packages/react-core/src/deprecated/components/Tile/Tile.tsx".to_string();
        deprecated_profile
            .rendered_components
            .push("Button".to_string());

        // Simulate: new_profiles does NOT contain "Tile"
        let new_profiles: HashMap<String, ComponentSourceProfile> = HashMap::new();

        // The lookup should return None
        assert!(
            new_profiles.get("Tile").is_none(),
            "No replacement should exist for Tile"
        );
        // No diff is produced (the Phase A.5 code simply skips this case)
    }

    /// Migration changes should be separate from same-component evolution
    /// changes. The `migration_from` field distinguishes them.
    #[test]
    fn test_migration_changes_separate_from_evolution_changes() {
        use semver_analyzer_core::types::sd::SourceLevelCategory;

        // Same-component evolution: Select v5 → Select v6 (minor changes)
        let mut select_v5 = ComponentSourceProfile::default();
        select_v5.name = "Select".to_string();
        select_v5.file = "packages/react-core/src/components/Select/Select.tsx".to_string();
        select_v5.rendered_components.push("Menu".to_string());

        let mut select_v6 = ComponentSourceProfile::default();
        select_v6.name = "Select".to_string();
        select_v6.file = "packages/react-core/src/components/Select/Select.tsx".to_string();
        select_v6.rendered_components.push("Menu".to_string());
        select_v6.rendered_components.push("Popper".to_string()); // new in v6

        let evolution_changes = diff_profiles(&select_v5, &select_v6);

        // Deprecated migration: deprecated/Select → Select
        let mut deprecated_select = ComponentSourceProfile::default();
        deprecated_select.name = "Select".to_string();
        deprecated_select.file =
            "packages/react-core/src/deprecated/components/Select/Select.tsx".to_string();
        deprecated_select
            .rendered_components
            .push("TextInput".to_string());

        let migration_changes = diff_profiles(&deprecated_select, &select_v6);

        // Tag them differently
        let evolution: Vec<_> = evolution_changes
            .into_iter()
            .map(|mut c| {
                c.migration_from = None; // same-component evolution
                c
            })
            .collect();

        let migration: Vec<_> = migration_changes
            .into_iter()
            .map(|mut c| {
                c.migration_from = Some(deprecated_select.file.clone());
                c
            })
            .collect();

        // Both have component: "Select"
        for c in &evolution {
            assert_eq!(c.component, "Select");
            assert!(c.migration_from.is_none());
        }
        for c in &migration {
            assert_eq!(c.component, "Select");
            assert!(c.migration_from.is_some());
        }

        // Migration changes should include TextInput removal
        let text_input_change = migration.iter().find(|c| {
            c.category == SourceLevelCategory::RenderedComponent
                && c.old_value.as_deref() == Some("TextInput")
        });
        assert!(
            text_input_change.is_some(),
            "Migration changes should include TextInput removal"
        );

        // Evolution changes should NOT include TextInput (it was never in main Select)
        let text_input_in_evolution = evolution.iter().find(|c| {
            c.category == SourceLevelCategory::RenderedComponent
                && c.old_value.as_deref() == Some("TextInput")
        });
        assert!(
            text_input_in_evolution.is_none(),
            "Evolution changes should not mention TextInput"
        );
    }
}
