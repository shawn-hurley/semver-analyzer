//! v2 Konveyor rule generation from SD pipeline results.
//!
//! Generates flat, precise rules from:
//! - Composition changes (new required wrappers, family restructuring)
//! - Composition trees (conformance: parent-child validation)
//! - Context dependency changes (provider/consumer changes)
//! - Prop↔child migration (TD removed props × SD new children)
//!
//! Rules are designed to be consumed by a fix-engine that aggregates
//! related incidents per component and builds LLM prompts. Each rule
//! fires on exactly one thing (a specific prop, component, or import)
//! and carries machine-readable fix_strategy metadata.

use crate::sd_types::{
    ChildRelationship, CompositionChangeType, CompositionTree, ConformanceCheck,
    ConformanceCheckType, SdPipelineResult, SourceLevelCategory, SourceLevelChange,
};
use semver_analyzer_core::types::MigrationTarget;
use semver_analyzer_core::{AnalysisReport, ApiChangeType};
use semver_analyzer_konveyor_core::{
    FileContentFields, FixStrategyEntry, FrontendPatternFields, FrontendReferencedFields,
    KonveyorCondition, KonveyorRule,
};

use crate::TypeScript;
use semver_analyzer_konveyor_core::resolve_npm_package;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Generate v2 rules from SD pipeline results + TD structural data.
///
/// Returns rules that are appended to the v1 TD-generated rules.
/// The v1 rules handle renamed/removed props, type changes, CSS prefixes,
/// manifests, and dependency updates. The v2 rules add:
/// - Composition migration rules
/// - Conformance rules
/// - Context dependency rules
/// - Prop↔child migration rules (cross-referencing TD + SD)
pub fn generate_sd_rules(
    report: &AnalysisReport<TypeScript>,
    sd: &SdPipelineResult,
    pkg_cache: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    // Build component → package lookup from SD profiles
    let component_packages = build_component_package_map(sd, pkg_cache);

    // ── Composition change rules ────────────────────────────────────
    rules.extend(generate_composition_change_rules(sd, &component_packages));

    // ── Conformance rules ───────────────────────────────────────────
    rules.extend(generate_conformance_rules(
        &sd.composition_trees,
        &sd.conformance_checks,
        &component_packages,
    ));

    // ── Context dependency rules ────────────────────────────────────
    rules.extend(generate_context_rules(
        &sd.source_level_changes,
        &component_packages,
    ));

    // ── Prop↔child migration rules ──────────────────────────────────
    rules.extend(generate_prop_child_migration_rules(
        report,
        sd,
        &component_packages,
    ));

    // ── Cross-family child→prop migration rules ───────────────────────
    rules.extend(generate_cross_family_child_to_prop_rules(
        report,
        sd,
        &component_packages,
    ));

    // ── Deprecated↔main migration rules ─────────────────────────────
    rules.extend(generate_deprecated_migration_rules(sd, &component_packages));

    // ── Prop value conformance rules ────────────────────────────────
    rules.extend(generate_prop_value_conformance_rules(
        report,
        sd,
        &component_packages,
    ));

    // ── Required prop added rules ───────────────────────────────────
    rules.extend(generate_required_prop_added_rules(sd, &component_packages));

    // ── Test impact rules ───────────────────────────────────────────
    rules.extend(generate_test_impact_rules(
        &sd.source_level_changes,
        &component_packages,
    ));

    // ── Portal prop rules (appendTo string values, removed props) ──
    rules.extend(generate_portal_prop_rules(
        &sd.source_level_changes,
        &component_packages,
    ));

    // ── Composition inversion rules (internal → render prop) ──────
    rules.extend(generate_composition_inversion_rules(
        sd,
        &component_packages,
    ));

    // ── Prop attribute override rules ──────────────────────────────
    rules.extend(generate_prop_attribute_override_rules(
        &sd.source_level_changes,
        sd,
        &component_packages,
    ));

    // ── CSS class removal rules ─────────────────────────────────────
    rules.extend(generate_css_class_removal_rules(&sd.removed_css_blocks));

    // ── Removed CSS entry point file rules ──────────────────────────
    rules.extend(generate_removed_css_file_rules(
        &sd.removed_css_entry_files,
    ));

    // ── Deprecated prop rules (@deprecated JSDoc) ────────────────────
    rules.extend(generate_deprecated_prop_rules(
        &sd.source_level_changes,
        &component_packages,
    ));

    // ── Dead CSS class rules (prefix swap produces non-existent class) ──
    rules.extend(generate_dead_css_class_rules(
        &sd.dead_css_classes_after_swap,
    ));

    // ── Enumerated CSS class rules ──────────────────────────────────
    // When CSS inventories are available, generate individual per-class
    // rules instead of relying on the catch-all prefix swap from v1.
    if !sd.old_css_class_inventory.is_empty() && !sd.new_css_class_inventory.is_empty() {
        let enumerated = generate_enumerated_css_class_rules(
            &sd.old_css_class_inventory,
            &sd.new_css_class_inventory,
        );
        if !enumerated.is_empty() {
            tracing::info!(
                count = enumerated.len(),
                "Enumerated CSS class rules will replace catch-all prefix rule"
            );
            rules.extend(enumerated);
        }
    }

    rules
}

/// Build a map from component name → npm package name.
///
/// Priority:
/// 1. Pre-computed `sd.component_packages` (available in saved reports)
/// 2. SD profiles' `file` field resolved via `pkg_cache` (available during pipeline run)
/// 3. Source-level change `component` field matched to file changes in the report
fn build_component_package_map(
    sd: &SdPipelineResult,
    pkg_cache: &HashMap<String, String>,
) -> HashMap<String, String> {
    // If the SD result already has the map (from a saved report), use it
    if !sd.component_packages.is_empty() {
        return sd.component_packages.clone();
    }

    // Build from profiles + pkg_cache
    let mut map = HashMap::new();
    for (name, profile) in &sd.new_profiles {
        if let Some(pkg) = resolve_npm_package(&profile.file, pkg_cache) {
            map.insert(name.clone(), pkg);
        }
    }
    for (name, profile) in &sd.old_profiles {
        if !map.contains_key(name) {
            if let Some(pkg) = resolve_npm_package(&profile.file, pkg_cache) {
                map.insert(name.clone(), pkg);
            }
        }
    }
    map
}

/// Look up the package for a component, with fallback.
fn pkg_for(component: &str, map: &HashMap<String, String>) -> String {
    map.get(component)
        .cloned()
        .unwrap_or_else(|| "@patternfly/react-core".to_string())
}

/// Look up the package for a component in a potentially deprecated family.
/// If the family root starts with "deprecated/" and the resolved package
/// doesn't already contain "/deprecated", appends "/deprecated" to scope
/// the rule to the deprecated import path.
fn pkg_for_deprecated(component: &str, family_root: &str, map: &HashMap<String, String>) -> String {
    let base = pkg_for(component, map);
    if family_root.starts_with("deprecated/") && !base.contains("/deprecated") {
        format!("{}/deprecated", base)
    } else {
        base
    }
}

/// Resolve the deprecated import package from a `migration_from` path.
///
/// Example: `"packages/react-core/src/deprecated/components/Select/Select.tsx"`
///        → `"@patternfly/react-core/deprecated"`
fn deprecated_pkg_from_migration_path(path: &str) -> String {
    // Extract the package directory name (e.g., "react-core" from "packages/react-core/...")
    if let Some(pkg_dir) = path
        .strip_prefix("packages/")
        .and_then(|s| s.split('/').next())
    {
        format!("@patternfly/{}/deprecated", pkg_dir)
    } else {
        "@patternfly/react-core/deprecated".to_string()
    }
}

/// Return a rule ID prefix based on whether this is a migration change.
/// Migration changes use "sd-migration-" to avoid colliding with
/// same-component evolution rules.
fn rule_prefix(migration_from: &Option<String>) -> &'static str {
    if migration_from.is_some() {
        "sd-migration"
    } else {
        "sd"
    }
}

// ── Composition change rules ────────────────────────────────────────────

fn generate_composition_change_rules(
    sd: &SdPipelineResult,
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    // Build a lookup of family members that are prop-passed on the root.
    // These should NOT be restructured as children by the LLM.
    // A member is prop-passed when:
    //   - It's a family member with no edge in the composition tree
    //   - The root has a ReactNode/ComponentType prop whose name matches
    let mut prop_passed_members: HashMap<String, Vec<String>> = HashMap::new();
    for tree in &sd.composition_trees {
        let root = &tree.root;
        let children_in_edges: HashSet<&str> =
            tree.edges.iter().map(|e| e.child.as_str()).collect();

        let root_prop_types = sd.new_component_prop_types.get(root);

        for member in &tree.family_members {
            if member == root {
                continue;
            }
            // Member has no edge — it's not a direct child or internal
            if children_in_edges.contains(member.as_str()) {
                continue;
            }
            // Check if a ReactNode prop on root matches this member
            if let Some(prop_types) = root_prop_types {
                let suffix = member.strip_prefix(root.as_str()).unwrap_or("");
                if !suffix.is_empty() {
                    let suffix_lower = suffix.to_lowercase();
                    for (prop_name, prop_type) in prop_types {
                        if prop_name == "children" {
                            continue;
                        }
                        if !prop_type.contains("ReactNode") && !prop_type.contains("ComponentType")
                        {
                            continue;
                        }
                        let prop_lower = prop_name.to_lowercase();
                        if suffix_lower.starts_with(&prop_lower)
                            || prop_lower.starts_with(&suffix_lower)
                        {
                            prop_passed_members
                                .entry(root.clone())
                                .or_default()
                                .push(format!("{} (via `{}` prop)", member, prop_name));
                        }
                    }
                }
            }
        }
    }

    for change in &sd.composition_changes {
        match &change.change_type {
            CompositionChangeType::NewRequiredChild { .. } => {
                // Skip — conformance rules already validate parent-child
                // relationships from the child's perspective (notParent).
                // Generating a "requires" rule from the parent's perspective
                // is redundant and produces false positives on code where the
                // child component is already present.
            }
            CompositionChangeType::FamilyMemberAdded { .. } => {
                // Skip — the migration rule (component-import-deprecated)
                // already lists new child components in its message with
                // guidance on how to use them. Generating a new-member rule
                // fires on every parent usage regardless of whether the new
                // component is already in use, adding noise. If the new
                // component is required, conformance rules handle it.
            }
            CompositionChangeType::FamilyMemberRemoved { member } => {
                let pkg = pkg_for(member, component_packages);
                let rule_id = format!(
                    "sd-composition-{}-removed-member-{}",
                    sanitize(&change.family),
                    sanitize(member)
                );

                rules.push(KonveyorRule {
                    rule_id,
                    labels: vec![
                        "source=semver-analyzer".into(),
                        "change-type=composition".into(),
                        format!("package={}", pkg),
                        format!("family={}", change.family),
                    ],
                    effort: 3,
                    category: "mandatory".into(),
                    description: change.description.clone(),
                    message: format!(
                        "<{}> has been removed from the {} family.\n\
                         Remove usages or replace with the recommended alternative.",
                        member, change.family
                    ),
                    links: vec![],
                    when: KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: format!("^{}$", member),
                            location: "JSX_COMPONENT".into(),
                            component: None,
                            parent: None,
                            parent_from: None,
                            not_parent: None,
                            child: None,
                            not_child: None,
                            requires_child: None,
                            value: None,
                            from: Some(pkg.to_string()),
                            file_pattern: None,
                        },
                    },
                    fix_strategy: Some(FixStrategyEntry {
                        strategy: "Manual".into(),
                        from: Some(member.clone()),
                        ..Default::default()
                    }),
                });
            }
            _ => {}
        }
    }

    rules
}

// ── Conformance rules ───────────────────────────────────────────────────
//
// Conformance rule IDs use abbreviated segments to keep IDs short.
// Component names are shortened by stripping the family root prefix
// (e.g., `DualListSelectorControl` → `control` in the `DualListSelector` family).
// When stripping would produce an empty string (component == family root),
// the full name is kept.
//
// Abbreviation scheme:
//   conformance → cf
//   must-be-in  → in
//   requires    → req
//   requires-wrapper → req-wrap
//
// Rule ID formats:
//   notParent:         sd-cf-{family}-{child}-in-{parent1-or-parent2}
//   invalidDirectChild: sd-cf-{family}-{child}-not-in-{grandparent}-use-{parent1-or-parent2}
//   requiresChild:     sd-cf-{family}-{parent}-req-{child1-and-child2}
//   exclusiveWrapper:  sd-cf-{family}-{parent}-req-wrap
//
// Examples:
//   sd-cf-duallistselector-control-in-list-or-tree
//   sd-cf-table-td-not-in-table-use-tr
//   sd-cf-tabs-tabs-req-tab
//   sd-cf-deprecated-duallistselector-control-in-list-or-tree

fn generate_conformance_rules(
    trees: &[CompositionTree],
    conformance_checks: &[ConformanceCheck],
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    for tree in trees {
        // ── Step 1: Build children needing notParent rules.
        //
        // Children with at least one incoming edge that has
        // child_requires_parent (CHP) — i.e., Required or Structural edges.
        // These children MUST be placed inside their parent when used.
        //
        // PropPassed edges are included for notParent (the scanner correctly
        // tracks parent_name through prop expressions).
        let mut children_needing_not_parent: HashSet<&str> = HashSet::new();
        for edge in &tree.edges {
            if edge.relationship != ChildRelationship::Internal
                && edge.strength.child_requires_parent()
            {
                children_needing_not_parent.insert(edge.child.as_str());
            }
        }

        // ── Step 2: Build parent → PMC children map.
        //
        // Edges where parent_requires_child (PMC) — i.e., Required or Wrapper.
        // These parents MUST contain these children.
        //
        // PropPassed edges are excluded because the requiresChild scanner
        // only checks direct JSX children (el.children), not prop value
        // expressions. A prop-passed child like <Tab actions={<TabAction/>}/>
        // is invisible to the scanner and would cause guaranteed FPs.
        let mut parent_to_req_children: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in &tree.edges {
            if edge.strength.parent_requires_child()
                && edge.relationship != ChildRelationship::Internal
                && edge.relationship != ChildRelationship::PropPassed
            {
                parent_to_req_children
                    .entry(edge.parent.as_str())
                    .or_default()
                    .push(edge.child.as_str());
            }
        }

        // ── Step 3: Build child → all parents map.
        //
        // ALL non-internal edges (all strengths). Used for the notParent
        // regex so valid-but-not-required placements don't trigger false
        // positives, and for InvalidDirectChild grandparent lookup.
        let mut child_to_all_parents: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in &tree.edges {
            if edge.relationship != ChildRelationship::Internal {
                child_to_all_parents
                    .entry(edge.child.as_str())
                    .or_default()
                    .push(edge.parent.as_str());
            }
        }

        // ── Step 3b: Build parent → all children map (all strengths).
        //
        // Used for the requiresChild scanner regex. Including non-PMC children
        // prevents false positives when a parent has valid-but-not-required
        // children (e.g., ToolbarContent with ToolbarGroup/ToolbarItem).
        // The `parent_to_req_children` map still determines WHICH parents get
        // requiresChild rules — this map only expands the scanner regex.
        //
        // PropPassed edges are excluded (same reason as Step 2 — the scanner
        // can only see direct JSX children, not prop values).
        let mut parent_to_all_children: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in &tree.edges {
            if edge.relationship != ChildRelationship::Internal
                && edge.relationship != ChildRelationship::PropPassed
            {
                parent_to_all_children
                    .entry(edge.parent.as_str())
                    .or_default()
                    .push(edge.child.as_str());
            }
        }

        // ── Step 4: Generate rules.
        //
        // Two independent rule types based on the two dimensions:
        //
        //   notParent rule on child:
        //     Generated for children in children_needing_not_parent.
        //     "Td must be inside Tr" — child has CHP edge.
        //     Scanner: pattern=Td, notParent=^(Tr)$
        //
        //   requiresChild rule on parent:
        //     Generated for parents in parent_to_req_children.
        //     "ToggleGroup must contain ToggleGroupItem" — parent has PMC edge.
        //     Scanner: pattern=ToggleGroup, requiresChild=^(ToggleGroupItem)$

        // Extract the base family name for root comparison.
        // "deprecated/DualListSelector" → "DualListSelector", "Alert" → "Alert"
        let base_root = tree.root.rsplit('/').next().unwrap_or(&tree.root);

        // For deprecated families, scope the `from` field to the deprecated
        // import path (e.g., "@patternfly/react-core/deprecated"). Without
        // this, deprecated conformance rules share identical `when` clauses
        // with v6 rules because both families use the same component names
        // from the same base package.
        let family_root = &tree.root;
        let pkg_for_family = |component: &str| -> String {
            pkg_for_deprecated(component, family_root, component_packages)
        };

        // 4b: Generate notParent rules (child must be inside parent).
        for child in &children_needing_not_parent {
            // Skip notParent rules for the family root component. A family root
            // is standalone by definition — it can exist outside any parent.
            // Examples: Alert does not require AlertGroup, ChartDonutUtilization
            // does not require ChartDonutThreshold.
            if *child == base_root {
                continue;
            }

            let pkg = pkg_for_family(child);

            // Use ALL parents (Required + Allowed) for the notParent regex
            // so valid-but-not-required placements don't trigger false positives.
            let all_parents = child_to_all_parents
                .get(child)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);

            let mut sorted_parents: Vec<&str> = all_parents.to_vec();
            sorted_parents.sort();
            sorted_parents.dedup();

            let not_parent_pattern = if sorted_parents.len() == 1 {
                format!("^{}$", sorted_parents[0])
            } else {
                format!("^({})$", sorted_parents.join("|"))
            };

            let rule_id_suffix = sorted_parents
                .iter()
                .map(|p| short_component_id(p, &tree.root))
                .collect::<Vec<_>>()
                .join("-or-");
            let rule_id = format!(
                "sd-cf-{}-{}-in-{}",
                sanitize(&tree.root),
                short_component_id(child, &tree.root),
                rule_id_suffix,
            );

            let parent_list = sorted_parents.join(" or ");

            let message = if sorted_parents.len() == 1 {
                format!(
                    "<{}> must be used inside <{}>.\n\n\
                     Correct usage:\n  <{}>\n    <{} />\n  </{}>",
                    child, sorted_parents[0], sorted_parents[0], child, sorted_parents[0],
                )
            } else {
                let examples: Vec<String> = sorted_parents
                    .iter()
                    .map(|p| format!("  <{}>\n    <{} />\n  </{}>", p, child, p))
                    .collect();
                format!(
                    "<{}> must be used inside {}.\n\n\
                     Correct usage (either):\n{}",
                    child,
                    parent_list,
                    examples.join("\n  or\n"),
                )
            };

            rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".into(),
                    "change-type=conformance".into(),
                    format!("package={}", pkg),
                    format!("family={}", tree.root),
                ],
                effort: 1,
                category: "mandatory".into(),
                description: format!("<{}> must be a child of {}", child, parent_list),
                message,
                links: vec![],
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: format!("^{}$", child),
                        location: "JSX_COMPONENT".into(),
                        component: None,
                        parent: None,
                        not_parent: Some(not_parent_pattern),
                        child: None,
                        not_child: None,
                        requires_child: None,
                        parent_from: None,
                        value: None,
                        from: Some(pkg.to_string()),
                        file_pattern: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry {
                    strategy: "LlmAssisted".into(),
                    component: Some(child.to_string()),
                    replacement: Some(sorted_parents[0].to_string()),
                    ..Default::default()
                }),
            });

            // Build set of CHP parents for this child — parents where the child
            // has a Required or Structural edge (child_requires_parent = true).
            //
            // Used for two purposes:
            // 1. The grandparent walk only follows CHP parents in the first
            //    hop. Allowed parents (CSS descendant matches between peer
            //    components) create false intermediate paths and generate
            //    noise rules like "DLDescription not-in DLTermHelpText, use
            //    DLTerm" when Term and TermHelpText are actually peers.
            // 2. If the grandparent is already a CHP parent, the child IS a
            //    valid direct child of that grandparent — the invalidDirectChild
            //    rule would contradict the notParent rule.
            let chp_parents: HashSet<&str> = tree
                .edges
                .iter()
                .filter(|e| {
                    e.child == *child
                        && e.relationship != ChildRelationship::Internal
                        && e.strength.child_requires_parent()
                })
                .map(|e| e.parent.as_str())
                .collect();

            // ── InvalidDirectChild: child inside grandparent, skipping parent.
            //
            // For each CHP parent of this child, look up that parent's own
            // parents (grandparents of the child). Group by grandparent to
            // merge when multiple parents share the same grandparent (e.g.,
            // Tr in Table needs either Thead or Tbody).
            //
            // Only CHP parents are walked (first hop) because Allowed parents
            // represent weak CSS descendant signals between peer components,
            // not real parent-child API constraints. The second hop (parent →
            // grandparent) uses ALL parents to find all valid ancestors.
            let mut grandparent_to_expected: HashMap<&str, Vec<&str>> = HashMap::new();
            for parent in &sorted_parents {
                // First hop: only follow CHP parents
                if !chp_parents.contains(parent) {
                    continue;
                }
                if let Some(grandparents) = child_to_all_parents.get(parent) {
                    for grandparent in grandparents {
                        grandparent_to_expected
                            .entry(grandparent)
                            .or_default()
                            .push(parent);
                    }
                }
            }

            for (grandparent, expected_parents) in &grandparent_to_expected {
                // Suppress when the child already has a CHP edge to the
                // grandparent. The child is a valid direct child there, so
                // "X should not be directly in G" is wrong.
                if chp_parents.contains(grandparent) {
                    continue;
                }
                let mut unique_parents: Vec<&str> = expected_parents.clone();
                unique_parents.sort();
                unique_parents.dedup();

                let parent_list = unique_parents.join(" or ");
                let rule_id_suffix = unique_parents
                    .iter()
                    .map(|p| short_component_id(p, &tree.root))
                    .collect::<Vec<_>>()
                    .join("-or-");
                let rule_id = format!(
                    "sd-cf-{}-{}-not-in-{}-use-{}",
                    sanitize(&tree.root),
                    short_component_id(child, &tree.root),
                    short_component_id(grandparent, &tree.root),
                    rule_id_suffix,
                );

                let message = if unique_parents.len() == 1 {
                    format!(
                        "<{}> should be wrapped in <{}> inside <{}>.\n\n\
                         Replace:\n  <{}>\n    <{} />\n  </{}>\n\n\
                         With:\n  <{}>\n    <{}>\n      <{} />\n    </{}>\n  </{}>",
                        child,
                        unique_parents[0],
                        grandparent,
                        grandparent,
                        child,
                        grandparent,
                        grandparent,
                        unique_parents[0],
                        child,
                        unique_parents[0],
                        grandparent,
                    )
                } else {
                    let examples: Vec<String> = unique_parents
                        .iter()
                        .map(|p| {
                            format!(
                                "  <{}>\n    <{}>\n      <{} />\n    </{}>\n  </{}>",
                                grandparent, p, child, p, grandparent,
                            )
                        })
                        .collect();
                    format!(
                        "<{}> should be wrapped in {} inside <{}>.\n\n\
                         Replace:\n  <{}>\n    <{} />\n  </{}>\n\n\
                         With (either):\n{}",
                        child,
                        parent_list,
                        grandparent,
                        grandparent,
                        child,
                        grandparent,
                        examples.join("\n  or\n"),
                    )
                };

                rules.push(KonveyorRule {
                    rule_id,
                    labels: vec![
                        "source=semver-analyzer".into(),
                        "change-type=conformance".into(),
                        format!("package={}", pkg),
                        format!("family={}", tree.root),
                    ],
                    effort: 3,
                    category: "mandatory".into(),
                    description: format!(
                        "<{}> must be inside {}, not directly in <{}>",
                        child, parent_list, grandparent
                    ),
                    message,
                    links: vec![],
                    when: KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: format!("^{}$", child),
                            location: "JSX_COMPONENT".into(),
                            component: None,
                            parent: Some(format!("^{}$", grandparent)),
                            parent_from: Some(pkg.to_string()),
                            not_parent: None,
                            child: None,
                            not_child: None,
                            requires_child: None,
                            value: None,
                            from: Some(pkg.to_string()),
                            file_pattern: None,
                        },
                    },
                    fix_strategy: Some(FixStrategyEntry {
                        strategy: "CompositionChange".into(),
                        component: Some(child.to_string()),
                        replacement: Some(unique_parents[0].to_string()),
                        ..Default::default()
                    }),
                });
            }
        }

        // 4c: Generate requiresChild rules (parent must contain children).
        //
        // For parents with PMC edges (Required or Wrapper), the constraint
        // is "if you use this component, it must contain these children."
        //
        // The scanner regex uses ALL children (all strengths) so that
        // valid-but-optional children don't trigger false positives. The rule
        // still only fires on parents that have PMC children (from
        // parent_to_req_children), and the description lists the PMC ones.
        for (parent, children) in &parent_to_req_children {
            let pkg = pkg_for_family(parent);
            let mut sorted_children: Vec<&str> = children.clone();
            sorted_children.sort();
            sorted_children.dedup();

            // Use ALL children (Required + Allowed) for the scanner regex to
            // avoid false positives when valid-but-Allowed children are present.
            let all_children = parent_to_all_children
                .get(parent)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let mut sorted_all: Vec<&str> = all_children.to_vec();
            sorted_all.sort();
            sorted_all.dedup();
            let children_pattern = format!("^({})$", sorted_all.join("|"));
            let children_list = sorted_all.join(" or ");

            let rule_id_suffix = sorted_all
                .iter()
                .map(|c| short_component_id(c, &tree.root))
                .collect::<Vec<_>>()
                .join("-and-");
            let rule_id = format!(
                "sd-cf-{}-{}-req-{}",
                sanitize(&tree.root),
                short_component_id(parent, &tree.root),
                rule_id_suffix,
            );

            let message = format!(
                "<{}> must contain at least one {} child component.\n\n\
                 Correct usage:\n  <{}>\n    <{} />\n  </{}>",
                parent, children_list, parent, sorted_all[0], parent,
            );

            rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".into(),
                    "change-type=conformance".into(),
                    format!("package={}", pkg),
                    format!("family={}", tree.root),
                ],
                effort: 1,
                category: "mandatory".into(),
                description: format!("<{}> must contain {} children", parent, children_list),
                message,
                links: vec![],
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: format!("^{}$", parent),
                        location: "JSX_COMPONENT".into(),
                        component: None,
                        parent: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: Some(children_pattern),
                        parent_from: None,
                        value: None,
                        from: Some(pkg.to_string()),
                        file_pattern: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry {
                    strategy: "LlmAssisted".into(),
                    component: Some(parent.to_string()),
                    replacement: Some(sorted_all.join(", ")),
                    ..Default::default()
                }),
            });
        }
    }

    // ── ExclusiveWrapper: all children must be a specific wrapper
    for check in conformance_checks {
        if let ConformanceCheckType::ExclusiveWrapper {
            parent,
            allowed_children,
        } = &check.check_type
        {
            let pkg = pkg_for_deprecated(parent, &check.family, component_packages);
            let allowed_pattern = format!("^({})$", allowed_children.join("|"));
            let allowed_list = allowed_children.join(" or ");

            let rule_id = format!(
                "sd-cf-{}-{}-req-wrap",
                sanitize(&check.family),
                short_component_id(parent, &check.family),
            );

            rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".into(),
                    "change-type=conformance".into(),
                    format!("package={}", pkg),
                    format!("family={}", check.family),
                ],
                effort: 3,
                category: "mandatory".into(),
                description: format!(
                    "All children of <{}> must be wrapped in {}",
                    parent, allowed_list
                ),
                message: format!(
                    "Components placed directly inside <{}> must be wrapped in <{}>.\n\n\
                     Replace:\n  <{}>\n    <SomeComponent />\n  </{}>\n\n\
                     With:\n  <{}>\n    <{}>\n      <SomeComponent />\n    </{}>\n  </{}>",
                    parent,
                    allowed_children.first().unwrap_or(&parent.clone()),
                    parent,
                    parent,
                    parent,
                    allowed_children.first().unwrap_or(&parent.clone()),
                    allowed_children.first().unwrap_or(&parent.clone()),
                    parent,
                ),
                links: vec![],
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: format!("^{}$", parent),
                        location: "JSX_COMPONENT".into(),
                        component: None,
                        parent: None,
                        not_parent: None,
                        child: None,
                        not_child: Some(allowed_pattern),
                        requires_child: None,
                        parent_from: None,
                        value: None,
                        from: Some(pkg.to_string()),
                        file_pattern: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry {
                    strategy: "LlmAssisted".into(),
                    component: Some(parent.clone()),
                    replacement: Some(allowed_children.first().unwrap_or(&parent.clone()).clone()),
                    ..Default::default()
                }),
            });
        }
    }

    rules
}

// ── Context dependency rules ────────────────────────────────────────────

