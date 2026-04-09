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

use semver_analyzer_core::types::sd::{
    ChildRelationship, CompositionChangeType, CompositionTree, ConformanceCheck,
    ConformanceCheckType, SdPipelineResult, SourceLevelCategory, SourceLevelChange,
};
use semver_analyzer_core::{AnalysisReport, ApiChangeType};
use semver_analyzer_konveyor_core::{
    FixStrategyEntry, FrontendPatternFields, FrontendReferencedFields, KonveyorCondition,
    KonveyorRule,
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
        // ── Step 1: Build the `has_required_incoming` set.
        //
        // Members with at least one incoming Required non-internal edge have
        // a mandatory parent — the constraint is on where they must be PLACED
        // (notParent rule on the child).
        //
        // Members with zero Required incoming edges are "entry points" (the
        // family root or secondary roots like AlertGroup, JumpLinksList).
        // These can exist standalone — the constraint is on what they must
        // CONTAIN (requiresChild rule on the parent).
        //
        // Only Required edges count. An Allowed incoming edge (e.g., Tab→Tabs
        // for recursive nesting) doesn't make the child mandatory — it's a
        // valid-but-optional placement.
        let mut has_required_incoming: HashSet<&str> = HashSet::new();
        for edge in &tree.edges {
            if edge.relationship != ChildRelationship::Internal
                && edge.strength == semver_analyzer_core::types::sd::EdgeStrength::Required
            {
                has_required_incoming.insert(edge.child.as_str());
            }
        }

        // ── Step 2: Build parent → required children map.
        //
        // Only Required, non-internal edges. Determines WHICH parents need
        // conformance rules and what type:
        //   - Parent in no_incoming → requiresChild rule on the parent
        //   - Parent NOT in no_incoming → notParent rule on each child
        let mut parent_to_req_children: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in &tree.edges {
            if edge.strength == semver_analyzer_core::types::sd::EdgeStrength::Required
                && edge.relationship != ChildRelationship::Internal
            {
                parent_to_req_children
                    .entry(edge.parent.as_str())
                    .or_default()
                    .push(edge.child.as_str());
            }
        }

        // ── Step 3: Build child → all parents map.
        //
        // ALL non-internal edges (Required + Allowed). Used for the notParent
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

        // ── Step 4: Generate rules.
        //
        // For each parent with Required children, the rule type depends on
        // whether the parent has incoming edges:
        //
        //   no_incoming parent → requiresChild rule
        //     "If you use <AlertGroup>, it must contain <Alert> children"
        //     Scanner: pattern=AlertGroup, requiresChild=^(Alert)$
        //
        //   has_incoming parent → notParent rule on each child
        //     "Td must be inside Tr"
        //     Scanner: pattern=Td, notParent=^(Tr)$

        // 4a: Collect children that need notParent rules (parents with incoming edges).
        //     Group by child to merge parents into one regex.
        let mut children_needing_not_parent: HashSet<&str> = HashSet::new();
        for (parent, children) in &parent_to_req_children {
            if has_required_incoming.contains(parent) {
                for child in children {
                    children_needing_not_parent.insert(child);
                }
            }
        }

        // 4b: Generate notParent rules (child must be inside parent).
        for child in &children_needing_not_parent {
            let pkg = pkg_for(child, component_packages);

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

            // ── InvalidDirectChild: child inside grandparent, skipping parent.
            //
            // For each valid parent of this child, look up that parent's own
            // parents (grandparents of the child). Group by grandparent to
            // merge when multiple parents share the same grandparent (e.g.,
            // Tr in Table needs either Thead or Tbody).
            let mut grandparent_to_expected: HashMap<&str, Vec<&str>> = HashMap::new();
            for parent in &sorted_parents {
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
        // For parents in no_incoming (roots / secondary roots), the constraint
        // is "if you use this component, it must contain these children."
        for (parent, children) in &parent_to_req_children {
            if has_required_incoming.contains(parent) {
                continue; // handled above as notParent
            }

            let pkg = pkg_for(parent, component_packages);
            let mut sorted_children: Vec<&str> = children.clone();
            sorted_children.sort();
            sorted_children.dedup();

            let children_pattern = format!("^({})$", sorted_children.join("|"));
            let children_list = sorted_children.join(" or ");

            let rule_id_suffix = sorted_children
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
                parent, children_list, parent, sorted_children[0], parent,
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
                    replacement: Some(sorted_children[0].to_string()),
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
            let pkg = pkg_for(parent, component_packages);
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
const ROLE_QUERY_PATTERN: &str =
    "^(getByRole|queryByRole|findByRole|getAllByRole|queryAllByRole|findAllByRole)$";
const LABEL_QUERY_PATTERN: &str =
    "^(getByLabelText|queryByLabelText|findByLabelText|getAllByLabelText|queryAllByLabelText|findAllByLabelText)$";
const TEST_FILE_PATTERN: &str = ".*\\.(test|spec)\\.(ts|tsx|js|jsx)$";

/// Map HTML element names to their implicit ARIA roles.
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

/// Check if a value is a concrete string literal (not a JSX expression).
fn is_concrete_value(value: &str) -> bool {
    !value.starts_with('{') && value != "true" && value != "false"
}

fn generate_test_impact_rules(
    changes: &[SourceLevelChange],
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    for change in changes {
        if !change.has_test_implications {
            continue;
        }

        let pkg = pkg_for(&change.component, component_packages);

        match change.category {
            // ── Role changes: match getByRole('oldValue') ───────────
            SourceLevelCategory::RoleChange => {
                // Role removed — tests using getByRole('X') will break
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
                }
            }

            // ── ARIA label changes: match getByLabelText('oldValue') ─
            SourceLevelCategory::AriaChange => {
                // Only generate rules for aria-label changes (not aria-hidden, etc.)
                if !change.description.contains("aria-label") {
                    continue;
                }

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
            }

            // ── DOM structure changes: match getByRole(implicit_role) ─
            SourceLevelCategory::DomStructure => {
                // Element removed — tests using getByRole for its implicit
                // role may break (e.g., <button> removed → getByRole('button'))
                if let Some(ref old_val) = change.old_value {
                    // Extract element name from values like "<button>" or "<button> (×2)"
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

            _ => {}
        }
    }

    rules
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
                    let attrs: Vec<String> =
                        parts[1].split(", ").map(|s| s.trim().to_string()).collect();
                    (parts[0].to_string(), attrs)
                } else {
                    continue;
                }
            }
            None => continue,
        };

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
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Dropdown".into(),
                    child: "DropdownList".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "DropdownList".into(),
                    child: "DropdownItem".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
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
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Tabs".into(),
                    child: "Tab".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                // Recursive nesting: nested tabs inside a tab (Allowed, not Required)
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Tab".into(),
                    child: "Tabs".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Allowed,
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
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Table".into(),
                    child: "Thead".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Table".into(),
                    child: "Tbody".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Thead".into(),
                    child: "Tr".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Tbody".into(),
                    child: "Tr".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Tr".into(),
                    child: "Td".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Tr".into(),
                    child: "Th".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
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
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Table".into(),
                    child: "Thead".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Table".into(),
                    child: "Tbody".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Thead".into(),
                    child: "Tr".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Tbody".into(),
                    child: "Tr".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
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
        let tree = CompositionTree {
            root: "Dropdown".into(),
            family_members: vec![
                "Dropdown".into(),
                "DropdownList".into(),
                "DropdownItem".into(),
            ],
            edges: vec![
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Dropdown".into(),
                    child: "DropdownList".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "DropdownList".into(),
                    child: "DropdownItem".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &test_pkg_map());

        // Dropdown is root (no incoming) → requiresChild rule
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
            assert_eq!(
                referenced.requires_child.as_deref(),
                Some("^(DropdownList)$"),
            );
            assert!(referenced.not_parent.is_none());
        } else {
            panic!("Expected FrontendReferenced condition");
        }

        // DropdownItem has incoming edge (from DropdownList) → notParent rule
        let di_rule = rules.iter().find(|r| r.rule_id.contains("item-in-list"));
        assert!(
            di_rule.is_some(),
            "Expected notParent rule for DropdownItem. Got rules: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
        if let KonveyorCondition::FrontendReferenced { referenced } = &di_rule.unwrap().when {
            assert_eq!(referenced.pattern, "^DropdownItem$");
            assert_eq!(referenced.not_parent.as_deref(), Some("^DropdownList$"),);
        } else {
            panic!("Expected FrontendReferenced condition");
        }

        // NO notParent rule for DropdownList (its parent Dropdown is no_incoming)
        assert!(
            !rules.iter().any(|r| r.rule_id.contains("list-in")),
            "DropdownList should not have a notParent rule (parent is root/no_incoming)"
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
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Table".into(),
                    child: "Thead".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Table".into(),
                    child: "Tbody".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                // Tbody→Tr is Required (e.g., CSS direct-child selector)
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Tbody".into(),
                    child: "Tr".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                // Thead→Tr is Allowed (e.g., CSS descendant selector)
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Thead".into(),
                    child: "Tr".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Allowed,
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

        // InvalidDirectChild rule should also mention both parents
        let tr_not_in_table = rules.iter().find(|r| r.rule_id.contains("tr-not-in-table"));
        if let Some(idc_rule) = tr_not_in_table {
            assert!(
                idc_rule.description.contains("Tbody") && idc_rule.description.contains("Thead"),
                "InvalidDirectChild description should mention both parents: {}",
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
            edges: vec![semver_analyzer_core::types::sd::CompositionEdge {
                parent: "Menu".into(),
                child: "MenuContent".into(),
                relationship: ChildRelationship::DirectChild,
                required: false,
                bem_evidence: None,
                strength: semver_analyzer_core::types::sd::EdgeStrength::Allowed,
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

        let tree = CompositionTree {
            root: "Alert".into(),
            family_members: vec![
                "Alert".into(),
                "AlertGroup".into(),
                "AlertActionCloseButton".into(),
            ],
            edges: vec![
                // AlertGroup is a secondary root — no incoming edges
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "AlertGroup".into(),
                    child: "Alert".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "AlertGroup".into(),
                    child: "AlertActionCloseButton".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &[], &pkgs);

        // AlertGroup should get a requiresChild rule
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

        // Alert should NOT get a notParent rule — its only Required parent
        // is AlertGroup which is no_incoming (secondary root)
        assert!(
            !rules.iter().any(|r| r.rule_id.contains("alert-in")),
            "Alert should NOT have a notParent rule. Got: {:?}",
            rules.iter().map(|r| &r.rule_id).collect::<Vec<_>>()
        );
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
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Table".into(),
                    child: "Tbody".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Tbody".into(),
                    child: "Tr".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Tr".into(),
                    child: "Td".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
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

        // Tbody should NOT have a notParent rule (parent is Table = no_incoming)
        assert!(
            !rules.iter().any(|r| r.rule_id.contains("tbody-in")),
            "Tbody should NOT have notParent (parent Table is no_incoming)"
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
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "Accordion".into(),
                    child: "AccordionItem".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
                },
                // Internal rendering: AccordionItem renders AccordionContent
                semver_analyzer_core::types::sd::CompositionEdge {
                    parent: "AccordionItem".into(),
                    child: "AccordionContent".into(),
                    relationship: ChildRelationship::Internal,
                    required: false,
                    bem_evidence: None,
                    strength: semver_analyzer_core::types::sd::EdgeStrength::Required,
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
}
