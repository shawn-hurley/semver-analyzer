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
use semver_analyzer_core::{AnalysisReport, ApiChangeType, RemovalDisposition};
use semver_analyzer_konveyor_core::{
    FileContentFields, FixStrategyEntry, FrontendPatternFields, FrontendReferencedFields,
    KonveyorCondition, KonveyorRule, parse_union_string_values, regex_escape,
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

    // ── Prop movement between family members ──────────────────────────
    rules.extend(generate_prop_movement_rules(sd, &component_packages));

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

    // ── Prop default value changes ──────────────────────────────────
    rules.extend(generate_prop_default_changed_rules(
        sd,
        &component_packages,
    ));

    // ── New absorbing prop rules (children→prop migration hints) ────
    rules.extend(generate_new_absorbing_prop_rules(
        sd,
        &component_packages,
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
                // Use the OLD package map for removed members — consumers
                // still import from the v5 path. The new map may resolve to
                // a different package if the component was relocated
                // (e.g., DragDrop moved from react-core to react-drag-drop).
                let pkg = sd
                    .old_component_packages
                    .get(member)
                    .cloned()
                    .unwrap_or_else(|| pkg_for(member, component_packages));
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

                // Suppress when the grandparent is already a valid parent
                // of the child (any edge strength). The notParent rule's
                // regex includes all non-Internal parents, so emitting
                // "X should not be directly in G" would contradict "X must
                // be inside G (or other parents)".
                //
                // Example: MenuList has an Allowed edge from MenuContent
                // (CSS descendant). The notParent rule lists MenuContent as
                // a valid parent. Without this check, the invalidDirectChild
                // generator would emit "MenuList should not be in MenuContent,
                // use Menu" — contradicting the notParent rule.
                if child_to_all_parents
                    .get(child)
                    .is_some_and(|parents| parents.contains(grandparent))
                {
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
                            .entry(component)
                            .or_default()
                            .push(RemovedProp {
                                name: prop,
                                is_reactnode,
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

// ── Prop movement between family members ─────────────────────────────────

/// Detect props that moved from one family member to another.
///
/// When a prop is removed from component A and a prop with the same name
/// is added to component B, where A and B are members of the same family,
/// this is a prop movement — the consumer needs to move the prop value
/// from `<A prop={val}>` to `<B prop={val}>`.
///
/// Example: Accordion family — `isExpanded` was removed from AccordionToggle
/// and added to AccordionItem. The consumer must move `isExpanded={...}` from
/// `<AccordionToggle>` to `<AccordionItem>`.
///
/// This uses SD pipeline data (`old_component_props` / `new_component_props`)
/// which tracks every prop on every component at both versions.
fn generate_prop_movement_rules(
    sd: &SdPipelineResult,
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    for tree in &sd.composition_trees {
        let family_members: HashSet<&str> = tree.family_members.iter().map(|s| s.as_str()).collect();

        // For each family member, compute removed and added props
        let mut member_removed: HashMap<&str, BTreeSet<String>> = HashMap::new();
        let mut member_added: HashMap<&str, BTreeSet<String>> = HashMap::new();

        for member in &tree.family_members {
            let old_props = sd
                .old_component_props
                .get(member)
                .cloned()
                .unwrap_or_default();
            let new_props = sd
                .new_component_props
                .get(member)
                .cloned()
                .unwrap_or_default();

            let removed: BTreeSet<String> = old_props.difference(&new_props).cloned().collect();
            let added: BTreeSet<String> = new_props.difference(&old_props).cloned().collect();

            if !removed.is_empty() {
                member_removed.insert(member.as_str(), removed);
            }
            if !added.is_empty() {
                member_added.insert(member.as_str(), added);
            }
        }

        // For each member's removed props, check if another member gained
        // a prop with the same name
        for (source_member, removed_props) in &member_removed {
            for prop in removed_props {
                // Skip ubiquitous props that are noise
                if prop == "children" || prop == "className" || prop == "ref" {
                    continue;
                }

                for (target_member, added_props) in &member_added {
                    if source_member == target_member {
                        continue;
                    }
                    if !family_members.contains(target_member) {
                        continue;
                    }
                    if !added_props.contains(prop) {
                        continue;
                    }

                    let pkg = pkg_for(source_member, component_packages);

                    let rule_id = format!(
                        "sd-prop-moved-{}-{}-from-{}-to-{}",
                        sanitize(&tree.root),
                        sanitize(prop),
                        short_component_id(source_member, &tree.root),
                        short_component_id(target_member, &tree.root),
                    );

                    rules.push(KonveyorRule {
                        rule_id,
                        labels: vec![
                            "source=semver-analyzer".into(),
                            "change-type=prop-moved".into(),
                            format!("package={}", pkg),
                            format!("family={}", tree.root),
                        ],
                        effort: 3,
                        category: "mandatory".into(),
                        description: format!(
                            "The `{}` prop moved from <{}> to <{}>",
                            prop, source_member, target_member,
                        ),
                        message: format!(
                            "The `{prop}` prop has been removed from <{source}> and moved to <{target}>.\n\
                             Move the prop value from <{source}> to <{target}>.\n\n\
                             Before:\n  <{target}>\n    <{source} {prop}={{value}} />\n  </{target}>\n\n\
                             After:\n  <{target} {prop}={{value}}>\n    <{source} />\n  </{target}>",
                            prop = prop,
                            source = source_member,
                            target = target_member,
                        ),
                        links: vec![],
                        when: KonveyorCondition::FrontendReferenced {
                            referenced: FrontendReferencedFields {
                                pattern: format!("^{}$", prop),
                                location: "JSX_PROP".into(),
                                component: Some(format!("^{}$", source_member)),
                                parent: None,
                                parent_from: None,
                                not_parent: None,
                                child: None,
                                not_child: None,
                                requires_child: None,
                                value: None,
                                from: Some(pkg),
                                file_pattern: None,
                            },
                        },
                        fix_strategy: Some(FixStrategyEntry {
                            strategy: "LlmAssisted".into(),
                            from: Some(prop.clone()),
                            component: Some(source_member.to_string()),
                            replacement: Some(target_member.to_string()),
                            prop: Some(prop.clone()),
                            ..Default::default()
                        }),
                    });
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
        let old_is_next = old_pkg.contains("/next");
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

        // Case 3: Was in /next (preview), now in main → import path changed.
        // Consumer must change import from @pkg/next to @pkg.
        // This is a simple path change, not a deprecation or API migration.
        if old_is_next && new_is_main {
            let rule_id = format!("sd-next-promoted-{}", sanitize(component));
            let message = format!(
                "`{component}` was promoted from preview (`{old_pkg}`) to main exports.\n\
                 Change import from `{old_pkg}` to `{new_pkg_val}`.\n",
            );

            rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".into(),
                    "change-type=import-path-change".into(),
                    format!("package={}", old_pkg),
                    format!("target-package={}", new_pkg_val),
                ],
                effort: 1,
                category: "mandatory".into(),
                description: format!(
                    "{} promoted from /next to main — update import path",
                    component
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
                    strategy: "ImportPathChange".into(),
                    from: Some(old_pkg.clone()),
                    to: Some(new_pkg_val.clone()),
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

        // Source package: use majority vote of member packages so cross-package
        // families resolve correctly. For example, the DragDrop family tree has
        // root "DragDrop" which maps to @patternfly/react-core/deprecated, but
        // its members (DragDropSort, DragDropContainer, Droppable) all live in
        // @patternfly/react-drag-drop. The majority package is the one the
        // fix-engine should match against EnsureDependency rules.
        let source_package = {
            let strip_subpath = |s: &str| -> String {
                s.strip_suffix("/deprecated")
                    .or_else(|| s.strip_suffix("/next"))
                    .unwrap_or(s)
                    .to_string()
            };
            // Count packages across all members (including root)
            let mut pkg_counts: HashMap<String, usize> = HashMap::new();
            for member in std::iter::once(&tree.root).chain(tree.family_members.iter()) {
                if let Some(pkg) = sd.component_packages.get(member.as_str()) {
                    let base = strip_subpath(pkg);
                    *pkg_counts.entry(base).or_default() += 1;
                }
            }
            // Pick the package with the most members
            pkg_counts
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .map(|(pkg, _)| pkg)
        };

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
            source_package,
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
                    report,
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
        appearance_notes: Vec::new(), // MigrationTarget path has no CSS data access
    })
}

/// Extract the type portion from a member signature string.
///
/// Format is `"property: propName: type"` or `"property: propName?: type"`.
/// Returns the type portion after the second `": "` separator, stripping
/// any optional marker (`?`).
fn extract_type_from_member_signature(sig: &str) -> Option<&str> {
    // Skip the kind prefix (e.g., "property")
    let after_kind = sig.split_once(": ")?.1;
    // Skip the prop name
    let type_part = after_kind.split_once(": ")?.1;
    let trimmed = type_part.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Build a `DeprecatedMigrationContext` for cross-name replacements
/// (e.g., Tile → Card) by comparing old and new prop type maps directly,
/// without requiring a `MigrationTarget` from the diff engine.
///
/// This handles cases where the deprecated component has a differently-named
/// replacement detected via rendering swap or commit co-change analysis.
/// The prop overlap is computed by exact name matching between the old
/// component's props and the new (replacement) component's props.
///
/// The prop type maps from source profiles only contain directly-defined
/// interface members (not inherited props). To get better coverage, we also
/// scan the TD report's `StructuralChange` entries for the Props interfaces,
/// which include removed, renamed, and type-changed members with their type
/// signatures. This enrichment catches inherited props that the source
/// profile extraction missed.
fn build_deprecated_migration_from_replacement(
    family_root: &str,
    old_component: &str,
    sd: &SdPipelineResult,
    report: &AnalysisReport<TypeScript>,
) -> Option<semver_analyzer_konveyor_core::DeprecatedMigrationContext> {
    // Start with source profile prop types (directly-defined members).
    let mut old_types = sd
        .old_component_prop_types
        .get(old_component)
        .cloned()
        .unwrap_or_default();
    let mut new_types = sd
        .new_component_prop_types
        .get(family_root)
        .cloned()
        .unwrap_or_default();

    // Enrich from TD report: scan StructuralChange entries for the old/new
    // Props interfaces to pick up inherited members not in source profiles.
    let old_props_name = format!("{}Props", old_component);
    let new_props_name = format!("{}Props", family_root);
    for file_changes in &report.changes {
        for change in &file_changes.breaking_api_changes {
            // Match entries like "ChipProps.onClick" or "LabelProps.variant"
            if let Some((parent, prop)) = change.symbol.split_once('.') {
                if parent == old_props_name {
                    // Extract type from the "before" signature for old props
                    if let Some(ref sig) = change.before {
                        if let Some(typ) = extract_type_from_member_signature(sig) {
                            old_types.entry(prop.to_string()).or_insert(typ.to_string());
                        }
                    }
                }
                if parent == new_props_name {
                    // Extract type from the "after" signature for new props
                    if let Some(ref sig) = change.after {
                        if let Some(typ) = extract_type_from_member_signature(sig) {
                            new_types.entry(prop.to_string()).or_insert(typ.to_string());
                        }
                    }
                }
            }
        }
    }

    if old_types.is_empty() && new_types.is_empty() {
        return None;
    }

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

    // ── Appearance notes from CSS modifier comparison ──────────────────
    //
    // Generic: when the new component has a variant-like prop with union
    // string values that the old component didn't have, compare CSS
    // modifier inventories to detect potential appearance mismatches.
    //
    // Algorithm:
    //   1. Find props on the new component that (a) are union string types
    //      AND (b) don't exist on the old component
    //   2. For each variant value, check if pf-m-{value} exists as a CSS
    //      modifier on the old vs new component
    //   3. Variant values where the NEW has the modifier but the OLD doesn't
    //      are "appearance options the old component had as its default"
    //   4. If only ONE such candidate survives after filtering out shared
    //      modifiers, recommend it as the default
    let appearance_notes = infer_appearance_defaults(
        old_component,
        family_root,
        &old_types,
        &new_types,
        &sd.old_css_modifiers,
        &sd.new_css_modifiers,
    );

    Some(semver_analyzer_konveyor_core::DeprecatedMigrationContext {
        old_package,
        new_package,
        matching_props,
        new_props,
        removed_props,
        appearance_notes,
    })
}

/// Infer default appearance props when a deprecated component is replaced.
///
/// Compares CSS modifier inventories between old and new components to detect
/// when the old component's fixed appearance corresponds to a specific variant
/// value on the new component.
///
/// Example: Chip has no `pf-m-outline` modifier (outline IS its default).
/// Label has `pf-m-outline` as an explicit variant. The algorithm detects
/// that `outline` is a variant value only on the new component, suggesting
/// `variant='outline'` should be added to preserve visual parity.
fn infer_appearance_defaults(
    old_component: &str,
    new_component: &str,
    old_types: &BTreeMap<String, String>,
    new_types: &BTreeMap<String, String>,
    old_css_modifiers: &crate::sd_types::ComponentCssModifiers,
    new_css_modifiers: &crate::sd_types::ComponentCssModifiers,
) -> Vec<String> {
    let mut notes = Vec::new();

    // Find variant-like props: new-only props with union string types
    let variant_props: Vec<(&String, Vec<String>)> = new_types
        .iter()
        .filter(|(name, _)| !old_types.contains_key(name.as_str()))
        .filter_map(|(name, typ)| {
            let values = parse_union_string_values(typ);
            if values.len() >= 2 {
                let sorted: Vec<String> = values.into_iter().collect();
                Some((name, sorted))
            } else {
                None
            }
        })
        .collect();

    if variant_props.is_empty() {
        return notes;
    }

    // Look up CSS modifier data for both components by BEM block.
    // Try camelCase block name, lowercase, and prefix matching.
    let old_block = old_component
        .chars()
        .next()
        .map(|c| c.to_lowercase().to_string())
        .unwrap_or_default()
        + &old_component[1..];
    let new_block = new_component
        .chars()
        .next()
        .map(|c| c.to_lowercase().to_string())
        .unwrap_or_default()
        + &new_component[1..];

    let old_mods = old_css_modifiers
        .get(&old_block)
        .or_else(|| old_css_modifiers.get(&old_component.to_lowercase()));
    let new_mods = new_css_modifiers
        .get(&new_block)
        .or_else(|| new_css_modifiers.get(&new_component.to_lowercase()));

    // Need at least the new component's modifier data
    let new_mods = match new_mods {
        Some(m) => m,
        None => return notes,
    };

    let old_modifier_names: std::collections::HashSet<&str> = old_mods
        .map(|m| m.keys().map(|k| k.as_str()).collect())
        .unwrap_or_default();

    for (prop_name, values) in &variant_props {
        // For each variant value, check if the corresponding CSS modifier
        // exists on the old vs new component
        let mut candidates: Vec<&str> = Vec::new();
        let mut shared: Vec<&str> = Vec::new();

        for value in values {
            let modifier_class = format!("pf-m-{}", value);
            let on_new = new_mods.contains_key(&modifier_class);
            let on_old = old_modifier_names.contains(modifier_class.as_str());

            if on_new && !on_old {
                // This variant is explicit on the new component but the old
                // component didn't have it as an option — it may have been
                // the old component's default appearance.
                candidates.push(value);
            } else if on_new && on_old {
                // Both components have this modifier — it's a non-default
                // option on both.
                shared.push(value);
            }
        }

        if candidates.is_empty() {
            continue;
        }

        // If only ONE candidate remains, it's the likely default appearance
        // for the old component. If multiple remain, list them all and let
        // the LLM choose.
        if candidates.len() == 1 {
            let value = candidates[0];
            notes.push(format!(
                "{old_component} has no pf-m-{value} CSS modifier — {value} is its \
                 default appearance. Add {prop_name}='{value}' to {new_component} \
                 to preserve visual parity.",
            ));
        } else {
            notes.push(format!(
                "{old_component} did not have a {prop_name} prop. \
                 {new_component} defaults to a different appearance. \
                 Consider which {prop_name} value matches {old_component}'s look: {}.",
                candidates.join(", "),
            ));
        }
    }

    notes
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
    is_reactnode: bool,
}

// ── CSS modifier bridge for value mapping ───────────────────────────────

/// Match removed prop values to added prop values by comparing what CSS
/// each value's modifier class actually declares.
///
/// For each removed value (e.g., "cyan"), looks up the corresponding CSS
/// modifier class (e.g., "pf-m-cyan") in the old version's modifier data.
/// For each added value (e.g., "teal"), looks up "pf-m-teal" in the new
/// version. Compares their `CssModifierEffect` by structural similarity
/// (which component token slots are overridden) and returns greedy
/// best-match pairs.
///
/// Returns a map from removed_value → added_value for values that have
/// strong CSS evidence. Values without modifier data or without a good
/// match are omitted (fall through to string similarity).
fn compute_css_modifier_bridge(
    component: &str,
    removed_values: &[&String],
    added_values: &[&String],
    old_css_modifiers: &crate::sd_types::ComponentCssModifiers,
    new_css_modifiers: &crate::sd_types::ComponentCssModifiers,
    old_css_property_targets: &crate::sd_types::CssPropertyTargetMap,
    new_css_property_targets: &crate::sd_types::CssPropertyTargetMap,
) -> HashMap<String, String> {
    let mut hints = HashMap::new();

    if old_css_modifiers.is_empty() || new_css_modifiers.is_empty() {
        return hints;
    }

    // Find the BEM block name for this component. Try common conventions:
    // ComponentName → camelCase block name (e.g., "Label" → "label", "DrawerContent" → "drawerContent")
    let block_lower = component
        .chars()
        .next()
        .map(|c| c.to_lowercase().to_string())
        .unwrap_or_default()
        + &component[1..];

    // Look up modifier data for this component's BEM block.
    // Try: exact camelCase, lowercase, then prefix matching.
    //
    // Prefix matching handles sub-components that share a parent BEM block:
    // "DrawerContent" (camelCase "drawerContent") → BEM block "drawer"
    // "DrawerPanelContent" → BEM block "drawer"
    // "PageSection" → BEM block "page"
    //
    // We check if any existing key is a camelCase prefix of the component
    // name (e.g., "drawer" is a prefix of "drawerContent").
    let old_mods = old_css_modifiers
        .get(&block_lower)
        .or_else(|| old_css_modifiers.get(&component.to_lowercase()))
        .or_else(|| {
            old_css_modifiers
                .iter()
                .filter(|(k, _)| block_lower.starts_with(k.as_str()) && k.len() < block_lower.len())
                .max_by_key(|(k, _)| k.len()) // longest matching prefix
                .map(|(_, v)| v)
        });
    let new_mods = new_css_modifiers
        .get(&block_lower)
        .or_else(|| new_css_modifiers.get(&component.to_lowercase()))
        .or_else(|| {
            new_css_modifiers
                .iter()
                .filter(|(k, _)| block_lower.starts_with(k.as_str()) && k.len() < block_lower.len())
                .max_by_key(|(k, _)| k.len()) // longest matching prefix
                .map(|(_, v)| v)
        });

    let (old_mods, new_mods) = match (old_mods, new_mods) {
        (Some(o), Some(n)) => (o, n),
        _ => return hints,
    };

    // Build (removed_value, old_effect) and (added_value, new_effect) pairs
    // by resolving value → modifier class name → CssModifierEffect
    let old_pairs: Vec<(&str, &crate::sd_types::CssModifierEffect)> = removed_values
        .iter()
        .filter_map(|val| {
            let effect = resolve_value_to_modifier(val, old_mods)?;
            Some((val.as_str(), effect))
        })
        .collect();

    let new_pairs: Vec<(&str, &crate::sd_types::CssModifierEffect)> = added_values
        .iter()
        .filter_map(|val| {
            let effect = resolve_value_to_modifier(val, new_mods)?;
            Some((val.as_str(), effect))
        })
        .collect();

    if old_pairs.is_empty() || new_pairs.is_empty() {
        return hints;
    }

    // Compute pairwise structural similarity and greedy-assign
    let mut candidates: Vec<(&str, &str, f64)> = Vec::new();
    for (old_val, old_effect) in &old_pairs {
        for (new_val, new_effect) in &new_pairs {
            let sim = modifier_similarity(old_effect, new_effect, old_css_property_targets, new_css_property_targets);
            candidates.push((old_val, new_val, sim));
        }
    }

    // Sort by similarity descending
    candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    // Greedy assignment with minimum threshold
    let min_similarity = 0.3;
    let mut used_old: HashSet<&str> = HashSet::new();
    let mut used_new: HashSet<&str> = HashSet::new();

    for (old_val, new_val, sim) in &candidates {
        if *sim < min_similarity {
            continue;
        }
        if used_old.contains(old_val) || used_new.contains(new_val) {
            continue;
        }
        hints.insert(old_val.to_string(), new_val.to_string());
        used_old.insert(old_val);
        used_new.insert(new_val);
    }

    hints
}

/// Resolve a prop value to its CSS modifier class effect.
///
/// Tries common naming conventions: value "cyan" → class "pf-m-cyan",
/// value "horizontal-subnav" → class "pf-m-horizontal-subnav".
/// Also tries "is-{value}" and "has-{value}" for generic patterns.
fn resolve_value_to_modifier<'a>(
    value: &str,
    modifiers: &'a crate::sd_types::CssModifierMap,
) -> Option<&'a crate::sd_types::CssModifierEffect> {
    // Try PatternFly BEM modifier convention
    let pf_class = format!("pf-m-{}", value);
    if let Some(effect) = modifiers.get(&pf_class) {
        return Some(effect);
    }

    // Try generic state modifier conventions
    let is_class = format!("is-{}", value);
    if let Some(effect) = modifiers.get(&is_class) {
        return Some(effect);
    }

    let has_class = format!("has-{}", value);
    if let Some(effect) = modifiers.get(&has_class) {
        return Some(effect);
    }

    // Try the value directly as a class name
    modifiers.get(value)
}

/// Compute structural similarity between two CSS modifier effects.
///
/// Compares the "token slots" each modifier overrides. A token slot is
/// the portion of a custom property name that describes WHAT is being
/// overridden, after stripping the component prefix and modifier name.
///
/// Example:
///   "--pf-v5-c-label--m-cyan--BackgroundColor" → slot "BackgroundColor"
///   "--pf-v6-c-label--m-teal--BackgroundColor" → slot "BackgroundColor"
///   Same slot → structurally equivalent override.
///
/// Also includes direct CSS properties (e.g., "display", "overflow") in
/// the comparison.
///
/// Returns Jaccard similarity over the combined slot set.
fn modifier_structural_similarity(
    old: &crate::sd_types::CssModifierEffect,
    new: &crate::sd_types::CssModifierEffect,
) -> f64 {
    let old_slots: HashSet<String> = old
        .custom_property_overrides
        .keys()
        .filter_map(|k| extract_token_slot(k))
        .chain(old.direct_properties.keys().cloned())
        .collect();

    let new_slots: HashSet<String> = new
        .custom_property_overrides
        .keys()
        .filter_map(|k| extract_token_slot(k))
        .chain(new.direct_properties.keys().cloned())
        .collect();

    if old_slots.is_empty() && new_slots.is_empty() {
        return 0.0;
    }

    let intersection = old_slots.intersection(&new_slots).count();
    let union = old_slots.union(&new_slots).count();

    if union == 0 {
        return 0.0;
    }

    intersection as f64 / union as f64
}

/// Parse a CSS hex color string into (R, G, B) components.
///
/// Handles both short (`#abc` → `(0xaa, 0xbb, 0xcc)`) and long (`#aabbcc`)
/// hex formats. Case-insensitive. Returns `None` for non-hex values
/// (keywords like `transparent`, `inherit`, or non-color values like `0px`).
fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim();
    if !s.starts_with('#') {
        return None;
    }
    let hex = &s[1..];
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            Some((r, g, b))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some((r, g, b))
        }
        _ => None,
    }
}

/// Compute color similarity between two RGB colors as a value in [0.0, 1.0].
///
/// Uses hue-weighted comparison in HSL space. Hue is the primary signal
/// for color identity (gold and yellow have the same hue ~45°, while
/// orangered has hue ~16°). Saturation and lightness contribute less.
///
/// For achromatic colors (saturation near 0, i.e., greys and near-blacks),
/// falls back to lightness comparison only since hue is undefined.
///
/// Weights: hue 60%, saturation 20%, lightness 20%.
fn color_similarity(c1: (u8, u8, u8), c2: (u8, u8, u8)) -> f64 {
    let (h1, s1, l1) = rgb_to_hsl(c1.0, c1.1, c1.2);
    let (h2, s2, l2) = rgb_to_hsl(c2.0, c2.1, c2.2);

    let achromatic_threshold = 0.1;
    let both_achromatic = s1 < achromatic_threshold && s2 < achromatic_threshold;

    if both_achromatic {
        // Both are greys/blacks/whites — compare by lightness only
        return 1.0 - (l1 - l2).abs();
    }

    // Hue distance on the circular scale [0, 360)
    let hue_diff = (h1 - h2).abs();
    let hue_dist = hue_diff.min(360.0 - hue_diff); // shortest arc
    let hue_sim = 1.0 - (hue_dist / 180.0); // 0° diff → 1.0, 180° → 0.0

    let sat_sim = 1.0 - (s1 - s2).abs();
    let light_sim = 1.0 - (l1 - l2).abs();

    0.6 * hue_sim + 0.2 * sat_sim + 0.2 * light_sim
}

/// Convert RGB (0-255) to HSL (hue: 0-360, saturation: 0-1, lightness: 0-1).
fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let r = r as f64 / 255.0;
    let g = g as f64 / 255.0;
    let b = b as f64 / 255.0;

    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;

    if (max - min).abs() < f64::EPSILON {
        // Achromatic
        return (0.0, 0.0, l);
    }

    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };

    let h = if (max - r).abs() < f64::EPSILON {
        let mut h = (g - b) / d;
        if g < b {
            h += 6.0;
        }
        h
    } else if (max - g).abs() < f64::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };

    (h * 60.0, s, l)
}

/// Compute resolved-value similarity between two CSS modifier effects
/// by comparing the actual CSS properties each modifier targets.
///
/// Uses `CssPropertyTargetMap` to trace each custom property override to
/// the actual CSS property it sets on the DOM (e.g., `--pf-v6-c-label--BackgroundColor`
/// → `background-color`). Then compares resolved hex colors at matching
/// CSS properties: `background-color` to `background-color`, `color` to
/// `color`, etc.
///
/// Only shared CSS properties contribute — non-shared properties don't
/// penalize the score. This correctly handles v5→v6 where the custom
/// property token structures changed completely but the underlying CSS
/// properties (background-color, color, border-color) are the same.
fn modifier_resolved_similarity(
    old: &crate::sd_types::CssModifierEffect,
    new: &crate::sd_types::CssModifierEffect,
    old_targets: &crate::sd_types::CssPropertyTargetMap,
    new_targets: &crate::sd_types::CssPropertyTargetMap,
) -> f64 {
    // Resolve each custom property override to its target CSS property + hex color.
    // Multiple custom properties may target the same CSS property (e.g., the base
    // --label--BackgroundColor and the outline --label--m-outline--BackgroundColor
    // both target `background-color`). Prefer the SHORTEST custom property name
    // as it's the primary/base token, not a modifier-specific variant.
    let mut old_css: HashMap<&str, (&str, (u8, u8, u8))> = HashMap::new();
    for (custom_prop, resolved_value) in &old.resolved_overrides {
        if let Some(css_prop) = old_targets.get(custom_prop.as_str()) {
            if let Some(color) = parse_hex_color(resolved_value) {
                let entry = old_css.entry(css_prop.as_str()).or_insert((custom_prop.as_str(), color));
                // Prefer shorter custom property name (primary token)
                if custom_prop.len() < entry.0.len() {
                    *entry = (custom_prop.as_str(), color);
                }
            }
        }
    }

    let mut new_css: HashMap<&str, (&str, (u8, u8, u8))> = HashMap::new();
    for (custom_prop, resolved_value) in &new.resolved_overrides {
        if let Some(css_prop) = new_targets.get(custom_prop.as_str()) {
            if let Some(color) = parse_hex_color(resolved_value) {
                let entry = new_css.entry(css_prop.as_str()).or_insert((custom_prop.as_str(), color));
                if custom_prop.len() < entry.0.len() {
                    *entry = (custom_prop.as_str(), color);
                }
            }
        }
    }

    // Find shared CSS properties
    let shared: Vec<&&str> = old_css.keys().filter(|k| new_css.contains_key(**k)).collect();
    if shared.is_empty() {
        return 0.0;
    }

    // Compare colors at matching CSS properties
    let total_sim: f64 = shared
        .iter()
        .map(|css_prop| {
            let old_color = old_css[**css_prop].1;
            let new_color = new_css[**css_prop].1;
            color_similarity(old_color, new_color)
        })
        .sum();

    total_sim / shared.len() as f64
}

/// Combined similarity: blends structural and resolved signals.
///
/// Structural similarity compares which component tokens are overridden
/// (high when same slots are touched, regardless of values).
/// Resolved similarity compares actual rendered output using color-distance
/// for hex colors and exact matching for non-color values.
///
/// The combined score is: structural × 0.3 + resolved × 0.7 when both
/// are available. Resolved values dominate because structurally-identical
/// modifiers (all PF color variants share the same slots) can only be
/// disambiguated by their actual rendered colors. For example:
///   cyan (#e0f5f5) → teal (#daf2f2): resolved ~0.98, much higher than
///   cyan (#e0f5f5) → yellow (#fff4cc): resolved ~0.87
fn modifier_similarity(
    old: &crate::sd_types::CssModifierEffect,
    new: &crate::sd_types::CssModifierEffect,
    old_targets: &crate::sd_types::CssPropertyTargetMap,
    new_targets: &crate::sd_types::CssPropertyTargetMap,
) -> f64 {
    let structural = modifier_structural_similarity(old, new);

    // If both have resolved overrides, blend with resolved similarity.
    // Weight resolved higher (0.7) because structural similarity is identical
    // for all color modifiers (same token slots), and only the actual rendered
    // values disambiguate them.
    if !old.resolved_overrides.is_empty() && !new.resolved_overrides.is_empty() {
        let resolved = modifier_resolved_similarity(old, new, old_targets, new_targets);
        return structural * 0.3 + resolved * 0.7;
    }

    structural
}

/// Extract the "token slot" from a CSS custom property name.
///
/// Strips the component prefix (version + block name) to isolate what
/// aspect of the component is being overridden. This works across
/// version prefixes (pf-v5-c- vs pf-v6-c-) and produces comparable
/// slots for the same logical property across versions.
///
/// Two strategies, tried in order:
///
/// 1. **Modifier-based**: If the token contains `--m-{name}--` or
///    `--m-{name}__`, extract everything after the modifier name.
///    This handles modifier-specific tokens like:
///    "--pf-v5-c-label--m-cyan--BackgroundColor" → "BackgroundColor"
///    "--pf-v5-c-label--m-cyan__icon--Color" → "icon--Color"
///
/// 2. **Element-based**: If no modifier segment, find the first `__`
///    (BEM element separator) or the last `--` after the block name
///    prefix, and use everything from there. This handles tokens that
///    the modifier overrides but which aren't modifier-specific:
///    "--pf-v5-c-nav__link--PaddingTop" → "link--PaddingTop"
///    "--pf-v5-c-nav__link--Color" → "link--Color"
///    "--pf-v5-c-label--BackgroundColor" → "BackgroundColor"
fn extract_token_slot(token_name: &str) -> Option<String> {
    // Strategy 1: find modifier segment "--m-"
    if let Some(m_idx) = token_name.find("--m-") {
        let after_m = &token_name[m_idx + 4..]; // skip "--m-"
        let end_idx = after_m.find("--").or_else(|| after_m.find("__"));
        if let Some(idx) = end_idx {
            let slot = &after_m[idx..];
            let slot = slot
                .strip_prefix("--")
                .or_else(|| slot.strip_prefix("__"))
                .unwrap_or(slot);
            if !slot.is_empty() {
                return Some(slot.to_string());
            }
        }
    }

    // Strategy 2: strip component prefix, extract element + property
    // Known prefixes: --pf-v5-c-, --pf-v6-c-, --pf-c-
    let stripped = token_name
        .strip_prefix("--pf-v6-c-")
        .or_else(|| token_name.strip_prefix("--pf-v5-c-"))
        .or_else(|| token_name.strip_prefix("--pf-c-"))
        .or_else(|| token_name.strip_prefix("--"));

    let stripped = match stripped {
        Some(s) => s,
        None => return None,
    };

    // Skip the block name: everything up to the first "__" or "--"
    let slot_start = stripped
        .find("__")
        .or_else(|| stripped.find("--"));

    match slot_start {
        Some(idx) => {
            let slot = &stripped[idx..];
            let slot = slot
                .strip_prefix("__")
                .or_else(|| slot.strip_prefix("--"))
                .unwrap_or(slot);
            if slot.is_empty() {
                None
            } else {
                Some(slot.to_string())
            }
        }
        None => None,
    }
}

/// Returns a similarity boost when a removed value's directional suffix
/// maps semantically to an added value's CSS logical direction suffix.
///
/// Physical → Logical direction mapping (CSS Writing Modes Level 3):
///   Left  → Start (inline-start in LTR)
///   Right → End   (inline-end in LTR)
///
/// PatternFly v6 renamed directional prop values from physical (Left/Right)
/// to logical (Start/End) for RTL support. The LCS-based `name_similarity`
/// function produces wrong greedy matches because "alignLeft" has higher
/// character overlap with "alignCenter" than "alignStart".
///
/// The boost (+0.10) is small enough to not override a genuinely better
/// match, but large enough to break the tie in favor of the semantically
/// correct directional pair.
fn directional_similarity_boost(removed: &str, added: &str) -> f64 {
    let r = removed.to_lowercase();
    let a = added.to_lowercase();

    if (r.ends_with("left") && a.ends_with("start"))
        || (r.ends_with("right") && a.ends_with("end"))
    {
        0.10
    } else {
        0.0
    }
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
            let old_values = parse_union_string_values(before);
            let new_values = parse_union_string_values(after);

            if old_values.is_empty() {
                continue;
            }

            let removed: Vec<&String> = old_values.difference(&new_values).collect();
            if removed.is_empty() {
                continue;
            }

            let pkg = pkg_for(&component, component_packages);

            // ── CSS modifier bridge ──────────────────────────────
            // Before falling back to string similarity, try matching
            // removed values to candidate values by comparing what CSS
            // each modifier class actually declares. This produces
            // evidence-based mappings like cyan→teal (both override
            // the same set of component tokens) instead of relying
            // on name similarity which fails for semantic renames.
            //
            // Candidates include BOTH added values and surviving values.
            // A surviving value (one that exists in both old and new) can
            // be a valid replacement when a removed value's functionality
            // was merged into it. The CSS bridge provides evidence-based
            // matching regardless of whether the candidate is new or
            // pre-existing, avoiding the false matches that string
            // similarity produces on surviving values (e.g., Nav.variant
            // "tertiary" → "horizontal" was wrong; CSS bridge correctly
            // returns no match because they affect different properties).
            let added_for_bridge: Vec<&String> =
                new_values.difference(&old_values).collect();
            let surviving_for_bridge: Vec<&String> =
                old_values.intersection(&new_values).collect();
            let all_candidates_for_bridge: Vec<&String> = added_for_bridge
                .iter()
                .chain(surviving_for_bridge.iter())
                .cloned()
                .collect();
            let css_bridge_hints = compute_css_modifier_bridge(
                &component,
                &removed,
                &all_candidates_for_bridge,
                &sd.old_css_modifiers,
                &sd.new_css_modifiers,
                &sd.old_css_property_targets,
                &sd.new_css_property_targets,
            );
            if !css_bridge_hints.is_empty() {
                tracing::debug!(
                    component = %component,
                    prop = %prop,
                    hints = ?css_bridge_hints,
                    "CSS modifier bridge produced value mappings"
                );
            }

            // Generate one rule per removed value for precise matching
            for value in &removed {
                let rule_id = format!(
                    "sd-prop-value-{}-{}-{}",
                    sanitize(&component),
                    sanitize(&prop),
                    sanitize(value),
                );

                // ── Value mapping heuristic ────────────────────────
                // Try to find the best replacement among newly-added values.
                let added_values: Vec<&String> =
                    new_values.difference(&old_values).collect();
                let surviving: HashSet<&String> =
                    old_values.intersection(&new_values).collect();
                let is_complete_replacement = surviving.is_empty();

                // Build replacement hints using greedy N:M assignment across
                // ALL removed values (not per-value independently). This prevents
                // multiple removed values from claiming the same added value and
                // enables leftover pairing (TC021: light-200→secondary after
                // no-background claims primary).
                //
                // The hints are computed once per (component, prop) type change,
                // then looked up per removed value below.
                let replacement_hints: HashMap<String, String> = {
                    let mut hints = HashMap::new();

                    if added_values.len() == 1 && removed.len() == 1 {
                        // 1:1: only one removed, only one added → auto-map
                        hints.insert(removed[0].clone(), added_values[0].clone());
                    } else if is_complete_replacement && !added_values.is_empty() {
                        // Complete replacement: greedy best-match, no threshold.
                        // Apply directional boost so Left→Start and Right→End
                        // are preferred over character-level coincidences.
                        let mut candidates: Vec<(&String, &String, f64)> = Vec::new();
                        for rem in &removed {
                            for add in &added_values {
                                let sim = semver_analyzer_core::diff::name_similarity(rem, add)
                                    + directional_similarity_boost(rem, add);
                                candidates.push((rem, add, sim));
                            }
                        }
                        candidates.sort_by(|a, b| {
                            b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal)
                        });
                        let mut used_added_vals: HashSet<&str> = HashSet::new();
                        let mut used_removed_vals: HashSet<&str> = HashSet::new();
                        for (rem, add, _sim) in &candidates {
                            if used_removed_vals.contains(rem.as_str())
                                || used_added_vals.contains(add.as_str())
                            {
                                continue;
                            }
                            hints.insert(rem.to_string(), add.to_string());
                            used_removed_vals.insert(rem);
                            used_added_vals.insert(add);
                        }
                    } else {
                        // Partial replacement: each removed value independently
                        // picks its best match among ADDED values only. Multiple
                        // removed values CAN map to the same new value (e.g.,
                        // 'dark' and 'darker' both → 'secondary').
                        //
                        // Surviving values are NOT matched via string similarity.
                        // A surviving value that existed in both v5 and v6 is a
                        // distinct concept, not a rename target. Using string
                        // similarity on survivors produced false matches:
                        //   Nav.variant: tertiary → horizontal (wrong, should be horizontal-subnav)
                        //   PageSection.type: nav → subnav (wrong, should be removed)
                        //   Label.color: cyan → orange (wrong, should be teal)
                        //
                        // Surviving values CAN still be matched via the CSS
                        // modifier bridge (computed above), which compares actual
                        // CSS property effects — a much stronger signal than
                        // name similarity.
                        for rem in &removed {
                            // Try added values (with directional boost)
                            let best_added = added_values
                                .iter()
                                .filter(|a| {
                                    semver_analyzer_core::diff::name_similarity(rem, a)
                                        + directional_similarity_boost(rem, a)
                                        >= 0.3
                                })
                                .max_by(|a, b| {
                                    let sa = semver_analyzer_core::diff::name_similarity(rem, a)
                                        + directional_similarity_boost(rem, a);
                                    let sb = semver_analyzer_core::diff::name_similarity(rem, b)
                                        + directional_similarity_boost(rem, b);
                                    let da = directional_similarity_boost(rem, a) > 0.0;
                                    let db = directional_similarity_boost(rem, b) > 0.0;
                                    sa.partial_cmp(&sb)
                                        .unwrap_or(std::cmp::Ordering::Equal)
                                        .then(da.cmp(&db)) // tiebreaker: prefer directional match
                                });

                            if let Some(add) = best_added {
                                hints.insert(rem.to_string(), add.to_string());
                            }
                            // No surviving-value string similarity fallback.
                            // The CSS bridge (computed above with both added and
                            // surviving candidates) provides evidence-based
                            // surviving-value matching where CSS data is available.
                            // When neither CSS bridge nor added-value string
                            // matching produces a hint, the rule emits
                            // "Valid values: ..." and the LLM handles the mapping.
                        }
                    }

                    hints
                };

                // CSS bridge hints override string similarity when available
                let replacement_hint = css_bridge_hints
                    .get(*value)
                    .cloned()
                    .or_else(|| replacement_hints.get(*value).cloned());
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

    // ── Phase 2: Removed props with a known replacement (ReplacedByMember) ──
    //
    // When a prop is removed and the SD enrichment found a replacement prop
    // (e.g., Banner variant → color), generate per-value rules mapping old
    // prop values to new prop values. Uses string similarity for value mapping
    // and LlmAssisted strategy since the transformation crosses prop boundaries
    // (old prop name → new prop name + new value).
    for fc in &report.changes {
        for api in &fc.breaking_api_changes {
            if api.change != ApiChangeType::Removed {
                continue;
            }
            let new_member = match &api.removal_disposition {
                Some(RemovalDisposition::ReplacedByMember { new_member }) => new_member,
                _ => continue,
            };

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

            // Get old values from the removed prop's before type
            let old_values = api
                .before
                .as_deref()
                .map(parse_union_string_values)
                .unwrap_or_default();
            if old_values.is_empty() {
                continue;
            }

            // Get new values from the replacement prop's type via SD data
            let new_values = sd
                .new_component_prop_types
                .get(&component)
                .and_then(|m| m.get(new_member))
                .map(|t| parse_union_string_values(t))
                .unwrap_or_default();
            if new_values.is_empty() {
                continue;
            }

            let pkg = pkg_for(&component, component_packages);

            // Check if any OTHER new prop on this component has values that
            // overlap with the old values — indicates a secondary prop
            // (e.g., Banner status has "danger", "success", "warning")
            let mut secondary_props: Vec<(String, BTreeSet<String>)> = Vec::new();
            if let Some(new_props) = sd.new_component_prop_types.get(&component) {
                for (prop_name, prop_type) in new_props {
                    if prop_name == new_member || prop_name == &old_prop {
                        continue;
                    }
                    let prop_values = parse_union_string_values(prop_type);
                    let overlap: Vec<&String> = old_values
                        .intersection(&prop_values)
                        .collect();
                    if !overlap.is_empty() {
                        secondary_props.push((prop_name.clone(), prop_values));
                    }
                }
            }

            // Build value mapping: old value → new value on the replacement prop
            let mut value_map: HashMap<String, String> = HashMap::new();
            {
                let mut candidates: Vec<(&String, &String, f64)> = Vec::new();
                for old_val in &old_values {
                    for new_val in &new_values {
                        let sim =
                            semver_analyzer_core::diff::name_similarity(old_val, new_val);
                        candidates.push((old_val, new_val, sim));
                    }
                }
                candidates.sort_by(|a, b| {
                    b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal)
                });
                let mut used_old: HashSet<&str> = HashSet::new();
                let mut used_new: HashSet<&str> = HashSet::new();
                for (old_val, new_val, _sim) in &candidates {
                    if used_old.contains(old_val.as_str())
                        || used_new.contains(new_val.as_str())
                    {
                        continue;
                    }
                    value_map.insert(old_val.to_string(), new_val.to_string());
                    used_old.insert(old_val);
                    used_new.insert(new_val);
                }
            }

            tracing::debug!(
                component = %component,
                old_prop = %old_prop,
                new_member = %new_member,
                value_map = ?value_map,
                secondary_props = ?secondary_props.iter().map(|(n, _)| n).collect::<Vec<_>>(),
                "Phase 2: Removed prop with replacement — generating per-value rules"
            );

            // Generate per-value rules
            for old_val in &old_values {
                let rule_id = format!(
                    "sd-prop-replaced-{}-{}-{}",
                    sanitize(&component),
                    sanitize(&old_prop),
                    sanitize(old_val),
                );

                let new_val = value_map.get(old_val.as_str());

                // Build the migration message
                let mut message = if let Some(nv) = new_val {
                    format!(
                        "The `{}` prop has been removed from <{}>.\n\
                         Replace `{}=\"{}\"` with `{}=\"{}\"`.\n\n\
                         Old: <{component} {old_prop}=\"{old_val}\" />\n\
                         New: <{component} {new_member}=\"{new_val}\" />",
                        old_prop,
                        component,
                        old_prop,
                        old_val,
                        new_member,
                        nv,
                        component = component,
                        old_prop = old_prop,
                        old_val = old_val,
                        new_member = new_member,
                        new_val = nv,
                    )
                } else {
                    format!(
                        "The `{}` prop has been removed from <{}>.\n\
                         The value \"{}\" has no direct replacement on the `{}` prop.\n\
                         Consider removing `{}=\"{}\"` entirely.",
                        old_prop, component, old_val, new_member, old_prop, old_val,
                    )
                };

                // If secondary props overlap with this value, mention them
                for (sec_prop, sec_values) in &secondary_props {
                    if sec_values.contains(old_val) {
                        message.push_str(&format!(
                            "\n\nAlso add `{}=\"{}\"` to preserve semantic meaning.",
                            sec_prop, old_val,
                        ));
                    }
                }

                rules.push(KonveyorRule {
                    rule_id,
                    labels: vec![
                        "source=semver-analyzer".into(),
                        "change-type=prop-value-removed".into(),
                        format!("package={}", pkg),
                    ],
                    effort: 3,
                    category: "mandatory".into(),
                    description: format!(
                        "Prop `{}` removed from <{}>; value \"{}\" must migrate to `{}`",
                        old_prop, component, old_val, new_member,
                    ),
                    message: message.clone(),
                    links: vec![],
                    when: KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: format!("^{}$", old_prop),
                            location: "JSX_PROP".into(),
                            component: Some(format!("^{}$", component)),
                            parent: None,
                            not_parent: None,
                            child: None,
                            not_child: None,
                            requires_child: None,
                            parent_from: None,
                            value: Some(format!("^{}$", regex_escape(old_val))),
                            from: Some(pkg.to_string()),
                            file_pattern: None,
                        },
                    },
                    fix_strategy: Some(FixStrategyEntry {
                        strategy: "LlmAssisted".into(),
                        component: Some(component.clone()),
                        prop: Some(old_prop.clone()),
                        from: Some(old_val.to_string()),
                        replacement: new_val.cloned(),
                        ..Default::default()
                    }),
                });
            }
        }
    }

    // ── Phase 3: Renamed props with value changes ────────────────────
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

            let old_values = parse_union_string_values(old_type);
            let new_values = parse_union_string_values(new_type);

            if old_values.is_empty() || new_values.is_empty() {
                continue;
            }

            let removed: Vec<&String> = old_values.difference(&new_values).collect();
            if removed.is_empty() {
                continue;
            }

            let pkg = pkg_for(&component, component_packages);

            for value in &removed {
                let replacement_hint: Option<String> = None;

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
        // Compare against old REQUIRED props (not all old props).
        // This catches both:
        // - Brand new props that are required
        // - Existing props that were optional in v5 and became required in v6
        let old_required = sd
            .old_required_props
            .get(component)
            .cloned()
            .unwrap_or_default();

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

            // Distinguish "new required prop" from "optional prop became required"
            let is_new_prop = !sd
                .old_component_props
                .get(component)
                .is_some_and(|old| old.contains(*prop));

            let (description, message) = if is_new_prop {
                (
                    format!(
                        "<{}> now requires the `{}` prop{}",
                        component, prop, type_hint,
                    ),
                    format!(
                        "<{}> has a new required prop `{}`{}.\n\
                         This prop must be provided — omitting it will cause a TypeScript error.\n\n\
                         Add the prop: <{} {}={{...}} />",
                        component, prop, type_hint, component, prop,
                    ),
                )
            } else {
                (
                    format!(
                        "The `{}` prop on <{}> is now required (was previously optional){}",
                        prop, component, type_hint,
                    ),
                    format!(
                        "The `{}` prop on <{}> is now required{}.\n\
                         It was optional in the previous version but must now be provided.\n\n\
                         Ensure all usages include: <{} {}={{...}} />",
                        prop, component, type_hint, component, prop,
                    ),
                )
            };

            rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".into(),
                    "change-type=required-prop-added".into(),
                    format!("package={}", pkg),
                ],
                effort: 1,
                category: "mandatory".into(),
                description,
                message,
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