fn generate_context_rules(
    changes: &[SourceLevelChange],
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    for change in changes {
        if change.category != SourceLevelCategory::ContextDependency {
            continue;
        }

        // Extract context name from old_value or new_value
        let context_name = change
            .new_value
            .as_ref()
            .or(change.old_value.as_ref())
            .and_then(|v| {
                // Values are like "useContext(MenuContext)" or "<MenuContext.Provider>"
                v.strip_prefix("useContext(")
                    .and_then(|s| s.strip_suffix(')'))
                    .or_else(|| {
                        v.strip_prefix('<')
                            .and_then(|s| s.strip_suffix(".Provider>"))
                    })
            });

        let Some(ctx_name) = context_name else {
            continue;
        };

        let pkg = pkg_for(&change.component, component_packages);
        let prefix = rule_prefix(&change.migration_from);
        let rule_id = format!(
            "{}-context-{}-{}",
            prefix,
            sanitize(&change.component),
            sanitize(ctx_name),
        );

        // For migration changes, match imports from the deprecated path.
        // For evolution changes, match imports from the current package.
        let from_pkg = if let Some(ref mf) = change.migration_from {
            deprecated_pkg_from_migration_path(mf)
        } else {
            pkg.clone()
        };

        // Fire on import of the context — consumers who directly import
        // and use the context are affected.
        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".into(),
                "change-type=context-dependency".into(),
                format!("package={}", from_pkg),
                format!("component={}", change.component),
            ],
            effort: 3,
            category: "mandatory".into(),
            description: change.description.clone(),
            message: format!(
                "{}\n\n\
                 If you import and use {} directly, review your usage.\n\
                 The context shape or provider location may have changed.",
                change.description, ctx_name,
            ),
            links: vec![],
            when: KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: format!("^{}$", ctx_name),
                    location: "IMPORT".into(),
                    component: None,
                    parent: None,
                    parent_from: None,
                    not_parent: None,
                    child: None,
                    not_child: None,
                    requires_child: None,
                    value: None,
                    from: Some(from_pkg),
                    file_pattern: None,
                },
            },
            fix_strategy: Some(FixStrategyEntry {
                strategy: "Manual".into(),
                component: Some(change.component.clone()),
                from: change.old_value.clone(),
                to: change.new_value.clone(),
                ..Default::default()
            }),
        });
    }

    rules
}

// ── Prop↔Child migration rules ─────────────────────────────────────────

/// Detect props that migrated between parent and child components.
///
/// Cross-references TD structural data (removed/added props) with
/// SD composition data (new/removed children) to find:
/// - Prop→child: parent lost a prop, new child gained it
/// - Child→prop: child removed, parent gained a prop of same name
fn generate_prop_child_migration_rules(
    report: &AnalysisReport<TypeScript>,
    sd: &SdPipelineResult,
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    // Build lookup: component name → removed props
    let mut removed_props: HashMap<String, Vec<RemovedProp>> = HashMap::new();
    // Build lookup: component name → added props
    let mut added_props: HashMap<String, HashSet<String>> = HashMap::new();

    for file_changes in &report.changes {
        for change in &file_changes.breaking_api_changes {
            if let Some(component) = extract_component_name_from_symbol(&change.symbol) {
                if let Some(prop) = extract_prop_name_from_symbol(&change.symbol) {
                    if change.change == ApiChangeType::Removed {
                        let is_reactnode = change
                            .before
                            .as_ref()
                            .map(|b| is_react_node_type(b))
                            .unwrap_or(false);

                        removed_props
                            .entry(component.clone())
                            .or_default()
                            .push(RemovedProp {
                                name: prop,
                                component,
                                is_reactnode,
                                before_type: change.before.clone(),
                            });
                    }
                }
            }
        }

        // Track added props from the new surface (non-breaking additions)
        // We need to check the new API surface for child component props
    }

    // For added props, scan all file changes for new symbols too
    // (TD reports additions as well as removals in some cases)
    // Also check the new API surface directly
    if let Some(_new_surface) = report.changes.first() {
        // Build added props from the new surface
        for file_changes in &report.changes {
            for change in &file_changes.breaking_api_changes {
                if change.change == ApiChangeType::Renamed {
                    // If renamed, the new name is an "added" prop
                    if let Some(component) = extract_component_name_from_symbol(&change.symbol) {
                        if let Some(after) = &change.after {
                            added_props
                                .entry(component)
                                .or_default()
                                .insert(after.clone());
                        }
                    }
                }
            }
        }
    }

    // For each composition tree, find prop→child migrations
    for tree in &sd.composition_trees {
        let new_children: HashSet<&str> = tree
            .edges
            .iter()
            .filter(|e| e.parent == tree.root)
            .map(|e| e.child.as_str())
            .collect();

        // Get removed props from the root component
        let root_removed = removed_props.get(&tree.root);
        let Some(root_removed) = root_removed else {
            continue;
        };

        // For each new child, check the new API surface for its props
        // We need to get the child's prop names from the new surface
        let child_props = get_child_props_from_report(report, sd, &new_children);

        let pkg = pkg_for(&tree.root, component_packages);

        for removed in root_removed {
            // Phase 1: Exact prop name match
            for (child_name, child_prop_set) in &child_props {
                if child_prop_set.contains(&removed.name) {
                    // Prop→Prop migration: same name on new child
                    let rule_id = format!(
                        "sd-prop-to-child-{}-{}-to-{}",
                        sanitize(&tree.root),
                        sanitize(&removed.name),
                        sanitize(child_name),
                    );

                    rules.push(KonveyorRule {
                        rule_id,
                        labels: vec![
                            "source=semver-analyzer".into(),
                            "change-type=prop-to-child".into(),
                            format!("package={}", pkg),
                            format!("family={}", tree.root),
                            format!("target-component={}", child_name),
                        ],
                        effort: 3,
                        category: "mandatory".into(),
                        description: format!(
                            "The `{}` prop moved from <{}> to <{}>",
                            removed.name, tree.root, child_name
                        ),
                        message: {
                            let mut msg = format!(
                                "The `{}` prop has been removed from <{}>.\n\
                                 Use <{} {}={{...}} /> as a child of <{}> instead.\n\n\
                                 Before:\n  <{} {}={{value}}>\n    ...\n  </{}>\n\n\
                                 After:\n  <{}>\n    <{} {}={{value}} />\n    ...\n  </{}>",
                                removed.name,
                                tree.root,
                                child_name,
                                removed.name,
                                tree.root,
                                tree.root,
                                removed.name,
                                tree.root,
                                tree.root,
                                child_name,
                                removed.name,
                                tree.root,
                            );
                            // List props that STAY on the parent component so the
                            // LLM doesn't accidentally move them to the child.
                            if let Some(parent_props) = sd.new_component_props.get(&tree.root) {
                                let staying: Vec<&String> = parent_props
                                    .iter()
                                    .filter(|p| {
                                        p.as_str() != "children" && p.as_str() != "className"
                                    })
                                    .take(10)
                                    .collect();
                                if !staying.is_empty() {
                                    msg.push_str(&format!(
                                        "\n\nIMPORTANT: These props stay on <{}>: {}.\n\
                                         Do NOT move them to <{}>.",
                                        tree.root,
                                        staying
                                            .iter()
                                            .map(|p| format!("`{}`", p))
                                            .collect::<Vec<_>>()
                                            .join(", "),
                                        child_name,
                                    ));
                                }
                            }
                            msg
                        },
                        links: vec![],
                        when: KonveyorCondition::FrontendReferenced {
                            referenced: FrontendReferencedFields {
                                pattern: format!("^{}$", removed.name),
                                location: "JSX_PROP".into(),
                                component: Some(format!("^{}$", tree.root)),
                                parent: None,
                                parent_from: None,
                                not_parent: None,
                                child: None,
                                not_child: None,
                                requires_child: None,
                                value: None,
                                from: Some(pkg.to_string()),
                                file_pattern: None,
                            },
                        },
                        fix_strategy: Some(FixStrategyEntry {
                            strategy: "PropToChild".into(),
                            from: Some(removed.name.clone()),
                            component: Some(tree.root.clone()),
                            replacement: Some(child_name.clone()),
                            prop: Some(removed.name.clone()),
                            ..Default::default()
                        }),
                    });
                    break; // Found match, stop checking other children
                }
            }

            // Phase 2: Name containment for ReactNode props
            if removed.is_reactnode {
                let matched_in_phase1 = rules.iter().any(|r| {
                    r.labels.iter().any(|l| l == "change-type=prop-to-child")
                        && r.fix_strategy
                            .as_ref()
                            .map(|fs| fs.from.as_deref() == Some(removed.name.as_str()))
                            .unwrap_or(false)
                });

                if !matched_in_phase1 {
                    // Check if prop name appears in any child component name
                    for child_name in &new_children {
                        if child_name
                            .to_lowercase()
                            .contains(&removed.name.to_lowercase())
                        {
                            let rule_id = format!(
                                "sd-prop-to-children-{}-{}-to-{}",
                                sanitize(&tree.root),
                                sanitize(&removed.name),
                                sanitize(child_name),
                            );

                            rules.push(KonveyorRule {
                                rule_id,
                                labels: vec![
                                    "source=semver-analyzer".into(),
                                    "change-type=prop-to-child".into(),
                                    format!("package={}", pkg),
                                    format!("family={}", tree.root),
                                    format!("target-component={}", child_name),
                                ],
                                effort: 3,
                                category: "mandatory".into(),
                                description: format!(
                                    "The `{}` prop (ReactNode) moved from <{}> to <{}> children",
                                    removed.name, tree.root, child_name
                                ),
                                message: format!(
                                    "The `{}` prop has been removed from <{}>.\n\
                                     Pass this content as children of <{}> instead.\n\n\
                                     Before:\n  <{} {}={{content}}>\n    ...\n  </{}>\n\n\
                                     After:\n  <{}>\n    <{}>{{content}}</{}>\n    ...\n  </{}>",
                                    removed.name,
                                    tree.root,
                                    child_name,
                                    tree.root,
                                    removed.name,
                                    tree.root,
                                    tree.root,
                                    child_name,
                                    child_name,
                                    tree.root,
                                ),
                                links: vec![],
                                when: KonveyorCondition::FrontendReferenced {
                                    referenced: FrontendReferencedFields {
                                        pattern: format!("^{}$", removed.name),
                                        location: "JSX_PROP".into(),
                                        component: Some(format!("^{}$", tree.root)),
                                        parent: None,
                                        not_parent: None,
                                        child: None,
                                        not_child: None,
                                        requires_child: None,
                                        parent_from: None,
                                        value: None,
                                        from: Some(pkg.to_string()),
                                        file_pattern: None,
                                    },
                                },
                                fix_strategy: Some(FixStrategyEntry {
                                    strategy: "PropToChildren".into(),
                                    from: Some(removed.name.clone()),
                                    component: Some(tree.root.clone()),
                                    replacement: Some(child_name.to_string()),
                                    ..Default::default()
                                }),
                            });
                            break;
                        }
                    }
                }
            }
        }
    }

    // ── Child→prop migration (reverse direction) ─────────────────
    //
    // Detect when a child component was removed from a family and the
    // parent gained a new prop that serves the same purpose.
    //
    // Algorithm:
    // 1. Find family members in old profiles but not in new profiles
    //    (removed children)
    // 2. Find props on the parent that exist in the new version but
    //    not the old version (added props)
    // 3. Match: removed child name ↔ added prop name

    for tree in &sd.composition_trees {
        let root = &tree.root;
        let pkg = pkg_for(root, component_packages);

        // Get old and new props for the root component
        let old_root_props = sd
            .old_component_props
            .get(root)
            .cloned()
            .unwrap_or_default();
        let new_root_props = sd
            .new_component_props
            .get(root)
            .cloned()
            .unwrap_or_default();

        // Added props = in new but not in old
        let added_props: BTreeSet<String> = new_root_props
            .difference(&old_root_props)
            .cloned()
            .collect();

        if added_props.is_empty() {
            continue;
        }

        // Get the prop types from the new version
        let new_prop_types = sd
            .new_component_prop_types
            .get(root)
            .cloned()
            .unwrap_or_default();

        // Find removed family members (in old component props but not in new tree)
        let old_members: HashSet<&str> = sd
            .old_component_props
            .keys()
            .filter(|name| {
                // Only consider members of this family (name starts with root)
                name.starts_with(root.as_str()) && *name != root
            })
            .map(|s| s.as_str())
            .collect();
        let new_members: HashSet<&str> = tree.family_members.iter().map(|s| s.as_str()).collect();

        let removed_children: Vec<&str> = old_members.difference(&new_members).copied().collect();

        for removed_child in &removed_children {
            let child_lower = removed_child.to_lowercase();
            // Strip the root prefix to get the child suffix
            // e.g., "ModalIcon" with root "Modal" → suffix "icon"
            let child_suffix = child_lower
                .strip_prefix(&root.to_lowercase())
                .unwrap_or(&child_lower)
                .to_lowercase();

            if child_suffix.is_empty() {
                continue;
            }

            // Check if any added prop matches the child suffix
            for added_prop in &added_props {
                if added_prop.to_lowercase() == child_suffix {
                    // Check if the prop type is ReactNode-ish
                    let is_reactnode = new_prop_types
                        .get(added_prop)
                        .map(|t| is_react_node_type(t))
                        .unwrap_or(false);

                    let rule_id = format!(
                        "sd-child-to-prop-{}-{}-to-{}",
                        sanitize(root),
                        sanitize(removed_child),
                        sanitize(added_prop),
                    );

                    let message = if is_reactnode {
                        format!(
                            "<{}> has been removed. Pass its content via the `{}` prop on <{}> instead.\n\n\
                             Before:\n  <{}>\n    <{}>{{}}</{}>\n  </{}>\n\n\
                             After:\n  <{} {}={{content}} />",
                            removed_child, added_prop, root,
                            root, removed_child, removed_child, root,
                            root, added_prop,
                        )
                    } else {
                        format!(
                            "<{}> has been removed. Use the `{}` prop on <{}> instead.\n\n\
                             Before:\n  <{}>\n    <{} />\n  </{}>\n\n\
                             After:\n  <{} {}={{...}} />",
                            removed_child,
                            added_prop,
                            root,
                            root,
                            removed_child,
                            root,
                            root,
                            added_prop,
                        )
                    };

                    rules.push(KonveyorRule {
                        rule_id,
                        labels: vec![
                            "source=semver-analyzer".into(),
                            "change-type=child-to-prop".into(),
                            format!("package={}", pkg),
                            format!("family={}", root),
                        ],
                        effort: 3,
                        category: "mandatory".into(),
                        description: format!(
                            "<{}> removed — use `{}` prop on <{}> instead",
                            removed_child, added_prop, root
                        ),
                        message,
                        links: vec![],
                        when: KonveyorCondition::FrontendReferenced {
                            referenced: FrontendReferencedFields {
                                pattern: format!("^{}$", removed_child),
                                location: "JSX_COMPONENT".into(),
                                component: None,
                                parent: Some(format!("^{}$", root)),
                                parent_from: Some(pkg.clone()),
                                not_parent: None,
                                child: None,
                                not_child: None,
                                requires_child: None,
                                value: None,
                                from: Some(pkg.clone()),
                                file_pattern: None,
                            },
                        },
                        fix_strategy: Some(FixStrategyEntry {
                            strategy: "ChildToProp".into(),
                            from: Some(removed_child.to_string()),
                            to: Some(added_prop.clone()),
                            component: Some(root.clone()),
                            prop: Some(added_prop.clone()),
                            ..Default::default()
                        }),
                    });
                    break;
                }
            }
        }
    }

    rules
}

// ── Cross-family child→prop migration rules ─────────────────────────────

/// Detect non-family components that should be replaced by a new prop on the parent.
///
/// Tier 1 heuristic using three converging signals:
///
/// 1. **BEM evidence** from the old composition tree: a removed family member's
///    edge carries `bem_evidence` naming a prop (e.g., `"EmptyStateHeader is BEM
///    element 'titleText' of emptyState block"`).
///
/// 2. **Migration target**: the removed member's Props interface has a
///    `matching_members` entry mapping that prop to the root's new prop
///    (e.g., `EmptyStateHeaderProps.titleText → EmptyStateProps.titleText`).
///
/// 3. **Component name match**: a standalone PF component's name (case-insensitive)
///    is a prefix of the added prop name (e.g., `Title` → `titleText`), AND the
///    component is NOT a family member.
///
/// When all three signals align, we generate a rule that detects the standalone
/// component used as a child of the root and recommends using the prop instead.
///
/// Example: `<Title>` inside `<EmptyState>` → use `titleText` prop.
fn generate_cross_family_child_to_prop_rules(
    report: &AnalysisReport<TypeScript>,
    sd: &SdPipelineResult,
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    // Build a set of all known PF component names (old + new)
    let all_component_names: HashSet<&str> = sd
        .component_packages
        .keys()
        .chain(sd.old_component_packages.keys())
        .map(|s| s.as_str())
        .collect();

    // Build migration target lookup: "EmptyStateHeaderProps" → MigrationTarget
    let mut migration_targets: HashMap<String, &semver_analyzer_core::MigrationTarget> =
        HashMap::new();
    for file_changes in &report.changes {
        for change in &file_changes.breaking_api_changes {
            if let Some(ref mt) = change.migration_target {
                migration_targets.insert(mt.removed_symbol.clone(), mt);
            }
        }
    }

    // For each new composition tree, look at the OLD tree for removed members
    // with BEM evidence that names a prop.
    for new_tree in &sd.composition_trees {
        let root = &new_tree.root;
        let pkg = pkg_for(root, component_packages);

        // Find the old tree for this family
        let old_tree = match sd.old_composition_trees.iter().find(|t| t.root == *root) {
            Some(t) => t,
            None => continue,
        };

        // Compute added props on the root
        let old_root_props: BTreeSet<&str> = sd
            .old_component_props
            .get(root)
            .map(|s| s.iter().map(|p| p.as_str()).collect())
            .unwrap_or_default();
        let new_root_props: BTreeSet<&str> = sd
            .new_component_props
            .get(root)
            .map(|s| s.iter().map(|p| p.as_str()).collect())
            .unwrap_or_default();
        let added_props: BTreeSet<&str> = new_root_props
            .difference(&old_root_props)
            .copied()
            .collect();

        if added_props.is_empty() {
            continue;
        }

        // New tree family members (for dedup — skip components already in the family)
        let new_family: HashSet<&str> =
            new_tree.family_members.iter().map(|s| s.as_str()).collect();

        // Find removed family members with BEM evidence
        let new_members: HashSet<&str> =
            new_tree.family_members.iter().map(|s| s.as_str()).collect();

        for edge in &old_tree.edges {
            // Only consider edges to members that were removed
            if new_members.contains(edge.child.as_str()) {
                continue;
            }

            // Signal 1: BEM evidence must name a prop
            let bem_prop = match &edge.bem_evidence {
                Some(evidence) => {
                    // Parse "EmptyStateHeader is BEM element 'titleText' of emptyState block"
                    // Extract the quoted prop name
                    extract_bem_prop_name(evidence)
                }
                None => continue,
            };

            let bem_prop = match bem_prop {
                Some(p) => p,
                None => continue,
            };

            // The BEM prop must be an added prop on the root
            if !added_props.contains(bem_prop.as_str()) {
                continue;
            }

            // Signal 2: migration_target confirms the prop mapping
            let removed_props_iface = format!("{}Props", edge.child);
            let has_migration_match = migration_targets
                .get(&removed_props_iface)
                .map(|mt| {
                    mt.matching_members
                        .iter()
                        .any(|mm| mm.old_name == bem_prop && mm.new_name == bem_prop)
                })
                .unwrap_or(false);

            if !has_migration_match {
                continue;
            }

            // Signal 3: find a standalone PF component whose name is a prefix
            // of the prop name (case-insensitive) and is NOT a family member
            let prop_lower = bem_prop.to_lowercase();

            for comp_name in &all_component_names {
                let comp_lower = comp_name.to_lowercase();

                // Component name must be a prefix of the prop name
                if !prop_lower.starts_with(&comp_lower) {
                    continue;
                }

                // Must not be a family member of this root
                if new_family.contains(comp_name) {
                    continue;
                }

                // Must not be the removed family member itself (that's
                // already handled by the family-based child→prop detection)
                if *comp_name == edge.child.as_str() {
                    continue;
                }

                let comp_pkg = pkg_for(comp_name, component_packages);

                let rule_id = format!(
                    "sd-cross-family-child-to-prop-{}-{}-to-{}",
                    sanitize(root),
                    sanitize(comp_name),
                    sanitize(&bem_prop),
                );

                let message = format!(
                    "<{comp}> should no longer be used as a child of <{root}>.\n\
                     Use the `{prop}` prop on <{root}> instead.\n\n\
                     Before:\n\
                     \x20 <{root}>\n\
                     \x20   <{comp} ...>...</{comp}>\n\
                     \x20 </{root}>\n\n\
                     After:\n\
                     \x20 <{root} {prop}={{...}}>\n\
                     \x20   ...\n\
                     \x20 </{root}>\n\n\
                     The <{removed}> component that previously wrapped this content \
                     has been removed. Its `{prop}` prop has moved to <{root}>.",
                    comp = comp_name,
                    root = root,
                    prop = bem_prop,
                    removed = edge.child,
                );

                rules.push(KonveyorRule {
                    rule_id,
                    labels: vec![
                        "source=semver-analyzer".into(),
                        "change-type=child-to-prop".into(),
                        format!("package={}", pkg),
                        format!("family={}", root),
                    ],
                    effort: 3,
                    category: "mandatory".into(),
                    description: format!(
                        "<{}> inside <{}> — use `{}` prop instead",
                        comp_name, root, bem_prop
                    ),
                    message,
                    links: vec![],
                    when: KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: format!("^{}$", comp_name),
                            location: "JSX_COMPONENT".into(),
                            component: None,
                            parent: Some(format!("^{}$", root)),
                            parent_from: Some(pkg.clone()),
                            not_parent: None,
                            child: None,
                            not_child: None,
                            requires_child: None,
                            value: None,
                            from: Some(comp_pkg),
                            file_pattern: None,
                        },
                    },
                    fix_strategy: Some(FixStrategyEntry {
                        strategy: "ChildToProp".into(),
                        from: Some(comp_name.to_string()),
                        to: Some(bem_prop.clone()),
                        component: Some(root.clone()),
                        prop: Some(bem_prop.clone()),
                        ..Default::default()
                    }),
                });
            }
        }
    }

    if !rules.is_empty() {
        tracing::info!(
            count = rules.len(),
            "Generated cross-family child→prop migration rules"
        );
    }

    rules
}

/// Extract the prop name from a BEM evidence string.
///
/// Parses strings like:
///   "EmptyStateHeader is BEM element 'titleText' of emptyState block"
/// Returns `Some("titleText")`.
fn extract_bem_prop_name(evidence: &str) -> Option<String> {
    let start = evidence.find('\'')?;
    let rest = &evidence[start + 1..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

// ── Deprecated↔main migration rules ─────────────────────────────────────

/// Generate rules for components that moved between deprecated and main.
///
/// Detects two cases:
/// 1. Component was in /deprecated in old version, removed in new → must migrate to main
/// 2. Component was in main in old version, moved to /deprecated in new → should migrate to new API
///
/// For both cases, includes the new component's composition tree in the
/// migration guidance.
fn generate_deprecated_migration_rules(
    sd: &SdPipelineResult,
    _component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    // Compare old vs new package assignments to find moves
    for (component, old_pkg) in &sd.old_component_packages {
        let new_pkg = sd.component_packages.get(component);

        let old_is_deprecated = old_pkg.contains("/deprecated");
        let new_pkg_val = new_pkg.cloned().unwrap_or_default();
        let new_is_deprecated = new_pkg_val.contains("/deprecated");
        let new_is_main = !new_pkg_val.is_empty()
            && !new_pkg_val.contains("/deprecated")
            && !new_pkg_val.contains("/next");

        // Case 1: Was in /deprecated, now either:
        //   a) removed entirely, or
        //   b) the deprecated version is gone but a main version exists
        // Both mean: consumer using /deprecated must migrate to main.
        if old_is_deprecated && !new_is_deprecated {
            // Check if a same-named component exists in main
            let main_pkg_name = if new_is_main {
                Some(new_pkg_val.clone())
            } else {
                sd.component_packages
                    .iter()
                    .find(|(name, pkg)| {
                        *name == component && !pkg.contains("/deprecated") && !pkg.contains("/next")
                    })
                    .map(|(_, pkg)| pkg.clone())
            };

            if let Some(main_pkg) = main_pkg_name {
                let composition = find_composition_tree_for(component, &sd.composition_trees);
                let rule_id = format!(
                    "sd-deprecated-removed-{}-migrate-to-main",
                    sanitize(component),
                );

                let mut message = format!(
                    "The deprecated `<{}>` from `{}` has been removed.\n\
                     Migrate to the new `<{}>` from `{}`.\n",
                    component, old_pkg, component, main_pkg,
                );
                if let Some(tree) = composition {
                    message.push_str(&format!(
                        "\nNew composition structure:\n{}",
                        format_tree_as_jsx(tree),
                    ));
                }

                rules.push(KonveyorRule {
                    rule_id,
                    labels: vec![
                        "source=semver-analyzer".into(),
                        "change-type=deprecated-migration".into(),
                        format!("package={}", old_pkg),
                        format!("target-package={}", main_pkg),
                    ],
                    effort: 5,
                    category: "mandatory".into(),
                    description: format!(
                        "Deprecated <{}> removed — migrate to new API in {}",
                        component, main_pkg
                    ),
                    message,
                    links: vec![],
                    when: KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: format!("^{}$", component),
                            location: "IMPORT".into(),
                            component: None,
                            parent: None,
                            parent_from: None,
                            not_parent: None,
                            child: None,
                            not_child: None,
                            requires_child: None,
                            value: None,
                            from: Some(old_pkg.clone()),
                            file_pattern: None,
                        },
                    },
                    fix_strategy: Some(FixStrategyEntry {
                        strategy: "DeprecatedMigration".into(),
                        from: Some(old_pkg.clone()),
                        to: Some(main_pkg.clone()),
                        component: Some(component.clone()),
                        ..Default::default()
                    }),
                });
            }
            continue;
        }

        // Case 2: Was in main, now in /deprecated → new API in main.
        // Fire on consumers importing from /deprecated (they're using the
        // old API explicitly). Consumers importing from main already have
        // the new API — they might need prop→child rules but not this one.
        if !old_is_deprecated && new_is_deprecated {
            let base_pkg = old_pkg.clone();
            let deprecated_pkg = format!("{}/deprecated", base_pkg);

            let composition = find_composition_tree_for(component, &sd.composition_trees);
            let rule_id = format!("sd-deprecated-moved-{}-to-deprecated", sanitize(component));

            let mut message = format!(
                "`<{}>` from `{}` uses the old API.\n\
                 Migrate to the new `<{}>` from `{}`.\n",
                component, deprecated_pkg, component, base_pkg,
            );
            if let Some(tree) = composition {
                message.push_str(&format!(
                    "\nNew composition structure:\n{}",
                    format_tree_as_jsx(tree),
                ));
            }

            rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".into(),
                    "change-type=deprecated-migration".into(),
                    format!("package={}", deprecated_pkg),
                    format!("target-package={}", base_pkg),
                ],
                effort: 5,
                category: "mandatory".into(),
                description: format!(
                    "<{}> from /deprecated — migrate to new API in {}",
                    component, base_pkg
                ),
                message,
                links: vec![],
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: format!("^{}$", component),
                        location: "IMPORT".into(),
                        component: None,
                        parent: None,
                        parent_from: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
                        value: None,
                        from: Some(deprecated_pkg.clone()),
                        file_pattern: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry {
                    strategy: "DeprecatedMigration".into(),
                    from: Some(deprecated_pkg),
                    to: Some(base_pkg),
                    component: Some(component.clone()),
                    ..Default::default()
                }),
            });
        }
    }

    rules
}

/// Find the composition tree for a component (as root).
fn find_composition_tree_for<'a>(
    component: &str,
    trees: &'a [CompositionTree],
) -> Option<&'a CompositionTree> {
    trees.iter().find(|t| t.root == component)
}

/// Format a composition tree as a JSX code example.
fn format_tree_as_jsx(tree: &CompositionTree) -> String {
    let mut lines = Vec::new();

    // Build children lookup: parent → [child]
    let mut parent_children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for edge in &tree.edges {
        if edge.relationship != ChildRelationship::Internal {
            parent_children
                .entry(edge.parent.as_str())
                .or_default()
                .push(edge.child.as_str());
        }
    }

    fn render(
        component: &str,
        parent_children: &BTreeMap<&str, Vec<&str>>,
        indent: usize,
        lines: &mut Vec<String>,
        visited: &mut HashSet<String>,
    ) {
        let pad = "  ".repeat(indent);
        if !visited.insert(component.to_string()) || indent > 5 {
            lines.push(format!("{}<{} />", pad, component));
            return;
        }
        if let Some(children) = parent_children.get(component) {
            lines.push(format!("{}<{}>", pad, component));
            for child in children {
                render(child, parent_children, indent + 1, lines, visited);
            }
            lines.push(format!("{}</{}>", pad, component));
        } else {
            lines.push(format!("{}<{} />", pad, component));
        }
        visited.remove(component);
    }

    let mut visited = HashSet::new();
    render(&tree.root, &parent_children, 1, &mut lines, &mut visited);
    lines.join("\n")
}

