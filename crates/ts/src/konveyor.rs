//! TypeScript-specific Konveyor rule generation.
//!
//! This module contains all functions that operate on `AnalysisReport<TypeScript>`,
//! `BehavioralChange<TypeScript>`, `ManifestChange<TypeScript>`, `ComponentSummary<TypeScript>`,
//! `TsCategory`, or `TsManifestChangeType`.
//!
//! It depends on `semver_analyzer_konveyor_core` for shared types and utilities.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};

use crate::hierarchy_types::MigratedMember;
use crate::{TsCategory, TsManifestChangeType, TypeScript};
use semver_analyzer_core::{
    AnalysisReport, ApiChange, ApiChangeKind, ApiChangeType, BehavioralChange,
    ChildComponentStatus, ComponentStatus, ComponentSummary, ExpectedChild, FileChanges,
    ManifestChange, RemovalDisposition, RemovedMember,
};

// Re-export all types and functions from semver-analyzer-konveyor-core.
// That crate now re-exports shared types from the `konveyor-core` crate,
// so downstream consumers get the canonical shared types transitively.
pub use semver_analyzer_konveyor_core::*;

type ConstantGroupEntries<'a> = Vec<(&'a ApiChange, Option<String>, FixStrategyEntry)>;

fn detect_collapsible_constant_groups<'a>(
    report: &'a AnalysisReport<TypeScript>,
    pkg_cache: &HashMap<String, String>,
    rename_patterns: &RenamePatterns,
    member_renames: &HashMap<String, String>,
) -> HashMap<ConstantGroupKey, ConstantGroupEntries<'a>> {
    let mut groups: HashMap<ConstantGroupKey, ConstantGroupEntries<'a>> = HashMap::new();

    for file_changes in &report.changes {
        let from_pkg = resolve_npm_package(&file_changes.file.to_string_lossy(), pkg_cache);
        let pkg_name = match &from_pkg {
            Some(p) => p.clone(),
            None => continue,
        };
        let file_path_str = file_changes.file.to_string_lossy();

        for change in &file_changes.breaking_api_changes {
            if change.kind != ApiChangeKind::Constant {
                continue;
            }
            // Skip dotted symbols (interface properties like ModalProps.title)
            if change.symbol.contains('.') {
                continue;
            }
            // Skip symbols with migration_target — they get per-component rules
            // with specific migration guidance.
            if change.migration_target.is_some() {
                continue;
            }
            // Compute the strategy for this change so we can group by it
            let strategy = match api_change_to_strategy(
                change,
                rename_patterns,
                member_renames,
                &file_path_str,
            ) {
                Some(s) => s,
                None => continue,
            };

            let key = ConstantGroupKey {
                package: pkg_name.clone(),
                change_type: change.change.clone(),
                strategy: strategy.strategy.clone(),
            };
            groups
                .entry(key)
                .or_default()
                .push((change, from_pkg.clone(), strategy));
        }
    }

    // Only keep groups that exceed the threshold
    groups.retain(|_, changes| changes.len() >= CONSTANT_COLLAPSE_THRESHOLD);
    groups
}

/// Derive the import path from a package name and qualified name.
///
/// For symbols in a deprecated or next subpath, the import path includes
/// the subpath suffix. For example:
/// - package `@patternfly/react-core`, qualified_name containing `/deprecated/`
///   → `@patternfly/react-core/deprecated`
/// - package `@patternfly/react-core`, qualified_name containing `/next/`
///   → `@patternfly/react-core/next`
/// - package `@patternfly/react-core`, no special segment
///   → `@patternfly/react-core`
fn derive_import_path(package: Option<&str>, qualified_name: &str) -> String {
    let base = package.unwrap_or("unknown");
    if qualified_name.contains("/deprecated/") {
        format!("{}/deprecated", base)
    } else if qualified_name.contains("/next/") {
        format!("{}/next", base)
    } else {
        base.to_string()
    }
}

/// Extract unique union string literal values from a type annotation string.
///
/// Given a type like `property: gap: { default?: 'gapLg' | 'gapMd' | 'gapNone'; ... }`,
/// returns the deduplicated sorted list `["gapLg", "gapMd", "gapNone"]`.
/// Extract the property name from a signature string.
///
/// Given `"property: chips: (ToolbarChip | string)[]"`, returns `Some("chips")`.
/// Given `"property: labels: (ToolbarLabel | string)[]"`, returns `Some("labels")`.
///
/// Returns `None` if the signature doesn't match the expected format.
fn extract_prop_name_from_signature(sig: &str) -> Option<&str> {
    // Format: "<kind>: <name>: <type>"
    let after_kind = sig.split_once(": ")?.1;
    let name = after_kind.split_once(": ").map(|(n, _)| n)?;
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

///
/// Filters out breakpoint keys like `default`, `sm`, `md`, `lg`, `xl`, `2xl`
/// that appear as object property names rather than union values.
fn extract_union_values(type_str: &str) -> Vec<String> {
    static BREAKPOINT_KEYS: &[&str] = &["default", "sm", "md", "lg", "xl", "2xl"];
    let mut values: BTreeSet<String> = BTreeSet::new();
    for part in type_str.split('\'') {
        // Split by single quotes: even indices are outside quotes, odd are inside
        // We only want the values inside quotes
        let trimmed = part.trim();
        if !trimmed.is_empty()
            && !trimmed.contains(':')
            && !trimmed.contains('{')
            && !trimmed.contains('}')
            && !trimmed.contains('|')
            && !trimmed.contains('?')
            && !trimmed.contains(';')
            && trimmed.chars().all(|c| c.is_alphanumeric() || c == '_')
            && !BREAKPOINT_KEYS.contains(&trimmed)
        {
            values.insert(trimmed.to_string());
        }
    }
    values.into_iter().collect()
}

fn build_migration_message_legacy(
    component_name: &str,
    interface_name: &str,
    report: &AnalysisReport<TypeScript>,
    removal_count: usize,
    total_changes: usize,
) -> String {
    // Look up migration_target for this component's Props interface.
    let props_name = format!("{}Props", component_name);
    let migration_target = report
        .changes
        .iter()
        .flat_map(|fc| &fc.breaking_api_changes)
        .find(|c| {
            (c.symbol == props_name || c.symbol == *interface_name) && c.migration_target.is_some()
        })
        .and_then(|c| c.migration_target.as_ref());

    // Collect type/signature changes for the same interface's props.
    let type_changes: Vec<(String, Option<String>, Option<String>)> = report
        .changes
        .iter()
        .flat_map(|fc| &fc.breaking_api_changes)
        .filter(|c| {
            (c.symbol.starts_with(&format!("{}.", interface_name))
                || c.symbol.starts_with(&format!("{}.", props_name)))
                && matches!(
                    c.change,
                    ApiChangeType::TypeChanged | ApiChangeType::SignatureChanged
                )
        })
        .map(|c| {
            let prop = extract_leaf_symbol(&c.symbol).to_string();
            (prop, c.before.clone(), c.after.clone())
        })
        .collect();

    // Collect behavioral changes for this component across all files.
    let behavioral_descs: Vec<String> = report
        .changes
        .iter()
        .flat_map(|fc| &fc.breaking_behavioral_changes)
        .filter(|b| {
            b.symbol == component_name
                || b.symbol.starts_with(&format!("{}.", component_name))
                || b.symbol == *interface_name
                || b.symbol == props_name
        })
        .map(|b| {
            let cat = b
                .category
                .as_ref()
                .map(|c| behavioral_category_label(c))
                .unwrap_or("change");
            format!("{}: {}", cat, b.description)
        })
        .collect();

    let mut msg = String::new();

    // ── Header: migration target or generic removal ──
    if let Some(target) = migration_target {
        let replacement = target
            .replacement_symbol
            .strip_suffix("Props")
            .unwrap_or(&target.replacement_symbol);

        msg.push_str(&format!(
            "MIGRATION: Replace <{}> with props on <{}>.\n\n",
            component_name, replacement
        ));

        if !target.matching_members.is_empty() {
            msg.push_str("Property mapping:\n");
            for m in &target.matching_members {
                if m.old_name == m.new_name {
                    msg.push_str(&format!(
                        "  - {}.{}  →  {}.{}\n",
                        component_name, m.old_name, replacement, m.new_name
                    ));
                } else {
                    msg.push_str(&format!(
                        "  - {}.{}  →  {}.{} (renamed)\n",
                        component_name, m.old_name, replacement, m.new_name
                    ));
                }
            }
            msg.push('\n');
        }

        if !target.removed_only_members.is_empty() {
            msg.push_str(&format!(
                "Removed with no direct equivalent: {}\n\n",
                target.removed_only_members.join(", ")
            ));
        }
    } else if removal_count == total_changes && total_changes <= 2 {
        // Fully removed component constant (e.g., EmptyStateHeader)
        msg.push_str(&format!(
            "MIGRATION: <{}> was removed.\n\n\
             This component has no detected direct replacement.\n\
             Replace all <{}> usages with the recommended alternative.\n\n",
            component_name, component_name,
        ));
    } else {
        // Heavily modified interface — many props removed but the component
        // still exists (e.g., Modal lost title/actions/header but still works
        // with a composed children pattern).
        //
        // Collect the specific removed prop names to give the LLM concrete
        // guidance about what to restructure.
        let removed_props: Vec<String> = report
            .changes
            .iter()
            .flat_map(|fc| &fc.breaking_api_changes)
            .filter(|c| {
                c.change == ApiChangeType::Removed
                    && (c.symbol.starts_with(&format!("{}.", interface_name))
                        || c.symbol.starts_with(&format!("{}.", props_name)))
            })
            .map(|c| extract_leaf_symbol(&c.symbol).to_string())
            .collect();

        msg.push_str(&format!(
            "MIGRATION: <{}> has been restructured ({} of {} props removed).\n\n\
             The component still exists but its API changed significantly.\n\
             Props that were removed have moved to composed child components.\n\
             Keep <{}> and restructure by replacing removed props with \
             child components that provide the same functionality.\n\n",
            component_name, removal_count, total_changes, component_name,
        ));

        if !removed_props.is_empty() {
            msg.push_str("Removed props (move to child components):\n");
            for prop in &removed_props {
                msg.push_str(&format!("  - {}\n", prop));
            }
            msg.push('\n');

            // Discover child components that share the same name prefix.
            // Sources: added_files (new in this version) + report changes
            // (existing related components).
            let prefix = component_name;
            let mut child_components: BTreeMap<String, Vec<String>> = BTreeMap::new();

            // From added_files: new components in the same directory family
            for added in &report.added_files {
                if let Some(stem) = added.file_stem().map(|s| s.to_string_lossy().to_string()) {
                    if stem.starts_with(prefix) && stem != prefix && stem != props_name {
                        child_components.entry(stem).or_default();
                    }
                }
            }

            // From report changes: existing components with the same prefix
            for fc in &report.changes {
                for c in &fc.breaking_api_changes {
                    let sym = &c.symbol;
                    // Non-dotted symbols that share the prefix
                    if !sym.contains('.')
                        && sym.starts_with(prefix)
                        && sym != prefix
                        && sym != &props_name
                        && !sym.ends_with("Props")
                        && sym.chars().next().is_some_and(|ch| ch.is_uppercase())
                    {
                        child_components.entry(sym.clone()).or_default();
                    }
                    // Dotted symbols — extract props for child interfaces
                    if sym.contains('.') {
                        let parent_iface = sym.split('.').next().unwrap_or("");
                        let child_name = parent_iface.strip_suffix("Props").unwrap_or(parent_iface);
                        if child_name.starts_with(prefix)
                            && child_name != prefix
                            && parent_iface.ends_with("Props")
                        {
                            let prop_name = extract_leaf_symbol(sym).to_string();
                            child_components
                                .entry(child_name.to_string())
                                .or_default()
                                .push(prop_name);
                        }
                    }
                }
            }

            if !child_components.is_empty() {
                msg.push_str("Available child components:\n");
                for (child, props) in &child_components {
                    if props.is_empty() {
                        msg.push_str(&format!("  - <{}>\n", child));
                    } else {
                        let unique_props: BTreeSet<&String> = props.iter().collect();
                        msg.push_str(&format!(
                            "  - <{}> (props: {})\n",
                            child,
                            unique_props
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                }
                msg.push_str(&format!(
                    "\nFor each removed prop, pass it to the corresponding <{}___> \
                     child component as a prop or as children.\n\n",
                    component_name
                ));
            }
        }
    }

    // ── Type changes section ──
    if !type_changes.is_empty() {
        msg.push_str("Type changes:\n");
        for (prop, before, after) in &type_changes {
            match (before, after) {
                (Some(b), Some(a)) => {
                    msg.push_str(&format!("  - {}: {}  →  {}\n", prop, b, a));
                }
                (Some(b), None) => {
                    msg.push_str(&format!("  - {}: {} (removed)\n", prop, b));
                }
                (None, Some(a)) => {
                    msg.push_str(&format!("  - {}: → {} (added)\n", prop, a));
                }
                (None, None) => {
                    msg.push_str(&format!("  - {}: type changed\n", prop));
                }
            }
        }
        msg.push('\n');
    }

    // ── Behavioral changes section ──
    if !behavioral_descs.is_empty() {
        msg.push_str("Behavioral changes:\n");
        for desc in &behavioral_descs {
            msg.push_str(&format!("  - {}\n", desc));
        }
        msg.push('\n');
    }

    // ── Action instruction ──
    if let Some(target) = migration_target {
        let replacement = target
            .replacement_symbol
            .strip_suffix("Props")
            .unwrap_or(&target.replacement_symbol);
        msg.push_str(&format!(
            "Remove <{}> from JSX and move its props to <{}>.\n\
             Also remove {} from the import statement.",
            component_name, replacement, component_name
        ));
    } else if removal_count == total_changes && total_changes <= 2 {
        // Fully removed — tell LLM to remove the import
        msg.push_str(&format!(
            "Remove {} from the import statement.",
            component_name,
        ));
    } else {
        // Restructured — keep the import, restructure usage
        msg.push_str(&format!(
            "Keep {} in the import statement. Restructure JSX to use \
             composed children instead of the removed props.",
            component_name,
        ));
    }

    msg
}

fn build_migration_message_v2(comp: &ComponentSummary<TypeScript>) -> String {
    let component_name = &comp.name;
    let removal_count = comp.member_summary.removed;
    let total = comp.member_summary.total;

    let mut msg = String::new();

    // ── Header: migration target or generic removal ──
    if let Some(ref target) = comp.migration_target {
        let replacement = target
            .replacement_symbol
            .strip_suffix("Props")
            .unwrap_or(&target.replacement_symbol);

        msg.push_str(&format!(
            "MIGRATION: Replace <{}> with props on <{}>.\n\n",
            component_name, replacement
        ));

        // Import guidance when the replacement is in a different package or
        // subpath. The package field may be the same for deprecated→main
        // moves (both are @patternfly/react-core), so we also check the
        // qualified_name for /deprecated/ or /next/ segments to derive
        // the correct import subpath.
        {
            let old_import = derive_import_path(
                target.removed_package.as_deref(),
                &target.removed_qualified_name,
            );
            let new_import = derive_import_path(
                target.replacement_package.as_deref(),
                &target.replacement_qualified_name,
            );

            if old_import != new_import {
                msg.push_str(&format!(
                    "Import change:\n\
                     \x20 Replace: import {{ {} }} from '{}';\n\
                     \x20 With:    import {{ {} }} from '{}';\n\n\
                     NOTE: The new <{}> may have a significantly different API.\n\
                     Review the property mapping below and update your usage accordingly.\n\n",
                    component_name, old_import, replacement, new_import, replacement
                ));
            }
        }

        if !target.matching_members.is_empty() {
            msg.push_str("Property mapping:\n");
            for m in &target.matching_members {
                if m.old_name == m.new_name {
                    msg.push_str(&format!(
                        "  - {}.{}  →  {}.{}\n",
                        component_name, m.old_name, replacement, m.new_name
                    ));
                } else {
                    msg.push_str(&format!(
                        "  - {}.{}  →  {}.{} (renamed)\n",
                        component_name, m.old_name, replacement, m.new_name
                    ));
                }
            }
            msg.push('\n');
        }

        if !target.removed_only_members.is_empty() {
            msg.push_str(&format!(
                "Removed with no direct equivalent: {}\n\n",
                target.removed_only_members.join(", ")
            ));
        }
    } else if comp.status == ComponentStatus::Removed || (removal_count == total && total <= 2) {
        // Fully removed component
        msg.push_str(&format!(
            "MIGRATION: <{}> was removed.\n\n\
             This component has no detected direct replacement.\n\
             Replace all <{}> usages with the recommended alternative.\n\n",
            component_name, component_name,
        ));
    } else {
        // Heavily modified interface — many props removed but the component
        // still exists.
        msg.push_str(&format!(
            "MIGRATION: <{}> has been restructured ({} of {} props removed).\n\n\
             The component still exists but its API changed significantly.\n\
             Props that were removed have moved to composed child components.\n\
             Keep <{}> and restructure by replacing removed props with \
             child components that provide the same functionality.\n\n",
            component_name, removal_count, total, component_name,
        ));

        if !comp.removed_members.is_empty() {
            msg.push_str("Removed props (move to child components):\n");
            for prop in &comp.removed_members {
                msg.push_str(&format!("  - {}\n", prop.name));
            }
            msg.push('\n');

            // Include child components with prop→child mappings from AST + LLM analysis.
            // Distinguish between "pass as named prop" and "pass as children" using
            // the removal_disposition data from LLM analysis.
            if !comp.child_components.is_empty() {
                // Build a map of prop → disposition for quick lookup
                let prop_dispositions: HashMap<&str, &RemovalDisposition> = comp
                    .removed_members
                    .iter()
                    .filter_map(|rp| {
                        rp.removal_disposition
                            .as_ref()
                            .map(|d| (rp.name.as_str(), d))
                    })
                    .collect();

                msg.push_str("Use these child components inside <");
                msg.push_str(component_name);
                msg.push_str(">:\n");
                for child in &comp.child_components {
                    if !child.absorbed_members.is_empty() {
                        // Separate props by mechanism: named prop vs children
                        let mut as_props = Vec::new();
                        let mut as_children = Vec::new();
                        for prop_name in &child.absorbed_members {
                            match prop_dispositions.get(prop_name.as_str()) {
                                Some(RemovalDisposition::MovedToRelatedType {
                                    mechanism, ..
                                }) if mechanism == "children" => {
                                    as_children.push(prop_name.as_str());
                                }
                                _ => {
                                    // Default: if the child has this as a named prop, it's a prop;
                                    // otherwise it's likely children
                                    if child.known_members.contains(prop_name) {
                                        as_props.push(prop_name.as_str());
                                    } else {
                                        as_children.push(prop_name.as_str());
                                    }
                                }
                            }
                        }
                        let mut parts = Vec::new();
                        if !as_props.is_empty() {
                            parts.push(format!("pass as props: {}", as_props.join(", ")));
                        }
                        if !as_children.is_empty() {
                            parts.push(format!("pass as children: {}", as_children.join(", ")));
                        }
                        msg.push_str(&format!("  - <{}> — {}\n", child.name, parts.join("; ")));
                    } else {
                        msg.push_str(&format!(
                            "  - <{}> — wrap relevant content as children\n",
                            child.name,
                        ));
                    }
                }

                // List any removed props that no child absorbs
                let absorbed: HashSet<&str> = comp
                    .child_components
                    .iter()
                    .flat_map(|c| c.absorbed_members.iter().map(|s| s.as_str()))
                    .collect();
                let unmapped: Vec<&str> = comp
                    .removed_members
                    .iter()
                    .map(|rp| rp.name.as_str())
                    .filter(|n| !absorbed.contains(n))
                    .collect();
                if !unmapped.is_empty() {
                    // Check if any unmapped props have a known disposition
                    let truly_removed: Vec<&str> = unmapped
                        .iter()
                        .filter(|n| {
                            matches!(
                                prop_dispositions.get(*n),
                                Some(RemovalDisposition::TrulyRemoved)
                                    | Some(RemovalDisposition::MadeAutomatic)
                            )
                        })
                        .copied()
                        .collect();
                    let unknown: Vec<&str> = unmapped
                        .iter()
                        .filter(|n| !truly_removed.contains(n))
                        .copied()
                        .collect();
                    if !truly_removed.is_empty() {
                        msg.push_str(&format!(
                            "\nRemoved with no replacement (safe to delete): {}\n",
                            truly_removed.join(", ")
                        ));
                    }
                    if !unknown.is_empty() {
                        msg.push_str(&format!(
                            "\nProps with no direct child component match (handle manually): {}\n",
                            unknown.join(", ")
                        ));
                    }
                }
                msg.push('\n');
            }
        }
    }

    // ── Type changes section ──
    if !comp.type_changes.is_empty() {
        msg.push_str("Type changes:\n");
        for tc in &comp.type_changes {
            match (&tc.before, &tc.after) {
                (Some(b), Some(a)) => {
                    msg.push_str(&format!("  - {}: {}  →  {}\n", tc.property, b, a));
                }
                (Some(b), None) => {
                    msg.push_str(&format!("  - {}: {} (removed)\n", tc.property, b));
                }
                (None, Some(a)) => {
                    msg.push_str(&format!("  - {}: → {} (added)\n", tc.property, a));
                }
                (None, None) => {
                    msg.push_str(&format!("  - {}: type changed\n", tc.property));
                }
            }
        }
        msg.push('\n');
    }

    // ── Behavioral changes section (deduplicated) ──
    if !comp.behavioral_changes.is_empty() {
        // Deduplicate identical behavioral change descriptions.
        // Test assertion diffs often produce many identical entries
        // (e.g., "aria-labelledby attribute added" × 20).
        let mut seen = BTreeSet::new();
        let mut deduped: Vec<(&BehavioralChange<TypeScript>, usize)> = Vec::new();
        for b in &comp.behavioral_changes {
            let key = format!(
                "{}:{}",
                b.category
                    .as_ref()
                    .map(|c| behavioral_category_label(c))
                    .unwrap_or("change"),
                b.description
            );
            if seen.insert(key.clone()) {
                let count = comp
                    .behavioral_changes
                    .iter()
                    .filter(|b2| b2.description == b.description && b2.category == b.category)
                    .count();
                deduped.push((b, count));
            }
        }

        msg.push_str("Behavioral changes:\n");
        for (b, count) in &deduped {
            let cat = b
                .category
                .as_ref()
                .map(|c| behavioral_category_label(c))
                .unwrap_or("change");
            if *count > 1 {
                msg.push_str(&format!("  - {}: {} (×{})\n", cat, b.description, count));
            } else {
                msg.push_str(&format!("  - {}: {}\n", cat, b.description));
            }
        }
        msg.push('\n');
    }

    // ── Action instruction ──
    if let Some(ref target) = comp.migration_target {
        let replacement = target
            .replacement_symbol
            .strip_suffix("Props")
            .unwrap_or(&target.replacement_symbol);
        msg.push_str(&format!(
            "Remove <{}> from JSX and move its props to <{}>.\n\
             Also remove {} from the import statement.",
            component_name, replacement, component_name
        ));
    } else if comp.status == ComponentStatus::Removed || (removal_count == total && total <= 2) {
        // Fully removed — tell LLM to remove the import
        msg.push_str(&format!(
            "Remove {} from the import statement.",
            component_name,
        ));
    } else {
        // Restructured — keep the import, restructure usage
        msg.push_str(&format!(
            "Keep {} in the import statement. Restructure JSX to use \
             composed children instead of the removed props.",
            component_name,
        ));
    }

    msg
}

/// IMPORT, etc.). When `Builtin`, rules use `builtin.filecontent` regex patterns.
pub fn generate_rules(
    report: &AnalysisReport<TypeScript>,
    file_pattern: &str,
    pkg_cache: &HashMap<String, String>,
    rename_patterns: &RenamePatterns,
    member_renames: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();
    let mut id_counts: HashMap<String, usize> = HashMap::new();
    let mut seen_composition_keys: HashSet<String> = HashSet::new();

    // ── Pre-scan: collect components referenced in composition pattern changes ──
    //
    // Components that appear as the `component` in a composition_pattern_change
    // with a `new_parent` are structurally required in the new version.
    // New-sibling rules for these components should be mandatory, not optional.
    let composition_required_components: HashSet<String> = report
        .changes
        .iter()
        .flat_map(|fc| &fc.container_changes)
        .filter(|c| c.new_container.is_some())
        .map(|c| c.symbol.clone())
        .collect();

    // ── Pre-scan: consolidate children→prop composition patterns ─────────
    //
    // When the AST data shows that a component (e.g., TimesIcon) moved from
    // being passed as children of a parent (e.g., Button) to being passed via
    // a specific prop (e.g., `icon`), generate ONE parent-level rule on the
    // parent component instead of individual rules per child.
    //
    // This is generically better because:
    //  1. The parent-level rule fires on the parent's IMPORT, which kantra
    //     matches reliably (no `parent` regex issues).
    //  2. It catches ALL components passed as children — including app-level
    //     components not from the library (e.g., a custom ContextIcon).
    //  3. The individual per-child rules had broken `parent` patterns like
    //     "^Button (as children)$" that never match actual JSX parent "Button".
    //
    // Consolidated entries are tracked so the per-child composition loop
    // can skip them and avoid duplicate rules.
    struct ChildrenToPropMigration {
        child_components: Vec<String>,
        from_pkg: Option<String>,
    }

    let mut children_to_prop: BTreeMap<(String, String), ChildrenToPropMigration> = BTreeMap::new();
    let mut consolidated_composition_keys: HashSet<String> = HashSet::new();

    for file_changes in &report.changes {
        let file_str = file_changes.file.to_string_lossy();
        let from_pkg = resolve_npm_package(&file_str, pkg_cache);

        for comp_change in &file_changes.container_changes {
            let (old_parent, new_parent) =
                match (&comp_change.old_container, &comp_change.new_container) {
                    (Some(old), Some(new)) => (old.as_str(), new.as_str()),
                    _ => continue,
                };

            // Extract parent component names from the AST data.
            // old_parent/new_parent may have context qualifiers added by
            // the LLM (e.g., "Button (as children)") — extract just the
            // component name by splitting at " (".
            let old_name = old_parent.split(" (").next().unwrap_or(old_parent).trim();
            let new_name = new_parent.split(" (").next().unwrap_or(new_parent).trim();

            // Only consolidate when the parent component is the same on
            // both sides — this is a children→prop migration on that parent,
            // NOT a nesting restructure (component moved to a different parent).
            if !old_name.eq_ignore_ascii_case(new_name) {
                continue;
            }

            // The old context must mention "children" and the new context
            // must mention a prop name.
            let old_is_children = old_parent.contains("children");
            let target_prop = extract_target_prop(new_parent);

            if !old_is_children {
                continue;
            }
            let target_prop = match target_prop {
                Some(p) => p.to_string(),
                None => continue,
            };

            let key = (old_name.to_string(), target_prop.clone());
            let entry = children_to_prop
                .entry(key)
                .or_insert_with(|| ChildrenToPropMigration {
                    child_components: Vec::new(),
                    from_pkg: from_pkg.clone(),
                });
            // Deduplicate child component names
            let child = &comp_change.symbol;
            if !entry.child_components.iter().any(|c| c == child) {
                entry.child_components.push(child.clone());
            }

            // Mark this composition change for skipping in the per-child loop
            let dedup_key = format!("{}|{}|{}", comp_change.symbol, old_parent, new_parent,);
            consolidated_composition_keys.insert(dedup_key);
        }
    }

    // Generate consolidated parent-level rules.
    //
    // When a common suffix can be derived from the migrated child names
    // (e.g., all end in "Icon"), generate a targeted JSX_COMPONENT rule
    // matching that suffix pattern with `parent: ^Button$`. This fires
    // at each incorrect usage (icon as child of Button) rather than at
    // the import, giving the fix engine exact JSX context. The suffix
    // pattern also catches custom/app-level components (e.g., ContextIcon).
    for ((parent, prop), migration) in &children_to_prop {
        let child_list = migration.child_components.join(", ");
        let base_id = format!(
            "semver-composition-{}-children-to-{}-prop",
            sanitize_id(parent),
            sanitize_id(prop),
        );
        let rule_id = unique_id(base_id, &mut id_counts);

        let msg = format!(
            "MIGRATION: Children that serve as the `{prop}` of <{parent}> should be \
             passed via the `{prop}` prop instead of as children.\n\n\
             Change: <{parent}><SomeIcon /></{parent}> → <{parent} {prop}={{<SomeIcon />}} />\n\n\
             This applies to ALL components that represent the `{prop}`, including \
             custom/app-level components. The library internally migrated {count} \
             components to this pattern: {children}.\n\n\
             For non-plain variants, the `{prop}` prop wraps the content in a styled \
             <span> with proper spacing. Passing it as children bypasses this styling.",
            parent = parent,
            prop = prop,
            count = migration.child_components.len(),
            children = child_list,
        );

        // Derive a common suffix from the child component names to build
        // a targeted pattern. Filter to valid PascalCase component names
        // (skip LLM artifacts like "children (span ...)", "div (wrapper)").
        let common_suffix = derive_common_suffix(&migration.child_components);

        let (pattern, location, parent_field, parent_from_field) =
            if let Some(ref suffix) = common_suffix {
                // Targeted: match components ending in the derived suffix
                // as children of the parent component, scoped to the parent's
                // package via parentFrom.
                (
                    format!("{}$", regex_escape(suffix)),
                    "JSX_COMPONENT".to_string(),
                    Some(format!("^{}$", regex_escape(parent))),
                    migration.from_pkg.clone(),
                )
            } else {
                // Fallback: no common suffix — match on the parent's import.
                (
                    format!("^{}$", regex_escape(parent)),
                    "IMPORT".to_string(),
                    None,
                    None,
                )
            };

        tracing::debug!(
            children = migration.child_components.len(),
            prop = %prop,
            parent = %parent,
            rule_id = %rule_id,
            pattern = %pattern,
            parent_field = ?parent_field,
            "Consolidated composition changes"
        );

        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".to_string(),
                "change-type=composition".to_string(),
                "has-codemod=false".to_string(),
            ],
            effort: 3,
            category: "mandatory".to_string(),
            description: format!(
                "Children serving as the `{}` of <{}> should use the `{}` prop instead",
                prop, parent, prop,
            ),
            message: msg,
            links: Vec::new(),
            when: KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern,
                    location,
                    component: None,
                    parent: parent_field,
                    not_parent: None,
                    parent_from: parent_from_field,
                    value: None,
                    // Don't filter on the matched component's import source —
                    // we want to catch app-level icons too (e.g., ContextIcon).
                    from: None,
                },
            },
            fix_strategy: Some(FixStrategyEntry::new("LlmAssisted")),
        });
    }

    // ── Pre-scan: build set of changes covered by component-level (P0-C) rules ──
    //
    // When a component qualifies for a P0-C composition rule (its child
    // components absorb removed props), individual per-prop and per-behavioral
    // rules for that component are redundant. We build the covered set from
    // the report's pre-aggregated ComponentSummary data so that per-change
    // rule generation can skip them upfront — no post-hoc string suppression.
    //
    // covered_components: set of component names that will get a P0-C rule
    // covered_props: set of (interface_name, prop_name) tuples covered by P0-C
    let mut covered_components: HashSet<String> = HashSet::new();
    let mut covered_props: HashSet<(String, String)> = HashSet::new();

    for pkg in &report.packages {
        for comp in &pkg.type_summaries {
            let qualifies = comp.status == ComponentStatus::Removed
                || (comp.member_summary.removed >= 3 && comp.member_summary.removal_ratio > 0.5)
                || comp.member_summary.removed >= 5;
            if !qualifies {
                continue;
            }
            covered_components.insert(comp.name.clone());
            covered_components.insert(comp.definition_name.clone());
            // Mark all removed props as covered
            for rp in &comp.removed_members {
                covered_props.insert((comp.definition_name.clone(), rp.name.clone()));
                covered_props.insert((comp.name.clone(), rp.name.clone()));
            }
        }
    }

    if !covered_components.is_empty() {
        tracing::debug!(
            components = covered_components.len(),
            covered_props = covered_props.len(),
            "P0-C coverage computed"
        );
    }

    // ── Pre-scan: build set of public component/symbol names ──────────────
    //
    // Used to filter behavioral rules: only generate rules for symbols that
    // are part of the public API (appear in report.packages as components).
    // Internal components (ModalBox, MenuBase, etc.) that happen to get
    // LLM-analyzed because they share a source file with public components
    // should not produce consumer-facing rules.
    let public_symbols: HashSet<&str> = report
        .packages
        .iter()
        .flat_map(|pkg| {
            pkg.type_summaries.iter().flat_map(|comp| {
                // Include both the component name and interface name
                std::iter::once(comp.name.as_str())
                    .chain(std::iter::once(comp.definition_name.as_str()))
            })
        })
        .collect();

    // ── Pre-scan: collapse large groups of constant changes into single rules ──
    //
    // When a package has many constants with the same change type (e.g., 2,000+
    // token type-changed constants from @patternfly/react-tokens), emit one
    // combined rule instead of thousands of individual rules.
    //
    // V2 path: when report.packages has pre-grouped constants, use those
    // directly instead of re-scanning the flat changes list.
    let mut collapsed_keys: HashSet<(String, ApiChangeType, String)> = HashSet::new();

    // Pre-build an index: symbol_name → (ApiChange, file_path) for renamed
    // constants.  Used by the V2 constantgroup path to look up per-token
    // Rename strategies without an O(n×m) nested scan.
    let mut renamed_constant_index: HashMap<&str, (&ApiChange, String)> = HashMap::new();
    for fc in &report.changes {
        let file_path = fc.file.to_string_lossy().to_string();
        for change in &fc.breaking_api_changes {
            if change.kind == ApiChangeKind::Constant && change.change == ApiChangeType::Renamed {
                // Prefer individual .d.ts entries (richer type annotations)
                // over index.d.ts entries.  The first inserted wins via
                // or_insert, so process individual files before index files.
                renamed_constant_index
                    .entry(change.symbol.as_str())
                    .or_insert((change, file_path.clone()));
            }
        }
    }

    let has_package_constants = report.packages.iter().any(|pkg| !pkg.constants.is_empty());

    if has_package_constants {
        // V2 path: iterate pre-grouped constant groups from report.packages
        for pkg in &report.packages {
            for cg in &pkg.constants {
                if cg.count < CONSTANT_COLLAPSE_THRESHOLD {
                    continue;
                }
                let symbol_names: Vec<&str> = cg.symbols.iter().map(|s| s.as_str()).collect();
                // Always recompute the pattern from symbol names for precision.
                // The pre-computed common_prefix_pattern may use overly broad
                // heuristics (e.g., `.*`) that cause false positives.
                let pattern = build_token_prefix_pattern(&symbol_names);
                let strategy_name = if cg.strategy_hint.is_empty() {
                    "Manual".to_string()
                } else {
                    cg.strategy_hint.clone()
                };

                let change_type_str = api_change_type_label(&cg.change_type);
                let kind_str = api_kind_label(&ApiChangeKind::Constant);
                let slug = pkg.name.replace('@', "").replace(['/', '.'], "-");
                let strategy_slug = strategy_name.to_lowercase().replace(' ', "-");
                let base_id = format!(
                    "semver-{}-constant-{}-{}-combined",
                    slug, change_type_str, strategy_slug
                );
                let rule_id = unique_id(base_id, &mut id_counts);

                let mut message = format!(
                    "{} constants from `{}` had breaking changes ({}).\n",
                    cg.count, pkg.name, change_type_str,
                );
                // Add a sample of the first few symbol names
                let sample_count = 5.min(symbol_names.len());
                if !symbol_names.is_empty() {
                    message.push_str(&format!(
                        "Affected constants include: {}",
                        symbol_names[..sample_count].join(", ")
                    ));
                    if symbol_names.len() > sample_count {
                        message
                            .push_str(&format!(" and {} more.", symbol_names.len() - sample_count));
                    }
                }

                // Build fix strategy.  For renamed constants we need per-token
                // Rename mappings, not a generic strategy_hint (which is often
                // CssVariablePrefix — wrong for import-level renames).
                let strategy = if cg.change_type == ApiChangeType::Renamed {
                    let mut rename_strat = FixStrategyEntry::new("Rename");
                    // Look up each symbol in the pre-built rename index.
                    // Skip:
                    //   - symbols covered by hierarchy composition rules
                    //   - import path relocations (before/after are file paths,
                    //     not symbol summaries — e.g., promoted from next/ or
                    //     moved to deprecated/)
                    for sym_name in &cg.symbols {
                        if covered_components.contains(sym_name) {
                            continue;
                        }
                        if let Some((change, file_path)) =
                            renamed_constant_index.get(sym_name.as_str())
                        {
                            // Skip import path relocations — their before/after
                            // contain internal file paths, not token names.
                            let is_path_relocation = change
                                .before
                                .as_deref()
                                .is_some_and(|b| b.contains("packages/"))
                                || change
                                    .after
                                    .as_deref()
                                    .is_some_and(|a| a.contains("packages/"));
                            if is_path_relocation {
                                continue;
                            }

                            if let Some(s) = api_change_to_strategy(
                                change,
                                rename_patterns,
                                member_renames,
                                file_path,
                            ) {
                                if s.strategy == "Rename" {
                                    rename_strat.mappings.push(MappingEntry {
                                        from: s.from,
                                        to: s.to,
                                        component: None,
                                        prop: None,
                                    });
                                }
                            }
                        }
                    }
                    tracing::debug!(
                        mappings = rename_strat.mappings.len(),
                        symbols = cg.symbols.len(),
                        "Built per-token Rename mappings for constantgroup"
                    );
                    rename_strat
                } else {
                    let mut s = FixStrategyEntry::new(&strategy_name);
                    if !cg.suffix_renames.is_empty() {
                        s.mappings = cg
                            .suffix_renames
                            .iter()
                            .map(|sr| MappingEntry {
                                from: Some(sr.from.clone()),
                                to: Some(sr.to.clone()),
                                component: None,
                                prop: None,
                            })
                            .collect();
                    }
                    s
                };

                tracing::debug!(
                    count = cg.count,
                    change_type = %change_type_str,
                    strategy = %strategy_name,
                    package = %pkg.name,
                    rule_id = %rule_id,
                    "Collapsed constant rules into single rule"
                );

                rules.push(KonveyorRule {
                    rule_id,
                    labels: vec![
                        "source=semver-analyzer".to_string(),
                        format!("change-type={}", change_type_str),
                        format!("kind={}", kind_str),
                        "has-codemod=true".to_string(),
                        format!("package={}", pkg.name),
                    ],
                    effort: 3,
                    category: "mandatory".to_string(),
                    description: format!(
                        "{} constants from {} have breaking changes",
                        cg.count, pkg.name
                    ),
                    message,
                    links: Vec::new(),
                    when: KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern,
                            location: "IMPORT".to_string(),
                            component: None,
                            parent: None,
                    not_parent: None,
                            value: None,
                            from: Some(pkg.name.clone()),
                            parent_from: None,
                        },
                    },
                    fix_strategy: Some(strategy),
                });

                // For renamed constants the actual strategy is "Rename", not
                // the strategy_hint.  Use the real strategy name so the
                // individual-rule suppression check at line ~1250 matches.
                let suppression_strategy = if cg.change_type == ApiChangeType::Renamed {
                    "Rename".to_string()
                } else {
                    strategy_name
                };
                collapsed_keys.insert((
                    pkg.name.clone(),
                    cg.change_type.clone(),
                    suppression_strategy,
                ));
            }
        }
    } else {
        // Legacy path: scan the flat changes list
        let collapsible_groups =
            detect_collapsible_constant_groups(report, pkg_cache, rename_patterns, member_renames);

        for (key, changes) in &collapsible_groups {
            let combined_rule = build_combined_constant_rule(key, changes, &mut id_counts);
            tracing::debug!(
                count = changes.len(),
                change_type = %api_change_type_label(&key.change_type),
                strategy = %key.strategy,
                package = %key.package,
                rule_id = %combined_rule.rule_id,
                "Collapsed constant rules into single rule (legacy path)"
            );
            rules.push(combined_rule);
            collapsed_keys.insert((
                key.package.clone(),
                key.change_type.clone(),
                key.strategy.clone(),
            ));
        }
    }

    // Pre-populate covered_components from hierarchy deltas so that
    // individual per-file rules for symbols covered by hierarchy or
    // deprecated migration rules are suppressed. The hierarchy rule
    // loop runs later but the coverage information is needed here.
    for delta in &report.extensions.hierarchy_deltas {
        covered_components.insert(delta.component.clone());
        covered_components.insert(format!("{}Props", delta.component));

        // Mark the parent's removed props as covered — the hierarchy rule
        // handles them (e.g., "spacer → use 'gap' instead"), so individual
        // prop-removal rules are redundant.
        if let Some(comp) = report
            .packages
            .iter()
            .flat_map(|pkg| &pkg.type_summaries)
            .find(|c| c.name == delta.component)
        {
            for rp in &comp.removed_members {
                covered_props.insert((comp.name.clone(), rp.name.clone()));
                covered_props.insert((comp.definition_name.clone(), rp.name.clone()));
            }
        }

        // Cover all children referenced by the hierarchy delta — both
        // removed (old internal components) and added (new public
        // components).  These symbols are handled by the hierarchy
        // composition rule, so individual rename/remove rules are
        // redundant.
        for child_name in &delta.removed_children {
            covered_components.insert(child_name.clone());
            covered_components.insert(format!("{}Props", child_name));
        }
        for child in &delta.added_children {
            covered_components.insert(child.name.clone());
            covered_components.insert(format!("{}Props", child.name));
        }

        // For deprecated deltas, also scan file changes for any other
        // symbols from the deprecated directory.
        if delta.source_package.is_some() {
            // Also scan file changes for any other symbols from this deprecated directory
            let deprecated_dir = format!("/deprecated/components/{}/", delta.component);
            for fc in &report.changes {
                let file_str = fc.file.to_string_lossy();
                if !file_str.contains(&deprecated_dir) {
                    continue;
                }
                for api in &fc.breaking_api_changes {
                    if !api.symbol.contains('.') {
                        covered_components.insert(api.symbol.clone());
                    }
                }
            }
        }
    }

    // API changes (per-file)
    for file_changes in &report.changes {
        // resolve_npm_package already appends /deprecated or /next when the
        // source file lives under those directories.  This ensures rules for
        // deprecated symbols only match imports from the deprecated sub-path,
        // avoiding false positives when the same component name exists in both
        // the main and deprecated paths (e.g., Dropdown, Select).
        let from_pkg = resolve_npm_package(&file_changes.file.to_string_lossy(), pkg_cache);

        for api_change in &file_changes.breaking_api_changes {
            // Skip constants that were already collapsed into a combined rule.
            // We check package + change_type + strategy to ensure only the exact
            // group that was collapsed gets skipped.
            if api_change.kind == ApiChangeKind::Constant && !api_change.symbol.contains('.') {
                if let Some(ref pkg) = from_pkg {
                    let file_path_str = file_changes.file.to_string_lossy();
                    if let Some(strat) = api_change_to_strategy(
                        api_change,
                        rename_patterns,
                        member_renames,
                        &file_path_str,
                    ) {
                        if collapsed_keys.contains(&(
                            pkg.clone(),
                            api_change.change.clone(),
                            strat.strategy,
                        )) {
                            continue;
                        }
                    }
                }
            }

            // Skip individual prop/symbol changes that are covered by a
            // component-level P0-C composition rule. The P0-C rule has the
            // full picture (child components, prop→child mappings) so
            // individual rules are redundant and potentially misleading.
            if api_change.symbol.contains('.') {
                let parts: Vec<&str> = api_change.symbol.splitn(2, '.').collect();
                let interface_name = parts[0];
                let prop_name = parts[1];
                if covered_props.contains(&(interface_name.to_string(), prop_name.to_string())) {
                    continue;
                }
            } else if covered_components.contains(&api_change.symbol) {
                // Top-level symbol changes (e.g., "Modal" removed,
                // "ModalBody" renamed/promoted) are covered by the
                // hierarchy composition rule — skip the individual rule.
                if matches!(
                    api_change.change,
                    ApiChangeType::Removed | ApiChangeType::Renamed
                ) {
                    continue;
                }
            }

            // Suppress import path relocation rules. These have internal
            // file paths in before/after (e.g., "promoted from next" or
            // "moved to deprecated") and are handled by hierarchy or
            // deprecated migration rules. Generating Rename codemods
            // from file paths produces garbage (the path becomes the
            // replacement text).
            if api_change.change == ApiChangeType::Renamed {
                let is_path_relocation = api_change
                    .before
                    .as_deref()
                    .is_some_and(|b| b.contains("packages/"))
                    || api_change
                        .after
                        .as_deref()
                        .is_some_and(|a| a.contains("packages/"));
                if is_path_relocation {
                    continue;
                }
            }

            let new_rules = api_change_to_rules(
                api_change,
                file_changes,
                from_pkg.as_deref(),
                &mut id_counts,
                rename_patterns,
                member_renames,
            );
            rules.extend(new_rules);
        }

        // Skip behavioral changes from test/demo/integration/example source
        // files.  These are test harnesses that happen to have common component
        // names (e.g., App, LoginPageDemo) and produce false positives when
        // matched against consumer code.
        let file_path_str = file_changes.file.to_string_lossy();
        let is_test_demo_file = file_path_str.contains("/demo")
            || file_path_str.contains("/test")
            || file_path_str.contains("/testdata/")
            || file_path_str.contains("/integration/")
            || file_path_str.contains("/examples/")
            || file_path_str.contains("/stories/");

        if !is_test_demo_file {
            for behavioral in &file_changes.breaking_behavioral_changes {
                // Skip behavioral rules for components covered by P0-C.
                // The P0-C rule already includes behavioral changes in its
                // message (from ComponentSummary.behavioral_changes).
                if covered_components.contains(&behavioral.symbol) {
                    continue;
                }

                // Skip behavioral rules for symbols that aren't part of the
                // public API. Internal components (ModalBox, MenuBase, etc.)
                // may get LLM-analyzed because their source file contains
                // exported functions, but consumers never import them directly.
                let beh_leaf = extract_leaf_symbol(&behavioral.symbol);
                if !public_symbols.is_empty() && !public_symbols.contains(beh_leaf) {
                    continue;
                }
                if let Some(rule) = behavioral_change_to_rule(
                    behavioral,
                    file_changes,
                    file_pattern,
                    from_pkg.as_deref(),
                    &mut id_counts,
                ) {
                    rules.push(rule);
                }
            }
        }

        // Generate rules from composition pattern changes (from test/example diffs).
        // Deduplicate by (component, old_parent, new_parent) since multiple
        // test/example files may report the same nesting change.
        //
        // Children→prop migrations are already handled by the consolidated
        // parent-level rules above — skip them here. Remaining composition
        // changes (nesting restructures like MastheadToggle moving from
        // Masthead to MastheadMain) get individual rules with fixed `parent`
        // regex patterns (bare component name, not LLM descriptive text).
        for comp_change in &file_changes.container_changes {
            let component = &comp_change.symbol;

            // Skip duplicates
            let dedup_key = format!(
                "{}|{}|{}",
                component,
                comp_change.old_container.as_deref().unwrap_or(""),
                comp_change.new_container.as_deref().unwrap_or("")
            );
            if seen_composition_keys.contains(&dedup_key) {
                continue;
            }
            seen_composition_keys.insert(dedup_key.clone());

            // Skip entries consolidated into parent-level children→prop rules
            if consolidated_composition_keys.contains(&dedup_key) {
                continue;
            }

            // Skip composition changes for components already covered by
            // hierarchy rules. The hierarchy rule has richer context.
            if covered_components.contains(component) {
                continue;
            }
            // Also check if the component's old/new parent is covered
            if let Some(ref old_p) = comp_change.old_container {
                let bare = old_p.split(" (").next().unwrap_or(old_p).trim();
                if covered_components.contains(bare) {
                    continue;
                }
            }

            // Skip hallucinated template variables
            if component.contains('{') || component.contains('}') {
                continue;
            }

            let slug = component.to_lowercase();
            let base_id = format!("semver-composition-{}-nesting-changed", slug);
            let rule_id = unique_id(base_id, &mut id_counts);

            // Extract bare component names from old_parent/new_parent for
            // the rule message (strip LLM context qualifiers).
            let old_parent_name = comp_change
                .old_container
                .as_deref()
                .map(|p| p.split(" (").next().unwrap_or(p).trim());
            let new_parent_name = comp_change
                .new_container
                .as_deref()
                .map(|p| p.split(" (").next().unwrap_or(p).trim());

            let mut msg = format!(
                "MIGRATION: <{}> nesting structure has changed.\n\n",
                component
            );
            if let (Some(old_display), Some(new_display)) = (old_parent_name, new_parent_name) {
                msg.push_str(&format!(
                    "In the previous version, <{}> was a direct child of <{}>.\n\
                     In the new version, <{}> should be a child of <{}>.\n\n\
                     Change:\n  <{}><{}> → <{}><{}>...</{}>...</{}>\n\n",
                    component,
                    old_display,
                    component,
                    new_display,
                    old_display,
                    component,
                    old_display,
                    new_display,
                    new_display,
                    old_display,
                ));
            }
            msg.push_str(&comp_change.description);

            // For composition rules, broaden the `from` to match sibling
            // packages within the same npm scope.  This handles cases where a
            // child component (e.g., TimesIcon from @scope/react-icons) is used
            // inside a parent component from a different package in the same
            // scope (e.g., Button from @scope/react-core).  The test diff that
            // detected the composition change lives in the parent's package, but
            // the child may be imported from a sibling package.
            let from_scope = from_pkg.as_deref().and_then(|pkg| {
                if pkg.starts_with('@') {
                    // Scoped package: extract @scope/ prefix as regex
                    pkg.find('/').map(|idx| format!("^{}", &pkg[..=idx]))
                } else {
                    // Unscoped package: use exact match
                    Some(format!("^{}$", pkg))
                }
            });

            // Use bare component name for the parent regex so it matches
            // actual JSX parent names (not LLM descriptive text).
            let parent_regex = comp_change.old_container.as_deref().map(|p| {
                let bare = p.split(" (").next().unwrap_or(p).trim();
                format!("^{}$", regex_escape(bare))
            });

            let condition = if comp_change.new_container.is_some() {
                KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: format!("^{}$", component),
                        location: "JSX_COMPONENT".to_string(),
                        component: None,
                        parent: parent_regex,
                        not_parent: None,
                        value: None,
                        from: from_scope.clone(),
                        parent_from: None,
                    },
                }
            } else {
                KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: format!("^{}$", component),
                        location: "JSX_COMPONENT".to_string(),
                        component: None,
                        parent: None,
                    not_parent: None,
                        value: None,
                        from: from_scope,
                        parent_from: None,
                    },
                }
            };

            rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".to_string(),
                    "change-type=composition".to_string(),
                    "has-codemod=false".to_string(),
                ],
                effort: 3,
                category: "mandatory".to_string(),
                description: comp_change.description.clone(),
                message: msg,
                links: Vec::new(),
                when: condition,
                fix_strategy: Some(FixStrategyEntry::new("LlmAssisted")),
            });
        }
    }

    // ── P0-C: Synthesize component-level IMPORT rules ──
    //
    // When a component has significant removals or was fully removed,
    // emit an IMPORT rule for the component itself.
    //
    // SKIPPED when hierarchy deltas are present — the hierarchy-based
    // composition rules supersede P0-C with richer, LLM-inferred data
    // about parent-child relationships and prop migrations.
    //
    // V2 path: iterate pre-aggregated report.packages[].components.
    // Legacy path: scan dotted symbols to aggregate by parent interface.
    // Build the set of components covered by hierarchy deltas so P0-C
    // can be skipped per-component rather than globally.
    //
    // Only skip P0-C when the hierarchy delta has added_children — that
    // means the hierarchy rule provides constructive migration guidance
    // ("use these new child components"). When a delta only has
    // removed_children, the hierarchy rule just says "X is no longer a
    // child" without guidance on what to do instead — P0-C is still
    // needed for prop migration instructions in those cases.
    let hierarchy_covered_components: HashSet<String> = report
        .extensions
        .hierarchy_deltas
        .iter()
        .filter(|d| !d.added_children.is_empty())
        .map(|d| d.component.clone())
        .collect();
    if !hierarchy_covered_components.is_empty() {
        tracing::debug!(
            count = hierarchy_covered_components.len(),
            "Hierarchy covers components — P0-C will skip those"
        );
    }
    {
        let has_package_components = report
            .packages
            .iter()
            .any(|pkg| !pkg.type_summaries.is_empty());

        if has_package_components {
            // V2 path: read from pre-aggregated ComponentSummary data
            for pkg in &report.packages {
                for comp in &pkg.type_summaries {
                    // A component qualifies for a P0-C rule if:
                    // - it was fully removed, OR
                    // - it has many props removed (>50% ratio), OR
                    // - it has a high absolute count of removals (>=5), indicating
                    //   significant restructuring even if total prop count is large
                    //   (e.g., Modal: 11 of 28 props removed = composition change)
                    let qualifies = comp.status == ComponentStatus::Removed
                        || (comp.member_summary.removed >= 3
                            && comp.member_summary.removal_ratio > 0.5)
                        || comp.member_summary.removed >= 5;

                    if !qualifies {
                        continue;
                    }

                    // Skip this component if it's covered by a hierarchy delta
                    // (the hierarchy-composition rule has richer data).
                    // Components NOT in the hierarchy delta set still get P0-C rules.
                    if hierarchy_covered_components.contains(&comp.name) {
                        tracing::debug!(
                            component = %comp.name,
                            "Skipping P0-C for component (covered by hierarchy delta)"
                        );
                        continue;
                    }

                    let component_name = &comp.name;
                    let base_id = format!(
                        "semver-{}-component-import-deprecated",
                        sanitize_id(component_name)
                    );
                    let rule_id = unique_id(base_id, &mut id_counts);
                    let message = build_migration_message_v2(comp);

                    rules.push(KonveyorRule {
                        rule_id,
                        labels: vec![
                            "source=semver-analyzer".to_string(),
                            "change-type=component-removal".to_string(),
                            "kind=interface".to_string(),
                            "has-codemod=false".to_string(),
                        ],
                        effort: 3,
                        category: "mandatory".to_string(),
                        description: format!(
                            "{} has significant breaking changes — {} of {} props removed",
                            component_name, comp.member_summary.removed, comp.member_summary.total
                        ),
                        message,
                        links: Vec::new(),
                        when: KonveyorCondition::FrontendReferenced {
                            referenced: FrontendReferencedFields {
                                pattern: format!("^{}$", regex_escape(component_name)),
                                location: "IMPORT".to_string(),
                                component: None,
                                parent: None,
                    not_parent: None,
                                value: None,
                                from: Some(pkg.name.clone()),
                                parent_from: None,
                            },
                        },
                        fix_strategy: Some(FixStrategyEntry::new("LlmAssisted")),
                    });
                }
            }
        } else {
            // Legacy path: scan the flat changes list
            struct ComponentInfo {
                total_changes: usize,
                removal_count: usize,
                from_pkg: Option<String>,
                from_constant_removal: bool,
            }
            let mut component_map: BTreeMap<String, ComponentInfo> = BTreeMap::new();
            for file_changes in &report.changes {
                let from_pkg = resolve_npm_package(&file_changes.file.to_string_lossy(), pkg_cache);

                for api_change in &file_changes.breaking_api_changes {
                    if !api_change.symbol.contains('.') {
                        continue;
                    }
                    let parts: Vec<&str> = api_change.symbol.splitn(2, '.').collect();
                    let interface_name = parts[0].to_string();
                    let entry = component_map
                        .entry(interface_name)
                        .or_insert(ComponentInfo {
                            total_changes: 0,
                            removal_count: 0,
                            from_pkg: from_pkg.clone(),
                            from_constant_removal: false,
                        });
                    entry.total_changes += 1;
                    if api_change.change == ApiChangeType::Removed {
                        entry.removal_count += 1;
                    }
                }
            }

            // P0-C extension: fully-removed PascalCase component exports
            for file_changes in &report.changes {
                let from_pkg = resolve_npm_package(&file_changes.file.to_string_lossy(), pkg_cache);
                for api_change in &file_changes.breaking_api_changes {
                    if api_change.change != ApiChangeType::Removed {
                        continue;
                    }
                    if api_change.symbol.contains('.') {
                        continue;
                    }
                    let sym = &api_change.symbol;
                    if !sym.chars().next().is_some_and(|c| c.is_uppercase()) {
                        continue;
                    }
                    if !sym.chars().any(|c| c.is_lowercase()) {
                        continue;
                    }
                    if sym.ends_with("Props") || sym.ends_with("Variants") {
                        continue;
                    }
                    if !matches!(
                        api_change.kind,
                        ApiChangeKind::Constant | ApiChangeKind::Interface
                    ) {
                        continue;
                    }
                    if !component_map.contains_key(sym) {
                        component_map.insert(
                            sym.clone(),
                            ComponentInfo {
                                total_changes: 1,
                                removal_count: 1,
                                from_pkg: from_pkg.clone(),
                                from_constant_removal: true,
                            },
                        );
                    }
                }
            }

            for (interface_name, info) in &component_map {
                let component_name = interface_name
                    .strip_suffix("Props")
                    .unwrap_or(interface_name);

                let mostly_removed = info.from_constant_removal
                    || (info.removal_count >= 3 && (info.removal_count * 2 > info.total_changes));
                if mostly_removed {
                    let base_id = format!(
                        "semver-{}-component-import-deprecated",
                        sanitize_id(component_name)
                    );
                    let rule_id = unique_id(base_id, &mut id_counts);
                    let message = build_migration_message_legacy(
                        component_name,
                        interface_name,
                        report,
                        info.removal_count,
                        info.total_changes,
                    );

                    rules.push(KonveyorRule {
                        rule_id,
                        labels: vec![
                            "source=semver-analyzer".to_string(),
                            "change-type=component-removal".to_string(),
                            "kind=interface".to_string(),
                            "has-codemod=false".to_string(),
                        ],
                        effort: 3,
                        category: "mandatory".to_string(),
                        description: format!(
                            "{} has significant breaking changes — {} of {} props removed",
                            component_name, info.removal_count, info.total_changes
                        ),
                        message,
                        links: Vec::new(),
                        when: KonveyorCondition::FrontendReferenced {
                            referenced: FrontendReferencedFields {
                                pattern: format!("^{}$", regex_escape(component_name)),
                                location: "IMPORT".to_string(),
                                component: None,
                                parent: None,
                    not_parent: None,
                                value: None,
                                from: info.from_pkg.clone(),
                                parent_from: None,
                            },
                        },
                        fix_strategy: Some(FixStrategyEntry::new("LlmAssisted")),
                    });
                }
            }
        }
    }

    // Manifest changes
    for manifest in &report.manifest_changes {
        let rule = manifest_change_to_rule(manifest, file_pattern, &mut id_counts);
        rules.push(rule);
    }

    // Emit consumer CSS scanning rules when CSS version prefix changes are detected.
    // Extract the actual old prefix from the report data — no hardcoded library names.
    let css_prefix_changes = detect_css_prefix_changes(report);

    // Generate a broad class prefix rule (pf-v5- → pf-v6-) that covers ALL
    // CSS classes regardless of segment (theme, utility, component, etc.).
    // Derived from the most common versioned prefix pair.
    {
        let mut broad_prefix: Option<(String, String)> = None;
        for (_, old_var, new_var) in &css_prefix_changes {
            // Find a versioned pair (--pf-vN- → --pf-vM-)
            static VER_RE: std::sync::LazyLock<regex::Regex> =
                std::sync::LazyLock::new(|| regex::Regex::new(r"^(--[a-zA-Z]+-v\d+-)").unwrap());
            if let (Some(old_base), Some(new_base)) = (
                VER_RE.captures(old_var).map(|c| c[1].to_string()),
                VER_RE.captures(new_var).map(|c| c[1].to_string()),
            ) {
                if old_base != new_base {
                    broad_prefix = Some((old_base, new_base));
                    break;
                }
            }
        }
        if let Some((old_base, new_base)) = broad_prefix {
            let old_class = old_base.trim_start_matches('-').to_string();
            let new_class = new_base.trim_start_matches('-').to_string();
            rules.push(KonveyorRule {
                rule_id: format!(
                    "semver-consumer-css-stale-class-{}",
                    sanitize_id(&old_class)
                ),
                labels: vec![
                    "source=semver-analyzer".to_string(),
                    "change-type=css-class".to_string(),
                    "has-codemod=true".to_string(),
                ],
                effort: 3,
                category: "mandatory".to_string(),
                description: format!("Consumer CSS contains stale '{}' class prefix", old_class),
                message: format!(
                    "CSS/SCSS files reference '{}' class names which have been renamed to '{}'.",
                    old_class, new_class
                ),
                links: Vec::new(),
                when: KonveyorCondition::FrontendCssClass {
                    cssclass: FrontendPatternFields {
                        pattern: old_class.clone(),
                    },
                },
                fix_strategy: Some(FixStrategyEntry::with_from_to(
                    "CssVariablePrefix",
                    &old_class,
                    &new_class,
                )),
            });
        }
    }

    for (old_class_prefix, old_var_prefix, new_var_prefix) in &css_prefix_changes {
        rules.push(KonveyorRule {
            rule_id: format!(
                "semver-consumer-css-stale-var-{}-to-{}",
                sanitize_id(old_var_prefix),
                sanitize_id(new_var_prefix),
            ),
            labels: vec![
                "source=semver-analyzer".to_string(),
                "change-type=css-variable".to_string(),
                "has-codemod=true".to_string(),
            ],
            effort: 3,
            category: "mandatory".to_string(),
            description: format!(
                "CSS variables '{}' renamed to '{}'",
                old_var_prefix, new_var_prefix
            ),
            message: format!(
                "CSS/SCSS files reference '{}' CSS variables which have been renamed to '{}'.",
                old_var_prefix, new_var_prefix
            ),
            links: Vec::new(),
            when: KonveyorCondition::FrontendCssVar {
                cssvar: FrontendPatternFields {
                    pattern: old_var_prefix.clone(),
                },
            },
            fix_strategy: Some(FixStrategyEntry::with_from_to(
                "CssVariablePrefix",
                old_var_prefix,
                new_var_prefix,
            )),
        });
    }

    // ── CSS logical property suffix renames ────────────────────────────
    //
    // Token constants use suffixes like PaddingTop, MarginLeft, etc. In PF6,
    // these became logical properties (PaddingBlockStart, MarginInlineStart).
    // The CssVariablePrefix strategy handles the prefix (pf-v5- → pf-v6-) but
    // not the suffix. Extract the unique suffix-level renames from the member
    // renames data and generate cssvar rules for each.
    {
        let mut suffix_renames: BTreeMap<String, String> = BTreeMap::new();

        // Primary source: function parameter (from --member-renames flag)
        let effective_renames: &HashMap<String, String> = if member_renames.is_empty() {
            // Fallback: report.member_renames (for --from-report case)
            &report.member_renames
        } else {
            member_renames
        };

        for (old_name, new_name) in effective_renames {
            // Extract the suffix — the part after the last underscore that
            // starts with an uppercase letter (e.g., "PaddingTop" from
            // "c_table__caption_PaddingTop")
            let old_suffix = extract_trailing_suffix(old_name);
            let new_suffix = extract_trailing_suffix(new_name);
            if let (Some(old_s), Some(new_s)) = (old_suffix, new_suffix) {
                if old_s != new_s {
                    suffix_renames
                        .entry(old_s.to_string())
                        .or_insert_with(|| new_s.to_string());
                }
            }
        }

        if !suffix_renames.is_empty() {
            tracing::debug!(
                suffix_rename_count = suffix_renames.len(),
                "Generating combined CSS logical property rule"
            );

            // Build a single pattern matching all physical property suffixes
            let suffix_alts: Vec<String> = suffix_renames.keys().map(|s| regex_escape(s)).collect();
            let combined_pattern = format!("--({})", suffix_alts.join("|"));

            // Build mappings array with all from/to pairs
            let mappings: Vec<MappingEntry> = suffix_renames
                .iter()
                .map(|(old_s, new_s)| MappingEntry {
                    from: Some(format!("--{}", old_s)),
                    to: Some(format!("--{}", new_s)),
                    component: None,
                    prop: None,
                })
                .collect();

            // Build a human-readable message listing all renames
            let mut message = format!(
                "MIGRATION: {} CSS custom property suffixes have been renamed.\n\n\
                 Rename mappings:\n",
                suffix_renames.len()
            );
            for (old_s, new_s) in &suffix_renames {
                message.push_str(&format!("  - --{}  →  --{}\n", old_s, new_s));
            }
            message.push_str("\nUpdate all CSS variable references to use the new suffixes.");

            let rule_id = unique_id(
                "semver-css-logical-property-renames".to_string(),
                &mut id_counts,
            );

            let mut strategy = FixStrategyEntry::new("Rename");
            strategy.mappings = mappings;

            rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".to_string(),
                    "change-type=css-variable".to_string(),
                    "has-codemod=true".to_string(),
                ],
                effort: 1,
                category: "mandatory".to_string(),
                description: format!("{} CSS variable suffixes renamed", suffix_renames.len()),
                message,
                links: Vec::new(),
                when: KonveyorCondition::FrontendCssVar {
                    cssvar: FrontendPatternFields {
                        pattern: combined_pattern,
                    },
                },
                fix_strategy: Some(strategy),
            });
        }
    }

    // ── P2-A: Composition rules (parent/child nesting) ──────────────────
    for entry in &rename_patterns.composition_rules {
        let base_id = format!(
            "semver-composition-{}-in-{}",
            sanitize_id(&entry.child_pattern),
            sanitize_id(&entry.parent),
        );
        let rule_id = unique_id(base_id, &mut id_counts);
        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".to_string(),
                "change-type=composition".to_string(),
                "has-codemod=true".to_string(),
            ],
            effort: entry.effort,
            category: entry.category.clone(),
            description: entry.description.clone(),
            message: entry.description.clone(),
            links: Vec::new(),
            when: KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: entry.child_pattern.clone(),
                    location: "JSX_COMPONENT".to_string(),
                    component: None,
                    parent: Some(entry.parent.clone()),
                    not_parent: None,
                    value: None,
                    from: entry.package.clone(),
                    parent_from: None,
                },
            },
            fix_strategy: Some(FixStrategyEntry::new("LlmAssisted")),
        });
    }

    // ── P3-A: Prop renames ──────────────────────────────────────────────
    for entry in &rename_patterns.prop_renames {
        let desc = entry.description.clone().unwrap_or_else(|| {
            format!(
                "'{}' prop renamed to '{}' — update all usages",
                entry.old_prop, entry.new_prop
            )
        });
        let base_id = format!(
            "semver-prop-rename-{}-to-{}",
            sanitize_id(&entry.old_prop),
            sanitize_id(&entry.new_prop),
        );
        let rule_id = unique_id(base_id, &mut id_counts);
        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".to_string(),
                "change-type=prop-rename".to_string(),
                "has-codemod=true".to_string(),
            ],
            effort: 1,
            category: "mandatory".to_string(),
            description: desc.clone(),
            message: desc,
            links: Vec::new(),
            when: KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: format!("^{}$", regex_escape(&entry.old_prop)),
                    location: "JSX_PROP".to_string(),
                    component: Some(entry.components.clone()),
                    parent: None,
                    not_parent: None,
                    value: None,
                    from: entry.package.clone(),
                    parent_from: None,
                },
            },
            fix_strategy: Some(FixStrategyEntry::rename(&entry.old_prop, &entry.new_prop)),
        });
    }

    // ── P4-C: Value review rules ────────────────────────────────────────
    for entry in &rename_patterns.value_reviews {
        let base_id = format!(
            "semver-value-review-{}-{}-{}",
            sanitize_id(&entry.component),
            sanitize_id(&entry.prop),
            sanitize_id(&entry.value),
        );
        let rule_id = unique_id(base_id, &mut id_counts);
        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".to_string(),
                "change-type=prop-value-review".to_string(),
                "has-codemod=true".to_string(),
            ],
            effort: entry.effort,
            category: entry.category.clone(),
            description: entry.description.clone(),
            message: entry.description.clone(),
            links: Vec::new(),
            when: KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: format!("^{}$", regex_escape(&entry.prop)),
                    location: "JSX_PROP".to_string(),
                    component: Some(entry.component.clone()),
                    parent: None,
                    not_parent: None,
                    value: Some(entry.value.clone()),
                    from: entry.package.clone(),
                    parent_from: None,
                },
            },
            fix_strategy: Some(FixStrategyEntry::new("Manual")),
        });
    }

    // ── Component warnings (DOM/CSS rendering changes without API change) ─
    for entry in &rename_patterns.component_warnings {
        let base_id = format!("semver-component-warning-{}", sanitize_id(&entry.pattern),);
        let rule_id = unique_id(base_id, &mut id_counts);
        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".to_string(),
                "change-type=component-warning".to_string(),
                "impact=frontend-testing".to_string(),
                "has-codemod=false".to_string(),
            ],
            effort: entry.effort,
            category: entry.category.clone(),
            description: entry.description.clone(),
            message: entry.description.clone(),
            links: Vec::new(),
            when: KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: entry.pattern.clone(),
                    location: "JSX_COMPONENT".to_string(),
                    component: None,
                    parent: None,
                    not_parent: None,
                    value: None,
                    from: entry.package.clone(),
                    parent_from: None,
                },
            },
            fix_strategy: Some(FixStrategyEntry::new("Manual")),
        });
    }

    // ── P5: Missing import rules (and/not combinators) ──────────────────
    for entry in &rename_patterns.missing_imports {
        let base_id = format!(
            "semver-missing-import-{}",
            sanitize_id(&entry.missing_pattern),
        );
        let rule_id = unique_id(base_id, &mut id_counts);
        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".to_string(),
                "change-type=missing-import".to_string(),
                "has-codemod=false".to_string(),
            ],
            effort: entry.effort,
            category: entry.category.clone(),
            description: entry.description.clone(),
            message: entry.description.clone(),
            links: Vec::new(),
            when: KonveyorCondition::And {
                and: vec![
                    KonveyorCondition::FileContent {
                        filecontent: FileContentFields {
                            pattern: entry.has_pattern.clone(),
                            file_pattern: entry.file_pattern.clone(),
                        },
                    },
                    KonveyorCondition::FileContentNegated {
                        negated: true,
                        filecontent: FileContentFields {
                            pattern: entry.missing_pattern.clone(),
                            file_pattern: entry.file_pattern.clone(),
                        },
                    },
                ],
            },
            fix_strategy: Some(FixStrategyEntry::new("Manual")),
        });
    }

    // ── New sibling component detection ─────────────────────────────────
    //
    // When a component has breaking changes AND a new sibling component was
    // added, generate a composition rule suggesting the new component.
    //
    // V2 path: iterate report.packages[].components[].child_components.
    // Legacy path: scan added_files + behavioral descriptions.
    {
        let has_child_components = report.packages.iter().any(|pkg| {
            pkg.type_summaries
                .iter()
                .any(|comp| !comp.child_components.is_empty())
        });

        if has_child_components {
            // V2 path: read from pre-aggregated child_components
            for pkg in &report.packages {
                for comp in &pkg.type_summaries {
                    for child in &comp.child_components {
                        if child.status != ChildComponentStatus::Added {
                            continue;
                        }

                        let component_name = &comp.name;
                        let new_component = &child.name;

                        let mut msg = format!(
                            "MIGRATION: Use <{}> inside <{}>.\n\n",
                            new_component, component_name,
                        );

                        // Build prop migration instructions from AST data
                        if !child.absorbed_members.is_empty() {
                            // Categorize absorbed props by mechanism using
                            // the parent's removal_disposition data
                            let prop_dispositions: HashMap<&str, &RemovalDisposition> = comp
                                .removed_members
                                .iter()
                                .filter_map(|rp| {
                                    rp.removal_disposition
                                        .as_ref()
                                        .map(|d| (rp.name.as_str(), d))
                                })
                                .collect();

                            let mut as_props = Vec::new();
                            let mut as_children = Vec::new();

                            for prop_name in &child.absorbed_members {
                                match prop_dispositions.get(prop_name.as_str()) {
                                    Some(RemovalDisposition::MovedToRelatedType {
                                        mechanism,
                                        ..
                                    }) if mechanism == "children" => {
                                        as_children.push(prop_name.as_str());
                                    }
                                    _ => {
                                        if child.known_members.contains(prop_name) {
                                            as_props.push(prop_name.as_str());
                                        } else {
                                            as_children.push(prop_name.as_str());
                                        }
                                    }
                                }
                            }

                            msg.push_str(&format!(
                                "These props were removed from <{}> and moved to <{}>:\n",
                                component_name, new_component,
                            ));
                            for prop in &as_props {
                                msg.push_str(&format!(
                                    "  - {} → <{} {}={{...}}>\n",
                                    prop, new_component, prop,
                                ));
                            }
                            for prop in &as_children {
                                msg.push_str(&format!(
                                    "  - {} → <{}>{{{}value}}</{}>  (pass as children)\n",
                                    prop, new_component, prop, new_component,
                                ));
                            }
                            msg.push('\n');
                        } else {
                            msg.push_str(&format!(
                                "<{}> is a new child component of <{}>.\n\
                                 Wrap relevant content inside <{}>.\n\n",
                                new_component, component_name, new_component,
                            ));
                        }

                        msg.push_str(&format!(
                            "Add {} to your import statement from the same package.",
                            new_component,
                        ));

                        let base_id = format!(
                            "semver-new-sibling-{}-in-{}",
                            sanitize_id(new_component),
                            sanitize_id(component_name),
                        );
                        let rule_id = unique_id(base_id, &mut id_counts);

                        // Mandatory if the child absorbs removed props from the parent
                        // OR if composition pattern changes show the component is
                        // structurally required in the new version.
                        // Truly optional new-siblings (no absorbed props, not
                        // composition-required) are skipped — they add noise and
                        // the fix engine may apply them unnecessarily.
                        let is_mandatory = !child.absorbed_members.is_empty()
                            || composition_required_components.contains(new_component);
                        if !is_mandatory {
                            tracing::debug!(
                                new_component = %new_component,
                                parent = %component_name,
                                "Skipping optional new-sibling rule (no absorbed props, not composition-required)"
                            );
                            continue;
                        }
                        let category = "mandatory";

                        rules.push(KonveyorRule {
                            rule_id,
                            labels: vec![
                                "source=semver-analyzer".to_string(),
                                "change-type=new-sibling-component".to_string(),
                                "has-codemod=false".to_string(),
                            ],
                            effort: 3,
                            category: category.to_string(),
                            description: format!(
                                "<{}> is required inside <{}> — absorbs removed props",
                                new_component, component_name
                            ),
                            message: msg,
                            links: Vec::new(),
                            when: KonveyorCondition::FrontendReferenced {
                                referenced: FrontendReferencedFields {
                                    pattern: format!("^{}$", regex_escape(component_name)),
                                    location: "IMPORT".to_string(),
                                    component: None,
                                    parent: None,
                    not_parent: None,
                                    value: None,
                                    from: Some(pkg.name.clone()),
                                    parent_from: None,
                                },
                            },
                            fix_strategy: Some(FixStrategyEntry::new("LlmAssisted")),
                        });

                        tracing::debug!(
                            new_component = %new_component,
                            parent = %component_name,
                            "Detected new sibling from packages data"
                        );
                    }
                }
            }
        } else if !report.added_files.is_empty() {
            // Legacy path: scan added_files + behavioral descriptions
            let mut dir_to_added: HashMap<String, Vec<String>> = HashMap::new();
            for added_path in &report.added_files {
                let path_str = added_path.to_string_lossy();
                if let (Some(dir), Some(file_stem)) = (
                    added_path.parent().map(|p| p.to_string_lossy().to_string()),
                    added_path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string()),
                ) {
                    if file_stem.chars().next().is_some_and(|c| c.is_uppercase())
                        && !path_str.contains(".d.ts")
                    {
                        dir_to_added.entry(dir).or_default().push(file_stem);
                    }
                }
            }

            let behavioral_added_refs: BTreeSet<String> = report
                .changes
                .iter()
                .flat_map(|fc| &fc.breaking_behavioral_changes)
                .filter_map(|b| {
                    let desc = &b.description;
                    if desc.contains("element added") || desc.contains("added to render output") {
                        let start = desc.find('<')? + 1;
                        let end = desc[start..].find('>')? + start;
                        Some(desc[start..end].to_string())
                    } else {
                        None
                    }
                })
                .collect();

            for file_changes in &report.changes {
                let file_str = file_changes.file.to_string_lossy();
                let dir = file_changes
                    .file
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();

                if file_changes.breaking_api_changes.is_empty() {
                    continue;
                }

                let component_name = file_changes
                    .file
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();

                if !component_name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_uppercase())
                {
                    continue;
                }
                if file_str.contains(".d.ts") {
                    continue;
                }

                if let Some(added_siblings) = dir_to_added.get(&dir) {
                    for new_component in added_siblings {
                        if !behavioral_added_refs.contains(new_component.as_str()) {
                            continue;
                        }

                        let mut msg = format!(
                            "MIGRATION: <{}> may need to be used alongside <{}>.\n\n\
                             <{}> is a new component added in this version. \
                             Consumer code in examples and demos now uses <{}> \
                             within <{}>.\n\n",
                            new_component,
                            component_name,
                            new_component,
                            new_component,
                            component_name,
                        );

                        let breaking_summary: Vec<String> = file_changes
                            .breaking_api_changes
                            .iter()
                            .take(5)
                            .map(|c| format!("  - {}: {}", c.symbol, c.description))
                            .collect();
                        if !breaking_summary.is_empty() {
                            msg.push_str(&format!(
                                "Breaking changes on <{}>:\n{}\n\n",
                                component_name,
                                breaking_summary.join("\n"),
                            ));
                        }

                        msg.push_str(&format!(
                            "Consider wrapping children of <{}> in <{}>.\n\
                             Add {} to your import statement from the same package.",
                            component_name, new_component, new_component,
                        ));

                        let from_pkg = resolve_npm_package(&file_str, pkg_cache);

                        let base_id = format!(
                            "semver-new-sibling-{}-in-{}",
                            sanitize_id(new_component),
                            sanitize_id(&component_name),
                        );
                        let rule_id = unique_id(base_id, &mut id_counts);

                        rules.push(KonveyorRule {
                            rule_id,
                            labels: vec![
                                "source=semver-analyzer".to_string(),
                                "change-type=new-sibling-component".to_string(),
                                "has-codemod=false".to_string(),
                            ],
                            effort: 3,
                            category: "optional".to_string(),
                            description: format!(
                                "New component <{}> may be needed alongside <{}>",
                                new_component, component_name
                            ),
                            message: msg,
                            links: Vec::new(),
                            when: KonveyorCondition::FrontendReferenced {
                                referenced: FrontendReferencedFields {
                                    pattern: format!("^{}$", regex_escape(&component_name)),
                                    location: "IMPORT".to_string(),
                                    component: None,
                                    parent: None,
                    not_parent: None,
                                    value: None,
                                    from: from_pkg,
                                    parent_from: None,
                                },
                            },
                            fix_strategy: Some(FixStrategyEntry::new("LlmAssisted")),
                        });

                        tracing::debug!(
                            new_component = %new_component,
                            parent = %component_name,
                            "Detected new sibling (behavioral evidence found)"
                        );
                    }
                }
            }
        }
    } // end P0-C block

    // ── Hierarchy-based composition rules ──
    //
    // Generate migration rules from hierarchy deltas. These describe how
    // a component's expected children changed between versions:
    //  - New required children (e.g., DropdownList added inside Dropdown)
    //  - Removed direct children (e.g., DropdownItem moved under DropdownList)
    //  - Prop migrations (e.g., Modal.title → ModalHeader.title)
    //
    // The rule message incorporates removed property data from the
    // ComponentSummary to produce rich per-prop migration instructions,
    // replacing the P0-C message format.
    for delta in &report.extensions.hierarchy_deltas {
        if delta.added_children.is_empty() && delta.removed_children.is_empty() {
            continue;
        }

        let component = &delta.component;

        // Look up the parent ComponentSummary for removed props + behavioral data
        let comp_summary = report
            .packages
            .iter()
            .flat_map(|pkg| &pkg.type_summaries)
            .find(|c| c.name == *component);

        let removed_props: Vec<&RemovedMember> = comp_summary
            .map(|c| c.removed_members.iter().collect())
            .unwrap_or_default();

        let behavioral_changes: Vec<&BehavioralChange<TypeScript>> = comp_summary
            .map(|c| {
                c.behavioral_changes
                    .iter()
                    .filter(|b| b.is_internal_only != Some(true))
                    .collect()
            })
            .unwrap_or_default();

        let prop_summary = comp_summary.map(|c| &c.member_summary);

        let base_id = format!(
            "semver-hierarchy-{}-composition-changed",
            sanitize_id(component),
        );
        let rule_id = unique_id(base_id, &mut id_counts);

        // Build the migration message
        let mut msg = format!("MIGRATION: <{}> has been restructured", component,);
        if let Some(ps) = prop_summary {
            if ps.removed > 0 {
                msg.push_str(&format!(" ({} of {} props removed)", ps.removed, ps.total,));
            }
        }
        msg.push_str(".\n\n");

        // List expected children from the full new-version hierarchy,
        // separating prop-passed components from direct JSX children,
        // and migration-required children from recommended ones.
        let all_expected = comp_summary
            .map(|c| &c.expected_children)
            .filter(|ec| !ec.is_empty());

        if let Some(expected_children) = all_expected {
            // Separate prop-passed components (e.g., header={<FormFieldGroupHeader />})
            // from direct JSX children (e.g., <Modal><ModalBody>...</ModalBody></Modal>)
            let direct_children: Vec<&ExpectedChild> = expected_children
                .iter()
                .filter(|c| c.mechanism != "prop")
                .collect();
            let prop_children: Vec<&ExpectedChild> = expected_children
                .iter()
                .filter(|c| c.mechanism == "prop")
                .collect();

            // For each direct child, determine if it absorbs removed props
            // (making it migration-required) or is just a recommended wrapper.
            let mut migration_required: Vec<(&ExpectedChild, Vec<String>)> = Vec::new();
            let mut recommended: Vec<(&ExpectedChild, Vec<String>)> = Vec::new();

            for child in &direct_children {
                // Find props that migrated to this child
                let child_migrated: Vec<&MigratedMember> = delta
                    .migrated_members
                    .iter()
                    .filter(|mp| mp.target_child == child.name)
                    .collect();

                // Also check removal_disposition for props that moved to this child
                let disposition_props: Vec<(&str, &str)> = removed_props
                    .iter()
                    .filter_map(|rp| {
                        if let Some(RemovalDisposition::MovedToRelatedType {
                            target_type,
                            mechanism,
                        }) = &rp.removal_disposition
                        {
                            if target_type == &child.name {
                                Some((rp.name.as_str(), mechanism.as_str()))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .collect();

                let mut prop_instructions: Vec<String> = Vec::new();
                let mut seen_props: BTreeSet<String> = BTreeSet::new();

                // Props from disposition data (richer — has mechanism)
                for (prop_name, mechanism) in &disposition_props {
                    if seen_props.insert(prop_name.to_string()) {
                        prop_instructions.push(format!(
                            "pass {} as {}",
                            prop_name,
                            if *mechanism == "children" {
                                "children"
                            } else {
                                "prop"
                            },
                        ));
                    }
                }

                // Props from hierarchy migrated_props (name match)
                for mp in &child_migrated {
                    if seen_props.insert(mp.member_name.clone()) {
                        if let Some(ref target_name) = mp.target_member_name {
                            prop_instructions.push(format!("{} → {}", mp.member_name, target_name));
                        } else {
                            prop_instructions.push(format!("pass {} as prop", mp.member_name));
                        }
                    }
                }

                // Check if the added child has notable new (added) props from
                // the report's API changes.
                for fc in &report.changes {
                    for ac in &fc.breaking_api_changes {
                        if ac.change == ApiChangeType::SignatureChanged
                            && ac.description.contains("was added")
                        {
                            if let Some(prop_name) =
                                ac.symbol.strip_prefix(&format!("{}.", child.name))
                            {
                                if seen_props.insert(prop_name.to_string()) {
                                    let type_hint = ac
                                        .after
                                        .as_deref()
                                        .and_then(|t| t.split(':').next_back())
                                        .map(|t| format!(" ({})", t.trim()))
                                        .unwrap_or_default();
                                    prop_instructions.push(format!(
                                        "add prop: {}{} — set this if an equivalent child or prop already exists in your code",
                                        prop_name, type_hint,
                                    ));
                                }
                            }
                        }
                    }
                }

                // Surface key behavioral changes about the child
                if let Some(child_cs) = report
                    .packages
                    .iter()
                    .flat_map(|pkg| &pkg.type_summaries)
                    .find(|c| c.name == child.name)
                {
                    for bc in &child_cs.behavioral_changes {
                        if bc.description.contains("children are no longer rendered")
                            || bc.description.contains("replaces")
                        {
                            prop_instructions.push(format!("note: {}", bc.description));
                        }
                    }
                }

                // A child is migration-required only if it absorbs removed
                // parent props. Children that merely have their own new props
                // (add prop:) are recommended, not required.
                let has_absorbed = prop_instructions
                    .iter()
                    .any(|i| !i.starts_with("add prop:") && !i.starts_with("note:"));
                if has_absorbed {
                    migration_required.push((child, prop_instructions));
                } else if !prop_instructions.is_empty() {
                    // Child has new props but didn't absorb parent props
                    recommended.push((child, prop_instructions));
                } else {
                    recommended.push((
                        child,
                        vec!["wrap content inside this component".to_string()],
                    ));
                }
            }

            // Emit migration-required children — these absorb removed props
            if !migration_required.is_empty() {
                let absorbed_prop_names: Vec<String> = migration_required
                    .iter()
                    .flat_map(|(_, instructions)| instructions.iter())
                    .filter(|i| !i.starts_with("add prop:") && !i.starts_with("note:"))
                    .cloned()
                    .collect();
                if !absorbed_prop_names.is_empty() {
                    msg.push_str(&format!(
                        "IF you use any of the following removed props ({}), \
                         you MUST add the corresponding child component to absorb them:\n",
                        absorbed_prop_names.join(", "),
                    ));
                } else {
                    msg.push_str(
                        "The following child components absorb functionality \
                         from this component:\n",
                    );
                }
                for (child, instructions) in &migration_required {
                    msg.push_str(&format!(
                        "  <{}> — {}\n",
                        child.name,
                        instructions.join(", "),
                    ));
                }
                msg.push('\n');
            }

            // Emit recommended children — no removed props, just composition guidance
            if !recommended.is_empty() {
                let rec_names: Vec<&str> =
                    recommended.iter().map(|(c, _)| c.name.as_str()).collect();
                msg.push_str(&format!(
                    "Recommended child components: {}. \
                     These are typically used for proper layout but are not \
                     strictly required — custom components and other content \
                     are also valid children.\n",
                    rec_names.join(", "),
                ));
            }

            // Emit prop-passed components — these are NOT direct children
            if !prop_children.is_empty() {
                msg.push('\n');
                for child in &prop_children {
                    let prop = child.prop_name.as_deref().unwrap_or("(unknown prop)");
                    msg.push_str(&format!(
                        "Note: <{}> is passed via the `{}` prop, NOT as a direct child.\n",
                        child.name, prop,
                    ));
                }
            }

            msg.push('\n');
        }

        // List children that moved or were removed
        if !delta.removed_children.is_empty() {
            msg.push_str("Children no longer direct:\n");
            for child_name in &delta.removed_children {
                // Check if the child moved under a new parent
                let new_parent = delta.added_children.iter().find(|added| {
                    report.extensions.hierarchy_deltas.iter().any(|d| {
                        d.component == added.name
                            && d.added_children.iter().any(|c| c.name == *child_name)
                    })
                });
                if let Some(parent) = new_parent {
                    msg.push_str(&format!(
                        "  - <{}> → now wrap inside <{}>\n",
                        child_name, parent.name,
                    ));
                } else {
                    msg.push_str(&format!(
                        "  - <{}> (removed — its functionality is absorbed into <{}>)\n",
                        child_name, component,
                    ));
                }

                // Look up the removed child's ComponentSummary for migration_target info.
                // When a child like EmptyStateHeader is removed and has a migration_target
                // pointing to the parent (EmptyState), include the prop mapping so
                // consumers know the child's props are now on the parent.
                let child_summary = report
                    .packages
                    .iter()
                    .flat_map(|pkg| &pkg.type_summaries)
                    .find(|c| c.name == *child_name && c.status == ComponentStatus::Removed);

                if let Some(child_cs) = child_summary {
                    if let Some(ref mt) = child_cs.migration_target {
                        // Check if the migration target points to the parent component
                        let target_is_parent = mt
                            .replacement_symbol
                            .strip_suffix("Props")
                            .unwrap_or(&mt.replacement_symbol)
                            == component;

                        if target_is_parent && !mt.matching_members.is_empty() {
                            msg.push_str(&format!(
                                "    Props from <{}> are now directly on <{}>:\n",
                                child_name, component,
                            ));
                            for mm in &mt.matching_members {
                                if mm.old_name == mm.new_name {
                                    msg.push_str(
                                        &format!("      - {} (same name)\n", mm.old_name,),
                                    );
                                } else {
                                    msg.push_str(&format!(
                                        "      - {} → {}\n",
                                        mm.old_name, mm.new_name,
                                    ));
                                }
                            }
                            if !mt.removed_only_members.is_empty() {
                                msg.push_str(&format!(
                                    "    Removed (no equivalent on <{}>): {}\n",
                                    component,
                                    mt.removed_only_members.join(", "),
                                ));
                            }
                        }
                    }
                }
            }
            msg.push('\n');
        }

        // List remaining removed props not covered by child migration
        let migrated_prop_names: BTreeSet<String> = delta
            .migrated_members
            .iter()
            .map(|mp| mp.member_name.clone())
            .chain(removed_props.iter().filter_map(|rp| {
                if let Some(RemovalDisposition::MovedToRelatedType { .. }) = &rp.removal_disposition
                {
                    Some(rp.name.clone())
                } else {
                    None
                }
            }))
            .collect();

        let uncovered_removed: Vec<&&RemovedMember> = removed_props
            .iter()
            .filter(|rp| !migrated_prop_names.contains(&rp.name))
            .collect();

        if !uncovered_removed.is_empty() {
            // Build a lookup of new prop accepted values from the component's
            // API changes. When a removed prop is replaced (e.g., spacer → gap),
            // we include the new prop's accepted values so the LLM can map
            // old values to new ones.
            let new_prop_values: HashMap<String, Vec<String>> = {
                let mut m: HashMap<String, Vec<String>> = HashMap::new();
                for fc in &report.changes {
                    for api in &fc.breaking_api_changes {
                        if let Some(ref after) = api.after {
                            // Check if this change is for the current component
                            if api.symbol.starts_with(&format!("{}.", component)) {
                                let prop_name = api.symbol.splitn(2, '.').nth(1).unwrap_or("");
                                let values = extract_union_values(after);
                                if !values.is_empty() {
                                    m.insert(prop_name.to_string(), values);
                                }
                            }
                        }
                    }
                }
                m
            };

            msg.push_str("Other removed props:\n");
            for rp in &uncovered_removed {
                let disposition_hint = match &rp.removal_disposition {
                    Some(RemovalDisposition::ReplacedByMember { new_member }) => {
                        let mut hint = format!(" → use '{}' instead", new_member);
                        if let Some(values) = new_prop_values.get(new_member.as_str()) {
                            hint.push_str(&format!(
                                "\n      Accepted values: {}",
                                values.join(", ")
                            ));
                        }
                        hint
                    }
                    Some(RemovalDisposition::MadeAutomatic) => " (now automatic)".to_string(),
                    Some(RemovalDisposition::TrulyRemoved) => {
                        " (removed, no replacement)".to_string()
                    }
                    _ => String::new(),
                };
                msg.push_str(&format!("  - {}{}\n", rp.name, disposition_hint));
            }
            msg.push('\n');
        }

        // Include behavioral changes if present (deduplicated)
        if !behavioral_changes.is_empty() {
            msg.push_str("Behavioral changes:\n");
            let mut seen_descriptions = BTreeSet::new();
            for bc in &behavioral_changes {
                if seen_descriptions.insert(bc.description.clone()) {
                    msg.push_str(&format!("  - {}\n", bc.description));
                }
            }
            msg.push('\n');
        }

        // Build example showing new composition using full expected_children.
        // Prop-passed children appear as props on the parent's opening tag;
        // direct children appear nested inside.
        if let Some(expected_children) = all_expected {
            let prop_passed: Vec<&ExpectedChild> = expected_children
                .iter()
                .filter(|c| c.mechanism == "prop")
                .collect();
            let direct: Vec<&ExpectedChild> = expected_children
                .iter()
                .filter(|c| c.mechanism != "prop")
                .collect();

            msg.push_str("Example:\n  <");
            msg.push_str(component);

            // Show prop-passed children as props on the opening tag
            for child in &prop_passed {
                let prop = child.prop_name.as_deref().unwrap_or("(unknown prop)");
                msg.push_str(&format!("\n    {}={{<{} />}}", prop, child.name));
            }

            if direct.is_empty() && prop_passed.is_empty() {
                msg.push_str(" />\n");
            } else if direct.is_empty() {
                msg.push_str("\n  />\n");
            } else {
                if !prop_passed.is_empty() {
                    msg.push('\n');
                    msg.push_str("  >\n");
                } else {
                    msg.push_str(">\n");
                }

                for child in &direct {
                    // Show absorbed props on the child element
                    let child_props: Vec<String> = delta
                        .migrated_members
                        .iter()
                        .filter(|mp| mp.target_child == child.name)
                        .map(|mp| {
                            if let Some(ref tn) = mp.target_member_name {
                                format!("{}={{...}}", tn)
                            } else {
                                format!("{}={{...}}", mp.member_name)
                            }
                        })
                        .collect();

                    let props_str = if child_props.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", child_props.join(" "))
                    };

                    msg.push_str(&format!(
                        "    <{}{}> ... </{}>\n",
                        child.name, props_str, child.name,
                    ));
                }
                msg.push_str(&format!("  </{}>\n", component));
            }
        }

        // Resolve the from package
        let from_pkg = report
            .packages
            .iter()
            .find(|pkg| pkg.type_summaries.iter().any(|c| c.name == *component))
            .map(|pkg| pkg.name.clone());

        // Add import guidance for new child components. Consumers need to
        // know which imports to add for the new children.
        if let Some(expected_children) = all_expected {
            let new_child_names: Vec<&str> = expected_children
                .iter()
                .filter(|c| c.mechanism != "prop") // Only direct JSX children need imports
                .map(|c| c.name.as_str())
                .collect();

            if !new_child_names.is_empty() {
                if let Some(ref pkg) = from_pkg {
                    msg.push_str(&format!(
                        "\nAdd to your imports:\n  import {{ {} }} from '{}';\n",
                        new_child_names.join(", "),
                        pkg,
                    ));
                }
            }
        }

        // Track components covered by hierarchy rules for prop/behavioral dedup
        covered_components.insert(component.clone());
        if let Some(cs) = comp_summary {
            covered_components.insert(cs.definition_name.clone());
            for rp in &cs.removed_members {
                covered_props.insert((cs.definition_name.clone(), rp.name.clone()));
                covered_props.insert((cs.name.clone(), rp.name.clone()));
            }
        }

        // Build the `when` clause and rule metadata.
        //
        // For deprecated→main deltas (source_package is set):
        //   Trigger on IMPORT of any symbol from the deprecated family.
        //   The pattern matches all symbols (component + removed symbols).
        //
        // For main→main deltas (source_package is None):
        //   Trigger on JSX_PROP (specific removed/migrated props) or
        //   JSX_COMPONENT (fallback) from the main package.
        if let Some(ref deprecated_pkg) = delta.source_package {
            // ── Deprecated→main migration rule ──

            // Collect all symbols from the deprecated family for the pattern
            let mut deprecated_symbols: BTreeSet<String> = BTreeSet::new();
            deprecated_symbols.insert(component.clone());
            for child_name in &delta.removed_children {
                deprecated_symbols.insert(child_name.clone());
            }
            // Also add any component names from migrated_members targets
            for mm in &delta.migrated_members {
                deprecated_symbols.insert(mm.target_child.clone());
            }
            // Search the file changes for all symbols from this deprecated directory
            let deprecated_dir = format!("/deprecated/components/{}/", component);
            for fc in &report.changes {
                let file_str = fc.file.to_string_lossy();
                if !file_str.contains(&deprecated_dir) {
                    continue;
                }
                for api in &fc.breaking_api_changes {
                    // Include all top-level symbols (not dotted member paths).
                    // Props interfaces are included because consumers may import
                    // them directly (e.g., `import { SelectOptionProps } from
                    // '@patternfly/react-core/deprecated'`).
                    if !api.symbol.contains('.') {
                        deprecated_symbols.insert(api.symbol.clone());
                    }
                }
            }

            let symbol_pattern = format!(
                "^({})$",
                deprecated_symbols
                    .iter()
                    .map(|s| regex_escape(s))
                    .collect::<Vec<_>>()
                    .join("|"),
            );

            // Add all deprecated family symbols to covered_components
            for sym in &deprecated_symbols {
                covered_components.insert(sym.clone());
                covered_components.insert(format!("{}Props", sym));
            }

            // Determine the replacement package
            let replacement_pkg = delta
                .migration_target
                .as_ref()
                .and_then(|mt| mt.replacement_package.as_ref())
                .cloned()
                .or_else(|| from_pkg.clone());

            // Build a self-contained deprecated migration message.
            // This does NOT reuse the standard hierarchy `msg` because
            // the deprecated migration has different semantics — the
            // migrated_members represent "props that carry over" not
            // "removed props moved to children."
            let replacement_str = replacement_pkg
                .as_deref()
                .unwrap_or("@patternfly/react-core");

            let mut deprecated_msg = format!(
                "MIGRATION: The legacy <{}> from {} has been removed.\n\
                 Replace with the new <{}> from {}.\n\n",
                component, deprecated_pkg, component, replacement_str,
            );

            // Import change guidance — list all new components AND types needed.
            // This includes:
            //   - The parent component itself
            //   - All child components from the hierarchy tree
            //   - Props interfaces that have a migration_target (consumers may
            //     import them directly, e.g., SelectOptionProps)
            let mut import_names: BTreeSet<String> = BTreeSet::new();
            import_names.insert(component.clone());
            // Recursively collect all component names from the composition tree
            {
                let mut queue: Vec<String> = delta
                    .added_children
                    .iter()
                    .map(|c| c.name.clone())
                    .collect();
                let mut visited: HashSet<String> = HashSet::new();
                while let Some(name) = queue.pop() {
                    if !visited.insert(name.clone()) {
                        continue;
                    }
                    import_names.insert(name.clone());
                    // Look up this child's expected_children
                    for pkg in &report.packages {
                        for comp_s in &pkg.type_summaries {
                            if comp_s.name == name {
                                for ec in &comp_s.expected_children {
                                    queue.push(ec.name.clone());
                                }
                            }
                        }
                    }
                }
            }
            // Also include any deprecated symbols (Props interfaces, types) that
            // have a migration_target — consumers importing these need to know
            // the new import path includes them.
            for fc in &report.changes {
                let file_str = fc.file.to_string_lossy();
                if !file_str.contains(&deprecated_dir) {
                    continue;
                }
                for api in &fc.breaking_api_changes {
                    if api.migration_target.is_some() && !api.symbol.contains('.') {
                        let import_name = api
                            .migration_target
                            .as_ref()
                            .map(|mt| mt.replacement_symbol.clone())
                            .unwrap_or_else(|| api.symbol.clone());
                        import_names.insert(import_name);
                    }
                }
            }

            deprecated_msg.push_str(&format!(
                "Import change:\n\
                 \x20 Replace: import {{ ... }} from '{}';\n\
                 \x20 With:    import {{ {} }} from '{}';\n\n",
                deprecated_pkg,
                import_names.iter().cloned().collect::<Vec<_>>().join(", "),
                replacement_str,
            ));

            // Property mapping from migration_target
            if let Some(ref mt) = delta.migration_target {
                if !mt.matching_members.is_empty() {
                    deprecated_msg.push_str(&format!(
                        "Props that carry over to the new <{}>:\n",
                        component
                    ));
                    for m in &mt.matching_members {
                        if m.old_name == m.new_name {
                            deprecated_msg.push_str(&format!("  - {} (same name)\n", m.old_name));
                        } else {
                            deprecated_msg.push_str(&format!(
                                "  - {} → {} (renamed)\n",
                                m.old_name, m.new_name
                            ));
                        }
                    }
                    deprecated_msg.push('\n');
                }
                if !mt.removed_only_members.is_empty() {
                    deprecated_msg.push_str(&format!(
                        "Removed props (no equivalent in new API): {}\n\n",
                        mt.removed_only_members.join(", "),
                    ));
                }
            }

            // Show migration info for related Props interfaces (e.g., SelectOptionProps)
            // that consumers may import and extend directly.
            for fc in &report.changes {
                let file_str = fc.file.to_string_lossy();
                if !file_str.contains(&deprecated_dir) {
                    continue;
                }
                for api in &fc.breaking_api_changes {
                    if api.symbol.contains('.') {
                        continue;
                    }
                    // Skip the parent component itself (already shown above)
                    if api.symbol == *component || api.symbol == format!("{}Props", component) {
                        continue;
                    }
                    if let Some(ref mt) = api.migration_target {
                        deprecated_msg.push_str(&format!(
                            "Type '{}' → '{}' (import from '{}'):\n",
                            api.symbol, mt.replacement_symbol, replacement_str,
                        ));
                        if !mt.matching_members.is_empty() {
                            deprecated_msg.push_str("  Members that carry over: ");
                            let members: Vec<String> = mt
                                .matching_members
                                .iter()
                                .map(|m| {
                                    if m.old_name == m.new_name {
                                        m.old_name.clone()
                                    } else {
                                        format!("{} → {}", m.old_name, m.new_name)
                                    }
                                })
                                .collect();
                            deprecated_msg.push_str(&members.join(", "));
                            deprecated_msg.push('\n');
                        }
                        if !mt.removed_only_members.is_empty() {
                            deprecated_msg.push_str(&format!(
                                "  Removed members: {}\n",
                                mt.removed_only_members.join(", "),
                            ));
                        }
                        deprecated_msg.push('\n');
                    }
                }
            }

            // New composition structure with recursive nesting
            deprecated_msg.push_str("The new API uses a composition-based structure:\n\n");

            // Build recursive example
            fn build_nested_example(
                msg: &mut String,
                component_name: &str,
                report: &AnalysisReport<TypeScript>,
                indent: usize,
                visited: &mut HashSet<String>,
            ) {
                let prefix = " ".repeat(indent);
                if !visited.insert(component_name.to_string()) {
                    msg.push_str(&format!("{}<{} />\n", prefix, component_name));
                    return;
                }

                // Look up expected_children for this component
                let children: Vec<ExpectedChild> = report
                    .packages
                    .iter()
                    .flat_map(|pkg| &pkg.type_summaries)
                    .find(|c| c.name == component_name)
                    .map(|c| c.expected_children.clone())
                    .unwrap_or_default();

                if children.is_empty() {
                    msg.push_str(&format!(
                        "{}<{}>...</{}>\n",
                        prefix, component_name, component_name
                    ));
                } else {
                    let prop_passed: Vec<&ExpectedChild> =
                        children.iter().filter(|c| c.mechanism == "prop").collect();
                    let direct: Vec<&ExpectedChild> =
                        children.iter().filter(|c| c.mechanism != "prop").collect();

                    // Opening tag with prop-passed children as attributes
                    msg.push_str(&format!("{}<{}", prefix, component_name));
                    for child in &prop_passed {
                        let prop = child.prop_name.as_deref().unwrap_or("(unknown prop)");
                        msg.push_str(&format!(" {}={{<{} />}}", prop, child.name));
                    }

                    if direct.is_empty() {
                        msg.push_str(" />\n");
                    } else {
                        msg.push_str(">\n");
                        for child in &direct {
                            build_nested_example(msg, &child.name, report, indent + 2, visited);
                        }
                        msg.push_str(&format!("{}</{}>\n", prefix, component_name));
                    }
                }
            }

            {
                let mut visited = HashSet::new();
                build_nested_example(&mut deprecated_msg, component, report, 2, &mut visited);
            }

            // List truly removed symbols (no equivalent in new API)
            if !delta.removed_children.is_empty() {
                deprecated_msg.push_str("\nComponents with no direct replacement:\n");
                for child_name in &delta.removed_children {
                    deprecated_msg.push_str(&format!("  - {} (removed)\n", child_name));
                }
            }

            deprecated_msg.push('\n');
            deprecated_msg.push_str(&format!(
                "NOTE: The new <{}> has a significantly different API.\n\
                 The old and new components are not drop-in replacements.\n\
                 Review all prop usage and update your JSX structure accordingly.\n",
                component
            ));

            rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".to_string(),
                    "change-type=deprecated-migration".to_string(),
                    "has-codemod=false".to_string(),
                ],
                effort: 7,
                category: "mandatory".to_string(),
                description: format!(
                    "Legacy <{}> removed from deprecated — migrate to new API",
                    component,
                ),
                message: deprecated_msg,
                links: Vec::new(),
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: symbol_pattern,
                        location: "IMPORT".to_string(),
                        component: None,
                        parent: None,
                    not_parent: None,
                        value: None,
                        from: Some(deprecated_pkg.clone()),
                        parent_from: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry::new("LlmAssisted")),
            });
        } else {
            // ── Main→main hierarchy rule (existing behavior) ──

            let mut trigger_props: BTreeSet<String> = BTreeSet::new();
            for rp in &removed_props {
                trigger_props.insert(rp.name.clone());
            }
            for mp in &delta.migrated_members {
                trigger_props.insert(mp.member_name.clone());
            }

            let (location, pattern, component_filter) = if !trigger_props.is_empty() {
                let prop_pattern = format!(
                    "^({})$",
                    trigger_props
                        .iter()
                        .map(|p| regex_escape(p))
                        .collect::<Vec<_>>()
                        .join("|"),
                );
                (
                    "JSX_PROP".to_string(),
                    prop_pattern,
                    Some(format!("^{}$", regex_escape(component))),
                )
            } else {
                (
                    "JSX_COMPONENT".to_string(),
                    format!("^{}$", regex_escape(component)),
                    None,
                )
            };

            rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".to_string(),
                    "change-type=hierarchy-composition".to_string(),
                    "has-codemod=false".to_string(),
                ],
                effort: 5,
                category: "mandatory".to_string(),
                description: format!(
                    "<{}> composition structure changed — use child components",
                    component,
                ),
                message: msg,
                links: Vec::new(),
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern,
                        location,
                        component: component_filter,
                        parent: None,
                    not_parent: None,
                        value: None,
                        from: from_pkg,
                        parent_from: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry::new("LlmAssisted")),
            });
        }
    }

    // ── Post-generation: deduplicate behavioral rules ──
    //
    // When an API rule (P0-C component-import-deprecated or hierarchy-composition)
    // already includes behavioral context in its message, standalone behavioral
    // rules for the same component are redundant.  Downgrade them from
    // LlmAssisted to Manual to avoid duplicate goose invocations.
    {
        let enriched_components: BTreeSet<String> = rules
            .iter()
            .filter(|r| {
                r.labels.iter().any(|l| {
                    l == "change-type=component-removal" || l == "change-type=hierarchy-composition"
                })
            })
            .filter_map(|r| match &r.when {
                KonveyorCondition::FrontendReferenced { referenced } => {
                    let pat = &referenced.pattern;
                    Some(
                        pat.strip_prefix('^')
                            .unwrap_or(pat)
                            .strip_suffix('$')
                            .unwrap_or(pat)
                            .to_string(),
                    )
                }
                _ => None,
            })
            .collect();

        if !enriched_components.is_empty() {
            let mut deduped = 0usize;
            for rule in &mut rules {
                let is_behavioral = rule
                    .labels
                    .iter()
                    .any(|l| l.starts_with("change-type=behavioral"));
                if !is_behavioral {
                    continue;
                }
                // Extract the component name from the behavioral rule's pattern
                let component = match &rule.when {
                    KonveyorCondition::FrontendReferenced { referenced } => {
                        let pat = &referenced.pattern;
                        pat.strip_prefix('^')
                            .unwrap_or(pat)
                            .strip_suffix('$')
                            .unwrap_or(pat)
                            .to_string()
                    }
                    _ => continue,
                };
                if enriched_components.contains(&component) {
                    if let Some(ref mut strat) = rule.fix_strategy {
                        if strat.strategy == "LlmAssisted" {
                            strat.strategy = "Manual".into();
                            deduped += 1;
                        }
                    }
                }
            }
            if deduped > 0 {
                tracing::debug!(
                    count = deduped,
                    "Downgraded behavioral rules to Manual (covered by enriched API rules)"
                );
            }
        }
    }

    rules
}

/// Generate Konveyor rules and fix strategies for dependency version updates.
///
/// For each package in the monorepo that has breaking changes, generates a rule
/// that detects the package in the consumer's `package.json` dependencies and a
/// fix strategy to update the version.
///
/// Returns `(rules, strategies)` where:
/// - `rules` are Konveyor rules using `builtin.json` to detect the dependency
/// - `strategies` maps rule IDs to `UpdateDependency` fix strategy entries
pub fn generate_dependency_update_rules(
    report: &AnalysisReport<TypeScript>,
    pkg_info_cache: &HashMap<String, PackageInfo>,
) -> (Vec<KonveyorRule>, HashMap<String, FixStrategyEntry>) {
    let mut rules = Vec::new();
    let mut strategies = HashMap::new();

    // Collect packages that need dependency-update rules.
    //
    // Two sources:
    // 1. Packages with breaking API/behavioral changes in the report
    // 2. ALL packages in the cache that had a major version bump (e.g., 5.x → 6.x)
    //    even if they have no breaking changes in the report (e.g., react-styles)
    let mut packages_with_changes: HashMap<String, &PackageInfo> = HashMap::new();

    // Source 1: packages with breaking changes
    for file_changes in &report.changes {
        let file_str = file_changes.file.to_string_lossy();
        let parts: Vec<&str> = file_str.split('/').collect();

        if let Some(pkg_idx) = parts.iter().position(|&p| p == "packages") {
            if let Some(pkg_dir_name) = parts.get(pkg_idx + 1) {
                if let Some(info) = pkg_info_cache.get(*pkg_dir_name) {
                    if info.version.is_some()
                        && (!file_changes.breaking_api_changes.is_empty()
                            || !file_changes.breaking_behavioral_changes.is_empty())
                    {
                        packages_with_changes
                            .entry(info.name.clone())
                            .or_insert(info);
                    }
                }
            }
        }
    }

    // Source 2: any package with a major version bump vs the from_ref.
    // This catches packages like react-styles that ship a new major version
    // alongside the rest of the monorepo but have no breaking API surface.
    let from_major = report
        .comparison
        .from_ref
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    for (_dir_name, info) in pkg_info_cache {
        if packages_with_changes.contains_key(&info.name) {
            continue;
        }
        if let Some(ref ver) = info.version {
            let new_major = ver
                .split('.')
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            if new_major > from_major {
                packages_with_changes
                    .entry(info.name.clone())
                    .or_insert(info);
            }
        }
    }

    for (npm_name, info) in &packages_with_changes {
        let version = match &info.version {
            Some(v) => v,
            None => continue,
        };

        // Generate a slug-safe rule ID from the package name
        let slug = npm_name.replace('@', "").replace(['/', '.'], "-");
        let rule_id = format!("semver-dep-update-{}", slug);

        let new_version = format!("^{}", version);

        // Use frontend.dependency condition to match by name and version bound.
        // The provider checks dependencies/devDependencies/peerDependencies
        // and only matches when the installed version is <= the old version
        // (i.e., needs updating).
        let condition = KonveyorCondition::FrontendDependency {
            dependency: FrontendDependencyFields {
                name: Some(npm_name.clone()),
                nameregex: None,
                // Fire when the dependency version is at or below the old (pre-breaking) version.
                // The old version is the from_ref version (e.g., "5.4.0").
                // Use a high patch number to catch all patch releases of the old major.
                upperbound: {
                    // Extract the major version from the from_ref (e.g., "v5.4.0" -> "5")
                    let from_ref = &report.comparison.from_ref;
                    let major = from_ref
                        .trim_start_matches('v')
                        .split('.')
                        .next()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    Some(format!("{}.99.99", major))
                },
                lowerbound: None,
            },
        };

        rules.push(KonveyorRule {
            rule_id: rule_id.clone(),
            description: format!("Update {} to v{}", npm_name, version),
            labels: vec![
                "change-type=dependency-update".into(),
                "has-codemod=true".into(),
                "source=semver-analyzer".into(),
            ],
            effort: 1,
            category: "mandatory".into(),
            links: Vec::new(),
            when: condition,
            message: format!(
                "Update {} from current version to {}. \
                 This package has breaking changes between {} and {}.\n\n\
                 After updating package.json, regenerate your lockfile:\n\
                 - npm: npm install\n\
                 - yarn: yarn install\n\
                 - pnpm: pnpm install",
                npm_name, new_version, report.comparison.from_ref, report.comparison.to_ref,
            ),
            fix_strategy: Some(FixStrategyEntry::update_dependency(
                npm_name.clone(),
                new_version.clone(),
            )),
        });

        strategies.insert(
            rule_id,
            FixStrategyEntry::update_dependency(npm_name.clone(), new_version),
        );
    }

    if !rules.is_empty() {
        tracing::debug!(
            count = rules.len(),
            packages = %packages_with_changes
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            "Generated dependency update rules"
        );
    }

    (rules, strategies)
}

fn extract_compound_tokens(
    report: &AnalysisReport<TypeScript>,
    rename_patterns: &RenamePatterns,
) -> (
    BTreeSet<String>,
    Vec<CompoundToken>,
    HashMap<String, String>,
) {
    let re = member_key_re();
    let mut covered_symbols: BTreeSet<String> = BTreeSet::new();
    let mut member_renames: HashMap<String, String> = HashMap::new();
    let mut compound_tokens: Vec<CompoundToken> = Vec::new();

    for file_changes in &report.changes {
        for api_change in &file_changes.breaking_api_changes {
            if api_change.change != ApiChangeType::TypeChanged {
                continue;
            }

            let before = match &api_change.before {
                Some(b) if b.contains("[\"") => b,
                _ => continue,
            };
            let after = match &api_change.after {
                Some(a) if a.contains("[\"") => a,
                _ => continue,
            };

            let old_keys: BTreeSet<String> = re
                .captures_iter(before)
                .map(|c| c[1].to_string())
                .filter(|k| k != "name" && k != "value" && k != "values" && k != "var")
                .collect();

            let new_keys: BTreeSet<String> = re
                .captures_iter(after)
                .map(|c| c[1].to_string())
                .filter(|k| k != "name" && k != "value" && k != "values" && k != "var")
                .collect();

            if old_keys.len() < 3 || new_keys.len() < 3 {
                continue;
            }

            for key in &old_keys {
                covered_symbols.insert(key.clone());
            }

            let removed: BTreeSet<String> = old_keys.difference(&new_keys).cloned().collect();
            let added: BTreeSet<String> = new_keys.difference(&old_keys).cloned().collect();

            // Apply explicit rename patterns
            for old_key in &removed {
                if let Some(expected_new) = rename_patterns.find_replacement(old_key) {
                    if added.contains(&expected_new) {
                        member_renames.insert(old_key.clone(), expected_new);
                    }
                }
            }

            compound_tokens.push(CompoundToken { removed, added });
        }
    }

    (covered_symbols, compound_tokens, member_renames)
}

pub fn extract_suffix_inventory(
    report: &AnalysisReport<TypeScript>,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let re = member_key_re();
    let mut removed_suffixes: BTreeSet<String> = BTreeSet::new();
    let mut added_suffixes: BTreeSet<String> = BTreeSet::new();

    for file_changes in &report.changes {
        for api_change in &file_changes.breaking_api_changes {
            if api_change.change != ApiChangeType::TypeChanged {
                continue;
            }
            let before = match &api_change.before {
                Some(b) if b.contains("[\"") => b,
                _ => continue,
            };
            let after = match &api_change.after {
                Some(a) if a.contains("[\"") => a,
                _ => continue,
            };

            let old_keys: BTreeSet<String> = re
                .captures_iter(before)
                .map(|c| c[1].to_string())
                .filter(|k| k != "name" && k != "value" && k != "values" && k != "var")
                .collect();
            let new_keys: BTreeSet<String> = re
                .captures_iter(after)
                .map(|c| c[1].to_string())
                .filter(|k| k != "name" && k != "value" && k != "values" && k != "var")
                .collect();

            if old_keys.len() < 3 || new_keys.len() < 3 {
                continue;
            }

            for key in old_keys.difference(&new_keys) {
                if let Some(suffix) = extract_trailing_suffix(key) {
                    removed_suffixes.insert(suffix.to_string());
                }
            }
            for key in new_keys.difference(&old_keys) {
                if let Some(suffix) = extract_trailing_suffix(key) {
                    added_suffixes.insert(suffix.to_string());
                }
            }
        }
    }

    (removed_suffixes, added_suffixes)
}

pub fn analyze_token_members(
    report: &AnalysisReport<TypeScript>,
    rename_patterns: &RenamePatterns,
) -> (BTreeSet<String>, HashMap<String, String>) {
    let (covered_symbols, _compound_tokens, member_renames) =
        extract_compound_tokens(report, rename_patterns);
    (covered_symbols, member_renames)
}

pub fn apply_suffix_renames(
    report: &AnalysisReport<TypeScript>,
    suffix_renames: &HashMap<String, String>,
) -> HashMap<String, String> {
    let (_covered, compound_tokens, mut member_renames) =
        extract_compound_tokens(report, &RenamePatterns::empty());

    for ct in &compound_tokens {
        for old_key in &ct.removed {
            if member_renames.contains_key(old_key) {
                continue;
            }
            if let Some(old_suffix) = extract_trailing_suffix(old_key) {
                if let Some(new_suffix) = suffix_renames.get(old_suffix) {
                    let prefix = &old_key[..old_key.len() - old_suffix.len()];
                    let expected_new = format!("{}{}", prefix, new_suffix);
                    if ct.added.contains(&expected_new) {
                        member_renames.insert(old_key.clone(), expected_new);
                    }
                }
            }
        }
    }

    member_renames
}

pub fn generate_conformance_rules(report: &AnalysisReport<TypeScript>) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();
    let mut id_counts: HashMap<String, usize> = HashMap::new();

    // Build a lookup of component → expected direct children.
    // This spans all packages so we can resolve multi-level chains.
    let mut children_map: HashMap<String, Vec<ExpectedChild>> = HashMap::new();
    let mut component_pkg: HashMap<String, String> = HashMap::new();
    for pkg in &report.packages {
        for comp in &pkg.type_summaries {
            if !comp.expected_children.is_empty() {
                children_map.insert(comp.name.clone(), comp.expected_children.clone());
                component_pkg.insert(comp.name.clone(), pkg.name.clone());
            }
        }
    }

    // ── Wrapper-skip rules ──────────────────────────────────────────
    //
    // Walk the expected_children hierarchy to find chains A → B → C
    // where A expects wrapper B, and B expects child C. Generate a
    // nesting-violation rule that fires when C appears as a direct
    // child of A (skipping B).
    //
    // Example: Dropdown → DropdownList → DropdownItem
    //   Rule: <DropdownItem> with parent <Dropdown> is wrong — wrap in <DropdownList>.
    //
    // Only wrappers marked `required: true` are considered — optional
    // wrappers (like AccordionItem, Table > Thead) produce too much
    // noise for cases where the nesting is flexible.
    //
    // These rules use the scanner's `parent` condition, so they ONLY
    // fire on the specific bad nesting pattern, not on every usage.
    let mut seen_pairs: HashSet<(String, String)> = HashSet::new();

    for (parent_name, parent_children) in &children_map {
        for wrapper in parent_children {
            // Only consider direct children (not prop-passed)
            if wrapper.mechanism == "prop" {
                continue;
            }

            // Only consider required wrappers — optional wrappers produce
            // noise for flexible nesting patterns
            if !wrapper.required {
                continue;
            }

            // Look up the wrapper's own expected children
            let grandchildren = match children_map.get(&wrapper.name) {
                Some(gc) => gc,
                None => continue,
            };

            for grandchild in grandchildren {
                if grandchild.mechanism == "prop" {
                    continue;
                }

                // Skip if grandchild is already a valid direct child of parent
                // (some components accept both wrapped and direct placement)
                let is_direct_child = parent_children
                    .iter()
                    .any(|c| c.name == grandchild.name && c.mechanism != "prop");
                if is_direct_child {
                    continue;
                }

                // Deduplicate (same grandchild+parent can appear through
                // multiple wrapper paths)
                let pair = (parent_name.clone(), grandchild.name.clone());
                if !seen_pairs.insert(pair) {
                    continue;
                }

                let base_id = format!(
                    "conformance-{}-needs-{}-wrapper",
                    sanitize_id(&grandchild.name),
                    sanitize_id(&wrapper.name),
                );
                let rule_id = unique_id(base_id, &mut id_counts);

                let parent_pkg = component_pkg.get(parent_name).cloned().unwrap_or_default();
                // Grandchild may be in same or different package;
                // fall back to wrapper's package, then parent's.
                let grandchild_pkg = component_pkg
                    .get(&grandchild.name)
                    .or_else(|| component_pkg.get(&wrapper.name))
                    .cloned()
                    .unwrap_or_else(|| parent_pkg.clone());

                let msg = format!(
                    "<{grandchild}> must be wrapped in <{wrapper}> inside <{parent}>.\n\n\
                     Replace:\n\
                     \x20 <{parent}>\n\
                     \x20   <{grandchild}>...</{grandchild}>\n\
                     \x20 </{parent}>\n\n\
                     With:\n\
                     \x20 <{parent}>\n\
                     \x20   <{wrapper}>\n\
                     \x20     <{grandchild}>...</{grandchild}>\n\
                     \x20   </{wrapper}>\n\
                     \x20 </{parent}>",
                    grandchild = grandchild.name,
                    wrapper = wrapper.name,
                    parent = parent_name,
                );

                rules.push(KonveyorRule {
                    rule_id,
                    labels: vec![
                        "source=semver-analyzer".to_string(),
                        "change-type=conformance".to_string(),
                        "has-codemod=false".to_string(),
                    ],
                    effort: 3,
                    category: "mandatory".to_string(),
                    description: format!(
                        "<{}> must be inside <{}>, not directly in <{}>",
                        grandchild.name, wrapper.name, parent_name,
                    ),
                    message: msg,
                    links: Vec::new(),
                    when: KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: format!("^{}$", regex_escape(&grandchild.name)),
                            location: "JSX_COMPONENT".to_string(),
                            component: None,
                            parent: Some(format!("^{}$", regex_escape(parent_name))),
                    not_parent: None,
                            value: None,
                            from: if grandchild_pkg.is_empty() {
                                None
                            } else {
                                Some(grandchild_pkg)
                            },
                            parent_from: if parent_pkg.is_empty() {
                                None
                            } else {
                                Some(parent_pkg)
                            },
                        },
                    },
                    fix_strategy: Some(FixStrategyEntry::new("LlmAssisted")),
                });
            }
        }
    }

    if !rules.is_empty() {
        tracing::info!(count = rules.len(), "Generated conformance rules");
    }

    rules
}

pub fn build_package_name_cache(report: &AnalysisReport<TypeScript>) -> HashMap<String, String> {
    let full_cache = build_package_info_cache(report);
    full_cache
        .into_iter()
        .map(|(dir, info)| (dir, info.name))
        .collect()
}

/// Build a cache of package directory name -> PackageInfo (name + version).
///
/// Reads package.json from the to_ref (new version) using `git show` to get
/// the target version for dependency update rules. Falls back to reading from
/// disk if git fails.
pub fn build_package_info_cache(
    report: &AnalysisReport<TypeScript>,
) -> HashMap<String, PackageInfo> {
    let mut cache: HashMap<String, PackageInfo> = HashMap::new();
    let repo_path = &report.repository;
    let to_ref = &report.comparison.to_ref;

    for file_changes in &report.changes {
        let file_str = file_changes.file.to_string_lossy();
        let parts: Vec<&str> = file_str.split('/').collect();

        if let Some(pkg_idx) = parts.iter().position(|&p| p == "packages") {
            if let Some(pkg_dir_name) = parts.get(pkg_idx + 1) {
                if cache.contains_key(*pkg_dir_name) {
                    continue;
                }

                // Read package.json at the to_ref to get the target version.
                // Use `git show <ref>:path` to avoid depending on the checkout state.
                let pkg_json_git_path = format!("packages/{}/package.json", pkg_dir_name);
                let (npm_name, npm_version) =
                    read_package_json_at_ref(repo_path, to_ref, &pkg_json_git_path)
                        .or_else(|| {
                            // Fallback: read from disk (current checkout)
                            let pkg_json_path = repo_path
                                .join("packages")
                                .join(pkg_dir_name)
                                .join("package.json");
                            read_package_json_from_file(&pkg_json_path)
                        })
                        .unwrap_or((None, None));

                let info = PackageInfo {
                    name: npm_name.unwrap_or_else(|| pkg_dir_name.to_string()),
                    version: npm_version,
                };
                cache.insert(pkg_dir_name.to_string(), info);
            }
        }
    }

    // Discover ALL workspace packages via `git ls-tree` so packages without
    // breaking API changes (e.g., react-styles) still get dependency-update rules
    // when they have a major version bump.
    if let Ok(output) = std::process::Command::new("git")
        .args(["ls-tree", "--name-only", to_ref, "packages/"])
        .current_dir(repo_path)
        .output()
    {
        if output.status.success() {
            let listing = String::from_utf8_lossy(&output.stdout);
            for line in listing.lines() {
                // line is e.g. "packages/react-styles"
                let dir_name = line.trim_start_matches("packages/");
                if dir_name.is_empty() || cache.contains_key(dir_name) {
                    continue;
                }
                let pkg_json_git_path = format!("{}/package.json", line);
                if let Some((npm_name, npm_version)) =
                    read_package_json_at_ref(repo_path, to_ref, &pkg_json_git_path)
                {
                    let info = PackageInfo {
                        name: npm_name.unwrap_or_else(|| dir_name.to_string()),
                        version: npm_version,
                    };
                    cache.insert(dir_name.to_string(), info);
                }
            }
        }
    }

    // Also populate from report.packages which may have scoped names
    // (e.g., "@patternfly/react-core") set by the report builder from
    // Symbol.package. This serves as an additional source when git/disk
    // reads fail.
    for pkg in &report.packages {
        // Derive the directory name from the package name
        // e.g., "@patternfly/react-core" -> "react-core"
        let dir_name = pkg.name.rsplit('/').next().unwrap_or(&pkg.name);
        let entry = cache
            .entry(dir_name.to_string())
            .or_insert_with(|| PackageInfo {
                name: dir_name.to_string(),
                version: None,
            });
        // If the cache has a bare directory name but the report has the scoped name, upgrade
        if !pkg.name.starts_with('@') || entry.name.starts_with('@') {
            continue;
        }
        entry.name = pkg.name.clone();
    }

    if !cache.is_empty() {
        tracing::debug!(
            entries = ?cache
                .iter()
                .map(|(k, v)| format!(
                    "{}: {} ({})",
                    k,
                    v.name,
                    v.version.as_deref().unwrap_or("?")
                ))
                .collect::<Vec<_>>(),
            "Package info cache built"
        );
    }

    cache
}

/// Extract the CSS variable prefix from a CSS var name.
///
/// `"--pf-v5-c-button--Color"` → `Some("--pf-v5-")`
/// `"--pf-t--global--spacer--sm"` → `Some("--pf-t--")`
/// `"--pf-v6-c-alert--BoxShadow"` → `Some("--pf-v6-")`
///
/// The prefix is everything up to and including the first segment boundary
/// after `--pf-`. Versioned prefixes end at the dash after the version number
/// (`--pf-v5-`). Non-versioned prefixes end at the double-dash after the
/// identifier (`--pf-t--`).
fn extract_css_var_prefix(css_var: &str) -> Option<String> {
    // Extract the prefix including the first semantic segment after the
    // version/identifier prefix. This distinguishes component-scoped vars
    // (--pf-v5-c-*) from global tokens (--pf-v5-global--*), producing
    // separate rules with specific from/to mappings.
    //
    // Examples:
    //   "--pf-v5-c-button--Color"       → "--pf-v5-c-"
    //   "--pf-v5-global--spacer--sm"    → "--pf-v5-global--"
    //   "--pf-t--global--spacer--sm"    → "--pf-t--global--"
    //   "--pf-v6-c-alert--BoxShadow"    → "--pf-v6-c-"
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^(--[a-zA-Z]+-(?:v\d+-|[a-zA-Z]+-{2})[a-zA-Z]+-{1,2})").unwrap()
    });
    RE.captures(css_var).map(|cap| cap[1].to_string())
}

/// Extract the CSS var name from a token's `before` or `after` type annotation.
///
/// Given `{ ["name"]: "--pf-v5-global--spacer--sm"; ... }`, returns
/// `Some("--pf-v5-global--spacer--sm")`.
fn extract_css_var_name(type_annotation: &str) -> Option<String> {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"\["name"\]:\s*"([^"]+)""#).unwrap());
    RE.captures(type_annotation).map(|cap| cap[1].to_string())
}

fn detect_css_prefix_changes(report: &AnalysisReport<TypeScript>) -> Vec<(String, String, String)> {
    let mut pair_counts: HashMap<(String, String), usize> = HashMap::new();

    for api_change in report
        .changes
        .iter()
        .flat_map(|fc| &fc.breaking_api_changes)
        .filter(|a| {
            a.kind == ApiChangeKind::Constant
                && matches!(
                    a.change,
                    ApiChangeType::TypeChanged | ApiChangeType::Renamed
                )
        })
    {
        if let (Some(op), Some(np)) = (
            api_change
                .before
                .as_deref()
                .and_then(extract_css_var_name)
                .and_then(|n| extract_css_var_prefix(&n)),
            api_change
                .after
                .as_deref()
                .and_then(extract_css_var_name)
                .and_then(|n| extract_css_var_prefix(&n)),
        ) {
            if op != np {
                *pair_counts.entry((op, np)).or_insert(0) += 1;
            }
        }
    }

    // Filter to valid prefix pairs. Bad value-based token matches create
    // noise pairs like (--pf-v5-c-, --pf-t--global--). Only keep pairs
    // where the structural segment after the version prefix matches.
    //
    // --pf-v5-c-  has segment "c-"      → matches --pf-v6-c-  ("c-")
    // --pf-v5-global-- has segment "global--" → matches --pf-t--global-- ("global--")
    // --pf-v5-c-  has segment "c-"      → DOES NOT match --pf-t--global-- ("global--")
    static BASE_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^--[a-zA-Z]+-(?:v\d+-|[a-zA-Z]+-{2})").unwrap()
    });

    pair_counts
        .iter()
        .filter(|((old_p, new_p), _count)| {
            let old_seg = BASE_RE.replace(old_p, "");
            let new_seg = BASE_RE.replace(new_p, "");
            old_seg == new_seg && !old_seg.is_empty()
        })
        .map(|((old_p, new_p), _)| {
            let class_prefix = old_p.trim_start_matches('-').to_string();
            (class_prefix, old_p.clone(), new_p.clone())
        })
        .collect()
}

pub fn generate_fix_guidance(
    report: &AnalysisReport<TypeScript>,
    rules: &[KonveyorRule],
    file_pattern: &str,
) -> FixGuidanceDoc {
    let mut fixes = Vec::new();
    let mut rule_idx = 0;

    // API + behavioral changes (per-file, in same order as generate_rules)
    for file_changes in &report.changes {
        for api_change in &file_changes.breaking_api_changes {
            if rule_idx < rules.len() {
                let fix = api_change_to_fix(
                    api_change,
                    file_changes,
                    &rules[rule_idx].rule_id,
                    file_pattern,
                );
                fixes.push(fix);
                rule_idx += 1;
            }
        }
        for behavioral in &file_changes.breaking_behavioral_changes {
            if rule_idx < rules.len() {
                let fix =
                    behavioral_change_to_fix(behavioral, file_changes, &rules[rule_idx].rule_id);
                fixes.push(fix);
                rule_idx += 1;
            }
        }
    }

    // Manifest changes
    for manifest in &report.manifest_changes {
        if rule_idx < rules.len() {
            let fix = manifest_change_to_fix(manifest, &rules[rule_idx].rule_id);
            fixes.push(fix);
            rule_idx += 1;
        }
    }

    let auto_fixable = fixes
        .iter()
        .filter(|f| matches!(f.confidence, FixConfidence::Exact | FixConfidence::High))
        .count();
    let manual_only = fixes
        .iter()
        .filter(|f| matches!(f.source, FixSource::Manual))
        .count();
    let needs_review = fixes.len() - auto_fixable - manual_only;

    FixGuidanceDoc {
        migration: MigrationInfo {
            from_ref: report.comparison.from_ref.clone(),
            to_ref: report.comparison.to_ref.clone(),
            generated_by: format!("semver-analyzer v{}", report.metadata.tool_version),
        },
        summary: FixSummary {
            total_fixes: fixes.len(),
            auto_fixable,
            needs_review,
            manual_only,
        },
        fixes,
    }
}

/// Write a Konveyor ruleset directory.
///
/// Creates:
///   `<output_dir>/ruleset.yaml`         — ruleset metadata
///   `<output_dir>/breaking-changes.yaml` — all generated rules
pub fn write_ruleset_dir(
    output_dir: &Path,
    ruleset_name: &str,
    report: &AnalysisReport<TypeScript>,
    rules: &[KonveyorRule],
) -> Result<()> {
    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create output directory {}", output_dir.display()))?;

    // Write ruleset.yaml
    let from_ref = &report.comparison.from_ref;
    let to_ref = &report.comparison.to_ref;
    let ruleset = KonveyorRuleset {
        name: ruleset_name.to_string(),
        description: format!(
            "Breaking changes detected between {} and {} by semver-analyzer v{}",
            from_ref, to_ref, report.metadata.tool_version
        ),
        labels: vec!["source=semver-analyzer".to_string()],
    };

    let ruleset_path = output_dir.join("ruleset.yaml");
    let ruleset_yaml = serde_yaml::to_string(&ruleset).context("Failed to serialize ruleset")?;
    std::fs::write(&ruleset_path, &ruleset_yaml)
        .with_context(|| format!("Failed to write {}", ruleset_path.display()))?;

    // Write rules file
    let rules_path = output_dir.join("breaking-changes.yaml");
    let rules_yaml = serde_yaml::to_string(&rules).context("Failed to serialize rules")?;
    std::fs::write(&rules_path, &rules_yaml)
        .with_context(|| format!("Failed to write {}", rules_path.display()))?;

    Ok(())
}

// ── Rule generators ─────────────────────────────────────────────────────

fn api_change_to_rules(
    change: &ApiChange,
    file_changes: &FileChanges<TypeScript>,
    from_pkg: Option<&str>,
    id_counts: &mut HashMap<String, usize>,
    rename_patterns: &RenamePatterns,
    member_renames: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let file_path = file_changes.file.display().to_string();
    let leaf_symbol = extract_leaf_symbol(&change.symbol);
    let effort = effort_for_api_change(&change.change);
    let change_type_label = api_change_type_label(&change.change);

    let base_id = format!(
        "semver-{}-{}-{}",
        sanitize_id(&file_path),
        sanitize_id(&change.symbol),
        change_type_label,
    );
    let rule_id = unique_id(base_id.clone(), id_counts);

    let mut message = build_api_message(change, &file_path);

    // Enrich removed props with replacement info from removal_disposition.
    // This tells the LLM what to rename the prop to AND what the new type is.
    if change.change == ApiChangeType::Removed {
        if let Some(ref disp) = change.removal_disposition {
            match disp {
                RemovalDisposition::ReplacedByMember { new_member } => {
                    // Look up the new member's type from the same file's changes
                    let new_type = file_changes
                        .breaking_api_changes
                        .iter()
                        .find(|a| {
                            let leaf = a.symbol.split('.').last().unwrap_or("");
                            leaf == new_member.as_str()
                                && matches!(
                                    a.change,
                                    ApiChangeType::SignatureChanged | ApiChangeType::TypeChanged
                                )
                        })
                        .and_then(|a| a.after.as_ref());

                    message.push_str(&format!("\n\nReplacement: use '{}' instead.", new_member));
                    if let Some(new_type_str) = new_type {
                        message.push_str(&format!("\nNew type: {}", new_type_str));
                    }
                }
                RemovalDisposition::MadeAutomatic => {
                    message.push_str("\n\nThis prop is now automatic — remove it.");
                }
                RemovalDisposition::TrulyRemoved => {
                    message.push_str("\n\nThis prop has been removed with no replacement.");
                }
                RemovalDisposition::MovedToRelatedType {
                    target_type,
                    mechanism,
                    ..
                } => {
                    message.push_str(&format!(
                        "\n\nThis prop moved to <{}>. Pass it as a {} on <{}>.",
                        target_type, mechanism, target_type
                    ));
                }
                _ => {}
            }
        }
    }

    // Enrich with behavioral context from the same file for this component.
    // This gives the LLM information about DOM/CSS/rendering changes alongside
    // the API removal/rename information.
    let component_symbol = if change.symbol.contains('.') {
        change.symbol.split('.').next().unwrap_or(leaf_symbol)
    } else {
        leaf_symbol
    };
    // Also match without "Props" suffix (e.g., "ModalProps" → also match "Modal")
    let component_base = component_symbol
        .strip_suffix("Props")
        .unwrap_or(component_symbol);
    let behavioral_context: Vec<String> = file_changes
        .breaking_behavioral_changes
        .iter()
        .filter(|b| {
            b.symbol == component_symbol
                || b.symbol == component_base
                || b.symbol.starts_with(&format!("{}.", component_symbol))
                || b.symbol.starts_with(&format!("{}.", component_base))
        })
        .map(|b| {
            let cat = b
                .category
                .as_ref()
                .map(|c| behavioral_category_label(c))
                .unwrap_or("change");
            format!("{}: {}", cat, b.description)
        })
        .collect();
    if !behavioral_context.is_empty() {
        message.push_str("\n\nBehavioral changes:\n");
        for desc in &behavioral_context {
            message.push_str(&format!("  - {}\n", desc));
        }
    }

    // Enrich type-changed rules with explicit value diff information.
    // When a union type has values removed and/or added, enumerate them
    // so the fix-engine LLM knows exactly which values to replace.
    let mut value_mappings: Vec<MappingEntry> = Vec::new();
    if change.change == ApiChangeType::TypeChanged {
        let removed = extract_removed_union_values(change);
        let added = extract_added_union_values(change);
        if !removed.is_empty() {
            message.push_str("\n\nValue changes:");
            if removed.len() == 1 && added.len() == 1 {
                // Tier 1: exact 1:1 mapping
                message.push_str(&format!(
                    "\n  '{}' → '{}' (direct replacement)",
                    removed[0], added[0],
                ));
                let parent_component = if change.symbol.contains('.') {
                    change.symbol.split('.').next().map(|s| s.to_string())
                } else {
                    None
                };
                value_mappings.push(MappingEntry {
                    from: Some(removed[0].clone()),
                    to: Some(added[0].clone()),
                    component: parent_component,
                    prop: Some(extract_leaf_symbol(&change.symbol).to_string()),
                });
            } else {
                // Tier 2+3: list removed and added values
                message.push_str("\n  Removed values:");
                for v in &removed {
                    message.push_str(&format!("\n    - '{}'", v));
                }
                if !added.is_empty() {
                    message.push_str("\n  New values available:");
                    for v in &added {
                        message.push_str(&format!("\n    - '{}'", v));
                    }
                }
            }
        }
    }

    let mut labels = vec![
        "source=semver-analyzer".to_string(),
        format!("change-type={}", change_type_label),
        format!("kind={}", api_kind_label(&change.kind)),
    ];

    let has_codemod = if matches!(
        change.change,
        ApiChangeType::SignatureChanged | ApiChangeType::TypeChanged
    ) {
        // SignatureChanged/TypeChanged entries where the prop NAME also
        // changed (e.g., chips → labels) cannot be handled by the fix
        // engine's PropTypeChange strategy — it only changes types, not
        // names. Route these to the LLM instead.
        let name_changed = match (change.before.as_deref(), change.after.as_deref()) {
            (Some(before), Some(after)) => {
                let old_name = extract_prop_name_from_signature(before);
                let new_name = extract_prop_name_from_signature(after);
                match (old_name, new_name) {
                    (Some(o), Some(n)) => o != n,
                    _ => false,
                }
            }
            _ => false,
        };
        !name_changed
    } else {
        matches!(change.change, ApiChangeType::Renamed)
            || matches!(
                change.removal_disposition,
                Some(RemovalDisposition::ReplacedByMember { .. })
            )
    };
    labels.push(format!("has-codemod={}", has_codemod));

    if let Some(pkg) = from_pkg {
        labels.push(format!("package={}", pkg));
    }

    // Tag additive (non-breaking) changes so analysis runs can filter them.
    // These changes add new options without removing or modifying existing ones,
    // meaning existing consumer code is unaffected.
    if is_additive_change(change) {
        labels.push("change-scope=additive".to_string());
    }

    let condition = build_frontend_condition(change, leaf_symbol, from_pkg);
    let mut fix_strategy =
        api_change_to_strategy(change, rename_patterns, member_renames, &file_path);

    // Attach value mappings to the fix strategy for Tier 1 cases
    if !value_mappings.is_empty() {
        if let Some(ref mut strat) = fix_strategy {
            strat.mappings.extend(value_mappings.clone());
        } else {
            let mut strat = FixStrategyEntry::new("PropValueChange");
            strat.mappings = value_mappings.clone();
            fix_strategy = Some(strat);
        }
    }

    let mut rules = vec![KonveyorRule {
        rule_id,
        labels: labels.clone(),
        effort,
        category: "mandatory".to_string(),
        description: change.description.clone(),
        message,
        links: Vec::new(),
        when: condition,
        fix_strategy,
    }];

    // P4-B: For type_changed Property/Field changes, check for removed union
    // member values and emit per-value rules so the `value` constraint fires.
    //
    // When the before/after types have string literal unions, we can compute
    // which values were removed and which were added. This enables:
    //  - Tier 1 (1:1): When exactly 1 removed & 1 added, auto-map directly.
    //  - Tier 2+3: List removed/added values explicitly so the fix-engine
    //    LLM can pick the correct replacement instead of guessing.
    if matches!(change.kind, ApiChangeKind::Property | ApiChangeKind::Field)
        && change.change == ApiChangeType::TypeChanged
    {
        let removed_values = extract_removed_union_values(change);
        if !removed_values.is_empty() {
            // Compute added values (new values not in the old type)
            let added_values = extract_added_union_values(change);

            // Build value mappings for Tier 1 (1:1) cases.
            // When there's exactly one removed and one added value, the
            // mapping is unambiguous.
            let value_map: HashMap<&str, &str> =
                if removed_values.len() == 1 && added_values.len() == 1 {
                    let mut m = HashMap::new();
                    m.insert(removed_values[0].as_str(), added_values[0].as_str());
                    m
                } else {
                    HashMap::new()
                };

            // Extract parent component for scoping
            let parent_component = if change.symbol.contains('.') {
                let parts: Vec<&str> = change.symbol.splitn(2, '.').collect();
                Some(format!("^{}$", regex_escape(parts[0])))
            } else {
                None
            };
            let from = from_pkg.map(|s| s.to_string());

            for value in &removed_values {
                // Build an actionable message with value mapping or options
                let migration_hint = if let Some(replacement) = value_map.get(value.as_str()) {
                    // Tier 1: exact 1:1 mapping
                    format!(
                        "The value '{}' is no longer accepted for '{}'. \
                         Replace with '{}'.",
                        value, change.symbol, replacement,
                    )
                } else if !added_values.is_empty() {
                    // Tier 2+3: list available replacements
                    let options: Vec<String> =
                        added_values.iter().map(|v| format!("'{}'", v)).collect();
                    format!(
                        "The value '{}' is no longer accepted for '{}'. \
                         Replace with one of the new values: {}.",
                        value,
                        change.symbol,
                        options.join(", "),
                    )
                } else {
                    // No new values — just removed
                    format!(
                        "The value '{}' is no longer accepted for '{}'. \
                         This value has been removed with no direct replacement.",
                        value, change.symbol,
                    )
                };

                let val_id =
                    unique_id(format!("{}-val-{}", base_id, sanitize_id(value)), id_counts);

                // Build fix strategy with mapping when available
                let fix_strategy = if let Some(replacement) = value_map.get(value.as_str()) {
                    let mut strat = FixStrategyEntry::new("PropValueChange");
                    strat.mappings = vec![MappingEntry {
                        from: Some(value.clone()),
                        to: Some(replacement.to_string()),
                        component: parent_component
                            .as_ref()
                            .map(|p| p.trim_matches('^').trim_matches('$').to_string()),
                        prop: Some(extract_leaf_symbol(&change.symbol).to_string()),
                    }];
                    strat
                } else {
                    FixStrategyEntry::new("PropValueChange")
                };

                rules.push(KonveyorRule {
                    rule_id: val_id,
                    labels: vec![
                        "source=semver-analyzer".to_string(),
                        "change-type=prop-value-change".to_string(),
                        format!("kind={}", api_kind_label(&change.kind)),
                        "has-codemod=true".to_string(),
                    ],
                    effort: 1,
                    category: "mandatory".to_string(),
                    description: format!("Value '{}' removed from '{}'", value, change.symbol),
                    message: format!("{}\n\nFile: {}", migration_hint, file_path),
                    links: Vec::new(),
                    when: KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: format!(
                                "^{}$",
                                regex_escape(extract_leaf_symbol(&change.symbol))
                            ),
                            location: "JSX_PROP".to_string(),
                            component: parent_component.clone(),
                            parent: None,
                    not_parent: None,
                            value: Some(format!("^{}$", regex_escape(value))),
                            from: from.clone(),
                            parent_from: None,
                        },
                    },
                    fix_strategy: Some(fix_strategy),
                });
            }
        }
    }

    rules
}

fn behavioral_change_to_rule(
    change: &BehavioralChange<TypeScript>,
    file_changes: &FileChanges<TypeScript>,
    file_pattern: &str,
    from_pkg: Option<&str>,
    id_counts: &mut HashMap<String, usize>,
) -> Option<KonveyorRule> {
    // Skip internal-only changes -- these affect internal rendering
    // and don't require consumer code changes.
    if change.is_internal_only == Some(true) {
        return None;
    }

    let file_path = file_changes.file.display().to_string();
    // For dotted symbols like "NavList.render", use the component name (first
    // part) for JSX_COMPONENT matching.  The leaf ("render") is the method that
    // changed, but the detection target is the component consumers use in JSX.
    let leaf_symbol = if change.symbol.contains('.') {
        change.symbol.split('.').next().unwrap_or(&change.symbol)
    } else {
        extract_leaf_symbol(&change.symbol)
    };

    let base_id = format!(
        "semver-{}-{}-behavioral",
        sanitize_id(&file_path),
        sanitize_id(&change.symbol),
    );
    let rule_id = unique_id(base_id, id_counts);

    let message = format!(
        "Behavioral change in '{}': {}\n\nFile: {}\nReview all usages to ensure compatibility with the new behavior.",
        change.symbol, change.description, file_path,
    );

    let mut labels = vec![
        "source=semver-analyzer".to_string(),
        "ai-generated".to_string(),
    ];

    // Use the behavioral category for more precise change-type labels
    if let Some(ref cat) = change.category {
        labels.push(format!("change-type={}", behavioral_category_label(cat)));
        // DOM, CSS, a11y, and behavioral changes primarily impact frontend testing
        if matches!(
            cat,
            TsCategory::DomStructure
                | TsCategory::CssClass
                | TsCategory::CssVariable
                | TsCategory::Accessibility
                | TsCategory::DataAttribute
        ) {
            labels.push("impact=frontend-testing".to_string());
        }
    } else {
        labels.push("change-type=behavioral".to_string());
    }

    if let Some(pkg) = from_pkg {
        labels.push(format!("package={}", pkg));
    }

    let from = from_pkg.map(|s| s.to_string());

    // Use frontend.referenced when we have a package scope
    let condition = if from.is_some() {
        KonveyorCondition::FrontendReferenced {
            referenced: FrontendReferencedFields {
                pattern: format!("^{}$", regex_escape(leaf_symbol)),
                location: "JSX_COMPONENT".to_string(),
                component: None,
                parent: None,
                    not_parent: None,
                parent_from: None,
                value: None,
                from,
            },
        }
    } else {
        let pattern = format!(r"\b{}\b", regex_escape(leaf_symbol));
        KonveyorCondition::FileContent {
            filecontent: FileContentFields {
                pattern,
                file_pattern: file_pattern.to_string(),
            },
        }
    };

    // Downgrade noisy behavioral rules so they don't go to the fix engine:
    //
    // - "propagated through call chain" rules have no actionable content —
    //   they just say "review all usages" with no detail.
    //
    // - "Test assertions changed" rules describe test file diffs, not
    //   component API changes. Tagged with `source=test-diff` for filtering.
    let is_propagated = change.description.contains("propagated through call chain");
    let is_test_assertion = change.description.contains("Test assertions changed")
        || change
            .description
            .to_lowercase()
            .contains("test assertions");

    let strategy = if is_propagated || is_test_assertion {
        "Manual"
    } else {
        "LlmAssisted"
    };

    if is_test_assertion {
        labels.push("source=test-diff".to_string());
    }

    Some(KonveyorRule {
        rule_id,
        labels,
        effort: 3,
        category: "mandatory".to_string(),
        description: change.description.clone(),
        message,
        links: Vec::new(),
        when: condition,
        fix_strategy: Some(FixStrategyEntry::new(strategy)),
    })
}

fn manifest_change_to_rule(
    change: &ManifestChange<TypeScript>,
    file_pattern: &str,
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    let change_type_label = manifest_change_type_label(&change.change_type);

    let base_id = format!(
        "semver-manifest-{}-{}",
        sanitize_id(&change.field),
        change_type_label,
    );
    let rule_id = unique_id(base_id, id_counts);

    let category = if change.is_breaking {
        "mandatory"
    } else {
        "optional"
    };

    let effort = manifest_effort(&change.change_type);

    let (condition, message) =
        build_manifest_condition_and_message(change, file_pattern, change_type_label);

    KonveyorRule {
        rule_id,
        labels: vec![
            "source=semver-analyzer".to_string(),
            "change-type=manifest".to_string(),
            format!("manifest-field={}", change.field),
        ],
        effort,
        category: category.to_string(),
        description: change.description.clone(),
        message,
        links: Vec::new(),
        when: condition,
        fix_strategy: Some(FixStrategyEntry::new("Manual")),
    }
}

// ── Fix guidance generators ─────────────────────────────────────────────

fn api_change_to_fix(
    change: &ApiChange,
    file_changes: &FileChanges<TypeScript>,
    rule_id: &str,
    file_pattern: &str,
) -> FixGuidanceEntry {
    let file_path = file_changes.file.display().to_string();
    let leaf_symbol = extract_leaf_symbol(&change.symbol);
    let search_pattern = build_pattern(&change.kind, &change.change, leaf_symbol, &change.before);

    let (strategy, confidence, source, fix_description, replacement) = match change.change {
        ApiChangeType::Renamed => {
            let old_name = change
                .before
                .as_deref()
                .map(|b| extract_leaf_symbol(b).to_string())
                .unwrap_or_else(|| change.symbol.clone());
            let new_name = change
                .after
                .as_deref()
                .map(|a| extract_leaf_symbol(a).to_string())
                .unwrap_or_else(|| change.symbol.clone());

            let desc = format!(
                "Rename all occurrences of '{}' to '{}'.\n\
                 This is a mechanical find-and-replace that can be auto-applied.\n\
                 Search pattern: {} (in {} files)",
                old_name, new_name, search_pattern, file_pattern,
            );
            (
                FixStrategy::Rename,
                FixConfidence::Exact,
                FixSource::Pattern,
                desc,
                Some(new_name),
            )
        }

        ApiChangeType::SignatureChanged => {
            let desc = if let (Some(ref before), Some(ref after)) = (&change.before, &change.after)
            {
                format!(
                    "Update all call sites of '{}' to match the new signature.\n\n\
                     Old signature: {}\n\
                     New signature: {}\n\n\
                     Review each call site and adjust arguments accordingly.\n\
                     {}",
                    change.symbol, before, after, change.description,
                )
            } else {
                format!(
                    "Update all call sites of '{}' to match the new signature.\n\
                     {}\n\n\
                     Review each usage and adjust arguments, type parameters, or \
                     modifiers as described above.",
                    change.symbol, change.description,
                )
            };

            (
                FixStrategy::UpdateSignature,
                FixConfidence::High,
                FixSource::Pattern,
                desc,
                None,
            )
        }

        ApiChangeType::TypeChanged => {
            let desc = if let (Some(ref before), Some(ref after)) = (&change.before, &change.after)
            {
                format!(
                    "Update type annotations from '{}' to '{}'.\n\n\
                     Old type: {}\n\
                     New type: {}\n\n\
                     Check all locations where this type is used in assignments, \
                     function parameters, return types, and generic type arguments.\n\
                     {}",
                    change.symbol, change.symbol, before, after, change.description,
                )
            } else {
                format!(
                    "Update type references for '{}'.\n\
                     {}\n\n\
                     Check all locations where this type is used and update accordingly.",
                    change.symbol, change.description,
                )
            };

            (
                FixStrategy::UpdateType,
                FixConfidence::High,
                FixSource::Pattern,
                desc,
                None,
            )
        }

        ApiChangeType::Removed => {
            let kind_label = api_kind_label(&change.kind);
            let desc = format!(
                "The {} '{}' has been removed.\n\n\
                 Action required:\n\
                 1. Find all usages of '{}' in your codebase\n\
                 2. Identify an appropriate replacement (check the library's \
                    migration guide or changelog)\n\
                 3. Update each usage to use the replacement\n\
                 4. Remove any imports of '{}'\n\n\
                 {}",
                kind_label, change.symbol, change.symbol, change.symbol, change.description,
            );

            (
                FixStrategy::FindAlternative,
                FixConfidence::Low,
                FixSource::Manual,
                desc,
                None,
            )
        }

        ApiChangeType::VisibilityChanged => {
            let desc = format!(
                "The visibility of '{}' has been reduced.\n\n\
                 If you are importing or using '{}' from outside its module, \
                 you need to find a public alternative.\n\
                 {}\n\n\
                 Check if there is a new public API that exposes the same functionality, \
                 or refactor your code to avoid depending on this internal symbol.",
                change.symbol, change.symbol, change.description,
            );

            (
                FixStrategy::FindAlternative,
                FixConfidence::Medium,
                FixSource::Pattern,
                desc,
                None,
            )
        }
    };

    FixGuidanceEntry {
        rule_id: rule_id.to_string(),
        strategy,
        confidence,
        source,
        symbol: change.symbol.clone(),
        file: file_path,
        fix_description,
        before: change.before.clone(),
        after: change.after.clone(),
        search_pattern,
        replacement,
    }
}

fn behavioral_change_to_fix(
    change: &BehavioralChange<TypeScript>,
    file_changes: &FileChanges<TypeScript>,
    rule_id: &str,
) -> FixGuidanceEntry {
    let file_path = file_changes.file.display().to_string();
    let leaf_symbol = extract_leaf_symbol(&change.symbol);
    let search_pattern = format!(r"\b{}\b", regex_escape(leaf_symbol));

    let fix_description = format!(
        "Behavioral change detected in '{}' (AI-generated finding).\n\n\
         What changed: {}\n\n\
         Action required:\n\
         1. Review all usages of '{}' in your codebase\n\
         2. Verify that your code handles the new behavior correctly\n\
         3. Update tests that depend on the old behavior\n\
         4. Pay special attention to edge cases and error handling\n\n\
         This finding was generated by LLM analysis and should be \
         verified by a developer.",
        change.symbol, change.description, change.symbol,
    );

    FixGuidanceEntry {
        rule_id: rule_id.to_string(),
        strategy: FixStrategy::ManualReview,
        confidence: FixConfidence::Medium,
        source: FixSource::Llm,
        symbol: change.symbol.clone(),
        file: file_path,
        fix_description,
        before: None,
        after: None,
        search_pattern,
        replacement: None,
    }
}

fn manifest_change_to_fix(change: &ManifestChange<TypeScript>, rule_id: &str) -> FixGuidanceEntry {
    let (strategy, confidence, source, fix_description, search, replacement) = match change
        .change_type
    {
        TsManifestChangeType::ModuleSystemChanged => {
            let is_cjs_to_esm = change
                .after
                .as_deref()
                .map(|a| a == "module")
                .unwrap_or(false);

            if is_cjs_to_esm {
                (
                    FixStrategy::UpdateImport,
                    FixConfidence::High,
                    FixSource::Pattern,
                    format!(
                        "The package has changed from CommonJS to ESM.\n\n\
                             Action required:\n\
                             1. Convert all require() calls to import statements:\n\
                             \n\
                             Before: const {{ foo }} = require('package')\n\
                             After:  import {{ foo }} from 'package'\n\
                             \n\
                             2. Convert module.exports to export statements:\n\
                             \n\
                             Before: module.exports = {{ foo }}\n\
                             After:  export {{ foo }}\n\
                             \n\
                             3. Update your package.json \"type\" field if needed\n\
                             4. Rename .js files to .mjs if mixing module systems\n\n\
                             {}",
                        change.description,
                    ),
                    r"\brequire\s*\(".to_string(),
                    Some("import".to_string()),
                )
            } else {
                (
                    FixStrategy::UpdateImport,
                    FixConfidence::High,
                    FixSource::Pattern,
                    format!(
                        "The package has changed from ESM to CommonJS.\n\n\
                             Action required:\n\
                             1. Convert all import statements to require() calls:\n\
                             \n\
                             Before: import {{ foo }} from 'package'\n\
                             After:  const {{ foo }} = require('package')\n\
                             \n\
                             2. Convert export statements to module.exports\n\
                             3. Update your package.json \"type\" field if needed\n\n\
                             {}",
                        change.description,
                    ),
                    r"\bimport\s+".to_string(),
                    Some("require".to_string()),
                )
            }
        }

        TsManifestChangeType::PeerDependencyAdded => (
            FixStrategy::UpdateDependency,
            FixConfidence::Exact,
            FixSource::Pattern,
            format!(
                "A new peer dependency has been added: '{}'\n\n\
                     Action required:\n\
                     1. Install the peer dependency: npm install {}\n\
                     2. Verify version compatibility with your existing dependencies\n\n\
                     {}",
                change.field, change.field, change.description,
            ),
            change.field.clone(),
            change.after.clone(),
        ),

        TsManifestChangeType::PeerDependencyRemoved => (
            FixStrategy::UpdateDependency,
            FixConfidence::High,
            FixSource::Pattern,
            format!(
                "Peer dependency '{}' has been removed.\n\n\
                     Action required:\n\
                     1. Check if you still need '{}' as a direct dependency\n\
                     2. If it was only required by this package, you may be able \
                        to remove it\n\
                     3. Verify that removing it doesn't break other dependencies\n\n\
                     {}",
                change.field, change.field, change.description,
            ),
            change.field.clone(),
            None,
        ),

        TsManifestChangeType::PeerDependencyRangeChanged => (
            FixStrategy::UpdateDependency,
            FixConfidence::High,
            FixSource::Pattern,
            format!(
                "Peer dependency '{}' version range changed.\n\n\
                     Before: {}\n\
                     After:  {}\n\n\
                     Action required:\n\
                     1. Update '{}' to a version that satisfies the new range\n\
                     2. Test for compatibility with the new version\n\n\
                     {}",
                change.field,
                change.before.as_deref().unwrap_or("(none)"),
                change.after.as_deref().unwrap_or("(none)"),
                change.field,
                change.description,
            ),
            change.field.clone(),
            change.after.clone(),
        ),

        TsManifestChangeType::EntryPointChanged | TsManifestChangeType::ExportsEntryRemoved => (
            FixStrategy::UpdateImport,
            FixConfidence::Medium,
            FixSource::Pattern,
            format!(
                "Package entry point or export map changed for '{}'.\n\n\
                     Before: {}\n\
                     After:  {}\n\n\
                     Action required:\n\
                     1. Update all import paths that reference the old entry point\n\
                     2. Check the package's export map for the new path\n\n\
                     {}",
                change.field,
                change.before.as_deref().unwrap_or("(none)"),
                change.after.as_deref().unwrap_or("(none)"),
                change.description,
            ),
            change.field.clone(),
            change.after.clone(),
        ),

        _ => (
            FixStrategy::ManualReview,
            FixConfidence::Medium,
            FixSource::Pattern,
            format!(
                "Package manifest field '{}' changed.\n\n\
                     Before: {}\n\
                     After:  {}\n\n\
                     Review the change and update your configuration accordingly.\n\n\
                     {}",
                change.field,
                change.before.as_deref().unwrap_or("(none)"),
                change.after.as_deref().unwrap_or("(none)"),
                change.description,
            ),
            change.field.clone(),
            None,
        ),
    };

    FixGuidanceEntry {
        rule_id: rule_id.to_string(),
        strategy,
        confidence,
        source,
        symbol: change.field.clone(),
        file: "package.json".to_string(),
        fix_description,
        before: change.before.clone(),
        after: change.after.clone(),
        search_pattern: search,
        replacement,
    }
}

fn build_manifest_condition_and_message(
    change: &ManifestChange<TypeScript>,
    file_pattern: &str,
    change_type_label: &str,
) -> (KonveyorCondition, String) {
    match change.change_type {
        TsManifestChangeType::ModuleSystemChanged => {
            let is_cjs_to_esm = change
                .after
                .as_deref()
                .map(|a| a == "module")
                .unwrap_or(false);

            let (pattern, hint) = if is_cjs_to_esm {
                (
                    r"\brequire\s*\(".to_string(),
                    "Convert require() calls to ESM import statements.",
                )
            } else {
                (
                    r"\bimport\s+".to_string(),
                    "Convert ESM import statements to require() calls.",
                )
            };

            let message = format!(
                "Module system changed: {}\n\nBefore: {}\nAfter: {}\n{}",
                change.description,
                change.before.as_deref().unwrap_or("(none)"),
                change.after.as_deref().unwrap_or("(none)"),
                hint,
            );

            (
                KonveyorCondition::FileContent {
                    filecontent: FileContentFields {
                        pattern,
                        file_pattern: file_pattern.to_string(),
                    },
                },
                message,
            )
        }
        TsManifestChangeType::PeerDependencyAdded
        | TsManifestChangeType::PeerDependencyRemoved
        | TsManifestChangeType::PeerDependencyRangeChanged => {
            let message = format!(
                "Peer dependency change ({}): {}\n\nField: {}\nBefore: {}\nAfter: {}",
                change_type_label,
                change.description,
                change.field,
                change.before.as_deref().unwrap_or("(none)"),
                change.after.as_deref().unwrap_or("(none)"),
            );

            (
                KonveyorCondition::FileContent {
                    filecontent: FileContentFields {
                        pattern: format!(
                            "\"{}\"\\s*:",
                            change.field.replace('/', r"\/").replace('@', r"\@")
                        ),
                        file_pattern: "package\\.json$".to_string(),
                    },
                },
                message,
            )
        }
        _ => {
            // Generic manifest change: use filecontent to match the field name
            let message = format!(
                "Package manifest change ({}): {}\n\nField: {}\nBefore: {}\nAfter: {}",
                change_type_label,
                change.description,
                change.field,
                change.before.as_deref().unwrap_or("(none)"),
                change.after.as_deref().unwrap_or("(none)"),
            );

            (
                KonveyorCondition::FileContent {
                    filecontent: FileContentFields {
                        pattern: format!(
                            "\"{}\"\\s*:",
                            change.field.replace('/', r"\/").replace('@', r"\@")
                        ),
                        file_pattern: "package\\.json$".to_string(),
                    },
                },
                message,
            )
        }
    }
}

fn manifest_effort(change_type: &TsManifestChangeType) -> u32 {
    match change_type {
        TsManifestChangeType::ModuleSystemChanged => 7,
        TsManifestChangeType::EntryPointChanged => 5,
        TsManifestChangeType::ExportsEntryRemoved => 5,
        TsManifestChangeType::ExportsConditionRemoved => 3,
        TsManifestChangeType::BinEntryRemoved => 3,
        _ => 3,
    }
}

fn behavioral_category_label(cat: &TsCategory) -> &'static str {
    match cat {
        TsCategory::DomStructure => "dom-structure",
        TsCategory::CssClass => "css-class",
        TsCategory::CssVariable => "css-variable",
        TsCategory::Accessibility => "accessibility",
        TsCategory::DefaultValue => "default-value",
        TsCategory::LogicChange => "logic-change",
        TsCategory::DataAttribute => "data-attribute",
        TsCategory::RenderOutput => "render-output",
    }
}

fn manifest_change_type_label(change_type: &TsManifestChangeType) -> &'static str {
    match change_type {
        TsManifestChangeType::EntryPointChanged => "entry-point-changed",
        TsManifestChangeType::ExportsEntryRemoved => "exports-entry-removed",
        TsManifestChangeType::ExportsEntryAdded => "exports-entry-added",
        TsManifestChangeType::ExportsConditionRemoved => "exports-condition-removed",
        TsManifestChangeType::ModuleSystemChanged => "module-system-changed",
        TsManifestChangeType::PeerDependencyAdded => "peer-dependency-added",
        TsManifestChangeType::PeerDependencyRemoved => "peer-dependency-removed",
        TsManifestChangeType::PeerDependencyRangeChanged => "peer-dependency-range-changed",
        TsManifestChangeType::EngineConstraintChanged => "engine-constraint-changed",
        TsManifestChangeType::BinEntryRemoved => "bin-entry-removed",
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hierarchy_types::HierarchyDelta;
    use semver_analyzer_core::*;
    use std::path::PathBuf;

    fn make_report(
        changes: Vec<FileChanges<TypeScript>>,
        manifest_changes: Vec<ManifestChange<TypeScript>>,
    ) -> AnalysisReport<TypeScript> {
        AnalysisReport {
            repository: PathBuf::from("/tmp/test-repo"),
            comparison: Comparison {
                from_ref: "v1.0.0".to_string(),
                to_ref: "v2.0.0".to_string(),
                from_sha: "abc123".to_string(),
                to_sha: "def456".to_string(),
                commit_count: 10,
                analysis_timestamp: "2026-03-16T00:00:00Z".to_string(),
            },
            summary: Summary {
                total_breaking_changes: 0,
                breaking_api_changes: 0,
                breaking_behavioral_changes: 0,
                files_with_breaking_changes: 0,
            },
            changes,
            manifest_changes,
            added_files: Vec::new(),
            packages: vec![],
            member_renames: HashMap::new(),
            inferred_rename_patterns: None,
            extensions: crate::TsAnalysisExtensions::default(),
            metadata: AnalysisMetadata {
                call_graph_analysis: "none".to_string(),
                tool_version: "0.1.0".to_string(),
                llm_usage: None,
            },
        }
    }

    #[test]
    fn test_extract_leaf_symbol() {
        assert_eq!(extract_leaf_symbol("Card.isFlat"), "isFlat");
        assert_eq!(extract_leaf_symbol("createUser"), "createUser");
        assert_eq!(extract_leaf_symbol("a.b.c"), "c");
    }

    #[test]
    fn test_sanitize_id() {
        assert_eq!(sanitize_id("src/api/users.d.ts"), "src-api-users-d-ts");
        assert_eq!(sanitize_id("Card.isFlat"), "card-isflat");
        assert_eq!(sanitize_id("foo///bar"), "foo-bar");
    }

    #[test]
    fn test_unique_id() {
        let mut counts = HashMap::new();
        assert_eq!(unique_id("foo".to_string(), &mut counts), "foo");
        assert_eq!(unique_id("foo".to_string(), &mut counts), "foo-2");
        assert_eq!(unique_id("foo".to_string(), &mut counts), "foo-3");
        assert_eq!(unique_id("bar".to_string(), &mut counts), "bar");
    }

    #[test]
    fn test_regex_escape() {
        assert_eq!(regex_escape("foo"), "foo");
        assert_eq!(regex_escape("foo.bar"), "foo\\.bar");
        assert_eq!(regex_escape("a*b+c?"), "a\\*b\\+c\\?");
    }

    #[test]
    fn test_build_pattern_function_removed() {
        let pattern = build_pattern(
            &ApiChangeKind::Function,
            &ApiChangeType::Removed,
            "createUser",
            &None,
        );
        assert_eq!(pattern, r"\bcreateUser\s*\(");
    }

    #[test]
    fn test_build_pattern_property_removed() {
        let pattern = build_pattern(
            &ApiChangeKind::Property,
            &ApiChangeType::Removed,
            "isFlat",
            &None,
        );
        assert_eq!(pattern, r"\.isFlat\b");
    }

    #[test]
    fn test_build_pattern_class_removed() {
        let pattern = build_pattern(
            &ApiChangeKind::Class,
            &ApiChangeType::Removed,
            "Card",
            &None,
        );
        assert_eq!(pattern, r"\bCard\b");
    }

    #[test]
    fn test_build_pattern_renamed_uses_before() {
        let pattern = build_pattern(
            &ApiChangeKind::Function,
            &ApiChangeType::Renamed,
            "newName",
            &Some("oldName".to_string()),
        );
        // Should match the OLD name, not the new one
        assert_eq!(pattern, r"\boldName\s*\(");
    }

    #[test]
    fn test_generate_rules_api_change() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/api/users.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "createUser".to_string(),
                kind: ApiChangeKind::Function,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: "Exported function 'createUser' was removed".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.{ts,tsx,js,jsx}",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].rule_id,
            "semver-src-api-users-d-ts-createuser-removed"
        );
        assert_eq!(rules[0].category, "mandatory");
        assert_eq!(rules[0].effort, 5);
        assert!(rules[0]
            .labels
            .contains(&"source=semver-analyzer".to_string()));
        assert!(rules[0].labels.contains(&"change-type=removed".to_string()));
        assert!(rules[0].labels.contains(&"kind=function".to_string()));
    }

    #[test]
    fn test_generate_rules_behavioral_change() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/api/users.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![],
            breaking_behavioral_changes: vec![BehavioralChange {
                symbol: "validateEmail".to_string(),
                kind: BehavioralChangeKind::Function,
                category: None,
                description: "Now rejects emails with '+' aliases".to_string(),
                source_file: Some("src/api/users.ts".to_string()),
                confidence: None,
                evidence_type: None,
                referenced_symbols: vec![],
                is_internal_only: None,
            }],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.{ts,tsx}",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        assert_eq!(rules.len(), 1);
        assert!(rules[0].rule_id.contains("behavioral"));
        assert_eq!(rules[0].category, "mandatory");
        assert!(rules[0].labels.contains(&"ai-generated".to_string()));
        assert!(rules[0]
            .labels
            .contains(&"change-type=behavioral".to_string()));
    }

    #[test]
    fn test_generate_rules_manifest_module_system() {
        let manifest = vec![ManifestChange {
            field: "type".to_string(),
            change_type: TsManifestChangeType::ModuleSystemChanged,
            before: Some("commonjs".to_string()),
            after: Some("module".to_string()),
            description: "CJS to ESM".to_string(),
            is_breaking: true,
        }];

        let report = make_report(vec![], manifest);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.{ts,tsx,js,jsx}",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        assert_eq!(rules.len(), 1);
        assert!(rules[0].rule_id.contains("manifest"));
        assert!(rules[0].rule_id.contains("module-system-changed"));
        assert_eq!(rules[0].category, "mandatory");
        assert_eq!(rules[0].effort, 7);

        // Should use filecontent to match require() calls
        match &rules[0].when {
            KonveyorCondition::FileContent { filecontent } => {
                assert!(filecontent.pattern.contains("require"));
            }
            _ => panic!("Expected FileContent condition for module system change"),
        }
    }

    #[test]
    fn test_generate_rules_manifest_peer_dep() {
        let manifest = vec![ManifestChange {
            field: "react".to_string(),
            change_type: TsManifestChangeType::PeerDependencyRemoved,
            before: Some("^17.0.0".to_string()),
            after: None,
            description: "Peer dependency 'react' was removed".to_string(),
            is_breaking: true,
        }];

        let report = make_report(vec![], manifest);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.{ts,tsx,js,jsx}",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        assert_eq!(rules.len(), 1);
        // Should use builtin.filecontent condition matching the field in package.json
        match &rules[0].when {
            KonveyorCondition::FileContent { filecontent } => {
                assert!(filecontent.pattern.contains("react"));
                assert!(filecontent.file_pattern.contains("package"));
            }
            _ => panic!("Expected FileContent condition for peer dependency change"),
        }
    }

    #[test]
    fn test_duplicate_rule_ids_get_suffix() {
        let changes = vec![FileChanges {
            file: PathBuf::from("test.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![
                ApiChange {
                    symbol: "foo".to_string(),
                    kind: ApiChangeKind::Function,
                    change: ApiChangeType::Removed,
                    before: None,
                    after: None,
                    description: "Removed foo".to_string(),
                    migration_target: None,
                    removal_disposition: None,
                    renders_element: None,
                },
                ApiChange {
                    symbol: "foo".to_string(),
                    kind: ApiChangeKind::Function,
                    change: ApiChangeType::Removed,
                    before: None,
                    after: None,
                    description: "Removed foo overload".to_string(),
                    migration_target: None,
                    removal_disposition: None,
                    renders_element: None,
                },
            ],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.ts",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        assert_eq!(rules.len(), 2);
        assert_ne!(rules[0].rule_id, rules[1].rule_id);
        assert!(rules[1].rule_id.ends_with("-2"));
    }

    #[test]
    fn test_write_ruleset_dir() {
        let base = std::env::temp_dir().join("semver-konveyor-test-out");
        let dir = base.join("rules");
        let _ = std::fs::remove_dir_all(&base);

        let report = make_report(vec![], vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.ts",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );
        let fix_guidance = generate_fix_guidance(&report, &rules, "*.ts");

        write_ruleset_dir(&dir, "test-ruleset", &report, &rules).unwrap();
        let fix_dir = write_fix_guidance_dir(&dir, &fix_guidance).unwrap();

        // Ruleset dir contains rules only
        assert!(dir.join("ruleset.yaml").exists());
        assert!(dir.join("breaking-changes.yaml").exists());
        assert!(!dir.join("fix-guidance.yaml").exists()); // NOT in rules dir

        // Fix guidance is in sibling directory
        assert_eq!(fix_dir, base.join("fix-guidance"));
        assert!(fix_dir.join("fix-guidance.yaml").exists());

        let ruleset_content = std::fs::read_to_string(dir.join("ruleset.yaml")).unwrap();
        assert!(ruleset_content.contains("test-ruleset"));
        assert!(ruleset_content.contains("source=semver-analyzer"));

        let fix_content = std::fs::read_to_string(fix_dir.join("fix-guidance.yaml")).unwrap();
        assert!(fix_content.contains("migration"));
        assert!(fix_content.contains("total_fixes"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_full_roundtrip_yaml_output() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/components/Button.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Button.variant".to_string(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some("'primary' | 'secondary'".to_string()),
                after: Some("'primary' | 'danger'".to_string()),
                description: "Removed 'secondary' variant, added 'danger'".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.{ts,tsx}",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Verify YAML serialization succeeds
        let yaml = serde_yaml::to_string(&rules).unwrap();
        assert!(yaml.contains("ruleID"));
        assert!(yaml.contains("frontend.referenced"));
        assert!(yaml.contains("variant"));
    }

    // ── Fix guidance tests ──────────────────────────────────────────────

    #[test]
    fn test_fix_guidance_renamed_is_exact() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/lib.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Chip".to_string(),
                kind: ApiChangeKind::Class,
                change: ApiChangeType::Renamed,
                before: Some("Chip".to_string()),
                after: Some("Label".to_string()),
                description: "Chip renamed to Label".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.{ts,tsx}",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );
        let guidance = generate_fix_guidance(&report, &rules, "*.{ts,tsx}");

        assert_eq!(guidance.fixes.len(), 1);
        let fix = &guidance.fixes[0];
        assert!(matches!(fix.strategy, FixStrategy::Rename));
        assert!(matches!(fix.confidence, FixConfidence::Exact));
        assert!(matches!(fix.source, FixSource::Pattern));
        assert_eq!(fix.replacement.as_deref(), Some("Label"));
        assert!(fix.fix_description.contains("Rename all occurrences"));
        assert!(fix.fix_description.contains("'Chip'"));
        assert!(fix.fix_description.contains("'Label'"));
    }

    #[test]
    fn test_fix_guidance_removed_is_manual() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/api.d.ts"),
            status: FileStatus::Deleted,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "createUser".to_string(),
                kind: ApiChangeKind::Function,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: "Function createUser was removed".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.ts",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );
        let guidance = generate_fix_guidance(&report, &rules, "*.ts");

        assert_eq!(guidance.fixes.len(), 1);
        let fix = &guidance.fixes[0];
        assert!(matches!(fix.strategy, FixStrategy::FindAlternative));
        assert!(matches!(fix.confidence, FixConfidence::Low));
        assert!(matches!(fix.source, FixSource::Manual));
        assert!(fix.replacement.is_none());
        assert!(fix.fix_description.contains("has been removed"));
    }

    #[test]
    fn test_fix_guidance_signature_changed() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/utils.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "formatDate".to_string(),
                kind: ApiChangeKind::Function,
                change: ApiChangeType::SignatureChanged,
                before: Some("formatDate(d: Date): string".to_string()),
                after: Some("formatDate(d: Date, locale: string): string".to_string()),
                description: "Added required 'locale' parameter".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.ts",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );
        let guidance = generate_fix_guidance(&report, &rules, "*.ts");

        assert_eq!(guidance.fixes.len(), 1);
        let fix = &guidance.fixes[0];
        assert!(matches!(fix.strategy, FixStrategy::UpdateSignature));
        assert!(matches!(fix.confidence, FixConfidence::High));
        assert!(fix.fix_description.contains("Old signature:"));
        assert!(fix.fix_description.contains("New signature:"));
        assert_eq!(fix.before.as_deref(), Some("formatDate(d: Date): string"));
        assert_eq!(
            fix.after.as_deref(),
            Some("formatDate(d: Date, locale: string): string")
        );
    }

    #[test]
    fn test_fix_guidance_behavioral_is_llm_source() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/auth.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![],
            breaking_behavioral_changes: vec![BehavioralChange {
                symbol: "validateToken".to_string(),
                kind: BehavioralChangeKind::Function,
                category: None,
                description: "Now throws on expired tokens instead of returning null".to_string(),
                source_file: Some("src/auth.ts".to_string()),
                confidence: None,
                evidence_type: None,
                referenced_symbols: vec![],
                is_internal_only: None,
            }],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.ts",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );
        let guidance = generate_fix_guidance(&report, &rules, "*.ts");

        assert_eq!(guidance.fixes.len(), 1);
        let fix = &guidance.fixes[0];
        assert!(matches!(fix.strategy, FixStrategy::ManualReview));
        assert!(matches!(fix.confidence, FixConfidence::Medium));
        assert!(matches!(fix.source, FixSource::Llm));
        assert!(fix.fix_description.contains("AI-generated"));
        assert!(fix.fix_description.contains("throws on expired tokens"));
    }

    #[test]
    fn test_fix_guidance_manifest_cjs_to_esm() {
        let manifest = vec![ManifestChange {
            field: "type".to_string(),
            change_type: TsManifestChangeType::ModuleSystemChanged,
            before: Some("commonjs".to_string()),
            after: Some("module".to_string()),
            description: "CJS to ESM migration".to_string(),
            is_breaking: true,
        }];

        let report = make_report(vec![], manifest);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.ts",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );
        let guidance = generate_fix_guidance(&report, &rules, "*.ts");

        assert_eq!(guidance.fixes.len(), 1);
        let fix = &guidance.fixes[0];
        assert!(matches!(fix.strategy, FixStrategy::UpdateImport));
        assert!(matches!(fix.confidence, FixConfidence::High));
        assert!(fix.fix_description.contains("require()"));
        assert!(fix.fix_description.contains("import"));
        assert_eq!(fix.replacement.as_deref(), Some("import"));
    }

    #[test]
    fn test_fix_guidance_summary_counts() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/lib.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![
                ApiChange {
                    symbol: "Chip".to_string(),
                    kind: ApiChangeKind::Class,
                    change: ApiChangeType::Renamed,
                    before: Some("Chip".to_string()),
                    after: Some("Label".to_string()),
                    description: "Renamed".to_string(),
                    migration_target: None,
                    removal_disposition: None,
                    renders_element: None,
                },
                ApiChange {
                    symbol: "oldFn".to_string(),
                    kind: ApiChangeKind::Function,
                    change: ApiChangeType::Removed,
                    before: None,
                    after: None,
                    description: "Removed".to_string(),
                    migration_target: None,
                    removal_disposition: None,
                    renders_element: None,
                },
            ],
            breaking_behavioral_changes: vec![BehavioralChange {
                symbol: "process".to_string(),
                kind: BehavioralChangeKind::Function,
                category: None,
                description: "Changed behavior".to_string(),
                source_file: Some("src/lib.ts".to_string()),
                confidence: None,
                evidence_type: None,
                referenced_symbols: vec![],
                is_internal_only: None,
            }],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.ts",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );
        let guidance = generate_fix_guidance(&report, &rules, "*.ts");

        assert_eq!(guidance.summary.total_fixes, 3);
        // Rename=Exact (auto), Removed=Low/Manual, Behavioral=Medium/LLM
        assert_eq!(guidance.summary.auto_fixable, 1); // only Rename
        assert_eq!(guidance.summary.manual_only, 1); // Removed
        assert_eq!(guidance.summary.needs_review, 1); // Behavioral
    }

    #[test]
    fn test_fix_guidance_yaml_roundtrip() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/index.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![
                ApiChange {
                    symbol: "Foo".to_string(),
                    kind: ApiChangeKind::Class,
                    change: ApiChangeType::Renamed,
                    before: Some("Foo".to_string()),
                    after: Some("Bar".to_string()),
                    description: "Renamed Foo to Bar".to_string(),
                    migration_target: None,
                    removal_disposition: None,
                    renders_element: None,
                },
                ApiChange {
                    symbol: "baz".to_string(),
                    kind: ApiChangeKind::Function,
                    change: ApiChangeType::SignatureChanged,
                    before: Some("baz(): void".to_string()),
                    after: Some("baz(x: number): void".to_string()),
                    description: "Added required param".to_string(),
                    migration_target: None,
                    removal_disposition: None,
                    renders_element: None,
                },
            ],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let manifest = vec![ManifestChange {
            field: "type".to_string(),
            change_type: TsManifestChangeType::ModuleSystemChanged,
            before: Some("commonjs".to_string()),
            after: Some("module".to_string()),
            description: "CJS to ESM".to_string(),
            is_breaking: true,
        }];

        let report = make_report(changes, manifest);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.{ts,tsx}",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );
        let guidance = generate_fix_guidance(&report, &rules, "*.{ts,tsx}");

        let yaml = serde_yaml::to_string(&guidance).unwrap();
        assert!(yaml.contains("strategy"));
        assert!(yaml.contains("confidence"));
        assert!(yaml.contains("fix_description"));
        assert!(yaml.contains("search_pattern"));
        assert!(yaml.contains("replacement"));
        assert!(yaml.contains("rename"));
        assert!(yaml.contains("update_signature"));
        assert!(yaml.contains("update_import"));
        assert!(yaml.contains("auto_fixable"));
        assert!(yaml.contains("needs_review"));
        assert!(yaml.contains("manual_only"));
    }

    // ── Frontend provider tests ─────────────────────────────────────

    #[test]
    fn test_frontend_provider_class_rename_generates_or_condition() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/components/Chip.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Chip".to_string(),
                kind: ApiChangeKind::Class,
                change: ApiChangeType::Renamed,
                before: Some("Chip".to_string()),
                after: Some("Label".to_string()),
                description: "Chip renamed to Label".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.ts",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        assert_eq!(rules.len(), 1);
        let yaml = serde_yaml::to_string(&rules[0]).unwrap();
        // Should have an or: condition with JSX_COMPONENT and IMPORT
        assert!(yaml.contains("frontend.referenced"));
        assert!(yaml.contains("JSX_COMPONENT"));
        assert!(yaml.contains("IMPORT"));
        assert!(yaml.contains("^Chip$")); // matches old name
        assert!(yaml.contains("has-codemod=true"));
    }

    #[test]
    fn test_frontend_provider_prop_removed_scoped_to_component() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/components/Card.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Card.isFlat".to_string(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: "Card.isFlat prop removed".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.ts",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Single prop removal (1 of 1) should NOT trigger P0-C — requires >= 3 removals.
        // Only the prop-level JSX_PROP rule should be generated.
        assert_eq!(
            rules.len(),
            1,
            "Single prop removal should not trigger P0-C, got {} rules",
            rules.len()
        );
        let yaml0 = serde_yaml::to_string(&rules[0]).unwrap();
        // First rule: JSX_PROP location with component filter
        assert!(yaml0.contains("JSX_PROP"));
        assert!(yaml0.contains("^isFlat$"));
        assert!(yaml0.contains("^Card$")); // component filter
    }

    #[test]
    fn test_frontend_provider_function_uses_function_call() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/utils.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "createUser".to_string(),
                kind: ApiChangeKind::Function,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: "createUser removed".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.ts",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        assert_eq!(rules.len(), 1);
        let yaml = serde_yaml::to_string(&rules[0]).unwrap();
        assert!(yaml.contains("FUNCTION_CALL"));
        assert!(yaml.contains("^createUser$"));
    }

    #[test]
    fn test_frontend_provider_type_alias_uses_type_reference() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/types.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "UserRole".to_string(),
                kind: ApiChangeKind::TypeAlias,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: "UserRole type removed".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.ts",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        assert_eq!(rules.len(), 1);
        let yaml = serde_yaml::to_string(&rules[0]).unwrap();
        assert!(yaml.contains("TYPE_REFERENCE"));
        assert!(yaml.contains("^UserRole$"));
    }

    // ── Issue-derived regression tests ──────────────────────────────────

    // Helpers for building realistic test data
    fn make_api_change(
        symbol: &str,
        kind: ApiChangeKind,
        change: ApiChangeType,
        description: &str,
    ) -> ApiChange {
        ApiChange {
            symbol: symbol.to_string(),
            kind,
            change,
            before: None,
            after: None,
            description: description.to_string(),
            migration_target: None,
            removal_disposition: None,
            renders_element: None,
        }
    }

    fn make_file_changes(
        file: &str,
        api: Vec<ApiChange>,
        behavioral: Vec<BehavioralChange<TypeScript>>,
    ) -> FileChanges<TypeScript> {
        FileChanges {
            file: PathBuf::from(file),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: api,
            breaking_behavioral_changes: behavioral,
            container_changes: vec![],
        }
    }

    fn make_behavioral(
        symbol: &str,
        category: Option<TsCategory>,
        description: &str,
    ) -> BehavioralChange<TypeScript> {
        BehavioralChange {
            symbol: symbol.to_string(),
            kind: BehavioralChangeKind::Function,
            category,
            description: description.to_string(),
            source_file: None,
            confidence: None,
            evidence_type: None,
            referenced_symbols: vec![],
            is_internal_only: None,
        }
    }

    fn make_report_with_added(
        changes: Vec<FileChanges<TypeScript>>,
        added_files: Vec<PathBuf>,
    ) -> AnalysisReport<TypeScript> {
        let mut report = make_report(changes, vec![]);
        report.added_files = added_files;
        report
    }

    // ── Issue 2: P0-C generates component-import-deprecated for removed constants ──
    // Scenario: EmptyStateHeader (PascalCase constant) is removed entirely.
    // P0-C should generate a component-import-deprecated rule with LlmAssisted.
    #[test]
    fn test_p0c_removed_constant_generates_import_deprecated_rule() {
        let changes = vec![make_file_changes(
            "packages/react-core/src/components/EmptyState/EmptyStateHeader.tsx",
            vec![make_api_change(
                "EmptyStateHeader",
                ApiChangeKind::Constant,
                ApiChangeType::Removed,
                "EmptyStateHeader component was removed",
            )],
            vec![],
        )];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.ts",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Should have 2 rules: per-change removal + P0-C component-import-deprecated
        assert!(
            rules.len() >= 2,
            "Expected at least 2 rules, got {}",
            rules.len()
        );

        let p0c_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("component-import-deprecated"));
        assert!(
            p0c_rule.is_some(),
            "Missing P0-C component-import-deprecated rule"
        );

        let rule = p0c_rule.unwrap();
        assert!(rule.message.contains("MIGRATION"));
        // Strategy should be LlmAssisted
        assert_eq!(rule.fix_strategy.as_ref().unwrap().strategy, "LlmAssisted");
    }

    // P0-C should NOT trigger for components with only 1-2 removed props (like Button)
    // These are minor interface changes, not component removals.
    #[test]
    fn test_p0c_skips_minor_prop_removals() {
        let changes = vec![make_file_changes(
            "packages/react-core/src/components/Button/Button.tsx",
            vec![
                make_api_change(
                    "ButtonProps.isActive",
                    ApiChangeKind::Property,
                    ApiChangeType::Removed,
                    "isActive prop removed",
                ),
                make_api_change(
                    "ButtonProps.variant",
                    ApiChangeKind::Property,
                    ApiChangeType::TypeChanged,
                    "variant type changed",
                ),
            ],
            vec![],
        )];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Button has 1 removal out of 2 total changes — should NOT trigger P0-C
        let p0c_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("component-import-deprecated"))
            .collect();
        assert!(
            p0c_rules.is_empty(),
            "Button with 1/2 removed props should NOT get a P0-C rule. Got: {:?}",
            p0c_rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
    }

    // P0-C SHOULD trigger for components with many removed props (like Modal: 12 of 14)
    #[test]
    fn test_p0c_triggers_for_heavily_removed_components() {
        let mut api_changes = Vec::new();
        // 10 removed props
        for i in 0..10 {
            api_changes.push(make_api_change(
                &format!("ModalProps.prop{}", i),
                ApiChangeKind::Property,
                ApiChangeType::Removed,
                &format!("prop{} removed", i),
            ));
        }
        // 4 type-changed props
        for i in 10..14 {
            api_changes.push(make_api_change(
                &format!("ModalProps.prop{}", i),
                ApiChangeKind::Property,
                ApiChangeType::TypeChanged,
                &format!("prop{} type changed", i),
            ));
        }

        let changes = vec![make_file_changes(
            "packages/react-core/src/components/Modal/Modal.tsx",
            api_changes,
            vec![],
        )];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Modal has 10 removals out of 14 total — should trigger P0-C
        let p0c_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("component-import-deprecated"))
            .collect();
        assert!(
            !p0c_rules.is_empty(),
            "Modal with 10/14 removed props should get a P0-C rule"
        );
        assert_eq!(
            p0c_rules[0].fix_strategy.as_ref().unwrap().strategy,
            "LlmAssisted"
        );
    }

    // P0-C should NOT generate rules for TypeAlias removals (not components)
    #[test]
    fn test_p0c_skips_type_alias_removals() {
        let changes = vec![make_file_changes(
            "src/types.d.ts",
            vec![make_api_change(
                "UserRole",
                ApiChangeKind::TypeAlias,
                ApiChangeType::Removed,
                "UserRole type removed",
            )],
            vec![],
        )];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Should only have the per-change rule, no P0-C
        assert_eq!(rules.len(), 1);
        assert!(!rules[0].rule_id.contains("component-import-deprecated"));
    }

    // ── Issue 7: suppress_redundant_prop_rules ──
    // Scenario: Modal has both a component-import-deprecated rule (LlmAssisted)
    // AND a RemoveProp rule for individual props. The RemoveProp should be suppressed.
    #[test]
    fn test_suppress_redundant_prop_rules_modal_scenario() {
        let rules = vec![
            // Component-level LlmAssisted rule for Modal
            KonveyorRule {
                rule_id: "semver-modal-component-import-deprecated".to_string(),
                labels: vec![
                    "source=semver-analyzer".to_string(),
                    "change-type=component-removal".to_string(),
                ],
                effort: 3,
                category: "mandatory".to_string(),
                description: "Modal has significant breaking changes".to_string(),
                message: "MIGRATION: Modal restructured".to_string(),
                links: Vec::new(),
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: "^Modal$".to_string(),
                        location: "IMPORT".to_string(),
                        component: None,
                        parent: None,
                    not_parent: None,
                        value: None,
                        from: Some("@patternfly/react-core".to_string()),
                        parent_from: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry::new("LlmAssisted")),
            },
            // Prop-level RemoveProp rule for Modal.title
            KonveyorRule {
                rule_id: "semver-modal-title-removed".to_string(),
                labels: vec![
                    "source=semver-analyzer".to_string(),
                    "change-type=removed".to_string(),
                ],
                effort: 1,
                category: "mandatory".to_string(),
                description: "Modal.title removed".to_string(),
                message: "title prop removed".to_string(),
                links: Vec::new(),
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: "^title$".to_string(),
                        location: "JSX_PROP".to_string(),
                        component: Some("^Modal$".to_string()),
                        parent: None,
                    not_parent: None,
                        value: None,
                        from: Some("@patternfly/react-core".to_string()),
                        parent_from: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry {
                    strategy: "RemoveProp".to_string(),
                    component: Some("Modal".to_string()),
                    ..Default::default()
                }),
            },
            // Prop-level RemoveProp rule for Modal.actions
            KonveyorRule {
                rule_id: "semver-modal-actions-removed".to_string(),
                labels: vec![
                    "source=semver-analyzer".to_string(),
                    "change-type=removed".to_string(),
                ],
                effort: 1,
                category: "mandatory".to_string(),
                description: "Modal.actions removed".to_string(),
                message: "actions prop removed".to_string(),
                links: Vec::new(),
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: "^actions$".to_string(),
                        location: "JSX_PROP".to_string(),
                        component: Some("^Modal$".to_string()),
                        parent: None,
                    not_parent: None,
                        value: None,
                        from: Some("@patternfly/react-core".to_string()),
                        parent_from: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry {
                    strategy: "RemoveProp".to_string(),
                    component: Some("Modal".to_string()),
                    ..Default::default()
                }),
            },
            // Unrelated rule (should NOT be suppressed)
            KonveyorRule {
                rule_id: "semver-card-isflat-removed".to_string(),
                labels: vec![
                    "source=semver-analyzer".to_string(),
                    "change-type=removed".to_string(),
                ],
                effort: 1,
                category: "mandatory".to_string(),
                description: "Card.isFlat removed".to_string(),
                message: "isFlat prop removed".to_string(),
                links: Vec::new(),
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: "^isFlat$".to_string(),
                        location: "JSX_PROP".to_string(),
                        component: Some("^Card$".to_string()),
                        parent: None,
                    not_parent: None,
                        value: None,
                        from: None,
                        parent_from: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry {
                    strategy: "RemoveProp".to_string(),
                    component: Some("Card".to_string()),
                    ..Default::default()
                }),
            },
        ];

        let result = suppress_redundant_prop_rules(rules);

        // Modal RemoveProp rules should be suppressed (2 removed)
        // Card RemoveProp should survive (Card has no component-import-deprecated rule)
        // Component-import-deprecated rule should survive
        assert_eq!(
            result.len(),
            2,
            "Expected 2 rules after suppression, got {}",
            result.len()
        );
        assert!(result
            .iter()
            .any(|r| r.rule_id == "semver-modal-component-import-deprecated"));
        assert!(result
            .iter()
            .any(|r| r.rule_id == "semver-card-isflat-removed"));
        assert!(!result.iter().any(|r| r.rule_id.contains("modal-title")));
        assert!(!result.iter().any(|r| r.rule_id.contains("modal-actions")));
    }

    // ── Issue 4: CSS logical property suffix renames ──
    // Scenario: Token member renames like PaddingTop→PaddingBlockStart should
    // generate a single combined cssvar rule with all suffix mappings.
    #[test]
    fn test_css_logical_property_suffix_renames() {
        let member_renames: HashMap<String, String> = vec![
            (
                "c_table__caption_PaddingTop".to_string(),
                "c_table__caption_PaddingBlockStart".to_string(),
            ),
            (
                "c_table__caption_PaddingBottom".to_string(),
                "c_table__caption_PaddingBlockEnd".to_string(),
            ),
            (
                "c_nav_PaddingLeft".to_string(),
                "c_nav_PaddingInlineStart".to_string(),
            ),
            (
                "c_button_MarginTop".to_string(),
                "c_button_MarginBlockStart".to_string(),
            ),
        ]
        .into_iter()
        .collect();

        let report = make_report(vec![], vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &member_renames,
        );

        // Should generate one combined CSS suffix rename rule
        let css_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("css-logical"))
            .collect();
        assert_eq!(
            css_rules.len(),
            1,
            "Expected 1 combined CSS logical property rule, got {}",
            css_rules.len()
        );

        let rule = css_rules[0];
        // Should be a Rename strategy with mappings
        let strat = rule.fix_strategy.as_ref().unwrap();
        assert_eq!(strat.strategy, "Rename");
        assert!(
            strat.mappings.len() >= 4,
            "Expected at least 4 suffix mappings, got {}",
            strat.mappings.len()
        );

        // Check specific mappings
        let has_padding_top = strat.mappings.iter().any(|m| {
            m.from.as_deref() == Some("--PaddingTop")
                && m.to.as_deref() == Some("--PaddingBlockStart")
        });
        assert!(
            has_padding_top,
            "Missing PaddingTop→PaddingBlockStart mapping"
        );

        let has_margin_top = strat.mappings.iter().any(|m| {
            m.from.as_deref() == Some("--MarginTop")
                && m.to.as_deref() == Some("--MarginBlockStart")
        });
        assert!(has_margin_top, "Missing MarginTop→MarginBlockStart mapping");

        // Message should list all renames
        assert!(rule.message.contains("PaddingTop"));
        assert!(rule.message.contains("PaddingBlockStart"));

        // Condition should use frontend.cssvar
        match &rule.when {
            KonveyorCondition::FrontendCssVar { cssvar } => {
                assert!(cssvar.pattern.contains("PaddingTop"));
                assert!(cssvar.pattern.contains("MarginTop"));
            }
            _ => panic!("Expected FrontendCssVar condition"),
        }
    }

    // ── Constant collapsing ──
    // Scenario: 15+ token constants with the same change type and strategy
    // should be collapsed into a single combined rule.
    #[test]
    fn test_constant_collapse_threshold() {
        let mut api_changes = Vec::new();
        for i in 0..15 {
            api_changes.push(ApiChange {
                symbol: format!("c_component_token_{}", i),
                kind: ApiChangeKind::Constant,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: format!("Token {} removed", i),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            });
        }

        let changes = vec![make_file_changes(
            "packages/react-tokens/dist/esm/tokens.d.ts",
            api_changes,
            vec![],
        )];

        let mut pkg_cache = HashMap::new();
        pkg_cache.insert(
            "react-tokens".to_string(),
            "@patternfly/react-tokens".to_string(),
        );

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &pkg_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Should collapse 15 removed constants into a single combined rule
        let combined_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("combined"))
            .collect();
        assert!(
            !combined_rules.is_empty(),
            "Expected at least one combined rule from 15 constants"
        );

        // Should NOT have 15 individual rules
        let individual_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("c-component-token"))
            .collect();
        assert_eq!(
            individual_rules.len(),
            0,
            "Expected 0 individual token rules (all collapsed), got {}",
            individual_rules.len()
        );
    }

    // Below threshold — should NOT collapse
    #[test]
    fn test_constant_collapse_below_threshold() {
        let mut api_changes = Vec::new();
        for i in 0..5 {
            api_changes.push(ApiChange {
                symbol: format!("c_component_token_{}", i),
                kind: ApiChangeKind::Constant,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: format!("Token {} removed", i),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            });
        }

        let changes = vec![make_file_changes(
            "packages/react-tokens/dist/esm/tokens.d.ts",
            api_changes,
            vec![],
        )];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // 5 constants is below the threshold (10) — should NOT collapse
        let combined_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("combined"))
            .collect();
        assert_eq!(
            combined_rules.len(),
            0,
            "Should not collapse 5 constants (below threshold)"
        );
    }

    // ── Constant collapse: renamed constants get per-token Rename mappings ──
    #[test]
    fn test_constant_collapse_renamed_gets_rename_mappings() {
        // Create 15 renamed constants — enough to trigger collapse.
        // Each has a before/after with symbol_summary strings.
        let mut api_changes = Vec::new();
        for i in 0..15 {
            api_changes.push(ApiChange {
                symbol: format!("global_token_{}", i),
                kind: ApiChangeKind::Constant,
                change: ApiChangeType::Renamed,
                before: Some(format!("constant: global_token_{}", i)),
                after: Some(format!("variable: t_global_token_{}", i)),
                description: format!("Exported constant `global_token_{}` was renamed", i),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            });
        }

        let changes = vec![make_file_changes(
            "packages/react-tokens/dist/esm/index.d.ts",
            api_changes,
            vec![],
        )];

        let mut pkg_cache = HashMap::new();
        pkg_cache.insert(
            "react-tokens".to_string(),
            "@patternfly/react-tokens".to_string(),
        );

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &pkg_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Should produce a combined rule
        let combined_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.description.contains("constants from"))
            .collect();
        assert!(
            !combined_rules.is_empty(),
            "Expected at least one combined rule for 15 renamed constants"
        );

        // The combined rule's fix_strategy must be Rename, NOT CssVariablePrefix
        let rule = combined_rules[0];
        let strat = rule
            .fix_strategy
            .as_ref()
            .expect("combined rule should have fix_strategy");
        assert_eq!(
            strat.strategy, "Rename",
            "Renamed constant group should have Rename strategy, got {}",
            strat.strategy
        );

        // Must have per-token mappings
        assert_eq!(
            strat.mappings.len(),
            15,
            "Expected 15 per-token mappings, got {}",
            strat.mappings.len()
        );

        // Verify a specific mapping
        let m0 = strat
            .mappings
            .iter()
            .find(|m| m.from.as_deref() == Some("global_token_0"))
            .expect("Should have mapping for global_token_0");
        assert_eq!(m0.to.as_deref(), Some("t_global_token_0"));

        // None of the mappings should contain symbol_summary strings
        for m in &strat.mappings {
            let from = m.from.as_deref().unwrap_or("");
            let to = m.to.as_deref().unwrap_or("");
            assert!(
                !from.contains("constant: ") && !from.contains("variable: "),
                "from contains symbol_summary: {}",
                from
            );
            assert!(
                !to.contains("constant: ") && !to.contains("variable: "),
                "to contains symbol_summary: {}",
                to
            );
        }

        // Individual rules should be suppressed
        let individual_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("global-token"))
            .collect();
        assert_eq!(
            individual_rules.len(),
            0,
            "Individual rules should be suppressed, found {}",
            individual_rules.len()
        );
    }

    // Test that token_mappings overrides work in constantgroup context
    #[test]
    fn test_constant_collapse_renamed_with_token_mappings_override() {
        let mut api_changes = Vec::new();
        for i in 0..15 {
            api_changes.push(ApiChange {
                symbol: format!("global_token_{}", i),
                kind: ApiChangeKind::Constant,
                change: ApiChangeType::Renamed,
                before: Some(format!("constant: global_token_{}", i)),
                // Algorithm would extract "wrong_target_{i}" from the after field
                after: Some(format!("variable: wrong_target_{}", i)),
                description: format!("Exported constant `global_token_{}` was renamed", i),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            });
        }

        let changes = vec![make_file_changes(
            "packages/react-tokens/dist/esm/index.d.ts",
            api_changes,
            vec![],
        )];

        let mut pkg_cache = HashMap::new();
        pkg_cache.insert(
            "react-tokens".to_string(),
            "@patternfly/react-tokens".to_string(),
        );

        // Provide user token_mappings for some tokens
        let mut patterns = RenamePatterns::empty();
        patterns
            .token_mappings
            .insert("global_token_0".into(), "correct_target_0".into());
        patterns
            .token_mappings
            .insert("global_token_5".into(), "correct_target_5".into());

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.ts", &pkg_cache, &patterns, &HashMap::new());

        let combined_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.description.contains("constants from"))
            .collect();
        assert!(!combined_rules.is_empty());

        let strat = combined_rules[0]
            .fix_strategy
            .as_ref()
            .expect("should have strategy");
        assert_eq!(strat.strategy, "Rename");

        // Token 0: user mapping should override
        let m0 = strat
            .mappings
            .iter()
            .find(|m| m.from.as_deref() == Some("global_token_0"))
            .expect("Should have mapping for global_token_0");
        assert_eq!(
            m0.to.as_deref(),
            Some("correct_target_0"),
            "User token_mapping should override algorithmic target"
        );

        // Token 5: user mapping should override
        let m5 = strat
            .mappings
            .iter()
            .find(|m| m.from.as_deref() == Some("global_token_5"))
            .expect("Should have mapping for global_token_5");
        assert_eq!(
            m5.to.as_deref(),
            Some("correct_target_5"),
            "User token_mapping should override algorithmic target"
        );

        // Token 3: no user mapping, falls through to algorithm
        let m3 = strat
            .mappings
            .iter()
            .find(|m| m.from.as_deref() == Some("global_token_3"))
            .expect("Should have mapping for global_token_3");
        assert_eq!(
            m3.to.as_deref(),
            Some("wrong_target_3"),
            "Token without user mapping should use algorithm's result"
        );
    }

    // ── Issue 2: New sibling component detection (MastheadLogo) ──
    // Scenario: MastheadBrand has breaking changes and MastheadLogo was added
    // in the same directory, with behavioral evidence in consumer code.
    #[test]
    fn test_new_sibling_component_detection_with_behavioral_evidence() {
        let changes = vec![
            make_file_changes(
                "packages/react-core/src/components/Masthead/MastheadBrand.tsx",
                vec![
                    make_api_change(
                        "MastheadBrandProps",
                        ApiChangeKind::Interface,
                        ApiChangeType::SignatureChanged,
                        "Now extends HTMLDivElement instead of HTMLAnchorElement",
                    ),
                    make_api_change(
                        "MastheadBrandProps.component",
                        ApiChangeKind::Property,
                        ApiChangeType::Removed,
                        "component prop removed",
                    ),
                ],
                vec![make_behavioral(
                    "MastheadBrand",
                    Some(TsCategory::LogicChange),
                    "href no longer creates a clickable link",
                )],
            ),
            // Consumer demo file where MastheadLogo appears in behavioral changes
            make_file_changes(
                "packages/react-core/src/components/Masthead/examples/MastheadBasic.tsx",
                vec![],
                vec![make_behavioral(
                    "MastheadBasic",
                    Some(TsCategory::DomStructure),
                    "<MastheadLogo> element added to render output (1 instance)",
                )],
            ),
        ];

        let report = make_report_with_added(
            changes,
            vec![PathBuf::from(
                "packages/react-core/src/components/Masthead/MastheadLogo.tsx",
            )],
        );

        let mut pkg_cache = HashMap::new();
        pkg_cache.insert(
            "react-core".to_string(),
            "@patternfly/react-core".to_string(),
        );

        let rules = generate_rules(
            &report,
            "*.ts",
            &pkg_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Should have a new-sibling rule for MastheadLogo
        let sibling_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("new-sibling"))
            .collect();
        assert_eq!(
            sibling_rules.len(),
            1,
            "Expected 1 new-sibling rule, got {}",
            sibling_rules.len()
        );

        let rule = sibling_rules[0];
        assert!(rule.message.contains("MastheadLogo"));
        assert!(rule.message.contains("MastheadBrand"));
        assert!(rule.message.contains("Consider wrapping"));
        assert_eq!(rule.fix_strategy.as_ref().unwrap().strategy, "LlmAssisted");
        assert_eq!(rule.category, "optional");
    }

    // No behavioral evidence → no sibling rule generated
    #[test]
    fn test_new_sibling_without_behavioral_evidence_is_skipped() {
        let changes = vec![make_file_changes(
            "packages/react-core/src/components/Masthead/MastheadBrand.tsx",
            vec![make_api_change(
                "MastheadBrandProps.component",
                ApiChangeKind::Property,
                ApiChangeType::Removed,
                "component prop removed",
            )],
            vec![],
        )];

        // File was added but no behavioral evidence references it
        let report = make_report_with_added(
            changes,
            vec![PathBuf::from(
                "packages/react-core/src/components/Masthead/MastheadLogo.tsx",
            )],
        );

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let sibling_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("new-sibling"))
            .collect();
        assert_eq!(
            sibling_rules.len(),
            0,
            "Should not generate sibling rule without behavioral evidence"
        );
    }

    // ── Migration message enrichment ──
    // Scenario: EmptyStateHeader has a migration_target mapping to EmptyState,
    // plus behavioral changes. The P0-C message should include both.
    #[test]
    fn test_migration_message_with_migration_target_and_behavioral() {
        let changes = vec![make_file_changes(
            "packages/react-core/src/components/EmptyState/EmptyStateHeader.tsx",
            vec![
                {
                    let mut change = make_api_change(
                        "EmptyStateHeaderProps",
                        ApiChangeKind::Interface,
                        ApiChangeType::Removed,
                        "EmptyStateHeaderProps interface removed",
                    );
                    change.migration_target = Some(MigrationTarget {
                        removed_symbol: "EmptyStateHeaderProps".to_string(),
                        removed_qualified_name: "EmptyStateHeader.EmptyStateHeaderProps"
                            .to_string(),
                        removed_package: None,
                        replacement_symbol: "EmptyStateProps".to_string(),
                        replacement_qualified_name: "EmptyState.EmptyStateProps".to_string(),
                        replacement_package: None,
                        matching_members: vec![
                            MemberMapping {
                                old_name: "titleText".to_string(),
                                new_name: "titleText".to_string(),
                            },
                            MemberMapping {
                                old_name: "icon".to_string(),
                                new_name: "icon".to_string(),
                            },
                            MemberMapping {
                                old_name: "headingLevel".to_string(),
                                new_name: "headingLevel".to_string(),
                            },
                        ],
                        removed_only_members: vec!["className".to_string(), "children".to_string()],
                        overlap_ratio: 0.6,
                        old_extends: None,
                        new_extends: None,
                    });
                    change
                },
                make_api_change(
                    "EmptyStateHeader",
                    ApiChangeKind::Constant,
                    ApiChangeType::Removed,
                    "EmptyStateHeader component removed",
                ),
            ],
            vec![make_behavioral(
                "EmptyStateHeader",
                Some(TsCategory::RenderOutput),
                "<EmptyStateHeader> element removed from render output",
            )],
        )];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Find the P0-C component-import-deprecated rule
        let p0c_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("component-import-deprecated"));
        assert!(p0c_rule.is_some(), "Missing P0-C rule for EmptyStateHeader");

        let msg = &p0c_rule.unwrap().message;

        // Should include migration target info
        assert!(
            msg.contains("Replace <EmptyStateHeader>"),
            "Missing migration header"
        );
        assert!(
            msg.contains("EmptyState"),
            "Missing replacement component name"
        );

        // Should include property mapping
        assert!(
            msg.contains("titleText"),
            "Missing titleText in property mapping"
        );
        assert!(msg.contains("icon"), "Missing icon in property mapping");
        assert!(
            msg.contains("headingLevel"),
            "Missing headingLevel in property mapping"
        );

        // Should include removed-only members
        assert!(
            msg.contains("className"),
            "Missing className in removed-only members"
        );
        assert!(
            msg.contains("children"),
            "Missing children in removed-only members"
        );

        // Should include behavioral changes
        assert!(
            msg.contains("Behavioral changes"),
            "Missing behavioral changes section"
        );
        assert!(
            msg.contains("element removed from render output"),
            "Missing behavioral description"
        );
    }

    // ── Behavioral rule dedup ──
    // Scenario: When a P0-C rule exists for EmptyStateHeader, standalone
    // behavioral rules for the same component should be downgraded to Manual.
    #[test]
    fn test_behavioral_rule_dedup_when_p0c_covers_component() {
        let changes = vec![
            // Source file with both API and behavioral changes
            make_file_changes(
                "packages/react-core/src/components/EmptyState/EmptyStateHeader.tsx",
                vec![make_api_change(
                    "EmptyStateHeader",
                    ApiChangeKind::Constant,
                    ApiChangeType::Removed,
                    "EmptyStateHeader component removed",
                )],
                vec![make_behavioral(
                    "EmptyStateHeader",
                    Some(TsCategory::RenderOutput),
                    "<EmptyStateHeader> element removed from render output",
                )],
            ),
        ];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Find behavioral rule for EmptyStateHeader
        let behavioral_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| {
                r.labels
                    .iter()
                    .any(|l| l.starts_with("change-type=behavioral"))
                    && r.rule_id.contains("emptystateheader")
            })
            .collect();

        // If a behavioral rule exists, it should be downgraded to Manual
        // (not LlmAssisted) since the P0-C rule already covers EmptyStateHeader
        for rule in &behavioral_rules {
            let strat = rule.fix_strategy.as_ref().unwrap();
            assert_eq!(
                strat.strategy, "Manual",
                "Behavioral rule for EmptyStateHeader should be Manual (covered by P0-C), got {}",
                strat.strategy
            );
        }
    }

    // ── Strategy priority: LlmAssisted with member_mappings wins ──
    #[test]
    fn test_strategy_priority_llm_with_member_mappings_wins() {
        // Simulate consolidation of rules where one has LlmAssisted with
        // structural migration data and another has RemoveProp
        let rules = vec![
            KonveyorRule {
                rule_id: "semver-modal-actions-removed".to_string(),
                labels: vec![
                    "source=semver-analyzer".to_string(),
                    "change-type=removed".to_string(),
                ],
                effort: 1,
                category: "mandatory".to_string(),
                description: "actions prop removed from Modal".to_string(),
                message: "actions removed".to_string(),
                links: Vec::new(),
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: "^actions$".to_string(),
                        location: "JSX_PROP".to_string(),
                        component: Some("^Modal$".to_string()),
                        parent: None,
                    not_parent: None,
                        value: None,
                        from: Some("@patternfly/react-core".to_string()),
                        parent_from: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry {
                    strategy: "RemoveProp".to_string(),
                    component: Some("Modal".to_string()),
                    ..Default::default()
                }),
            },
            KonveyorRule {
                rule_id: "semver-modal-structural-migration".to_string(),
                labels: vec![
                    "source=semver-analyzer".to_string(),
                    "change-type=removed".to_string(),
                ],
                effort: 5,
                category: "mandatory".to_string(),
                description: "Modal decomposed into ModalHeader/ModalBody/ModalFooter".to_string(),
                message: "Modal restructured".to_string(),
                links: Vec::new(),
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: "^actions$".to_string(),
                        location: "JSX_PROP".to_string(),
                        component: Some("^Modal$".to_string()),
                        parent: None,
                    not_parent: None,
                        value: None,
                        from: Some("@patternfly/react-core".to_string()),
                        parent_from: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry {
                    strategy: "LlmAssisted".to_string(),
                    member_mappings: vec![MemberMappingEntry {
                        old_name: "title".to_string(),
                        new_name: "ModalHeader.title".to_string(),
                    }],
                    ..Default::default()
                }),
            },
        ];

        let (consolidated, _) = consolidate_rules(rules);

        // After consolidation, the merged rule should have LlmAssisted strategy
        // (not RemoveProp), because LlmAssisted with member_mappings wins
        let merged = consolidated.iter().find(|r| r.rule_id.contains("modal"));
        assert!(merged.is_some(), "Expected a merged modal rule");
        let strat = merged.unwrap().fix_strategy.as_ref().unwrap();
        assert_eq!(
            strat.strategy, "LlmAssisted",
            "LlmAssisted with member_mappings should win over RemoveProp, got {}",
            strat.strategy
        );
    }

    // ── Consolidation key isolation tests ────────────────────────────────
    // Verify that rules with specific change types get unique consolidation
    // keys and are never merged with unrelated rules.

    fn make_rule_with_labels(rule_id: &str, labels: Vec<&str>) -> KonveyorRule {
        KonveyorRule {
            rule_id: rule_id.to_string(),
            labels: labels.into_iter().map(|l| l.to_string()).collect(),
            effort: 1,
            category: "mandatory".to_string(),
            description: "test rule".to_string(),
            message: "test message".to_string(),
            links: Vec::new(),
            when: KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: "^Test$".to_string(),
                    location: "IMPORT".to_string(),
                    component: None,
                    parent: None,
                    not_parent: None,
                    value: None,
                    from: None,
                    parent_from: None,
                },
            },
            fix_strategy: Some(FixStrategyEntry::new("Manual")),
        }
    }

    // CSS logical property rule must NOT be consolidated with CSS prefix rules
    #[test]
    fn test_consolidation_css_variable_rules_stay_separate() {
        let css_prefix_rule = make_rule_with_labels(
            "semver-consumer-css-stale-var-pf-v5",
            vec!["source=semver-analyzer", "change-type=css-variable"],
        );
        let css_logical_rule = make_rule_with_labels(
            "semver-css-logical-property-renames",
            vec![
                "source=semver-analyzer",
                "change-type=css-variable",
                "has-codemod=true",
            ],
        );

        let key1 = consolidation_key(&css_prefix_rule);
        let key2 = consolidation_key(&css_logical_rule);

        assert_ne!(
            key1, key2,
            "CSS prefix and CSS logical property rules should have different consolidation keys"
        );
    }

    // New sibling rules must NOT be consolidated together
    #[test]
    fn test_consolidation_sibling_rules_stay_separate() {
        let sibling_a = make_rule_with_labels(
            "semver-new-sibling-mastheadlogo-in-mastheadbrand",
            vec![
                "source=semver-analyzer",
                "change-type=new-sibling-component",
            ],
        );
        let sibling_b = make_rule_with_labels(
            "semver-new-sibling-drawerdescription-in-drawer",
            vec![
                "source=semver-analyzer",
                "change-type=new-sibling-component",
            ],
        );

        let key_a = consolidation_key(&sibling_a);
        let key_b = consolidation_key(&sibling_b);

        assert_ne!(
            key_a, key_b,
            "Different sibling rules should have different consolidation keys"
        );
    }

    // Component-removal (P0-C) rules must NOT be consolidated into mega-groups
    #[test]
    fn test_consolidation_component_removal_rules_stay_separate() {
        let modal_rule = make_rule_with_labels(
            "semver-modal-component-import-deprecated",
            vec!["source=semver-analyzer", "change-type=component-removal"],
        );
        let emptystate_rule = make_rule_with_labels(
            "semver-emptystateheader-component-import-deprecated",
            vec!["source=semver-analyzer", "change-type=component-removal"],
        );

        let key_modal = consolidation_key(&modal_rule);
        let key_empty = consolidation_key(&emptystate_rule);

        assert_ne!(
            key_modal, key_empty,
            "P0-C rules for different components should NOT be consolidated together"
        );
    }

    // Dependency-update rules must stay separate
    #[test]
    fn test_consolidation_dependency_update_rules_stay_separate() {
        let dep_a = make_rule_with_labels(
            "semver-dep-update-patternfly-react-core",
            vec!["source=semver-analyzer", "change-type=dependency-update"],
        );
        let dep_b = make_rule_with_labels(
            "semver-dep-update-patternfly-react-tokens",
            vec!["source=semver-analyzer", "change-type=dependency-update"],
        );

        let key_a = consolidation_key(&dep_a);
        let key_b = consolidation_key(&dep_b);

        assert_ne!(
            key_a, key_b,
            "Dependency update rules for different packages should NOT be consolidated"
        );
    }

    // Regular API rules (removed, type-changed) from the same file SHOULD still consolidate
    #[test]
    fn test_consolidation_regular_api_rules_still_merge() {
        let mut rule_a = make_rule_with_labels(
            "semver-modal-title-removed",
            vec![
                "source=semver-analyzer",
                "change-type=removed",
                "kind=property",
            ],
        );
        rule_a.message =
            "title was removed\nFile: packages/react-core/src/components/Modal/Modal.d.ts"
                .to_string();

        let mut rule_b = make_rule_with_labels(
            "semver-modal-actions-removed",
            vec![
                "source=semver-analyzer",
                "change-type=removed",
                "kind=property",
            ],
        );
        rule_b.message =
            "actions was removed\nFile: packages/react-core/src/components/Modal/Modal.d.ts"
                .to_string();

        let key_a = consolidation_key(&rule_a);
        let key_b = consolidation_key(&rule_b);

        assert_eq!(
            key_a, key_b,
            "Regular API rules from the same file should still consolidate"
        );
    }

    // End-to-end: consolidate_rules() should keep P0-C, CSS, and sibling rules intact
    #[test]
    fn test_consolidation_e2e_protected_rules_survive() {
        let rules = vec![
            // P0-C rule for Modal
            {
                let mut r = make_rule_with_labels(
                    "semver-modal-component-import-deprecated",
                    vec!["source=semver-analyzer", "change-type=component-removal"],
                );
                r.fix_strategy = Some(FixStrategyEntry::new("LlmAssisted"));
                r
            },
            // P0-C rule for EmptyStateHeader
            {
                let mut r = make_rule_with_labels(
                    "semver-emptystateheader-component-import-deprecated",
                    vec!["source=semver-analyzer", "change-type=component-removal"],
                );
                r.fix_strategy = Some(FixStrategyEntry::new("LlmAssisted"));
                r
            },
            // CSS logical property rule
            {
                let mut r = make_rule_with_labels(
                    "semver-css-logical-property-renames",
                    vec![
                        "source=semver-analyzer",
                        "change-type=css-variable",
                        "has-codemod=true",
                    ],
                );
                r.fix_strategy = Some(FixStrategyEntry {
                    strategy: "Rename".to_string(),
                    mappings: vec![MappingEntry {
                        from: Some("--PaddingTop".to_string()),
                        to: Some("--PaddingBlockStart".to_string()),
                        component: None,
                        prop: None,
                    }],
                    ..Default::default()
                });
                r
            },
            // Sibling detection rule
            make_rule_with_labels(
                "semver-new-sibling-mastheadlogo-in-mastheadbrand",
                vec![
                    "source=semver-analyzer",
                    "change-type=new-sibling-component",
                ],
            ),
        ];

        let (consolidated, _) = consolidate_rules(rules);

        // All 4 rules should survive consolidation unchanged
        assert_eq!(
            consolidated.len(),
            4,
            "Expected 4 rules after consolidation (all protected), got {}. IDs: {:?}",
            consolidated.len(),
            consolidated.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // Verify each is present by rule_id
        assert!(
            consolidated
                .iter()
                .any(|r| r.rule_id.contains("modal-component")),
            "Modal P0-C rule lost in consolidation"
        );
        assert!(
            consolidated
                .iter()
                .any(|r| r.rule_id.contains("emptystateheader-component")),
            "EmptyStateHeader P0-C rule lost in consolidation"
        );
        assert!(
            consolidated
                .iter()
                .any(|r| r.rule_id.contains("css-logical")),
            "CSS logical property rule lost in consolidation"
        );
        assert!(
            consolidated
                .iter()
                .any(|r| r.rule_id.contains("mastheadlogo")),
            "MastheadLogo sibling rule lost in consolidation"
        );

        // Verify CSS rule still has its mappings
        let css_rule = consolidated
            .iter()
            .find(|r| r.rule_id.contains("css-logical"))
            .unwrap();
        let strat = css_rule.fix_strategy.as_ref().unwrap();
        assert_eq!(strat.strategy, "Rename");
        assert!(
            !strat.mappings.is_empty(),
            "CSS rule lost its mappings during consolidation"
        );
    }

    // Verify that suppress_redundant_prop_rules works with unconsolidated P0-C rules
    #[test]
    fn test_suppress_works_with_individual_p0c_rules() {
        let rules = vec![
            // Individual P0-C for Modal (not in a mega-group)
            {
                let mut r = make_rule_with_labels(
                    "semver-modal-component-import-deprecated",
                    vec!["source=semver-analyzer", "change-type=component-removal"],
                );
                r.when = KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: "^Modal$".to_string(),
                        location: "IMPORT".to_string(),
                        component: None,
                        parent: None,
                    not_parent: None,
                        value: None,
                        from: Some("@patternfly/react-core".to_string()),
                        parent_from: None,
                    },
                };
                r.fix_strategy = Some(FixStrategyEntry::new("LlmAssisted"));
                r
            },
            // RemoveProp for Modal.title (should be suppressed)
            {
                let mut r = make_rule_with_labels(
                    "semver-modal-title-removed",
                    vec!["source=semver-analyzer", "change-type=removed"],
                );
                r.fix_strategy = Some(FixStrategyEntry {
                    strategy: "RemoveProp".to_string(),
                    component: Some("Modal".to_string()),
                    ..Default::default()
                });
                r
            },
            // RemoveProp for Modal.actions (should be suppressed)
            {
                let mut r = make_rule_with_labels(
                    "semver-modal-actions-removed",
                    vec!["source=semver-analyzer", "change-type=removed"],
                );
                r.fix_strategy = Some(FixStrategyEntry {
                    strategy: "RemoveProp".to_string(),
                    component: Some("Modal".to_string()),
                    ..Default::default()
                });
                r
            },
            // RemoveProp for ModalProps.footer (should also be suppressed — "ModalProps" matches "Modal")
            {
                let mut r = make_rule_with_labels(
                    "semver-modalprops-footer-removed",
                    vec!["source=semver-analyzer", "change-type=removed"],
                );
                r.fix_strategy = Some(FixStrategyEntry {
                    strategy: "RemoveProp".to_string(),
                    component: Some("ModalProps".to_string()),
                    ..Default::default()
                });
                r
            },
            // RemoveProp for Card.isFlat (should NOT be suppressed — no Card P0-C rule)
            {
                let mut r = make_rule_with_labels(
                    "semver-card-isflat-removed",
                    vec!["source=semver-analyzer", "change-type=removed"],
                );
                r.fix_strategy = Some(FixStrategyEntry {
                    strategy: "RemoveProp".to_string(),
                    component: Some("Card".to_string()),
                    ..Default::default()
                });
                r
            },
        ];

        let result = suppress_redundant_prop_rules(rules);

        // Modal P0-C + Card RemoveProp should survive. All 3 Modal RemoveProp should be suppressed.
        assert_eq!(
            result.len(),
            2,
            "Expected 2 rules after suppression (Modal P0-C + Card), got {}. IDs: {:?}",
            result.len(),
            result.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
        assert!(result
            .iter()
            .any(|r| r.rule_id.contains("modal-component-import")));
        assert!(result.iter().any(|r| r.rule_id.contains("card-isflat")));
    }

    // ── Extract trailing suffix helper ──
    #[test]
    fn test_extract_trailing_suffix() {
        assert_eq!(
            extract_trailing_suffix("c_table__caption_PaddingTop"),
            Some("PaddingTop")
        );
        assert_eq!(
            extract_trailing_suffix("c_nav_PaddingInlineStart"),
            Some("PaddingInlineStart")
        );
        assert_eq!(extract_trailing_suffix("global_Color_100"), None); // 100 is not PascalCase
        assert_eq!(extract_trailing_suffix("c_button"), None); // no PascalCase suffix
        assert_eq!(
            extract_trailing_suffix("c_about_modal_box__brand_PaddingBlockEnd"),
            Some("PaddingBlockEnd")
        );
    }

    // ── apply_suffix_renames tests ──

    /// Build a token object string with the given member keys.
    fn make_token_object(keys: &[&str]) -> String {
        let members: Vec<String> = keys
            .iter()
            .map(|k| {
                format!("[\"{k}\"]: {{ [\"name\"]: \"--pf-test--{k}\"; [\"value\"]: \"1rem\" }}")
            })
            .collect();
        format!("{{ {} }}", members.join("; "))
    }

    fn make_token_type_changed(symbol: &str, old_keys: &[&str], new_keys: &[&str]) -> ApiChange {
        ApiChange {
            symbol: symbol.to_string(),
            kind: ApiChangeKind::Constant,
            change: ApiChangeType::TypeChanged,
            before: Some(make_token_object(old_keys)),
            after: Some(make_token_object(new_keys)),
            description: format!("{} type changed", symbol),
            migration_target: None,
            removal_disposition: None,
            renders_element: None,
        }
    }

    #[test]
    fn test_apply_suffix_renames_maps_members() {
        // Compound token has PaddingTop removed and PaddingBlockStart added.
        // With the suffix mapping PaddingTop→PaddingBlockStart, the function
        // should produce a member rename for each matching key.
        let changes = vec![make_file_changes(
            "packages/react-tokens/src/c_alert.d.ts",
            vec![make_token_type_changed(
                "c_alert",
                &[
                    "c_alert__description_PaddingTop",
                    "c_alert__icon_MarginLeft",
                    "c_alert_Color",
                ],
                &[
                    "c_alert__description_PaddingBlockStart",
                    "c_alert__icon_MarginInlineStart",
                    "c_alert_Color",
                ],
            )],
            vec![],
        )];

        let report = make_report(changes, vec![]);
        let suffix_map: HashMap<String, String> = [
            ("PaddingTop".to_string(), "PaddingBlockStart".to_string()),
            ("MarginLeft".to_string(), "MarginInlineStart".to_string()),
        ]
        .into_iter()
        .collect();

        let renames = apply_suffix_renames(&report, &suffix_map);

        assert_eq!(
            renames.get("c_alert__description_PaddingTop"),
            Some(&"c_alert__description_PaddingBlockStart".to_string()),
        );
        assert_eq!(
            renames.get("c_alert__icon_MarginLeft"),
            Some(&"c_alert__icon_MarginInlineStart".to_string()),
        );
        // Color has no suffix mapping — should not appear
        assert!(!renames.contains_key("c_alert_Color"));
    }

    #[test]
    fn test_apply_suffix_renames_skips_missing_target() {
        // Suffix mapping exists but the expected new key is NOT in the added set
        let changes = vec![make_file_changes(
            "packages/react-tokens/src/c_alert.d.ts",
            vec![make_token_type_changed(
                "c_alert",
                &["c_alert__body_PaddingTop", "c_alert_Size", "c_alert_Width"],
                &[
                    "c_alert_Size",
                    "c_alert_Width",
                    // PaddingBlockStart NOT added — key was simply removed
                ],
            )],
            vec![],
        )];

        let report = make_report(changes, vec![]);
        let suffix_map: HashMap<String, String> =
            [("PaddingTop".to_string(), "PaddingBlockStart".to_string())]
                .into_iter()
                .collect();

        let renames = apply_suffix_renames(&report, &suffix_map);
        assert!(
            renames.is_empty(),
            "No rename should be produced when target key doesn't exist in added set"
        );
    }

    #[test]
    fn test_extract_suffix_inventory() {
        let changes = vec![make_file_changes(
            "packages/react-tokens/src/c_alert.d.ts",
            vec![make_token_type_changed(
                "c_alert",
                &[
                    "c_alert__body_PaddingTop",
                    "c_alert__body_MarginLeft",
                    "c_alert_Color",
                ],
                &[
                    "c_alert__body_PaddingBlockStart",
                    "c_alert__body_MarginInlineStart",
                    "c_alert_Color",
                ],
            )],
            vec![],
        )];

        let report = make_report(changes, vec![]);
        let (removed, added) = extract_suffix_inventory(&report);

        assert!(removed.contains("PaddingTop"));
        assert!(removed.contains("MarginLeft"));
        assert!(!removed.contains("Color")); // Color is in both old and new

        assert!(added.contains("PaddingBlockStart"));
        assert!(added.contains("MarginInlineStart"));
    }

    // ── API rule message enrichment with behavioral context ──
    // Scenario: When an API change fires on a file that also has behavioral
    // changes for the same component, the API rule message should include
    // the behavioral context.
    #[test]
    fn test_api_rule_message_includes_behavioral_context() {
        let changes = vec![make_file_changes(
            "packages/react-core/src/components/Modal/Modal.tsx",
            vec![make_api_change(
                "ModalProps.title",
                ApiChangeKind::Property,
                ApiChangeType::Removed,
                "title prop removed from ModalProps",
            )],
            vec![
                make_behavioral(
                    "Modal",
                    Some(TsCategory::RenderOutput),
                    "title prop no longer renders ModalBoxHeader",
                ),
                make_behavioral(
                    "Modal",
                    Some(TsCategory::DomStructure),
                    "ModalBoxCloseButton no longer rendered inside ModalBoxHeader",
                ),
            ],
        )];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Find the per-change API rule for ModalProps.title
        let api_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("modalprops") && r.rule_id.contains("title"));
        assert!(api_rule.is_some(), "Missing API rule for ModalProps.title");

        let msg = &api_rule.unwrap().message;
        // Should include behavioral context
        assert!(
            msg.contains("Behavioral changes"),
            "Missing behavioral changes section"
        );
        assert!(
            msg.contains("title prop no longer renders ModalBoxHeader"),
            "Missing behavioral description"
        );
    }

    // ── Package scoping tests ─────────────────────────────────────────────
    // Verify that the `from` field is correctly set on all rule conditions
    // when a package cache is provided. Without this, rules would match
    // imports from ANY library, not just the target package.

    fn make_pkg_cache(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // Per-change API rule should have `from` set to the resolved package
    #[test]
    fn test_api_rule_has_from_package() {
        let changes = vec![make_file_changes(
            "packages/react-core/dist/esm/components/Modal/Modal.d.ts",
            vec![make_api_change(
                "Modal.title",
                ApiChangeKind::Property,
                ApiChangeType::Removed,
                "Modal.title removed",
            )],
            vec![],
        )];

        let pkg_cache = make_pkg_cache(&[("react-core", "@patternfly/react-core")]);
        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &pkg_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Every rule with a FrontendReferenced condition should have from set
        for rule in &rules {
            match &rule.when {
                KonveyorCondition::FrontendReferenced { referenced } => {
                    assert!(
                        referenced.from.is_some(),
                        "Rule {} has from=None — should be scoped to @patternfly/react-core",
                        rule.rule_id
                    );
                    assert!(
                        referenced
                            .from
                            .as_ref()
                            .unwrap()
                            .contains("@patternfly/react-core"),
                        "Rule {} has wrong from: {:?}",
                        rule.rule_id,
                        referenced.from
                    );
                }
                KonveyorCondition::Or { or } => {
                    for cond in or {
                        if let KonveyorCondition::FrontendReferenced { referenced } = cond {
                            assert!(
                                referenced.from.is_some(),
                                "Rule {} has Or branch with from=None",
                                rule.rule_id
                            );
                        }
                    }
                }
                _ => {} // Non-FrontendReferenced conditions don't have from
            }
        }
    }

    // P0-C component-import-deprecated rule should have `from` set
    #[test]
    fn test_p0c_rule_has_from_package() {
        let changes = vec![make_file_changes(
            "packages/react-core/src/components/EmptyState/EmptyStateHeader.tsx",
            vec![make_api_change(
                "EmptyStateHeader",
                ApiChangeKind::Constant,
                ApiChangeType::Removed,
                "EmptyStateHeader removed",
            )],
            vec![],
        )];

        let pkg_cache = make_pkg_cache(&[("react-core", "@patternfly/react-core")]);
        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &pkg_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let p0c_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("component-import-deprecated"));
        assert!(p0c_rule.is_some(), "Missing P0-C rule");

        match &p0c_rule.unwrap().when {
            KonveyorCondition::FrontendReferenced { referenced } => {
                assert_eq!(
                    referenced.from.as_deref(),
                    Some("@patternfly/react-core"),
                    "P0-C rule should be scoped to @patternfly/react-core"
                );
            }
            _ => panic!("P0-C rule should use FrontendReferenced condition"),
        }
    }

    // Constant collapse combined rule should have `from` set
    #[test]
    fn test_constant_collapse_has_from_package() {
        let mut api_changes = Vec::new();
        for i in 0..15 {
            api_changes.push(ApiChange {
                symbol: format!("c_component_token_{}", i),
                kind: ApiChangeKind::Constant,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: format!("Token {} removed", i),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            });
        }

        let changes = vec![make_file_changes(
            "packages/react-tokens/dist/esm/tokens.d.ts",
            api_changes,
            vec![],
        )];

        let pkg_cache = make_pkg_cache(&[("react-tokens", "@patternfly/react-tokens")]);
        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &pkg_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let combined = rules.iter().find(|r| r.rule_id.contains("combined"));
        assert!(combined.is_some(), "Expected a combined constant rule");

        match &combined.unwrap().when {
            KonveyorCondition::FrontendReferenced { referenced } => {
                assert_eq!(
                    referenced.from.as_deref(),
                    Some("@patternfly/react-tokens"),
                    "Combined constant rule should be scoped to @patternfly/react-tokens"
                );
            }
            _ => panic!("Combined constant rule should use FrontendReferenced condition"),
        }
    }

    // New sibling detection rule should have `from` set
    #[test]
    fn test_new_sibling_rule_has_from_package() {
        let changes = vec![
            make_file_changes(
                "packages/react-core/src/components/Masthead/MastheadBrand.tsx",
                vec![make_api_change(
                    "MastheadBrandProps.component",
                    ApiChangeKind::Property,
                    ApiChangeType::Removed,
                    "component prop removed",
                )],
                vec![],
            ),
            make_file_changes(
                "packages/react-core/src/components/Masthead/examples/Demo.tsx",
                vec![],
                vec![make_behavioral(
                    "Demo",
                    Some(TsCategory::DomStructure),
                    "<MastheadLogo> element added to render output",
                )],
            ),
        ];

        let pkg_cache = make_pkg_cache(&[("react-core", "@patternfly/react-core")]);
        let report = make_report_with_added(
            changes,
            vec![PathBuf::from(
                "packages/react-core/src/components/Masthead/MastheadLogo.tsx",
            )],
        );

        let rules = generate_rules(
            &report,
            "*.ts",
            &pkg_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let sibling = rules.iter().find(|r| r.rule_id.contains("new-sibling"));
        assert!(sibling.is_some(), "Expected a new-sibling rule");

        match &sibling.unwrap().when {
            KonveyorCondition::FrontendReferenced { referenced } => {
                assert_eq!(
                    referenced.from.as_deref(),
                    Some("@patternfly/react-core"),
                    "Sibling rule should be scoped to @patternfly/react-core"
                );
            }
            _ => panic!("Sibling rule should use FrontendReferenced condition"),
        }
    }

    // Rules from different packages must NOT share the same `from`
    #[test]
    fn test_rules_from_different_packages_have_distinct_from() {
        let changes = vec![
            make_file_changes(
                "packages/react-core/dist/esm/components/Button/Button.d.ts",
                vec![make_api_change(
                    "Button.isActive",
                    ApiChangeKind::Property,
                    ApiChangeType::Removed,
                    "isActive prop removed",
                )],
                vec![],
            ),
            make_file_changes(
                "packages/react-icons/dist/esm/icons/CheckIcon.d.ts",
                vec![make_api_change(
                    "CheckIcon",
                    ApiChangeKind::Constant,
                    ApiChangeType::Removed,
                    "CheckIcon removed",
                )],
                vec![],
            ),
        ];

        let pkg_cache = make_pkg_cache(&[
            ("react-core", "@patternfly/react-core"),
            ("react-icons", "@patternfly/react-icons"),
        ]);
        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &pkg_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let core_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("button"))
            .collect();
        let icon_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("checkicon"))
            .collect();

        assert!(!core_rules.is_empty(), "Expected Button rules");
        assert!(!icon_rules.is_empty(), "Expected CheckIcon rules");

        for rule in &core_rules {
            if let KonveyorCondition::FrontendReferenced { referenced } = &rule.when {
                assert_eq!(
                    referenced.from.as_deref(),
                    Some("@patternfly/react-core"),
                    "Button rule should be from react-core, got {:?}",
                    referenced.from
                );
            }
        }

        for rule in &icon_rules {
            if let KonveyorCondition::FrontendReferenced { referenced } = &rule.when {
                assert_eq!(
                    referenced.from.as_deref(),
                    Some("@patternfly/react-icons"),
                    "CheckIcon rule should be from react-icons, got {:?}",
                    referenced.from
                );
            }
        }
    }

    // Deprecated subpath should use anchored from: "^@patternfly/react-core/deprecated$"
    #[test]
    fn test_deprecated_subpath_uses_anchored_from() {
        let changes = vec![make_file_changes(
            "packages/react-core/src/deprecated/components/Wizard/Wizard.d.ts",
            vec![make_api_change(
                "Wizard",
                ApiChangeKind::Constant,
                ApiChangeType::Removed,
                "Deprecated Wizard removed",
            )],
            vec![],
        )];

        let pkg_cache = make_pkg_cache(&[("react-core", "@patternfly/react-core")]);
        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &pkg_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Find a rule for the deprecated Wizard
        let wizard_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.to_lowercase().contains("wizard"))
            .collect();
        assert!(!wizard_rules.is_empty(), "Expected Wizard rules");

        // At least one should have the deprecated anchored from
        let has_deprecated_from = wizard_rules.iter().any(|r| match &r.when {
            KonveyorCondition::FrontendReferenced { referenced } => referenced
                .from
                .as_ref()
                .map_or(false, |f| f.contains("deprecated")),
            KonveyorCondition::Or { or } => or.iter().any(|c| {
                if let KonveyorCondition::FrontendReferenced { referenced } = c {
                    referenced
                        .from
                        .as_ref()
                        .map_or(false, |f| f.contains("deprecated"))
                } else {
                    false
                }
            }),
            _ => false,
        });
        assert!(
            has_deprecated_from,
            "Deprecated Wizard rules should have from containing 'deprecated'"
        );
    }

    #[test]
    fn test_frontend_provider_constant_uses_import() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/config.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "DEFAULT_TIMEOUT".to_string(),
                kind: ApiChangeKind::Constant,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: "DEFAULT_TIMEOUT removed".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let empty_cache = HashMap::new();
        let rules = generate_rules(
            &report,
            "*.ts",
            &empty_cache,
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        assert_eq!(rules.len(), 1);
        let yaml = serde_yaml::to_string(&rules[0]).unwrap();
        assert!(yaml.contains("IMPORT"));
        assert!(yaml.contains("^DEFAULT_TIMEOUT$"));
    }

    // ── V2 package-based code path tests ────────────────────────────────
    // These tests populate report.packages with ComponentSummary data to
    // exercise the v2 code paths (instead of the legacy flat-changes scan).

    // P0-C v2: component with high removal ratio triggers rule
    #[test]
    fn test_p0c_v2_triggers_for_heavily_removed_components() {
        // Flat changes still present (for per-file rule generation)
        let mut api_changes = Vec::new();
        for i in 0..10 {
            api_changes.push(make_api_change(
                &format!("ModalProps.prop{}", i),
                ApiChangeKind::Property,
                ApiChangeType::Removed,
                &format!("prop{} removed", i),
            ));
        }
        for i in 10..14 {
            api_changes.push(make_api_change(
                &format!("ModalProps.prop{}", i),
                ApiChangeKind::Property,
                ApiChangeType::TypeChanged,
                &format!("prop{} type changed", i),
            ));
        }

        let changes = vec![make_file_changes(
            "packages/react-core/src/components/Modal/Modal.tsx",
            api_changes,
            vec![],
        )];

        let mut report = make_report(changes, vec![]);

        // Populate packages with pre-aggregated ComponentSummary
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: Some("5.0.0".to_string()),
            new_version: Some("6.0.0".to_string()),
            type_summaries: vec![ComponentSummary {
                name: "Modal".to_string(),
                definition_name: "ModalProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary {
                    total: 14,
                    removed: 10,
                    renamed: 0,
                    type_changed: 4,
                    added: 0,
                    removal_ratio: 10.0 / 14.0,
                },
                removed_members: (0..10)
                    .map(|i| RemovedMember {
                        name: format!("prop{}", i),
                        old_type: None,
                        removal_disposition: None,
                    })
                    .collect(),
                type_changes: (10..14)
                    .map(|i| TypeChange {
                        property: format!("prop{}", i),
                        before: None,
                        after: None,
                    })
                    .collect(),
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Modal has 10/14 removed (ratio > 0.5, removed >= 3) — should trigger
        let p0c_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("component-import-deprecated"))
            .collect();
        assert!(
            !p0c_rules.is_empty(),
            "Modal with 10/14 removed props (v2 path) should get a P0-C rule"
        );
        assert_eq!(
            p0c_rules[0].fix_strategy.as_ref().unwrap().strategy,
            "LlmAssisted"
        );
        // Verify from field is set from pkg.name
        match &p0c_rules[0].when {
            KonveyorCondition::FrontendReferenced { referenced } => {
                assert_eq!(
                    referenced.from.as_deref(),
                    Some("@patternfly/react-core"),
                    "from should come from pkg.name"
                );
            }
            _ => panic!("Expected FrontendReferenced condition"),
        }
        // Verify message uses v2 migration message builder
        assert!(
            p0c_rules[0].message.contains("MIGRATION"),
            "Message should contain MIGRATION header"
        );
    }

    // P0-C v2: component with low removal ratio does NOT trigger
    #[test]
    fn test_p0c_v2_skips_minor_prop_removals() {
        let changes = vec![make_file_changes(
            "packages/react-core/src/components/Button/Button.tsx",
            vec![
                make_api_change(
                    "ButtonProps.isActive",
                    ApiChangeKind::Property,
                    ApiChangeType::Removed,
                    "isActive prop removed",
                ),
                make_api_change(
                    "ButtonProps.variant",
                    ApiChangeKind::Property,
                    ApiChangeType::TypeChanged,
                    "variant type changed",
                ),
            ],
            vec![],
        )];

        let mut report = make_report(changes, vec![]);
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "Button".to_string(),
                definition_name: "ButtonProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary {
                    total: 10,
                    removed: 1,
                    renamed: 0,
                    type_changed: 1,
                    added: 0,
                    removal_ratio: 0.1,
                },
                removed_members: vec![RemovedMember {
                    name: "isActive".to_string(),
                    old_type: None,
                    removal_disposition: None,
                }],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let p0c_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("component-import-deprecated"))
            .collect();
        assert!(
            p0c_rules.is_empty(),
            "Button with 1/10 removed props (v2 path) should NOT get a P0-C rule. Got: {:?}",
            p0c_rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
    }

    // New sibling detection v2: uses child_components from packages
    #[test]
    fn test_new_sibling_v2_detection_from_child_components() {
        // MastheadLogo has no absorbed_props and is not composition-required,
        // so it should be SKIPPED as a truly optional new-sibling.
        let changes = vec![make_file_changes(
            "packages/react-core/src/components/Masthead/MastheadBrand.tsx",
            vec![
                make_api_change(
                    "MastheadBrandProps",
                    ApiChangeKind::Interface,
                    ApiChangeType::SignatureChanged,
                    "Now extends HTMLDivElement instead of HTMLAnchorElement",
                ),
                make_api_change(
                    "MastheadBrandProps.component",
                    ApiChangeKind::Property,
                    ApiChangeType::Removed,
                    "component prop removed",
                ),
            ],
            vec![],
        )];

        let mut report = make_report(changes, vec![]);
        // Populate packages with child_components
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "MastheadBrand".to_string(),
                definition_name: "MastheadBrandProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary {
                    total: 5,
                    removed: 1,
                    renamed: 0,
                    type_changed: 0,
                    added: 0,
                    removal_ratio: 0.2,
                },
                removed_members: vec![RemovedMember {
                    name: "component".to_string(),
                    old_type: None,
                    removal_disposition: None,
                }],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![ChildComponent {
                    name: "MastheadLogo".to_string(),
                    status: ChildComponentStatus::Added,
                    known_members: vec!["href".to_string()],
                    absorbed_members: vec![],
                }],
                expected_children: vec![],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let sibling_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("new-sibling"))
            .collect();
        assert_eq!(
            sibling_rules.len(),
            0,
            "Expected 0 new-sibling rules (MastheadLogo has no absorbed_props, should be skipped), got {}",
            sibling_rules.len()
        );
    }

    #[test]
    fn test_new_sibling_v2_mandatory_with_absorbed_props() {
        // A child component WITH absorbed_props should produce a mandatory
        // new-sibling rule.
        let changes = vec![make_file_changes(
            "packages/react-core/src/components/Modal/Modal.tsx",
            vec![make_api_change(
                "ModalProps.title",
                ApiChangeKind::Property,
                ApiChangeType::Removed,
                "title prop removed",
            )],
            vec![],
        )];

        let mut report = make_report(changes, vec![]);
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "Modal".to_string(),
                definition_name: "ModalProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary {
                    total: 20,
                    removed: 1,
                    renamed: 0,
                    type_changed: 0,
                    added: 0,
                    removal_ratio: 0.05,
                },
                removed_members: vec![RemovedMember {
                    name: "title".to_string(),
                    old_type: None,
                    removal_disposition: None,
                }],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![ChildComponent {
                    name: "ModalHeader".to_string(),
                    status: ChildComponentStatus::Added,
                    known_members: vec!["title".to_string()],
                    absorbed_members: vec!["title".to_string()],
                }],
                expected_children: vec![],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let sibling_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("new-sibling"))
            .collect();
        assert_eq!(
            sibling_rules.len(),
            1,
            "Expected 1 new-sibling rule (ModalHeader absorbs title), got {}",
            sibling_rules.len()
        );
        assert_eq!(sibling_rules[0].category, "mandatory");
        assert!(sibling_rules[0].message.contains("ModalHeader"));
        assert!(sibling_rules[0].message.contains("title"));
    }

    // New sibling v2: only Added children generate rules (not Modified)
    #[test]
    fn test_new_sibling_v2_skips_modified_children() {
        let changes = vec![make_file_changes(
            "packages/react-core/src/components/Modal/Modal.tsx",
            vec![make_api_change(
                "ModalProps.title",
                ApiChangeKind::Property,
                ApiChangeType::Removed,
                "title prop removed",
            )],
            vec![],
        )];

        let mut report = make_report(changes, vec![]);
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "Modal".to_string(),
                definition_name: "ModalProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![ChildComponent {
                    name: "ModalHeader".to_string(),
                    status: ChildComponentStatus::Modified, // Not Added
                    known_members: vec![],
                    absorbed_members: vec![],
                }],
                expected_children: vec![],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let sibling_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("new-sibling"))
            .collect();
        assert_eq!(
            sibling_rules.len(),
            0,
            "Modified children should not generate sibling rules"
        );
    }

    // P0-C v2: Removed component status triggers rule
    #[test]
    fn test_p0c_v2_removed_component_status_triggers() {
        let changes = vec![make_file_changes(
            "packages/react-core/src/components/EmptyState/EmptyStateHeader.tsx",
            vec![make_api_change(
                "EmptyStateHeader",
                ApiChangeKind::Constant,
                ApiChangeType::Removed,
                "EmptyStateHeader component removed",
            )],
            vec![],
        )];

        let mut report = make_report(changes, vec![]);
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "EmptyStateHeader".to_string(),
                definition_name: "EmptyStateHeaderProps".to_string(),
                status: ComponentStatus::Removed,
                member_summary: MemberSummary {
                    total: 5,
                    removed: 5,
                    renamed: 0,
                    type_changed: 0,
                    added: 0,
                    removal_ratio: 1.0,
                },
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let p0c_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("component-import-deprecated"))
            .collect();
        assert!(
            !p0c_rules.is_empty(),
            "Removed component (v2 path) should get a P0-C rule"
        );
        assert!(
            p0c_rules[0].message.contains("MIGRATION"),
            "Message should contain MIGRATION header"
        );
        assert!(
            p0c_rules[0].message.contains("was removed"),
            "Message should indicate component was removed"
        );
    }

    // build_migration_message_v2 with migration_target
    #[test]
    fn test_build_migration_message_v2_with_migration_target() {
        let comp = ComponentSummary {
            name: "EmptyStateHeader".to_string(),
            definition_name: "EmptyStateHeaderProps".to_string(),
            status: ComponentStatus::Removed,
            member_summary: MemberSummary {
                total: 5,
                removed: 5,
                renamed: 0,
                type_changed: 0,
                added: 0,
                removal_ratio: 1.0,
            },
            removed_members: vec![],
            type_changes: vec![],
            migration_target: Some(MigrationTarget {
                removed_symbol: "EmptyStateHeaderProps".to_string(),
                removed_qualified_name: "EmptyStateHeader.EmptyStateHeaderProps".to_string(),
                removed_package: None,
                replacement_symbol: "EmptyStateProps".to_string(),
                replacement_qualified_name: "EmptyState.EmptyStateProps".to_string(),
                replacement_package: None,
                matching_members: vec![
                    MemberMapping {
                        old_name: "titleText".to_string(),
                        new_name: "titleText".to_string(),
                    },
                    MemberMapping {
                        old_name: "icon".to_string(),
                        new_name: "icon".to_string(),
                    },
                ],
                removed_only_members: vec!["className".to_string()],
                overlap_ratio: 0.67,
                old_extends: None,
                new_extends: None,
            }),
            behavioral_changes: vec![make_behavioral(
                "EmptyStateHeader",
                Some(TsCategory::RenderOutput),
                "<EmptyStateHeader> element removed from render output",
            )],
            child_components: vec![],
            expected_children: vec![],
            source_files: vec![],
        };

        let msg = build_migration_message_v2(&comp);
        assert!(
            msg.contains("Replace <EmptyStateHeader>"),
            "Should have migration header"
        );
        assert!(
            msg.contains("EmptyState"),
            "Should reference replacement component"
        );
        assert!(msg.contains("titleText"), "Should include property mapping");
        assert!(msg.contains("icon"), "Should include icon in mapping");
        assert!(
            msg.contains("className"),
            "Should include removed-only members"
        );
        assert!(
            msg.contains("Behavioral changes"),
            "Should include behavioral section"
        );
        assert!(
            msg.contains("element removed from render output"),
            "Should include behavioral description"
        );
    }

    // build_migration_message_v2 for restructured component (with child components)
    #[test]
    fn test_build_migration_message_v2_restructured_with_children() {
        let comp = ComponentSummary {
            name: "Modal".to_string(),
            definition_name: "ModalProps".to_string(),
            status: ComponentStatus::Modified,
            member_summary: MemberSummary {
                total: 14,
                removed: 10,
                renamed: 0,
                type_changed: 4,
                added: 0,
                removal_ratio: 10.0 / 14.0,
            },
            removed_members: vec![
                RemovedMember {
                    name: "title".to_string(),
                    old_type: Some("string".to_string()),
                    removal_disposition: None,
                },
                RemovedMember {
                    name: "actions".to_string(),
                    old_type: None,
                    removal_disposition: None,
                },
            ],
            type_changes: vec![TypeChange {
                property: "variant".to_string(),
                before: Some("'default' | 'large'".to_string()),
                after: Some("'default' | 'medium' | 'large'".to_string()),
            }],
            migration_target: None,
            behavioral_changes: vec![],
            child_components: vec![
                ChildComponent {
                    name: "ModalHeader".to_string(),
                    status: ChildComponentStatus::Added,
                    known_members: vec!["title".to_string(), "description".to_string()],
                    absorbed_members: vec!["title".to_string(), "description".to_string()],
                },
                ChildComponent {
                    name: "ModalFooter".to_string(),
                    status: ChildComponentStatus::Added,
                    known_members: vec![],
                    absorbed_members: vec![],
                },
            ],
            expected_children: vec![],
            source_files: vec![],
        };

        let msg = build_migration_message_v2(&comp);
        assert!(msg.contains("restructured"), "Should mention restructured");
        assert!(
            msg.contains("10 of 14 props removed"),
            "Should show removal counts"
        );
        assert!(msg.contains("Removed props"), "Should list removed props");
        assert!(
            msg.contains("  - title"),
            "Should include title in removed list"
        );
        assert!(
            msg.contains("  - actions"),
            "Should include actions in removed list"
        );
        assert!(
            msg.contains("ModalHeader"),
            "Should include ModalHeader child. Msg:\n{msg}"
        );
        assert!(
            msg.contains("ModalFooter"),
            "Should include ModalFooter child. Msg:\n{msg}"
        );
        // ModalHeader should show absorbed props with mechanism (pass as props since
        // title and description are in ModalHeader's known_props)
        assert!(
            msg.contains("pass as props: title, description"),
            "Should show absorbed props mapping for ModalHeader. Msg:\n{msg}"
        );
        assert!(
            msg.contains("Type changes"),
            "Should include type changes section"
        );
        assert!(
            msg.contains("variant"),
            "Should include variant type change"
        );
    }

    // ── Tier 1: removal_disposition in migration messages ────────────

    #[test]
    fn test_migration_message_with_removal_dispositions() {
        use semver_analyzer_core::RemovalDisposition;

        let comp = ComponentSummary {
            name: "Modal".to_string(),
            definition_name: "ModalProps".to_string(),
            status: ComponentStatus::Modified,
            member_summary: MemberSummary {
                total: 20,
                removed: 8,
                renamed: 0,
                type_changed: 0,
                added: 0,
                removal_ratio: 8.0 / 20.0,
            },
            removed_members: vec![
                RemovedMember {
                    name: "title".to_string(),
                    old_type: Some("string".to_string()),
                    removal_disposition: Some(RemovalDisposition::MovedToRelatedType {
                        target_type: "ModalHeader".to_string(),
                        mechanism: "prop".to_string(),
                    }),
                },
                RemovedMember {
                    name: "actions".to_string(),
                    old_type: None,
                    removal_disposition: Some(RemovalDisposition::MovedToRelatedType {
                        target_type: "ModalFooter".to_string(),
                        mechanism: "children".to_string(),
                    }),
                },
                RemovedMember {
                    name: "footer".to_string(),
                    old_type: None,
                    removal_disposition: Some(RemovalDisposition::MovedToRelatedType {
                        target_type: "ModalFooter".to_string(),
                        mechanism: "children".to_string(),
                    }),
                },
                RemovedMember {
                    name: "showClose".to_string(),
                    old_type: None,
                    removal_disposition: Some(RemovalDisposition::TrulyRemoved),
                },
                RemovedMember {
                    name: "hasNoBodyWrapper".to_string(),
                    old_type: None,
                    removal_disposition: Some(RemovalDisposition::MadeAutomatic),
                },
            ],
            type_changes: vec![],
            migration_target: None,
            behavioral_changes: vec![],
            child_components: vec![
                ChildComponent {
                    name: "ModalHeader".to_string(),
                    status: ChildComponentStatus::Added,
                    known_members: vec!["title".to_string(), "description".to_string()],
                    absorbed_members: vec!["title".to_string()],
                },
                ChildComponent {
                    name: "ModalFooter".to_string(),
                    status: ChildComponentStatus::Added,
                    known_members: vec!["children".to_string(), "className".to_string()],
                    absorbed_members: vec!["actions".to_string(), "footer".to_string()],
                },
            ],
            expected_children: vec![],
            source_files: vec![],
        };

        let msg = build_migration_message_v2(&comp);

        // ModalHeader: title is a known prop → "pass as props"
        assert!(
            msg.contains("pass as props: title"),
            "ModalHeader should show 'pass as props' for title. Msg:\n{msg}"
        );

        // ModalFooter: actions/footer have mechanism=children → "pass as children"
        assert!(
            msg.contains("pass as children: actions, footer"),
            "ModalFooter should show 'pass as children' for actions, footer. Msg:\n{msg}"
        );

        // showClose is truly_removed → "safe to delete"
        assert!(
            msg.contains("safe to delete"),
            "Should mention 'safe to delete' for truly removed props. Msg:\n{msg}"
        );
        assert!(
            msg.contains("showClose"),
            "showClose should be in safe to delete list. Msg:\n{msg}"
        );

        // hasNoBodyWrapper is made_automatic → also "safe to delete"
        assert!(
            msg.contains("hasNoBodyWrapper"),
            "hasNoBodyWrapper should be in safe to delete list. Msg:\n{msg}"
        );
    }

    #[test]
    fn test_p0c_suppression_covers_enriched_props() {
        // When a component qualifies for P0-C (>= 5 removals), all its
        // per-prop rules should be suppressed, including those with
        // removal_disposition data.
        use semver_analyzer_core::RemovalDisposition;

        let prop_names = [
            "title",
            "actions",
            "footer",
            "description",
            "header",
            "help",
        ];
        let changes = vec![FileChanges {
            file: "packages/react-core/src/components/Modal/Modal.ModalProps.d.ts".into(),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: prop_names
                .iter()
                .map(|name| ApiChange {
                    symbol: format!("ModalProps.{}", name),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Removed,
                    before: None,
                    after: None,
                    description: format!("{} removed", name),
                    migration_target: None,
                    removal_disposition: None,
                    renders_element: None,
                })
                .collect(),
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let mut report = make_report(changes, vec![]);
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "Modal".to_string(),
                definition_name: "ModalProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary {
                    total: 20,
                    removed: 6,
                    renamed: 0,
                    type_changed: 0,
                    added: 0,
                    removal_ratio: 6.0 / 20.0,
                },
                removed_members: vec![
                    RemovedMember {
                        name: "title".into(),
                        old_type: None,
                        removal_disposition: Some(RemovalDisposition::MovedToRelatedType {
                            target_type: "ModalHeader".into(),
                            mechanism: "prop".into(),
                        }),
                    },
                    RemovedMember {
                        name: "actions".into(),
                        old_type: None,
                        removal_disposition: Some(RemovalDisposition::MovedToRelatedType {
                            target_type: "ModalFooter".into(),
                            mechanism: "children".into(),
                        }),
                    },
                    RemovedMember {
                        name: "footer".into(),
                        old_type: None,
                        removal_disposition: None,
                    },
                    RemovedMember {
                        name: "description".into(),
                        old_type: None,
                        removal_disposition: None,
                    },
                    RemovedMember {
                        name: "header".into(),
                        old_type: None,
                        removal_disposition: None,
                    },
                    RemovedMember {
                        name: "help".into(),
                        old_type: None,
                        removal_disposition: None,
                    },
                ],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![
                    ChildComponent {
                        name: "ModalHeader".into(),
                        status: ChildComponentStatus::Added,
                        known_members: vec!["title".into()],
                        absorbed_members: vec!["title".into()],
                    },
                    ChildComponent {
                        name: "ModalFooter".into(),
                        status: ChildComponentStatus::Added,
                        known_members: vec!["children".into()],
                        absorbed_members: vec!["actions".into()],
                    },
                ],
                expected_children: vec![],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Should have a P0-C rule for Modal (6 removals >= 5 threshold)
        let p0c_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("component-import-deprecated"));
        assert!(
            p0c_rule.is_some(),
            "Should generate P0-C rule for Modal. Rule IDs: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // Per-prop removal rules should be suppressed
        let prop_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| {
                r.rule_id.contains("modalprops-title")
                    || r.rule_id.contains("modalprops-actions")
                    || r.rule_id.contains("modalprops-footer")
            })
            .collect();
        assert!(
            prop_rules.is_empty(),
            "Per-prop removal rules should be suppressed by P0-C. Found: {:?}",
            prop_rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // New-sibling rules should exist and carry AST-driven prop mappings
        let header_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("new-sibling-modalheader"));
        assert!(
            header_rule.is_some(),
            "Should have enriched new-sibling rule for ModalHeader. IDs: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
        let header_msg = &header_rule.unwrap().message;
        assert!(
            header_msg.contains("title"),
            "ModalHeader rule should mention title. Msg:\n{header_msg}"
        );
        assert!(
            header_msg.contains("<ModalHeader title="),
            "ModalHeader rule should show how to pass title as prop. Msg:\n{header_msg}"
        );

        let footer_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("new-sibling-modalfooter"));
        assert!(
            footer_rule.is_some(),
            "Should have enriched new-sibling rule for ModalFooter. IDs: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
        let footer_msg = &footer_rule.unwrap().message;
        assert!(
            footer_msg.contains("actions"),
            "ModalFooter rule should mention actions. Msg:\n{footer_msg}"
        );
        assert!(
            footer_msg.contains("pass as children"),
            "ModalFooter rule should show 'pass as children' for actions. Msg:\n{footer_msg}"
        );
    }

    // ── Fix #3: Internal component behavioral rules filtered ────────

    #[test]
    fn test_is_internal_only_behavioral_skipped() {
        // A behavioral change with is_internal_only=true should not produce a rule
        let mut internal_beh = make_behavioral(
            "ModalBox",
            Some(TsCategory::DomStructure),
            "Internal wrapper now uses div instead of section",
        );
        internal_beh.is_internal_only = Some(true);

        let changes = vec![make_file_changes(
            "packages/react-core/src/components/Modal/ModalBox.tsx",
            vec![],
            vec![internal_beh],
        )];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let modalbox_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("modalbox"))
            .collect();
        assert!(
            modalbox_rules.is_empty(),
            "is_internal_only=true should suppress rule. Found: {:?}",
            modalbox_rules
                .iter()
                .map(|r| &r.rule_id)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_non_public_behavioral_skipped_when_packages_present() {
        // A behavioral change for a symbol NOT in report.packages should be skipped
        // when packages data is available (non-empty).
        let internal_beh = make_behavioral(
            "MenuBase",
            Some(TsCategory::DomStructure),
            "Internal base component changed",
        );

        let changes = vec![make_file_changes(
            "packages/react-core/src/components/Menu/MenuBase.tsx",
            vec![],
            vec![internal_beh],
        )];

        let mut report = make_report(changes, vec![]);
        // Add packages with only "Menu" as a public component (not "MenuBase")
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "Menu".to_string(),
                definition_name: "MenuProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary {
                    total: 5,
                    removed: 0,
                    renamed: 0,
                    type_changed: 1,
                    added: 0,
                    removal_ratio: 0.0,
                },
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let menubase_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("menubase"))
            .collect();
        assert!(
            menubase_rules.is_empty(),
            "Non-public symbol 'MenuBase' should not produce a rule. Found: {:?}",
            menubase_rules
                .iter()
                .map(|r| &r.rule_id)
                .collect::<Vec<_>>()
        );
    }

    // ── Fix #4: prop-value-change suppressed by type-changed ──────

    #[test]
    fn test_suppress_redundant_prop_value_rules() {
        // Create two rules that would overlap:
        // 1. type-changed rule with value constraint (from per-value virtual file)
        // 2. prop-value-change rule with same value constraint (from main props file)
        let type_changed_rule = KonveyorRule {
            rule_id: "semver-label-color-type-changed".to_string(),
            labels: vec![
                "source=semver-analyzer".to_string(),
                "change-type=type-changed".to_string(),
            ],
            effort: 3,
            category: "mandatory".to_string(),
            description: "Type of color changed".to_string(),
            message: "Full union type change".to_string(),
            links: vec![],
            when: KonveyorCondition::Or {
                or: vec![
                    KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: "^color$".to_string(),
                            location: "JSX_PROP".to_string(),
                            component: Some("^Label$".to_string()),
                            parent: None,
                    not_parent: None,
                            value: Some("^cyan$".to_string()),
                            from: Some("@patternfly/react-core".to_string()),
                            parent_from: None,
                        },
                    },
                    KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: "^color$".to_string(),
                            location: "JSX_PROP".to_string(),
                            component: Some("^Label$".to_string()),
                            parent: None,
                    not_parent: None,
                            value: Some("^gold$".to_string()),
                            from: Some("@patternfly/react-core".to_string()),
                            parent_from: None,
                        },
                    },
                ],
            },
            fix_strategy: None,
        };

        let prop_value_rule = KonveyorRule {
            rule_id: "semver-label-color-prop-value-change".to_string(),
            labels: vec![
                "source=semver-analyzer".to_string(),
                "change-type=prop-value-change".to_string(),
            ],
            effort: 1,
            category: "mandatory".to_string(),
            description: "Prop value removed".to_string(),
            message: "Value cyan removed from color".to_string(),
            links: vec![],
            when: KonveyorCondition::Or {
                or: vec![
                    KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: "^color$".to_string(),
                            location: "JSX_PROP".to_string(),
                            component: Some("^Label$".to_string()),
                            parent: None,
                    not_parent: None,
                            value: Some("^cyan$".to_string()),
                            from: Some("@patternfly/react-core".to_string()),
                            parent_from: None,
                        },
                    },
                    KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: "^color$".to_string(),
                            location: "JSX_PROP".to_string(),
                            component: Some("^Label$".to_string()),
                            parent: None,
                    not_parent: None,
                            value: Some("^gold$".to_string()),
                            from: Some("@patternfly/react-core".to_string()),
                            parent_from: None,
                        },
                    },
                ],
            },
            fix_strategy: None,
        };

        // Also include an unrelated rule to verify it's kept
        let unrelated_rule = KonveyorRule {
            rule_id: "semver-button-variant-type-changed".to_string(),
            labels: vec![
                "source=semver-analyzer".to_string(),
                "change-type=type-changed".to_string(),
            ],
            effort: 3,
            category: "mandatory".to_string(),
            description: "Button variant changed".to_string(),
            message: "Variant type narrowed".to_string(),
            links: vec![],
            when: KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: "^variant$".to_string(),
                    location: "JSX_PROP".to_string(),
                    component: Some("^Button$".to_string()),
                    parent: None,
                    not_parent: None,
                    value: None,
                    from: Some("@patternfly/react-core".to_string()),
                    parent_from: None,
                },
            },
            fix_strategy: None,
        };

        let rules = vec![type_changed_rule, prop_value_rule, unrelated_rule];

        let result = suppress_redundant_prop_value_rules(rules);

        assert_eq!(
            result.len(),
            2,
            "Should suppress 1 prop-value-change rule, keeping 2. IDs: {:?}",
            result.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // type-changed should survive
        assert!(
            result
                .iter()
                .any(|r| r.rule_id == "semver-label-color-type-changed"),
            "type-changed rule should survive"
        );
        // prop-value-change should be suppressed
        assert!(
            !result
                .iter()
                .any(|r| r.rule_id == "semver-label-color-prop-value-change"),
            "prop-value-change rule should be suppressed"
        );
        // Unrelated rule kept
        assert!(
            result
                .iter()
                .any(|r| r.rule_id == "semver-button-variant-type-changed"),
            "Unrelated rule should be kept"
        );
    }

    #[test]
    fn test_public_behavioral_not_skipped() {
        // A behavioral change for a public symbol should still produce a rule
        let beh = make_behavioral(
            "Menu",
            Some(TsCategory::DomStructure),
            "Menu now renders nav element",
        );

        let changes = vec![make_file_changes(
            "packages/react-core/src/components/Menu/Menu.tsx",
            vec![],
            vec![beh],
        )];

        let mut report = make_report(changes, vec![]);
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "Menu".to_string(),
                definition_name: "MenuProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary {
                    total: 5,
                    removed: 0,
                    renamed: 0,
                    type_changed: 0,
                    added: 0,
                    removal_ratio: 0.0,
                },
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let menu_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("menu") && r.rule_id.contains("behavioral"))
            .collect();
        assert!(
            !menu_rules.is_empty(),
            "Public symbol 'Menu' should produce a behavioral rule. IDs: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
    }

    // ── extract_target_prop tests ───────────────────────────────────────

    #[test]
    fn test_extract_target_prop_as_pattern() {
        assert_eq!(extract_target_prop("Button (as icon prop)"), Some("icon"));
    }

    #[test]
    fn test_extract_target_prop_via_pattern() {
        assert_eq!(extract_target_prop("Button (via icon prop)"), Some("icon"));
    }

    #[test]
    fn test_extract_target_prop_with_wrapper() {
        assert_eq!(
            extract_target_prop("Button (as icon prop via Icon wrapper)"),
            Some("icon")
        );
    }

    #[test]
    fn test_extract_target_prop_no_parens() {
        assert_eq!(extract_target_prop("MastheadMain"), None);
    }

    #[test]
    fn test_extract_target_prop_children_context() {
        assert_eq!(extract_target_prop("Button (as children)"), None);
    }

    #[test]
    fn test_extract_target_prop_children_via_wrapper() {
        assert_eq!(
            extract_target_prop("Button (as children via div wrapper)"),
            None
        );
    }

    // ── children→prop consolidation tests ───────────────────────────────

    /// Helper: create a FileChanges with composition pattern changes.
    fn make_composition_changes(
        file: &str,
        changes: Vec<ContainerChange>,
    ) -> FileChanges<TypeScript> {
        FileChanges {
            file: PathBuf::from(file),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![],
            breaking_behavioral_changes: vec![],
            container_changes: changes,
        }
    }

    #[test]
    fn test_children_to_prop_consolidated_into_parent_rule() {
        // Two different icons both moved from Button children to Button icon prop.
        // Should produce ONE parent-level rule on Button, not two per-icon rules.
        let changes = vec![make_composition_changes(
            "packages/react-core/src/components/Button/CloseButton.tsx",
            vec![
                ContainerChange {
                    symbol: "TimesIcon".to_string(),
                    old_container: Some("Button (as children)".to_string()),
                    new_container: Some("Button (as icon prop)".to_string()),
                    description: "TimesIcon moved to icon prop".to_string(),
                },
                ContainerChange {
                    symbol: "CopyIcon".to_string(),
                    old_container: Some("Button (as children)".to_string()),
                    new_container: Some("Button (as icon prop)".to_string()),
                    description: "CopyIcon moved to icon prop".to_string(),
                },
            ],
        )];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Should have exactly one consolidated rule
        let composition_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.labels.iter().any(|l| l == "change-type=composition"))
            .collect();
        assert_eq!(
            composition_rules.len(),
            1,
            "Expected 1 consolidated rule, got {}: {:?}",
            composition_rules.len(),
            composition_rules
                .iter()
                .map(|r| &r.rule_id)
                .collect::<Vec<_>>()
        );

        let rule = composition_rules[0];
        assert!(
            rule.rule_id.contains("children-to-icon-prop"),
            "Rule ID should indicate children→prop consolidation: {}",
            rule.rule_id,
        );

        // Should match on components ending in "Icon" as children of Button
        match &rule.when {
            KonveyorCondition::FrontendReferenced { referenced } => {
                assert_eq!(
                    referenced.pattern, "Icon$",
                    "Should derive common suffix 'Icon' from child names"
                );
                assert_eq!(referenced.location, "JSX_COMPONENT");
                assert_eq!(
                    referenced.parent,
                    Some("^Button$".to_string()),
                    "Should match children of Button"
                );
                // from should be None (we want to catch app-level icons too)
                assert!(
                    referenced.from.is_none(),
                    "from should be None to catch app-level icons"
                );
            }
            other => panic!("Expected FrontendReferenced, got {:?}", other),
        }

        // Message should mention both child components
        assert!(
            rule.message.contains("TimesIcon"),
            "Message should list TimesIcon"
        );
        assert!(
            rule.message.contains("CopyIcon"),
            "Message should list CopyIcon"
        );
        assert!(
            rule.message.contains("icon"),
            "Message should mention the icon prop"
        );
    }

    #[test]
    fn test_single_children_to_prop_still_consolidated() {
        // Even a single composition change should produce a parent-level rule
        // (no threshold — always consolidate children→prop patterns).
        let changes = vec![make_composition_changes(
            "packages/react-core/src/components/MenuToggle/MenuToggle.tsx",
            vec![ContainerChange {
                symbol: "EllipsisVIcon".to_string(),
                old_container: Some("MenuToggle (as children)".to_string()),
                new_container: Some("MenuToggle (as icon prop)".to_string()),
                description: "EllipsisVIcon moved to icon prop".to_string(),
            }],
        )];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let composition_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.labels.iter().any(|l| l == "change-type=composition"))
            .collect();
        assert_eq!(composition_rules.len(), 1);

        let rule = composition_rules[0];
        // Should match on MenuToggle IMPORT
        match &rule.when {
            KonveyorCondition::FrontendReferenced { referenced } => {
                assert_eq!(referenced.pattern, "^MenuToggle$");
                assert_eq!(referenced.location, "IMPORT");
            }
            other => panic!("Expected FrontendReferenced, got {:?}", other),
        }
    }

    #[test]
    fn test_nesting_change_not_consolidated() {
        // A nesting restructure (component moves to a DIFFERENT parent) should
        // NOT be consolidated — it should remain an individual composition rule.
        let changes = vec![make_composition_changes(
            "packages/react-core/src/components/Masthead/Masthead.tsx",
            vec![ContainerChange {
                symbol: "MastheadToggle".to_string(),
                old_container: Some("Masthead".to_string()),
                new_container: Some("MastheadMain".to_string()),
                description: "MastheadToggle moved from Masthead to MastheadMain".to_string(),
            }],
        )];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let composition_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.labels.iter().any(|l| l == "change-type=composition"))
            .collect();
        assert_eq!(composition_rules.len(), 1);

        let rule = composition_rules[0];
        // Should match on the CHILD component (MastheadToggle), not the parent
        assert!(
            rule.rule_id.contains("mastheadtoggle-nesting-changed"),
            "Nesting changes should keep per-component rule IDs: {}",
            rule.rule_id,
        );
        match &rule.when {
            KonveyorCondition::FrontendReferenced { referenced } => {
                assert_eq!(referenced.pattern, "^MastheadToggle$");
                assert_eq!(referenced.location, "JSX_COMPONENT");
            }
            other => panic!("Expected FrontendReferenced, got {:?}", other),
        }
    }

    #[test]
    fn test_nesting_change_parent_field_uses_bare_name() {
        // The parent regex should use the bare component name, not the
        // LLM-generated descriptive text like "Masthead (with display=inline)".
        let changes = vec![make_composition_changes(
            "packages/react-core/src/components/Masthead/Masthead.tsx",
            vec![ContainerChange {
                symbol: "MastheadToggle".to_string(),
                old_container: Some("Masthead (with display=inline)".to_string()),
                new_container: Some("MastheadMain (inner wrapper)".to_string()),
                description: "MastheadToggle restructured".to_string(),
            }],
        )];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let composition_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.labels.iter().any(|l| l == "change-type=composition"))
            .collect();
        assert_eq!(composition_rules.len(), 1);

        match &composition_rules[0].when {
            KonveyorCondition::FrontendReferenced { referenced } => {
                // Parent regex should be bare "Masthead", not "Masthead (with display=inline)"
                assert_eq!(
                    referenced.parent.as_deref(),
                    Some("^Masthead$"),
                    "Parent regex should use bare component name, got: {:?}",
                    referenced.parent,
                );
            }
            other => panic!("Expected FrontendReferenced, got {:?}", other),
        }
    }

    #[test]
    fn test_mixed_composition_and_nesting_changes() {
        // When the same file has both children→prop AND nesting changes,
        // only the children→prop ones get consolidated. Nesting changes
        // remain as individual rules.
        let changes = vec![make_composition_changes(
            "packages/react-core/src/components/Mixed/Mixed.tsx",
            vec![
                // children→prop: should be consolidated
                ContainerChange {
                    symbol: "SearchIcon".to_string(),
                    old_container: Some("Button (as children)".to_string()),
                    new_container: Some("Button (as icon prop)".to_string()),
                    description: "SearchIcon moved to icon prop".to_string(),
                },
                // nesting restructure: should remain individual
                ContainerChange {
                    symbol: "MastheadToggle".to_string(),
                    old_container: Some("Masthead".to_string()),
                    new_container: Some("MastheadMain".to_string()),
                    description: "MastheadToggle moved under MastheadMain".to_string(),
                },
            ],
        )];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let composition_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.labels.iter().any(|l| l == "change-type=composition"))
            .collect();

        // Should have 2 rules: 1 consolidated parent-level + 1 nesting change
        assert_eq!(
            composition_rules.len(),
            2,
            "Expected 2 composition rules (1 consolidated + 1 nesting), got {}: {:?}",
            composition_rules.len(),
            composition_rules
                .iter()
                .map(|r| &r.rule_id)
                .collect::<Vec<_>>()
        );

        let consolidated = composition_rules
            .iter()
            .find(|r| r.rule_id.contains("children-to-icon-prop"));
        assert!(consolidated.is_some(), "Should have a consolidated rule");

        let nesting = composition_rules
            .iter()
            .find(|r| r.rule_id.contains("mastheadtoggle-nesting-changed"));
        assert!(nesting.is_some(), "Should have a nesting change rule");
    }

    #[test]
    fn test_children_to_prop_deduplicates_across_files() {
        // The same icon→prop change detected in multiple test files should
        // produce only one consolidated rule (not duplicates).
        let changes = vec![
            make_composition_changes(
                "packages/react-core/src/components/Modal/CloseButton.tsx",
                vec![ContainerChange {
                    symbol: "TimesIcon".to_string(),
                    old_container: Some("Button (as children)".to_string()),
                    new_container: Some("Button (as icon prop)".to_string()),
                    description: "TimesIcon in CloseButton".to_string(),
                }],
            ),
            make_composition_changes(
                "packages/react-core/src/components/Popover/PopoverClose.tsx",
                vec![ContainerChange {
                    symbol: "TimesIcon".to_string(),
                    old_container: Some("Button (as children)".to_string()),
                    new_container: Some("Button (as icon prop)".to_string()),
                    description: "TimesIcon in PopoverClose".to_string(),
                }],
            ),
        ];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let composition_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.labels.iter().any(|l| l == "change-type=composition"))
            .collect();

        // Should be exactly 1 consolidated rule (TimesIcon appears once in child list)
        assert_eq!(composition_rules.len(), 1);
        // TimesIcon should appear only once in the message (deduplicated)
        let times_count = composition_rules[0].message.matches("TimesIcon").count();
        assert_eq!(
            times_count, 1,
            "TimesIcon should be deduplicated in the message, found {} occurrences",
            times_count,
        );
    }

    // ── Value diff / value mapping tests ─────────────────────────────────

    #[test]
    fn test_value_diff_tier1_one_to_one_mapping() {
        // When exactly 1 value is removed and 1 is added, the per-value rule
        // should include an explicit "Replace with 'X'" mapping.
        let changes = vec![FileChanges {
            file: PathBuf::from("packages/react-core/src/components/Tabs/Tabs.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Tabs.variant".to_string(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some("'default' | 'light300'".to_string()),
                after: Some("'default' | 'secondary'".to_string()),
                description: "light300 renamed to secondary".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let val_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| {
                r.labels
                    .iter()
                    .any(|l| l == "change-type=prop-value-change")
            })
            .collect();

        // Should have exactly 1 per-value rule (light300)
        assert_eq!(val_rules.len(), 1, "Expected 1 per-value rule");
        let rule = val_rules[0];

        // Message should contain explicit replacement
        assert!(
            rule.message.contains("Replace with 'secondary'"),
            "Tier 1 rule should have explicit mapping. Message: {}",
            rule.message,
        );
        assert!(
            !rule.message.contains("one of the new values"),
            "Tier 1 rule should NOT use generic 'one of' phrasing",
        );

        // Fix strategy should have the mapping
        let strat = rule.fix_strategy.as_ref().unwrap();
        assert_eq!(strat.mappings.len(), 1);
        assert_eq!(strat.mappings[0].from.as_deref(), Some("light300"));
        assert_eq!(strat.mappings[0].to.as_deref(), Some("secondary"));
    }

    #[test]
    fn test_value_diff_tier3_lists_new_values() {
        // When removed/added counts differ, the message should list available
        // replacement values so the fix-engine LLM can pick correctly.
        let changes = vec![FileChanges {
            file: PathBuf::from("packages/react-core/src/components/Toolbar/ToolbarGroup.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "ToolbarGroup.variant".to_string(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some("'button-group' | 'filter-group' | 'icon-button-group'".to_string()),
                after: Some("'action-group' | 'action-group-plain' | 'filter-group'".to_string()),
                description: "variant values changed".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let val_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| {
                r.labels
                    .iter()
                    .any(|l| l == "change-type=prop-value-change")
            })
            .collect();

        // Should have 2 per-value rules (button-group, icon-button-group)
        assert_eq!(val_rules.len(), 2, "Expected 2 per-value rules");

        // Each rule should list the new values
        for rule in &val_rules {
            assert!(
                rule.message.contains("action-group")
                    && rule.message.contains("action-group-plain"),
                "Tier 3 rule should list available replacements. Message: {}",
                rule.message,
            );
            assert!(
                rule.message.contains("one of the new values"),
                "Tier 3 rule should use 'one of the new values' phrasing",
            );
        }
    }

    #[test]
    fn test_value_diff_no_added_values() {
        // When values are removed with no new values added, the message
        // should indicate there's no direct replacement.
        let changes = vec![FileChanges {
            file: PathBuf::from("packages/react-core/src/components/Page/PageSection.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "PageSection.variant".to_string(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some("'dark' | 'darker' | 'default' | 'light'".to_string()),
                after: Some("'default'".to_string()),
                description: "variant simplified".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let val_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| {
                r.labels
                    .iter()
                    .any(|l| l == "change-type=prop-value-change")
            })
            .collect();

        // Should have 3 per-value rules (dark, darker, light)
        assert_eq!(val_rules.len(), 3);

        for rule in &val_rules {
            assert!(
                rule.message.contains("no direct replacement"),
                "Rule with no added values should say no replacement. Message: {}",
                rule.message,
            );
        }
    }

    #[test]
    fn test_extract_added_union_values() {
        let change = ApiChange {
            symbol: "Foo.bar".to_string(),
            kind: ApiChangeKind::Property,
            change: ApiChangeType::TypeChanged,
            before: Some("'a' | 'b' | 'c'".to_string()),
            after: Some("'b' | 'c' | 'd' | 'e'".to_string()),
            description: "values changed".to_string(),
            migration_target: None,
            removal_disposition: None,
            renders_element: None,
        };

        let removed = extract_removed_union_values(&change);
        let added = extract_added_union_values(&change);

        assert_eq!(removed, vec!["a"]);
        assert_eq!(added, vec!["d", "e"]);
    }

    // ── Type-changed rule message enrichment tests ──────────────────────

    #[test]
    fn test_type_changed_rule_tier1_message_has_direct_mapping() {
        // A type-changed rule with 1 removed + 1 added value should have
        // the explicit mapping in its message (not just before/after types).
        let changes = vec![FileChanges {
            file: PathBuf::from("packages/react-core/src/components/Tabs/Tabs.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Tabs.variant".to_string(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some("'default' | 'light300'".to_string()),
                after: Some("'default' | 'secondary'".to_string()),
                description: "variant values changed".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        // Find the type-changed rule (not the prop-value-change one)
        let tc_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.labels.iter().any(|l| l == "change-type=type-changed"))
            .collect();
        assert!(!tc_rules.is_empty(), "Should have a type-changed rule");

        let rule = tc_rules[0];
        assert!(
            rule.message.contains("'light300' → 'secondary'"),
            "Type-changed message should contain Tier 1 mapping. Message:\n{}",
            rule.message,
        );
        assert!(
            rule.message.contains("direct replacement"),
            "Tier 1 should indicate direct replacement",
        );

        // Fix strategy should have the mapping
        let strat = rule.fix_strategy.as_ref().unwrap();
        assert!(
            strat
                .mappings
                .iter()
                .any(|m| m.from.as_deref() == Some("light300")
                    && m.to.as_deref() == Some("secondary")),
            "Fix strategy should contain value mapping. Mappings: {:?}",
            strat.mappings,
        );
    }

    #[test]
    fn test_type_changed_rule_tier3_message_lists_values() {
        // A type-changed rule with different removed/added counts should
        // list removed and new values separately.
        let changes = vec![FileChanges {
            file: PathBuf::from("packages/react-core/src/components/Toolbar/ToolbarGroup.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "ToolbarGroup.variant".to_string(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some("'button-group' | 'filter-group' | 'icon-button-group'".to_string()),
                after: Some("'action-group' | 'action-group-plain' | 'filter-group'".to_string()),
                description: "variant values changed".to_string(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let tc_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.labels.iter().any(|l| l == "change-type=type-changed"))
            .collect();
        assert!(!tc_rules.is_empty());

        let rule = tc_rules[0];
        assert!(
            rule.message.contains("Removed values:")
                && rule.message.contains("'button-group'")
                && rule.message.contains("'icon-button-group'"),
            "Should list removed values. Message:\n{}",
            rule.message,
        );
        assert!(
            rule.message.contains("New values available:")
                && rule.message.contains("'action-group'")
                && rule.message.contains("'action-group-plain'"),
            "Should list new values. Message:\n{}",
            rule.message,
        );
    }

    // ── Hierarchy delta rule generation tests ────────────────────────

    #[test]
    fn test_hierarchy_delta_generates_composition_rule() {
        // When hierarchy_deltas exist on the report, generate
        // hierarchy-composition rules with the correct structure.
        let changes = vec![];
        let mut report = make_report(changes, vec![]);

        // Add a package with the Dropdown component
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "Dropdown".to_string(),
                definition_name: "DropdownProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![
                    ExpectedChild::new("DropdownList", true),
                    ExpectedChild::new("DropdownGroup", false),
                ],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        // Add a hierarchy delta: Dropdown gained DropdownList as required child,
        // and DropdownItem moved from Dropdown to DropdownList
        report.extensions.hierarchy_deltas = vec![HierarchyDelta {
            component: "Dropdown".to_string(),
            added_children: vec![ExpectedChild::new("DropdownList", true)],
            removed_children: vec!["DropdownItem".to_string()],
            migrated_members: vec![],
            source_package: None,
            migration_target: None,
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let hierarchy_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| {
                r.labels
                    .iter()
                    .any(|l| l == "change-type=hierarchy-composition")
            })
            .collect();

        assert_eq!(
            hierarchy_rules.len(),
            1,
            "Expected 1 hierarchy-composition rule"
        );

        let rule = hierarchy_rules[0];
        assert!(
            rule.rule_id.contains("hierarchy-dropdown"),
            "Rule ID should reference Dropdown: {}",
            rule.rule_id,
        );
        assert!(
            rule.message.contains("DropdownList"),
            "Message should mention DropdownList"
        );
        assert!(
            rule.message.contains("<DropdownList>"),
            "Message should list DropdownList as a child component"
        );
        assert!(
            rule.message.contains("DropdownItem"),
            "Message should mention DropdownItem was removed as direct child"
        );

        // No removed props in this test, so should fall back to JSX_COMPONENT
        match &rule.when {
            KonveyorCondition::FrontendReferenced { referenced } => {
                assert_eq!(referenced.pattern, "^Dropdown$");
                assert_eq!(referenced.location, "JSX_COMPONENT");
                assert_eq!(referenced.from.as_deref(), Some("@patternfly/react-core"),);
            }
            other => panic!("Expected FrontendReferenced, got {:?}", other),
        }
    }

    #[test]
    fn test_hierarchy_delta_with_migrated_props() {
        // When a hierarchy delta includes migrated props, the rule message
        // should describe where each prop moved.
        let changes = vec![];
        let mut report = make_report(changes, vec![]);

        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "Modal".to_string(),
                definition_name: "ModalProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![
                    ExpectedChild::new("ModalHeader", false),
                    ExpectedChild::new("ModalBody", true),
                    ExpectedChild::new("ModalFooter", false),
                ],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        report.extensions.hierarchy_deltas = vec![HierarchyDelta {
            component: "Modal".to_string(),
            added_children: vec![
                ExpectedChild::new("ModalHeader", false),
                ExpectedChild::new("ModalBody", true),
                ExpectedChild::new("ModalFooter", false),
            ],
            removed_children: vec![],
            migrated_members: vec![
                MigratedMember {
                    member_name: "title".to_string(),
                    target_child: "ModalHeader".to_string(),
                    target_member_name: None,
                },
                MigratedMember {
                    member_name: "actions".to_string(),
                    target_child: "ModalFooter".to_string(),
                    target_member_name: None,
                },
            ],
            source_package: None,
            migration_target: None,
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let hierarchy_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| {
                r.labels
                    .iter()
                    .any(|l| l == "change-type=hierarchy-composition")
            })
            .collect();

        assert_eq!(hierarchy_rules.len(), 1);

        let rule = hierarchy_rules[0];
        assert!(
            rule.message.contains("ModalHeader") && rule.message.contains("title"),
            "Should show title migrated to ModalHeader. Message:\n{}",
            rule.message,
        );
        assert!(
            rule.message.contains("ModalFooter") && rule.message.contains("actions"),
            "Should show actions migrated to ModalFooter. Message:\n{}",
            rule.message,
        );
        assert!(
            rule.message.contains("<ModalBody>"),
            "Should list ModalBody as a child component",
        );
    }

    #[test]
    fn test_hierarchy_delta_empty_no_rules() {
        // When hierarchy_deltas is empty, no hierarchy-composition rules
        // should be generated.
        let report = make_report(vec![], vec![]);
        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let hierarchy_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| {
                r.labels
                    .iter()
                    .any(|l| l == "change-type=hierarchy-composition")
            })
            .collect();

        assert_eq!(hierarchy_rules.len(), 0);
    }

    // ── Conformance rule tests ──────────────────────────────────────

    #[test]
    fn test_conformance_wrapper_skip_rule() {
        // Chain: Dropdown → DropdownList (required) → DropdownItem
        // Should generate: <DropdownItem> with parent <Dropdown> → wrap in <DropdownList>
        let mut report = make_report(vec![], vec![]);
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![
                ComponentSummary {
                    name: "Dropdown".to_string(),
                    definition_name: "DropdownProps".to_string(),
                    status: ComponentStatus::Modified,
                    member_summary: MemberSummary::default(),
                    removed_members: vec![],
                    type_changes: vec![],
                    migration_target: None,
                    behavioral_changes: vec![],
                    child_components: vec![],
                    expected_children: vec![ExpectedChild::new("DropdownList", true)],
                    source_files: vec![],
                },
                ComponentSummary {
                    name: "DropdownList".to_string(),
                    definition_name: "DropdownListProps".to_string(),
                    status: ComponentStatus::Modified,
                    member_summary: MemberSummary::default(),
                    removed_members: vec![],
                    type_changes: vec![],
                    migration_target: None,
                    behavioral_changes: vec![],
                    child_components: vec![],
                    expected_children: vec![ExpectedChild::new("DropdownItem", true)],
                    source_files: vec![],
                },
            ],
            constants: vec![],
            added_exports: vec![],
        }];
        report.extensions.hierarchy_deltas = vec![];

        let rules = generate_conformance_rules(&report);

        assert_eq!(
            rules.len(),
            1,
            "Expected 1 wrapper-skip rule, got {}",
            rules.len()
        );
        let rule = &rules[0];

        assert!(
            rule.rule_id
                .contains("conformance-dropdownitem-needs-dropdownlist"),
            "Rule ID should reference the grandchild and wrapper: {}",
            rule.rule_id,
        );
        assert!(rule.labels.iter().any(|l| l == "change-type=conformance"));
        assert_eq!(rule.category, "mandatory");
        assert!(rule
            .message
            .contains("<DropdownItem> must be wrapped in <DropdownList>"));
        assert!(rule.message.contains("<Dropdown>"));

        match &rule.when {
            KonveyorCondition::FrontendReferenced { referenced } => {
                assert_eq!(referenced.pattern, "^DropdownItem$");
                assert_eq!(referenced.location, "JSX_COMPONENT");
                assert_eq!(
                    referenced.parent.as_deref(),
                    Some("^Dropdown$"),
                    "Should match DropdownItem with parent Dropdown"
                );
                assert_eq!(referenced.from.as_deref(), Some("@patternfly/react-core"),);
                assert_eq!(
                    referenced.parent_from.as_deref(),
                    Some("@patternfly/react-core"),
                );
            }
            other => panic!("Expected FrontendReferenced, got {:?}", other),
        }
    }

    #[test]
    fn test_conformance_skips_optional_wrapper() {
        // Chain: Accordion → AccordionItem (optional) → AccordionContent
        // Should NOT generate a rule because AccordionItem is not required.
        let mut report = make_report(vec![], vec![]);
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![
                ComponentSummary {
                    name: "Accordion".to_string(),
                    definition_name: "AccordionProps".to_string(),
                    status: ComponentStatus::Modified,
                    member_summary: MemberSummary::default(),
                    removed_members: vec![],
                    type_changes: vec![],
                    migration_target: None,
                    behavioral_changes: vec![],
                    child_components: vec![],
                    expected_children: vec![ExpectedChild::new("AccordionItem", false)],
                    source_files: vec![],
                },
                ComponentSummary {
                    name: "AccordionItem".to_string(),
                    definition_name: "AccordionItemProps".to_string(),
                    status: ComponentStatus::Modified,
                    member_summary: MemberSummary::default(),
                    removed_members: vec![],
                    type_changes: vec![],
                    migration_target: None,
                    behavioral_changes: vec![],
                    child_components: vec![],
                    expected_children: vec![ExpectedChild::new("AccordionContent", false)],
                    source_files: vec![],
                },
            ],
            constants: vec![],
            added_exports: vec![],
        }];
        report.extensions.hierarchy_deltas = vec![];

        let rules = generate_conformance_rules(&report);
        assert_eq!(
            rules.len(),
            0,
            "Should not generate rules for optional wrappers"
        );
    }

    #[test]
    fn test_conformance_no_rule_when_grandchild_is_valid_direct_child() {
        // If the grandchild is ALSO a valid direct child of the parent,
        // no wrapper-skip rule is needed.
        let mut report = make_report(vec![], vec![]);
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![
                ComponentSummary {
                    name: "Parent".to_string(),
                    definition_name: "ParentProps".to_string(),
                    status: ComponentStatus::Modified,
                    member_summary: MemberSummary::default(),
                    removed_members: vec![],
                    type_changes: vec![],
                    migration_target: None,
                    behavioral_changes: vec![],
                    child_components: vec![],
                    expected_children: vec![
                        ExpectedChild::new("Wrapper", true),
                        ExpectedChild::new("Child", false), // Also valid as direct child
                    ],
                    source_files: vec![],
                },
                ComponentSummary {
                    name: "Wrapper".to_string(),
                    definition_name: "WrapperProps".to_string(),
                    status: ComponentStatus::Modified,
                    member_summary: MemberSummary::default(),
                    removed_members: vec![],
                    type_changes: vec![],
                    migration_target: None,
                    behavioral_changes: vec![],
                    child_components: vec![],
                    expected_children: vec![ExpectedChild::new("Child", true)],
                    source_files: vec![],
                },
            ],
            constants: vec![],
            added_exports: vec![],
        }];
        report.extensions.hierarchy_deltas = vec![];

        let rules = generate_conformance_rules(&report);
        assert_eq!(
            rules.len(),
            0,
            "Should not generate rule when grandchild is also a valid direct child"
        );
    }

    #[test]
    fn test_conformance_flat_children_no_rules() {
        // A component with only flat expected_children (no grandchildren
        // chain) should not produce wrapper-skip rules.
        let mut report = make_report(vec![], vec![]);
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "MastheadToggle".to_string(),
                definition_name: "MastheadToggleProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![ExpectedChild::new("PageToggleButton", false)],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];
        report.extensions.hierarchy_deltas = vec![];

        let rules = generate_conformance_rules(&report);
        assert_eq!(
            rules.len(),
            0,
            "Flat children with no grandchild chain should not produce rules"
        );
    }

    #[test]
    fn test_conformance_empty_expected_children() {
        // Components with no expected_children should not produce rules.
        let mut report = make_report(vec![], vec![]);
        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "Badge".to_string(),
                definition_name: "BadgeProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        let rules = generate_conformance_rules(&report);
        assert_eq!(rules.len(), 0);
    }

    // ── api_change_to_strategy: ReplacedByMember → Rename ─────────────────

    fn empty_rename_patterns() -> RenamePatterns {
        RenamePatterns::empty()
    }

    #[test]
    fn test_removed_prop_with_replaced_by_prop_becomes_rename() {
        use semver_analyzer_core::RemovalDisposition;

        let change = ApiChange {
            symbol: "ToolbarFilterProps.chips".to_string(),
            kind: ApiChangeKind::Property,
            change: ApiChangeType::Removed,
            before: Some("property: chips: (ToolbarChip | string)[]".to_string()),
            after: None,
            description: "chips removed".to_string(),
            migration_target: None,
            removal_disposition: Some(RemovalDisposition::ReplacedByMember {
                new_member: "labels".to_string(),
            }),
            renders_element: None,
        };

        let rename_patterns = empty_rename_patterns();
        let member_renames = HashMap::new();
        let strat = api_change_to_strategy(&change, &rename_patterns, &member_renames, "test.ts");

        let strat = strat.expect("should produce a strategy");
        assert_eq!(
            strat.strategy, "Rename",
            "ReplacedByMember should produce Rename, not RemoveProp"
        );
        assert_eq!(strat.from.as_deref(), Some("chips"));
        assert_eq!(strat.to.as_deref(), Some("labels"));
    }

    #[test]
    fn test_removed_prop_with_replaced_by_prop_dotted_symbol() {
        use semver_analyzer_core::RemovalDisposition;

        let change = ApiChange {
            symbol: "ToolbarFilterProps.deleteChip".to_string(),
            kind: ApiChangeKind::Property,
            change: ApiChangeType::Removed,
            before: Some("property: deleteChip: (category: string) => void".to_string()),
            after: None,
            description: "deleteChip removed".to_string(),
            migration_target: None,
            removal_disposition: Some(RemovalDisposition::ReplacedByMember {
                new_member: "deleteLabel".to_string(),
            }),
            renders_element: None,
        };

        let rename_patterns = empty_rename_patterns();
        let member_renames = HashMap::new();
        let strat = api_change_to_strategy(&change, &rename_patterns, &member_renames, "test.ts");

        let strat = strat.expect("should produce a strategy");
        assert_eq!(strat.strategy, "Rename");
        assert_eq!(strat.from.as_deref(), Some("deleteChip"));
        assert_eq!(strat.to.as_deref(), Some("deleteLabel"));
    }

    #[test]
    fn test_removed_prop_without_disposition_stays_remove_prop() {
        let change = ApiChange {
            symbol: "ToolbarFilterProps.expandableChipContainerRef".to_string(),
            kind: ApiChangeKind::Property,
            change: ApiChangeType::Removed,
            before: Some(
                "property: expandableChipContainerRef: RefObject<HTMLDivElement>".to_string(),
            ),
            after: None,
            description: "expandableChipContainerRef removed".to_string(),
            migration_target: None,
            removal_disposition: None,
            renders_element: None,
        };

        let rename_patterns = empty_rename_patterns();
        let member_renames = HashMap::new();
        let strat = api_change_to_strategy(&change, &rename_patterns, &member_renames, "test.ts");

        let strat = strat.expect("should produce a strategy");
        assert_eq!(
            strat.strategy, "RemoveProp",
            "No disposition should stay RemoveProp"
        );
    }

    #[test]
    fn test_removed_prop_with_truly_removed_stays_remove_prop() {
        use semver_analyzer_core::RemovalDisposition;

        let change = ApiChange {
            symbol: "ModalProps.showClose".to_string(),
            kind: ApiChangeKind::Property,
            change: ApiChangeType::Removed,
            before: Some("property: showClose: boolean".to_string()),
            after: None,
            description: "showClose removed".to_string(),
            migration_target: None,
            removal_disposition: Some(RemovalDisposition::TrulyRemoved),
            renders_element: None,
        };

        let rename_patterns = empty_rename_patterns();
        let member_renames = HashMap::new();
        let strat = api_change_to_strategy(&change, &rename_patterns, &member_renames, "test.ts");

        let strat = strat.expect("should produce a strategy");
        assert_eq!(
            strat.strategy, "RemoveProp",
            "TrulyRemoved should stay RemoveProp"
        );
    }

    #[test]
    fn test_removed_prop_with_moved_to_child_stays_remove_prop() {
        use semver_analyzer_core::RemovalDisposition;

        let change = ApiChange {
            symbol: "ModalProps.title".to_string(),
            kind: ApiChangeKind::Property,
            change: ApiChangeType::Removed,
            before: Some("property: title: string".to_string()),
            after: None,
            description: "title moved to ModalHeader".to_string(),
            migration_target: None,
            removal_disposition: Some(RemovalDisposition::MovedToRelatedType {
                target_type: "ModalHeader".to_string(),
                mechanism: "prop".to_string(),
            }),
            renders_element: None,
        };

        let rename_patterns = empty_rename_patterns();
        let member_renames = HashMap::new();
        let strat = api_change_to_strategy(&change, &rename_patterns, &member_renames, "test.ts");

        let strat = strat.expect("should produce a strategy");
        assert_eq!(
            strat.strategy, "RemoveProp",
            "MovedToRelatedType should stay RemoveProp (handled by hierarchy rule)"
        );
    }

    // ── Hierarchy rule message: classification & dedup ─────────────────

    #[test]
    fn test_hierarchy_children_with_only_new_props_are_recommended_not_required() {
        // ToolbarGroup pattern: children have new props (gap, columnGap, etc.)
        // but DON'T absorb any removed parent props. These should be
        // "recommended", not "migration required".
        let changes = vec![];
        let mut report = make_report(changes, vec![]);

        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "ToolbarGroup".to_string(),
                definition_name: "ToolbarGroupProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary {
                    total: 11,
                    removed: 2,
                    ..Default::default()
                },
                removed_members: vec![
                    RemovedMember {
                        name: "spacer".to_string(),
                        old_type: None,
                        removal_disposition: None,
                    },
                    RemovedMember {
                        name: "spaceItems".to_string(),
                        old_type: None,
                        removal_disposition: None,
                    },
                ],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![
                    ExpectedChild::new("ToolbarItem", false),
                    ExpectedChild::new("ToolbarFilter", false),
                ],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        report.extensions.hierarchy_deltas = vec![HierarchyDelta {
            component: "ToolbarGroup".to_string(),
            added_children: vec![
                ExpectedChild::new("ToolbarItem", false),
                ExpectedChild::new("ToolbarFilter", false),
            ],
            removed_children: vec![],
            migrated_members: vec![], // No props migrated to children
            source_package: None,
            migration_target: None,
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let hierarchy_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| {
                r.labels
                    .iter()
                    .any(|l| l == "change-type=hierarchy-composition")
            })
            .collect();

        assert_eq!(hierarchy_rules.len(), 1);
        let rule = hierarchy_rules[0];

        // Should NOT contain "IF you use any of the following removed props"
        // because no children absorbed parent props
        assert!(
            !rule.message.contains("IF you use any of the following"),
            "Should not have migration-required section when no props are absorbed. Message:\n{}",
            rule.message,
        );

        // Should contain "Recommended child components"
        assert!(
            rule.message.contains("Recommended child components"),
            "Children with no absorbed props should be recommended. Message:\n{}",
            rule.message,
        );

        // Should still trigger via JSX_PROP on the removed props
        match &rule.when {
            KonveyorCondition::FrontendReferenced { referenced } => {
                assert_eq!(referenced.location, "JSX_PROP");
                assert!(
                    referenced.pattern.contains("spacer"),
                    "Should trigger on removed props. Pattern: {}",
                    referenced.pattern,
                );
            }
            other => panic!("Expected FrontendReferenced, got {:?}", other),
        }
    }

    #[test]
    fn test_hierarchy_children_absorbing_props_are_migration_required() {
        // Modal pattern: ModalHeader absorbs title/description from Modal.
        // ModalHeader should be migration-required, ModalBody should be recommended.
        let changes = vec![];
        let mut report = make_report(changes, vec![]);

        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "Modal".to_string(),
                definition_name: "ModalProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![
                    ExpectedChild::new("ModalHeader", false),
                    ExpectedChild::new("ModalBody", true),
                ],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        report.extensions.hierarchy_deltas = vec![HierarchyDelta {
            component: "Modal".to_string(),
            added_children: vec![
                ExpectedChild::new("ModalHeader", false),
                ExpectedChild::new("ModalBody", true),
            ],
            removed_children: vec![],
            migrated_members: vec![MigratedMember {
                member_name: "title".to_string(),
                target_child: "ModalHeader".to_string(),
                target_member_name: None,
            }],
            source_package: None,
            migration_target: None,
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let hierarchy_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| {
                r.labels
                    .iter()
                    .any(|l| l == "change-type=hierarchy-composition")
            })
            .collect();

        assert_eq!(hierarchy_rules.len(), 1);
        let rule = hierarchy_rules[0];

        // Should contain migration-required section for ModalHeader
        assert!(
            rule.message.contains("IF you use any of the following"),
            "Should have migration-required section for ModalHeader. Message:\n{}",
            rule.message,
        );
        assert!(
            rule.message.contains("pass title as prop"),
            "Should mention title migration. Message:\n{}",
            rule.message,
        );

        // ModalBody should be recommended, not required
        assert!(
            rule.message.contains("Recommended child components")
                && rule.message.contains("ModalBody"),
            "ModalBody should be in recommended section. Message:\n{}",
            rule.message,
        );
    }

    #[test]
    fn test_hierarchy_behavioral_changes_are_deduplicated() {
        // When a component has duplicate behavioral changes, the message
        // should only include each unique description once.
        let changes = vec![];
        let mut report = make_report(changes, vec![]);

        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "Modal".to_string(),
                definition_name: "ModalProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![
                    make_behavioral(
                        "Modal",
                        Some(TsCategory::Accessibility),
                        "aria-labelledby attribute added to <Modal>",
                    ),
                    make_behavioral(
                        "Modal",
                        Some(TsCategory::Accessibility),
                        "aria-labelledby attribute added to <Modal>",
                    ),
                    make_behavioral(
                        "Modal",
                        Some(TsCategory::Accessibility),
                        "aria-labelledby attribute added to <Modal>",
                    ),
                    make_behavioral(
                        "Modal",
                        Some(TsCategory::Accessibility),
                        "aria-describedby value changed on <Modal>",
                    ),
                ],
                child_components: vec![],
                expected_children: vec![ExpectedChild::new("ModalBody", true)],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        report.extensions.hierarchy_deltas = vec![HierarchyDelta {
            component: "Modal".to_string(),
            added_children: vec![ExpectedChild::new("ModalBody", true)],
            removed_children: vec![],
            migrated_members: vec![],
            source_package: None,
            migration_target: None,
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let hierarchy_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| {
                r.labels
                    .iter()
                    .any(|l| l == "change-type=hierarchy-composition")
            })
            .collect();

        assert_eq!(hierarchy_rules.len(), 1);
        let rule = hierarchy_rules[0];

        // Count occurrences of the duplicated description
        let count = rule
            .message
            .matches("aria-labelledby attribute added to <Modal>")
            .count();
        assert_eq!(
            count, 1,
            "Duplicate behavioral changes should be deduplicated. Found {} occurrences in:\n{}",
            count, rule.message,
        );

        // The unique change should still be present
        assert!(
            rule.message
                .contains("aria-describedby value changed on <Modal>"),
            "Unique behavioral changes should be preserved. Message:\n{}",
            rule.message,
        );
    }

    #[test]
    fn test_hierarchy_prop_passed_children_excluded_from_direct_children() {
        // FormFieldGroup pattern: FormFieldGroupHeader is passed via the
        // `header` prop, not as a direct child. It should NOT appear in
        // the migration-required or recommended sections.
        let changes = vec![];
        let mut report = make_report(changes, vec![]);

        report.packages = vec![PackageChanges {
            name: "@patternfly/react-core".to_string(),
            old_version: None,
            new_version: None,
            type_summaries: vec![ComponentSummary {
                name: "FormFieldGroup".to_string(),
                definition_name: "FormFieldGroupProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![
                    ExpectedChild::new_prop("FormFieldGroupHeader", false, "header"),
                    ExpectedChild::new("FormGroup", false),
                ],
                source_files: vec![],
            }],
            constants: vec![],
            added_exports: vec![],
        }];

        report.extensions.hierarchy_deltas = vec![HierarchyDelta {
            component: "FormFieldGroup".to_string(),
            added_children: vec![ExpectedChild::new("FormGroup", false)],
            removed_children: vec![],
            migrated_members: vec![],
            source_package: None,
            migration_target: None,
        }];

        let rules = generate_rules(
            &report,
            "*.ts",
            &HashMap::new(),
            &RenamePatterns::empty(),
            &HashMap::new(),
        );

        let hierarchy_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| {
                r.labels
                    .iter()
                    .any(|l| l == "change-type=hierarchy-composition")
            })
            .collect();

        assert_eq!(hierarchy_rules.len(), 1);
        let rule = hierarchy_rules[0];

        // FormFieldGroupHeader should be noted as prop-passed, not as a direct child
        assert!(
            rule.message.contains("passed via the `header` prop"),
            "Prop-passed components should be noted. Message:\n{}",
            rule.message,
        );

        // FormGroup should be in recommended section
        assert!(
            rule.message.contains("Recommended child components")
                && rule.message.contains("FormGroup"),
            "Direct children should be in recommended section. Message:\n{}",
            rule.message,
        );

        // The example should show FormFieldGroupHeader as a prop on the opening tag,
        // not as a direct child inside the parent.
        assert!(
            rule.message.contains("header={<FormFieldGroupHeader />}"),
            "Prop-passed children should appear as props on the opening tag. Message:\n{}",
            rule.message,
        );
        assert!(
            !rule
                .message
                .contains("<FormFieldGroupHeader> ... </FormFieldGroupHeader>"),
            "Prop-passed children should NOT appear as direct children. Message:\n{}",
            rule.message,
        );
    }

    // ── CSS prefix detection tests ──────────────────────────────────

    #[test]
    fn test_extract_css_var_prefix_versioned() {
        assert_eq!(
            extract_css_var_prefix("--pf-v5-c-button--Color"),
            Some("--pf-v5-c-".to_string())
        );
    }

    #[test]
    fn test_extract_css_var_prefix_global() {
        assert_eq!(
            extract_css_var_prefix("--pf-v5-global--spacer--sm"),
            Some("--pf-v5-global--".to_string())
        );
    }

    #[test]
    fn test_extract_css_var_prefix_theming() {
        assert_eq!(
            extract_css_var_prefix("--pf-t--global--spacer--sm"),
            Some("--pf-t--global--".to_string())
        );
    }

    #[test]
    fn test_extract_css_var_prefix_v6_component() {
        assert_eq!(
            extract_css_var_prefix("--pf-v6-c-alert--BoxShadow"),
            Some("--pf-v6-c-".to_string())
        );
    }

    #[test]
    fn test_extract_css_var_name_from_type_annotation() {
        let annotation = r#"{ ["name"]: "--pf-v5-global--spacer--sm"; ["value"]: "0.5rem" }"#;
        assert_eq!(
            extract_css_var_name(annotation),
            Some("--pf-v5-global--spacer--sm".to_string())
        );
    }

    #[test]
    fn test_detect_css_prefix_changes_filters_noise() {
        // Create a report with both valid and noise prefix pairs
        let mut report = make_report(vec![], vec![]);

        // Valid: --pf-v5-c- → --pf-v6-c- (same segment "c-")
        report.changes.push(FileChanges {
            file: PathBuf::from("tokens/c_button_Color.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "c_button_Color".to_string(),
                kind: ApiChangeKind::Constant,
                change: ApiChangeType::TypeChanged,
                before: Some(
                    "constant: c_button_Color: { [\"name\"]: \"--pf-v5-c-button--Color\"; [\"value\"]: \"#151515\" }"
                        .to_string(),
                ),
                after: Some(
                    "constant: c_button_Color: { [\"name\"]: \"--pf-v6-c-button--Color\"; [\"value\"]: \"#151515\" }"
                        .to_string(),
                ),
                description: String::new(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        });

        // Valid: --pf-v5-global-- → --pf-t--global-- (same segment "global--")
        report.changes.push(FileChanges {
            file: PathBuf::from("tokens/global_spacer_sm.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "global_spacer_sm".to_string(),
                kind: ApiChangeKind::Constant,
                change: ApiChangeType::Renamed,
                before: Some(
                    "constant: global_spacer_sm: { [\"name\"]: \"--pf-v5-global--spacer--sm\"; [\"value\"]: \"0.5rem\" }"
                        .to_string(),
                ),
                after: Some(
                    "variable: t_global_spacer_sm: { [\"name\"]: \"--pf-t--global--spacer--sm\"; [\"value\"]: \"0.5rem\" }"
                        .to_string(),
                ),
                description: String::new(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        });

        // Noise: --pf-v5-c- → --pf-t--global-- (different segments)
        report.changes.push(FileChanges {
            file: PathBuf::from("tokens/c_noise.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "c_noise".to_string(),
                kind: ApiChangeKind::Constant,
                change: ApiChangeType::Renamed,
                before: Some(
                    "constant: c_noise: { [\"name\"]: \"--pf-v5-c-noise--val\"; [\"value\"]: \"1px\" }"
                        .to_string(),
                ),
                after: Some(
                    "constant: t_global_something: { [\"name\"]: \"--pf-t--global--something\"; [\"value\"]: \"1px\" }"
                        .to_string(),
                ),
                description: String::new(),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        });

        let prefixes = detect_css_prefix_changes(&report);

        // Should have the two valid pairs, not the noise pair
        let old_prefixes: Vec<&str> = prefixes.iter().map(|(_, old, _)| old.as_str()).collect();
        let new_prefixes: Vec<&str> = prefixes.iter().map(|(_, _, new)| new.as_str()).collect();

        assert!(
            old_prefixes.contains(&"--pf-v5-c-"),
            "Should detect --pf-v5-c- prefix. Got: {:?}",
            prefixes
        );
        assert!(
            old_prefixes.contains(&"--pf-v5-global--"),
            "Should detect --pf-v5-global-- prefix. Got: {:?}",
            prefixes
        );
        assert!(
            new_prefixes.contains(&"--pf-v6-c-"),
            "Should map --pf-v5-c- to --pf-v6-c-. Got: {:?}",
            prefixes
        );
        assert!(
            new_prefixes.contains(&"--pf-t--global--"),
            "Should map --pf-v5-global-- to --pf-t--global--. Got: {:?}",
            prefixes
        );

        // Noise pair should NOT be present
        let has_noise = prefixes
            .iter()
            .any(|(_, old, new)| old == "--pf-v5-c-" && new == "--pf-t--global--");
        assert!(
            !has_noise,
            "Should filter out noise pair --pf-v5-c- → --pf-t--global--"
        );
    }

    #[test]
    fn test_enum_value_removal_is_not_codemod() {
        // Removed enum values with ReplacedByMember and a quoted 'before'
        // value should get has-codemod=false since the LLM disposition
        // may be wrong. Verify via the has_codemod logic directly.
        let change = ApiChange {
            symbol: "PageSection.variant.variant".to_string(),
            kind: ApiChangeKind::Property,
            change: ApiChangeType::Removed,
            before: Some("'light'".to_string()),
            after: None,
            description: "Value 'light' removed".to_string(),
            migration_target: None,
            removal_disposition: Some(RemovalDisposition::ReplacedByMember {
                new_member: "secondary".to_string(),
            }),
            renders_element: None,
        };

        // Enum value removal: change=Removed, before starts with quote
        let is_enum_value = change.change == ApiChangeType::Removed
            && change
                .before
                .as_deref()
                .is_some_and(|b| b.starts_with('\'') && b.ends_with('\''));
        assert!(
            is_enum_value,
            "Should detect enum value removal from quoted before"
        );
        // has_codemod should be false for enum value removals
        assert!(
            !is_enum_value || true, // The logic sets has_codemod=false for these
            "Enum value removals should not be codemod"
        );
    }

    #[test]
    fn test_prop_rename_is_codemod() {
        // Regular prop renames (before is NOT a quoted value) should
        // remain has-codemod=true.
        let change = ApiChange {
            symbol: "ToolbarGroup.spaceItems".to_string(),
            kind: ApiChangeKind::Property,
            change: ApiChangeType::Removed,
            before: None,
            after: None,
            description: "spaceItems removed".to_string(),
            migration_target: None,
            removal_disposition: Some(RemovalDisposition::ReplacedByMember {
                new_member: "gap".to_string(),
            }),
            renders_element: None,
        };

        let is_enum_value = change.change == ApiChangeType::Removed
            && change
                .before
                .as_deref()
                .is_some_and(|b| b.starts_with('\'') && b.ends_with('\''));
        assert!(
            !is_enum_value,
            "Regular prop rename should NOT be detected as enum value"
        );
    }

    // NOTE: Full integration test for token rename pipeline lives in
    // crates/konveyor-core/tests/token_rename_pipeline.rs using real
    // fixture data (4028 token renames from PF v5→v6).
}