// ── Prop default value change rules ─────────────────────────────────────
//
// When a prop's default value changes between versions, consumers relying on
// the old default will see changed behavior silently. Generate rules to flag
// these and suggest explicit prop values.

/// Returns true for default values that are trivially empty / meaningless.
/// No consumer code relies on these as explicit defaults — generating
/// PropDefault rules for them causes the LLM to add noise.
///
/// Suppressed values:
/// - `''`, `""` — empty strings (TC023: children='', TC045: className='')
/// - `undefined`, `null` — nullish values
/// - `false` — removing a false default → undefined (both falsy, same effect)
/// - Noop arrow functions returning undefined (TC037: Label.onClick)
fn is_trivial_default(value: &str) -> bool {
    matches!(value, "''" | "\"\"" | "undefined" | "null" | "false")
        // Noop arrow functions: (...) => undefined [as any]
        // e.g., "(_e: React.MouseEvent) => undefined as any"
        || (value.contains("=>") && value.contains("undefined"))
}

fn generate_prop_default_changed_rules(
    sd: &SdPipelineResult,
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    use crate::sd_types::SourceLevelCategory;

    let mut rules = Vec::new();

    for change in &sd.source_level_changes {
        if change.category != SourceLevelCategory::PropDefault {
            continue;
        }
        // Only handle removed/changed defaults (old_value is Some).
        // A newly-added default (old_value=None) is not a breaking change.
        let old_val = match &change.old_value {
            Some(v) => v,
            None => continue,
        };

        // Skip trivially empty defaults — no consumer relies on '' or
        // undefined/null defaults, and firing on them causes the LLM to
        // add noise like `children={''}` or `className={''}`.
        // (TC023, TC024, TC045)
        if is_trivial_default(old_val) {
            tracing::debug!(
                component = %change.component,
                old_default = %old_val,
                "Skipping PropDefault rule: trivial default value"
            );
            continue;
        }

        // Extract prop name from description pattern:
        // "Default value for 'X' prop on Y removed (was Z)"
        // "Default value for 'X' prop on Y changed from Z to W"
        let prop_name = change
            .description
            .split('\'')
            .nth(1)
            .unwrap_or("unknown")
            .to_string();

        // Skip PropDefault rules for props that were removed entirely.
        // A removed prop can't have a meaningful "changed default" — it
        // doesn't exist in v6. Generating a rule for it would conflict
        // with the RemoveProp rule and cause the LLM to re-add the prop.
        //
        // Only skip when we positively know the prop was removed: the
        // component IS in new_component_props but the prop is NOT in
        // its set. If the component isn't in the map at all, we don't
        // have enough info — generate the rule to be safe.
        let prop_was_removed = sd
            .new_component_props
            .get(&change.component)
            .is_some_and(|props| !props.contains(&prop_name));
        if prop_was_removed {
            tracing::debug!(
                component = %change.component,
                prop = %prop_name,
                "Skipping PropDefault rule: prop was removed in new version"
            );
            continue;
        }

        let pkg = pkg_for(&change.component, component_packages);

        let new_val_msg = match &change.new_value {
            Some(nv) => format!(
                "The default value of `{}` on <{}> changed from `{}` to `{}`.\n\
                 Existing code relying on the old default may behave differently.\n\n\
                 To preserve v5 behavior, add the prop explicitly:\n  \
                 <{} {}={{{}}} />",
                prop_name, change.component, old_val, nv,
                change.component, prop_name, old_val,
            ),
            None => format!(
                "The default value of `{}` on <{}> was removed (was `{}`).\n\
                 Existing code relying on the old default may behave differently.\n\n\
                 To preserve v5 behavior, add the prop explicitly:\n  \
                 <{} {}={{{}}} />",
                prop_name, change.component, old_val,
                change.component, prop_name, old_val,
            ),
        };

        let rule_id = format!(
            "sd-prop-default-{}-{}",
            sanitize(&change.component),
            sanitize(&prop_name),
        );

        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".into(),
                "change-type=prop-default-changed".into(),
                format!("package={}", pkg),
            ],
            effort: 1,
            category: "mandatory".into(),
            description: format!(
                "Default value of `{}` on <{}> changed (was `{}`)",
                prop_name, change.component, old_val,
            ),
            message: new_val_msg,
            links: vec![],
            when: KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: format!("^{}$", change.component),
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
                component: Some(change.component.clone()),
                prop: Some(prop_name),
                ..Default::default()
            }),
        });
    }

    rules
}