// ── Family-level strategy generation ────────────────────────────────────
//
// Generates one `FixStrategyEntry` per family that has structural composition
// changes. These entries describe the complete target v6 component structure
// so the frontend-analyzer-provider can build a single coherent LLM prompt
// per (file, family) instead of N overlapping rule-level prompts.

/// Generate family-level fix strategy entries for families with structural changes.
///
/// Returns a map of `"family:<Name>"` → `FixStrategyEntry` with the complete
/// target structure, prop assignments, and import changes.
pub fn generate_family_strategies(
    report: &AnalysisReport<TypeScript>,
    sd: &SdPipelineResult,
) -> HashMap<String, FixStrategyEntry> {
    let mut family_strats = HashMap::new();

    // Build lookup: component name → removed props (from TD pipeline)
    let mut removed_props_by_component: HashMap<String, Vec<String>> = HashMap::new();
    for file_changes in &report.changes {
        for change in &file_changes.breaking_api_changes {
            if change.change == ApiChangeType::Removed {
                if let Some(component) = extract_component_name_from_symbol(&change.symbol) {
                    if let Some(prop) = extract_prop_name_from_symbol(&change.symbol) {
                        removed_props_by_component
                            .entry(component)
                            .or_default()
                            .push(prop);
                    }
                }
            }
        }
    }

    for tree in &sd.composition_trees {
        // Skip single-component families and deprecated families
        if tree.family_members.len() <= 1 || tree.root.starts_with("deprecated/") {
            continue;
        }

        // Generate for families that have composition changes OR are the
        // replacement target for a deprecated component (e.g., Label replaces
        // Chip, Card replaces Tile). Replacement targets need a family entry
        // so the deprecated_migration context (removed/matching/new props)
        // reaches the LLM prompt via fix-strategies.json.
        let has_changes = sd.composition_changes.iter().any(|c| c.family == tree.root);
        let is_replacement_target = sd
            .deprecated_replacements
            .iter()
            .any(|dr| dr.new_component == tree.root);
        if !has_changes && !is_replacement_target {
            continue;
        }

        // 1. Render target structure with props
        let target_jsx = render_family_target_with_props(tree, &sd.new_component_props);

        // 2. Retained props (props on root in new version)
        let retained_props: Vec<String> = sd
            .new_component_props
            .get(&tree.root)
            .map(|props| {
                props
                    .iter()
                    .filter(|p| p.as_str() != "children" && p.as_str() != "className")
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        // 3. Prop-to-child map: removed root props that appear on new children
        let mut prop_to_child: BTreeMap<String, String> = BTreeMap::new();
        let new_children: HashSet<&str> = tree
            .edges
            .iter()
            .filter(|e| e.parent == tree.root && e.relationship != ChildRelationship::Internal)
            .map(|e| e.child.as_str())
            .collect();

        if let Some(removed) = removed_props_by_component.get(&tree.root) {
            for prop_name in removed {
                for &child_name in &new_children {
                    if let Some(child_props) = sd.new_component_props.get(child_name) {
                        if child_props.contains(prop_name) {
                            prop_to_child.insert(prop_name.clone(), child_name.to_string());
                            break;
                        }
                    }
                }
            }
        }

        // 4. Child-to-parent map: props named after removed children
        let mut child_props_to_parent: BTreeMap<String, String> = BTreeMap::new();
        let old_members: HashSet<&str> = sd
            .old_composition_trees
            .iter()
            .find(|t| t.root == tree.root)
            .map(|t| t.family_members.iter().map(|m| m.as_str()).collect())
            .unwrap_or_default();
        let new_members: HashSet<&str> = tree.family_members.iter().map(|m| m.as_str()).collect();
        let removed_members: Vec<&str> = old_members.difference(&new_members).copied().collect();

        for removed_member in &removed_members {
            // Check if root gained a prop matching the child suffix
            if let Some(root_props) = sd.new_component_props.get(&tree.root) {
                let suffix = removed_member
                    .strip_prefix(&tree.root)
                    .unwrap_or(removed_member)
                    .to_lowercase();
                for prop in root_props {
                    if !suffix.is_empty() && prop.to_lowercase() == suffix {
                        child_props_to_parent.insert(
                            format!("{}.props", removed_member),
                            format!("{}.{}", tree.root, prop),
                        );
                    }
                }
            }
        }

        // 5. Removed children (in old tree but not new)
        let removed_children: Vec<String> = removed_members.iter().map(|m| m.to_string()).collect();

        // 6. New imports: ALL consumer-facing family members that need importing
        // (at any depth, not just direct children of root). Consumers must
        // import MastheadLogo even though it's a grandchild of the root
        // (MastheadBrand -> MastheadLogo).
        //
        // Excludes:
        //  - Context providers (e.g., AlertContext, FormContext) — consumers
        //    get context implicitly from the parent, not via direct import.
        //  - Members with only Internal edges — these are rendered by the
        //    parent component, not placed by the consumer.
        let consumer_facing_members: HashSet<&str> = {
            let mut members = HashSet::new();
            for edge in &tree.edges {
                if edge.relationship != ChildRelationship::Internal {
                    members.insert(edge.parent.as_str());
                    members.insert(edge.child.as_str());
                }
            }
            members
        };
        let new_imports: Vec<String> = tree
            .family_members
            .iter()
            .filter(|member| {
                let name = member.as_str();
                name != tree.root
                    && !old_members.contains(name)
                    && !name.ends_with("Context")
                    && consumer_facing_members.contains(name)
            })
            .cloned()
            .collect();

        // 7. Removed imports: old children no longer in the family
        let removed_imports: Vec<String> = removed_children.clone();

        // 8. Import source package
        let import_source = sd.component_packages.get(&tree.root).cloned();

        // 9. Prop value changes from composition changes
        let prop_value_changes: BTreeMap<String, Vec<semver_analyzer_konveyor_core::MappingEntry>> =
            BTreeMap::new();
        for change in &sd.composition_changes {
            if change.family != tree.root {
                continue;
            }
            if let CompositionChangeType::PropToChild { props, child, .. } = &change.change_type {
                for prop in props {
                    prop_to_child.insert(prop.clone(), child.clone());
                }
            }
            if let CompositionChangeType::ChildToProp { props, child, .. } = &change.change_type {
                for prop in props {
                    child_props_to_parent.insert(
                        format!("{}.content", child),
                        format!("{}.{}", tree.root, prop),
                    );
                }
            }
        }

        // 10. Prop type changes: compare old and new prop types for each
        //     component in the family to detect callback signature changes,
        //     type narrowing/broadening, and other per-prop type differences.
        let prop_type_changes = {
            let mut changes: BTreeMap<String, Vec<semver_analyzer_konveyor_core::MappingEntry>> =
                BTreeMap::new();
            for member in std::iter::once(&tree.root).chain(tree.family_members.iter()) {
                let old_types = sd.old_component_prop_types.get(member.as_str());
                let new_types = sd.new_component_prop_types.get(member.as_str());
                match (old_types, new_types) {
                    (Some(old_map), Some(new_map)) => {
                        // Props that exist in both versions with different types
                        for (prop_name, old_type) in old_map {
                            if let Some(new_type) = new_map.get(prop_name) {
                                if old_type != new_type {
                                    let key = if member == &tree.root {
                                        prop_name.clone()
                                    } else {
                                        format!("{}.{}", member, prop_name)
                                    };
                                    changes.entry(key).or_default().push(
                                        semver_analyzer_konveyor_core::MappingEntry {
                                            from: Some(old_type.clone()),
                                            to: Some(new_type.clone()),
                                            component: Some(member.clone()),
                                            prop: Some(prop_name.clone()),
                                        },
                                    );
                                }
                            }
                        }
                    }
                    (None, Some(new_map)) => {
                        // New-only: component had no explicit props in old version
                        // (all inherited) but now has explicit declarations.
                        // Include callback/function types so the LLM knows the
                        // current signatures.
                        for (prop_name, new_type) in new_map {
                            if new_type.contains("=>") {
                                let key = if member == &tree.root {
                                    prop_name.clone()
                                } else {
                                    format!("{}.{}", member, prop_name)
                                };
                                changes.entry(key).or_default().push(
                                    semver_analyzer_konveyor_core::MappingEntry {
                                        from: None,
                                        to: Some(new_type.clone()),
                                        component: Some(member.clone()),
                                        prop: Some(prop_name.clone()),
                                    },
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
            changes
        };

        // 11. Deprecated migration context: cross-reference MigrationTarget
        //     with prop type maps to build a complete old→new mapping.
        //
        //     Look for a MigrationTarget whose replacement matches this family's
        //     root Props interface (e.g., "SelectProps" → "SelectProps"). This
        //     means a deprecated component was removed and detected as having a
        //     migration path to this family's root.
        let deprecated_migration = build_deprecated_migration_context(&tree.root, report, sd);

        // 11. Unmapped removed props: props removed from the root that don't
        //     have an exact prop-name match on any child (not in prop_to_child).
        //     Uses the shared classifier to determine where each prop should go.
        let unmapped_removed_props = {
            use crate::konveyor::classify_removed_props;
            let mut unmapped = BTreeMap::new();

            // Find the TypeSummary for this family root to get
            // removed_members and child_components.
            let type_summary = report
                .packages
                .iter()
                .flat_map(|pkg| &pkg.type_summaries)
                .find(|comp| comp.name == tree.root);

            if let Some(comp) = type_summary {
                let classifications = classify_removed_props(
                    &comp.removed_members,
                    &comp.language_data.child_components,
                );
                for c in &classifications {
                    // Skip props already in prop_to_child (exact match)
                    if prop_to_child.contains_key(&c.name) {
                        continue;
                    }
                    // Skip retained props
                    if retained_props.contains(&c.name) {
                        continue;
                    }
                    let type_hint = c.old_type.as_deref().unwrap_or("unknown type");
                    match (c.target_child.as_deref(), c.mechanism.as_str()) {
                        (Some(child), "prop") => {
                            unmapped.insert(
                                c.name.clone(),
                                format!("{} (as prop, {})", child, type_hint),
                            );
                        }
                        (Some(child), "children") => {
                            unmapped.insert(
                                c.name.clone(),
                                format!("{} (as children, {})", child, type_hint),
                            );
                        }
                        (_, "removed") => {
                            unmapped.insert(c.name.clone(), format!("removed ({})", type_hint));
                        }
                        _ => {
                            unmapped.insert(
                                c.name.clone(),
                                format!("map to appropriate child component ({})", type_hint),
                            );
                        }
                    }
                }
            }
            unmapped
        };

        // Only emit if we have meaningful data
        if target_jsx.is_empty()
            && retained_props.is_empty()
            && prop_to_child.is_empty()
            && child_props_to_parent.is_empty()
            && removed_children.is_empty()
            && deprecated_migration.is_none()
            && prop_type_changes.is_empty()
        {
            continue;
        }

        let entry = FixStrategyEntry {
            strategy: "FamilyMigration".into(),
            component: Some(tree.root.clone()),
            target_structure: Some(target_jsx),
            retained_props,
            prop_to_child,
            unmapped_removed_props,
            child_props_to_parent,
            removed_children,
            prop_value_changes,
            prop_type_changes,
            new_imports,
            removed_imports,
            import_source,
            deprecated_migration,
            ..Default::default()
        };

        family_strats.insert(format!("family:{}", tree.root), entry);
    }

    family_strats
}

/// Build a `DeprecatedMigrationContext` for a family root by finding
/// `MigrationTarget` entries where the replacement matches this family's
/// Props interface, then cross-referencing with prop type maps.
fn build_deprecated_migration_context(
    family_root: &str,
    report: &AnalysisReport<TypeScript>,
    sd: &SdPipelineResult,
) -> Option<semver_analyzer_konveyor_core::DeprecatedMigrationContext> {
    let root_props_name = format!("{}Props", family_root);

    // Find the MigrationTarget where replacement_symbol matches our root Props.
    // This means a deprecated interface was detected as migrating TO this family.
    let mut best_mt: Option<&MigrationTarget> = None;
    let mut best_change_file: Option<String> = None;

    for file_changes in &report.changes {
        let file_str = file_changes.file.to_string_lossy();
        for change in &file_changes.breaking_api_changes {
            if let Some(ref mt) = change.migration_target {
                if mt.replacement_symbol == root_props_name && mt.removed_symbol != root_props_name
                {
                    // Prefer higher overlap ratio
                    let dominated = best_mt
                        .map(|prev| mt.overlap_ratio > prev.overlap_ratio)
                        .unwrap_or(true);
                    if dominated {
                        best_mt = Some(mt);
                        best_change_file = Some(file_str.to_string());
                    }
                }
                // Also check deprecated→promoted same-name migration
                // (e.g., deprecated SelectProps → promoted SelectProps)
                if mt.replacement_symbol == root_props_name && mt.removed_symbol == root_props_name
                {
                    // Same-name migration (deprecated → promoted version)
                    let is_deprecated = change.qualified_name.contains("deprecated")
                        || file_str.contains("deprecated");
                    if is_deprecated {
                        let dominated = best_mt
                            .map(|prev| mt.overlap_ratio > prev.overlap_ratio)
                            .unwrap_or(true);
                        if dominated {
                            best_mt = Some(mt);
                            best_change_file = Some(file_str.to_string());
                        }
                    }
                }
            }
        }
    }

    let mt = match best_mt {
        Some(mt) => mt,
        None => {
            // No MigrationTarget found. Check deprecated_replacements for
            // cross-name replacements (e.g., Tile → Card detected via commit
            // co-change or rendering swap). Build the migration context
            // manually from prop type maps.
            let dr = sd
                .deprecated_replacements
                .iter()
                .find(|dr| dr.new_component == family_root);
            if let Some(dr) = dr {
                return build_deprecated_migration_from_replacement(
                    family_root,
                    &dr.old_component,
                    sd,
                );
            }
            return None;
        }
    };

    // Determine old/new package from component_packages or the file path.
    let old_package = mt
        .removed_package
        .clone()
        .or_else(|| {
            best_change_file.as_deref().and_then(|f| {
                if f.contains("deprecated") {
                    sd.old_component_packages
                        .get(family_root)
                        .cloned()
                        .map(|p| {
                            if p.contains("/deprecated") {
                                p
                            } else {
                                format!("{}/deprecated", p)
                            }
                        })
                } else {
                    sd.old_component_packages.get(family_root).cloned()
                }
            })
        })
        .unwrap_or_else(|| "@patternfly/react-core/deprecated".to_string());

    let new_package = mt
        .replacement_package
        .clone()
        .or_else(|| sd.component_packages.get(family_root).cloned())
        .unwrap_or_else(|| "@patternfly/react-core".to_string());

    // Cross-reference matching members with prop type maps.
    let old_types = sd.old_component_prop_types.get(family_root);
    let new_types = sd.new_component_prop_types.get(family_root);

    let matching_props: Vec<semver_analyzer_konveyor_core::PropMigrationEntry> = mt
        .matching_members
        .iter()
        .map(|m| {
            let ot = old_types.and_then(|t| t.get(&m.old_name)).cloned();
            let nt = new_types.and_then(|t| t.get(&m.new_name)).cloned();
            let type_changed = match (&ot, &nt) {
                (Some(a), Some(b)) => a != b,
                _ => false,
            };
            semver_analyzer_konveyor_core::PropMigrationEntry {
                old_name: m.old_name.clone(),
                new_name: m.new_name.clone(),
                old_type: ot,
                new_type: nt,
                type_changed,
            }
        })
        .collect();

    // Compute new-only props: props on the v6 component that have NO match
    // in the deprecated component's matching or removed lists.
    let matching_new_names: HashSet<&str> = mt
        .matching_members
        .iter()
        .map(|m| m.new_name.as_str())
        .collect();
    let new_props: BTreeMap<String, String> = new_types
        .map(|types| {
            types
                .iter()
                .filter(|(name, _)| {
                    !matching_new_names.contains(name.as_str())
                        && name.as_str() != "children"
                        && name.as_str() != "className"
                })
                .map(|(name, typ)| (name.clone(), typ.clone()))
                .collect()
        })
        .unwrap_or_default();

    let removed_props = mt.removed_only_members.clone();

    // Only return if we have meaningful data
    if matching_props.is_empty() && new_props.is_empty() && removed_props.is_empty() {
        return None;
    }

    Some(semver_analyzer_konveyor_core::DeprecatedMigrationContext {
        old_package,
        new_package,
        matching_props,
        new_props,
        removed_props,
    })
}

/// Build a `DeprecatedMigrationContext` for cross-name replacements
/// (e.g., Tile → Card) by comparing old and new prop type maps directly,
/// without requiring a `MigrationTarget` from the diff engine.
///
/// This handles cases where the deprecated component has a differently-named
/// replacement detected via rendering swap or commit co-change analysis.
/// The prop overlap is computed by exact name matching between the old
/// component's props and the new (replacement) component's props.
fn build_deprecated_migration_from_replacement(
    family_root: &str,
    old_component: &str,
    sd: &SdPipelineResult,
) -> Option<semver_analyzer_konveyor_core::DeprecatedMigrationContext> {
    let old_types = sd.old_component_prop_types.get(old_component)?;
    let new_types = sd.new_component_prop_types.get(family_root)?;

    let old_package = sd
        .old_component_packages
        .get(old_component)
        .cloned()
        .unwrap_or_else(|| "@patternfly/react-core".to_string());
    let new_package = sd
        .component_packages
        .get(family_root)
        .cloned()
        .unwrap_or_else(|| "@patternfly/react-core".to_string());

    // Compute matching props (same name in both old and new).
    let matching_props: Vec<semver_analyzer_konveyor_core::PropMigrationEntry> = old_types
        .iter()
        .filter(|(name, _)| new_types.contains_key(name.as_str()))
        .filter(|(name, _)| name.as_str() != "children" && name.as_str() != "className")
        .map(|(name, old_type)| {
            let new_type = new_types.get(name).cloned();
            let type_changed = new_type
                .as_ref()
                .map(|nt| nt != old_type)
                .unwrap_or(false);
            semver_analyzer_konveyor_core::PropMigrationEntry {
                old_name: name.clone(),
                new_name: name.clone(),
                old_type: Some(old_type.clone()),
                new_type,
                type_changed,
            }
        })
        .collect();

    // Props only on old component (no equivalent on new).
    let removed_props: Vec<String> = old_types
        .keys()
        .filter(|name| !new_types.contains_key(name.as_str()))
        .filter(|name| name.as_str() != "children" && name.as_str() != "className")
        .cloned()
        .collect();

    // Props only on new component (not on old).
    let new_props: BTreeMap<String, String> = new_types
        .iter()
        .filter(|(name, _)| !old_types.contains_key(name.as_str()))
        .filter(|(name, _)| name.as_str() != "children" && name.as_str() != "className")
        .map(|(name, typ)| (name.clone(), typ.clone()))
        .collect();

    if matching_props.is_empty() && new_props.is_empty() && removed_props.is_empty() {
        return None;
    }

    Some(semver_analyzer_konveyor_core::DeprecatedMigrationContext {
        old_package,
        new_package,
        matching_props,
        new_props,
        removed_props,
    })
}

/// Render a family's target JSX structure with prop names on each component.
fn render_family_target_with_props(
    tree: &CompositionTree,
    new_props: &HashMap<String, BTreeSet<String>>,
) -> String {
    let mut lines = Vec::new();

    // Build children lookup: parent → [child]
    let mut parent_children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for edge in &tree.edges {
        if edge.relationship != ChildRelationship::Internal {
            parent_children
                .entry(edge.parent.as_str())
                .or_default()
                .push(edge.child.as_str());
        }
    }

    fn render(
        component: &str,
        parent_children: &BTreeMap<&str, Vec<&str>>,
        new_props: &HashMap<String, BTreeSet<String>>,
        indent: usize,
        lines: &mut Vec<String>,
        visited: &mut HashSet<String>,
    ) {
        let pad = "  ".repeat(indent);
        if !visited.insert(component.to_string()) || indent > 5 {
            lines.push(format!("{}<{} />", pad, component));
            return;
        }

        // Format props for this component (show most important ones)
        let props_str = if let Some(props) = new_props.get(component) {
            let display_props: Vec<String> = props
                .iter()
                .filter(|p| p.as_str() != "children" && p.as_str() != "className")
                .take(8) // limit to avoid overly long lines
                .map(|p| format!("{}={{...}}", p))
                .collect();
            if display_props.is_empty() {
                String::new()
            } else {
                format!(" {}", display_props.join(" "))
            }
        } else {
            String::new()
        };

        if let Some(children) = parent_children.get(component) {
            lines.push(format!("{}<{}{}>", pad, component, props_str));
            for child in children {
                render(
                    child,
                    parent_children,
                    new_props,
                    indent + 1,
                    lines,
                    visited,
                );
            }
            lines.push(format!("{}</{}>", pad, component));
        } else {
            lines.push(format!("{}<{}{} />", pad, component, props_str));
        }
        visited.remove(component);
    }

    let mut visited = HashSet::new();
    render(
        &tree.root,
        &parent_children,
        new_props,
        1,
        &mut lines,
        &mut visited,
    );
    lines.join("\n")
}

// ── Helper types ────────────────────────────────────────────────────────

struct RemovedProp {
    name: String,
    #[allow(dead_code)]
    component: String,
    is_reactnode: bool,
    #[allow(dead_code)]
    before_type: Option<String>,
}

// ── Prop value conformance rules ────────────────────────────────────────
//
// When a prop's string union type narrows (values removed), generate a rule
// that fires on the removed value. E.g., if PageSection.variant lost "dark",
// fire on `<PageSection variant="dark">`.

fn generate_prop_value_conformance_rules(
    report: &AnalysisReport<crate::language::TypeScript>,
    sd: &SdPipelineResult,
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    for fc in &report.changes {
        for api in &fc.breaking_api_changes {
            if api.change != ApiChangeType::TypeChanged {
                continue;
            }
            let symbol = &api.symbol;
            if !symbol.contains('.') {
                continue;
            }

            let component = match extract_component_name_from_symbol(symbol) {
                Some(c) => c,
                None => continue,
            };
            let prop = match extract_prop_name_from_symbol(symbol) {
                Some(p) => p,
                None => continue,
            };

            let before = match &api.before {
                Some(b) => b,
                None => continue,
            };
            let after = match &api.after {
                Some(a) => a,
                None => continue,
            };

            // Extract string literal values from union types
            let old_values: HashSet<String> = extract_union_values(before);
            let new_values: HashSet<String> = extract_union_values(after);

            if old_values.is_empty() {
                continue;
            }

            let removed: Vec<&String> = old_values.difference(&new_values).collect();
            if removed.is_empty() {
                continue;
            }

            let pkg = pkg_for(&component, component_packages);

            // Generate one rule per removed value for precise matching
            for value in &removed {
                let rule_id = format!(
                    "sd-prop-value-{}-{}-{}",
                    sanitize(&component),
                    sanitize(&prop),
                    sanitize(value),
                );

                // Find replacement suggestion if there's a close match in new values
                let replacement_hint = find_replacement_value(value, &new_values);
                let message = if let Some(ref replacement) = replacement_hint {
                    format!(
                        "The value \"{}\" is no longer valid for the `{}` prop on <{}>.\n\
                         Use \"{}\" instead.\n\n\
                         Old: <{component} {prop}=\"{value}\" />\n\
                         New: <{component} {prop}=\"{replacement}\" />",
                        value,
                        prop,
                        component,
                        replacement,
                        component = component,
                        prop = prop,
                        value = value,
                        replacement = replacement,
                    )
                } else {
                    format!(
                        "The value \"{}\" is no longer valid for the `{}` prop on <{}>.\n\
                         Valid values: {}",
                        value,
                        prop,
                        component,
                        new_values
                            .iter()
                            .map(|v| format!("\"{}\"", v))
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                };

                rules.push(KonveyorRule {
                    rule_id,
                    labels: vec![
                        "source=semver-analyzer".into(),
                        "change-type=prop-value-removed".into(),
                        format!("package={}", pkg),
                    ],
                    effort: 1,
                    category: "mandatory".into(),
                    description: format!(
                        "Value \"{}\" removed from `{}` prop on <{}>",
                        value, prop, component,
                    ),
                    message,
                    links: vec![],
                    when: KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: format!("^{}$", prop),
                            location: "JSX_PROP".into(),
                            component: Some(format!("^{}$", component)),
                            parent: None,
                            not_parent: None,
                            child: None,
                            not_child: None,
                            requires_child: None,
                            parent_from: None,
                            value: Some(format!("^{}$", regex::escape(value))),
                            from: Some(pkg.to_string()),
                            file_pattern: None,
                        },
                    },
                    fix_strategy: Some(FixStrategyEntry {
                        strategy: "PropValueChange".into(),
                        component: Some(component.clone()),
                        prop: Some(prop.clone()),
                        from: Some(value.to_string()),
                        replacement: replacement_hint,
                        ..Default::default()
                    }),
                });
            }
        }
    }

    // ── Phase 2: Renamed props with value changes ────────────────────
    //
    // When a prop is renamed (e.g., spacer → gap), the values may also
    // change (e.g., spacerNone → gapNone). Detect these by comparing
    // old prop type (from old_component_prop_types) with new prop type
    // (from new_component_prop_types). Generate per-value rules that
    // trigger on the old value in EITHER the old or new prop name.
    for fc in &report.changes {
        for api in &fc.breaking_api_changes {
            if api.change != ApiChangeType::Renamed {
                continue;
            }
            let symbol = &api.symbol;
            if !symbol.contains('.') {
                continue;
            }

            let component = match extract_component_name_from_symbol(symbol) {
                Some(c) => c,
                None => continue,
            };
            let old_prop = match extract_prop_name_from_symbol(symbol) {
                Some(p) => p,
                None => continue,
            };
            let new_prop = match &api.after {
                Some(a) => a.clone(),
                None => continue,
            };

            // Look up old and new types from SD prop type data
            let old_type = sd
                .old_component_prop_types
                .get(&component)
                .and_then(|m| m.get(&old_prop));
            let new_type = sd
                .new_component_prop_types
                .get(&component)
                .and_then(|m| m.get(&new_prop));

            let (old_type, new_type) = match (old_type, new_type) {
                (Some(o), Some(n)) => (o, n),
                _ => continue,
            };

            let old_values = extract_union_values(old_type);
            let new_values = extract_union_values(new_type);

            if old_values.is_empty() || new_values.is_empty() {
                continue;
            }

            let removed: Vec<&String> = old_values.difference(&new_values).collect();
            if removed.is_empty() {
                continue;
            }

            let pkg = pkg_for(&component, component_packages);

            for value in &removed {
                let replacement_hint = find_replacement_value(value, &new_values);

                // Generate rules for BOTH old and new prop names, since the
                // rename fix may or may not have been applied yet.
                for prop in &[&old_prop, &new_prop] {
                    let rule_id = format!(
                        "sd-prop-value-{}-{}-{}",
                        sanitize(&component),
                        sanitize(prop),
                        sanitize(value),
                    );

                    let message = if let Some(ref replacement) = replacement_hint {
                        format!(
                            "The value \"{value}\" is no longer valid for the `{prop}` prop on <{component}>.\n\
                             Use \"{replacement}\" instead.\n\n\
                             Old: <{component} {prop}=\"{value}\" />\n\
                             New: <{component} {prop}=\"{replacement}\" />\n\n\
                             Note: `{old_prop}` was renamed to `{new_prop}`.",
                            value = value,
                            prop = prop,
                            component = component,
                            replacement = replacement,
                            old_prop = old_prop,
                            new_prop = new_prop,
                        )
                    } else {
                        let valid = new_values
                            .iter()
                            .map(|v| format!("\"{}\"", v))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!(
                            "The value \"{value}\" is no longer valid for the `{prop}` prop on <{component}>.\n\
                             Note: `{old_prop}` was renamed to `{new_prop}`.\n\
                             Valid values: {valid}",
                            value = value,
                            prop = prop,
                            component = component,
                            old_prop = old_prop,
                            new_prop = new_prop,
                            valid = valid,
                        )
                    };

                    rules.push(KonveyorRule {
                        rule_id,
                        labels: vec![
                            "source=semver-analyzer".into(),
                            "change-type=prop-value-removed".into(),
                            format!("package={}", pkg),
                        ],
                        effort: 1,
                        category: "mandatory".into(),
                        description: format!(
                            "Value \"{}\" removed from `{}` prop on <{}> (renamed from `{}`)",
                            value, prop, component, old_prop,
                        ),
                        message,
                        links: vec![],
                        when: KonveyorCondition::FrontendReferenced {
                            referenced: FrontendReferencedFields {
                                pattern: format!("^{}$", prop),
                                location: "JSX_PROP".into(),
                                component: Some(format!("^{}$", component)),
                                parent: None,
                                not_parent: None,
                                child: None,
                                not_child: None,
                                requires_child: None,
                                parent_from: None,
                                value: Some(format!("^{}$", regex::escape(value))),
                                from: Some(pkg.to_string()),
                                file_pattern: None,
                            },
                        },
                        fix_strategy: Some(FixStrategyEntry {
                            strategy: "PropValueChange".into(),
                            component: Some(component.clone()),
                            prop: Some(prop.to_string()),
                            from: Some(value.to_string()),
                            replacement: replacement_hint.clone(),
                            ..Default::default()
                        }),
                    });
                }
            }
        }
    }

    rules
}

/// Extract string literal values from a TypeScript union type string.
/// E.g., "'dark' | 'light' | 'default'" → {"dark", "light", "default"}
fn extract_union_values(type_str: &str) -> HashSet<String> {
    let re = regex::Regex::new(r"'([^']+)'").unwrap();
    re.captures_iter(type_str)
        .map(|c| c[1].to_string())
        .collect()
}

/// Try to find a replacement value in the new set for a removed value.
/// Heuristic: looks for common PF rename patterns.
fn find_replacement_value(removed: &str, new_values: &HashSet<String>) -> Option<String> {
    // Common PF v5→v6 renames
    let mappings = [
        ("light", "secondary"),
        ("dark", "secondary"),
        ("darker", "secondary"),
        ("light-200", "secondary"),
        ("light300", "secondary"),
        ("tertiary", "secondary"),
        ("cyan", "teal"),
        ("gold", "yellow"),
        ("alignLeft", "start"),
        ("alignRight", "end"),
        ("button-group", "action-group"),
        ("icon-button-group", "action-group-plain"),
        ("chip-group", "label-group"),
        ("TableComposable", "default"),
    ];

    for (old, new) in &mappings {
        if removed == *old && new_values.contains(*new) {
            return Some(new.to_string());
        }
    }

    None
}

// ── Required prop added rules ───────────────────────────────────────────
//
// When a component gains a new REQUIRED prop (not optional, no default),
// fire on every usage of that component to warn that the prop must be provided.

fn generate_required_prop_added_rules(
    sd: &SdPipelineResult,
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    for (component, required) in &sd.new_required_props {
        let old_props = sd.old_component_props.get(component);
        let old_required = old_props.cloned().unwrap_or_default();

        // Find required props that are NEW (not in old version)
        let newly_required: Vec<&String> = required
            .iter()
            .filter(|p| !old_required.contains(*p))
            // Skip children — it's always "required" but passed as JSX children
            .filter(|p| p.as_str() != "children")
            .collect();

        if newly_required.is_empty() {
            continue;
        }

        let pkg = pkg_for(component, component_packages);

        for prop in &newly_required {
            let rule_id = format!(
                "sd-required-prop-{}-{}",
                sanitize(component),
                sanitize(prop),
            );

            // Look up the type for context
            let type_hint = sd
                .new_component_prop_types
                .get(component)
                .and_then(|types| types.get(*prop))
                .map(|t| format!(" (type: `{}`)", t))
                .unwrap_or_default();

            rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".into(),
                    "change-type=required-prop-added".into(),
                    format!("package={}", pkg),
                ],
                effort: 1,
                category: "mandatory".into(),
                description: format!(
                    "<{}> now requires the `{}` prop{}",
                    component, prop, type_hint,
                ),
                message: format!(
                    "<{}> has a new required prop `{}`{}.\n\
                     This prop must be provided — omitting it will cause a TypeScript error.\n\n\
                     Add the prop: <{} {}={{...}} />",
                    component, prop, type_hint, component, prop,
                ),
                links: vec![],
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: format!("^{}$", component),
                        location: "JSX_COMPONENT".into(),
                        component: None,
                        parent: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
                        parent_from: None,
                        value: None,
                        from: Some(pkg.to_string()),
                        file_pattern: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry {
                    strategy: "LlmAssisted".into(),
                    component: Some(component.clone()),
                    prop: Some(prop.to_string()),
                    ..Default::default()
                }),
            });
        }
    }

    rules
}

// ── Test impact rules ───────────────────────────────────────────────────
//
// Generate rules that match testing-library function calls in test files
// when a component's rendered ARIA roles, aria-label values, or DOM
// structure has changed between versions.

/// Testing Library query function pattern (all variants).
// Test-impact query patterns. The callee resolver produces qualified names
// like "screen.getByRole" for member-expression calls, so we use `(^|\\.)`
// instead of a bare `^` to match both `getByRole` and `screen.getByRole`
// (or `container.querySelector`, `within(el).getByRole`, etc.).
const ROLE_QUERY_PATTERN: &str =
    "(^|\\.)(getByRole|queryByRole|findByRole|getAllByRole|queryAllByRole|findAllByRole)$";
const LABEL_QUERY_PATTERN: &str =
    "(^|\\.)(getByLabelText|queryByLabelText|findByLabelText|getAllByLabelText|queryAllByLabelText|findAllByLabelText)$";
const DATA_ATTR_QUERY_PATTERN: &str =
    "(^|\\.)(querySelector|querySelectorAll|getByAttribute|queryByAttribute|findByAttribute)$";
const TEXT_QUERY_PATTERN: &str =
    "(^|\\.)(getByText|queryByText|findByText|getAllByText|queryAllByText|findAllByText|getByLabelText|queryByLabelText|findByLabelText|getAllByLabelText|queryAllByLabelText|findAllByLabelText)$";
const TEST_FILE_PATTERN: &str = ".*\\.(test|spec)\\.(ts|tsx|js|jsx)$";

/// Map HTML element names to their implicit ARIA roles.
///
/// For `<input>`, the implicit role depends on the `type` attribute, but
/// since the source profile doesn't yet track `type`, we return `"textbox"`
/// as the default (correct for `<input type="text">`). Use
/// [`implicit_role_for_input_override`] to infer the actual implicit role
/// when an explicit `role` attribute overrides the default.
fn implicit_aria_role(element: &str) -> Option<&'static str> {
    match element {
        "button" => Some("button"),
        "input" => Some("textbox"),
        "a" => Some("link"),
        "img" => Some("img"),
        "select" => Some("combobox"),
        "textarea" => Some("textbox"),
        "table" => Some("table"),
        "tr" => Some("row"),
        "td" => Some("cell"),
        "th" => Some("columnheader"),
        "ul" | "ol" => Some("list"),
        "li" => Some("listitem"),
        "nav" => Some("navigation"),
        "main" => Some("main"),
        "header" => Some("banner"),
        "footer" => Some("contentinfo"),
        "form" => Some("form"),
        "dialog" => Some("dialog"),
        "article" => Some("article"),
        "section" => Some("region"),
        "aside" => Some("complementary"),
        "progress" => Some("progressbar"),
        _ => None,
    }
}

/// Infer the implicit role of an `<input>` element when we know an
/// explicit `role` was added to override it. The explicit role hints at
/// what `type` the input has:
///
/// - `role="switch"` → was `<input type="checkbox">` (implicit: "checkbox")
/// - `role="slider"` → was `<input type="range">` (implicit: "slider")
/// - `role="spinbutton"` → was `<input type="number">` (implicit: "spinbutton")
///
/// Returns `None` if the override doesn't help infer the old role.
fn implicit_role_for_input_override(explicit_role: &str) -> Option<&'static str> {
    match explicit_role {
        "switch" => Some("checkbox"),
        "slider" => Some("slider"),
        "spinbutton" => Some("spinbutton"),
        "combobox" => Some("textbox"),
        "searchbox" => Some("textbox"),
        _ => None,
    }
}

/// Check if a value is a concrete string literal (not a JSX expression).
fn is_concrete_value(value: &str) -> bool {
    !value.starts_with('{') && value != "true" && value != "false"
}

fn generate_test_impact_rules(
    changes: &[SourceLevelChange],
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    // ── Phase 1: Direct changes — process individually ──────────────
    //
    // Changes with no dependency_chain are direct (the component's own
    // source changed). Components that exist in both regular and deprecated
    // families (e.g., WizardHeader in Wizard and deprecated/Wizard) can
    // produce duplicate SourceLevelChange entries with identical rule IDs.
    // Deduplicated after the loop via retain().
    for change in changes {
        if !change.has_test_implications {
            continue;
        }
        // Skip transitive changes — handled in Phase 2
        if change.dependency_chain.is_some() {
            continue;
        }

        let pkg = pkg_for(&change.component, component_packages);

        match change.category {
            // ── Role changes: match getByRole('oldValue') ───────────
            SourceLevelCategory::RoleChange => {
                if let Some(ref old_val) = change.old_value {
                    if !is_concrete_value(old_val) {
                        continue;
                    }

                    let prefix = rule_prefix(&change.migration_from);
                    let elem_part = change
                        .element
                        .as_deref()
                        .map(|e| format!("-{}", sanitize(e)))
                        .unwrap_or_default();
                    let rule_id = format!(
                        "{}-test-{}-role-{}{}-{}",
                        prefix,
                        sanitize(&change.component),
                        sanitize(old_val),
                        elem_part,
                        if change.new_value.is_some() {
                            "changed"
                        } else {
                            "removed"
                        },
                    );

                    let message = if let Some(ref new_val) = change.new_value {
                        if is_concrete_value(new_val) {
                            format!(
                                "{} role changed from '{}' to '{}'.\n\n\
                                 Update test queries:\n  \
                                 getByRole('{}') → getByRole('{}')",
                                change.component, old_val, new_val, old_val, new_val
                            )
                        } else {
                            format!(
                                "{} role '{}' changed to a dynamic value.\n\n\
                                 Tests using getByRole('{}') may need updating.\n\n\
                                 {}",
                                change.component, old_val, old_val, change.description
                            )
                        }
                    } else {
                        format!(
                            "{} no longer has role='{}'.\n\n\
                             Tests using getByRole('{}') to find this component will fail.\n\n\
                             {}",
                            change.component, old_val, old_val, change.description
                        )
                    };

                    rules.push(KonveyorRule {
                        rule_id,
                        labels: vec![
                            "source=semver-analyzer".into(),
                            "change-type=test-impact".into(),
                            "impact=frontend-testing".into(),
                            format!("package={}", pkg),
                        ],
                        effort: 1,
                        category: "optional".into(),
                        description: format!(
                            "Test impact: {} role '{}' {}",
                            change.component,
                            old_val,
                            if change.new_value.is_some() {
                                "changed"
                            } else {
                                "removed"
                            }
                        ),
                        message,
                        links: vec![],
                        when: KonveyorCondition::FrontendReferenced {
                            referenced: FrontendReferencedFields {
                                pattern: ROLE_QUERY_PATTERN.into(),
                                location: "FUNCTION_CALL".into(),
                                component: None,
                                parent: None,
                                not_parent: None,
                                child: None,
                                not_child: None,
                                requires_child: None,
                                parent_from: None,
                                value: Some(format!("^{}$", old_val)),
                                from: None,
                                file_pattern: Some(TEST_FILE_PATTERN.into()),
                            },
                        },
                        fix_strategy: None,
                    });
                } else if let Some(ref new_val) = change.new_value {
                    // Role ADDED — the element previously had no explicit role,
                    // so consumers used the implicit role from the element type.
                    // Infer the old implicit role and generate a rule for it.
                    if !is_concrete_value(new_val) {
                        continue;
                    }

                    let element = change.element.as_deref().unwrap_or("");
                    let inferred_old = if element == "input" {
                        implicit_role_for_input_override(new_val)
                    } else {
                        implicit_aria_role(element)
                    };

                    if let Some(old_implicit) = inferred_old {
                        // Only generate a rule if the new explicit role differs
                        // from the old implicit role (otherwise nothing changed
                        // for consumers).
                        if old_implicit == new_val {
                            continue;
                        }

                        let prefix = rule_prefix(&change.migration_from);
                        let elem_part = change
                            .element
                            .as_deref()
                            .map(|e| format!("-{}", sanitize(e)))
                            .unwrap_or_default();
                        let rule_id = format!(
                            "{}-test-{}-role-{}{}-overridden",
                            prefix,
                            sanitize(&change.component),
                            sanitize(old_implicit),
                            elem_part,
                        );

                        let message = format!(
                            "{} changed from implicit role '{}' (from <{}>) to explicit role='{}'.\n\n\
                             Update test queries:\n  \
                             getByRole('{}') → getByRole('{}')",
                            change.component,
                            old_implicit,
                            element,
                            new_val,
                            old_implicit,
                            new_val,
                        );

                        rules.push(KonveyorRule {
                            rule_id,
                            labels: vec![
                                "source=semver-analyzer".into(),
                                "change-type=test-impact".into(),
                                "impact=frontend-testing".into(),
                                format!("package={}", pkg),
                            ],
                            effort: 1,
                            category: "optional".into(),
                            description: format!(
                                "Test impact: {} implicit role '{}' overridden by '{}'",
                                change.component, old_implicit, new_val,
                            ),
                            message,
                            links: vec![],
                            when: KonveyorCondition::FrontendReferenced {
                                referenced: FrontendReferencedFields {
                                    pattern: ROLE_QUERY_PATTERN.into(),
                                    location: "FUNCTION_CALL".into(),
                                    component: None,
                                    parent: None,
                                    not_parent: None,
                                    child: None,
                                    not_child: None,
                                    requires_child: None,
                                    parent_from: None,
                                    value: Some(format!("^{}$", old_implicit)),
                                    from: None,
                                    file_pattern: Some(TEST_FILE_PATTERN.into()),
                                },
                            },
                            fix_strategy: None,
                        });
                    }
                }
            }

            // ── ARIA changes ─────────────────────────────────────────
            SourceLevelCategory::AriaChange => {
                if change.description.contains("aria-label") {
                    // ── aria-label changes: match getByLabelText('oldValue') ─
                    if let Some(ref old_val) = change.old_value {
                        if !is_concrete_value(old_val) {
                            continue;
                        }

                        let prefix = rule_prefix(&change.migration_from);
                        let elem_part = change
                            .element
                            .as_deref()
                            .map(|e| format!("-{}", sanitize(e)))
                            .unwrap_or_default();
                        let rule_id = format!(
                            "{}-test-{}-aria-label-{}{}-{}",
                            prefix,
                            sanitize(&change.component),
                            sanitize(old_val),
                            elem_part,
                            if change.new_value.is_some() {
                                "changed"
                            } else {
                                "removed"
                            },
                        );

                        let message = if let Some(ref new_val) = change.new_value {
                            if is_concrete_value(new_val) {
                                format!(
                                    "{} aria-label changed from '{}' to '{}'.\n\n\
                                     Update test queries:\n  \
                                     getByLabelText('{}') → getByLabelText('{}')",
                                    change.component, old_val, new_val, old_val, new_val
                                )
                            } else {
                                format!(
                                    "{} aria-label '{}' changed to a dynamic value.\n\n\
                                     Tests using getByLabelText('{}') may need updating.\n\n\
                                     {}",
                                    change.component, old_val, old_val, change.description
                                )
                            }
                        } else {
                            format!(
                                "{} no longer has aria-label='{}'.\n\n\
                                 Tests using getByLabelText('{}') to find this component will fail.\n\n\
                                 {}",
                                change.component, old_val, old_val, change.description
                            )
                        };

                        rules.push(KonveyorRule {
                            rule_id,
                            labels: vec![
                                "source=semver-analyzer".into(),
                                "change-type=test-impact".into(),
                                "impact=frontend-testing".into(),
                                format!("package={}", pkg),
                            ],
                            effort: 1,
                            category: "optional".into(),
                            description: format!(
                                "Test impact: {} aria-label '{}' {}",
                                change.component,
                                old_val,
                                if change.new_value.is_some() {
                                    "changed"
                                } else {
                                    "removed"
                                }
                            ),
                            message,
                            links: vec![],
                            when: KonveyorCondition::FrontendReferenced {
                                referenced: FrontendReferencedFields {
                                    pattern: LABEL_QUERY_PATTERN.into(),
                                    location: "FUNCTION_CALL".into(),
                                    component: None,
                                    parent: None,
                                    not_parent: None,
                                    child: None,
                                    not_child: None,
                                    requires_child: None,
                                    parent_from: None,
                                    value: Some(format!("^{}$", old_val)),
                                    from: None,
                                    file_pattern: Some(TEST_FILE_PATTERN.into()),
                                },
                            },
                            fix_strategy: None,
                        });
                    }
                } else {
                    // ── Other ARIA attribute changes (aria-disabled, etc.):
                    //    match getAttribute('attrName') in test files ─────
                    //
                    // When an ARIA attribute other than aria-label is removed
                    // or changed, tests using getAttribute() or toHaveAttribute()
                    // with that attribute will break. Generate a FileContent rule
                    // (same approach as AttributeConditionality).

                    // Skip "added" — new attributes don't break existing tests
                    if change.description.contains("attribute added") {
                        continue;
                    }

                    let attr_name = match extract_aria_attr_name(&change.description) {
                        Some(name) => name,
                        None => continue,
                    };

                    let prefix = rule_prefix(&change.migration_from);
                    let elem_part = change
                        .element
                        .as_deref()
                        .map(|e| format!("-{}", sanitize(e)))
                        .unwrap_or_default();
                    let rule_id = format!(
                        "{}-test-{}-aria-{}{}-{}",
                        prefix,
                        sanitize(&change.component),
                        sanitize(&attr_name),
                        elem_part,
                        if change.new_value.is_some() {
                            "changed"
                        } else {
                            "removed"
                        },
                    );

                    let elem_display = change.element.as_deref().unwrap_or("element");
                    let message = if change.new_value.is_none() {
                        format!(
                            "{component} no longer renders `{attr}` on `<{elem}>`.\n\n\
                             Tests using `getAttribute('{attr}')` will now get `null`.\n\n\
                             Update test assertions:\n  \
                             `.getAttribute('{attr}')` → check the native HTML attribute instead \
                             (e.g., `.toBeDisabled()` or `.toHaveAttribute('disabled')`)\n  \
                             `.getAttribute('{attr}').toBe('false')` → `.not.toHaveAttribute('{attr}')`",
                            component = change.component,
                            attr = attr_name,
                            elem = elem_display,
                        )
                    } else {
                        format!(
                            "{component} changed how `{attr}` is rendered on `<{elem}>`.\n\n\
                             Tests using `getAttribute('{attr}')` may return different values.\n\n\
                             {}",
                            change.description,
                            component = change.component,
                            attr = attr_name,
                            elem = elem_display,
                        )
                    };

                    rules.push(KonveyorRule {
                        rule_id,
                        labels: vec![
                            "source=semver-analyzer".into(),
                            "change-type=test-impact".into(),
                            "impact=frontend-testing".into(),
                            format!("package={}", pkg),
                        ],
                        effort: 1,
                        category: "optional".into(),
                        description: format!(
                            "Test impact: {} `{}` {}",
                            change.component,
                            attr_name,
                            if change.new_value.is_some() {
                                "changed"
                            } else {
                                "removed"
                            },
                        ),
                        message,
                        links: vec![],
                        when: KonveyorCondition::FileContent {
                            filecontent: FileContentFields {
                                pattern: format!(
                                    "(getAttribute|toHaveAttribute|\\.not\\.toHaveAttribute)\\(\\s*['\"]{}['\"]\\s*[,)]",
                                    regex_escape(&attr_name),
                                ),
                                file_pattern: TEST_FILE_PATTERN.into(),
                            },
                        },
                        fix_strategy: Some(FixStrategyEntry::new("LlmAssisted")),
                    });
                }
            }

            // ── DOM structure changes: match getByRole(implicit_role) ─
            SourceLevelCategory::DomStructure => {
                if let Some(ref old_val) = change.old_value {
                    let element = old_val
                        .trim_start_matches('<')
                        .split('>')
                        .next()
                        .unwrap_or("")
                        .trim();

                    if let Some(role) = implicit_aria_role(element) {
                        let prefix = rule_prefix(&change.migration_from);
                        let rule_id = format!(
                            "{}-test-{}-dom-{}-removed",
                            prefix,
                            sanitize(&change.component),
                            sanitize(element),
                        );

                        rules.push(KonveyorRule {
                            rule_id,
                            labels: vec![
                                "source=semver-analyzer".into(),
                                "change-type=test-impact".into(),
                                "impact=frontend-testing".into(),
                                format!("package={}", pkg),
                            ],
                            effort: 1,
                            category: "optional".into(),
                            description: format!(
                                "Test impact: {} no longer renders <{}>",
                                change.component, element
                            ),
                            message: format!(
                                "{} no longer renders a <{}> element (implicit role='{}').\n\n\
                                 Tests using getByRole('{}') inside {} may fail.\n\n\
                                 {}",
                                change.component,
                                element,
                                role,
                                role,
                                change.component,
                                change.description,
                            ),
                            links: vec![],
                            when: KonveyorCondition::FrontendReferenced {
                                referenced: FrontendReferencedFields {
                                    pattern: ROLE_QUERY_PATTERN.into(),
                                    location: "FUNCTION_CALL".into(),
                                    component: None,
                                    parent: None,
                                    not_parent: None,
                                    child: None,
                                    not_child: None,
                                    requires_child: None,
                                    parent_from: None,
                                    value: Some(format!("^{}$", role)),
                                    from: None,
                                    file_pattern: Some(TEST_FILE_PATTERN.into()),
                                },
                            },
                            fix_strategy: None,
                        });
                    }
                }
            }

            // ── Attribute conditionality: match getAttribute('attrName') ─
            SourceLevelCategory::AttributeConditionality => {
                let attr_name = match change.description.split(" on <").next() {
                    Some(name) if !name.is_empty() => name.to_string(),
                    _ => continue,
                };

                let elem_part = change
                    .element
                    .as_deref()
                    .map(|e| format!("-{}", sanitize(e)))
                    .unwrap_or_default();
                let prefix = rule_prefix(&change.migration_from);
                let rule_id = format!(
                    "{}-test-{}-attr-conditionality-{}{}",
                    prefix,
                    sanitize(&change.component),
                    sanitize(&attr_name),
                    elem_part,
                );

                let elem_display = change.element.as_deref().unwrap_or("element");
                let message = format!(
                    "{component} no longer always renders `{attr}` on `<{elem}>`.\n\n\
                     Previously, `getAttribute('{attr}')` returned a string value \
                     (e.g., `\"false\"`) even when the attribute was semantically \"off\". \
                     Now the attribute is omitted entirely when not active, so \
                     `getAttribute('{attr}')` returns `null`.\n\n\
                     Update test assertions:\n  \
                     `.getAttribute('{attr}').toBe('false')` → `.not.toHaveAttribute('{attr}')`\n  \
                     or: `.getAttribute('{attr}').toBeNull()`\n  \
                     or: `expect(element).not.toHaveAttribute('{attr}')`",
                    component = change.component,
                    attr = attr_name,
                    elem = elem_display,
                );

                rules.push(KonveyorRule {
                    rule_id,
                    labels: vec![
                        "source=semver-analyzer".into(),
                        "change-type=test-impact".into(),
                        "change-type=attribute-conditionality".into(),
                        "impact=frontend-testing".into(),
                        format!("package={}", pkg),
                    ],
                    effort: 1,
                    category: "optional".into(),
                    description: format!(
                        "Test impact: {} `{}` now conditionally rendered",
                        change.component, attr_name,
                    ),
                    message,
                    links: vec![],
                    when: KonveyorCondition::FileContent {
                        filecontent: FileContentFields {
                            pattern: format!(
                                "(getAttribute|toHaveAttribute|\\.not\\.toHaveAttribute)\\(\\s*['\"]{}['\"]\\s*[,)]",
                                regex_escape(&attr_name),
                            ),
                            file_pattern: TEST_FILE_PATTERN.into(),
                        },
                    },
                    fix_strategy: Some(FixStrategyEntry::new("Manual")),
                });
            }

            // ── Prop default changes: match getByText/getByLabelText('old') ─
            SourceLevelCategory::PropDefault => {
                if let Some(ref old_val) = change.old_value {
                    // Strip quotes to get the raw string value
                    let unquoted = old_val.trim_matches(|c| c == '\'' || c == '"');
                    if unquoted.is_empty() || !is_concrete_value(unquoted) {
                        continue;
                    }

                    let prefix = rule_prefix(&change.migration_from);
                    let rule_id = format!(
                        "{}-test-{}-default-{}-changed",
                        prefix,
                        sanitize(&change.component),
                        sanitize(unquoted),
                    );

                    let new_display = change
                        .new_value
                        .as_deref()
                        .map(|v| v.trim_matches(|c| c == '\'' || c == '"'))
                        .unwrap_or("(removed)");

                    let message = if change.new_value.is_some() {
                        format!(
                            "{} default prop value changed: '{}' → '{}'.\n\n\
                             Tests using queries that match the old default value will fail:\n  \
                             getByLabelText('{}') → getByLabelText('{}')\n  \
                             getByText('{}') → getByText('{}')",
                            change.component,
                            unquoted,
                            new_display,
                            unquoted,
                            new_display,
                            unquoted,
                            new_display,
                        )
                    } else {
                        format!(
                            "{} default prop value '{}' was removed.\n\n\
                             Tests using queries that match the old default value will fail:\n  \
                             getByLabelText('{}') and getByText('{}') may no longer match.",
                            change.component, unquoted, unquoted, unquoted,
                        )
                    };

                    rules.push(KonveyorRule {
                        rule_id,
                        labels: vec![
                            "source=semver-analyzer".into(),
                            "change-type=test-impact".into(),
                            "impact=frontend-testing".into(),
                            format!("package={}", pkg),
                        ],
                        effort: 1,
                        category: "optional".into(),
                        description: format!(
                            "Test impact: {} default '{}' {}",
                            change.component,
                            unquoted,
                            if change.new_value.is_some() {
                                "changed"
                            } else {
                                "removed"
                            }
                        ),
                        message,
                        links: vec![],
                        when: KonveyorCondition::FrontendReferenced {
                            referenced: FrontendReferencedFields {
                                pattern: TEXT_QUERY_PATTERN.into(),
                                location: "FUNCTION_CALL".into(),
                                component: None,
                                parent: None,
                                not_parent: None,
                                child: None,
                                not_child: None,
                                requires_child: None,
                                parent_from: None,
                                value: Some(format!(
                                    "^{}$",
                                    regex::escape(unquoted)
                                )),
                                from: None,
                                file_pattern: Some(TEST_FILE_PATTERN.into()),
                            },
                        },
                        fix_strategy: None,
                    });
                }
            }

            // DataAttribute and PortalUsage are transitive-only — handled in Phase 2
            _ => {}
        }
    }

    // Deduplicate Phase 1 rules. Components that exist in both regular and
    // deprecated families (e.g., WizardHeader in Wizard and deprecated/Wizard)
    // produce duplicate SourceLevelChange entries with identical rule IDs.
    {
        let mut seen = HashSet::new();
        rules.retain(|r| seen.insert(r.rule_id.clone()));
    }

    // ── Phase 2: Transitive changes — one consolidated rule per component ─
    //
    // Group all transitive changes (dependency_chain: Some) by component.
    // For each component, build a single rule with OR'd when conditions
    // to prevent duplicate rule IDs when multiple sub-components produce
    // the same category of change.
    let mut transitive_by_component: HashMap<String, Vec<&SourceLevelChange>> = HashMap::new();
    for change in changes {
        if !change.has_test_implications {
            continue;
        }
        if change.dependency_chain.is_none() {
            continue;
        }
        transitive_by_component
            .entry(change.component.clone())
            .or_default()
            .push(change);
    }

    for (component, component_changes) in &transitive_by_component {
        if !component_packages.contains_key(component) {
            continue;
        }
        let pkg = pkg_for(component, component_packages);

        // Build individual when conditions and message parts for each change.
        // Use a HashSet to deduplicate identical conditions (e.g., two sub-components
        // both losing <button> produce the same getByRole('button') condition).
        let mut conditions: Vec<KonveyorCondition> = Vec::new();
        let mut seen_condition_keys: HashSet<String> = HashSet::new();
        let mut message_parts: Vec<String> = Vec::new();
        let mut max_effort = 1u32;

        for change in component_changes {
            if let Some((cond, key)) =
                build_when_for_transitive_change(change, component, &pkg)
            {
                if seen_condition_keys.insert(key) {
                    conditions.push(cond);
                }
            }
            message_parts.push(format!("- {}", change.description));
            // PortalUsage changes are harder to fix (effort 3)
            if change.category == SourceLevelCategory::PortalUsage {
                max_effort = max_effort.max(3);
            }
        }

        if conditions.is_empty() {
            continue;
        }

        let when = if conditions.len() == 1 {
            conditions.remove(0)
        } else {
            KonveyorCondition::Or { or: conditions }
        };

        let rule_id = format!(
            "sd-test-{}-transitive-behavioral-changes",
            sanitize(&component.to_lowercase()),
        );

        let message = format!(
            "{component} is affected by behavioral changes in rendered sub-components:\n\
             {changes}\n\n\
             Tests for {component} may need updating. Fix options depend on the specific change:\n\
             - For portal changes: use waitFor(), popperProps={{{{ appendTo: 'inline' }}}}, \
             or within(document.body)\n\
             - For role/DOM changes: update getByRole() queries to match new roles\n\
             - For attribute changes: update querySelector/getAttribute calls",
            changes = message_parts.join("\n"),
        );

        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".into(),
                "change-type=test-impact".into(),
                "impact=frontend-testing".into(),
                format!("package={}", pkg),
            ],
            effort: max_effort,
            category: "potential".into(),
            description: format!(
                "Test impact: {} has transitive behavioral changes",
                component,
            ),
            message,
            links: vec![],
            when,
            fix_strategy: None,
        });
    }

    rules
}

