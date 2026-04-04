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
    ChildRelationship, CompositionChange, CompositionChangeType, CompositionTree, ConformanceCheck,
    ConformanceCheckType, SdPipelineResult, SourceLevelCategory, SourceLevelChange,
};
use semver_analyzer_core::{AnalysisReport, ApiChange, ApiChangeType, FileChanges};
use semver_analyzer_konveyor_core::{
    FixStrategyEntry, FrontendReferencedFields, KonveyorCondition, KonveyorRule,
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
    rules.extend(generate_composition_change_rules(
        &sd.composition_changes,
        &component_packages,
    ));

    // ── Conformance rules ───────────────────────────────────────────
    rules.extend(generate_conformance_rules(
        &sd.composition_trees,
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

    // ── Deprecated↔main migration rules ─────────────────────────────
    rules.extend(generate_deprecated_migration_rules(sd, &component_packages));

    // ── Suppress conformance rules covered by other rule types ──────
    //
    // Conformance rules ("ModalHeader must be inside Modal") can't
    // actually validate parent relationships yet (Phase 8 provider work).
    // They fire on every instance of the component, creating noise when
    // composition or prop→child rules already cover the same component.
    // Suppress them for components that have actionable rules.
    let covered_components: HashSet<String> = rules
        .iter()
        .filter(|r| {
            r.labels.iter().any(|l| {
                l == "change-type=composition"
                    || l == "change-type=prop-to-child"
                    || l == "change-type=child-to-prop"
                    || l == "change-type=deprecated-migration"
            })
        })
        .filter_map(|r| {
            r.labels
                .iter()
                .find(|l| l.starts_with("family="))
                .map(|l| l.strip_prefix("family=").unwrap_or(l).to_string())
        })
        .collect();

    if !covered_components.is_empty() {
        let before = rules.len();
        rules.retain(|r| {
            if !r.labels.iter().any(|l| l == "change-type=conformance") {
                return true; // keep non-conformance rules
            }
            // Check if this conformance rule's family is covered
            let family = r
                .labels
                .iter()
                .find(|l| l.starts_with("family="))
                .and_then(|l| l.strip_prefix("family="));
            match family {
                Some(f) => !covered_components.contains(f),
                None => true, // keep rules without family label
            }
        });
        let suppressed = before - rules.len();
        if suppressed > 0 {
            tracing::info!(
                suppressed,
                covered_families = covered_components.len(),
                "Conformance rules suppressed (covered by composition/prop-to-child rules)"
            );
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

// ── Composition change rules ────────────────────────────────────────────

fn generate_composition_change_rules(
    changes: &[CompositionChange],
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    for change in changes {
        match &change.change_type {
            CompositionChangeType::NewRequiredChild {
                parent,
                new_child,
                wraps,
            } => {
                let rule_id = format!(
                    "sd-composition-{}-requires-{}",
                    sanitize(parent),
                    sanitize(new_child)
                );

                let mut message = format!(
                    "<{}> now requires <{}> as a child component.\n",
                    parent, new_child
                );
                if let Some(ref after) = change.after_pattern {
                    message.push_str(&format!("\nExpected pattern:\n{}\n", after));
                }
                if !wraps.is_empty() {
                    message.push_str(&format!("\n<{}> wraps: {}\n", new_child, wraps.join(", ")));
                }

                let pkg = pkg_for(parent, component_packages);
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
                    message,
                    links: vec![],
                    when: KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: format!("^{}$", parent),
                            location: "JSX_COMPONENT".into(),
                            component: None,
                            parent: None,
                            not_parent: None,
                            parent_from: None,
                            value: None,
                            from: Some(pkg.clone()),
                        },
                    },
                    fix_strategy: Some(FixStrategyEntry {
                        strategy: "CompositionChange".into(),
                        component: Some(parent.clone()),
                        replacement: Some(new_child.clone()),
                        ..Default::default()
                    }),
                });
            }
            CompositionChangeType::FamilyMemberAdded { member } => {
                let pkg = pkg_for(member, component_packages);
                let rule_id = format!(
                    "sd-composition-{}-new-member-{}",
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
                    effort: 1,
                    category: "optional".into(),
                    description: change.description.clone(),
                    message: format!(
                        "<{}> is a new component in the {} family.\n\
                         Consider using it for better structure and semantics.",
                        member, change.family
                    ),
                    links: vec![],
                    when: KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: format!("^{}$", change.family),
                            location: "JSX_COMPONENT".into(),
                            component: None,
                            parent: None,
                            not_parent: None,
                            parent_from: None,
                            value: None,
                            from: Some(pkg.to_string()),
                        },
                    },
                    fix_strategy: None,
                });
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
                            not_parent: None,
                            parent_from: None,
                            value: None,
                            from: Some(pkg.to_string()),
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

fn generate_conformance_rules(
    trees: &[CompositionTree],
    component_packages: &HashMap<String, String>,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();

    for tree in trees {
        // Build parent lookup for InvalidDirectChild detection
        let mut child_to_parents: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in &tree.edges {
            child_to_parents
                .entry(edge.child.as_str())
                .or_default()
                .push(edge.parent.as_str());
        }

        for edge in &tree.edges {
            // Skip internal rendering edges — not consumer-facing
            if edge.relationship == ChildRelationship::Internal {
                continue;
            }

            let pkg = pkg_for(&edge.child, component_packages);

            // ── InvalidDirectChild: child inside grandparent, skipping parent
            if let Some(grandparents) = child_to_parents.get(edge.parent.as_str()) {
                for grandparent in grandparents {
                    let rule_id = format!(
                        "sd-conformance-{}-not-in-{}-use-{}",
                        sanitize(&edge.child),
                        sanitize(grandparent),
                        sanitize(&edge.parent),
                    );

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
                            "<{}> must be inside <{}>, not directly in <{}>",
                            edge.child, edge.parent, grandparent
                        ),
                        message: format!(
                            "<{}> should be wrapped in <{}> inside <{}>.\n\n\
                             Replace:\n  <{}>\n    <{} />\n  </{}>\n\n\
                             With:\n  <{}>\n    <{}>\n      <{} />\n    </{}>\n  </{}>",
                            edge.child,
                            edge.parent,
                            grandparent,
                            grandparent,
                            edge.child,
                            grandparent,
                            grandparent,
                            edge.parent,
                            edge.child,
                            edge.parent,
                            grandparent,
                        ),
                        links: vec![],
                        when: KonveyorCondition::FrontendReferenced {
                            referenced: FrontendReferencedFields {
                                pattern: format!("^{}$", edge.child),
                                location: "JSX_COMPONENT".into(),
                                component: None,
                                parent: Some(format!("^{}$", grandparent)),
                                not_parent: None,
                                parent_from: Some(pkg.to_string()),
                                value: None,
                                from: Some(pkg.to_string()),
                            },
                        },
                        fix_strategy: Some(FixStrategyEntry {
                            strategy: "CompositionChange".into(),
                            component: Some(edge.child.clone()),
                            replacement: Some(edge.parent.clone()),
                            ..Default::default()
                        }),
                    });
                }
            }

            // ── Must-be-inside: child must have parent as ancestor
            // Fire on every usage of the child. The fix-engine checks
            // whether the parent constraint is satisfied.
            // When kantra supports `not` on `frontend.referenced`, this
            // can be made precise (fire only on violations).
            let rule_id = format!(
                "sd-conformance-{}-must-be-in-{}",
                sanitize(&edge.child),
                sanitize(&edge.parent),
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
                description: format!("<{}> must be a child of <{}>", edge.child, edge.parent),
                message: format!(
                    "<{}> must be used inside <{}>.\n\n\
                     Correct usage:\n  <{}>\n    <{} />\n  </{}>",
                    edge.child, edge.parent, edge.parent, edge.child, edge.parent,
                ),
                links: vec![],
                // For now, fires on all usages. Phase 8 adds `not` parent
                // negation so this only fires on violations.
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: format!("^{}$", edge.child),
                        location: "JSX_COMPONENT".into(),
                        component: None,
                        parent: None,
                        not_parent: None,
                        parent_from: None,
                        value: None,
                        from: Some(pkg.to_string()),
                    },
                },
                fix_strategy: None,
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
        let rule_id = format!(
            "sd-context-{}-{}",
            sanitize(&change.component),
            sanitize(ctx_name),
        );

        // Fire on import of the context — consumers who directly import
        // and use the context are affected.
        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".into(),
                "change-type=context-dependency".into(),
                format!("package={}", pkg),
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
                    not_parent: None,
                    parent_from: None,
                    value: None,
                    from: Some(pkg.to_string()),
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
                    match &change.change {
                        ApiChangeType::Removed => {
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
                        _ => {}
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
    if let Some(new_surface) = report.changes.first() {
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
                        message: format!(
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
                        ),
                        links: vec![],
                        when: KonveyorCondition::FrontendReferenced {
                            referenced: FrontendReferencedFields {
                                pattern: format!("^{}$", removed.name),
                                location: "JSX_PROP".into(),
                                component: Some(format!("^{}$", tree.root)),
                                parent: None,
                                not_parent: None,
                                parent_from: None,
                                value: None,
                                from: Some(pkg.to_string()),
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
                                        parent_from: None,
                                        value: None,
                                        from: Some(pkg.to_string()),
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
                                not_parent: None,
                                parent_from: Some(pkg.clone()),
                                value: None,
                                from: Some(pkg.clone()),
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
    component_packages: &HashMap<String, String>,
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
                            not_parent: None,
                            parent_from: None,
                            value: None,
                            from: Some(old_pkg.clone()),
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
                        not_parent: None,
                        parent_from: None,
                        value: None,
                        from: Some(deprecated_pkg.clone()),
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
    component: String,
    is_reactnode: bool,
    before_type: Option<String>,
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
                },
                crate::sd_types::CompositionEdge {
                    parent: "DropdownList".into(),
                    child: "DropdownItem".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                },
            ],
        };

        let rules = generate_conformance_rules(&[tree], &test_pkg_map());

        // Should have an InvalidDirectChild rule: DropdownItem in Dropdown
        let invalid_rule = rules
            .iter()
            .find(|r| r.rule_id.contains("dropdownitem-not-in-dropdown"));
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
}