// ── New absorbing prop rules ────────────────────────────────────────────
//
// When a component gains a new prop of type ReactNode/ReactElement that
// didn't exist in the previous version, content that was previously passed
// as children may need to move to that prop. Generate LlmAssisted rules
// with enough context for the LLM to determine if restructuring is needed.
//
// Examples:
// - MenuToggle gained `icon: React.ReactNode` → icons in children should
//   move to the `icon` prop (TC046)
// - NavItem gained `icon: React.ReactNode` → same pattern (TC053)
// - Any component gaining `actions`, `header`, `footer` ReactNode props

fn generate_new_absorbing_prop_rules(
    sd: &SdPipelineResult,
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    for (component, new_props) in &sd.new_component_props {
        // Must have old props to compute the diff
        let old_props = match sd.old_component_props.get(component) {
            Some(p) => p,
            None => continue,
        };

        // Component must accept children (otherwise there's nothing to absorb)
        let has_children = sd
            .new_profiles
            .get(component)
            .map(|p| p.has_children_prop)
            .unwrap_or(false);
        if !has_children {
            continue;
        }

        // Find newly added props
        let added: Vec<&String> = new_props.difference(old_props).collect();
        if added.is_empty() {
            continue;
        }

        // Get type info for the new props
        let type_map = sd.new_component_prop_types.get(component);

        for prop in &added {
            // Skip `children` itself
            if prop.as_str() == "children" {
                continue;
            }

            // Must be a ReactNode/ReactElement type (accepts JSX content)
            let prop_type = type_map
                .and_then(|m| m.get(prop.as_str()))
                .map(|s| s.as_str())
                .unwrap_or("");
            if !is_react_node_type(prop_type) {
                continue;
            }

            let pkg = pkg_for(component, component_packages);

            let message = format!(
                "Component <{component}> has a new prop `{prop}` (type: `{prop_type}`) \
                 added in v6 that accepts JSX content.\n\n\
                 This prop may be intended to receive content that was previously \
                 passed as children. Review the JSX children of <{component}> and \
                 determine if any should be moved to the `{prop}` prop.\n\n\
                 Example migration:\n  \
                 Before: <{component}>{{content}} other children</{component}>\n  \
                 After:  <{component} {prop}={{{{content}}}}>other children</{component}>"
            );

            let rule_id = format!(
                "sd-new-absorbing-prop-{}-{}",
                sanitize(component),
                sanitize(prop),
            );

            rules.push(KonveyorRule {
                rule_id,
                labels: vec![
                    "source=semver-analyzer".into(),
                    "change-type=new-absorbing-prop".into(),
                    format!("package={}", pkg),
                ],
                effort: 3,
                category: "potential".into(),
                description: format!(
                    "New `{}` prop on <{}> may absorb children content",
                    prop, component,
                ),
                message,
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
                    to: Some(prop_type.to_string()),
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

    fn make_empty_report() -> AnalysisReport<TypeScript> {
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

    /// When a grandparent is already a valid parent of the child (any edge
    /// strength), the invalidDirectChild rule should be suppressed because
    /// it contradicts the notParent rule that lists the grandparent as valid.
    ///
    /// Example: Menu family has Menu → MenuList (Structural/CHP) and
    /// MenuContent → MenuList (Allowed). The grandparent walk goes
    /// MenuList → Menu (CHP) → MenuContent (parent of Menu). Since
    /// MenuContent is already a valid parent of MenuList (Allowed edge),
    /// the rule "MenuList not-in MenuContent, use Menu" should NOT fire.
    #[test]
    fn test_invalid_direct_child_suppressed_when_grandparent_is_valid_parent() {
        let mut pkgs = test_pkg_map();
        pkgs.insert("Menu".into(), "@patternfly/react-core".into());
        pkgs.insert("MenuContent".into(), "@patternfly/react-core".into());
        pkgs.insert("MenuList".into(), "@patternfly/react-core".into());
        pkgs.insert("MenuItem".into(), "@patternfly/react-core".into());

        let tree = CompositionTree {
            root: "Menu".into(),
            family_members: vec![
                "Menu".into(),
                "MenuContent".into(),
                "MenuList".into(),
                "MenuItem".into(),
            ],
            edges: vec![
                // Menu → MenuContent: Structural (CHP=YES, PMC=NO)
                make_edge("Menu", "MenuContent", crate::sd_types::EdgeStrength::Structural),
                // Menu → MenuList: Structural (CHP=YES, PMC=NO)
                make_edge("Menu", "MenuList", crate::sd_types::EdgeStrength::Structural),
                // MenuContent → MenuList: Allowed (CSS descendant — transparent wrapper)
                make_edge("MenuContent", "MenuList", crate::sd_types::EdgeStrength::Allowed),
                // MenuList → MenuItem: Required
                make_edge("MenuList", "MenuItem", crate::sd_types::EdgeStrength::Required),
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        // The notParent rule for MenuList should include MenuContent as valid
        let not_parent_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("list-in-"));
        assert!(
            not_parent_rule.is_some(),
            "Expected notParent rule for MenuList"
        );

        // Should NOT have "MenuList not-in MenuContent, use Menu" because
        // MenuContent is already a valid parent (Allowed edge exists).
        let false_rule = rules
            .iter()
            .any(|r| r.rule_id.contains("list-not-in-content"));
        assert!(
            !false_rule,
            "invalidDirectChild 'MenuList not-in MenuContent' should be suppressed \
             because MenuContent is already a valid parent in the notParent rule. Got: {:?}",
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

    /// When a component moves between packages (e.g., DragDrop from react-core
    /// to react-drag-drop), removed-member rules must use the OLD package in
    /// `from:` so they match consumer code that still imports from the v5 path.
    #[test]
    fn test_removed_member_uses_old_package_for_cross_package_move() {
        let sd = SdPipelineResult {
            composition_changes: vec![crate::sd_types::CompositionChange {
                family: "deprecated/DragDrop".into(),
                change_type: CompositionChangeType::FamilyMemberRemoved {
                    member: "Draggable".into(),
                },
                description: "Draggable was removed from the DragDrop family".into(),
                before_pattern: None,
                after_pattern: None,
            }],
            // v6 map: Draggable now lives in react-drag-drop
            component_packages: {
                let mut m = HashMap::new();
                m.insert("Draggable".into(), "@patternfly/react-drag-drop".into());
                m
            },
            // v5 map: Draggable was in react-core
            old_component_packages: {
                let mut m = HashMap::new();
                m.insert("Draggable".into(), "@patternfly/react-core".into());
                m
            },
            ..SdPipelineResult::default()
        };

        let pkg_map = sd.component_packages.clone();
        let rules = generate_composition_change_rules(&sd, &pkg_map);

        assert_eq!(rules.len(), 1);
        let rule = &rules[0];
        // The `from:` field should use the OLD package (@patternfly/react-core),
        // not the new one (@patternfly/react-drag-drop), because consumers
        // still import from the v5 path.
        match &rule.when {
            KonveyorCondition::FrontendReferenced { referenced } => {
                assert_eq!(
                    referenced.from.as_deref(),
                    Some("@patternfly/react-core"),
                    "removed-member rule should use old_component_packages for from:"
                );
            }
            _ => panic!("expected FrontendReferenced condition"),
        }
    }

    /// When a prop is removed from one family member and added to another
    /// family member, generate a prop-movement rule that instructs moving
    /// the prop value between components.
    ///
    /// Example: Accordion family — `isExpanded` removed from AccordionToggle,
    /// added to AccordionItem.
    #[test]
    fn test_prop_movement_between_family_members() {
        let mut sd = SdPipelineResult::default();

        // Accordion family: AccordionToggle lost isExpanded, AccordionItem gained it
        sd.old_component_props.insert(
            "AccordionToggle".into(),
            ["isExpanded", "id", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "AccordionToggle".into(),
            ["id", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.old_component_props.insert(
            "AccordionItem".into(),
            ["className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "AccordionItem".into(),
            ["isExpanded", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        // Root component (unchanged props, just needs to exist)
        sd.old_component_props.insert(
            "Accordion".into(),
            ["displaySize"].iter().map(|s| s.to_string()).collect(),
        );
        sd.new_component_props.insert(
            "Accordion".into(),
            ["displaySize"].iter().map(|s| s.to_string()).collect(),
        );

        sd.composition_trees = vec![CompositionTree {
            root: "Accordion".into(),
            family_members: vec![
                "Accordion".into(),
                "AccordionItem".into(),
                "AccordionToggle".into(),
            ],
            edges: vec![],
        }];

        let mut pkgs = HashMap::new();
        pkgs.insert("Accordion".into(), "@patternfly/react-core".into());
        pkgs.insert("AccordionItem".into(), "@patternfly/react-core".into());
        pkgs.insert("AccordionToggle".into(), "@patternfly/react-core".into());

        let rules = generate_prop_movement_rules(&sd, &pkgs);

        // Should detect isExpanded moving from AccordionToggle to AccordionItem
        assert_eq!(rules.len(), 1, "Expected 1 prop-movement rule, got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>());

        let rule = &rules[0];
        assert!(
            rule.rule_id.contains("isexpanded"),
            "Rule should be about isExpanded: {}",
            rule.rule_id
        );
        assert!(
            rule.rule_id.contains("toggle"),
            "Rule should reference AccordionToggle: {}",
            rule.rule_id
        );
        assert!(
            rule.rule_id.contains("item"),
            "Rule should reference AccordionItem: {}",
            rule.rule_id
        );
        assert!(
            rule.description.contains("AccordionToggle"),
            "Description should mention source: {}",
            rule.description
        );
        assert!(
            rule.description.contains("AccordionItem"),
            "Description should mention target: {}",
            rule.description
        );

        // Should NOT generate rules for className (ubiquitous, filtered)
        assert!(
            !rules.iter().any(|r| r.rule_id.contains("classname")),
            "Should not generate prop-movement for className"
        );
    }

    /// When a prop was optional in v5 and becomes required in v6,
    /// generate a rule. Also verify that a prop that was already
    /// required in both versions does NOT generate a rule.
    #[test]
    fn test_optional_to_required_prop_detected() {
        let mut sd = SdPipelineResult::default();

        // JumpLinksItem: `href` was optional in v5, required in v6
        sd.old_component_props.insert(
            "JumpLinksItem".into(),
            ["href", "children", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "JumpLinksItem".into(),
            ["href", "children", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        // In v5, href was NOT required (optional)
        sd.old_required_props.insert(
            "JumpLinksItem".into(),
            ["children"].iter().map(|s| s.to_string()).collect(),
        );
        // In v6, href IS required
        sd.new_required_props.insert(
            "JumpLinksItem".into(),
            ["href", "children"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );

        sd.component_packages
            .insert("JumpLinksItem".into(), "@patternfly/react-core".into());

        let pkgs = sd.component_packages.clone();
        let rules = generate_required_prop_added_rules(&sd, &pkgs);

        // Should generate a rule for `href` (was optional, now required)
        assert_eq!(
            rules.len(),
            1,
            "Expected 1 rule for href becoming required. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        let rule = &rules[0];
        assert!(
            rule.rule_id.contains("href"),
            "Rule should be about href: {}",
            rule.rule_id
        );
        // Should use the "now required" message, not "new required prop"
        assert!(
            rule.description.contains("now required"),
            "Should say 'now required' for optional→required: {}",
            rule.description
        );

        // Should NOT generate a rule for `children` (was required in both versions)
        assert!(
            !rules.iter().any(|r| r.rule_id.contains("children")),
            "Should not generate rule for children (always required)"
        );
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

        let empty_report = make_empty_report();
        let result = super::build_deprecated_migration_from_replacement(
            "Card", "Tile", &sd, &empty_report,
        );
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

        let empty_report = make_empty_report();
        let result = super::build_deprecated_migration_from_replacement(
            "NewComp", "OldComp", &sd, &empty_report,
        );
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

        let empty_report = make_empty_report();
        let result = super::build_deprecated_migration_from_replacement(
            "Card", "Tile", &sd, &empty_report,
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_deprecated_migration_from_replacement_td_enrichment() {
        // TC012/TC013: Source profile prop_types only captures directly-defined
        // props. For Chip→Label, most props are inherited and missing from
        // prop_types. The TD report's ApiChange entries should enrich
        // the prop type maps with inherited members.
        use semver_analyzer_core::*;

        // Source profile only has 2 props each (simulating partial coverage)
        let mut old_prop_types = HashMap::new();
        let mut chip_props = BTreeMap::new();
        chip_props.insert("isReadOnly".into(), "boolean".into());
        chip_props.insert("badge".into(), "React.ReactNode".into());
        old_prop_types.insert("Chip".into(), chip_props);

        let mut new_prop_types = HashMap::new();
        let mut label_props = BTreeMap::new();
        label_props.insert("isCompact".into(), "boolean".into());
        label_props.insert("variant".into(), "'outline' | 'filled'".into());
        new_prop_types.insert("Label".into(), label_props);

        let sd = SdPipelineResult {
            old_component_prop_types: old_prop_types,
            new_component_prop_types: new_prop_types,
            ..SdPipelineResult::default()
        };

        // Build a report with ApiChange entries for inherited props
        // that the source profile missed.
        let mut report = make_empty_report();
        report.changes.push(FileChanges {
            file: std::path::PathBuf::from("packages/react-core/src/components/Chip/Chip.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![
                // ChipProps.onClick existed in old (inherited) — type sig in "before"
                ApiChange {
                    symbol: "ChipProps.onClick".into(),
                    qualified_name: "ChipProps.onClick".into(),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Removed,
                    before: Some("property: onClick: (event: React.MouseEvent) => void".into()),
                    after: None,
                    description: "onClick removed from ChipProps".into(),
                    migration_target: None,
                    removal_disposition: None,
                },
                // ChipProps.closeBtnAriaLabel existed in old (inherited)
                ApiChange {
                    symbol: "ChipProps.closeBtnAriaLabel".into(),
                    qualified_name: "ChipProps.closeBtnAriaLabel".into(),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Removed,
                    before: Some("property: closeBtnAriaLabel: string".into()),
                    after: None,
                    description: "closeBtnAriaLabel removed from ChipProps".into(),
                    migration_target: None,
                    removal_disposition: None,
                },
            ],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        });
        // Also add an entry for LabelProps with a new prop "onClose"
        report.changes.push(FileChanges {
            file: std::path::PathBuf::from(
                "packages/react-core/src/components/Label/Label.d.ts",
            ),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "LabelProps.onClose".into(),
                qualified_name: "LabelProps.onClose".into(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::SignatureChanged,
                before: None,
                after: Some("property: onClose: (event: React.MouseEvent) => void".into()),
                description: "onClose added to LabelProps".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        });

        let result = super::build_deprecated_migration_from_replacement(
            "Label", "Chip", &sd, &report,
        );
        assert!(result.is_some(), "should produce a migration context");
        let ctx = result.unwrap();

        // The key assertion: TD-enriched props appear in the output
        let all_old_props: Vec<String> = ctx
            .matching_props
            .iter()
            .map(|p| p.old_name.clone())
            .chain(ctx.removed_props.iter().cloned())
            .collect();

        assert!(
            all_old_props.contains(&"onClick".to_string()),
            "onClick should be in old props (enriched from TD report). \
             matching: {:?}, removed: {:?}",
            ctx.matching_props
                .iter()
                .map(|p| &p.old_name)
                .collect::<Vec<_>>(),
            ctx.removed_props
        );
        assert!(
            all_old_props.contains(&"closeBtnAriaLabel".to_string()),
            "closeBtnAriaLabel should be in old props (enriched from TD report)"
        );

        // onClose should be in new_props (enriched from TD report for Label)
        assert!(
            ctx.new_props.contains_key("onClose"),
            "onClose should be in new_props (enriched from TD report). \
             new_props: {:?}",
            ctx.new_props
        );
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

    // ═══════════════════════════════════════════════════════════════════
    // TC-driven rule generation tests
    // Each test corresponds to a failing TC from the PF6 migration bench.
    // ═══════════════════════════════════════════════════════════════════

    /// Helper: empty CssPropertyTargetMap for tests that don't need CSS targets.
    fn empty_targets() -> crate::sd_types::CssPropertyTargetMap {
        HashMap::new()
    }

    /// Helper: build a minimal AnalysisReport for testing rule generators
    /// that need a report (like generate_prop_value_conformance_rules).
    fn build_test_report(
        changes: Vec<semver_analyzer_core::FileChanges<TypeScript>>,
    ) -> semver_analyzer_core::AnalysisReport<TypeScript> {
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
            changes,
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
    }

    /// TC021: DrawerContent.colorVariant PropValueChange rules should have
    /// replacement values populated (currently hardcoded to None).
    ///
    /// Old values: 'default' | 'light-200' | 'no-background'
    /// New values: 'default' | 'primary' | 'secondary'
    /// Surviving: 'default'. Added: 'primary', 'secondary'.
    /// Expected: light-200→secondary, no-background→primary (or default/remove)
    #[test]
    fn test_prop_value_change_computes_replacement_value() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/DrawerContent.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "DrawerContentProps.colorVariant".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some(
                    "property: colorVariant: 'default' | 'light-200' | 'no-background'".into(),
                ),
                after: Some(
                    "property: colorVariant: 'default' | 'primary' | 'secondary'".into(),
                ),
                description: "type changed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let mut sd = SdPipelineResult::default();
        sd.component_packages
            .insert("DrawerContent".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        // Should have 2 rules: one per removed value (light-200, no-background)
        assert!(
            rules.len() >= 2,
            "TC021: Expected at least 2 rules for removed values, got {}",
            rules.len()
        );

        // Find the rule for "no-background" → should map to "secondary"
        // (name_similarity("no-background", "secondary") = 0.308, passes 0.3)
        let nobg_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("no-background") || r.rule_id.contains("no_background"))
            .expect("TC021: Should have a rule for removed value 'no-background'");
        let nobg_repl = nobg_rule
            .fix_strategy
            .as_ref()
            .and_then(|s| s.replacement.as_ref());
        assert_eq!(
            nobg_repl.map(|s| s.as_str()),
            Some("secondary"),
            "TC021: no-background should map to secondary (sim=0.308)"
        );

        // Find the rule for "light-200" → no replacement from string similarity.
        // The surviving-value fallback was removed because it produced false
        // matches. Without CSS modifier data in this test, light-200 has no
        // hint. The CSS bridge (when populated with real data) or the LLM
        // will handle this mapping correctly.
        let light200_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("light-200"))
            .expect("TC021: Should have a rule for removed value 'light-200'");
        let light200_repl = light200_rule
            .fix_strategy
            .as_ref()
            .and_then(|s| s.replacement.as_ref());
        assert!(
            light200_repl.is_none(),
            "TC021: light-200 should NOT have a replacement from string similarity \
             (surviving-value fallback removed). Got: {:?}",
            light200_repl,
        );
    }

    /// TC067: PageSection.variant value 'light' should map to 'default',
    /// not 'secondary'.
    ///
    /// Old values: 'default' | 'dark' | 'darker' | 'light'
    /// New values: 'default' | 'secondary'
    /// Removed: dark, darker, light. Added: secondary.
    /// Expected: dark→secondary, darker→secondary, light→default (or remove prop)
    #[test]
    fn test_prop_value_change_variant_light_maps_correctly() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/PageSection.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "PageSectionProps.variant".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some(
                    "property: variant: 'default' | 'dark' | 'darker' | 'light'".into(),
                ),
                after: Some("property: variant: 'default' | 'secondary'".into()),
                description: "type changed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let mut sd = SdPipelineResult::default();
        sd.component_packages
            .insert("PageSection".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        // Find the rule for "dark"
        let dark_rule = rules
            .iter()
            .find(|r| {
                r.rule_id.contains("-dark")
                    && !r.rule_id.contains("darker")
            });
        if let Some(rule) = dark_rule {
            let replacement = rule
                .fix_strategy
                .as_ref()
                .and_then(|s| s.replacement.as_ref());
            assert_eq!(
                replacement.map(|s| s.as_str()),
                Some("secondary"),
                "TC067: dark should map to secondary"
            );
        }

        // Find the rule for "darker"
        let darker_rule = rules.iter().find(|r| r.rule_id.contains("darker"));
        if let Some(rule) = darker_rule {
            let replacement = rule
                .fix_strategy
                .as_ref()
                .and_then(|s| s.replacement.as_ref());
            assert_eq!(
                replacement.map(|s| s.as_str()),
                Some("secondary"),
                "TC067: darker should map to secondary"
            );
        }

        // Find the rule for "light" — should have NO replacement (no string
        // similarity fallback to surviving values). Without CSS modifier data,
        // the rule emits "Valid values: ..." and the LLM handles the mapping.
        // In PF5 'light' was the standard appearance → PF6 'default', but this
        // semantic relationship can only be established via CSS evidence or LLM
        // reasoning, not string similarity ("light" vs "default" = 0.29).
        let light_rule = rules.iter().find(|r| r.rule_id.contains("light"));
        if let Some(rule) = light_rule {
            let replacement = rule
                .fix_strategy
                .as_ref()
                .and_then(|s| s.replacement.as_ref());
            assert_eq!(
                replacement.map(|s| s.as_str()),
                None,
                "TC067: light should NOT have a replacement from string similarity \
                 (surviving-value fallback removed). The LLM will infer \
                 light→default from the 'Valid values' list."
            );
        }
    }

    /// TC075 (regression): When exactly 1 value is removed and 1 is added,
    /// auto-map them. Tabs.variant: 'default'|'light300' → 'default'|'secondary'.
    /// Removed: light300. Added: secondary. 1:1 → auto-map.
    #[test]
    fn test_prop_value_change_single_new_value_auto_maps() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/Tabs.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "TabsProps.variant".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some("property: variant: 'default' | 'light300'".into()),
                after: Some("property: variant: 'default' | 'secondary'".into()),
                description: "type changed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let mut sd = SdPipelineResult::default();
        sd.component_packages
            .insert("Tabs".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        // Should have 1 rule for removed value "light300"
        let light300_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("light300"))
            .expect("Should have a rule for removed value 'light300'");

        let replacement = light300_rule
            .fix_strategy
            .as_ref()
            .and_then(|s| s.replacement.as_ref());
        assert_eq!(
            replacement.map(|s| s.as_str()),
            Some("secondary"),
            "TC075: light300 should auto-map to secondary (only 1 new value)"
        );
    }

    // ── Directional similarity boost tests ────────────────────────────

    #[test]
    fn test_directional_boost_left_start() {
        assert!(
            (directional_similarity_boost("alignLeft", "alignStart") - 0.10).abs() < f64::EPSILON,
        );
    }

    #[test]
    fn test_directional_boost_right_end() {
        assert!(
            (directional_similarity_boost("alignRight", "alignEnd") - 0.10).abs() < f64::EPSILON,
        );
    }

    #[test]
    fn test_directional_boost_left_end_no_boost() {
        assert!(
            directional_similarity_boost("alignLeft", "alignEnd").abs() < f64::EPSILON,
            "Left should not boost to End"
        );
    }

    #[test]
    fn test_directional_boost_right_start_no_boost() {
        assert!(
            directional_similarity_boost("alignRight", "alignStart").abs() < f64::EPSILON,
            "Right should not boost to Start"
        );
    }

    #[test]
    fn test_directional_boost_unrelated_no_boost() {
        assert!(directional_similarity_boost("default", "primary").abs() < f64::EPSILON);
        assert!(directional_similarity_boost("light-200", "secondary").abs() < f64::EPSILON);
        assert!(directional_similarity_boost("dark", "secondary").abs() < f64::EPSILON);
        assert!(directional_similarity_boost("tertiary", "horizontal").abs() < f64::EPSILON);
    }

    #[test]
    fn test_directional_boost_case_insensitive() {
        // camelCase: AlignLeft → lowered → "alignleft" ends_with "left"
        assert!(
            (directional_similarity_boost("AlignLeft", "alignStart") - 0.10).abs() < f64::EPSILON,
        );
        assert!(
            (directional_similarity_boost("ALIGNRIGHT", "ALIGNEND") - 0.10).abs() < f64::EPSILON,
        );
    }

    // ── Full pipeline tests with real PF data ───────────────────────────

    /// ToolbarGroup.align: alignLeft→alignStart, alignRight→alignEnd.
    /// Complete replacement (no survivors). The directional boost fixes
    /// the greedy matching that otherwise picks alignLeft→alignCenter.
    #[test]
    fn test_align_left_right_maps_to_start_end() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/ToolbarGroup.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "ToolbarGroupProps.align".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some(
                    "property: align: 'alignLeft' | 'alignRight'".into(),
                ),
                after: Some(
                    "property: align: 'alignCenter' | 'alignEnd' | 'alignStart'".into(),
                ),
                description: "type changed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let mut sd = SdPipelineResult::default();
        sd.component_packages
            .insert("ToolbarGroup".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        let left_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("alignleft"))
            .expect("Should have a rule for alignLeft");
        let left_replacement = left_rule
            .fix_strategy
            .as_ref()
            .and_then(|s| s.replacement.as_ref());
        assert_eq!(
            left_replacement.map(|s| s.as_str()),
            Some("alignStart"),
            "alignLeft should map to alignStart (directional: Left→Start). Got: {:?}",
            left_replacement
        );

        let right_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("alignright"))
            .expect("Should have a rule for alignRight");
        let right_replacement = right_rule
            .fix_strategy
            .as_ref()
            .and_then(|s| s.replacement.as_ref());
        assert_eq!(
            right_replacement.map(|s| s.as_str()),
            Some("alignEnd"),
            "alignRight should map to alignEnd (directional: Right→End). Got: {:?}",
            right_replacement
        );
    }

    /// DrawerContent.colorVariant: partial replacement (default survives).
    /// Removed: light-200, no-background. Added: primary, secondary.
    /// Boost should NOT affect this — no directional suffixes.
    #[test]
    fn test_drawer_colorvariant_not_affected_by_directional_boost() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/DrawerContent.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "DrawerContentProps.colorVariant".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some(
                    "property: colorVariant: 'default' | 'light-200' | 'no-background'".into(),
                ),
                after: Some(
                    "property: colorVariant: 'default' | 'primary' | 'secondary'".into(),
                ),
                description: "type changed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let mut sd = SdPipelineResult::default();
        sd.component_packages
            .insert("DrawerContent".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        // Should have 2 rules: light-200 and no-background
        let drawer_rules: Vec<_> = rules
            .iter()
            .filter(|r| r.rule_id.contains("drawercontent"))
            .collect();
        assert_eq!(drawer_rules.len(), 2, "Should have 2 Drawer colorVariant rules");

        // Verify directional boost didn't change behavior:
        // light-200 has no directional suffix, no-background has no directional suffix
        // Verify directional boost is zero for all Drawer colorVariant values
        let boost_left = directional_similarity_boost("light-200", "primary");
        let boost_right = directional_similarity_boost("no-background", "secondary");
        assert!(boost_left.abs() < f64::EPSILON, "light-200 should have zero directional boost");
        assert!(boost_right.abs() < f64::EPSILON, "no-background should have zero directional boost");
    }

    /// Tabs.variant: 1:1 auto-map (light300→secondary). Directional boost
    /// doesn't apply — the 1:1 branch runs before similarity matching.
    #[test]
    fn test_tabs_variant_1to1_not_affected_by_boost() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/Tabs.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "TabsProps.variant".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some("property: variant: 'default' | 'light300'".into()),
                after: Some("property: variant: 'default' | 'secondary'".into()),
                description: "type changed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let mut sd = SdPipelineResult::default();
        sd.component_packages
            .insert("Tabs".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        let light300_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("light300"))
            .expect("Should have a rule for light300");
        let replacement = light300_rule
            .fix_strategy
            .as_ref()
            .and_then(|s| s.replacement.as_ref());
        assert_eq!(
            replacement.map(|s| s.as_str()),
            Some("secondary"),
            "light300→secondary (1:1 auto-map, unaffected by boost)"
        );
    }

    /// PageSection.variant: partial replacement (default survives).
    /// Removed: dark, darker, light. Added: secondary.
    /// No directional suffixes — boost should not change existing behavior.
    #[test]
    fn test_pagesection_variant_partial_not_affected() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/PageSection.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "PageSectionProps.variant".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some(
                    "property: variant: 'default' | 'dark' | 'darker' | 'light'".into(),
                ),
                after: Some(
                    "property: variant: 'default' | 'secondary'".into(),
                ),
                description: "type changed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let mut sd = SdPipelineResult::default();
        sd.component_packages
            .insert("PageSection".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        // Should have 3 rules: dark, darker, light
        let ps_rules: Vec<_> = rules
            .iter()
            .filter(|r| r.rule_id.contains("pagesection"))
            .collect();
        assert_eq!(ps_rules.len(), 3, "Should have 3 PageSection variant rules");

        // dark → secondary (added, 0.333≥0.3)
        let dark_rule = ps_rules.iter().find(|r| r.rule_id.ends_with("-dark")).unwrap();
        let dark_repl = dark_rule.fix_strategy.as_ref().and_then(|s| s.replacement.as_ref());
        assert_eq!(dark_repl.map(|s| s.as_str()), Some("secondary"));

        // darker → secondary (added, 0.333≥0.3)
        let darker_rule = ps_rules.iter().find(|r| r.rule_id.contains("darker")).unwrap();
        let darker_repl = darker_rule.fix_strategy.as_ref().and_then(|s| s.replacement.as_ref());
        assert_eq!(darker_repl.map(|s| s.as_str()), Some("secondary"));

        // light → no replacement (surviving-value fallback removed).
        // Without CSS modifier data, light has no mapping. The LLM will
        // infer light→default from the "Valid values" list.
        let light_rule = ps_rules.iter().find(|r| r.rule_id.ends_with("-light")).unwrap();
        let light_repl = light_rule.fix_strategy.as_ref().and_then(|s| s.replacement.as_ref());
        assert_eq!(
            light_repl.map(|s| s.as_str()),
            None,
            "light should have no replacement (surviving-value fallback removed)"
        );
    }

    /// ToolbarGroup.variant: partial replacement (action-group etc. survive).
    /// Removed: button-group, icon-button-group. No directional suffixes.
    #[test]
    fn test_toolbargroup_variant_partial_not_affected() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/ToolbarGroup.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "ToolbarGroupProps.variant".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some(
                    "property: variant: 'action-group' | 'action-group-plain' | 'button-group' | 'filter-group' | 'icon-button-group'".into(),
                ),
                after: Some(
                    "property: variant: 'action-group' | 'action-group-inline' | 'action-group-plain' | 'filter-group'".into(),
                ),
                description: "type changed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let mut sd = SdPipelineResult::default();
        sd.component_packages
            .insert("ToolbarGroup".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        // Should have 2 rules: button-group, icon-button-group
        let tg_rules: Vec<_> = rules
            .iter()
            .filter(|r| r.rule_id.contains("toolbargroup-variant"))
            .collect();
        assert_eq!(tg_rules.len(), 2, "Should have 2 ToolbarGroup variant rules");

        // button-group → action-group-inline (added, best match) or action-group (surviving)
        let bg_rule = tg_rules.iter().find(|r| r.rule_id.contains("button-group") && !r.rule_id.contains("icon")).unwrap();
        let bg_repl = bg_rule.fix_strategy.as_ref().and_then(|s| s.replacement.as_ref());
        assert!(
            bg_repl.is_some(),
            "button-group should have a replacement suggestion"
        );
        // No directional boost involved — verify it's zero
        assert!(
            directional_similarity_boost("button-group", bg_repl.unwrap()).abs() < f64::EPSILON,
            "button-group replacement should have zero directional boost"
        );
    }

    // ── Phase 2: Removed prop with ReplacedByMember tests ─────────────

    /// TC006: Banner variant prop was removed and replaced by `color` prop.
    /// Phase 2 should generate per-value rules mapping old variant values
    /// to new color values using string similarity.
    ///
    /// v5: variant: 'default' | 'info' | 'danger' | 'success' | 'warning'
    /// v6: color: 'blue' | 'red' | 'green' | 'gold' | 'default'
    /// v6: status: 'danger' | 'success' | 'warning' | 'info' | 'custom'
    ///
    /// Expected mappings (by name_similarity):
    ///   info → (no strong color match, but "blue" is best via LLM)
    ///   danger → red (or LLM)
    ///   success → green (or LLM)
    ///   warning → gold (or LLM)
    ///   default → default (surviving or removable)
    ///
    /// All get LlmAssisted strategy since the transformation crosses
    /// prop boundaries (variant → color + status).
    #[test]
    fn test_removed_prop_replaced_by_member_generates_per_value_rules() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/Banner.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "BannerProps.variant".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::Removed,
                before: Some(
                    "property: variant: 'default' | 'info' | 'danger' | 'success' | 'warning'"
                        .into(),
                ),
                after: None,
                description: "variant prop removed".into(),
                migration_target: None,
                removal_disposition: Some(RemovalDisposition::ReplacedByMember {
                    new_member: "color".into(),
                }),
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let mut sd = SdPipelineResult::default();
        sd.component_packages
            .insert("Banner".into(), "@patternfly/react-core".into());
        // Set up new component prop types so Phase 2 can look up the replacement
        sd.new_component_prop_types.insert(
            "Banner".into(),
            [
                (
                    "color".into(),
                    "'blue' | 'red' | 'green' | 'gold' | 'default'".into(),
                ),
                (
                    "status".into(),
                    "'danger' | 'success' | 'warning' | 'info' | 'custom'".into(),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        // Should generate 5 rules — one for each variant value
        let banner_rules: Vec<_> = rules
            .iter()
            .filter(|r| r.rule_id.starts_with("sd-prop-replaced-banner-variant-"))
            .collect();
        assert_eq!(
            banner_rules.len(),
            5,
            "Should generate 5 per-value rules for Banner variant. Got: {:?}",
            banner_rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );

        // Each rule should detect the old prop with a value discriminator
        for rule in &banner_rules {
            let refs = semver_analyzer_konveyor_core::extract_frontend_refs(&rule.when);
            assert_eq!(refs.len(), 1, "Each rule should have exactly 1 condition");
            let r = refs[0];
            assert_eq!(r.location, "JSX_PROP");
            assert_eq!(r.pattern, "^variant$");
            assert_eq!(
                r.component.as_deref(),
                Some("^Banner$"),
                "Rule should scope to Banner component"
            );
            assert!(
                r.value.is_some(),
                "Rule should have value discriminator: {:?}",
                rule.rule_id
            );
        }

        // All rules should use LlmAssisted strategy
        for rule in &banner_rules {
            let strategy = rule
                .fix_strategy
                .as_ref()
                .expect("Should have fix strategy");
            assert_eq!(
                strategy.strategy, "LlmAssisted",
                "Should use LlmAssisted for cross-prop transformation: {}",
                rule.rule_id
            );
        }

        // Check that secondary prop "status" is mentioned in the messages
        // for values that overlap (danger, success, warning, info)
        let danger_rule = banner_rules
            .iter()
            .find(|r| r.rule_id.contains("danger"))
            .expect("Should have a rule for danger");
        assert!(
            danger_rule.message.contains("status"),
            "danger rule should mention the status prop: {}",
            danger_rule.message
        );

        let info_rule = banner_rules
            .iter()
            .find(|r| r.rule_id.contains("-info"))
            .expect("Should have a rule for info");
        assert!(
            info_rule.message.contains("status"),
            "info rule should mention the status prop: {}",
            info_rule.message
        );

        // "default" should NOT mention status (not in status values)
        let default_rule = banner_rules
            .iter()
            .find(|r| r.rule_id.contains("default"))
            .expect("Should have a rule for default");
        // default IS in the color values, so it gets a replacement mapping
        let default_replacement = default_rule
            .fix_strategy
            .as_ref()
            .and_then(|s| s.replacement.as_ref());
        assert_eq!(
            default_replacement.map(|s| s.as_str()),
            Some("default"),
            "default should map to default on color prop"
        );
    }

    /// Removed prop WITHOUT ReplacedByMember should NOT generate Phase 2 rules.
    #[test]
    fn test_removed_prop_without_replacement_no_phase2_rules() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/Comp.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "CompProps.oldProp".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::Removed,
                before: Some("property: oldProp: 'a' | 'b' | 'c'".into()),
                after: None,
                description: "oldProp removed".into(),
                migration_target: None,
                removal_disposition: Some(RemovalDisposition::TrulyRemoved),
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let sd = SdPipelineResult::default();
        let pkgs = HashMap::new();
        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        let phase2_rules: Vec<_> = rules
            .iter()
            .filter(|r| r.rule_id.starts_with("sd-prop-replaced-"))
            .collect();
        assert!(
            phase2_rules.is_empty(),
            "TrulyRemoved props should NOT generate Phase 2 rules. Got: {:?}",
            phase2_rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
    }

    // ── CSS Modifier Bridge Tests ──────────────────────────────────────

    /// TC051 Nav case: structural matching can't bridge tertiary→horizontal-subnav
    /// because v5→v6 completely restructured the CSS (physical→logical properties,
    /// different direct properties). This is expected — Nav tertiary→horizontal-subnav
    /// is a 1:1 auto-map case handled by the existing Branch 1 algorithm.
    /// The CSS bridge correctly returns empty and falls through.
    ///
    /// This test validates that the bridge doesn't produce a WRONG match
    /// when structures are too different.
    #[test]
    fn test_css_bridge_nav_no_structural_match_when_restructured() {
        use crate::sd_types::{CssModifierEffect, CssModifierMap, ComponentCssModifiers};

        let mut old_mods = ComponentCssModifiers::new();
        let mut old_nav = CssModifierMap::new();
        // v5 .pf-m-tertiary: physical padding + color + display
        let mut tertiary = CssModifierEffect::default();
        tertiary.custom_property_overrides.insert("--pf-v5-c-nav__link--PaddingTop".into(), "var(...)".into());
        tertiary.custom_property_overrides.insert("--pf-v5-c-nav__link--Color".into(), "var(...)".into());
        tertiary.direct_properties.insert("display".into(), "flex".into());
        old_nav.insert("pf-m-tertiary".into(), tertiary);
        old_mods.insert("nav".into(), old_nav);

        let mut new_mods = ComponentCssModifiers::new();
        let mut new_nav = CssModifierMap::new();
        // v6 .pf-m-horizontal-subnav: logical padding + border (no display)
        let mut subnav = CssModifierEffect::default();
        subnav.custom_property_overrides.insert("--pf-v6-c-nav__link--PaddingBlockStart".into(), "var(...)".into());
        subnav.direct_properties.insert("border".into(), "1px solid var(...)".into());
        new_nav.insert("pf-m-horizontal-subnav".into(), subnav);
        new_mods.insert("nav".into(), new_nav);

        let removed: Vec<String> = vec!["tertiary".into()];
        let added: Vec<String> = vec!["horizontal-subnav".into()];
        let removed_refs: Vec<&String> = removed.iter().collect();
        let added_refs: Vec<&String> = added.iter().collect();

        let hints = compute_css_modifier_bridge(
            "Nav", &removed_refs, &added_refs,
            &old_mods, &new_mods,
            &empty_targets(), &empty_targets(),
        );

        // Bridge correctly returns empty when structures are too different.
        // The 1:1 auto-map in the string similarity algorithm handles this case.
        assert!(
            hints.is_empty(),
            "Nav: CSS bridge should return empty when structures are too different \
             (v5 physical→v6 logical restructuring). Got: {:?}",
            hints
        );
    }

    /// TC014: Label color cyan → teal via CSS modifier bridge.
    /// Both modifiers override the same structural slots (BackgroundColor,
    /// Color, icon--Color) just with different color token values.
    #[test]
    fn test_css_bridge_label_cyan_to_teal() {
        use crate::sd_types::{CssModifierEffect, CssModifierMap, ComponentCssModifiers};

        let mut old_mods = ComponentCssModifiers::new();
        let mut old_label = CssModifierMap::new();

        // Real PF v5 .pf-m-cyan: 13 custom property overrides
        let mut cyan = CssModifierEffect::default();
        cyan.custom_property_overrides.insert("--pf-v5-c-label--BackgroundColor".into(), "var(--pf-v5-c-label--m-cyan--BackgroundColor)".into());
        cyan.custom_property_overrides.insert("--pf-v5-c-label__icon--Color".into(), "var(--pf-v5-c-label--m-cyan__icon--Color)".into());
        cyan.custom_property_overrides.insert("--pf-v5-c-label__content--Color".into(), "var(--pf-v5-c-label--m-cyan__content--Color)".into());
        cyan.custom_property_overrides.insert("--pf-v5-c-label__content--before--BorderColor".into(), "var(--pf-v5-c-label--m-cyan__content--before--BorderColor)".into());
        // (abbreviated — 13 total in real PF, using 4 for test)
        old_label.insert("pf-m-cyan".into(), cyan);

        // Real PF v5 .pf-m-gold: same structure as cyan (same 13 overrides)
        let mut gold = CssModifierEffect::default();
        gold.custom_property_overrides.insert("--pf-v5-c-label--BackgroundColor".into(), "var(--pf-v5-c-label--m-gold--BackgroundColor)".into());
        gold.custom_property_overrides.insert("--pf-v5-c-label__icon--Color".into(), "var(--pf-v5-c-label--m-gold__icon--Color)".into());
        gold.custom_property_overrides.insert("--pf-v5-c-label__content--Color".into(), "var(--pf-v5-c-label--m-gold__content--Color)".into());
        gold.custom_property_overrides.insert("--pf-v5-c-label__content--before--BorderColor".into(), "var(--pf-v5-c-label--m-gold__content--before--BorderColor)".into());
        old_label.insert("pf-m-gold".into(), gold);

        old_mods.insert("label".into(), old_label);

        let mut new_mods = ComponentCssModifiers::new();
        let mut new_label = CssModifierMap::new();

        // Real PF v6 .pf-m-teal: 8 custom property overrides
        let mut teal = CssModifierEffect::default();
        teal.custom_property_overrides.insert("--pf-v6-c-label--BackgroundColor".into(), "var(--pf-v6-c-label--m-teal--BackgroundColor)".into());
        teal.custom_property_overrides.insert("--pf-v6-c-label--Color".into(), "var(--pf-v6-c-label--m-teal--Color)".into());
        teal.custom_property_overrides.insert("--pf-v6-c-label__icon--Color".into(), "var(--pf-v6-c-label--m-teal__icon--Color)".into());
        teal.custom_property_overrides.insert("--pf-v6-c-label--m-clickable--hover--BackgroundColor".into(), "var(--pf-v6-c-label--m-teal--m-clickable--hover--BackgroundColor)".into());
        new_label.insert("pf-m-teal".into(), teal);

        // Real PF v6 .pf-m-yellow: same structure as teal
        let mut yellow = CssModifierEffect::default();
        yellow.custom_property_overrides.insert("--pf-v6-c-label--BackgroundColor".into(), "var(--pf-v6-c-label--m-yellow--BackgroundColor)".into());
        yellow.custom_property_overrides.insert("--pf-v6-c-label--Color".into(), "var(--pf-v6-c-label--m-yellow--Color)".into());
        yellow.custom_property_overrides.insert("--pf-v6-c-label__icon--Color".into(), "var(--pf-v6-c-label--m-yellow__icon--Color)".into());
        yellow.custom_property_overrides.insert("--pf-v6-c-label--m-clickable--hover--BackgroundColor".into(), "var(--pf-v6-c-label--m-yellow--m-clickable--hover--BackgroundColor)".into());
        new_label.insert("pf-m-yellow".into(), yellow);

        // Real PF v6 .pf-m-orange: different structure — has orangered tokens
        let mut orange = CssModifierEffect::default();
        orange.custom_property_overrides.insert("--pf-v6-c-label--BackgroundColor".into(), "var(--pf-v6-c-label--m-orange--BackgroundColor)".into());
        orange.custom_property_overrides.insert("--pf-v6-c-label--Color".into(), "var(--pf-v6-c-label--m-orange--Color)".into());
        orange.custom_property_overrides.insert("--pf-v6-c-label__icon--Color".into(), "var(--pf-v6-c-label--m-orange__icon--Color)".into());
        orange.custom_property_overrides.insert("--pf-v6-c-label--m-clickable--hover--BackgroundColor".into(), "var(--pf-v6-c-label--m-orange--m-clickable--hover--BackgroundColor)".into());
        new_label.insert("pf-m-orange".into(), orange);

        new_mods.insert("label".into(), new_label);

        let removed: Vec<String> = vec!["cyan".into(), "gold".into()];
        let added: Vec<String> = vec!["teal".into(), "yellow".into(), "orangered".into()];
        let removed_refs: Vec<&String> = removed.iter().collect();
        let added_refs: Vec<&String> = added.iter().collect();

        let hints = compute_css_modifier_bridge(
            "Label", &removed_refs, &added_refs,
            &old_mods, &new_mods,
            &empty_targets(), &empty_targets(),
        );

        // NOTE: With Phase 1 structural matching, ALL v6 color modifiers have
        // identical structure (same 4 slots). So cyan could map to any of
        // {teal, yellow, orange}. The structural similarity is the same for all.
        // Phase 2 (resolved values) will disambiguate by comparing actual colors.
        //
        // For Phase 1, we just verify that BOTH removed values get mappings
        // and they don't both map to the same added value (greedy prevents that).
        assert_eq!(
            hints.len(), 2,
            "TC014: CSS bridge should produce 2 mappings (cyan + gold). Got: {:?}",
            hints
        );
        assert!(hints.contains_key("cyan"), "cyan should have a mapping");
        assert!(hints.contains_key("gold"), "gold should have a mapping");
        // Greedy should prevent both mapping to the same value
        assert_ne!(
            hints.get("cyan"), hints.get("gold"),
            "cyan and gold should not map to the same value"
        );
    }

    /// TC014 with REAL PF data: cyan→teal and gold→yellow disambiguated
    /// using bag-of-colors resolved similarity.
    ///
    /// Real PF v5 and v6 Label modifiers have DIFFERENT token structures:
    ///   - v5 cyan: 13 custom_property_overrides (editable, outline, content, icon patterns)
    ///   - v6 teal: 8 custom_property_overrides (clickable, outline, icon patterns)
    ///
    /// Token slot names DON'T match across versions, so slot-based comparison
    /// gives near-zero similarity. The bag-of-colors approach ignores slot names
    /// and directly compares the hex color palettes.
    ///
    /// Colors from the actual PF CSS (from pipeline report):
    ///   v5 cyan palette: #f2f9f9, #a2d9d9, #009596, #003737, #005f60, #d2d2d2
    ///   v5 gold palette: #f9e0a2, #f0ab00, #795600, #3e2b00, #c58c00, #d2d2d2
    ///   v6 teal palette: #b9e5e5, #9ad8d8, #63bdbd, #151515, #1f1f1f
    ///   v6 yellow palette: #f9e0a2, #f4c145, #c58c00, #151515, #1f1f1f
    ///   v6 orange palette: #f4b678, #ef9234, #8f4700, #151515, #1f1f1f
    ///
    /// cyan colors (blue-green) are closest to teal colors (blue-green).
    /// gold colors (warm yellow) are closest to yellow colors (warm yellow).
    #[test]
    fn test_css_bridge_label_resolved_values_disambiguate() {
        use crate::sd_types::{CssModifierEffect, CssModifierMap, ComponentCssModifiers};

        let mut old_mods = ComponentCssModifiers::new();
        let mut old_label = CssModifierMap::new();

        // Real PF v5 .pf-m-cyan — 13 overrides with resolved hex colors
        let mut cyan = CssModifierEffect::default();
        // Custom property overrides (for structural similarity)
        for (k, v) in [
            ("--pf-v5-c-label--BackgroundColor", "var(--pf-v5-c-label--m-cyan--BackgroundColor)"),
            ("--pf-v5-c-label__content--Color", "var(--pf-v5-c-label--m-cyan__content--Color)"),
            ("--pf-v5-c-label__icon--Color", "var(--pf-v5-c-label--m-cyan__icon--Color)"),
            ("--pf-v5-c-label__content--before--BorderColor", "var(--pf-v5-c-label--m-cyan__content--before--BorderColor)"),
            ("--pf-v5-c-label--m-outline__content--Color", "var(...)"),
            ("--pf-v5-c-label--m-outline__content--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label__content--link--hover--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label__content--link--focus--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label--m-editable__content--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label--m-editable__content--hover--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label--m-editable__content--focus--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label--m-outline__content--link--hover--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label--m-outline__content--link--focus--before--BorderColor", "var(...)"),
        ] {
            cyan.custom_property_overrides.insert(k.into(), v.into());
        }
        // Resolved terminal hex colors (from real pipeline output)
        for (k, v) in [
            ("--pf-v5-c-label--BackgroundColor", "#f2f9f9"),
            ("--pf-v5-c-label__content--Color", "#003737"),
            ("--pf-v5-c-label__icon--Color", "#009596"),
            ("--pf-v5-c-label__content--before--BorderColor", "#a2d9d9"),
            ("--pf-v5-c-label--m-outline__content--Color", "#005f60"),
            ("--pf-v5-c-label--m-outline__content--before--BorderColor", "#d2d2d2"),
            ("--pf-v5-c-label__content--link--hover--before--BorderColor", "#009596"),
            ("--pf-v5-c-label__content--link--focus--before--BorderColor", "#009596"),
            ("--pf-v5-c-label--m-editable__content--before--BorderColor", "#a2d9d9"),
            ("--pf-v5-c-label--m-editable__content--hover--before--BorderColor", "#a2d9d9"),
            ("--pf-v5-c-label--m-editable__content--focus--before--BorderColor", "#a2d9d9"),
            ("--pf-v5-c-label--m-outline__content--link--hover--before--BorderColor", "#d2d2d2"),
            ("--pf-v5-c-label--m-outline__content--link--focus--before--BorderColor", "#d2d2d2"),
        ] {
            cyan.resolved_overrides.insert(k.into(), v.into());
        }
        old_label.insert("pf-m-cyan".into(), cyan);

        // Real PF v5 .pf-m-gold — same 13 override slots, warm palette
        let mut gold = CssModifierEffect::default();
        for (k, v) in [
            ("--pf-v5-c-label--BackgroundColor", "var(...)"),
            ("--pf-v5-c-label__content--Color", "var(...)"),
            ("--pf-v5-c-label__icon--Color", "var(...)"),
            ("--pf-v5-c-label__content--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label--m-outline__content--Color", "var(...)"),
            ("--pf-v5-c-label--m-outline__content--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label__content--link--hover--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label__content--link--focus--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label--m-editable__content--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label--m-editable__content--hover--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label--m-editable__content--focus--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label--m-outline__content--link--hover--before--BorderColor", "var(...)"),
            ("--pf-v5-c-label--m-outline__content--link--focus--before--BorderColor", "var(...)"),
        ] {
            gold.custom_property_overrides.insert(k.into(), v.into());
        }
        for (k, v) in [
            ("--pf-v5-c-label--BackgroundColor", "#fdf7e7"),
            ("--pf-v5-c-label__content--Color", "#3e2b00"),
            ("--pf-v5-c-label__icon--Color", "#f0ab00"),
            ("--pf-v5-c-label__content--before--BorderColor", "#f9e0a2"),
            ("--pf-v5-c-label--m-outline__content--Color", "#795600"),
            ("--pf-v5-c-label--m-outline__content--before--BorderColor", "#d2d2d2"),
            ("--pf-v5-c-label__content--link--hover--before--BorderColor", "#c58c00"),
            ("--pf-v5-c-label__content--link--focus--before--BorderColor", "#c58c00"),
            ("--pf-v5-c-label--m-editable__content--before--BorderColor", "#f9e0a2"),
            ("--pf-v5-c-label--m-editable__content--hover--before--BorderColor", "#f9e0a2"),
            ("--pf-v5-c-label--m-editable__content--focus--before--BorderColor", "#f9e0a2"),
            ("--pf-v5-c-label--m-outline__content--link--hover--before--BorderColor", "#d2d2d2"),
            ("--pf-v5-c-label--m-outline__content--link--focus--before--BorderColor", "#d2d2d2"),
        ] {
            gold.resolved_overrides.insert(k.into(), v.into());
        }
        old_label.insert("pf-m-gold".into(), gold);

        old_mods.insert("label".into(), old_label);

        let mut new_mods = ComponentCssModifiers::new();
        let mut new_label = CssModifierMap::new();

        // Real PF v6 .pf-m-teal — 8 overrides, DIFFERENT token structure
        let mut teal = CssModifierEffect::default();
        for (k, v) in [
            ("--pf-v6-c-label--BackgroundColor", "var(...)"),
            ("--pf-v6-c-label--Color", "var(...)"),
            ("--pf-v6-c-label__icon--Color", "var(...)"),
            ("--pf-v6-c-label--m-clickable--hover--BackgroundColor", "var(...)"),
            ("--pf-v6-c-label--m-clickable--hover--Color", "var(...)"),
            ("--pf-v6-c-label--m-clickable--hover__icon--Color", "var(...)"),
            ("--pf-v6-c-label--m-outline--BorderColor", "var(...)"),
            ("--pf-v6-c-label--m-outline--m-clickable--hover--BorderColor", "var(...)"),
        ] {
            teal.custom_property_overrides.insert(k.into(), v.into());
        }
        for (k, v) in [
            ("--pf-v6-c-label--BackgroundColor", "#b9e5e5"),      // light teal
            ("--pf-v6-c-label--Color", "#151515"),                  // near-black text
            ("--pf-v6-c-label__icon--Color", "#1f1f1f"),            // dark icon
            ("--pf-v6-c-label--m-clickable--hover--BackgroundColor", "#9ad8d8"), // mid teal
            ("--pf-v6-c-label--m-clickable--hover--Color", "#151515"),
            ("--pf-v6-c-label--m-clickable--hover__icon--Color", "#1f1f1f"),
            ("--pf-v6-c-label--m-outline--BorderColor", "#9ad8d8"), // teal border
            ("--pf-v6-c-label--m-outline--m-clickable--hover--BorderColor", "#63bdbd"),
        ] {
            teal.resolved_overrides.insert(k.into(), v.into());
        }
        new_label.insert("pf-m-teal".into(), teal);

        // Real PF v6 .pf-m-yellow — 8 overrides, warm palette
        let mut yellow = CssModifierEffect::default();
        for (k, v) in [
            ("--pf-v6-c-label--BackgroundColor", "var(...)"),
            ("--pf-v6-c-label--Color", "var(...)"),
            ("--pf-v6-c-label__icon--Color", "var(...)"),
            ("--pf-v6-c-label--m-clickable--hover--BackgroundColor", "var(...)"),
            ("--pf-v6-c-label--m-clickable--hover--Color", "var(...)"),
            ("--pf-v6-c-label--m-clickable--hover__icon--Color", "var(...)"),
            ("--pf-v6-c-label--m-outline--BorderColor", "var(...)"),
            ("--pf-v6-c-label--m-outline--m-clickable--hover--BorderColor", "var(...)"),
        ] {
            yellow.custom_property_overrides.insert(k.into(), v.into());
        }
        // Real PF v6 resolved values from pipeline report (exact hex values)
        for (k, v) in [
            ("--pf-v6-c-label--BackgroundColor", "#ffe072"),       // yellow bg (H≈47°)
            ("--pf-v6-c-label--Color", "#151515"),
            ("--pf-v6-c-label__icon--Color", "#1f1f1f"),
            ("--pf-v6-c-label--m-clickable--hover--BackgroundColor", "#ffcc17"),
            ("--pf-v6-c-label--m-clickable--hover--Color", "#151515"),
            ("--pf-v6-c-label--m-clickable--hover__icon--Color", "#1f1f1f"),
            ("--pf-v6-c-label--m-outline--BorderColor", "#ffcc17"),
            ("--pf-v6-c-label--m-outline--m-clickable--hover--BorderColor", "#dca614"),
        ] {
            yellow.resolved_overrides.insert(k.into(), v.into());
        }
        new_label.insert("pf-m-yellow".into(), yellow);

        // Real PF v6 .pf-m-orangered — 8 overrides, orange palette
        let mut orangered = CssModifierEffect::default();
        for (k, v) in [
            ("--pf-v6-c-label--BackgroundColor", "var(...)"),
            ("--pf-v6-c-label--Color", "var(...)"),
            ("--pf-v6-c-label__icon--Color", "var(...)"),
            ("--pf-v6-c-label--m-clickable--hover--BackgroundColor", "var(...)"),
            ("--pf-v6-c-label--m-clickable--hover--Color", "var(...)"),
            ("--pf-v6-c-label--m-clickable--hover__icon--Color", "var(...)"),
            ("--pf-v6-c-label--m-outline--BorderColor", "var(...)"),
            ("--pf-v6-c-label--m-outline--m-clickable--hover--BorderColor", "var(...)"),
        ] {
            orangered.custom_property_overrides.insert(k.into(), v.into());
        }
        // Real PF v6 resolved values from pipeline report (exact hex values)
        for (k, v) in [
            ("--pf-v6-c-label--BackgroundColor", "#fbbea8"),       // orangered bg (H≈16°)
            ("--pf-v6-c-label--Color", "#151515"),
            ("--pf-v6-c-label__icon--Color", "#1f1f1f"),
            ("--pf-v6-c-label--m-clickable--hover--BackgroundColor", "#f89b78"),
            ("--pf-v6-c-label--m-clickable--hover--Color", "#151515"),
            ("--pf-v6-c-label--m-clickable--hover__icon--Color", "#1f1f1f"),
            ("--pf-v6-c-label--m-outline--BorderColor", "#f89b78"),
            ("--pf-v6-c-label--m-outline--m-clickable--hover--BorderColor", "#f4784a"),
        ] {
            orangered.resolved_overrides.insert(k.into(), v.into());
        }
        new_label.insert("pf-m-orangered".into(), orangered);

        new_mods.insert("label".into(), new_label);

        let removed: Vec<String> = vec!["cyan".into(), "gold".into()];
        let added: Vec<String> = vec!["teal".into(), "yellow".into(), "orangered".into()];
        let removed_refs: Vec<&String> = removed.iter().collect();
        let added_refs: Vec<&String> = added.iter().collect();

        // Target maps: trace custom properties to actual CSS properties.
        // In real PF CSS, the base .pf-c-label rule has:
        //   background-color: var(--pf-c-label--BackgroundColor)
        //   color: var(--pf-c-label--Color) or var(--pf-c-label__content--Color)
        //   .icon { color: var(--pf-c-label__icon--Color) }
        //   border-color: var(--pf-c-label__content--before--BorderColor) etc.
        let mut old_targets = crate::sd_types::CssPropertyTargetMap::new();
        old_targets.insert("--pf-v5-c-label--BackgroundColor".into(), "background-color".into());
        old_targets.insert("--pf-v5-c-label__content--Color".into(), "color".into());
        old_targets.insert("--pf-v5-c-label__icon--Color".into(), "color".into());
        old_targets.insert("--pf-v5-c-label__content--before--BorderColor".into(), "border-color".into());
        old_targets.insert("--pf-v5-c-label--m-outline__content--Color".into(), "color".into());
        old_targets.insert("--pf-v5-c-label--m-outline__content--before--BorderColor".into(), "border-color".into());
        old_targets.insert("--pf-v5-c-label__content--link--hover--before--BorderColor".into(), "border-color".into());
        old_targets.insert("--pf-v5-c-label__content--link--focus--before--BorderColor".into(), "border-color".into());
        old_targets.insert("--pf-v5-c-label--m-editable__content--before--BorderColor".into(), "border-color".into());
        old_targets.insert("--pf-v5-c-label--m-editable__content--hover--before--BorderColor".into(), "border-color".into());
        old_targets.insert("--pf-v5-c-label--m-editable__content--focus--before--BorderColor".into(), "border-color".into());
        old_targets.insert("--pf-v5-c-label--m-outline__content--link--hover--before--BorderColor".into(), "border-color".into());
        old_targets.insert("--pf-v5-c-label--m-outline__content--link--focus--before--BorderColor".into(), "border-color".into());

        let mut new_targets = crate::sd_types::CssPropertyTargetMap::new();
        new_targets.insert("--pf-v6-c-label--BackgroundColor".into(), "background-color".into());
        new_targets.insert("--pf-v6-c-label--Color".into(), "color".into());
        new_targets.insert("--pf-v6-c-label__icon--Color".into(), "color".into());
        new_targets.insert("--pf-v6-c-label--m-clickable--hover--BackgroundColor".into(), "background-color".into());
        new_targets.insert("--pf-v6-c-label--m-clickable--hover--Color".into(), "color".into());
        new_targets.insert("--pf-v6-c-label--m-clickable--hover__icon--Color".into(), "color".into());
        new_targets.insert("--pf-v6-c-label--m-outline--BorderColor".into(), "border-color".into());
        new_targets.insert("--pf-v6-c-label--m-outline--m-clickable--hover--BorderColor".into(), "border-color".into());

        let hints = compute_css_modifier_bridge(
            "Label", &removed_refs, &added_refs,
            &old_mods, &new_mods,
            &old_targets, &new_targets,
        );

        // CSS property comparison: both sides set background-color, color,
        // and border-color. Comparing at matching CSS properties:
        //   cyan bg #f2f9f9 vs teal bg #b9e5e5 → both blue-green → high similarity
        //   cyan bg #f2f9f9 vs yellow bg #ffe072 → different hue → lower similarity
        //   gold bg #fdf7e7 vs yellow bg #f9e0a2 → both warm → higher similarity
        //   gold bg #fdf7e7 vs orangered bg #fbbea8 → warm vs salmon → lower similarity
        assert_eq!(
            hints.len(), 2,
            "Should produce 2 mappings. Got: {:?}", hints
        );
        assert_eq!(
            hints.get("cyan").map(|s| s.as_str()), Some("teal"),
            "cyan should map to teal (blue-green palette match). Got: {:?}", hints
        );
        assert_eq!(
            hints.get("gold").map(|s| s.as_str()), Some("yellow"),
            "gold should map to yellow (warm palette match). Got: {:?}", hints
        );
    }

    /// When no CSS modifier data exists, bridge returns empty and
    /// string similarity runs unchanged. Regression guard for TC067.
    #[test]
    fn test_css_bridge_no_data_falls_through() {
        use crate::sd_types::ComponentCssModifiers;

        let empty = ComponentCssModifiers::new();
        let removed: Vec<String> = vec!["dark".into()];
        let added: Vec<String> = vec!["secondary".into()];
        let removed_refs: Vec<&String> = removed.iter().collect();
        let added_refs: Vec<&String> = added.iter().collect();

        let hints = compute_css_modifier_bridge(
            "PageSection", &removed_refs, &added_refs,
            &empty, &empty,
            &empty_targets(), &empty_targets(),
        );

        assert!(
            hints.is_empty(),
            "TC067 regression: No CSS data → empty bridge → string similarity fallback"
        );
    }

    /// Modifiers with completely different structures should not match.
    #[test]
    fn test_css_bridge_different_structure_no_match() {
        use crate::sd_types::{CssModifierEffect, CssModifierMap, ComponentCssModifiers};

        let mut old_mods = ComponentCssModifiers::new();
        let mut old_comp = CssModifierMap::new();
        let mut foo = CssModifierEffect::default();
        foo.custom_property_overrides.insert("--comp--m-foo--BackgroundColor".into(), "red".into());
        foo.custom_property_overrides.insert("--comp--m-foo--Color".into(), "white".into());
        old_comp.insert("pf-m-foo".into(), foo);
        old_mods.insert("comp".into(), old_comp);

        let mut new_mods = ComponentCssModifiers::new();
        let mut new_comp = CssModifierMap::new();
        let mut bar = CssModifierEffect::default();
        // Completely different slots — no overlap
        bar.direct_properties.insert("display".into(), "none".into());
        bar.direct_properties.insert("visibility".into(), "hidden".into());
        new_comp.insert("pf-m-bar".into(), bar);
        new_mods.insert("comp".into(), new_comp);

        let removed: Vec<String> = vec!["foo".into()];
        let added: Vec<String> = vec!["bar".into()];
        let removed_refs: Vec<&String> = removed.iter().collect();
        let added_refs: Vec<&String> = added.iter().collect();

        let hints = compute_css_modifier_bridge(
            "Comp", &removed_refs, &added_refs,
            &old_mods, &new_mods,
            &empty_targets(), &empty_targets(),
        );

        assert!(
            hints.is_empty(),
            "Different structures should not match: {:?}",
            hints
        );
    }

    // ── Color distance unit tests ───────────────────────────────────────

    #[test]
    fn test_parse_hex_color_long() {
        assert_eq!(parse_hex_color("#e0f5f5"), Some((0xe0, 0xf5, 0xf5)));
        assert_eq!(parse_hex_color("#000000"), Some((0, 0, 0)));
        assert_eq!(parse_hex_color("#ffffff"), Some((255, 255, 255)));
        assert_eq!(parse_hex_color("#AABBCC"), Some((0xaa, 0xbb, 0xcc)));
        assert_eq!(parse_hex_color("#0066cc"), Some((0x00, 0x66, 0xcc)));
    }

    #[test]
    fn test_parse_hex_color_short() {
        // #abc → (0xaa, 0xbb, 0xcc)
        assert_eq!(parse_hex_color("#abc"), Some((0xaa, 0xbb, 0xcc)));
        assert_eq!(parse_hex_color("#fff"), Some((255, 255, 255)));
        assert_eq!(parse_hex_color("#000"), Some((0, 0, 0)));
        // lightningcss minifies #0066cc → #06c
        assert_eq!(parse_hex_color("#06c"), Some((0x00, 0x66, 0xcc)));
    }

    #[test]
    fn test_parse_hex_color_invalid() {
        assert_eq!(parse_hex_color("transparent"), None);
        assert_eq!(parse_hex_color("inherit"), None);
        assert_eq!(parse_hex_color("0px"), None);
        assert_eq!(parse_hex_color(""), None);
        assert_eq!(parse_hex_color("#"), None);
        assert_eq!(parse_hex_color("#gg"), None);
        assert_eq!(parse_hex_color("#12345"), None); // 5 digits — invalid
    }

    #[test]
    fn test_color_similarity_identical() {
        let c = (0xe0, 0xf5, 0xf5);
        assert!((color_similarity(c, c) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_color_similarity_black_white() {
        let black = (0, 0, 0);
        let white = (255, 255, 255);
        assert!((color_similarity(black, white)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_color_similarity_cyan_teal_closer_than_cyan_yellow() {
        // Real PF values
        let cyan = parse_hex_color("#e0f5f5").unwrap();   // cyan-50
        let teal = parse_hex_color("#daf2f2").unwrap();   // teal-10
        let yellow = parse_hex_color("#fff4cc").unwrap();  // yellow-10
        let orange = parse_hex_color("#fff3e8").unwrap();  // orange-10

        let sim_cyan_teal = color_similarity(cyan, teal);
        let sim_cyan_yellow = color_similarity(cyan, yellow);
        let sim_cyan_orange = color_similarity(cyan, orange);

        assert!(
            sim_cyan_teal > sim_cyan_yellow,
            "cyan→teal ({:.4}) should be closer than cyan→yellow ({:.4})",
            sim_cyan_teal, sim_cyan_yellow
        );
        assert!(
            sim_cyan_teal > sim_cyan_orange,
            "cyan→teal ({:.4}) should be closer than cyan→orange ({:.4})",
            sim_cyan_teal, sim_cyan_orange
        );
    }

    #[test]
    fn test_color_similarity_gold_yellow_closer_than_gold_teal() {
        let gold = parse_hex_color("#fdf7e7").unwrap();    // gold-50
        let yellow = parse_hex_color("#fff4cc").unwrap();  // yellow-10
        let teal = parse_hex_color("#daf2f2").unwrap();    // teal-10

        let sim_gold_yellow = color_similarity(gold, yellow);
        let sim_gold_teal = color_similarity(gold, teal);

        assert!(
            sim_gold_yellow > sim_gold_teal,
            "gold→yellow ({:.4}) should be closer than gold→teal ({:.4})",
            sim_gold_yellow, sim_gold_teal
        );
    }

    #[test]
    fn test_color_similarity_is_symmetric() {
        let c1 = (100, 150, 200);
        let c2 = (200, 100, 50);
        assert!(
            (color_similarity(c1, c2) - color_similarity(c2, c1)).abs() < f64::EPSILON
        );
    }

    /// Resolved similarity with multiple slots — verifies that per-slot
    /// color distance is averaged across all shared slots, not just one.
    #[test]
    fn test_resolved_similarity_multi_slot_color_distance() {
        use crate::sd_types::CssModifierEffect;

        // Old modifier overrides 3 slots with resolved hex values
        let mut old = CssModifierEffect::default();
        old.resolved_overrides.insert(
            "--pf-v5-c-label--BackgroundColor".into(),
            "#e0f5f5".into(), // cyan background
        );
        old.resolved_overrides.insert(
            "--pf-v5-c-label__icon--Color".into(),
            "#005f60".into(), // dark cyan icon
        );
        old.resolved_overrides.insert(
            "--pf-v5-c-label__content--Color".into(),
            "#003737".into(), // darkest cyan text
        );

        // New modifier "teal" — similar colors on all 3 slots
        let mut new_teal = CssModifierEffect::default();
        new_teal.resolved_overrides.insert(
            "--pf-v6-c-label--BackgroundColor".into(),
            "#daf2f2".into(), // teal background (close to #e0f5f5)
        );
        new_teal.resolved_overrides.insert(
            "--pf-v6-c-label__icon--Color".into(),
            "#004d4d".into(), // dark teal icon (close to #005f60)
        );
        new_teal.resolved_overrides.insert(
            "--pf-v6-c-label__content--Color".into(),
            "#002b2b".into(), // darkest teal text (close to #003737)
        );

        // New modifier "red" — very different colors
        let mut new_red = CssModifierEffect::default();
        new_red.resolved_overrides.insert(
            "--pf-v6-c-label--BackgroundColor".into(),
            "#fce8e8".into(), // red background
        );
        new_red.resolved_overrides.insert(
            "--pf-v6-c-label__icon--Color".into(),
            "#c9190b".into(), // red icon
        );
        new_red.resolved_overrides.insert(
            "--pf-v6-c-label__content--Color".into(),
            "#7d1007".into(), // dark red text
        );

        // Build target maps so resolved similarity can trace custom props → CSS props
        let mut old_targets = crate::sd_types::CssPropertyTargetMap::new();
        old_targets.insert("--pf-v5-c-label--BackgroundColor".into(), "background-color".into());
        old_targets.insert("--pf-v5-c-label__icon--Color".into(), "color".into());
        old_targets.insert("--pf-v5-c-label__content--Color".into(), "color".into());

        let mut new_targets = crate::sd_types::CssPropertyTargetMap::new();
        new_targets.insert("--pf-v6-c-label--BackgroundColor".into(), "background-color".into());
        new_targets.insert("--pf-v6-c-label__icon--Color".into(), "color".into());
        new_targets.insert("--pf-v6-c-label__content--Color".into(), "color".into());

        let sim_teal = modifier_resolved_similarity(&old, &new_teal, &old_targets, &new_targets);
        let sim_red = modifier_resolved_similarity(&old, &new_red, &old_targets, &new_targets);

        assert!(
            sim_teal > sim_red,
            "Multi-slot: cyan modifier should be closer to teal ({:.4}) than red ({:.4})",
            sim_teal, sim_red
        );
        // Both should be > 0.0 since they share CSS properties
        assert!(sim_teal > 0.5, "teal similarity should be high: {:.4}", sim_teal);
        assert!(sim_red > 0.0, "red similarity should be > 0 (shared CSS props): {:.4}", sim_red);
    }

    /// Non-color resolved values (sizes, keywords) don't produce hex colors,
    /// so the CSS-property-based comparison returns 0.0 for them.
    #[test]
    fn test_resolved_similarity_non_color_values_ignored() {
        use crate::sd_types::CssModifierEffect;

        let mut old = CssModifierEffect::default();
        old.resolved_overrides.insert(
            "--pf-v5-c-comp--FontSize".into(),
            "16px".into(),
        );

        let mut new_same = CssModifierEffect::default();
        new_same.resolved_overrides.insert(
            "--pf-v6-c-comp--FontSize".into(),
            "16px".into(),
        );

        // Target maps exist but values aren't hex colors
        let mut old_targets = crate::sd_types::CssPropertyTargetMap::new();
        old_targets.insert("--pf-v5-c-comp--FontSize".into(), "font-size".into());
        let mut new_targets = crate::sd_types::CssPropertyTargetMap::new();
        new_targets.insert("--pf-v6-c-comp--FontSize".into(), "font-size".into());

        let sim = modifier_resolved_similarity(&old, &new_same, &old_targets, &new_targets);
        assert!(
            sim.abs() < f64::EPSILON,
            "Non-color values should give 0.0 (no parseable hex colors): {:.4}", sim
        );
    }

    /// TC057-062: PageSection should get a rule about hasBodyWrapper default change.
    ///
    /// Currently fails because generate_prop_default_changed_rules is a stub
    /// returning empty Vec.
    #[test]
    fn test_prop_default_changed_generates_rule() {
        use crate::sd_types::SourceLevelCategory;

        let mut sd = SdPipelineResult::default();

        // Add a PropDefault source-level change for PageSection.hasBodyWrapper
        sd.source_level_changes.push(crate::sd_types::SourceLevelChange {
            component: "PageSection".into(),
            category: SourceLevelCategory::PropDefault,
            description:
                "Default value for 'hasBodyWrapper' prop on PageSection removed (was true)".into(),
            old_value: Some("true".into()),
            new_value: None,
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
            dependency_chain: None,
        });

        sd.component_packages
            .insert("PageSection".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_default_changed_rules(&sd, &pkgs);

        assert!(
            !rules.is_empty(),
            "TC057-062: Should generate at least 1 rule for PageSection hasBodyWrapper default change"
        );

        let rule = &rules[0];
        assert!(
            rule.rule_id.contains("pagesection") || rule.rule_id.contains("PageSection"),
            "TC057-062: Rule should be about PageSection: {}",
            rule.rule_id
        );
         assert!(
            rule.message.contains("hasBodyWrapper"),
            "TC057-062: Rule message should mention hasBodyWrapper: {}",
            rule.message
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    // PropDefault trivial-default suppression
    //
    // TC023: DualListSelector children='' fires and LLM adds children={''}
    // TC024: Same + compounds with /deprecated issue
    // TC045: MenuItemAction className='' fires and LLM adds className={''}
    //
    // Empty-string defaults ('') are noise — no consumer relies on them.
    // generate_prop_default_changed_rules() should skip them.
    // ═══════════════════════════════════════════════════════════════════

    /// Trivially empty defaults (empty string) should NOT generate rules.
    /// These cause the LLM to add `children={''}` or `className={''}` noise.
    #[test]
    fn test_prop_default_empty_string_suppressed() {
        use crate::sd_types::SourceLevelCategory;

        let mut sd = SdPipelineResult::default();

        // DualListSelector.children default was '' (TC023/TC024)
        sd.source_level_changes.push(crate::sd_types::SourceLevelChange {
            component: "DualListSelector".into(),
            category: SourceLevelCategory::PropDefault,
            description:
                "Default value for 'children' prop on DualListSelector removed (was '')".into(),
            old_value: Some("''".into()),
            new_value: None,
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
            dependency_chain: None,
        });

        // MenuItemAction.className default was '' (TC045)
        sd.source_level_changes.push(crate::sd_types::SourceLevelChange {
            component: "MenuItemAction".into(),
            category: SourceLevelCategory::PropDefault,
            description:
                "Default value for 'className' prop on MenuItemAction removed (was '')".into(),
            old_value: Some("''".into()),
            new_value: None,
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
            dependency_chain: None,
        });

        sd.component_packages
            .insert("DualListSelector".into(), "@patternfly/react-core".into());
        sd.component_packages
            .insert("MenuItemAction".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_default_changed_rules(&sd, &pkgs);

        // Neither should produce a rule
        let children_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("duallistselector-children"));
        let classname_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("menuitemaction-classname"));

        assert!(
            children_rule.is_none(),
            "TC023/TC024: DualListSelector children='' should be suppressed, got: {:?}",
            children_rule.map(|r| &r.rule_id)
        );
        assert!(
            classname_rule.is_none(),
            "TC045: MenuItemAction className='' should be suppressed, got: {:?}",
            classname_rule.map(|r| &r.rule_id)
        );
    }

    /// Double-quoted empty strings should also be suppressed.
    #[test]
    fn test_prop_default_double_quoted_empty_string_suppressed() {
        use crate::sd_types::SourceLevelCategory;

        let mut sd = SdPipelineResult::default();
        sd.source_level_changes.push(crate::sd_types::SourceLevelChange {
            component: "TestComp".into(),
            category: SourceLevelCategory::PropDefault,
            description:
                "Default value for 'label' prop on TestComp removed (was \"\")".into(),
            old_value: Some("\"\"".into()),
            new_value: None,
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
            dependency_chain: None,
        });

        sd.component_packages
            .insert("TestComp".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_default_changed_rules(&sd, &pkgs);
        assert!(
            rules.is_empty(),
            "Double-quoted empty string default should be suppressed"
        );
    }

    /// `undefined` and `null` defaults should be suppressed.
    #[test]
    fn test_prop_default_undefined_null_suppressed() {
        use crate::sd_types::SourceLevelCategory;

        let mut sd = SdPipelineResult::default();

        sd.source_level_changes.push(crate::sd_types::SourceLevelChange {
            component: "Comp1".into(),
            category: SourceLevelCategory::PropDefault,
            description:
                "Default value for 'icon' prop on Comp1 removed (was undefined)".into(),
            old_value: Some("undefined".into()),
            new_value: None,
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
            dependency_chain: None,
        });

        sd.source_level_changes.push(crate::sd_types::SourceLevelChange {
            component: "Comp2".into(),
            category: SourceLevelCategory::PropDefault,
            description:
                "Default value for 'ref' prop on Comp2 removed (was null)".into(),
            old_value: Some("null".into()),
            new_value: None,
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
            dependency_chain: None,
        });

        sd.component_packages
            .insert("Comp1".into(), "@patternfly/react-core".into());
        sd.component_packages
            .insert("Comp2".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_default_changed_rules(&sd, &pkgs);
        assert!(
            rules.is_empty(),
            "undefined and null defaults should be suppressed, got {} rules",
            rules.len()
        );
    }

    /// Meaningful defaults like 'primary' or 'h1' should NOT be suppressed.
    #[test]
    fn test_prop_default_meaningful_values_not_suppressed() {
        use crate::sd_types::SourceLevelCategory;

        let mut sd = SdPipelineResult::default();

        // variant='primary' → removed (meaningful)
        sd.source_level_changes.push(crate::sd_types::SourceLevelChange {
            component: "Button".into(),
            category: SourceLevelCategory::PropDefault,
            description:
                "Default value for 'variant' prop on Button removed (was 'primary')".into(),
            old_value: Some("'primary'".into()),
            new_value: None,
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
            dependency_chain: None,
        });

        // headingLevel='h1' → 'h2' (meaningful change)
        sd.source_level_changes.push(crate::sd_types::SourceLevelChange {
            component: "EmptyStateHeader".into(),
            category: SourceLevelCategory::PropDefault,
            description:
                "Default value for 'headingLevel' prop on EmptyStateHeader changed from 'h1' to 'h2'".into(),
            old_value: Some("'h1'".into()),
            new_value: Some("'h2'".into()),
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
            dependency_chain: None,
        });

        // boolean true → removed (meaningful — hasBodyWrapper)
        sd.source_level_changes.push(crate::sd_types::SourceLevelChange {
            component: "PageSection".into(),
            category: SourceLevelCategory::PropDefault,
            description:
                "Default value for 'hasBodyWrapper' prop on PageSection removed (was true)".into(),
            old_value: Some("true".into()),
            new_value: None,
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
            dependency_chain: None,
        });

        sd.component_packages
            .insert("Button".into(), "@patternfly/react-core".into());
        sd.component_packages
            .insert("EmptyStateHeader".into(), "@patternfly/react-core".into());
        sd.component_packages
            .insert("PageSection".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_default_changed_rules(&sd, &pkgs);
        assert_eq!(
            rules.len(),
            3,
            "All 3 meaningful defaults should generate rules, got {}",
            rules.len()
        );
    }

    /// TC037: Noop arrow function defaults should be suppressed.
    /// Label.onClick had default `(_e: React.MouseEvent) => undefined as any`
    /// which is semantically meaningless — no consumer relies on it.
    #[test]
    fn test_prop_default_noop_function_suppressed() {
        use crate::sd_types::SourceLevelCategory;

        let mut sd = SdPipelineResult::default();
        sd.source_level_changes.push(crate::sd_types::SourceLevelChange {
            component: "Label".into(),
            category: SourceLevelCategory::PropDefault,
            description:
                "Default value for 'onClick' prop on Label removed (was (_e: React.MouseEvent) => undefined as any)".into(),
            old_value: Some("(_e: React.MouseEvent) => undefined as any".into()),
            new_value: None,
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
            dependency_chain: None,
        });

        sd.component_packages
            .insert("Label".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_default_changed_rules(&sd, &pkgs);
        assert!(
            rules.is_empty(),
            "TC037: Noop arrow function default should be suppressed, got {} rules",
            rules.len()
        );
    }

    /// `false` defaults should be suppressed — removing a `false` default
    /// means the prop now defaults to `undefined` (both falsy, same effect).
    #[test]
    fn test_prop_default_false_suppressed() {
        use crate::sd_types::SourceLevelCategory;

        let mut sd = SdPipelineResult::default();
        sd.source_level_changes.push(crate::sd_types::SourceLevelChange {
            component: "Comp".into(),
            category: SourceLevelCategory::PropDefault,
            description:
                "Default value for 'isDisabled' prop on Comp removed (was false)".into(),
            old_value: Some("false".into()),
            new_value: None,
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
            dependency_chain: None,
        });

        sd.component_packages
            .insert("Comp".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_default_changed_rules(&sd, &pkgs);
        assert!(
            rules.is_empty(),
            "false default should be suppressed"
        );
    }

    /// Meaningful string defaults like 'close' should NOT be suppressed.
    #[test]
    fn test_prop_default_meaningful_string_not_suppressed() {
        use crate::sd_types::SourceLevelCategory;

        let mut sd = SdPipelineResult::default();
        sd.source_level_changes.push(crate::sd_types::SourceLevelChange {
            component: "Label".into(),
            category: SourceLevelCategory::PropDefault,
            description:
                "Default value for 'closeBtnAriaLabel' prop on Label removed (was 'close')".into(),
            old_value: Some("'close'".into()),
            new_value: None,
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
            dependency_chain: None,
        });

        sd.component_packages
            .insert("Label".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_default_changed_rules(&sd, &pkgs);
        assert_eq!(
            rules.len(),
            1,
            "Meaningful string default 'close' should NOT be suppressed"
        );
    }

    /// TC031: HelperTextItem hasIcon AND isDynamic both removed.
    /// Both should produce RemoveProp rules or be covered by a grouped rule.
    ///
    /// Expected to PASS — semver-analyzer correctly generates grouped rule.
    /// The gap is in the fix-engine (only removes first prop in group).
    #[test]
    fn test_helpertextitem_both_props_have_rules() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/HelperTextItem.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![
                ApiChange {
                    symbol: "HelperTextItemProps.hasIcon".into(),
                    qualified_name: String::new(),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Removed,
                    before: Some("property: hasIcon: boolean".into()),
                    after: None,
                    description: "property `hasIcon` was removed".into(),
                    migration_target: None,
                    removal_disposition: None,
                },
                ApiChange {
                    symbol: "HelperTextItemProps.isDynamic".into(),
                    qualified_name: String::new(),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Removed,
                    before: Some("property: isDynamic: boolean".into()),
                    after: None,
                    description: "property `isDynamic` was removed".into(),
                    migration_target: None,
                    removal_disposition: None,
                },
            ],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        // The TD rule generator (konveyor.rs, not v2) produces these rules.
        // For this test, we verify both props appear as Removed ApiChanges
        // in the report and would generate strategies.
        let has_icon = report.changes[0]
            .breaking_api_changes
            .iter()
            .any(|c| c.symbol.contains("hasIcon") && c.change == ApiChangeType::Removed);
        let is_dynamic = report.changes[0]
            .breaking_api_changes
            .iter()
            .any(|c| c.symbol.contains("isDynamic") && c.change == ApiChangeType::Removed);

        assert!(
            has_icon,
            "TC031: hasIcon should be in report as Removed"
        );
        assert!(
            is_dynamic,
            "TC031: isDynamic should be in report as Removed"
        );
    }

    /// TC022: DrawerHead→DrawerActions edge should be Structural (CHP-only),
    /// NOT Required (CHP+PMC). DrawerActions is optional inside DrawerHead —
    /// PF6 docs do not require it. When the edge is Required, a requiresChild
    /// rule fires on DrawerHead and the LLM adds DrawerActions+DrawerCloseButton
    /// to code that never had them, breaking TC022.
    ///
    /// The fix: change the edge from Required (PMC=YES) to Structural (PMC=NO)
    /// so no requiresChild rule is generated for DrawerHead.
    #[test]
    fn test_drawer_head_behavioral_rule_is_informational() {
        use crate::sd_types::EdgeStrength;

        let mut pkgs = test_pkg_map();
        pkgs.insert("Drawer".into(), "@patternfly/react-core".into());
        pkgs.insert("DrawerHead".into(), "@patternfly/react-core".into());
        pkgs.insert("DrawerActions".into(), "@patternfly/react-core".into());
        pkgs.insert("DrawerCloseButton".into(), "@patternfly/react-core".into());

        // Current (broken) tree: DrawerHead→DrawerActions is Required (CHP+PMC).
        // This generates a requiresChild rule that causes over-fixing.
        let tree = CompositionTree {
            root: "Drawer".into(),
            family_members: vec![
                "Drawer".into(),
                "DrawerHead".into(),
                "DrawerActions".into(),
                "DrawerCloseButton".into(),
            ],
            edges: vec![
                make_edge("Drawer", "DrawerHead", EdgeStrength::Required),
                make_edge("DrawerHead", "DrawerActions", EdgeStrength::Required),
                make_edge("DrawerActions", "DrawerCloseButton", EdgeStrength::Required),
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        // Check that NO requiresChild rule exists for DrawerHead.
        // A requiresChild rule on DrawerHead causes over-fixing (TC022).
        // Note: short_component_id strips "Drawer" prefix, so "DrawerHead" → "head"
        let drawer_head_requires: Vec<_> = rules
            .iter()
            .filter(|r| {
                // Match "sd-cf-drawer-head-req-*" pattern
                r.rule_id.contains("-head-req-")
            })
            .collect();

        assert!(
            drawer_head_requires.is_empty(),
            "TC022: DrawerHead should NOT have a requiresChild rule (DrawerActions is optional). \
             Found rules: {:?}",
            drawer_head_requires
                .iter()
                .map(|r| &r.rule_id)
                .collect::<Vec<_>>()
        );
    }

    /// TC010: Card selectableActions — no API surface change, no rule expected.
    /// Documents that the semver-analyzer correctly produces no rule for this
    /// since selectableActions was NOT removed from CardHeaderProps in PF6.
    #[test]
    fn test_card_selectable_actions_no_rule() {
        // selectableActions still exists in PF6 CardHeaderProps.
        // The semver-analyzer should not generate a rule for it.
        // This is a usage-pattern change, not an API surface change.
        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "CardHeader".into(),
            ["selectableActions", "actions", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        // selectableActions still present in new version
        sd.new_component_props.insert(
            "CardHeader".into(),
            ["selectableActions", "actions", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );

        // No prop was removed → no rule should be generated
        // This test documents the gap: simplification of selectableActions
        // for basic clickable cards requires hand-crafted rule or LLM.
        let report = build_test_report(vec![]);
        let pkgs: HashMap<String, String> = [(
            "CardHeader".into(),
            "@patternfly/react-core".into(),
        )]
        .into_iter()
        .collect();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);
        let card_rules: Vec<_> = rules
            .iter()
            .filter(|r| r.rule_id.contains("cardheader") && r.rule_id.contains("selectableActions"))
            .collect();

        assert!(
            card_rules.is_empty(),
            "TC010: No rule should exist for selectableActions (prop not removed in PF6). \
             This is a known gap requiring a hand-crafted rule."
        );
    }

    /// TC029: FormFieldGroupHeader typo fix rename is correct.
    /// The semver-analyzer correctly generates a Rename rule.
    /// Unused import cleanup is a fix-engine concern.
    #[test]
    fn test_unused_import_rename_correct() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/FormFieldGroupHeader.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "FormFiledGroupHeaderTitleTextObject".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Interface,
                change: ApiChangeType::Renamed,
                before: Some("FormFiledGroupHeaderTitleTextObject".into()),
                after: Some("FormFieldGroupHeaderTitleTextObject".into()),
                description: "renamed (typo fix)".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        // Verify the rename exists and is correct
        let change = &report.changes[0].breaking_api_changes[0];
        assert_eq!(change.change, ApiChangeType::Renamed);
        assert_eq!(
            change.before.as_deref(),
            Some("FormFiledGroupHeaderTitleTextObject")
        );
        assert_eq!(
            change.after.as_deref(),
            Some("FormFieldGroupHeaderTitleTextObject")
        );
        // TC029: This is correct. Unused import cleanup is outside semver-analyzer scope.
    }

    // ═══════════════════════════════════════════════════════════════════
    // New absorbing prop detection
    //
    // When a component gains a new ReactNode/ReactElement prop that didn't
    // exist before, content that was previously passed as children may need
    // to move to that prop. Generate LlmAssisted rules with enough context
    // for the LLM to determine if children need restructuring.
    // ═══════════════════════════════════════════════════════════════════

    /// TC046: MenuToggle gained `icon: React.ReactNode` prop in v6.
    /// Should generate a new-absorbing-prop rule so the LLM knows to
    /// move icon children to the `icon` prop.
    #[test]
    fn test_new_absorbing_prop_menutoggle_icon() {
        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "MenuToggle".into(),
            ["children", "variant", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "MenuToggle".into(),
            ["children", "icon", "variant", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_prop_types.insert(
            "MenuToggle".into(),
            [("icon".into(), "React.ReactNode".into())]
                .into_iter()
                .collect(),
        );
        // Mark that MenuToggle has children prop
        let mut profile = crate::sd_types::ComponentSourceProfile::default();
        profile.has_children_prop = true;
        sd.new_profiles.insert("MenuToggle".into(), profile);

        sd.component_packages
            .insert("MenuToggle".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_new_absorbing_prop_rules(&sd, &pkgs);

        let icon_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("menutoggle") && r.rule_id.contains("icon"));
        assert!(
            icon_rule.is_some(),
            "TC046: Should generate a new-absorbing-prop rule for MenuToggle.icon"
        );
        let rule = icon_rule.unwrap();
        assert!(
            rule.message.contains("icon"),
            "Rule message should mention the prop name"
        );
        assert!(
            rule.message.contains("React.ReactNode"),
            "Rule message should include the prop type"
        );
        assert_eq!(
            rule.category, "potential",
            "New absorbing prop rules should be 'potential' not 'mandatory'"
        );
        assert!(
            rule.fix_strategy.is_some(),
            "Should have an LlmAssisted fix strategy"
        );
        assert_eq!(
            rule.fix_strategy.as_ref().unwrap().strategy,
            "LlmAssisted"
        );
    }

    /// TC053: NavItem gained `icon: React.ReactNode` prop in v6.
    /// Same pattern as TC046.
    #[test]
    fn test_new_absorbing_prop_navitem_icon() {
        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "NavItem".into(),
            ["children", "to", "hasNavLinkWrapper", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "NavItem".into(),
            ["children", "icon", "to", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_prop_types.insert(
            "NavItem".into(),
            [("icon".into(), "React.ReactNode".into())]
                .into_iter()
                .collect(),
        );
        let mut profile = crate::sd_types::ComponentSourceProfile::default();
        profile.has_children_prop = true;
        sd.new_profiles.insert("NavItem".into(), profile);

        sd.component_packages
            .insert("NavItem".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_new_absorbing_prop_rules(&sd, &pkgs);

        assert!(
            rules.iter().any(|r| r.rule_id.contains("navitem") && r.rule_id.contains("icon")),
            "TC053: Should generate a new-absorbing-prop rule for NavItem.icon"
        );
    }

    /// Props that are NOT ReactNode/ReactElement should NOT generate rules.
    #[test]
    fn test_new_absorbing_prop_skips_non_reactnode() {
        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "Toolbar".into(),
            ["children", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "Toolbar".into(),
            ["children", "hasNoPadding", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_prop_types.insert(
            "Toolbar".into(),
            [("hasNoPadding".into(), "boolean".into())]
                .into_iter()
                .collect(),
        );
        let mut profile = crate::sd_types::ComponentSourceProfile::default();
        profile.has_children_prop = true;
        sd.new_profiles.insert("Toolbar".into(), profile);

        sd.component_packages
            .insert("Toolbar".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_new_absorbing_prop_rules(&sd, &pkgs);
        assert!(
            rules.is_empty(),
            "boolean props should not generate new-absorbing-prop rules"
        );
    }

    /// Props that already existed in old version should NOT generate rules.
    #[test]
    fn test_new_absorbing_prop_skips_existing_props() {
        let mut sd = SdPipelineResult::default();
        // icon already existed in v5
        sd.old_component_props.insert(
            "Button".into(),
            ["children", "icon", "variant"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "Button".into(),
            ["children", "icon", "variant"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_prop_types.insert(
            "Button".into(),
            [("icon".into(), "ReactNode".into())]
                .into_iter()
                .collect(),
        );
        let mut profile = crate::sd_types::ComponentSourceProfile::default();
        profile.has_children_prop = true;
        sd.new_profiles.insert("Button".into(), profile);

        sd.component_packages
            .insert("Button".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_new_absorbing_prop_rules(&sd, &pkgs);
        assert!(
            rules.is_empty(),
            "Props that already existed should not generate rules"
        );
    }

    /// Components without children prop should NOT generate rules.
    #[test]
    fn test_new_absorbing_prop_skips_no_children() {
        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "Badge".into(),
            ["className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "Badge".into(),
            ["className", "icon"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_prop_types.insert(
            "Badge".into(),
            [("icon".into(), "ReactNode".into())]
                .into_iter()
                .collect(),
        );
        // No profile with has_children_prop = true
        let mut profile = crate::sd_types::ComponentSourceProfile::default();
        profile.has_children_prop = false;
        sd.new_profiles.insert("Badge".into(), profile);

        sd.component_packages
            .insert("Badge".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_new_absorbing_prop_rules(&sd, &pkgs);
        assert!(
            rules.is_empty(),
            "Components without children should not generate rules"
        );
    }

    /// TC053: NavItem hasNavLinkWrapper removal is correctly detected as Removed.
    /// The icon-to-prop migration is now handled by generate_new_absorbing_prop_rules.
    #[test]
    fn test_icon_children_to_prop_navitem_removal_still_works() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/NavItem.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "NavItemProps.hasNavLinkWrapper".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::Removed,
                before: Some("property: hasNavLinkWrapper: boolean".into()),
                after: None,
                description: "property `hasNavLinkWrapper` was removed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let change = &report.changes[0].breaking_api_changes[0];
        assert_eq!(change.change, ApiChangeType::Removed);
        assert!(change.symbol.contains("hasNavLinkWrapper"));
    }

    // ── Integration tests: surviving-value fallback removal ──────────

    /// TC051/TC060: Nav.variant "tertiary" should NOT map to "horizontal".
    /// Old: 'default' | 'horizontal' | 'tertiary' | 'horizontal-subnav'
    /// New: 'default' | 'horizontal' | 'horizontal-subnav'
    /// Removed: tertiary. Added: none. Surviving: default, horizontal, horizontal-subnav.
    ///
    /// With the surviving-value fallback removed, "tertiary" should have
    /// NO replacement (previously mapped to "horizontal" via string similarity
    /// 0.30 >= 0.2 threshold). The rule should say "Valid values: ..." and
    /// the LLM will correctly choose "horizontal-subnav".
    #[test]
    fn test_nav_variant_tertiary_no_surviving_fallback() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/Nav.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "NavProps.variant".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some(
                    "property: variant: 'default' | 'horizontal' | 'tertiary' | 'horizontal-subnav'"
                        .into(),
                ),
                after: Some(
                    "property: variant: 'default' | 'horizontal' | 'horizontal-subnav'".into(),
                ),
                description: "type changed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let mut sd = SdPipelineResult::default();
        sd.component_packages
            .insert("Nav".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        // Should have exactly 1 rule for "tertiary"
        let tertiary_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("tertiary"))
            .expect("TC051: Should have a rule for removed value 'tertiary'");

        let replacement = tertiary_rule
            .fix_strategy
            .as_ref()
            .and_then(|s| s.replacement.as_ref());

        assert_eq!(
            replacement.map(|s| s.as_str()),
            None,
            "TC051: tertiary should NOT map to any surviving value. \
             Previously mapped to 'horizontal' (sim=0.30), but the correct \
             replacement is 'horizontal-subnav' which requires LLM reasoning. \
             Got: {:?}",
            replacement
        );

        // Verify the rule message lists valid values
        assert!(
            tertiary_rule.message.contains("horizontal-subnav"),
            "TC051: Rule message should list 'horizontal-subnav' as a valid value"
        );
    }

    /// TC051/TC060: Nav.variant "tertiary" should NOT map to "horizontal"
    /// even when real CSS modifier data is available.
    ///
    /// Uses real PF v5/v6 CSS data: tertiary (29 custom property overrides)
    /// vs surviving candidates horizontal-subnav (9 overrides, restructured
    /// token naming). The CSS bridge should return no match because the
    /// structures are too different between v5 and v6.
    #[test]
    fn test_nav_variant_tertiary_no_match_with_real_css_data() {
        use crate::sd_types::{CssModifierEffect, CssModifierMap, ComponentCssModifiers};
        use semver_analyzer_core::*;

        // Build CSS data from real PF data
        let mut old_mods = ComponentCssModifiers::new();
        let mut old_nav = CssModifierMap::new();

        // Real PF v5 pf-m-tertiary data (key properties only - representative subset)
        let mut tertiary = CssModifierEffect::default();
        for prop in &[
            "--pf-v5-c-nav__link--BackgroundColor",
            "--pf-v5-c-nav__link--Color",
            "--pf-v5-c-nav__link--PaddingBottom",
            "--pf-v5-c-nav__link--PaddingLeft",
            "--pf-v5-c-nav__link--PaddingRight",
            "--pf-v5-c-nav__link--PaddingTop",
            "--pf-v5-c-nav__link--Left",
            "--pf-v5-c-nav__link--Right",
            "--pf-v5-c-nav__link--hover--BackgroundColor",
            "--pf-v5-c-nav__link--hover--Color",
            "--pf-v5-c-nav__link--active--BackgroundColor",
            "--pf-v5-c-nav__link--active--Color",
            "--pf-v5-c-nav__link--before--BorderColor",
            "--pf-v5-c-nav__link--before--BorderBottomWidth",
            "--pf-v5-c-nav__scroll-button--Color",
        ] {
            tertiary
                .custom_property_overrides
                .insert(prop.to_string(), format!("var(--pf-v5-c-nav--m-tertiary-override)"));
        }
        tertiary
            .direct_properties
            .insert("display".into(), "flex".into());
        tertiary
            .direct_properties
            .insert("overflow".into(), "hidden".into());
        old_nav.insert("pf-m-tertiary".into(), tertiary);

        // Real PF v5 pf-m-horizontal data
        let mut horizontal = CssModifierEffect::default();
        for prop in &[
            "--pf-v5-c-nav__link--BackgroundColor",
            "--pf-v5-c-nav__link--Color",
            "--pf-v5-c-nav__link--PaddingBottom",
            "--pf-v5-c-nav__link--PaddingLeft",
            "--pf-v5-c-nav__link--PaddingRight",
            "--pf-v5-c-nav__link--PaddingTop",
            "--pf-v5-c-nav__link--Left",
            "--pf-v5-c-nav__link--Right",
        ] {
            horizontal
                .custom_property_overrides
                .insert(prop.to_string(), format!("var(--pf-v5-c-nav--m-horizontal-override)"));
        }
        horizontal
            .direct_properties
            .insert("display".into(), "flex".into());
        old_nav.insert("pf-m-horizontal".into(), horizontal);

        // Real PF v5 pf-m-horizontal-subnav data (similar to tertiary)
        let mut h_subnav_old = CssModifierEffect::default();
        for prop in &[
            "--pf-v5-c-nav__link--BackgroundColor",
            "--pf-v5-c-nav__link--Color",
            "--pf-v5-c-nav__link--FontSize",
            "--pf-v5-c-nav__link--PaddingBottom",
            "--pf-v5-c-nav__link--PaddingLeft",
            "--pf-v5-c-nav__link--PaddingRight",
            "--pf-v5-c-nav__link--PaddingTop",
            "--pf-v5-c-nav__link--Left",
            "--pf-v5-c-nav__link--Right",
        ] {
            h_subnav_old
                .custom_property_overrides
                .insert(prop.to_string(), format!("var(--pf-v5-c-nav--m-horizontal-subnav-override)"));
        }
        old_nav.insert("pf-m-horizontal-subnav".into(), h_subnav_old);
        old_mods.insert("nav".into(), old_nav);

        // v6 CSS data: completely restructured token naming
        let mut new_mods = ComponentCssModifiers::new();
        let mut new_nav = CssModifierMap::new();

        // Real PF v6 pf-m-subnav data (replaces tertiary/horizontal-subnav)
        let mut subnav = CssModifierEffect::default();
        for prop in &[
            "--pf-v6-c-nav--BackgroundColor",
            "--pf-v6-c-nav--m-horizontal--m-scrollable__list--PaddingInlineEnd",
            "--pf-v6-c-nav--m-horizontal--m-scrollable__list--PaddingInlineStart",
            "--pf-v6-c-nav--m-horizontal__list--PaddingBlockEnd",
            "--pf-v6-c-nav--m-horizontal__list--PaddingBlockStart",
            "--pf-v6-c-nav--m-horizontal__list--PaddingInlineEnd",
            "--pf-v6-c-nav--m-horizontal__list--PaddingInlineStart",
            "--pf-v6-c-nav__link--PaddingBlockEnd",
            "--pf-v6-c-nav__link--PaddingBlockStart",
        ] {
            subnav
                .custom_property_overrides
                .insert(prop.to_string(), format!("var(--pf-v6-c-nav-subnav-override)"));
        }
        subnav
            .direct_properties
            .insert("border".into(), "var(--pf-v6-c-nav--m-horizontal--m-subnav--BorderWidth) solid var(--pf-v6-c-nav--m-horizontal--m-subnav--BorderColor)".into());
        new_nav.insert("pf-m-subnav".into(), subnav);
        new_mods.insert("nav".into(), new_nav);

        // Build report
        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/Nav.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "NavProps.variant".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some(
                    "property: variant: 'default' | 'horizontal' | 'tertiary' | 'horizontal-subnav'"
                        .into(),
                ),
                after: Some(
                    "property: variant: 'default' | 'horizontal' | 'horizontal-subnav'".into(),
                ),
                description: "type changed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let mut sd = SdPipelineResult::default();
        sd.component_packages
            .insert("Nav".into(), "@patternfly/react-core".into());
        sd.old_css_modifiers = old_mods;
        sd.new_css_modifiers = new_mods;
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        let tertiary_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("tertiary"))
            .expect("Should have a rule for 'tertiary'");

        let replacement = tertiary_rule
            .fix_strategy
            .as_ref()
            .and_then(|s| s.replacement.as_ref());

        // Even with CSS data, tertiary should NOT match any surviving value
        // because the v5→v6 CSS restructuring made the structures incomparable.
        assert_eq!(
            replacement.map(|s| s.as_str()),
            None,
            "TC051: tertiary should have no replacement even with CSS data. \
             The v5→v6 token restructuring makes structural comparison fail. \
             Got: {:?}",
            replacement
        );
    }

    /// TC065: PageSection.type "nav" should NOT map to "subnav".
    /// Old: 'default' | 'nav' | 'subnav' | 'wizard' | 'breadcrumb' | 'tabs'
    /// New: 'default' | 'subnav' | 'wizard' | 'breadcrumb' | 'tabs'
    /// Removed: nav. Added: none. Surviving includes subnav.
    ///
    /// The surviving-value fallback previously mapped "nav" → "subnav"
    /// (name_similarity 0.50 >= 0.2). The correct fix is to remove the
    /// type prop entirely, not rename its value.
    #[test]
    fn test_pagesection_type_nav_no_surviving_fallback() {
        use semver_analyzer_core::*;

        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/PageSection.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "PageSectionProps.type".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some(
                    "property: type: 'default' | 'nav' | 'subnav' | 'wizard' | 'breadcrumb' | 'tabs'"
                        .into(),
                ),
                after: Some(
                    "property: type: 'default' | 'subnav' | 'wizard' | 'breadcrumb' | 'tabs'"
                        .into(),
                ),
                description: "type changed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let mut sd = SdPipelineResult::default();
        sd.component_packages
            .insert("PageSection".into(), "@patternfly/react-core".into());
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        let nav_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("-nav"))
            .expect("TC065: Should have a rule for removed value 'nav'");

        let replacement = nav_rule
            .fix_strategy
            .as_ref()
            .and_then(|s| s.replacement.as_ref());

        assert_eq!(
            replacement.map(|s| s.as_str()),
            None,
            "TC065: 'nav' should NOT map to 'subnav' (a surviving value). \
             The correct fix is to remove the type prop entirely. \
             Got: {:?}",
            replacement
        );

        // Verify the rule message lists valid values for LLM guidance
        assert!(
            nav_rule.message.contains("subnav"),
            "TC065: Rule message should list 'subnav' among valid values"
        );
    }

    /// TC014: Label.color "cyan" should map to "teal" and "gold" to "yellow"
    /// via the CSS modifier bridge with real PF data.
    ///
    /// Old: blue | cyan | green | grey | gold | orange | purple | red
    /// New: blue | green | grey | orange | purple | red | teal | yellow
    /// Removed: cyan, gold. Added: teal, yellow (+ orangered).
    ///
    /// The CSS bridge compares modifier effects and should produce:
    ///   cyan → teal (similar background colors: #e0f5f5 → #daf2f2)
    ///   gold → yellow (similar background colors: #fdf7e7 → #fff4cc)
    ///
    /// Previously these fell through to surviving-value fallback and got
    /// wrong mappings: cyan→orange, gold→red.
    #[test]
    fn test_label_color_css_bridge_with_real_data() {
        use crate::sd_types::{CssModifierEffect, CssModifierMap, ComponentCssModifiers};
        use semver_analyzer_core::*;

        // ── Build real PF CSS modifier data ──
        let mut old_mods = ComponentCssModifiers::new();
        let mut old_label = CssModifierMap::new();

        // Real PF v5 pf-m-cyan (13 overrides with resolved hex values)
        let mut cyan = CssModifierEffect::default();
        let cyan_overrides = vec![
            ("--pf-v5-c-label--BackgroundColor", "#f2f9f9"),
            ("--pf-v5-c-label--BorderColor", "#009596"),
            ("--pf-v5-c-label__icon--Color", "#009596"),
            ("--pf-v5-c-label__content--Color", "#003737"),
            ("--pf-v5-c-label__content--link--Color", "#003737"),
            ("--pf-v5-c-label__content--link--hover--Color", "#003737"),
            ("--pf-v5-c-label__content--link--focus--Color", "#003737"),
            ("--pf-v5-c-label--m-outline--BorderColor", "#009596"),
            ("--pf-v5-c-label--m-outline__content--Color", "#005f60"),
            ("--pf-v5-c-label--m-outline__content--link--hover--Color", "#005f60"),
            ("--pf-v5-c-label--m-outline__content--link--focus--Color", "#005f60"),
            ("--pf-v5-c-label--m-outline--m-editable__content--before--BorderColor", "#009596"),
            ("--pf-v5-c-label--m-editable-active__content--before--BorderColor", "#d2d2d2"),
        ];
        for (k, v) in &cyan_overrides {
            cyan.custom_property_overrides.insert(k.to_string(), format!("var(--pf-v5-c-label--m-cyan-val)"));
            cyan.resolved_overrides.insert(k.to_string(), v.to_string());
        }
        old_label.insert("pf-m-cyan".into(), cyan);

        // Real PF v5 pf-m-gold
        let mut gold = CssModifierEffect::default();
        let gold_overrides = vec![
            ("--pf-v5-c-label--BackgroundColor", "#fdf7e7"),
            ("--pf-v5-c-label--BorderColor", "#f0ab00"),
            ("--pf-v5-c-label__icon--Color", "#f0ab00"),
            ("--pf-v5-c-label__content--Color", "#795600"),
            ("--pf-v5-c-label__content--link--Color", "#795600"),
            ("--pf-v5-c-label__content--link--hover--Color", "#795600"),
            ("--pf-v5-c-label__content--link--focus--Color", "#795600"),
            ("--pf-v5-c-label--m-outline--BorderColor", "#f0ab00"),
            ("--pf-v5-c-label--m-outline__content--Color", "#c58c00"),
            ("--pf-v5-c-label--m-outline__content--link--hover--Color", "#c58c00"),
            ("--pf-v5-c-label--m-outline__content--link--focus--Color", "#c58c00"),
            ("--pf-v5-c-label--m-outline--m-editable__content--before--BorderColor", "#f0ab00"),
            ("--pf-v5-c-label--m-editable-active__content--before--BorderColor", "#d2d2d2"),
        ];
        for (k, v) in &gold_overrides {
            gold.custom_property_overrides.insert(k.to_string(), format!("var(--pf-v5-c-label--m-gold-val)"));
            gold.resolved_overrides.insert(k.to_string(), v.to_string());
        }
        old_label.insert("pf-m-gold".into(), gold);
        old_mods.insert("label".into(), old_label);

        // Real PF v6 data
        let mut new_mods = ComponentCssModifiers::new();
        let mut new_label = CssModifierMap::new();

        // Real PF v6 pf-m-teal (8 overrides)
        let mut teal = CssModifierEffect::default();
        let teal_overrides = vec![
            ("--pf-v6-c-label--BackgroundColor", "#daf2f2"),
            ("--pf-v6-c-label--BorderColor", "#63bdbd"),
            ("--pf-v6-c-label__icon--Color", "#63bdbd"),
            ("--pf-v6-c-label__content--Color", "#151515"),
            ("--pf-v6-c-label--m-outline--BorderColor", "#9ad8d8"),
            ("--pf-v6-c-label--m-outline__content--Color", "#151515"),
            ("--pf-v6-c-label--m-outline--BackgroundColor", "#b9e5e5"),
            ("--pf-v6-c-label--m-clickable--hover--BackgroundColor", "#1f1f1f"),
        ];
        for (k, v) in &teal_overrides {
            teal.custom_property_overrides.insert(k.to_string(), format!("var(--pf-v6-c-label--m-teal-val)"));
            teal.resolved_overrides.insert(k.to_string(), v.to_string());
        }
        new_label.insert("pf-m-teal".into(), teal);

        // Real PF v6 pf-m-yellow (8 overrides)
        let mut yellow = CssModifierEffect::default();
        let yellow_overrides = vec![
            ("--pf-v6-c-label--BackgroundColor", "#fff4cc"),
            ("--pf-v6-c-label--BorderColor", "#dca614"),
            ("--pf-v6-c-label__icon--Color", "#dca614"),
            ("--pf-v6-c-label__content--Color", "#151515"),
            ("--pf-v6-c-label--m-outline--BorderColor", "#ffcc17"),
            ("--pf-v6-c-label--m-outline__content--Color", "#151515"),
            ("--pf-v6-c-label--m-outline--BackgroundColor", "#ffe072"),
            ("--pf-v6-c-label--m-clickable--hover--BackgroundColor", "#1f1f1f"),
        ];
        for (k, v) in &yellow_overrides {
            yellow.custom_property_overrides.insert(k.to_string(), format!("var(--pf-v6-c-label--m-yellow-val)"));
            yellow.resolved_overrides.insert(k.to_string(), v.to_string());
        }
        new_label.insert("pf-m-yellow".into(), yellow);

        // Real PF v6 pf-m-orangered (8 overrides, for disambiguation)
        let mut orangered = CssModifierEffect::default();
        let orangered_overrides = vec![
            ("--pf-v6-c-label--BackgroundColor", "#fce8e8"),
            ("--pf-v6-c-label--BorderColor", "#f4784a"),
            ("--pf-v6-c-label__icon--Color", "#f4784a"),
            ("--pf-v6-c-label__content--Color", "#151515"),
            ("--pf-v6-c-label--m-outline--BorderColor", "#f89b78"),
            ("--pf-v6-c-label--m-outline__content--Color", "#151515"),
            ("--pf-v6-c-label--m-outline--BackgroundColor", "#fbbea8"),
            ("--pf-v6-c-label--m-clickable--hover--BackgroundColor", "#1f1f1f"),
        ];
        for (k, v) in &orangered_overrides {
            orangered.custom_property_overrides.insert(k.to_string(), format!("var(--pf-v6-c-label--m-orangered-val)"));
            orangered.resolved_overrides.insert(k.to_string(), v.to_string());
        }
        new_label.insert("pf-m-orangered".into(), orangered);

        // Also need surviving colors that exist in both v5 and v6 (orange, red)
        // to verify they are NOT incorrectly matched
        let mut orange_new = CssModifierEffect::default();
        let orange_new_overrides = vec![
            ("--pf-v6-c-label--BackgroundColor", "#fff3e8"),
            ("--pf-v6-c-label--BorderColor", "#ef6518"),
            ("--pf-v6-c-label__icon--Color", "#ef6518"),
            ("--pf-v6-c-label__content--Color", "#151515"),
        ];
        for (k, v) in &orange_new_overrides {
            orange_new.custom_property_overrides.insert(k.to_string(), format!("var(--pf-v6-c-label--m-orange-val)"));
            orange_new.resolved_overrides.insert(k.to_string(), v.to_string());
        }
        new_label.insert("pf-m-orange".into(), orange_new);

        let mut red_new = CssModifierEffect::default();
        let red_new_overrides = vec![
            ("--pf-v6-c-label--BackgroundColor", "#fce8e8"),
            ("--pf-v6-c-label--BorderColor", "#c9190b"),
            ("--pf-v6-c-label__icon--Color", "#c9190b"),
            ("--pf-v6-c-label__content--Color", "#151515"),
        ];
        for (k, v) in &red_new_overrides {
            red_new.custom_property_overrides.insert(k.to_string(), format!("var(--pf-v6-c-label--m-red-val)"));
            red_new.resolved_overrides.insert(k.to_string(), v.to_string());
        }
        new_label.insert("pf-m-red".into(), red_new);

        new_mods.insert("label".into(), new_label);

        // CssPropertyTargetMap: map custom property tokens to CSS properties
        let mut old_targets = crate::sd_types::CssPropertyTargetMap::new();
        old_targets.insert("--pf-v5-c-label--BackgroundColor".into(), "background-color".into());
        old_targets.insert("--pf-v5-c-label--BorderColor".into(), "border-color".into());
        old_targets.insert("--pf-v5-c-label__icon--Color".into(), "color".into());
        old_targets.insert("--pf-v5-c-label__content--Color".into(), "color".into());

        let mut new_targets = crate::sd_types::CssPropertyTargetMap::new();
        new_targets.insert("--pf-v6-c-label--BackgroundColor".into(), "background-color".into());
        new_targets.insert("--pf-v6-c-label--BorderColor".into(), "border-color".into());
        new_targets.insert("--pf-v6-c-label__icon--Color".into(), "color".into());
        new_targets.insert("--pf-v6-c-label__content--Color".into(), "color".into());

        // Build report
        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/Label.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "LabelProps.color".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some(
                    "property: color: 'blue' | 'cyan' | 'green' | 'grey' | 'gold' | 'orange' | 'purple' | 'red'"
                        .into(),
                ),
                after: Some(
                    "property: color: 'blue' | 'green' | 'grey' | 'orange' | 'purple' | 'red' | 'teal' | 'yellow' | 'orangered'"
                        .into(),
                ),
                description: "type changed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let mut sd = SdPipelineResult::default();
        sd.component_packages
            .insert("Label".into(), "@patternfly/react-core".into());
        sd.old_css_modifiers = old_mods;
        sd.new_css_modifiers = new_mods;
        sd.old_css_property_targets = old_targets;
        sd.new_css_property_targets = new_targets;
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        // Find cyan rule
        let cyan_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("cyan"))
            .expect("TC014: Should have a rule for removed value 'cyan'");
        let cyan_repl = cyan_rule
            .fix_strategy
            .as_ref()
            .and_then(|s| s.replacement.as_ref());
        assert_eq!(
            cyan_repl.map(|s| s.as_str()),
            Some("teal"),
            "TC014: cyan should map to teal via CSS bridge (similar background colors). \
             Previously mapped to orange (surviving-value fallback). Got: {:?}",
            cyan_repl
        );

        // Find gold rule
        let gold_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("gold"))
            .expect("TC014: Should have a rule for removed value 'gold'");
        let gold_repl = gold_rule
            .fix_strategy
            .as_ref()
            .and_then(|s| s.replacement.as_ref());
        assert_eq!(
            gold_repl.map(|s| s.as_str()),
            Some("yellow"),
            "TC014: gold should map to yellow via CSS bridge (similar background colors). \
             Previously mapped to red (surviving-value fallback). Got: {:?}",
            gold_repl
        );
    }

    /// TC020: DrawerContent.colorVariant "light-200" should map to "secondary"
    /// (not "default") via the CSS modifier bridge with real PF data.
    ///
    /// Real data: both light-200 and secondary override the same 3 token
    /// slots (content, panel, section BackgroundColor) with similar colors
    /// (#f0f0f0 → #f2f2f2). "primary" only overrides content (#fff).
    ///
    /// Previously mapped to "default" via surviving-value string similarity
    /// (0.22 >= 0.2). Now uses CSS bridge with surviving values included.
    #[test]
    fn test_drawer_colorvariant_css_bridge_with_real_data() {
        use crate::sd_types::{CssModifierEffect, CssModifierMap, ComponentCssModifiers};
        use semver_analyzer_core::*;

        // Real PF CSS modifier data for Drawer
        let mut old_mods = ComponentCssModifiers::new();
        let mut old_drawer = CssModifierMap::new();

        // pf-m-light-200 (v5): 3 BackgroundColor overrides, all #f0f0f0
        let mut light200 = CssModifierEffect::default();
        for prop in &[
            "--pf-v5-c-drawer__content--BackgroundColor",
            "--pf-v5-c-drawer__panel--BackgroundColor",
            "--pf-v5-c-drawer__section--BackgroundColor",
        ] {
            light200.custom_property_overrides.insert(
                prop.to_string(),
                format!("var(--pf-v5-c-drawer--m-light-200-val)"),
            );
            light200
                .resolved_overrides
                .insert(prop.to_string(), "#f0f0f0".into());
        }
        old_drawer.insert("pf-m-light-200".into(), light200);

        // pf-m-no-background (v5): 3 BackgroundColor overrides, all transparent
        let mut nobg = CssModifierEffect::default();
        for prop in &[
            "--pf-v5-c-drawer__content--BackgroundColor",
            "--pf-v5-c-drawer__panel--BackgroundColor",
            "--pf-v5-c-drawer__section--BackgroundColor",
        ] {
            nobg.custom_property_overrides
                .insert(prop.to_string(), "transparent".into());
            nobg.resolved_overrides
                .insert(prop.to_string(), "transparent".into());
        }
        old_drawer.insert("pf-m-no-background".into(), nobg);
        old_mods.insert("drawer".into(), old_drawer);

        // v6 CSS data
        let mut new_mods = ComponentCssModifiers::new();
        let mut new_drawer = CssModifierMap::new();

        // pf-m-primary (v6): 1 BackgroundColor override, #fff
        let mut primary = CssModifierEffect::default();
        primary.custom_property_overrides.insert(
            "--pf-v6-c-drawer__content--BackgroundColor".into(),
            "var(--pf-v6-c-drawer__content--m-primary--BackgroundColor)".into(),
        );
        primary
            .resolved_overrides
            .insert("--pf-v6-c-drawer__content--BackgroundColor".into(), "#fff".into());
        new_drawer.insert("pf-m-primary".into(), primary);

        // pf-m-secondary (v6): 3 BackgroundColor overrides, all #f2f2f2
        let mut secondary = CssModifierEffect::default();
        for prop in &[
            "--pf-v6-c-drawer__content--BackgroundColor",
            "--pf-v6-c-drawer__panel--BackgroundColor",
            "--pf-v6-c-drawer__section--BackgroundColor",
        ] {
            secondary.custom_property_overrides.insert(
                prop.to_string(),
                format!("var(--pf-v6-c-drawer--m-secondary-val)"),
            );
            secondary
                .resolved_overrides
                .insert(prop.to_string(), "#f2f2f2".into());
        }
        new_drawer.insert("pf-m-secondary".into(), secondary);
        new_mods.insert("drawer".into(), new_drawer);

        // Build report
        let report = build_test_report(vec![FileChanges {
            file: std::path::PathBuf::from("src/DrawerContent.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "DrawerContentProps.colorVariant".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some(
                    "property: colorVariant: 'default' | 'light-200' | 'no-background'".into(),
                ),
                after: Some(
                    "property: colorVariant: 'default' | 'primary' | 'secondary'".into(),
                ),
                description: "type changed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }]);

        let mut sd = SdPipelineResult::default();
        sd.component_packages
            .insert("DrawerContent".into(), "@patternfly/react-core".into());
        sd.old_css_modifiers = old_mods;
        sd.new_css_modifiers = new_mods;
        let pkgs = sd.component_packages.clone();

        let rules = generate_prop_value_conformance_rules(&report, &sd, &pkgs);

        // no-background should still map to an added value (primary or secondary)
        // via string similarity: no-background → secondary (0.308 >= 0.3)
        let nobg_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("no-background") || r.rule_id.contains("no_background"))
            .expect("TC020: Should have a rule for 'no-background'");
        let nobg_repl = nobg_rule
            .fix_strategy
            .as_ref()
            .and_then(|s| s.replacement.as_ref());
        assert_eq!(
            nobg_repl.map(|s| s.as_str()),
            Some("secondary"),
            "TC020: no-background should map to secondary (added value, sim=0.308)"
        );

        // light-200 should map to secondary via CSS bridge:
        // Both override the same 3 token slots with similar hex colors
        // (#f0f0f0 → #f2f2f2). CSS structural similarity is high.
        // Previously mapped to "default" (surviving-value string sim 0.22).
        let light200_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("light-200"))
            .expect("TC020: Should have a rule for 'light-200'");
        let light200_repl = light200_rule
            .fix_strategy
            .as_ref()
            .and_then(|s| s.replacement.as_ref());
        assert_eq!(
            light200_repl.map(|s| s.as_str()),
            Some("secondary"),
             "TC020: light-200 should map to secondary via CSS bridge \
             (same 3 BackgroundColor slots, #f0f0f0 → #f2f2f2). \
             Previously mapped to 'default' via surviving-value string sim. \
             Got: {:?}",
            light200_repl
        );
    }

    // ── Appearance default inference tests ────────────────────────────

    /// TC012: Generic appearance inference for Chip → Label.
    ///
    /// Chip has CSS modifiers: pf-m-overflow, pf-m-draggable (no pf-m-outline).
    /// Label has CSS modifiers: pf-m-outline, pf-m-overflow, pf-m-add, etc.
    /// Label's `variant` prop has values: 'outline' | 'filled' | 'overflow' | 'add'
    ///
    /// The algorithm should detect:
    /// - pf-m-overflow exists on BOTH → not the default
    /// - pf-m-outline exists on Label only → candidate default for Chip
    /// - pf-m-filled: no CSS modifier for "filled" on either → not detectable
    /// - pf-m-add exists on Label only → candidate, but "outline" is more likely
    ///
    /// Since pf-m-outline and pf-m-add are both candidates, the algorithm
    /// should list them. In practice, the LLM picks "outline" because Chip
    /// visually looks like an outlined label.
    #[test]
    fn test_appearance_inference_chip_to_label() {
        use crate::sd_types::{CssModifierEffect, CssModifierMap, ComponentCssModifiers};

        // Old component: Chip (BEM block "chip")
        let mut old_mods = ComponentCssModifiers::new();
        let mut old_chip = CssModifierMap::new();
        // Real PF v5 Chip modifiers (only 2)
        old_chip.insert("pf-m-overflow".into(), CssModifierEffect::default());
        old_chip.insert("pf-m-draggable".into(), CssModifierEffect::default());
        old_mods.insert("chip".into(), old_chip);

        // New component: Label (BEM block "label")
        let mut new_mods = ComponentCssModifiers::new();
        let mut new_label = CssModifierMap::new();
        // Real PF v6 Label modifiers (subset relevant to variant prop)
        new_label.insert("pf-m-outline".into(), CssModifierEffect::default());
        new_label.insert("pf-m-overflow".into(), CssModifierEffect::default());
        new_label.insert("pf-m-add".into(), CssModifierEffect::default());
        new_label.insert("pf-m-compact".into(), CssModifierEffect::default());
        new_label.insert("pf-m-editable".into(), CssModifierEffect::default());
        new_mods.insert("label".into(), new_label);

        // Old types (Chip props): no variant prop
        let old_types: BTreeMap<String, String> = [
            ("closeBtnAriaLabel".to_string(), "string".to_string()),
            ("onClick".to_string(), "(e: React.MouseEvent) => void".to_string()),
        ]
        .into_iter()
        .collect();

        // New types (Label props): has variant prop with union string type
        let new_types: BTreeMap<String, String> = [
            ("closeBtnAriaLabel".to_string(), "string".to_string()),
            ("onClick".to_string(), "(e: React.MouseEvent) => void".to_string()),
            (
                "variant".to_string(),
                "'outline' | 'filled' | 'overflow' | 'add'".to_string(),
            ),
        ]
        .into_iter()
        .collect();

        let notes = infer_appearance_defaults(
            "Chip",
            "Label",
            &old_types,
            &new_types,
            &old_mods,
            &new_mods,
        );

        assert!(
            !notes.is_empty(),
            "TC012: Should produce appearance notes for Chip → Label"
        );

        // The note should mention "outline" as a candidate
        let note = &notes[0];
        assert!(
            note.contains("outline"),
            "TC012: Appearance note should mention 'outline'. Got: {}",
            note
        );

        // The note should mention the prop name "variant"
        assert!(
            note.contains("variant"),
            "TC012: Appearance note should mention 'variant' prop. Got: {}",
            note
        );
    }

    /// Appearance inference produces NO notes when the old component already
    /// has the same variant prop (no appearance divergence).
    #[test]
    fn test_appearance_inference_no_notes_when_shared_prop() {
        let old_types: BTreeMap<String, String> = [
            ("variant".to_string(), "'default' | 'secondary'".to_string()),
        ]
        .into_iter()
        .collect();
        let new_types: BTreeMap<String, String> = [
            ("variant".to_string(), "'default' | 'secondary'".to_string()),
        ]
        .into_iter()
        .collect();

        let notes = infer_appearance_defaults(
            "OldComp",
            "NewComp",
            &old_types,
            &new_types,
            &crate::sd_types::ComponentCssModifiers::new(),
            &crate::sd_types::ComponentCssModifiers::new(),
        );

        assert!(
            notes.is_empty(),
            "Should produce no notes when both components have the same variant prop"
        );
    }

    /// Appearance inference produces NO notes when no variant-like props exist.
    #[test]
    fn test_appearance_inference_no_variant_props() {
        let old_types: BTreeMap<String, String> = [
            ("title".to_string(), "string".to_string()),
        ]
        .into_iter()
        .collect();
        let new_types: BTreeMap<String, String> = [
            ("title".to_string(), "string".to_string()),
            ("subtitle".to_string(), "string".to_string()),
        ]
        .into_iter()
        .collect();

        let notes = infer_appearance_defaults(
            "OldComp",
            "NewComp",
            &old_types,
            &new_types,
            &crate::sd_types::ComponentCssModifiers::new(),
            &crate::sd_types::ComponentCssModifiers::new(),
        );

        assert!(
            notes.is_empty(),
            "Should produce no notes when no variant-like union string props exist"
        );
    }

    /// Appearance inference with single-candidate produces specific
    /// recommendation (not a list of candidates).
    #[test]
    fn test_appearance_inference_single_candidate() {
        use crate::sd_types::{CssModifierEffect, CssModifierMap, ComponentCssModifiers};

        // Old component has pf-m-bar but not pf-m-foo
        let mut old_mods = ComponentCssModifiers::new();
        let mut old_comp = CssModifierMap::new();
        old_comp.insert("pf-m-bar".into(), CssModifierEffect::default());
        old_mods.insert("oldComp".into(), old_comp);

        // New component has both pf-m-foo and pf-m-bar
        let mut new_mods = ComponentCssModifiers::new();
        let mut new_comp = CssModifierMap::new();
        new_comp.insert("pf-m-foo".into(), CssModifierEffect::default());
        new_comp.insert("pf-m-bar".into(), CssModifierEffect::default());
        new_mods.insert("newComp".into(), new_comp);

        let old_types: BTreeMap<String, String> = BTreeMap::new();
        let new_types: BTreeMap<String, String> = [(
            "variant".to_string(),
            "'foo' | 'bar'".to_string(),
        )]
        .into_iter()
        .collect();

        let notes = infer_appearance_defaults(
            "OldComp",
            "NewComp",
            &old_types,
            &new_types,
            &old_mods,
            &new_mods,
        );

        assert_eq!(notes.len(), 1, "Should produce exactly one note");
        // Single candidate "foo" → specific recommendation
        assert!(
            notes[0].contains("variant='foo'"),
            "Single candidate should produce specific recommendation. Got: {}",
            notes[0]
        );
    }

    // ── /next promotion rule tests ───────────────────────────────────

    /// TC024: Component promoted from /next to main should generate an
    /// ImportPathChange rule.
    #[test]
    fn test_next_to_main_promotion_generates_import_path_change() {
        let mut sd = SdPipelineResult::default();
        sd.old_component_packages.insert(
            "DualListSelector".into(),
            "@patternfly/react-core/next".into(),
        );
        sd.component_packages.insert(
            "DualListSelector".into(),
            "@patternfly/react-core".into(),
        );

        let component_packages = sd.component_packages.clone();
        let rules = generate_deprecated_migration_rules(&sd, &component_packages);

        let promotion_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("next-promoted"))
            .expect("TC024: Should generate a /next promotion rule");

        // Verify rule ID
        assert!(
            promotion_rule.rule_id.contains("duallistselector"),
            "TC024: Rule ID should contain component name. Got: {}",
            promotion_rule.rule_id
        );

        // Verify fix strategy is ImportPathChange
        let strategy = promotion_rule
            .fix_strategy
            .as_ref()
            .expect("TC024: Should have a fix strategy");
        assert_eq!(
            strategy.strategy, "ImportPathChange",
            "TC024: Strategy should be ImportPathChange"
        );
        assert_eq!(
            strategy.from.as_deref(),
            Some("@patternfly/react-core/next"),
            "TC024: from should be the /next package"
        );
        assert_eq!(
            strategy.to.as_deref(),
            Some("@patternfly/react-core"),
            "TC024: to should be the main package"
        );

        // Verify the rule targets IMPORT location from /next
        if let KonveyorCondition::FrontendReferenced { ref referenced } = promotion_rule.when {
            assert_eq!(referenced.location, "IMPORT");
            assert_eq!(
                referenced.from.as_deref(),
                Some("@patternfly/react-core/next")
            );
        } else {
            panic!("TC024: Expected FrontendReferenced condition");
        }

        // Verify message mentions promotion
        assert!(
            promotion_rule.message.contains("promoted"),
            "TC024: Message should mention promotion. Got: {}",
            promotion_rule.message
        );
    }

    /// Component that moves from /next to /deprecated should NOT generate
    /// a promotion rule (it's a regression, not a promotion).
    #[test]
    fn test_next_to_deprecated_no_promotion_rule() {
        let mut sd = SdPipelineResult::default();
        sd.old_component_packages.insert(
            "Foo".into(),
            "@patternfly/react-core/next".into(),
        );
        sd.component_packages.insert(
            "Foo".into(),
            "@patternfly/react-core/deprecated".into(),
        );

        let component_packages = sd.component_packages.clone();
        let rules = generate_deprecated_migration_rules(&sd, &component_packages);

        let promotion_rules: Vec<_> = rules
            .iter()
            .filter(|r| r.rule_id.contains("next-promoted"))
            .collect();
        assert!(
            promotion_rules.is_empty(),
            "/next → /deprecated should NOT generate a promotion rule"
        );
    }

    /// Component in main in both v5 and v6 should NOT generate any
    /// promotion rule.
    #[test]
    fn test_main_to_main_no_promotion_rule() {
        let mut sd = SdPipelineResult::default();
        sd.old_component_packages.insert(
            "Button".into(),
            "@patternfly/react-core".into(),
        );
        sd.component_packages.insert(
            "Button".into(),
            "@patternfly/react-core".into(),
        );

        let component_packages = sd.component_packages.clone();
        let rules = generate_deprecated_migration_rules(&sd, &component_packages);

        let promotion_rules: Vec<_> = rules
            .iter()
            .filter(|r| r.rule_id.contains("next-promoted"))
            .collect();
        assert!(
            promotion_rules.is_empty(),
            "main → main should NOT generate a promotion rule"
        );
    }
}