/// Generate prop-level rules for components with PortalUsage changes.
///
/// When a component's portal rendering behavior changed (e.g., Tooltip/Popover's
/// Popper `appendTo` default changed from `'inline'` to `() => document.body`),
/// generate JSX_PROP rules that catch:
///
/// 1. `appendTo="inline"` or `appendTo="parent"` — string values that may not
///    be accepted by the component's `appendTo` prop type
/// 2. `popperProps={...}` — prop that may have been removed from the component
///
/// These complement the IMPORT-level transitive behavioral change rules by
/// providing specific prop-level detection.
///
/// Both **direct** and **transitive** portal changes are processed. Transitive
/// changes (e.g., Tooltip → Popper `appendTo` default changed) are included
/// because the consumer-facing component (Tooltip) still exposes `appendTo`
/// and `popperProps` to the consumer, and the `'inline'` string value is no
/// longer valid in the prop's type signature.
fn generate_portal_prop_rules(
    changes: &[SourceLevelChange],
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();
    let mut seen_components = std::collections::HashSet::new();

    for change in changes {
        if change.category != SourceLevelCategory::PortalUsage {
            continue;
        }

        let component = &change.component;
        if !seen_components.insert(component.clone()) {
            continue;
        }

        let pkg = pkg_for(component, component_packages);
        let prefix = rule_prefix(&change.migration_from);

        // Rule A: appendTo with string value
        // Components affected by portal changes may no longer accept string
        // values for appendTo (e.g., Tooltip accepts HTMLElement|Function but
        // not "inline"). Generate a rule that flags string values.
        let rule_id_appendto = format!(
            "{}-{}-appendto-string-invalid",
            prefix,
            sanitize(&component.to_lowercase()),
        );
        rules.push(KonveyorRule {
            rule_id: rule_id_appendto,
            labels: vec![
                "source=semver-analyzer".into(),
                "change-type=prop-value-change".into(),
                format!("package={}", pkg),
            ],
            effort: 1,
            category: "mandatory".into(),
            description: format!(
                "{} appendTo prop may not accept string values",
                component,
            ),
            message: format!(
                "{component}'s portal rendering behavior changed. The appendTo \
                 prop may no longer accept string values like \"inline\" or \"parent\".\n\n\
                 Remove appendTo=\"inline\" — the default portal behavior is \
                 sufficient in most cases. If you need specific portal targeting, \
                 use a function: appendTo={{() => document.body}}.",
            ),
            links: vec![],
            when: KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: "^appendTo$".into(),
                    location: "JSX_PROP".into(),
                    component: Some(format!("^{}$", component)),
                    from: Some(pkg.clone()),
                    value: Some("^(inline|parent)$".into()),
                    parent: None,
                    not_parent: None,
                    child: None,
                    not_child: None,
                    requires_child: None,
                    parent_from: None,
                    file_pattern: None,
                },
            },
            fix_strategy: Some(FixStrategyEntry::new("RemoveAttribute")),
        });

        // Rule B: popperProps (may not exist on this component)
        // Some components had popperProps in v5 but removed it in v6.
        // Generate a rule that flags any usage of popperProps.
        let rule_id_popper = format!(
            "{}-{}-popperprops-removed",
            prefix,
            sanitize(&component.to_lowercase()),
        );
        rules.push(KonveyorRule {
            rule_id: rule_id_popper,
            labels: vec![
                "source=semver-analyzer".into(),
                "change-type=prop-removal".into(),
                format!("package={}", pkg),
            ],
            effort: 1,
            category: "optional".into(),
            description: format!(
                "{} popperProps prop may have been removed",
                component,
            ),
            message: format!(
                "{component} may no longer accept popperProps. If this causes a \
                 TypeScript error, remove the popperProps prop entirely.",
            ),
            links: vec![],
            when: KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: "^popperProps$".into(),
                    location: "JSX_PROP".into(),
                    component: Some(format!("^{}$", component)),
                    from: Some(pkg.clone()),
                    parent: None,
                    not_parent: None,
                    child: None,
                    not_child: None,
                    requires_child: None,
                    parent_from: None,
                    value: None,
                    file_pattern: None,
                },
            },
            fix_strategy: Some(FixStrategyEntry::new("RemoveAttribute")),
        });
    }

    if !rules.is_empty() {
        tracing::info!(
            count = rules.len(),
            "Generated portal prop rules"
        );
    }

    rules
}

/// Build a `KonveyorCondition` for a transitive source-level change.
///
/// Returns `Some((condition, dedup_key))` where `dedup_key` is a string
/// that uniquely identifies the condition to prevent duplicates when
/// multiple sub-components produce the same type of change.
///
/// Returns `None` if the change doesn't produce a meaningful test-impact
/// condition (e.g., non-concrete values, missing data).
fn build_when_for_transitive_change(
    change: &SourceLevelChange,
    component: &str,
    pkg: &str,
) -> Option<(KonveyorCondition, String)> {
    match change.category {
        SourceLevelCategory::RoleChange => {
            let old_val = change.old_value.as_ref()?;
            if !is_concrete_value(old_val) {
                return None;
            }
            let key = format!("role:{}", old_val);
            Some((
                KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: ROLE_QUERY_PATTERN.into(),
                        location: "FUNCTION_CALL".into(),
                        component: None,
                        parent: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
                        parent_from: None,
                        value: Some(format!("^{}$", old_val)),
                        from: None,
                        file_pattern: Some(TEST_FILE_PATTERN.into()),
                    },
                },
                key,
            ))
        }

        SourceLevelCategory::AriaChange => {
            if change.description.contains("aria-label") {
                // aria-label changes: match getByLabelText('oldValue')
                let old_val = change.old_value.as_ref()?;
                if !is_concrete_value(old_val) {
                    return None;
                }
                let key = format!("aria-label:{}", old_val);
                Some((
                    KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: LABEL_QUERY_PATTERN.into(),
                            location: "FUNCTION_CALL".into(),
                            component: None,
                            parent: None,
                            not_parent: None,
                            child: None,
                            not_child: None,
                            requires_child: None,
                            parent_from: None,
                            value: Some(format!("^{}$", old_val)),
                            from: None,
                            file_pattern: Some(TEST_FILE_PATTERN.into()),
                        },
                    },
                    key,
                ))
            } else {
                // Other ARIA attribute changes (aria-disabled, etc.):
                // match getAttribute('attrName') in test files.
                let attr_name = extract_aria_attr_name(&change.description)?;
                let key = format!("aria-attr:{}", attr_name);
                Some((
                    KonveyorCondition::FileContent {
                        filecontent: FileContentFields {
                            pattern: format!(
                                "(getAttribute|toHaveAttribute|\\.not\\.toHaveAttribute)\\(\\s*['\"]{}['\"]\\s*[,)]",
                                regex_escape(&attr_name),
                            ),
                            file_pattern: TEST_FILE_PATTERN.into(),
                        },
                    },
                    key,
                ))
            }
        }

        SourceLevelCategory::DomStructure => {
            let old_val = change.old_value.as_ref()?;
            let element = old_val
                .trim_start_matches('<')
                .split('>')
                .next()
                .unwrap_or("")
                .trim();
            let role = implicit_aria_role(element)?;
            let key = format!("dom:{}:{}", element, role);
            Some((
                KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: ROLE_QUERY_PATTERN.into(),
                        location: "FUNCTION_CALL".into(),
                        component: None,
                        parent: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
                        parent_from: None,
                        value: Some(format!("^{}$", role)),
                        from: None,
                        file_pattern: Some(TEST_FILE_PATTERN.into()),
                    },
                },
                key,
            ))
        }

        SourceLevelCategory::DataAttribute => {
            let old_val = change.old_value.as_ref()?;
            let (attr_name, raw_old_value) = if let Some(idx) = old_val.find("=\"") {
                let attr = &old_val[..idx];
                let val = old_val[idx + 2..].trim_end_matches('"');
                (attr.to_string(), val.to_string())
            } else if let Some(idx) = old_val.find(": ") {
                (old_val[..idx].to_string(), old_val[idx + 2..].to_string())
            } else {
                return None;
            };
            let old_value = raw_old_value.replace("${componentType}", component);
            if !is_concrete_value(&old_value) {
                return None;
            }
            let key = format!("data:{}:{}", attr_name, old_value);
            Some((
                KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: DATA_ATTR_QUERY_PATTERN.into(),
                        location: "FUNCTION_CALL".into(),
                        component: None,
                        parent: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
                        parent_from: None,
                        value: Some(format!(".*{}.*", regex_escape(&old_value))),
                        from: None,
                        file_pattern: Some(TEST_FILE_PATTERN.into()),
                    },
                },
                key,
            ))
        }

        SourceLevelCategory::AttributeConditionality => {
            let attr_name = match change.description.split(" on <").next() {
                Some(name) if !name.is_empty() => name.to_string(),
                _ => return None,
            };
            let key = format!("attr-cond:{}", attr_name);
            Some((
                KonveyorCondition::FileContent {
                    filecontent: FileContentFields {
                        pattern: format!(
                            "(getAttribute|toHaveAttribute|\\.not\\.toHaveAttribute)\\(\\s*['\"]{}['\"]\\s*[,)]",
                            regex_escape(&attr_name),
                        ),
                        file_pattern: TEST_FILE_PATTERN.into(),
                    },
                },
                key,
            ))
        }

        SourceLevelCategory::PortalUsage => {
            let key = "portal".to_string();
            Some((
                KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: format!("^{}$", component),
                        location: "IMPORT".into(),
                        component: None,
                        parent: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
                        parent_from: None,
                        value: None,
                        from: Some(pkg.to_string()),
                        file_pattern: None,
                    },
                },
                key,
            ))
        }

        _ => None,
    }
}

// ── CSS class removal rules ─────────────────────────────────────────────
//
// When entire CSS component blocks are removed between PF versions (e.g.,
// Select CSS removed because Select now uses Menu's CSS), generate rules
// that flag consumer CSS files referencing the removed class prefixes.

// ── Composition inversion rules ─────────────────────────────────────────
// Detect when an internal subcomponent was removed from a family and the
// parent gained a render-function prop instead. The consumer must now provide
// the subcomponent via a render prop rather than having it managed internally.
//
// Example: deprecated Select rendered <SelectToggle> internally. Next-gen
// Select exposes `toggle: (toggleRef) => ReactNode` — the consumer provides
// <MenuToggle> via the render prop.

/// Returns true if the type string looks like a render function — a function
/// that returns a React element. Matches patterns like:
/// - `(toggleRef: React.Ref<...>) => React.ReactNode`
/// - `((toggleRef: React.RefObject<any>) => React.ReactNode) | SelectToggleProps`
fn is_render_prop_type(type_str: &str) -> bool {
    type_str.contains("=>") && {
        let lower = type_str.to_lowercase();
        lower.contains("reactnode")
            || lower.contains("react.reactnode")
            || lower.contains("reactelement")
            || lower.contains("jsx.element")
    }
}

fn generate_composition_inversion_rules(
    sd: &SdPipelineResult,
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    for new_tree in &sd.composition_trees {
        let root = &new_tree.root;
        let pkg = pkg_for(root, component_packages);

        // Find the old tree for this family
        let old_tree = match sd.old_composition_trees.iter().find(|t| t.root == *root) {
            Some(t) => t,
            None => continue,
        };

        // Find removed family members (in old tree but not in new tree)
        let new_members: HashSet<&str> =
            new_tree.family_members.iter().map(|s| s.as_str()).collect();

        // Compute added props on the root
        let old_root_props: BTreeSet<&str> = sd
            .old_component_props
            .get(root)
            .map(|s| s.iter().map(|p| p.as_str()).collect())
            .unwrap_or_default();
        let new_root_props: BTreeSet<&str> = sd
            .new_component_props
            .get(root)
            .map(|s| s.iter().map(|p| p.as_str()).collect())
            .unwrap_or_default();
        let added_props: BTreeSet<&str> = new_root_props
            .difference(&old_root_props)
            .copied()
            .collect();

        // Get prop types for the root
        let new_prop_types = sd.new_component_prop_types.get(root);

        for old_member in &old_tree.family_members {
            // Only consider removed members
            if new_members.contains(old_member.as_str()) || old_member == root {
                continue;
            }

            // Check if any added prop on the root is a render function whose
            // name matches the removed member. We check several patterns:
            // - "SelectToggle" removed, "toggle" prop added
            // - Strip the root prefix: "Select" + "Toggle" → "toggle"
            let member_lower = old_member.to_lowercase();
            let root_lower = root.to_lowercase();
            let stripped = member_lower
                .strip_prefix(&root_lower)
                .unwrap_or(&member_lower);

            for prop_name in &added_props {
                let prop_lower = prop_name.to_lowercase();

                // Check name match: prop matches the stripped member name
                if prop_lower != stripped
                    && !stripped.contains(&prop_lower)
                    && !prop_lower.contains(stripped)
                {
                    continue;
                }

                // Check if the prop type is a render function
                let is_render = new_prop_types
                    .and_then(|types| types.get(*prop_name))
                    .map(|t| is_render_prop_type(t))
                    .unwrap_or(false);

                if !is_render {
                    continue;
                }

                // Composition inversion detected!
                let prop_type = new_prop_types
                    .and_then(|types| types.get(*prop_name))
                    .cloned()
                    .unwrap_or_default();

                let rule_id = format!(
                    "sd-composition-inversion-{}-{}-to-{}",
                    sanitize(root),
                    sanitize(old_member),
                    sanitize(prop_name),
                );

                let message = format!(
                    "<{root}> no longer internally renders <{old_member}>.\n\
                     Instead, provide a render function via the `{prop}` prop.\n\n\
                     The `{prop}` prop accepts: `{prop_type}`\n\n\
                     Before (v5):\n\
                     \x20 <{root}>\n\
                     \x20   {{/* {old_member} was rendered internally */}}\n\
                     \x20 </{root}>\n\n\
                     After (v6):\n\
                     \x20 <{root} {prop}={{(ref) => <MenuToggle ref={{ref}}>...</MenuToggle>}}>\n\
                     \x20   ...\n\
                     \x20 </{root}>\n\n\
                     Any props previously passed to <{root}> that controlled {old_member}\n\
                     (e.g., onToggle, toggleRef, toggleAriaLabel) should now be set\n\
                     directly on the component you provide via the `{prop}` render function.",
                    root = root,
                    old_member = old_member,
                    prop = prop_name,
                    prop_type = prop_type,
                );

                rules.push(KonveyorRule {
                    rule_id,
                    labels: vec![
                        "source=semver-analyzer".into(),
                        "change-type=composition-inversion".into(),
                        format!("package={}", pkg),
                        format!("family={}", root),
                    ],
                    effort: 5,
                    category: "mandatory".into(),
                    description: format!(
                        "<{}> internal <{}> replaced by `{}` render prop",
                        root, old_member, prop_name,
                    ),
                    message,
                    links: vec![],
                    when: KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: format!("^{}$", regex_escape(root)),
                            location: "IMPORT".into(),
                            component: None,
                            parent: None,
                            not_parent: None,
                            child: None,
                            not_child: None,
                            requires_child: None,
                            parent_from: None,
                            value: None,
                            from: Some(pkg.clone()),
                            file_pattern: None,
                        },
                    },
                    fix_strategy: Some(FixStrategyEntry {
                        strategy: "CompositionInversion".into(),
                        from: Some(old_member.clone()),
                        to: Some(prop_name.to_string()),
                        component: Some(root.clone()),
                        prop: Some(prop_name.to_string()),
                        ..Default::default()
                    }),
                });

                break; // Only one rule per removed member
            }
        }
    }

    rules
}

// ── Prop attribute override rules ───────────────────────────────────────
// When a component extracts a prop, transforms it via a helper, and spreads
// the result after rest props — overriding any consumer-provided HTML attribute.

fn generate_prop_attribute_override_rules(
    changes: &[SourceLevelChange],
    _sd: &SdPipelineResult,
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    for change in changes {
        if change.category != SourceLevelCategory::PropAttributeOverride {
            continue;
        }

        // Only generate rules for "new managed attribute" (not "removed")
        if change.old_value.is_some() && change.new_value.is_none() {
            continue;
        }

        let pkg = pkg_for(&change.component, component_packages);
        let prefix = rule_prefix(&change.migration_from);

        // For migration changes, match imports from the deprecated path.
        let from_pkg = if let Some(ref mf) = change.migration_from {
            deprecated_pkg_from_migration_path(mf)
        } else {
            pkg.clone()
        };

        // Parse the new_value to extract overridden attribute names.
        // Format is "propName → attr1, attr2, attr3"
        let (prop_name, overridden_attrs) = match &change.new_value {
            Some(val) => {
                let parts: Vec<&str> = val.splitn(2, " → ").collect();
                if parts.len() == 2 {
                    let attrs: Vec<String> = parts[1]
                        .split(", ")
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    (parts[0].to_string(), attrs)
                } else {
                    continue;
                }
            }
            None => continue,
        };

        // Skip when we don't know which attributes are overridden.
        // This happens when managed_attrs detected the helper spread but
        // couldn't correlate it with specific data-* attributes (e.g.,
        // useOUIAProps produces attributes at runtime, not as JSX literals).
        if overridden_attrs.is_empty() {
            continue;
        }

        // Generate one rule per overridden attribute
        for attr in &overridden_attrs {
            let rule_id = format!(
                "{}-prop-override-{}-{}-{}",
                prefix,
                sanitize(&change.component),
                sanitize(&prop_name),
                sanitize(attr),
            );

            let message = format!(
                "The <{component}> component internally generates the `{attr}` HTML \
                 attribute from the `{prop}` prop via its internal helper. If you pass \
                 `{attr}` as an HTML attribute, it will be silently overridden.\n\n\
                 Use the `{prop}` prop instead:\n\n\
                 Before: <{component} {attr}=\"value\" />\n\
                 After:  <{component} {prop}=\"value\" />",
                component = change.component,
                attr = attr,
                prop = prop_name,
            );

            rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".into(),
                    "change-type=prop-attribute-override".into(),
                    "has-codemod=false".into(),
                    format!("package={}", pkg),
                ],
                effort: 3,
                category: "mandatory".into(),
                description: format!(
                    "{} manages `{}` internally via the `{}` prop",
                    change.component, attr, prop_name,
                ),
                message,
                links: vec![],
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: format!("^{}$", regex_escape(attr)),
                        location: "JSX_PROP".into(),
                        component: Some(format!("^{}$", regex_escape(&change.component))),
                        parent: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
                        parent_from: None,
                        value: None,
                        from: if from_pkg != "unknown" {
                            Some(from_pkg.clone())
                        } else {
                            None
                        },
                        file_pattern: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry::new("LlmAssisted")),
            });
        }
    }

    rules
}

/// Extract the ARIA attribute name from a source-level change description.
///
/// Handles these description formats from `diff_aria_attributes`:
/// - `"{attr} attribute removed from <{elem}> in {component}"`
/// - `"{attr} attribute added to <{elem}> in {component}"`
/// - `"{attr} on <{elem}> in {component} changed from ..."`
///
/// Returns `None` if the first token is not an ARIA attribute (`aria-*`) or `role`.
fn extract_aria_attr_name(description: &str) -> Option<String> {
    let attr = description.split_whitespace().next()?;
    if attr.starts_with("aria-") || attr == "role" {
        Some(attr.to_string())
    } else {
        None
    }
}

/// Escape special regex characters in a string.
fn regex_escape(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '\\' | '^' | '$' | '|' => {
                result.push('\\');
                result.push(c);
            }
            _ => result.push(c),
        }
    }
    result
}

// ── Deprecated prop rules ───────────────────────────────────────────────

/// Generate targeted rules for props marked `@deprecated` in JSDoc.
///
/// Each rule uses `JSX_PROP` location with the prop name as pattern, so it
/// only fires when consumer code actually passes the deprecated prop to the
/// component. This avoids false positives on files that import but don't use
/// the deprecated prop.
fn generate_deprecated_prop_rules(
    changes: &[SourceLevelChange],
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    for change in changes {
        if change.category != SourceLevelCategory::PropDeprecated {
            continue;
        }

        let prop_name = match &change.old_value {
            Some(name) => name,
            None => continue,
        };
        let deprecation_msg = change.new_value.as_deref().unwrap_or("Deprecated");

        let pkg = pkg_for(&change.component, component_packages);

        let prefix = rule_prefix(&change.migration_from);
        let rule_id = format!(
            "{}-deprecated-prop-{}-{}",
            prefix,
            sanitize(&change.component),
            sanitize(prop_name),
        );

        let message = format!(
            "Prop '{}' on {} is deprecated. {}\n\n\
             Replace or remove this prop.",
            prop_name, change.component, deprecation_msg,
        );

        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".into(),
                "change-type=prop-deprecated".into(),
                format!("package={}", pkg),
            ],
            effort: 1,
            category: "optional".into(),
            description: format!(
                "Deprecated prop '{}' on {}",
                prop_name, change.component,
            ),
            message,
            links: vec![],
            when: KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: format!("^{}$", prop_name),
                    location: "JSX_PROP".into(),
                    component: Some(format!("^{}$", change.component)),
                    parent: None,
                    not_parent: None,
                    child: None,
                    not_child: None,
                    requires_child: None,
                    parent_from: None,
                    value: None,
                    from: Some(pkg.clone()),
                    file_pattern: None,
                },
            },
            fix_strategy: None,
        });
    }

    rules
}

const CSS_FILE_PATTERN: &str = ".*\\.css$";

fn generate_css_class_removal_rules(removed_blocks: &[String]) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    for block in removed_blocks {
        // Match both v5 and v6 prefixed versions of the class, plus any
        // BEM element or modifier suffixes.
        // e.g., block "select" → matches:
        //   .pf-v5-c-select, .pf-v6-c-select
        //   .pf-v5-c-select__menu, .pf-v6-c-select__menu
        //   .pf-v5-c-select.pf-m-scrollable
        let pattern = format!("pf-(v5|v6)-c-{}", block);

        let rule_id = format!("sd-css-removed-{}", block);

        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".into(),
                "change-type=css-removal".into(),
                "impact=visual-regression".into(),
            ],
            effort: 3,
            category: "mandatory".into(),
            description: format!("CSS component class 'pf-c-{}' was removed in PF v6", block),
            message: format!(
                "This CSS references the 'pf-c-{}' component class which was removed \
                 in PatternFly v6.\n\n\
                 The {} component was rebuilt and no longer uses this CSS class. \
                 This CSS override is dead and should be removed.\n\n\
                 Check if the behavior you were overriding is now available via a \
                 component prop instead.",
                block,
                block_to_component_name(block),
            ),
            links: vec![],
            when: KonveyorCondition::FrontendCssClass {
                cssclass: FrontendPatternFields {
                    pattern,
                    file_pattern: Some(CSS_FILE_PATTERN.into()),
                },
            },
            fix_strategy: None,
        });
    }

    rules
}

/// Generate rules for removed CSS entry point files.
///
/// When top-level SCSS files (e.g., `patternfly-charts-theme-dark.scss`) are
/// removed between dep-repo versions, consumer projects that import them will
/// get build errors. This generates `builtin.filecontent` rules that match
/// import statements referencing the removed file.
fn generate_removed_css_file_rules(removed_files: &[String]) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    for scss_file in removed_files {
        // Strip .scss extension to get the CSS name consumers import
        let css_name = scss_file
            .strip_suffix(".scss")
            .unwrap_or(scss_file);

        if css_name.is_empty() {
            continue;
        }

        let rule_id = format!("sd-css-file-removed-{}", sanitize(css_name));

        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".into(),
                "change-type=css-file-removed".into(),
                "impact=build-failure".into(),
            ],
            effort: 1,
            category: "mandatory".into(),
            description: format!(
                "CSS entry point '{}.css' was removed",
                css_name,
            ),
            message: format!(
                "The CSS file '{css_name}.css' (from @patternfly/patternfly) \
                 was removed. Importing this file will cause a build error.\n\n\
                 Remove the import statement.",
            ),
            links: vec![],
            when: KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: format!(
                        "@patternfly/patternfly/{}",
                        css_name,
                    ),
                    location: "IMPORT".into(),
                    component: None,
                    parent: None,
                    not_parent: None,
                    child: None,
                    not_child: None,
                    requires_child: None,
                    parent_from: None,
                    value: None,
                    from: None,
                    file_pattern: None,
                },
            },
            fix_strategy: None,
        });
    }

    rules
}

/// Convert a kebab-case BEM block name to a likely PascalCase component name.
/// e.g., "select" → "Select", "app-launcher" → "AppLauncher"
fn block_to_component_name(block: &str) -> String {
    block
        .split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Generate rules for CSS classes where a version prefix swap produces a
/// class name that does not exist in the target CSS distribution.
///
/// These rules catch two scenarios:
/// 1. Consumer code still using the old class (e.g., `pf-v5-c-form__actions--right`)
/// 2. Consumer code where a blind prefix swap was already applied, producing
///    a dead class (e.g., `pf-v6-c-form__actions--right` doesn't exist in PFv6)
///
/// Both versions are matched by a single rule using regex alternation.
/// The fix strategy is `None` (manual), since there's no valid v6 replacement.
fn generate_dead_css_class_rules(dead_classes: &[(String, String)]) -> Vec<KonveyorRule> {
    use semver_analyzer_konveyor_core::sanitize_id;

    let mut rules = Vec::new();

    for (old_class, dead_v6_class) in dead_classes {
        // Build a regex that matches both the old and the dead-swapped version.
        // Escape regex metacharacters in the class names.
        let old_escaped = regex::escape(old_class);
        let dead_escaped = regex::escape(dead_v6_class);
        let pattern = format!("({}|{})", old_escaped, dead_escaped);

        let rule_id = format!("sd-css-dead-class-{}", sanitize_id(old_class));

        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".into(),
                "change-type=css-dead-class".into(),
                "impact=visual-regression".into(),
                "suppresses-prefix-swap=true".into(),
            ],
            effort: 3,
            category: "mandatory".into(),
            description: format!(
                "CSS class '{}' was removed — prefix swap to '{}' is invalid",
                old_class, dead_v6_class
            ),
            message: format!(
                "The CSS class '{}' was removed in the new version. \
                 A simple version prefix swap to '{}' does NOT produce a valid class — \
                 this class does not exist in the target CSS distribution.\n\n\
                 Remove this class reference or replace it with appropriate custom CSS \
                 or a PatternFly component prop.",
                old_class, dead_v6_class
            ),
            links: vec![],
            when: KonveyorCondition::FrontendCssClass {
                cssclass: FrontendPatternFields {
                    pattern,
                    // Scan all file types — these appear in JSX className strings too
                    file_pattern: None,
                },
            },
            // No automated fix — manual intervention required since the class
            // was removed, not just renamed.
            fix_strategy: None,
        });
    }

    if !rules.is_empty() {
        tracing::info!(
            count = rules.len(),
            "Generated dead CSS class rules (prefix swap produces non-existent class)"
        );
    }

    rules
}

/// Generate enumerated per-class CSS rules from the full class inventories.
///
/// Instead of a single catch-all rule that matches any `pf-v5-*` class and
/// blindly renames it to `pf-v6-*`, this generates individual rules for each
/// class in the old inventory:
///
/// - Classes with a valid v6 counterpart → `Rename` strategy (exact match)
/// - Classes with no v6 counterpart → `Manual` review (class was removed)
///
/// Third-party classes that use the `pf-v5-` prefix but aren't in the library's
/// CSS are not in the inventory, so no rule is generated for them — they are
/// left untouched.
pub fn generate_enumerated_css_class_rules(
    old_inventory: &HashSet<String>,
    new_inventory: &HashSet<String>,
) -> Vec<KonveyorRule> {
    use semver_analyzer_konveyor_core::sanitize_id;

    // Detect the version prefix from each inventory
    let old_prefix = detect_inventory_prefix(old_inventory);
    let new_prefix = detect_inventory_prefix(new_inventory);

    let (old_prefix, new_prefix) = match (old_prefix, new_prefix) {
        (Some(old), Some(new)) if old != new => (old, new),
        _ => {
            tracing::debug!(
                "Cannot detect version prefix change from inventories, \
                 skipping enumerated CSS class rule generation"
            );
            return Vec::new();
        }
    };

    let mut rename_rules = Vec::new();
    let mut removed_rules = Vec::new();

    // Sort for deterministic output
    let mut old_classes: Vec<&String> = old_inventory.iter().collect();
    old_classes.sort();

    for old_class in old_classes {
        // Only process classes with the version prefix
        if !old_class.starts_with(&old_prefix) {
            continue;
        }

        let base = &old_class[old_prefix.len()..];
        let new_class = format!("{}{}", new_prefix, base);

        if new_inventory.contains(&new_class) {
            // 1:1 rename — exact match rule with Rename strategy
            let rule_id = format!("semver-css-class-rename-{}", sanitize_id(base));
            rename_rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".into(),
                    "change-type=css-class".into(),
                    "has-codemod=true".into(),
                ],
                effort: 1,
                category: "mandatory".into(),
                description: format!("CSS class '{}' renamed to '{}'", old_class, new_class),
                message: format!(
                    "CSS class '{}' has been renamed to '{}'. \
                     Update all references in className props, CSS/SCSS files, \
                     and CSS-in-JS.",
                    old_class, new_class
                ),
                links: vec![],
                when: KonveyorCondition::FrontendCssClass {
                    cssclass: FrontendPatternFields {
                        pattern: old_class.clone(),
                        file_pattern: None,
                    },
                },
                fix_strategy: Some(FixStrategyEntry::with_from_to(
                    "CssVariablePrefix",
                    old_class,
                    &new_class,
                )),
            });
        } else {
            // No v6 counterpart — class was removed
            let rule_id = format!("semver-css-class-removed-{}", sanitize_id(base));
            removed_rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".into(),
                    "change-type=css-dead-class".into(),
                    "impact=visual-regression".into(),
                ],
                effort: 3,
                category: "mandatory".into(),
                description: format!("CSS class '{}' was removed in the new version", old_class),
                message: format!(
                    "The CSS class '{}' has no equivalent in the new version. \
                     There is no valid '{}' class — this class was removed, not renamed.\n\n\
                     Remove this class reference or replace it with appropriate custom CSS \
                     or a component prop.",
                    old_class, new_class
                ),
                links: vec![],
                when: KonveyorCondition::FrontendCssClass {
                    cssclass: FrontendPatternFields {
                        pattern: old_class.clone(),
                        file_pattern: None,
                    },
                },
                fix_strategy: None,
            });
        }
    }

    tracing::info!(
        rename_count = rename_rules.len(),
        removed_count = removed_rules.len(),
        "Generated enumerated CSS class rules"
    );

    // Verify expected utility sub-categories are represented.
    // If a known PF utility category produced zero rules (neither rename nor
    // removed), it likely means the dep-repo build didn't compile that
    // utility SCSS file.
    let expected_subcategories = [
        ("u-text-align-", "Text alignment"),
        ("u-text-transform-", "Text transform"),
        ("u-text-wrap", "Text wrap"),
        ("u-text-nowrap", "Text nowrap"),
        ("u-text-break-word", "Text break-word"),
        ("u-display-", "Display"),
        ("u-flex-", "Flex"),
        ("u-float-", "Float"),
        ("u-w-", "Width sizing"),
        ("u-h-", "Height sizing"),
        ("u-m-", "Margin spacing"),
        ("u-p-", "Padding spacing"),
    ];

    for (subcat, label) in &expected_subcategories {
        let full_prefix = format!("{}{}", old_prefix, subcat);
        let has_any = old_inventory.iter().any(|c| c.starts_with(&full_prefix));
        if !has_any {
            tracing::warn!(
                prefix = %full_prefix,
                category = %label,
                "Expected utility sub-category has zero classes in old inventory — \
                 no rename or removal rules will be generated for these classes"
            );
        }
    }

    let mut all = rename_rules;
    all.extend(removed_rules);
    all
}

/// Detect the most common version prefix from a set of CSS class names.
/// Looks for patterns like `pf-v5-` or `pf-v6-`.
fn detect_inventory_prefix(classes: &HashSet<String>) -> Option<String> {
    static VER_PREFIX_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^(pf-v\d+-)").unwrap());

    let mut counts: HashMap<String, usize> = HashMap::new();
    for cls in classes {
        if let Some(caps) = VER_PREFIX_RE.captures(cls) {
            *counts.entry(caps[1].to_string()).or_default() += 1;
        }
    }

    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(prefix, _)| prefix)
}

// ── Helper functions ────────────────────────────────────────────────────

/// Extract component name from a dotted symbol like "ModalProps.title".
fn extract_component_name_from_symbol(symbol: &str) -> Option<String> {
    let parts: Vec<&str> = symbol.split('.').collect();
    if parts.len() >= 2 {
        let iface = parts[0];
        // Strip "Props" suffix: "ModalProps" → "Modal"
        Some(iface.strip_suffix("Props").unwrap_or(iface).to_string())
    } else {
        None
    }
}

/// Extract prop name from a dotted symbol like "ModalProps.title".
fn extract_prop_name_from_symbol(symbol: &str) -> Option<String> {
    let parts: Vec<&str> = symbol.split('.').collect();
    if parts.len() >= 2 {
        Some(parts[1..].join("."))
    } else {
        None
    }
}

/// Check if a type string represents a ReactNode-ish type.
fn is_react_node_type(type_str: &str) -> bool {
    let t = type_str.trim();
    t.contains("ReactNode")
        || t.contains("ReactElement")
        || t.contains("JSX.Element")
        || t.contains("React.ReactNode")
        || t.contains("React.ReactElement")
}

/// Get the props for child components from TD report + SD profiles.
///
/// Uses two sources:
/// 1. TD structural changes — symbols like "ModalHeaderProps.title" tell us
///    ModalHeader has a `title` prop.
/// 2. SD profiles — `prop_defaults` keys are prop names on the component.
fn get_child_props_from_report(
    report: &AnalysisReport<TypeScript>,
    sd: &SdPipelineResult,
    new_children: &HashSet<&str>,
) -> HashMap<String, HashSet<String>> {
    let mut child_props: HashMap<String, HashSet<String>> = HashMap::new();

    // Initialize entries for all children
    for child in new_children {
        child_props.insert(child.to_string(), HashSet::new());
    }

    // Source 1: TD structural changes — prop symbols on child components
    for file_changes in &report.changes {
        for change in &file_changes.breaking_api_changes {
            if let Some(component) = extract_component_name_from_symbol(&change.symbol) {
                if new_children.contains(component.as_str()) {
                    if let Some(prop) = extract_prop_name_from_symbol(&change.symbol) {
                        child_props.entry(component).or_default().insert(prop);
                    }
                }
            }
        }
    }

    // Source 2: TD packages — component type summaries
    for pkg in &report.packages {
        for comp in &pkg.type_summaries {
            if new_children.contains(comp.name.as_str()) {
                // Type changes include added/modified members
                for tc in &comp.type_changes {
                    child_props
                        .entry(comp.name.clone())
                        .or_default()
                        .insert(tc.property.clone());
                }
            }
        }
    }

    // Source 3: SD profiles — prop_defaults keys are prop names
    for (name, profile) in &sd.new_profiles {
        if new_children.contains(name.as_str()) {
            for prop_name in profile.prop_defaults.keys() {
                child_props
                    .entry(name.clone())
                    .or_default()
                    .insert(prop_name.clone());
            }
        }
    }

    // Source 4: SD new_component_props — full prop list from AST extraction.
    // This is the most complete source and catches props like ModalHeader.title
    // that don't appear in TD breaking changes or prop defaults.
    for (name, props) in &sd.new_component_props {
        if new_children.contains(name.as_str()) {
            for prop_name in props {
                child_props
                    .entry(name.clone())
                    .or_default()
                    .insert(prop_name.clone());
            }
        }
    }

    child_props
}

/// Sanitize a string for use in rule IDs.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                '-'
            }
        })
        .collect()
}

/// Shorten a component name for use in conformance rule IDs by stripping the
/// family root prefix. For example, in the `DualListSelector` family,
/// `DualListSelectorControl` becomes `control`.
///
/// If `family` contains a modifier prefix (e.g., `deprecated/DualListSelector`),
/// only the base name (`DualListSelector`) is used for prefix matching.
///
/// Returns the full sanitized name when:
/// - The component name doesn't start with the family base name
/// - Stripping would produce an empty string (component == family root)
fn short_component_id(component: &str, family: &str) -> String {
    // Extract the base family name: "deprecated/DualListSelector" → "DualListSelector"
    let base_family = family.rsplit('/').next().unwrap_or(family);

    if component.len() > base_family.len() && component.starts_with(base_family) {
        sanitize(&component[base_family.len()..])
    } else {
        sanitize(component)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_component_name() {
        assert_eq!(
            extract_component_name_from_symbol("ModalProps.title"),
            Some("Modal".into())
        );
        assert_eq!(
            extract_component_name_from_symbol("ButtonProps.variant"),
            Some("Button".into())
        );
        assert_eq!(extract_component_name_from_symbol("Button"), None);
    }

    #[test]
    fn test_extract_prop_name() {
        assert_eq!(
            extract_prop_name_from_symbol("ModalProps.title"),
            Some("title".into())
        );
        assert_eq!(extract_prop_name_from_symbol("Button"), None);
    }

    #[test]
    fn test_is_react_node_type() {
        assert!(is_react_node_type("React.ReactNode"));
        assert!(is_react_node_type("ReactElement<any>"));
        assert!(is_react_node_type("JSX.Element"));
        assert!(!is_react_node_type("string"));
        assert!(!is_react_node_type("boolean"));
    }

    #[test]
    fn test_sanitize() {
        assert_eq!(sanitize("ModalHeader"), "modalheader");
        assert_eq!(sanitize("Dropdown.Item"), "dropdown-item");
    }

    #[test]
    fn test_short_component_id() {
        // Strips family prefix when component starts with it
        assert_eq!(
            short_component_id("DualListSelectorControl", "DualListSelector"),
            "control"
        );
        assert_eq!(
            short_component_id("DualListSelectorList", "DualListSelector"),
            "list"
        );
        assert_eq!(short_component_id("CardBody", "Card"), "body");
        assert_eq!(short_component_id("AlertGroup", "Alert"), "group");

        // Keeps full name when component == family root (stripping would be empty)
        assert_eq!(
            short_component_id("DualListSelector", "DualListSelector"),
            "duallistselector"
        );
        assert_eq!(short_component_id("Card", "Card"), "card");

        // Keeps full name when component doesn't start with family
        assert_eq!(short_component_id("Tr", "Table"), "tr");
        assert_eq!(short_component_id("Thead", "Table"), "thead");
        assert_eq!(short_component_id("Tab", "Tabs"), "tab");
        assert_eq!(short_component_id("ActionGroup", "Form"), "actiongroup");

        // Handles deprecated/ prefix — strips the base family name
        assert_eq!(
            short_component_id("DualListSelectorControl", "deprecated/DualListSelector"),
            "control"
        );
        assert_eq!(
            short_component_id("DualListSelector", "deprecated/DualListSelector"),
            "duallistselector"
        );
    }

    #[test]
    fn test_extract_bem_prop_name() {
        assert_eq!(
            extract_bem_prop_name(
                "EmptyStateHeader is BEM element 'titleText' of emptyState block"
            ),
            Some("titleText".into())
        );
        assert_eq!(
            extract_bem_prop_name("FooBar is BEM element 'icon' of foo block"),
            Some("icon".into())
        );
        assert_eq!(extract_bem_prop_name("no quotes here"), None);
    }

    fn test_pkg_map() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("Dropdown".into(), "@patternfly/react-core".into());
        m.insert("DropdownList".into(), "@patternfly/react-core".into());
        m.insert("DropdownItem".into(), "@patternfly/react-core".into());
        m.insert("AccordionContent".into(), "@patternfly/react-core".into());
        m.insert("AccordionItem".into(), "@patternfly/react-core".into());
        m
    }

    #[test]
    fn test_conformance_invalid_direct_child() {
        let tree = CompositionTree {
            root: "Dropdown".into(),
            family_members: vec![
                "Dropdown".into(),
                "DropdownList".into(),
                "DropdownItem".into(),
            ],
            edges: vec![
                crate::sd_types::CompositionEdge {
                    parent: "Dropdown".into(),
                    child: "DropdownList".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                crate::sd_types::CompositionEdge {
                    parent: "DropdownList".into(),
                    child: "DropdownItem".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &test_pkg_map());

        // Should have an InvalidDirectChild rule: DropdownItem in Dropdown
        let invalid_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("item-not-in-dropdown"));
        assert!(
            invalid_rule.is_some(),
            "Expected InvalidDirectChild rule for DropdownItem in Dropdown, got rules: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // The condition should use parent: ^Dropdown$
        if let KonveyorCondition::FrontendReferenced { referenced } = &invalid_rule.unwrap().when {
            assert_eq!(referenced.pattern, "^DropdownItem$");
            assert_eq!(referenced.parent.as_deref(), Some("^Dropdown$"));
        } else {
            panic!("Expected FrontendReferenced condition");
        }
    }

    /// Recursive nesting edges (e.g., Tab → Tabs for nested tabs) should
    /// use Allowed strength, not Required. When both directions are Required
    /// (a tree accuracy bug), the rule generator produces contradictory
    /// notParent rules for both directions. This test verifies the correct
    /// behavior when the back-edge is properly marked as Allowed.
    #[test]
    fn test_conformance_rules_skip_allowed_back_edges() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("Tabs".into(), "@patternfly/react-core".into());
        pkgs.insert("Tab".into(), "@patternfly/react-core".into());

        let tree = CompositionTree {
            root: "Tabs".into(),
            family_members: vec!["Tabs".into(), "Tab".into()],
            edges: vec![
                crate::sd_types::CompositionEdge {
                    parent: "Tabs".into(),
                    child: "Tab".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                // Recursive nesting: nested tabs inside a tab (Allowed, not Required)
                crate::sd_types::CompositionEdge {
                    parent: "Tab".into(),
                    child: "Tabs".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Allowed,
                    prop_name: None,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        // Tabs is no_incoming (root), so it should get a requiresChild rule
        assert!(
            rules.iter().any(|r| r.rule_id.contains("tabs-req-tab")),
            "Expected requiresChild rule for Tabs. Got rules: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // "Tabs must be in Tab" should NOT exist — the Allowed back-edge
        // doesn't trigger Required conformance, and Tabs is no_incoming
        assert!(
            !rules.iter().any(|r| r.rule_id == "sd-cf-tabs-tabs-in-tab"),
            "Back-edge 'tabs-in-tab' should not exist. Got rules: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
    }

    /// When a child has multiple valid direct parents (e.g., Tr can be in
    /// Thead OR Tbody), the generator should produce ONE merged rule with
    /// a combined notParent regex instead of separate per-parent rules that
    /// false-positive against each other.
    #[test]
    fn test_multi_parent_must_be_in_merged() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("Table".into(), "@patternfly/react-table".into());
        pkgs.insert("Thead".into(), "@patternfly/react-table".into());
        pkgs.insert("Tbody".into(), "@patternfly/react-table".into());
        pkgs.insert("Tr".into(), "@patternfly/react-table".into());
        pkgs.insert("Td".into(), "@patternfly/react-table".into());
        pkgs.insert("Th".into(), "@patternfly/react-table".into());

        let tree = CompositionTree {
            root: "Table".into(),
            family_members: vec![
                "Table".into(),
                "Thead".into(),
                "Tbody".into(),
                "Tr".into(),
                "Td".into(),
                "Th".into(),
            ],
            edges: vec![
                crate::sd_types::CompositionEdge {
                    parent: "Table".into(),
                    child: "Thead".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                crate::sd_types::CompositionEdge {
                    parent: "Table".into(),
                    child: "Tbody".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                crate::sd_types::CompositionEdge {
                    parent: "Thead".into(),
                    child: "Tr".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                crate::sd_types::CompositionEdge {
                    parent: "Tbody".into(),
                    child: "Tr".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                crate::sd_types::CompositionEdge {
                    parent: "Tr".into(),
                    child: "Td".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                crate::sd_types::CompositionEdge {
                    parent: "Tr".into(),
                    child: "Th".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        // Tr should have ONE in- rule with combined notParent
        let tr_must_be_in: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("tr-in-"))
            .collect();
        assert_eq!(
            tr_must_be_in.len(),
            1,
            "Expected exactly 1 merged in- rule for Tr, got {}: {:?}",
            tr_must_be_in.len(),
            tr_must_be_in.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        let rule = tr_must_be_in[0];
        // Rule ID should contain both parents
        assert!(
            rule.rule_id.contains("tbody") && rule.rule_id.contains("thead"),
            "Rule ID should mention both parents: {}",
            rule.rule_id
        );

        // notParent should be a combined regex
        if let KonveyorCondition::FrontendReferenced { referenced } = &rule.when {
            let not_parent = referenced.not_parent.as_deref().unwrap();
            assert!(
                not_parent.contains("Thead") && not_parent.contains("Tbody"),
                "notParent should combine both parents: {}",
                not_parent
            );
            assert!(
                not_parent.contains('|'),
                "notParent should use alternation: {}",
                not_parent
            );
        } else {
            panic!("Expected FrontendReferenced condition");
        }

        // Description should mention both parents
        assert!(
            rule.description.contains("Tbody") && rule.description.contains("Thead"),
            "Description should mention both parents: {}",
            rule.description
        );

        // There should be NO separate tr-in-thead or tr-in-tbody
        assert!(
            !rules.iter().any(|r| r.rule_id == "sd-cf-table-tr-in-thead"),
            "Should not have separate tr-in-thead rule"
        );
        assert!(
            !rules.iter().any(|r| r.rule_id == "sd-cf-table-tr-in-tbody"),
            "Should not have separate tr-in-tbody rule"
        );
    }

    /// InvalidDirectChild rules should also be merged when a child has
    /// multiple valid parents under the same grandparent.
    #[test]
    fn test_multi_parent_invalid_direct_child_merged() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("Table".into(), "@patternfly/react-table".into());
        pkgs.insert("Thead".into(), "@patternfly/react-table".into());
        pkgs.insert("Tbody".into(), "@patternfly/react-table".into());
        pkgs.insert("Tr".into(), "@patternfly/react-table".into());

        let tree = CompositionTree {
            root: "Table".into(),
            family_members: vec!["Table".into(), "Thead".into(), "Tbody".into(), "Tr".into()],
            edges: vec![
                crate::sd_types::CompositionEdge {
                    parent: "Table".into(),
                    child: "Thead".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                crate::sd_types::CompositionEdge {
                    parent: "Table".into(),
                    child: "Tbody".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                crate::sd_types::CompositionEdge {
                    parent: "Thead".into(),
                    child: "Tr".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                crate::sd_types::CompositionEdge {
                    parent: "Tbody".into(),
                    child: "Tr".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        // Tr-not-in-Table should be ONE merged rule mentioning both Thead and Tbody
        let tr_not_in_table: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("tr-not-in-table"))
            .collect();
        assert_eq!(
            tr_not_in_table.len(),
            1,
            "Expected 1 merged not-in-table rule for Tr, got {}: {:?}",
            tr_not_in_table.len(),
            tr_not_in_table
                .iter()
                .map(|r| &r.rule_id)
                .collect::<Vec<_>>()
        );

        // Description should mention both valid parents
        let rule = tr_not_in_table[0];
        assert!(
            rule.description.contains("Tbody") && rule.description.contains("Thead"),
            "Description should mention both valid parents: {}",
            rule.description
        );
    }

    /// Root components (no incoming edges) get requiresChild rules, not
    /// notParent rules. Children of non-root parents get notParent rules.
    #[test]
    fn test_root_gets_requires_child_children_get_not_parent() {
        // Four-strength model test:
        // Dropdown→DropdownList: Wrapper (parent renders child internally)
        //   → generates requiresChild on Dropdown
        //   → does NOT generate notParent on DropdownList (CHP=NO for Wrapper)
        // DropdownList→DropdownItem: Required (DOM nesting <ul>→<li>)
        //   → generates both requiresChild on DropdownList AND notParent on DropdownItem
        let tree = CompositionTree {
            root: "Dropdown".into(),
            family_members: vec![
                "Dropdown".into(),
                "DropdownList".into(),
                "DropdownItem".into(),
            ],
            edges: vec![
                crate::sd_types::CompositionEdge {
                    parent: "Dropdown".into(),
                    child: "DropdownList".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Wrapper,
                    prop_name: None,
                },
                crate::sd_types::CompositionEdge {
                    parent: "DropdownList".into(),
                    child: "DropdownItem".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &test_pkg_map());

        // Dropdown→DropdownList is Wrapper (PMC=YES) → requiresChild rule on Dropdown
        let dropdown_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("dropdown-req-list"));
        assert!(
            dropdown_rule.is_some(),
            "Expected requiresChild rule for Dropdown. Got rules: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
        if let KonveyorCondition::FrontendReferenced { referenced } = &dropdown_rule.unwrap().when {
            assert_eq!(referenced.pattern, "^Dropdown$");
            assert!(referenced.requires_child.is_some());
            assert!(referenced.not_parent.is_none());
        } else {
            panic!("Expected FrontendReferenced condition");
        }

        // DropdownItem has Required incoming edge (CHP=YES) → notParent rule
        let di_rule = rules.iter().find(|r| r.rule_id.contains("item-in-list"));
        assert!(
            di_rule.is_some(),
            "Expected notParent rule for DropdownItem. Got rules: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
        if let KonveyorCondition::FrontendReferenced { referenced } = &di_rule.unwrap().when {
            assert_eq!(referenced.pattern, "^DropdownItem$");
            assert!(referenced.not_parent.is_some());
        } else {
            panic!("Expected FrontendReferenced condition");
        }

        // NO notParent for DropdownList — its parent Dropdown has Wrapper (CHP=NO)
        assert!(
            !rules.iter().any(|r| r.rule_id.contains("list-in")),
            "DropdownList should not have a notParent rule (Wrapper edge has CHP=NO)"
        );
    }

    #[test]
    fn test_context_rule_generation() {
        let changes = vec![SourceLevelChange {
            component: "AccordionItem".into(),
            category: SourceLevelCategory::ContextDependency,
            description: "AccordionItem now provides AccordionItemContext".into(),
            old_value: None,
            new_value: Some("<AccordionItemContext.Provider>".into()),
            has_test_implications: true,
            test_description: None,
            element: None,
            migration_from: None,
            dependency_chain: None,
        }];

        let rules = generate_context_rules(&changes, &test_pkg_map());

        assert_eq!(rules.len(), 1);
        assert!(rules[0].rule_id.contains("accordionitemcontext"));

        if let KonveyorCondition::FrontendReferenced { referenced } = &rules[0].when {
            assert_eq!(referenced.pattern, "^AccordionItemContext$");
            assert_eq!(referenced.location, "IMPORT");
        } else {
            panic!("Expected FrontendReferenced condition");
        }
    }

    // ── Migration-aware rule generation tests ───────────────────────

    #[test]
    fn test_migration_test_impact_rules_have_distinct_ids() {
        // Simulate: deprecated SelectOption had role='presentation' on 3 elements,
        // new SelectOption has none. This produces 3 migration-tagged changes.
        // They should all produce rules with "sd-migration-" prefix and
        // NOT collide with each other or with evolution rules.
        // The `element` field disambiguates rule IDs for same-role different-element.
        let changes = vec![
            SourceLevelChange {
                component: "SelectOption".into(),
                category: SourceLevelCategory::RoleChange,
                description: "role='presentation' removed from <button> in SelectOption".into(),
                old_value: Some("presentation".into()),
                new_value: None,
                has_test_implications: true,
                test_description: None,
                element: Some("button".into()),
                migration_from: Some(
                    "packages/react-core/src/deprecated/components/Select/SelectOption.tsx".into(),
                ),
                dependency_chain: None,
            },
            SourceLevelChange {
                component: "SelectOption".into(),
                category: SourceLevelCategory::RoleChange,
                description: "role='presentation' removed from <div> in SelectOption".into(),
                old_value: Some("presentation".into()),
                new_value: None,
                has_test_implications: true,
                test_description: None,
                element: Some("div".into()),
                migration_from: Some(
                    "packages/react-core/src/deprecated/components/Select/SelectOption.tsx".into(),
                ),
                dependency_chain: None,
            },
            // Also add a non-migration change for the same component
            SourceLevelChange {
                component: "SelectOption".into(),
                category: SourceLevelCategory::RoleChange,
                description: "role='option' removed from <li> in SelectOption".into(),
                old_value: Some("option".into()),
                new_value: None,
                has_test_implications: true,
                test_description: None,
                element: Some("li".into()),
                migration_from: None, // evolution change
                dependency_chain: None,
            },
        ];

        let mut pkgs = test_pkg_map();
        pkgs.insert("SelectOption".into(), "@patternfly/react-core".into());

        let rules = generate_test_impact_rules(&changes, &pkgs);

        // Migration rules should have "sd-migration-" prefix
        let migration_rules: Vec<_> = rules
            .iter()
            .filter(|r| r.rule_id.starts_with("sd-migration-"))
            .collect();
        assert!(
            !migration_rules.is_empty(),
            "Should produce migration-prefixed rules"
        );

        // Evolution rules should have "sd-" prefix (not "sd-migration-")
        let evolution_rules: Vec<_> = rules
            .iter()
            .filter(|r| r.rule_id.starts_with("sd-test-"))
            .collect();
        assert!(
            !evolution_rules.is_empty(),
            "Should produce evolution-prefixed rules"
        );

        // All rule IDs should be unique
        let mut seen = std::collections::HashSet::new();
        for r in &rules {
            assert!(seen.insert(&r.rule_id), "Duplicate rule ID: {}", r.rule_id);
        }
    }

    #[test]
    fn test_attr_conditionality_rule_generation() {
        let changes = vec![SourceLevelChange {
            component: "Button".into(),
            category: SourceLevelCategory::AttributeConditionality,
            description:
                "aria-disabled on <button> in Button changed from always-present to conditional"
                    .into(),
            old_value: Some("always-present (value: {isDisabled})".into()),
            new_value: Some("conditional".into()),
            has_test_implications: true,
            test_description: Some("getAttribute('aria-disabled') may now return null".into()),
            element: Some("button".into()),
            migration_from: None,
            dependency_chain: None,
        }];

        let mut pkgs = test_pkg_map();
        pkgs.insert("Button".into(), "@patternfly/react-core".into());

        let rules = generate_test_impact_rules(&changes, &pkgs);
        assert_eq!(rules.len(), 1, "Should produce exactly one rule");

        let rule = &rules[0];
        assert!(
            rule.rule_id.contains("attr-conditionality"),
            "Rule ID should contain 'attr-conditionality': {}",
            rule.rule_id
        );
        assert!(
            rule.rule_id.contains("aria-disabled"),
            "Rule ID should contain attribute name: {}",
            rule.rule_id
        );
        assert!(
            rule.message.contains("getAttribute"),
            "Message should mention getAttribute"
        );
        assert!(
            rule.message.contains(".toBeNull()"),
            "Message should suggest .toBeNull()"
        );
        assert!(
            rule.message.contains(".not.toHaveAttribute"),
            "Message should suggest .not.toHaveAttribute"
        );
        assert!(rule
            .labels
            .contains(&"change-type=attribute-conditionality".to_string()));
        assert!(rule.labels.contains(&"impact=frontend-testing".to_string()));

        if let KonveyorCondition::FileContent { filecontent } = &rule.when {
            assert!(
                filecontent.pattern.contains("aria-disabled"),
                "Pattern should match aria-disabled: {}",
                filecontent.pattern
            );
            assert!(
                filecontent.file_pattern.contains("test|spec"),
                "Should scope to test files"
            );
        } else {
            panic!("Expected FileContent condition, got {:?}", rule.when);
        }

        assert!(
            rule.fix_strategy.is_some(),
            "Should have a fix strategy (Manual)"
        );
    }

    #[test]
    fn test_attr_conditionality_no_rule_without_test_implications() {
        // Changes without has_test_implications should not produce rules
        let changes = vec![SourceLevelChange {
            component: "Button".into(),
            category: SourceLevelCategory::AttributeConditionality,
            description:
                "aria-disabled on <button> in Button changed from always-present to conditional"
                    .into(),
            old_value: Some("always-present (value: {isDisabled})".into()),
            new_value: Some("conditional".into()),
            has_test_implications: false, // no test implications
            test_description: None,
            element: Some("button".into()),
            migration_from: None,
            dependency_chain: None,
        }];

        let mut pkgs = test_pkg_map();
        pkgs.insert("Button".into(), "@patternfly/react-core".into());

        let rules = generate_test_impact_rules(&changes, &pkgs);
        assert!(
            rules.is_empty(),
            "Should not produce rules when has_test_implications is false"
        );
    }

    #[test]
    fn test_migration_context_rules_use_deprecated_from_path() {
        let changes = vec![SourceLevelChange {
            component: "Select".into(),
            category: SourceLevelCategory::ContextDependency,
            description: "Select no longer uses useContext(SelectContext)".into(),
            old_value: Some("useContext(SelectContext)".into()),
            new_value: None,
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: Some(
                "packages/react-core/src/deprecated/components/Select/Select.tsx".into(),
            ),
            dependency_chain: None,
        }];

        let mut pkgs = test_pkg_map();
        pkgs.insert("Select".into(), "@patternfly/react-core".into());

        let rules = generate_context_rules(&changes, &pkgs);

        assert_eq!(rules.len(), 1);

        // Rule ID should have migration prefix
        assert!(
            rules[0].rule_id.starts_with("sd-migration-context-"),
            "Expected migration prefix, got: {}",
            rules[0].rule_id
        );

        // from should be the deprecated package path
        if let KonveyorCondition::FrontendReferenced { referenced } = &rules[0].when {
            assert_eq!(
                referenced.from.as_deref(),
                Some("@patternfly/react-core/deprecated"),
                "Migration context rule should match deprecated import path"
            );
        } else {
            panic!("Expected FrontendReferenced condition");
        }
    }

    #[test]
    fn test_migration_prop_override_rules_use_deprecated_from_path() {
        let changes = vec![SourceLevelChange {
            component: "Dropdown".into(),
            category: SourceLevelCategory::PropAttributeOverride,
            description: "Dropdown's `ouiaId` prop overrides HTML attributes".into(),
            old_value: None,
            new_value: Some("ouiaId → data-ouia-component-id".into()),
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: Some(
                "packages/react-core/src/deprecated/components/Dropdown/Dropdown.tsx".into(),
            ),
            dependency_chain: None,
        }];

        let pkgs = test_pkg_map();

        let rules =
            generate_prop_attribute_override_rules(&changes, &SdPipelineResult::default(), &pkgs);

        assert_eq!(rules.len(), 1);

        // Rule ID should have migration prefix
        assert!(
            rules[0].rule_id.starts_with("sd-migration-prop-override-"),
            "Expected migration prefix, got: {}",
            rules[0].rule_id
        );

        // from should be the deprecated package path
        if let KonveyorCondition::FrontendReferenced { referenced } = &rules[0].when {
            assert_eq!(
                referenced.from.as_deref(),
                Some("@patternfly/react-core/deprecated"),
                "Migration prop-override rule should match deprecated import path"
            );
        } else {
            panic!("Expected FrontendReferenced condition");
        }
    }

    #[test]
    fn test_evolution_rules_unchanged_by_migration_support() {
        // Verify that non-migration changes still produce "sd-" prefixed rules
        // with the normal package in `from`.
        let changes = vec![SourceLevelChange {
            component: "Dropdown".into(),
            category: SourceLevelCategory::PropAttributeOverride,
            description: "Dropdown's `ouiaId` prop overrides HTML attributes".into(),
            old_value: None,
            new_value: Some("ouiaId → data-ouia-component-id".into()),
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None, // evolution, not migration
            dependency_chain: None,
        }];

        let rules = generate_prop_attribute_override_rules(
            &changes,
            &SdPipelineResult::default(),
            &test_pkg_map(),
        );

        assert_eq!(rules.len(), 1);

        // Rule ID should NOT have migration prefix
        assert!(
            rules[0].rule_id.starts_with("sd-prop-override-"),
            "Expected sd- prefix, got: {}",
            rules[0].rule_id
        );

        // from should be the normal package
        if let KonveyorCondition::FrontendReferenced { referenced } = &rules[0].when {
            assert_eq!(
                referenced.from.as_deref(),
                Some("@patternfly/react-core"),
                "Evolution prop-override rule should match normal import path"
            );
        } else {
            panic!("Expected FrontendReferenced condition");
        }
    }

    #[test]
    fn test_deprecated_pkg_from_migration_path() {
        assert_eq!(
            deprecated_pkg_from_migration_path(
                "packages/react-core/src/deprecated/components/Select/Select.tsx"
            ),
            "@patternfly/react-core/deprecated"
        );
        assert_eq!(
            deprecated_pkg_from_migration_path(
                "packages/react-table/src/deprecated/components/Table/Table.tsx"
            ),
            "@patternfly/react-table/deprecated"
        );
        // Fallback for unexpected format
        assert_eq!(
            deprecated_pkg_from_migration_path("some/random/path.tsx"),
            "@patternfly/react-core/deprecated"
        );
    }

    /// When a child has one Required parent and one Allowed parent, the
    /// notParent regex should include BOTH parents so that placement inside
    /// the Allowed parent doesn't trigger a false positive.
    #[test]
    fn test_allowed_parent_included_in_not_parent_regex() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("Table".into(), "@patternfly/react-table".into());
        pkgs.insert("Thead".into(), "@patternfly/react-table".into());
        pkgs.insert("Tbody".into(), "@patternfly/react-table".into());
        pkgs.insert("Tr".into(), "@patternfly/react-table".into());

        let tree = CompositionTree {
            root: "Table".into(),
            family_members: vec!["Table".into(), "Thead".into(), "Tbody".into(), "Tr".into()],
            edges: vec![
                crate::sd_types::CompositionEdge {
                    parent: "Table".into(),
                    child: "Thead".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                crate::sd_types::CompositionEdge {
                    parent: "Table".into(),
                    child: "Tbody".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                // Tbody→Tr is Required (e.g., CSS direct-child selector)
                crate::sd_types::CompositionEdge {
                    parent: "Tbody".into(),
                    child: "Tr".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                // Thead→Tr is Allowed (e.g., CSS descendant selector)
                crate::sd_types::CompositionEdge {
                    parent: "Thead".into(),
                    child: "Tr".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Allowed,
                    prop_name: None,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        // A conformance rule SHOULD be generated (because Tbody→Tr is Required)
        let tr_must_be_in = rules.iter().find(|r| r.rule_id.contains("tr-in-"));
        assert!(tr_must_be_in.is_some(), "Expected an in- rule for Tr");

        let rule = tr_must_be_in.unwrap();

        // The rule ID should include both parents
        assert!(
            rule.rule_id.contains("tbody") && rule.rule_id.contains("thead"),
            "Rule ID should include both parents: {}",
            rule.rule_id
        );

        // The notParent regex should include both parents
        if let KonveyorCondition::FrontendReferenced { referenced } = &rule.when {
            let not_parent = referenced.not_parent.as_deref().unwrap();
            assert!(
                not_parent.contains("Tbody") && not_parent.contains("Thead"),
                "notParent regex should include both Required and Allowed parents: {}",
                not_parent
            );
        } else {
            panic!("Expected FrontendReferenced condition");
        }

        // Description should mention both parents
        assert!(
            rule.description.contains("Tbody") && rule.description.contains("Thead"),
            "Description should mention both parents: {}",
            rule.description
        );

        // InvalidDirectChild rule should mention only CHP parents.
        // Thead→Tr is Allowed (not CHP), so the grandparent walk skips it.
        // Only Tbody (Required/CHP) appears as the suggested intermediate.
        let tr_not_in_table = rules.iter().find(|r| r.rule_id.contains("tr-not-in-table"));
        if let Some(idc_rule) = tr_not_in_table {
            assert!(
                idc_rule.description.contains("Tbody"),
                "InvalidDirectChild should mention CHP parent Tbody: {}",
                idc_rule.description
            );
            // Thead is NOT mentioned — it's only an Allowed parent,
            // excluded from the first-hop grandparent walk.
            assert!(
                !idc_rule.description.contains("Thead"),
                "InvalidDirectChild should NOT mention Allowed parent Thead: {}",
                idc_rule.description
            );
        }
    }

    /// When a child has ONLY Allowed parents (no Required edges), no
    /// conformance rule should be generated.
    #[test]
    fn test_only_allowed_parents_no_rule_generated() {
        let tree = CompositionTree {
            root: "Menu".into(),
            family_members: vec!["Menu".into(), "MenuContent".into()],
            edges: vec![crate::sd_types::CompositionEdge {
                parent: "Menu".into(),
                child: "MenuContent".into(),
                relationship: ChildRelationship::DirectChild,
                required: false,
                bem_evidence: None,
                strength: crate::sd_types::EdgeStrength::Allowed,
                prop_name: None,
            }],
        };

        let rules = generate_conformance_rules(&[tree], &[], &test_pkg_map());

        // No in- rule should be generated for MenuContent
        let mc_rule = rules.iter().find(|r| r.rule_id.contains("content-in"));
        assert!(
            mc_rule.is_none(),
            "No conformance rule should be generated when child only has Allowed parents"
        );
    }

    /// Secondary roots (no incoming Required edges, not the tree root) should
    /// get requiresChild rules for their Required children. The tree root
    /// itself is also no_incoming and gets requiresChild.
    #[test]
    fn test_secondary_root_gets_requires_child() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("Alert".into(), "@patternfly/react-core".into());
        pkgs.insert("AlertGroup".into(), "@patternfly/react-core".into());
        pkgs.insert(
            "AlertActionCloseButton".into(),
            "@patternfly/react-core".into(),
        );

        // Four-strength model: AlertGroup wraps Alert/AlertActionCloseButton
        // via Wrapper edges (parent requires child, child can exist standalone).
        let tree = CompositionTree {
            root: "Alert".into(),
            family_members: vec![
                "Alert".into(),
                "AlertGroup".into(),
                "AlertActionCloseButton".into(),
            ],
            edges: vec![
                // AlertGroup is a secondary root — no incoming edges
                // Wrapper: AlertGroup must contain Alert, Alert can exist standalone
                crate::sd_types::CompositionEdge {
                    parent: "AlertGroup".into(),
                    child: "Alert".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Wrapper,
                    prop_name: None,
                },
                crate::sd_types::CompositionEdge {
                    parent: "AlertGroup".into(),
                    child: "AlertActionCloseButton".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Wrapper,
                    prop_name: None,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        // AlertGroup should get a requiresChild rule (Wrapper = PMC=YES)
        let ag_rule = rules.iter().find(|r| r.rule_id.contains("group-req-"));
        assert!(
            ag_rule.is_some(),
            "Expected requiresChild rule for AlertGroup. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        if let KonveyorCondition::FrontendReferenced { referenced } = &ag_rule.unwrap().when {
            assert_eq!(referenced.pattern, "^AlertGroup$");
            assert!(
                referenced.requires_child.is_some(),
                "Should use requiresChild field"
            );
            let req = referenced.requires_child.as_deref().unwrap();
            assert!(req.contains("Alert"), "requiresChild should include Alert");
            assert!(
                req.contains("AlertActionCloseButton"),
                "requiresChild should include AlertActionCloseButton"
            );
        } else {
            panic!("Expected FrontendReferenced condition");
        }

        // Alert should NOT get a notParent rule — Wrapper edges have CHP=NO
        assert!(
            !rules.iter().any(|r| r.rule_id.contains("alert-in")),
            "Alert should NOT have a notParent rule (Wrapper has CHP=NO). Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
    }

    /// Family root should never get a notParent rule, even when edges have
    /// CHP=YES (Structural). The root is standalone by definition. This tests
    /// the rule-gen filter for the case where the composition builder produces
    /// Structural edges TO the root (e.g., cloneElement in AlertGroup→Alert).
    #[test]
    fn test_family_root_never_gets_not_parent_even_with_structural_edge() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("Alert".into(), "@patternfly/react-core".into());
        pkgs.insert("AlertGroup".into(), "@patternfly/react-core".into());

        let tree = CompositionTree {
            root: "Alert".into(),
            family_members: vec!["Alert".into(), "AlertGroup".into()],
            edges: vec![
                // Structural edge TO the root: CHP=YES in the edge, but the
                // root is standalone — the rule-gen filter must suppress this.
                crate::sd_types::CompositionEdge {
                    parent: "AlertGroup".into(),
                    child: "Alert".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Structural,
                    prop_name: None,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        // Alert (the family root) must NOT get a notParent rule, even though
        // the Structural edge has CHP=YES. The root is standalone.
        assert!(
            !rules.iter().any(|r| r.rule_id.contains("alert-in")),
            "Family root Alert should NOT have a notParent rule even with Structural edge. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // AlertGroup should NOT get a requiresChild rule either (Structural = PMC=NO).
        assert!(
            !rules.iter().any(|r| r.rule_id.contains("req-")),
            "AlertGroup should NOT get requiresChild (Structural = PMC=NO). Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
    }

    /// Same test for deprecated families: the deprecated/ prefix in tree.root
    /// should not prevent the root filter from matching edge.child.
    #[test]
    fn test_family_root_not_parent_filter_handles_deprecated_prefix() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("DualListSelector".into(), "@patternfly/react-core".into());
        pkgs.insert(
            "DualListSelectorPane".into(),
            "@patternfly/react-core".into(),
        );

        let tree = CompositionTree {
            root: "deprecated/DualListSelector".into(),
            family_members: vec!["DualListSelector".into(), "DualListSelectorPane".into()],
            edges: vec![crate::sd_types::CompositionEdge {
                parent: "DualListSelectorPane".into(),
                child: "DualListSelector".into(),
                relationship: ChildRelationship::DirectChild,
                required: false,
                bem_evidence: None,
                strength: crate::sd_types::EdgeStrength::Structural,
                prop_name: None,
            }],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        // DualListSelector is the root (deprecated/DualListSelector) —
        // must not get notParent rule.
        assert!(
            !rules.iter().any(|r| {
                r.rule_id.contains("duallistselector-in-") && !r.rule_id.contains("pane-in-")
            }),
            "Deprecated family root should NOT get notParent rule. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
    }

    /// Deprecated families should use the deprecated import path in their
    /// `from` field so they don't produce identical `when` clauses with v6
    /// rules for the same component names.
    #[test]
    fn test_deprecated_conformance_rules_use_deprecated_from_path() {
        let mut pkgs = test_pkg_map();
        // Both v6 and deprecated WizardNav resolve to @patternfly/react-core
        // in the component_packages map (name collision)
        pkgs.insert("WizardNav".into(), "@patternfly/react-core".into());
        pkgs.insert("WizardNavItem".into(), "@patternfly/react-core".into());

        let deprecated_tree = CompositionTree {
            root: "deprecated/Wizard".into(),
            family_members: vec!["WizardNav".into(), "WizardNavItem".into()],
            edges: vec![crate::sd_types::CompositionEdge {
                parent: "WizardNav".into(),
                child: "WizardNavItem".into(),
                relationship: ChildRelationship::DirectChild,
                required: true,
                bem_evidence: None,
                strength: crate::sd_types::EdgeStrength::Required,
                prop_name: None,
            }],
        };

        let rules = generate_conformance_rules(&[deprecated_tree], &[], &pkgs);

        // All rules should use @patternfly/react-core/deprecated, not @patternfly/react-core
        for rule in &rules {
            if let KonveyorCondition::FrontendReferenced { referenced } = &rule.when {
                let from = referenced.from.as_deref().unwrap_or("");
                assert!(
                    from.contains("/deprecated"),
                    "Rule {} should use deprecated from path, got: {}",
                    rule.rule_id,
                    from
                );
            }
        }

        // Verify at least one rule was generated
        assert!(
            !rules.is_empty(),
            "Expected at least one conformance rule for deprecated/Wizard"
        );
    }

    /// V6 families should NOT have /deprecated in their from path.
    #[test]
    fn test_v6_conformance_rules_use_normal_from_path() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("WizardNav".into(), "@patternfly/react-core".into());
        pkgs.insert("WizardNavItem".into(), "@patternfly/react-core".into());

        let v6_tree = CompositionTree {
            root: "Wizard".into(),
            family_members: vec!["WizardNav".into(), "WizardNavItem".into()],
            edges: vec![crate::sd_types::CompositionEdge {
                parent: "WizardNav".into(),
                child: "WizardNavItem".into(),
                relationship: ChildRelationship::DirectChild,
                required: true,
                bem_evidence: None,
                strength: crate::sd_types::EdgeStrength::Required,
                prop_name: None,
            }],
        };

        let rules = generate_conformance_rules(&[v6_tree], &[], &pkgs);

        // All rules should use @patternfly/react-core (no /deprecated)
        for rule in &rules {
            if let KonveyorCondition::FrontendReferenced { referenced } = &rule.when {
                let from = referenced.from.as_deref().unwrap_or("");
                assert!(
                    !from.contains("/deprecated"),
                    "v6 rule {} should NOT use deprecated from path, got: {}",
                    rule.rule_id,
                    from
                );
            }
        }
    }

    /// When the component_packages map already resolves to a deprecated path
    /// (e.g., Body → @patternfly/react-table/deprecated), don't double-append.
    #[test]
    fn test_deprecated_from_path_no_double_append() {
        let mut pkgs = test_pkg_map();
        // Body already resolves to the deprecated path in the map
        pkgs.insert("Body".into(), "@patternfly/react-table/deprecated".into());
        pkgs.insert("Header".into(), "@patternfly/react-table/deprecated".into());

        let tree = CompositionTree {
            root: "deprecated/Table".into(),
            family_members: vec!["Body".into(), "Header".into()],
            edges: vec![crate::sd_types::CompositionEdge {
                parent: "Header".into(),
                child: "Body".into(),
                relationship: ChildRelationship::DirectChild,
                required: false,
                bem_evidence: None,
                strength: crate::sd_types::EdgeStrength::Structural,
                prop_name: None,
            }],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        for rule in &rules {
            if let KonveyorCondition::FrontendReferenced { referenced } = &rule.when {
                let from = referenced.from.as_deref().unwrap_or("");
                assert!(
                    !from.contains("/deprecated/deprecated"),
                    "Rule {} has double /deprecated in from path: {}",
                    rule.rule_id,
                    from
                );
                assert!(
                    from.contains("/deprecated"),
                    "Rule {} should use deprecated from path: {}",
                    rule.rule_id,
                    from
                );
            }
        }
    }

    /// Table-like deep trees: root gets requiresChild, intermediate nodes
    /// get notParent, and invalidDirectChild rules fire for skip-level.
    #[test]
    fn test_deep_tree_requires_child_and_not_parent() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("Table".into(), "@patternfly/react-table".into());
        pkgs.insert("Tbody".into(), "@patternfly/react-table".into());
        pkgs.insert("Tr".into(), "@patternfly/react-table".into());
        pkgs.insert("Td".into(), "@patternfly/react-table".into());

        let tree = CompositionTree {
            root: "Table".into(),
            family_members: vec!["Table".into(), "Tbody".into(), "Tr".into(), "Td".into()],
            edges: vec![
                crate::sd_types::CompositionEdge {
                    parent: "Table".into(),
                    child: "Tbody".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                crate::sd_types::CompositionEdge {
                    parent: "Tbody".into(),
                    child: "Tr".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                crate::sd_types::CompositionEdge {
                    parent: "Tr".into(),
                    child: "Td".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        // Table (root, no_incoming) → requiresChild
        assert!(
            rules.iter().any(|r| r.rule_id.contains("table-req-tbody")),
            "Expected requiresChild on Table. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // Tr (has incoming from Tbody) → notParent
        assert!(
            rules.iter().any(|r| r.rule_id.contains("tr-in-tbody")),
            "Expected notParent on Tr. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // Td (has incoming from Tr) → notParent
        assert!(
            rules.iter().any(|r| r.rule_id.contains("td-in-tr")),
            "Expected notParent on Td. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // InvalidDirectChild: Tr in Table should use Tbody
        assert!(
            rules.iter().any(|r| r.rule_id.contains("tr-not-in-table")),
            "Expected InvalidDirectChild for Tr in Table. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // InvalidDirectChild: Td in Tbody should use Tr
        assert!(
            rules.iter().any(|r| r.rule_id.contains("td-not-in-tbody")),
            "Expected InvalidDirectChild for Td in Tbody. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // Tbody SHOULD have a notParent rule — Required edges have CHP=YES,
        // so Tbody must be inside Table regardless of Table being a root.
        assert!(
            rules.iter().any(|r| r.rule_id.contains("tbody-in")),
            "Tbody should have notParent (Required edge has CHP=YES). Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
    }

    /// When a child has a CHP edge (Required or Structural) directly to the
    /// grandparent, no invalidDirectChild rule should be generated for that
    /// grandparent because the child IS a valid direct child there. The
    /// notParent rule already lists the grandparent as a valid parent.
    ///
    /// Example: Card family has Card→CardBody (Structural) and
    /// Card→CardHeader (Structural), plus CardHeader→CardBody (Allowed from
    /// CSS layout). Without CHP suppression, the grandparent walk would
    /// generate "CardBody not-in Card, use CardHeader" — but CardBody IS a
    /// valid direct child of Card.
    #[test]
    fn test_invalid_direct_child_suppressed_when_child_has_chp_to_grandparent() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("Card".into(), "@patternfly/react-core".into());
        pkgs.insert("CardHeader".into(), "@patternfly/react-core".into());
        pkgs.insert("CardBody".into(), "@patternfly/react-core".into());
        pkgs.insert("CardFooter".into(), "@patternfly/react-core".into());

        let tree = CompositionTree {
            root: "Card".into(),
            family_members: vec![
                "Card".into(),
                "CardHeader".into(),
                "CardBody".into(),
                "CardFooter".into(),
            ],
            edges: vec![
                // Card → CardHeader: Structural (CHP=YES)
                crate::sd_types::CompositionEdge {
                    parent: "Card".into(),
                    child: "CardHeader".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Structural,
                    prop_name: None,
                },
                // Card → CardBody: Structural (CHP=YES)
                crate::sd_types::CompositionEdge {
                    parent: "Card".into(),
                    child: "CardBody".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Structural,
                    prop_name: None,
                },
                // Card → CardFooter: Structural (CHP=YES)
                crate::sd_types::CompositionEdge {
                    parent: "Card".into(),
                    child: "CardFooter".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Structural,
                    prop_name: None,
                },
                // CardHeader → CardBody: Allowed (CSS layout signal)
                crate::sd_types::CompositionEdge {
                    parent: "CardHeader".into(),
                    child: "CardBody".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Allowed,
                    prop_name: None,
                },
                // CardHeader → CardFooter: Allowed (CSS layout signal)
                crate::sd_types::CompositionEdge {
                    parent: "CardHeader".into(),
                    child: "CardFooter".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Allowed,
                    prop_name: None,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        // notParent rules should exist for CardBody, CardFooter, CardHeader
        // (they all have CHP edges to Card and/or CardHeader).
        assert!(
            rules.iter().any(|r| r.rule_id.contains("body-in-")),
            "Expected notParent for CardBody. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
        assert!(
            rules.iter().any(|r| r.rule_id.contains("footer-in-")),
            "Expected notParent for CardFooter. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // invalidDirectChild rules should NOT exist for CardBody/CardFooter
        // in Card, because they have direct Structural (CHP=YES) edges to
        // Card. The grandparent walk goes CardBody→CardHeader→Card, but
        // Card→CardBody is Structural, so it should be suppressed.
        let invalid_rules: Vec<&KonveyorRule> = rules
            .iter()
            .filter(|r| r.rule_id.contains("not-in-card"))
            .collect();
        assert!(
            invalid_rules.is_empty(),
            "CardBody/CardFooter should NOT get invalidDirectChild for Card \
             (they have CHP edges to Card). Got: {:?}",
            invalid_rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
    }

    /// The invalidDirectChild grandparent walk should only follow CHP=YES
    /// parents (Required or Structural) in the first hop. Allowed parents
    /// (CSS descendant matches between peer components) should NOT be walked
    /// because they create false intermediate paths.
    ///
    /// Example: DescriptionList has Group→Term [Allowed] and Group→Description
    /// [Structural]. Term→Description [Allowed] from CSS `.term .text`. Without
    /// CHP filtering, the walk goes Description→Term(Allowed)→TermHelpText
    /// (Allowed), generating "Description not-in TermHelpText, use Term" — but
    /// Term and Description are peers, not parent-child.
    #[test]
    fn test_invalid_direct_child_skips_allowed_first_hop() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("DL".into(), "@patternfly/react-core".into());
        pkgs.insert("DLGroup".into(), "@patternfly/react-core".into());
        pkgs.insert("DLTerm".into(), "@patternfly/react-core".into());
        pkgs.insert("DLTermHelp".into(), "@patternfly/react-core".into());
        pkgs.insert("DLDesc".into(), "@patternfly/react-core".into());

        let tree = CompositionTree {
            root: "DL".into(),
            family_members: vec![
                "DL".into(),
                "DLGroup".into(),
                "DLTerm".into(),
                "DLTermHelp".into(),
                "DLDesc".into(),
            ],
            edges: vec![
                // DL → DLGroup: Required
                crate::sd_types::CompositionEdge {
                    parent: "DL".into(),
                    child: "DLGroup".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                // DLGroup → DLDesc: Structural (CHP=YES — real parent)
                crate::sd_types::CompositionEdge {
                    parent: "DLGroup".into(),
                    child: "DLDesc".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Structural,
                    prop_name: None,
                },
                // DLGroup → DLTerm: Allowed (CSS noise — peer)
                crate::sd_types::CompositionEdge {
                    parent: "DLGroup".into(),
                    child: "DLTerm".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Allowed,
                    prop_name: None,
                },
                // DLGroup → DLTermHelp: Allowed (CSS noise — peer)
                crate::sd_types::CompositionEdge {
                    parent: "DLGroup".into(),
                    child: "DLTermHelp".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Allowed,
                    prop_name: None,
                },
                // DLTerm → DLDesc: Allowed (CSS descendant noise — peers!)
                crate::sd_types::CompositionEdge {
                    parent: "DLTerm".into(),
                    child: "DLDesc".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Allowed,
                    prop_name: None,
                },
                // DLTermHelp → DLDesc: Allowed (CSS descendant noise — peers!)
                crate::sd_types::CompositionEdge {
                    parent: "DLTermHelp".into(),
                    child: "DLDesc".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Allowed,
                    prop_name: None,
                },
                // DLTermHelp → DLTerm: Allowed (CSS descendant noise — peers!)
                crate::sd_types::CompositionEdge {
                    parent: "DLTermHelp".into(),
                    child: "DLTerm".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Allowed,
                    prop_name: None,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        // Should have "DLDesc not-in DL, use DLGroup" (valid — CHP first hop
        // through DLGroup, then DL as grandparent)
        assert!(
            rules.iter().any(|r| r.rule_id.contains("desc-not-in-dl")),
            "Expected valid invalidDirectChild for DLDesc in DL. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // Should NOT have "DLDesc not-in DLTermHelp, use DLTerm" — the first
        // hop DLTerm→DLDesc is Allowed (CSS noise between peers), so the
        // grandparent walk should skip it.
        let false_rule = rules.iter().any(|r| {
            r.rule_id.contains("desc-not-in-dlterm") || r.rule_id.contains("desc-not-in-termhelp")
        });
        assert!(
            !false_rule,
            "Should NOT generate invalidDirectChild between peer components \
             (DLDesc not-in DLTermHelp via Allowed first hop). Got: {:?}",
            rules
                .iter()
                .filter(|r| r.rule_id.contains("not-in"))
                .map(|r| &r.rule_id)
                .collect::<Vec<_>>()
        );
    }

    /// Internal edges should not affect conformance rules at all.
    #[test]
    fn test_internal_edges_ignored() {
        let tree = CompositionTree {
            root: "Accordion".into(),
            family_members: vec![
                "Accordion".into(),
                "AccordionItem".into(),
                "AccordionContent".into(),
            ],
            edges: vec![
                crate::sd_types::CompositionEdge {
                    parent: "Accordion".into(),
                    child: "AccordionItem".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
                // Internal rendering: AccordionItem renders AccordionContent
                crate::sd_types::CompositionEdge {
                    parent: "AccordionItem".into(),
                    child: "AccordionContent".into(),
                    relationship: ChildRelationship::Internal,
                    required: false,
                    bem_evidence: None,
                    strength: crate::sd_types::EdgeStrength::Required,
                    prop_name: None,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &test_pkg_map());

        // AccordionContent should NOT get any rule — the internal edge
        // doesn't count for no_incoming or parent_to_req_children
        assert!(
            !rules.iter().any(|r| r.rule_id.contains("content")),
            "Internal edges should not generate conformance rules. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
    }

    // ── Fix B: requiresChild includes all valid children ────────────────

    /// Helper: create a Required non-internal edge.
    fn req_edge(parent: &str, child: &str) -> crate::sd_types::CompositionEdge {
        crate::sd_types::CompositionEdge {
            parent: parent.into(),
            child: child.into(),
            relationship: ChildRelationship::DirectChild,
            required: true,
            bem_evidence: None,
            strength: crate::sd_types::EdgeStrength::Required,
            prop_name: None,
        }
    }

    /// Helper: create an Allowed non-internal edge.
    fn allowed_edge(parent: &str, child: &str) -> crate::sd_types::CompositionEdge {
        crate::sd_types::CompositionEdge {
            parent: parent.into(),
            child: child.into(),
            relationship: ChildRelationship::DirectChild,
            required: false,
            bem_evidence: None,
            strength: crate::sd_types::EdgeStrength::Allowed,
            prop_name: None,
        }
    }

    /// requiresChild scanner regex should include Allowed children so
    /// they don't trigger false positives. For example, ToolbarContent
    /// has Required edges to ToolbarFilter/ToolbarToggleGroup but also
    /// Allowed edges to ToolbarGroup/ToolbarItem. The scanner regex
    /// should match ALL of them.
    #[test]
    fn test_requires_child_includes_allowed_children() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("ToolbarContent".into(), "@patternfly/react-core".into());
        pkgs.insert("ToolbarFilter".into(), "@patternfly/react-core".into());
        pkgs.insert("ToolbarToggleGroup".into(), "@patternfly/react-core".into());
        pkgs.insert("ToolbarGroup".into(), "@patternfly/react-core".into());
        pkgs.insert("ToolbarItem".into(), "@patternfly/react-core".into());

        let tree = CompositionTree {
            root: "Toolbar".into(),
            family_members: vec![
                "Toolbar".into(),
                "ToolbarContent".into(),
                "ToolbarFilter".into(),
                "ToolbarToggleGroup".into(),
                "ToolbarGroup".into(),
                "ToolbarItem".into(),
            ],
            edges: vec![
                // Required context edges
                req_edge("ToolbarContent", "ToolbarFilter"),
                req_edge("ToolbarContent", "ToolbarToggleGroup"),
                // Allowed CSS descendant edges
                allowed_edge("ToolbarContent", "ToolbarGroup"),
                allowed_edge("ToolbarContent", "ToolbarItem"),
                // ToolbarContent itself hangs off Toolbar (Allowed)
                allowed_edge("Toolbar", "ToolbarContent"),
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        // ToolbarContent has no Required incoming → gets requiresChild
        let req_rule = rules.iter().find(|r| r.rule_id.contains("content-req-"));
        assert!(
            req_rule.is_some(),
            "Expected requiresChild rule for ToolbarContent. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // The scanner regex should include ALL children (Required + Allowed)
        if let KonveyorCondition::FrontendReferenced { referenced } = &req_rule.unwrap().when {
            let pattern = referenced.requires_child.as_deref().unwrap();
            assert!(
                pattern.contains("ToolbarFilter"),
                "requiresChild should include Required child ToolbarFilter: {}",
                pattern
            );
            assert!(
                pattern.contains("ToolbarGroup"),
                "requiresChild should include Allowed child ToolbarGroup: {}",
                pattern
            );
            assert!(
                pattern.contains("ToolbarItem"),
                "requiresChild should include Allowed child ToolbarItem: {}",
                pattern
            );
        } else {
            panic!("Expected FrontendReferenced condition");
        }

        // The message should mention all valid children
        let msg = &req_rule.unwrap().message;
        assert!(
            msg.contains("ToolbarGroup"),
            "Message should mention Allowed child ToolbarGroup: {}",
            msg
        );
    }

    /// When a parent has only Required children and no Allowed ones,
    /// requiresChild should still work identically (no regression).
    #[test]
    fn test_requires_child_only_required_children_unchanged() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("List".into(), "@patternfly/react-core".into());
        pkgs.insert("ListItem".into(), "@patternfly/react-core".into());

        let tree = CompositionTree {
            root: "List".into(),
            family_members: vec!["List".into(), "ListItem".into()],
            edges: vec![req_edge("List", "ListItem")],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        let req_rule = rules.iter().find(|r| r.rule_id.contains("list-req-"));
        assert!(req_rule.is_some(), "Expected requiresChild rule for List");

        if let KonveyorCondition::FrontendReferenced { referenced } = &req_rule.unwrap().when {
            let pattern = referenced.requires_child.as_deref().unwrap();
            assert_eq!(
                pattern, "^(ListItem)$",
                "With only Required children, regex should be unchanged"
            );
        } else {
            panic!("Expected FrontendReferenced condition");
        }
    }

    /// The fix strategy replacement field should list all valid children,
    /// not just the first one.
    #[test]
    fn test_requires_child_fix_strategy_lists_all_children() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("Menu".into(), "@patternfly/react-core".into());
        pkgs.insert("MenuItem".into(), "@patternfly/react-core".into());
        pkgs.insert("MenuContent".into(), "@patternfly/react-core".into());
        pkgs.insert("MenuList".into(), "@patternfly/react-core".into());

        let tree = CompositionTree {
            root: "Menu".into(),
            family_members: vec![
                "Menu".into(),
                "MenuItem".into(),
                "MenuContent".into(),
                "MenuList".into(),
            ],
            edges: vec![
                req_edge("Menu", "MenuItem"),
                allowed_edge("Menu", "MenuContent"),
                allowed_edge("Menu", "MenuList"),
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        let req_rule = rules.iter().find(|r| r.rule_id.contains("menu-req-"));
        assert!(req_rule.is_some(), "Expected requiresChild rule for Menu");

        let fix = req_rule.unwrap().fix_strategy.as_ref().unwrap();
        let replacement = fix.replacement.as_deref().unwrap();
        assert!(
            replacement.contains("MenuContent") && replacement.contains("MenuItem"),
            "Fix strategy replacement should list all valid children: {}",
            replacement
        );
    }

    // ── Fix C: prop-passed children excluded from requiresChild ─────────

    /// Helper: create a PropPassed edge (child passed via a named prop).
    fn prop_passed_edge(
        parent: &str,
        child: &str,
        prop_name: &str,
    ) -> crate::sd_types::CompositionEdge {
        crate::sd_types::CompositionEdge {
            parent: parent.into(),
            child: child.into(),
            relationship: ChildRelationship::PropPassed,
            required: true,
            bem_evidence: None,
            strength: crate::sd_types::EdgeStrength::Required,
            prop_name: Some(prop_name.into()),
        }
    }

    /// When ALL Required children of a parent are prop-passed, no
    /// requiresChild rule should be generated — the scanner only sees
    /// direct JSX children and would always report a false positive.
    #[test]
    fn test_requires_child_skipped_when_all_prop_passed() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("ChartBullet".into(), "@patternfly/react-charts".into());
        pkgs.insert("ChartBulletTitle".into(), "@patternfly/react-charts".into());
        pkgs.insert(
            "ChartBulletQualitativeRange".into(),
            "@patternfly/react-charts".into(),
        );

        let tree = CompositionTree {
            root: "ChartBullet".into(),
            family_members: vec![
                "ChartBullet".into(),
                "ChartBulletTitle".into(),
                "ChartBulletQualitativeRange".into(),
            ],
            edges: vec![
                prop_passed_edge("ChartBullet", "ChartBulletTitle", "titleComponent"),
                prop_passed_edge(
                    "ChartBullet",
                    "ChartBulletQualitativeRange",
                    "qualitativeRangeComponent",
                ),
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        assert!(
            !rules.iter().any(|r| r.rule_id.contains("req-")),
            "All-prop-passed parent should not get requiresChild rule. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
    }

    /// When a parent has a mix of direct and prop-passed Required children,
    /// only the direct children should appear in the requiresChild regex.
    /// The prop-passed children are invisible to the scanner.
    #[test]
    fn test_requires_child_excludes_prop_passed_children() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("Tab".into(), "@patternfly/react-core".into());
        pkgs.insert("TabAction".into(), "@patternfly/react-core".into());
        pkgs.insert("TabContent".into(), "@patternfly/react-core".into());

        let tree = CompositionTree {
            root: "Tabs".into(),
            family_members: vec![
                "Tabs".into(),
                "Tab".into(),
                "TabAction".into(),
                "TabContent".into(),
            ],
            edges: vec![
                // Direct child — scanner CAN see this
                req_edge("Tab", "TabContent"),
                // Prop-passed — scanner CANNOT see this
                prop_passed_edge("Tab", "TabAction", "actions"),
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        let req_rule = rules.iter().find(|r| r.rule_id.contains("tab-req-"));
        assert!(
            req_rule.is_some(),
            "Tab should still get requiresChild for its direct child. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        if let KonveyorCondition::FrontendReferenced { referenced } = &req_rule.unwrap().when {
            let pattern = referenced.requires_child.as_deref().unwrap();
            assert!(
                pattern.contains("TabContent"),
                "requiresChild should include direct child TabContent: {}",
                pattern
            );
            assert!(
                !pattern.contains("TabAction"),
                "requiresChild should NOT include prop-passed child TabAction: {}",
                pattern
            );
        } else {
            panic!("Expected FrontendReferenced condition");
        }
    }

    // ── Insta snapshot tests for v2 YAML output safety ─────────────────
    //
    // These snapshots capture the exact YAML serialization of v2 rules
    // (composition, conformance, CSS removal, deprecated migration).
    // Any change to serde field names, condition shapes, or rule
    // structure will show as a snapshot diff.

    /// Wrapper that captures both the serialized rule and its fix_strategy
    /// (which is normally skipped by serde on KonveyorRule).
    #[derive(Debug, serde::Serialize)]
    struct RuleSnapshot {
        rule: KonveyorRule,
        fix_strategy: Option<FixStrategyEntry>,
    }

    impl RuleSnapshot {
        fn from_rule(mut rule: KonveyorRule) -> Self {
            let fix_strategy = rule.fix_strategy.take();
            Self { rule, fix_strategy }
        }
    }

    fn snapshot_rules(mut rules: Vec<KonveyorRule>) -> Vec<RuleSnapshot> {
        // Sort by rule_id for deterministic snapshot ordering — the generator
        // iterates over HashSet/HashMap which has non-deterministic order.
        rules.sort_by(|a, b| a.rule_id.cmp(&b.rule_id));
        rules.into_iter().map(RuleSnapshot::from_rule).collect()
    }

    fn make_edge(
        parent: &str,
        child: &str,
        strength: crate::sd_types::EdgeStrength,
    ) -> crate::sd_types::CompositionEdge {
        crate::sd_types::CompositionEdge {
            parent: parent.into(),
            child: child.into(),
            relationship: ChildRelationship::DirectChild,
            required: false,
            bem_evidence: None,
            strength,
            prop_name: None,
        }
    }

    #[test]
    fn snapshot_conformance_not_parent_rules() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("Table".into(), "@patternfly/react-table".into());
        pkgs.insert("Thead".into(), "@patternfly/react-table".into());
        pkgs.insert("Tbody".into(), "@patternfly/react-table".into());
        pkgs.insert("Tr".into(), "@patternfly/react-table".into());
        pkgs.insert("Td".into(), "@patternfly/react-table".into());

        use crate::sd_types::EdgeStrength;

        let tree = CompositionTree {
            root: "Table".into(),
            family_members: vec![
                "Table".into(),
                "Thead".into(),
                "Tbody".into(),
                "Tr".into(),
                "Td".into(),
            ],
            edges: vec![
                make_edge("Table", "Thead", EdgeStrength::Required),
                make_edge("Table", "Tbody", EdgeStrength::Required),
                make_edge("Thead", "Tr", EdgeStrength::Required),
                make_edge("Tbody", "Tr", EdgeStrength::Required),
                make_edge("Tr", "Td", EdgeStrength::Required),
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);
        insta::assert_yaml_snapshot!(snapshot_rules(rules));
    }

    #[test]
    fn snapshot_conformance_requires_child_rule() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("Tabs".into(), "@patternfly/react-core".into());
        pkgs.insert("Tab".into(), "@patternfly/react-core".into());
        pkgs.insert("TabContent".into(), "@patternfly/react-core".into());

        use crate::sd_types::EdgeStrength;

        let tree = CompositionTree {
            root: "Tabs".into(),
            family_members: vec!["Tabs".into(), "Tab".into(), "TabContent".into()],
            edges: vec![
                make_edge("Tabs", "Tab", EdgeStrength::Required),
                make_edge("Tabs", "TabContent", EdgeStrength::Required),
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);
        insta::assert_yaml_snapshot!(snapshot_rules(rules));
    }

    #[test]
    fn snapshot_css_class_removal_rules() {
        let removed_blocks = vec!["select".to_string(), "options-menu".to_string()];
        let rules = generate_css_class_removal_rules(&removed_blocks);
        insta::assert_yaml_snapshot!(snapshot_rules(rules));
    }

    #[test]
    fn snapshot_composition_removed_member_rule() {
        let sd = SdPipelineResult {
            composition_changes: vec![crate::sd_types::CompositionChange {
                family: "EmptyState".into(),
                change_type: CompositionChangeType::FamilyMemberRemoved {
                    member: "EmptyStateHeader".into(),
                },
                description: "EmptyStateHeader was removed from EmptyState family".into(),
                before_pattern: None,
                after_pattern: None,
            }],
            component_packages: {
                let mut m = HashMap::new();
                m.insert("EmptyState".into(), "@patternfly/react-core".into());
                m.insert("EmptyStateHeader".into(), "@patternfly/react-core".into());
                m
            },
            ..SdPipelineResult::default()
        };

        let pkg_map = sd.component_packages.clone();
        let rules = generate_composition_change_rules(&sd, &pkg_map);
        insta::assert_yaml_snapshot!(snapshot_rules(rules));
    }

    #[test]
    fn snapshot_conformance_invalid_direct_child_rule() {
        use crate::sd_types::EdgeStrength;

        let mut pkgs = test_pkg_map();
        pkgs.insert("Nav".into(), "@patternfly/react-core".into());
        pkgs.insert("NavList".into(), "@patternfly/react-core".into());
        pkgs.insert("NavItem".into(), "@patternfly/react-core".into());

        let tree = CompositionTree {
            root: "Nav".into(),
            family_members: vec!["Nav".into(), "NavList".into(), "NavItem".into()],
            edges: vec![
                make_edge("Nav", "NavList", EdgeStrength::Required),
                make_edge("NavList", "NavItem", EdgeStrength::Required),
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);
        insta::assert_yaml_snapshot!(snapshot_rules(rules));
    }

    // ── build_deprecated_migration_from_replacement tests ───────────────

    #[test]
    fn test_deprecated_migration_from_replacement_basic() {
        // Simulate Tile → Card replacement: Tile has props {isSelected, title, icon},
        // Card has props {isSelected, isClickable, isCompact}.
        // Expected: isSelected matches, title/icon are removed, isClickable/isCompact are new.
        let mut old_prop_types = HashMap::new();
        let mut tile_props = BTreeMap::new();
        tile_props.insert("isSelected".into(), "boolean".into());
        tile_props.insert("title".into(), "React.ReactNode".into());
        tile_props.insert("icon".into(), "React.ReactNode".into());
        tile_props.insert("children".into(), "React.ReactNode".into()); // should be filtered
        old_prop_types.insert("Tile".into(), tile_props);

        let mut new_prop_types = HashMap::new();
        let mut card_props = BTreeMap::new();
        card_props.insert("isSelected".into(), "boolean".into());
        card_props.insert("isClickable".into(), "boolean".into());
        card_props.insert("isCompact".into(), "boolean".into());
        card_props.insert("children".into(), "React.ReactNode".into()); // should be filtered
        card_props.insert("className".into(), "string".into()); // should be filtered
        new_prop_types.insert("Card".into(), card_props);

        let mut component_packages = HashMap::new();
        component_packages.insert("Card".into(), "@patternfly/react-core".into());

        let mut old_component_packages = HashMap::new();
        old_component_packages.insert("Tile".into(), "@patternfly/react-core".into());

        let sd = SdPipelineResult {
            old_component_prop_types: old_prop_types,
            new_component_prop_types: new_prop_types,
            component_packages,
            old_component_packages,
            ..SdPipelineResult::default()
        };

        let result =
            super::build_deprecated_migration_from_replacement("Card", "Tile", &sd);
        assert!(result.is_some(), "should produce a migration context");

        let ctx = result.unwrap();
        assert_eq!(ctx.old_package, "@patternfly/react-core");
        assert_eq!(ctx.new_package, "@patternfly/react-core");

        // Matching props: isSelected (same name, same type)
        assert_eq!(ctx.matching_props.len(), 1);
        assert_eq!(ctx.matching_props[0].old_name, "isSelected");
        assert_eq!(ctx.matching_props[0].new_name, "isSelected");
        assert!(!ctx.matching_props[0].type_changed);

        // Removed props: title, icon (on Tile but not Card)
        assert!(ctx.removed_props.contains(&"title".to_string()));
        assert!(ctx.removed_props.contains(&"icon".to_string()));
        assert_eq!(ctx.removed_props.len(), 2);
        // children and className should NOT appear
        assert!(!ctx.removed_props.contains(&"children".to_string()));

        // New props: isClickable, isCompact (on Card but not Tile)
        assert!(ctx.new_props.contains_key("isClickable"));
        assert!(ctx.new_props.contains_key("isCompact"));
        assert_eq!(ctx.new_props.len(), 2);
        // children and className should NOT appear
        assert!(!ctx.new_props.contains_key("children"));
        assert!(!ctx.new_props.contains_key("className"));
    }

    #[test]
    fn test_deprecated_migration_from_replacement_type_changed() {
        // Test that type changes are correctly detected between matching props.
        let mut old_prop_types = HashMap::new();
        let mut old_props = BTreeMap::new();
        old_props.insert("onSelect".into(), "(event: MouseEvent) => void".into());
        old_prop_types.insert("OldComp".into(), old_props);

        let mut new_prop_types = HashMap::new();
        let mut new_props = BTreeMap::new();
        new_props.insert(
            "onSelect".into(),
            "(event: MouseEvent | KeyboardEvent) => void".into(),
        );
        new_prop_types.insert("NewComp".into(), new_props);

        let sd = SdPipelineResult {
            old_component_prop_types: old_prop_types,
            new_component_prop_types: new_prop_types,
            ..SdPipelineResult::default()
        };

        let result =
            super::build_deprecated_migration_from_replacement("NewComp", "OldComp", &sd);
        let ctx = result.unwrap();
        assert_eq!(ctx.matching_props.len(), 1);
        assert!(ctx.matching_props[0].type_changed);
        assert_eq!(
            ctx.matching_props[0].old_type.as_deref(),
            Some("(event: MouseEvent) => void")
        );
        assert_eq!(
            ctx.matching_props[0].new_type.as_deref(),
            Some("(event: MouseEvent | KeyboardEvent) => void")
        );
    }

    #[test]
    fn test_deprecated_migration_from_replacement_no_old_props() {
        // If the old component has no prop types in the map, return None.
        let mut new_prop_types = HashMap::new();
        new_prop_types.insert("Card".into(), BTreeMap::new());

        let sd = SdPipelineResult {
            new_component_prop_types: new_prop_types,
            ..SdPipelineResult::default()
        };

        let result =
            super::build_deprecated_migration_from_replacement("Card", "Tile", &sd);
        assert!(result.is_none());
    }

    #[test]
    fn test_deprecated_migration_context_fallback_to_replacement() {
        // Test the full build_deprecated_migration_context function:
        // no MigrationTarget in report, but deprecated_replacements has Tile → Card.
        let report = {
            use semver_analyzer_core::*;
            AnalysisReport {
                repository: std::path::PathBuf::from("/tmp/test"),
                comparison: Comparison {
                    from_ref: "v5".into(),
                    to_ref: "v6".into(),
                    from_sha: "aaa".into(),
                    to_sha: "bbb".into(),
                    commit_count: 1,
                    analysis_timestamp: "2026-01-01".into(),
                },
                summary: Summary {
                    total_breaking_changes: 0,
                    breaking_api_changes: 0,
                    breaking_behavioral_changes: 0,
                    files_with_breaking_changes: 0,
                },
                changes: vec![],
                manifest_changes: vec![],
                added_files: vec![],
                packages: vec![],
                member_renames: HashMap::new(),
                inferred_rename_patterns: None,
                extensions: crate::extensions::TsAnalysisExtensions {
                    sd_result: None,
                    hierarchy_deltas: Vec::new(),
                    new_hierarchies: Default::default(),
                },
                metadata: AnalysisMetadata {
                    call_graph_analysis: "none".into(),
                    tool_version: "0.1.0".into(),
                    llm_usage: None,
                },
            }
        };

        let mut old_prop_types = HashMap::new();
        let mut tile_props = BTreeMap::new();
        tile_props.insert("isSelected".into(), "boolean".into());
        tile_props.insert("title".into(), "string".into());
        old_prop_types.insert("Tile".into(), tile_props);

        let mut new_prop_types = HashMap::new();
        let mut card_props = BTreeMap::new();
        card_props.insert("isSelected".into(), "boolean".into());
        card_props.insert("isClickable".into(), "boolean".into());
        new_prop_types.insert("Card".into(), card_props);

        let sd = SdPipelineResult {
            old_component_prop_types: old_prop_types,
            new_component_prop_types: new_prop_types,
            deprecated_replacements: vec![crate::sd_types::DeprecatedReplacement {
                old_component: "Tile".into(),
                new_component: "Card".into(),
                evidence_hosts: vec![],
                evidence_source: crate::sd_types::ReplacementEvidence::CommitCoChange,
            }],
            ..SdPipelineResult::default()
        };

        let result =
            super::build_deprecated_migration_context("Card", &report, &sd);
        assert!(result.is_some(), "should fall back to deprecated_replacements");

        let ctx = result.unwrap();
        assert_eq!(ctx.matching_props.len(), 1);
        assert_eq!(ctx.matching_props[0].old_name, "isSelected");
        assert!(ctx.removed_props.contains(&"title".to_string()));
        assert!(ctx.new_props.contains_key("isClickable"));
    }

    // ── extract_aria_attr_name tests ────────────────────────────────

    #[test]
    fn test_extract_aria_attr_name_removed() {
        assert_eq!(
            super::extract_aria_attr_name(
                "aria-disabled attribute removed from <Component> in Button"
            ),
            Some("aria-disabled".into())
        );
    }

    #[test]
    fn test_extract_aria_attr_name_added() {
        assert_eq!(
            super::extract_aria_attr_name(
                "aria-expanded attribute added to <button> in Button"
            ),
            Some("aria-expanded".into())
        );
    }

    #[test]
    fn test_extract_aria_attr_name_changed() {
        assert_eq!(
            super::extract_aria_attr_name(
                "aria-label on <button> in Button changed from 'Close' to 'Dismiss'"
            ),
            Some("aria-label".into())
        );
    }

    #[test]
    fn test_extract_aria_attr_name_role() {
        assert_eq!(
            super::extract_aria_attr_name("role on <div> in Modal changed"),
            Some("role".into())
        );
    }

    #[test]
    fn test_extract_aria_attr_name_not_aria() {
        assert_eq!(
            super::extract_aria_attr_name("class attribute changed on <div> in Card"),
            None
        );
    }

    // ── AriaChange test-impact rule generation tests ────────────────

    #[test]
    fn test_aria_disabled_removed_generates_filecontent_rule() {
        // When aria-disabled is removed from Button, Phase 1 should
        // generate a FileContent rule matching getAttribute('aria-disabled')
        // in test files.
        let changes = vec![SourceLevelChange {
            component: "Button".into(),
            category: SourceLevelCategory::AriaChange,
            description: "aria-disabled attribute removed from <Component> in Button"
                .into(),
            old_value: Some("true".into()),
            new_value: None,
            has_test_implications: true,
            test_description: Some(
                "Removed aria-disabled will break getAttribute queries".into(),
            ),
            element: Some("Component".into()),
            migration_from: None,
            dependency_chain: None,
        }];

        let mut pkgs = HashMap::new();
        pkgs.insert("Button".into(), "@patternfly/react-core".into());

        let rules = super::generate_test_impact_rules(&changes, &pkgs);

        assert_eq!(rules.len(), 1, "should generate exactly one rule");
        let rule = &rules[0];
        assert!(
            rule.rule_id.contains("button"),
            "rule ID should contain component name: {}",
            rule.rule_id
        );
        assert!(
            rule.rule_id.contains("aria-disabled"),
            "rule ID should contain attribute name: {}",
            rule.rule_id
        );
        assert!(
            rule.rule_id.ends_with("removed"),
            "rule ID should end with 'removed': {}",
            rule.rule_id
        );
        assert!(rule.labels.contains(&"change-type=test-impact".to_string()));

        // Verify FileContent condition with getAttribute pattern
        if let KonveyorCondition::FileContent { ref filecontent } = rule.when {
            assert!(
                filecontent.pattern.contains("getAttribute"),
                "pattern should match getAttribute: {}",
                filecontent.pattern
            );
            assert!(
                filecontent.pattern.contains("aria-disabled"),
                "pattern should match aria-disabled: {}",
                filecontent.pattern
            );
            assert!(
                filecontent.file_pattern.contains("spec"),
                "should be scoped to test files: {}",
                filecontent.file_pattern
            );
        } else {
            panic!(
                "Expected FileContent condition, got: {:?}",
                rule.when
            );
        }

        // Verify LlmAssisted fix strategy
        assert!(
            rule.fix_strategy.is_some(),
            "should have a fix strategy"
        );
        assert_eq!(
            rule.fix_strategy.as_ref().unwrap().strategy,
            "LlmAssisted"
        );
    }

    #[test]
    fn test_aria_label_still_generates_function_call_rule() {
        // Regression: aria-label changes should still use the existing
        // FUNCTION_CALL / LABEL_QUERY_PATTERN approach.
        let changes = vec![SourceLevelChange {
            component: "PageToggleButton".into(),
            category: SourceLevelCategory::AriaChange,
            description:
                "aria-label on <button> in PageToggleButton changed from 'Navigation' to 'Menu'"
                    .into(),
            old_value: Some("Navigation".into()),
            new_value: Some("Menu".into()),
            has_test_implications: true,
            test_description: None,
            element: Some("button".into()),
            migration_from: None,
            dependency_chain: None,
        }];

        let mut pkgs = HashMap::new();
        pkgs.insert(
            "PageToggleButton".into(),
            "@patternfly/react-core".into(),
        );

        let rules = super::generate_test_impact_rules(&changes, &pkgs);

        assert_eq!(rules.len(), 1);
        let rule = &rules[0];
        assert!(rule.rule_id.contains("aria-label"));

        // Should be FrontendReferenced with FUNCTION_CALL, NOT FileContent
        if let KonveyorCondition::FrontendReferenced { ref referenced } = rule.when {
            assert_eq!(referenced.location, "FUNCTION_CALL");
            assert!(referenced.pattern.contains("getByLabelText"));
            assert_eq!(
                referenced.value.as_deref(),
                Some("^Navigation$")
            );
        } else {
            panic!(
                "Expected FrontendReferenced condition for aria-label, got: {:?}",
                rule.when
            );
        }

        // aria-label rules should NOT have fix strategy (existing behavior)
        assert!(rule.fix_strategy.is_none());
    }

    #[test]
    fn test_aria_added_skipped() {
        // "attribute added" entries should not generate rules — new
        // attributes don't break existing tests.
        let changes = vec![SourceLevelChange {
            component: "Button".into(),
            category: SourceLevelCategory::AriaChange,
            description: "aria-expanded attribute added to <button> in Button".into(),
            old_value: None,
            new_value: Some("false".into()),
            has_test_implications: true,
            test_description: None,
            element: Some("button".into()),
            migration_from: None,
            dependency_chain: None,
        }];

        let mut pkgs = HashMap::new();
        pkgs.insert("Button".into(), "@patternfly/react-core".into());

        let rules = super::generate_test_impact_rules(&changes, &pkgs);
        assert!(
            rules.is_empty(),
            "should not generate rules for added attributes"
        );
    }

    #[test]
    fn test_transitive_aria_disabled_generates_filecontent_condition() {
        // When aria-disabled removal propagates transitively to a parent
        // component, build_when_for_transitive_change should return a
        // FileContent condition matching getAttribute('aria-disabled').
        let change = SourceLevelChange {
            component: "SearchInput".into(),
            category: SourceLevelCategory::AriaChange,
            description:
                "aria-disabled attribute removed from <Component> in Button".into(),
            old_value: Some("true".into()),
            new_value: None,
            has_test_implications: true,
            test_description: None,
            element: Some("Component".into()),
            migration_from: None,
            dependency_chain: Some(vec!["SearchInput → Button".into()]),
        };

        let result = super::build_when_for_transitive_change(
            &change,
            "SearchInput",
            "@patternfly/react-core",
        );

        assert!(result.is_some(), "should produce a condition");
        let (cond, key) = result.unwrap();
        assert!(
            key.contains("aria-attr:aria-disabled"),
            "key should contain attribute name: {}",
            key
        );

        if let KonveyorCondition::FileContent { ref filecontent } = cond {
            assert!(
                filecontent.pattern.contains("aria-disabled"),
                "pattern should match aria-disabled: {}",
                filecontent.pattern
            );
        } else {
            panic!(
                "Expected FileContent condition, got: {:?}",
                cond
            );
        }
    }
}
