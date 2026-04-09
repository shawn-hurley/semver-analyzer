//! Composition tree derivation from component source profiles.
//!
//! Builds a `CompositionTree` for a component family by combining:
//! 1. Family member identification (from index file exports)
//! 2. Children slot tracing (where `{children}` lands in each component)
//! 3. BEM token analysis (structural parent-child relationships)
//! 4. Rendered components (which components each family member renders internally)
//!
//! The resulting tree describes the expected JSX composition structure
//! for consumers of the component family.

use crate::css_profile::CssBlockProfile;
use semver_analyzer_core::types::sd::{
    ChildRelationship, ComponentSourceProfile, CompositionEdge, CompositionTree, EdgeStrength,
};
use std::collections::{HashMap, HashSet};
use tracing::debug;

// ── Evidence-based composition tree builder ─────────────────────────────

/// Context for projecting a delegate family's composition tree edges onto
/// a wrapper family. Used when a family like Dropdown wraps another family
/// like Menu — each Dropdown component is a thin wrapper around a Menu
/// counterpart (DropdownList wraps MenuList, DropdownItem wraps MenuItem).
///
/// The delegate tree's edges are projected onto the wrapper family so that
/// composition constraints (context, DOM nesting, CSS) are inherited.
pub struct DelegateContext<'a> {
    /// The delegate family's resolved composition tree.
    pub delegate_tree: &'a CompositionTree,
    /// Mapping: wrapper component name → delegate component name.
    /// E.g., "DropdownList" → "MenuList", "DropdownItem" → "MenuItem".
    pub wrapper_to_delegate: HashMap<String, String>,
}

/// Build a composition tree using CSS structure, React patterns, and HTML
/// semantics instead of BEM-based edge creation.
///
/// BEM determines family membership only. All parent-child edges come from:
/// 1. Internal rendering (A renders B in JSX)
/// 1.5. Delegate tree projection (edges from a delegate family's tree)
/// 2. CSS direct-child selectors (`.A > .B`)
/// 3. CSS grid parent-child (`A` has grid-template, `B` has grid-column)
/// 4. CSS flex context (A wraps children in flex container, B is not a grid child)
/// 5. CSS descendant selectors (`.A .B`)
/// 5.5. CSS layout children (shared CSS rule with flex-wrap/gap implies containment)
/// 6. React context (A provides, B consumes)
/// 7. DOM nesting (A wraps children in `<ul>`, B renders `<li>`)
/// 8. cloneElement threading (A injects props into children that B declares)
/// 8.5. BEM element orphan fallback (orphan BEM elements → root→member)
/// 8.6. Secondary BEM block sub-root fallback
/// 8.7. Prop-passed detection (ReactNode/ReactElement props → PropPassed edges)
/// 9. Suppress root edges when intermediate exists
/// 10. Drop unconnected members (exported orphans are retained)
pub fn build_composition_tree_v2(
    profiles: &HashMap<String, ComponentSourceProfile>,
    family_exports: &[String],
    css_profiles: Option<&HashMap<String, CssBlockProfile>>,
    primary_css_block: Option<&str>,
    delegate_contexts: &[DelegateContext<'_>],
    // Barrel-file exports — components exported in `index.ts`. Members in
    // this set are retained even with zero edges (as orphans). If `None`,
    // Step 10 drops all zero-edge members (legacy behavior).
    exported_members: Option<&[String]>,
) -> Option<CompositionTree> {
    if family_exports.is_empty() {
        return None;
    }

    let root = family_exports[0].clone();
    let family_set: HashSet<&str> = family_exports.iter().map(|s| s.as_str()).collect();

    let mut tree = CompositionTree {
        root: root.clone(),
        family_members: family_exports.to_vec(),
        edges: Vec::new(),
    };

    // Track existing edges for O(1) dedup lookups instead of linear scan
    let mut edge_set: HashSet<(String, String)> = HashSet::new();

    // Resolve the primary CSS profile from the profiles map.
    let css_profile = primary_css_block.and_then(|key| css_profiles?.get(key));

    // Build CSS element → component mapping for CSS-based steps.
    // Maps a CSS BEM element name (e.g., "content-section") to the component
    // that uses the corresponding `styles.xxx` token.
    let css_to_component = if let Some(css_prof) = css_profile {
        build_css_element_to_component_map(profiles, family_exports, &css_prof.block)
    } else {
        HashMap::new()
    };

    // ── Step 1: Internal rendering ──────────────────────────────────
    for parent_name in family_exports {
        let Some(parent_profile) = profiles.get(parent_name) else {
            continue;
        };
        for rendered in &parent_profile.rendered_components {
            if family_set.contains(rendered.as_str()) {
                let key = (parent_name.clone(), rendered.clone());
                if edge_set.insert(key) {
                    tree.edges.push(CompositionEdge {
                        parent: parent_name.clone(),
                        child: rendered.clone(),
                        relationship: ChildRelationship::Internal,
                        required: true,
                        bem_evidence: Some("internally rendered".to_string()),
                        strength: EdgeStrength::Required,
                        prop_name: None,
                    });
                }
            }
        }
    }

    // ── Step 1.5: Delegate tree projection ──────────────────────────
    // For wrapper families (e.g., Dropdown wraps Menu), project edges
    // from the delegate family's tree onto this tree. Each edge in the
    // delegate tree where BOTH parent and child have wrapper mappings
    // produces a corresponding edge in this tree.
    //
    // This runs before Step 10 so projected edges prevent members from
    // being dropped. Strength is Allowed because the delegation itself
    // is a design choice — the underlying constraints are Required in
    // the delegate family but optional at the wrapper level.
    for ctx in delegate_contexts {
        // Build reverse map: delegate component → wrapper component
        let delegate_to_wrapper: HashMap<&str, &str> = ctx
            .wrapper_to_delegate
            .iter()
            .map(|(w, d)| (d.as_str(), w.as_str()))
            .collect();

        for edge in &ctx.delegate_tree.edges {
            let Some(&wrapper_parent) = delegate_to_wrapper.get(edge.parent.as_str()) else {
                continue;
            };
            let Some(&wrapper_child) = delegate_to_wrapper.get(edge.child.as_str()) else {
                continue;
            };

            // Both must be in this family
            if !family_set.contains(wrapper_parent) || !family_set.contains(wrapper_child) {
                continue;
            }
            // Skip self-edges
            if wrapper_parent == wrapper_child {
                continue;
            }

            let key = (wrapper_parent.to_string(), wrapper_child.to_string());
            if edge_set.insert(key) {
                debug!(
                    parent = %wrapper_parent,
                    child = %wrapper_child,
                    delegate_parent = %edge.parent,
                    delegate_child = %edge.child,
                    delegate_family = %ctx.delegate_tree.root,
                    "delegate tree projection"
                );
                tree.edges.push(CompositionEdge {
                    parent: wrapper_parent.to_string(),
                    child: wrapper_child.to_string(),
                    relationship: edge.relationship.clone(),
                    required: false,
                    bem_evidence: Some(format!(
                        "Delegate projection from {} tree: {} wraps {}, {} wraps {}",
                        ctx.delegate_tree.root,
                        wrapper_parent,
                        edge.parent,
                        wrapper_child,
                        edge.child,
                    )),
                    strength: EdgeStrength::Allowed,
                    prop_name: None,
                });
            }
        }
    }

    if let Some(css_prof) = css_profile {
        // ── Step 2: CSS direct-child selectors ──────────────────────
        for (css_parent, css_child) in &css_prof.direct_child_nesting {
            let Some(parent_comps) = css_to_component.get(css_parent.as_str()) else {
                continue;
            };
            let Some(child_comps) = css_to_component.get(css_child.as_str()) else {
                continue;
            };
            // When an element maps to multiple components, all edges from
            // that element are Allowed — the CSS class is ambiguous across
            // components and could be either one.
            let parent_ambiguous = parent_comps.len() > 1;
            let child_ambiguous = child_comps.len() > 1;
            for parent_comp in parent_comps {
                for child_comp in child_comps {
                    let key = (parent_comp.clone(), child_comp.clone());
                    if parent_comp != child_comp && edge_set.insert(key) {
                        // If the reverse edge already exists (child→parent from
                        // a prior step), this creates a bidirectional pair.
                        // Bidirectional CSS relationships represent optional
                        // recursive nesting (e.g., WizardNavItem > WizardNav
                        // for sub-navigation), not mandatory containment.
                        let reverse_key = (child_comp.clone(), parent_comp.clone());
                        let has_reverse = edge_set.contains(&reverse_key);
                        let strength = if *child_comp == root
                            || parent_ambiguous
                            || child_ambiguous
                            || has_reverse
                        {
                            EdgeStrength::Allowed
                        } else {
                            EdgeStrength::Required
                        };
                        tree.edges.push(CompositionEdge {
                            parent: parent_comp.clone(),
                            child: child_comp.clone(),
                            relationship: ChildRelationship::DirectChild,
                            required: false,
                            bem_evidence: Some(format!(
                                "CSS direct child: .{} > .{}",
                                css_parent, css_child
                            )),
                            strength,
                            prop_name: None,
                        });
                    }
                }
            }
        }

        // ── Step 3: CSS grid parent-child ───────────────────────────
        // Find grid containers (has_grid_template) and grid children
        // (has_grid_column/grid_row). Map to components.
        // With multi-component mapping, an element may map to multiple
        // components — expand each to (element, component) pairs.
        let grid_containers: Vec<(&str, String)> = css_prof
            .elements
            .iter()
            .filter(|(_, info)| info.has_grid_template && info.display_values.contains("grid"))
            .flat_map(|(el, _)| {
                css_to_component
                    .get(el.as_str())
                    .into_iter()
                    .flat_map(move |comps| {
                        comps.iter().map(move |comp| (el.as_str(), comp.clone()))
                    })
            })
            .collect();

        for (child_el, child_info) in &css_prof.elements {
            if !child_info.has_grid_column && !child_info.has_grid_row {
                continue;
            }
            let Some(child_comps) = css_to_component.get(child_el.as_str()) else {
                continue;
            };
            let child_ambiguous = child_comps.len() > 1;

            for child_comp in child_comps {
                // Find the best grid container for this child.
                let mut best_parent: Option<&str> = None;

                for (container_el, container_comp) in &grid_containers {
                    if *container_comp == *child_comp {
                        continue;
                    }
                    if css_prof
                        .direct_child_nesting
                        .contains(&(container_el.to_string(), child_el.clone()))
                    {
                        best_parent = Some(container_comp);
                        break;
                    }
                }

                if best_parent.is_none() {
                    for (container_el, container_comp) in &grid_containers {
                        if *container_comp == *child_comp {
                            continue;
                        }
                        if css_prof
                            .descendant_nesting
                            .contains(&(container_el.to_string(), child_el.clone()))
                        {
                            best_parent = Some(container_comp);
                            break;
                        }
                    }
                }

                if best_parent.is_none() && grid_containers.len() == 1 {
                    let (_, ref container_comp) = grid_containers[0];
                    if *container_comp != *child_comp {
                        best_parent = Some(container_comp);
                    }
                }

                if let Some(parent_comp) = best_parent {
                    let key = (parent_comp.to_string(), child_comp.clone());
                    if edge_set.insert(key) {
                        let strength = if *child_comp == root || child_ambiguous {
                            EdgeStrength::Allowed
                        } else {
                            EdgeStrength::Required
                        };
                        tree.edges.push(CompositionEdge {
                            parent: parent_comp.to_string(),
                            child: child_comp.clone(),
                            relationship: ChildRelationship::DirectChild,
                            required: false,
                            bem_evidence: Some(format!(
                                "CSS grid: {} has grid-template, {} has grid-column/row",
                                parent_comp, child_comp
                            )),
                            strength,
                            prop_name: None,
                        });
                    }
                }
            }
        }

        // Step 3b: Implicit grid children — elements inside a non-root
        // grid container that don't have explicit grid-column/grid-row.
        // Example: DescriptionListTerm and DescriptionListDescription are
        // implicit grid children of DescriptionListGroup (which has
        // grid-template-rows).
        //
        // Only applies to non-root grid containers (containers that are
        // themselves grid children of the root grid).
        let non_root_grid_containers: Vec<(&str, String)> = grid_containers
            .iter()
            .filter(|(el, _)| {
                // Must not be root and must itself be a grid child
                !el.is_empty()
                    && css_prof
                        .elements
                        .get(*el)
                        .is_some_and(|info| info.has_grid_column || info.has_grid_row)
            })
            .cloned()
            .collect();

        if !non_root_grid_containers.is_empty() {
            for (child_el, child_info) in &css_prof.elements {
                // Skip elements that already have grid positioning (handled above)
                if child_info.has_grid_column || child_info.has_grid_row {
                    continue;
                }
                // Skip root element
                if child_el.is_empty() {
                    continue;
                }
                // Skip elements that are grid containers themselves
                if child_info.has_grid_template {
                    continue;
                }
                let Some(child_comps) = css_to_component.get(child_el.as_str()) else {
                    continue;
                };
                let child_ambiguous = child_comps.len() > 1;

                for child_comp in child_comps {
                    // Skip if already has a non-root parent
                    if tree
                        .edges
                        .iter()
                        .any(|e| e.child == *child_comp && e.parent != root)
                    {
                        continue;
                    }

                    let mut best_parent: Option<&str> = None;

                    for (container_el, container_comp) in &non_root_grid_containers {
                        if *container_comp == *child_comp {
                            continue;
                        }
                        if css_prof
                            .direct_child_nesting
                            .contains(&(container_el.to_string(), child_el.clone()))
                            || css_prof
                                .descendant_nesting
                                .contains(&(container_el.to_string(), child_el.clone()))
                        {
                            best_parent = Some(container_comp);
                            break;
                        }
                    }

                    if best_parent.is_none() && non_root_grid_containers.len() == 1 {
                        let (_, ref comp) = non_root_grid_containers[0];
                        if *comp != *child_comp {
                            best_parent = Some(comp);
                        }
                    }

                    if let Some(parent_comp) = best_parent {
                        let key = (parent_comp.to_string(), child_comp.clone());
                        if edge_set.insert(key) {
                            let strength = if *child_comp == root || child_ambiguous {
                                EdgeStrength::Allowed
                            } else {
                                EdgeStrength::Required
                            };
                            tree.edges.push(CompositionEdge {
                                parent: parent_comp.to_string(),
                                child: child_comp.clone(),
                                relationship: ChildRelationship::DirectChild,
                                required: false,
                                bem_evidence: Some(format!(
                                    "CSS grid: {} is grid container, {} is implicit grid child",
                                    parent_comp, child_comp
                                )),
                                strength,
                                prop_name: None,
                            });
                        }
                    }
                }
            }
        }

        // ── Step 4: CSS flex context ────────────────────────────────
        // Only fires when the ROOT component's CSS slot is a grid container.
        // In that case, family members WITHOUT grid positioning can't be
        // direct children of root — they need a flex intermediary.
        //
        // Example: Toolbar root is display:grid. ToolbarContent wraps
        // children in content-section (display:flex). ToolbarItem has no
        // grid-column so it goes under ToolbarContent, not Toolbar.
        let root_is_grid = {
            let root_css = css_prof.elements.get("");
            root_css
                .is_some_and(|info| info.display_values.contains("grid") && info.has_grid_template)
        };

        if root_is_grid {
            // Find non-root components whose children_slot is a flex container
            let flex_parents: Vec<(String, String)> = family_exports
                .iter()
                .filter(|name| **name != root)
                .filter_map(|name| {
                    let prof = profiles.get(name)?;
                    if !prof.has_children_prop {
                        return None;
                    }
                    let innermost_token = prof
                        .children_slot_detail
                        .iter()
                        .rev()
                        .find_map(|(_, token)| token.as_ref())?;

                    let block_camel = &css_prof.block;
                    let element_camel = innermost_token.strip_prefix(block_camel.as_str())?;
                    if element_camel.is_empty() {
                        return None;
                    }
                    let element_camel_lower = {
                        let mut s = element_camel.to_string();
                        if let Some(c) = s.get_mut(0..1) {
                            c.make_ascii_lowercase();
                        }
                        s
                    };
                    let element_kebab = camel_to_kebab(&element_camel_lower);

                    let css_info = css_prof.elements.get(&element_kebab)?;
                    if css_info.display_values.contains("flex") {
                        Some((name.clone(), element_kebab))
                    } else {
                        None
                    }
                })
                .collect();

            if !flex_parents.is_empty() {
                for child_name in family_exports {
                    if child_name == &root {
                        continue;
                    }

                    // Skip children that already have a non-root parent
                    if tree
                        .edges
                        .iter()
                        .any(|e| e.child == *child_name && e.parent != root)
                    {
                        continue;
                    }

                    // Skip flex parents themselves (they're grid children of root)
                    if flex_parents.iter().any(|(p, _)| p == child_name) {
                        continue;
                    }

                    // Skip children whose CSS element has grid positioning
                    let child_is_grid = profiles.get(child_name).is_some_and(|cp| {
                        cp.css_tokens_used.iter().any(|token| {
                            // Strip "styles." prefix, skip modifiers
                            let raw = if let Some(rest) = token.strip_prefix("styles.") {
                                if rest.starts_with("modifiers.") {
                                    return false;
                                }
                                rest
                            } else {
                                token.as_str()
                            };
                            let block_camel = &css_prof.block;
                            if let Some(suffix) = raw.strip_prefix(block_camel.as_str()) {
                                if suffix.is_empty() {
                                    return false;
                                }
                                let mut el = suffix.to_string();
                                if let Some(c) = el.get_mut(0..1) {
                                    c.make_ascii_lowercase();
                                }
                                let el_kebab = camel_to_kebab(&el);
                                if let Some(info) = css_prof.elements.get(&el_kebab) {
                                    return info.has_grid_column || info.has_grid_row;
                                }
                            }
                            false
                        })
                    });

                    if child_is_grid {
                        continue;
                    }

                    // Match to best flex parent. Prefer one with existing edge
                    // from another signal, then longest CSS element name.
                    let best = flex_parents
                        .iter()
                        .filter(|(p, _)| p != child_name)
                        .max_by_key(|(p, el)| {
                            let has_other_edge = tree
                                .edges
                                .iter()
                                .any(|e| e.parent == *p && e.child == *child_name);
                            (has_other_edge as usize, el.len())
                        });

                    if let Some((best_parent, _)) = best {
                        let key = (best_parent.clone(), child_name.clone());
                        if edge_set.insert(key) {
                            tree.edges.push(CompositionEdge {
                                parent: best_parent.clone(),
                                child: child_name.clone(),
                                relationship: ChildRelationship::DirectChild,
                                required: false,
                                bem_evidence: Some(format!(
                                    "CSS flex context: {} wraps children in flex, root is grid",
                                    best_parent
                                )),
                                strength: EdgeStrength::Allowed,
                                prop_name: None,
                            });
                        }
                    }
                }
            }
        }

        // ── Step 5: CSS descendant selectors ────────────────────────
        for (css_parent, css_child) in &css_prof.descendant_nesting {
            let Some(parent_comps) = css_to_component.get(css_parent.as_str()) else {
                continue;
            };
            let Some(child_comps) = css_to_component.get(css_child.as_str()) else {
                continue;
            };
            for parent_comp in parent_comps {
                for child_comp in child_comps {
                    let key = (parent_comp.clone(), child_comp.clone());
                    if parent_comp != child_comp && edge_set.insert(key) {
                        tree.edges.push(CompositionEdge {
                            parent: parent_comp.clone(),
                            child: child_comp.clone(),
                            relationship: ChildRelationship::DirectChild,
                            required: false,
                            bem_evidence: Some(format!(
                                "CSS descendant: .{} .{}",
                                css_parent, css_child
                            )),
                            strength: EdgeStrength::Allowed,
                            prop_name: None,
                        });
                    }
                }
            }
        }

        // ── Step 5.5: CSS layout children ───────────────────────────
        // Consume `layout_children` from the CSS profile — pairs of BEM
        // elements where one is a layout container (has flex-wrap/gap/grid)
        // and the other is a co-rule sibling. Maps both to components and
        // creates an edge.
        //
        // This data was previously computed but never consumed. It catches
        // intermediate nesting within families (e.g., EmptyStateFooter →
        // EmptyStateActions from a shared CSS rule with flex-wrap).
        for (css_container, css_child) in &css_prof.layout_children {
            let Some(container_comps) = css_to_component.get(css_container.as_str()) else {
                continue;
            };
            let Some(child_comps) = css_to_component.get(css_child.as_str()) else {
                continue;
            };
            for container_comp in container_comps {
                for child_comp in child_comps {
                    let key = (container_comp.clone(), child_comp.clone());
                    if container_comp != child_comp && edge_set.insert(key) {
                        tree.edges.push(CompositionEdge {
                            parent: container_comp.clone(),
                            child: child_comp.clone(),
                            relationship: ChildRelationship::DirectChild,
                            required: false,
                            bem_evidence: Some(format!(
                                "CSS layout container: .{} wraps .{} (shared CSS rule with flex-wrap/gap)",
                                css_container, css_child
                            )),
                            strength: EdgeStrength::Allowed, prop_name: None,
                        });
                    }
                }
            }
        }
    }

    // ── Step 6: React context ───────────────────────────────────────
    infer_context_nesting(&mut tree, profiles, family_exports);

    // ── Step 7: DOM nesting ─────────────────────────────────────────
    infer_dom_nesting(&mut tree, profiles, family_exports);

    // ── Step 8: cloneElement threading ──────────────────────────────
    infer_clone_element_nesting(&mut tree, profiles, family_exports);

    // ── Step 8.5: BEM element orphan fallback ──────────────────────
    // For family members with zero incoming edges after all structural
    // signals, connect them to the root if they are BEM elements of the
    // root's block. This catches children-passthrough families where the
    // parent renders `{children}` and sub-components are placed by the
    // consumer in JSX (e.g., EmptyState → EmptyStateBody).
    //
    // Guards:
    // 1. Zero incoming edges (orphan gate — prevents creating wrong edges
    //    for already-connected components in Category 3 families)
    // 2. Member appears in css_element_to_component_map (has BEM element
    //    CSS tokens of the root's block)
    // 3. BEM independence check: member must NOT have its own distinct
    //    BEM block (prevents false edges for collision families like
    //    Label/LabelGroup, Menu/MenuToggle)
    // 4. Root has has_children_prop (root must accept children)
    {
        let root_has_children = profiles.get(&root).is_some_and(|p| p.has_children_prop);
        let root_bem_block = profiles
            .get(&root)
            .and_then(|p| p.bem_block.as_deref())
            .map(|s| s.to_string());

        if root_has_children && !css_to_component.is_empty() {
            // Collect all members that currently have incoming edges (owned to avoid borrow)
            let parented: HashSet<String> = tree.edges.iter().map(|e| e.child.clone()).collect();

            // Collect the set of components that are BEM elements (values in the map)
            let bem_element_components: HashSet<&str> = css_to_component
                .values()
                .flat_map(|comps| comps.iter().map(|s| s.as_str()))
                .collect();

            let mut fallback_edges = Vec::new();

            for member in family_exports {
                if member == &root {
                    continue;
                }
                // Guard 1: only orphans (no incoming edges)
                if parented.contains(member) {
                    continue;
                }
                // Guard 2: must be a BEM element of the root's block
                if !bem_element_components.contains(member.as_str()) {
                    continue;
                }
                // Guard 3: BEM independence — skip if member has its own
                // distinct BEM block (e.g., LabelGroup has block "labelGroup"
                // which differs from Label's "label")
                if let Some(member_bem) = profiles.get(member).and_then(|p| p.bem_block.as_deref())
                {
                    if let Some(ref root_block) = root_bem_block {
                        if member_bem != root_block.as_str() {
                            debug!(
                                root = %root,
                                member = %member,
                                member_bem = %member_bem,
                                root_block = %root_block,
                                "BEM orphan fallback: skipping independent block"
                            );
                            continue;
                        }
                    }
                }

                let key = (root.clone(), member.clone());
                if edge_set.insert(key) {
                    debug!(
                        root = %root,
                        member = %member,
                        "BEM orphan fallback: connecting orphan to root"
                    );
                    fallback_edges.push(CompositionEdge {
                        parent: root.clone(),
                        child: member.clone(),
                        relationship: ChildRelationship::DirectChild,
                        required: false,
                        bem_evidence: Some(format!(
                            "BEM element fallback: {} is a BEM element of {}'s block with no other parent",
                            member, root
                        )),
                        strength: EdgeStrength::Allowed, prop_name: None,
                    });
                }
            }

            tree.edges.extend(fallback_edges);
        }
    }

    // ── Step 8.6: Secondary BEM block sub-root fallback ───────────
    // Some families have components that use a different BEM block than the
    // root (e.g., Modal root uses "backdrop" while ModalBody uses "modalBox",
    // Tabs root uses "tabs" while TabContentBody uses "tabContent").
    //
    // For each secondary block:
    // 1. Build a secondary css_to_component map for that block
    // 2. Find the sub-root: the component that maps to element "" (root)
    //    of the secondary block
    // 3. Run Step 8.5 logic using the sub-root: orphan members whose
    //    bem_block matches the secondary block get an Allowed edge to the
    //    sub-root
    //
    // After collapse_internal_nodes, if the sub-root is internal (non-exported),
    // edges propagate to the family root automatically.
    if let Some(css_profs) = css_profiles {
        // Collect all distinct BEM blocks used by family members that
        // differ from the root's BEM block. These need sub-root fallback
        // because the root's Step 8.5 only connects orphans whose
        // bem_block matches the root's block.
        //
        // NOTE: we compare against the root's block, NOT the primary
        // CSS profile key. The primary CSS key may differ from the root's
        // block (e.g., Modal: root block = "backdrop", primary CSS key =
        // "modalBox" via dominant vote). The sub-root fallback is about
        // which components can't be reached from the root — that's
        // determined by the root's block, not the CSS file selection.
        let root_block = profiles
            .get(&root)
            .and_then(|p| p.bem_block.as_deref())
            .unwrap_or("");
        let mut secondary_blocks: HashSet<&str> = HashSet::new();
        for name in family_exports {
            if let Some(prof) = profiles.get(name) {
                if let Some(ref block) = prof.bem_block {
                    if block != root_block {
                        secondary_blocks.insert(block.as_str());
                    }
                }
            }
        }

        for sec_block in &secondary_blocks {
            // Only process if we have a CSS profile for this block
            if !css_profs.contains_key(*sec_block) {
                continue;
            }

            // Build secondary CSS element → component map
            let sec_css_to_component =
                build_css_element_to_component_map(profiles, family_exports, sec_block);

            // Find the sub-root: component(s) that map to element "" (the
            // block root) in the secondary map
            let sub_roots: Vec<&str> = sec_css_to_component
                .get("")
                .into_iter()
                .flat_map(|comps| comps.iter().map(|s| s.as_str()))
                .collect();

            // Find the best sub-root: prefer one with has_children_prop
            let sub_root = sub_roots
                .iter()
                .find(|name| profiles.get(**name).is_some_and(|p| p.has_children_prop))
                .or(sub_roots.first())
                .copied();

            let Some(sub_root) = sub_root else {
                continue;
            };

            let sub_root_has_children = profiles.get(sub_root).is_some_and(|p| p.has_children_prop);
            if !sub_root_has_children {
                continue;
            }

            // Collect BEM element components from the secondary map
            let sec_bem_components: HashSet<&str> = sec_css_to_component
                .values()
                .flat_map(|comps| comps.iter().map(|s| s.as_str()))
                .collect();

            // Refresh parented set (may have changed from primary Step 8.5)
            let parented: HashSet<String> = tree.edges.iter().map(|e| e.child.clone()).collect();

            let mut sec_fallback_edges = Vec::new();

            for member in family_exports {
                if member == sub_root || member == &root {
                    continue;
                }
                // Guard 1: only orphans
                if parented.contains(member) {
                    continue;
                }
                // Guard 2: must be in the secondary CSS element map
                if !sec_bem_components.contains(member.as_str()) {
                    continue;
                }
                // Guard 3: member's BEM block must match this secondary block
                if let Some(member_bem) = profiles.get(member).and_then(|p| p.bem_block.as_deref())
                {
                    if member_bem != *sec_block {
                        continue;
                    }
                } else {
                    continue;
                }

                let key = (sub_root.to_string(), member.clone());
                if edge_set.insert(key) {
                    debug!(
                        sub_root = %sub_root,
                        member = %member,
                        secondary_block = %sec_block,
                        "Secondary block fallback: connecting orphan to sub-root"
                    );
                    sec_fallback_edges.push(CompositionEdge {
                        parent: sub_root.to_string(),
                        child: member.clone(),
                        relationship: ChildRelationship::DirectChild,
                        required: false,
                        bem_evidence: Some(format!(
                            "Secondary block fallback: {} is a BEM element of {}'s block ({})",
                            member, sub_root, sec_block
                        )),
                        strength: EdgeStrength::Allowed,
                        prop_name: None,
                    });
                }
            }

            tree.edges.extend(sec_fallback_edges);
        }
    }

    // ── Step 8.7: Prop-passed detection ───────────────────────────
    // Detect components passed via named ReactNode/ReactElement props
    // rather than as JSX children. For each family member, check if any
    // other family member has a ReactNode prop whose name correlates
    // with the component name (e.g., Alert.actionLinks ↔ AlertActionLink).
    //
    // This step both:
    // - Creates new PropPassed edges for orphan components
    // - Reclassifies existing DirectChild edges to PropPassed when a
    //   prop name match is found
    {
        let parented: HashSet<String> = tree.edges.iter().map(|e| e.child.clone()).collect();

        let mut new_prop_edges = Vec::new();
        let mut reclassify: Vec<(String, String, String)> = Vec::new(); // (parent, child, prop_name)

        for child_name in family_exports {
            if child_name == &root {
                continue;
            }
            let child_lower = child_name.to_lowercase();

            for parent_name in family_exports {
                if parent_name == child_name {
                    continue;
                }
                let Some(parent_prof) = profiles.get(parent_name) else {
                    continue;
                };

                let parent_lower = parent_name.to_lowercase();

                // Strip parent name prefix from child to get suffix
                let suffix = if child_lower.starts_with(&parent_lower) {
                    &child_lower[parent_lower.len()..]
                } else {
                    continue; // child name doesn't start with parent name
                };

                if suffix.is_empty() {
                    continue;
                }

                // Check parent's prop_types for ReactNode/ReactElement props
                for (prop_name, prop_type) in &parent_prof.prop_types {
                    if prop_name == "children" {
                        continue;
                    }
                    if !prop_type.contains("ReactNode")
                        && !prop_type.contains("ReactElement")
                        && !prop_type.contains("ComponentType")
                    {
                        continue;
                    }

                    let prop_lower = prop_name.to_lowercase();

                    // Match: suffix starts with prop name or prop name
                    // starts with suffix (case-insensitive)
                    if suffix.starts_with(&prop_lower) || prop_lower.starts_with(suffix) {
                        // Check if edge already exists
                        let edge_exists = tree
                            .edges
                            .iter()
                            .any(|e| e.parent == *parent_name && e.child == *child_name);

                        if edge_exists {
                            // Reclassify existing edge to PropPassed
                            reclassify.push((
                                parent_name.clone(),
                                child_name.clone(),
                                prop_name.clone(),
                            ));
                        } else if !parented.contains(child_name) {
                            // Create new PropPassed edge for orphan
                            let key = (parent_name.clone(), child_name.clone());
                            if edge_set.insert(key) {
                                debug!(
                                    parent = %parent_name,
                                    child = %child_name,
                                    prop = %prop_name,
                                    "Prop-passed detection: {} accepts {} via prop '{}'",
                                    parent_name, child_name, prop_name
                                );
                                new_prop_edges.push(CompositionEdge {
                                    parent: parent_name.clone(),
                                    child: child_name.clone(),
                                    relationship: ChildRelationship::PropPassed,
                                    required: false,
                                    bem_evidence: Some(format!(
                                        "Prop-passed: {} accepts {} via `{}` prop ({})",
                                        parent_name, child_name, prop_name, prop_type
                                    )),
                                    strength: EdgeStrength::Allowed,
                                    prop_name: Some(prop_name.clone()),
                                });
                            }
                        }
                        break; // Found a match for this parent, no need to check more props
                    }
                }
            }
        }

        tree.edges.extend(new_prop_edges);

        // Reclassify existing edges
        for (parent, child, prop) in reclassify {
            if let Some(edge) = tree
                .edges
                .iter_mut()
                .find(|e| e.parent == parent && e.child == child)
            {
                edge.relationship = ChildRelationship::PropPassed;
                edge.prop_name = Some(prop.clone());
                edge.bem_evidence = Some(format!(
                    "Prop-passed (reclassified): {} accepts {} via `{}` prop",
                    parent, child, prop
                ));
            }
        }
    }

    // ── Step 9: Suppress + dedup ───────────────────────────────────
    deduplicate_edges(&mut tree);
    suppress_root_edges_with_intermediate(&mut tree);

    // ── Step 10: Drop unconnected members ───────────────────────────
    // Members with no edges at all are dropped from the tree, UNLESS
    // they are barrel-file exports (exported_members). Exported orphans
    // are retained — they're part of the family's public API even if
    // no structural signal links them (e.g., convenience composites
    // like LoginForm, orchestrators like MenuContainer).
    //
    // Non-exported members with zero edges are internal noise (context
    // objects, type exports, helper components) and are dropped.
    //
    // Members with outgoing edges but no incoming edges are secondary
    // roots — top-level containers within the family. These are retained
    // so that collapse_internal_nodes can properly handle them.
    let parented: HashSet<&str> = tree.edges.iter().map(|e| e.child.as_str()).collect();
    let parenting: HashSet<&str> = tree.edges.iter().map(|e| e.parent.as_str()).collect();
    let exported_set: HashSet<&str> = exported_members
        .map(|e| e.iter().map(|s| s.as_str()).collect())
        .unwrap_or_default();
    tree.family_members.retain(|m| {
        m == &root
            || parented.contains(m.as_str())
            || parenting.contains(m.as_str())
            || exported_set.contains(m.as_str())
    });

    Some(tree)
}

/// Infer parent→child edges from cloneElement prop injection chains.
///
/// If component A uses `cloneElement(child, { prop1 })` and family member B
/// declares `prop1` in its interface, then B is a child of A.
///
/// Two filters prevent false edges from shared prop vocabularies:
///
/// 1. **Skip reverse-of-existing**: If B→A already exists from a prior step
///    (e.g., Step 1 internal rendering), don't create A→B from cloneElement.
///    The prior edge establishes the direction; adding the reverse creates a
///    false cycle.
///
/// 2. **Remove bidirectional pairs**: After creating all cloneElement edges,
///    if both A→B and B→A exist from cloneElement, both are removed. This
///    indicates a peer relationship (shared prop vocabulary) rather than a
///    parent-child hierarchy. E.g., chart sub-components that all inject
///    the same layout props (height, width, theme) into non-family primitives.
fn infer_clone_element_nesting(
    tree: &mut CompositionTree,
    profiles: &HashMap<String, ComponentSourceProfile>,
    family_exports: &[String],
) {
    // Collect existing edges from prior steps to detect reverse conflicts
    let prior_edges: HashSet<(&str, &str)> = tree
        .edges
        .iter()
        .map(|e| (e.parent.as_str(), e.child.as_str()))
        .collect();

    let mut new_edges = Vec::new();

    for parent_name in family_exports {
        let Some(parent_profile) = profiles.get(parent_name) else {
            continue;
        };
        if parent_profile.clone_element_injections.is_empty() {
            continue;
        }

        // Collect all injected prop names, filtering out universal props
        // that every component declares (children, className, style, etc.)
        // — these create false edges because they match everything.
        let universal_props: HashSet<&str> =
            ["children", "className", "style", "id", "key", "ref"].into();

        let injected_props: HashSet<&str> = parent_profile
            .clone_element_injections
            .iter()
            .flat_map(|inj| inj.injected_props.iter().map(|s| s.as_str()))
            .filter(|p| !universal_props.contains(p))
            .collect();

        if injected_props.is_empty() {
            continue;
        }

        // Find family members that declare any of these props
        for child_name in family_exports {
            if child_name == parent_name {
                continue;
            }
            if edge_exists(tree, parent_name, child_name) {
                continue;
            }
            // Filter 1: skip if reverse edge already exists from a prior step.
            // If B→A exists (B renders A), creating A→B would be a false cycle.
            if prior_edges.contains(&(child_name.as_str(), parent_name.as_str())) {
                continue;
            }

            let Some(child_profile) = profiles.get(child_name) else {
                continue;
            };

            // Check if child declares any of the injected props
            let matching_props: Vec<&str> = injected_props
                .iter()
                .filter(|prop| child_profile.all_props.contains(**prop))
                .copied()
                .collect();

            if !matching_props.is_empty() {
                new_edges.push(CompositionEdge {
                    parent: parent_name.clone(),
                    child: child_name.clone(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: Some(format!(
                        "cloneElement: {} injects [{}], {} declares them",
                        parent_name,
                        matching_props.join(", "),
                        child_name
                    )),
                    strength: EdgeStrength::Required,
                    prop_name: None,
                });
            }
        }
    }

    // Filter 2: remove bidirectional cloneElement pairs.
    // If A→B and B→A both come from cloneElement, they indicate peers
    // with shared prop vocabulary, not a parent-child relationship.
    let clone_pairs: HashSet<(String, String)> = new_edges
        .iter()
        .map(|e| (e.parent.clone(), e.child.clone()))
        .collect();

    new_edges.retain(|e| !clone_pairs.contains(&(e.child.clone(), e.parent.clone())));

    tree.edges.extend(new_edges);
}

/// Build a mapping from CSS BEM element names to component names.
///
/// Uses `css_tokens_used` on each component to determine which CSS elements
/// it renders. The mapping strips the BEM block prefix from tokens.
///
/// Example: ToolbarContent uses `styles.toolbarContentSection`. Block is
/// "toolbar". Strip prefix → "ContentSection" → kebab → "content-section".
/// Maps "content-section" → "ToolbarContent".
fn build_css_element_to_component_map(
    profiles: &HashMap<String, ComponentSourceProfile>,
    family_exports: &[String],
    block_name: &str,
) -> HashMap<String, HashSet<String>> {
    let mut map: HashMap<String, HashSet<String>> = HashMap::new();
    let root_name = family_exports.first().map(|s| s.as_str());

    for comp_name in family_exports {
        let Some(profile) = profiles.get(comp_name) else {
            continue;
        };

        for token in &profile.css_tokens_used {
            // Tokens are stored as "styles.drawerBody" or "styles.modifiers.expanded".
            // Strip the "styles." prefix to get the raw token (e.g., "drawerBody").
            // Skip modifier tokens ("styles.modifiers.*") — they don't map to BEM elements.
            let raw_token = if let Some(rest) = token.strip_prefix("styles.") {
                if rest.starts_with("modifiers.") {
                    continue;
                }
                rest
            } else {
                token.as_str()
            };

            if let Some(suffix) = raw_token.strip_prefix(block_name) {
                let element_name = if suffix.is_empty() {
                    // Root element — token matches block exactly
                    String::new()
                } else {
                    // Element — strip block prefix, lowercase first char,
                    // convert to kebab-case
                    let mut camel = suffix.to_string();
                    if let Some(c) = camel.get_mut(0..1) {
                        c.make_ascii_lowercase();
                    }
                    camel_to_kebab(&camel)
                };

                // For non-root elements, skip the root component claiming
                // child tokens when a dedicated component already exists.
                // The root often uses child CSS tokens (e.g., JumpLinks uses
                // `styles.jumpLinksList`) because it renders those elements
                // internally, but JumpLinksList is the dedicated component.
                //
                // For the root element (""), all components are allowed
                // (both root and sub-components may use the block token).
                if !element_name.is_empty()
                    && root_name == Some(comp_name.as_str())
                    && map.contains_key(&element_name)
                {
                    // Root trying to claim a non-root element that already
                    // has a dedicated component — skip.
                    continue;
                }

                map.entry(element_name)
                    .or_default()
                    .insert(comp_name.clone());
            }
        }
    }

    map
}

/// Convert camelCase to kebab-case.
fn camel_to_kebab(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            result.push('-');
        }
        result.push(ch.to_ascii_lowercase());
    }
    result
}

/// Check if an edge from parent to child already exists in the tree.
fn edge_exists(tree: &CompositionTree, parent: &str, child: &str) -> bool {
    tree.edges
        .iter()
        .any(|e| e.parent == parent && e.child == child)
}

/// Infer parent→child edges from HTML DOM nesting rules.
///
/// For each family member that wraps `{children}` in an HTML element,
/// check if any other family member renders a compatible child element
/// as its root. For example, if component A wraps children in `<ul>` and
/// component B renders `<li>` as its outermost element, then B should be
/// nested inside A.
///
/// This catches relationships that BEM can't express because BEM elements
/// are flat (MenuList and MenuItem are both elements of the "menu" block,
/// but MenuItem's `<li>` goes inside MenuList's `<ul>`).
fn infer_dom_nesting(
    tree: &mut CompositionTree,
    profiles: &HashMap<String, ComponentSourceProfile>,
    family_exports: &[String],
) {
    // Build a set of existing edges to avoid duplicates
    let existing: HashSet<(String, String)> = tree
        .edges
        .iter()
        .map(|e| (e.parent.clone(), e.child.clone()))
        .collect();

    let mut new_edges = Vec::new();

    for parent_name in family_exports {
        let Some(parent_profile) = profiles.get(parent_name) else {
            continue;
        };
        if !parent_profile.has_children_prop {
            continue;
        }

        // Get the element that wraps {children} — last lowercase entry
        // in children_slot_path
        let slot_element = parent_profile
            .children_slot_path
            .iter()
            .rev()
            .find(|e| e.starts_with(|c: char| c.is_lowercase()));

        let Some(slot_el) = slot_element else {
            continue;
        };

        // Get the expected child elements for this slot
        let expected_children = html_expected_children(slot_el);
        if expected_children.is_empty() {
            continue;
        }

        // Check if any family member renders one of these elements as root
        for child_name in family_exports {
            if child_name == parent_name {
                continue;
            }
            if existing.contains(&(parent_name.clone(), child_name.clone())) {
                continue;
            }

            let Some(child_profile) = profiles.get(child_name) else {
                continue;
            };

            // Get the child's root element — first lowercase entry in
            // children_slot_path, or the most prominent rendered element
            let child_root = child_profile
                .children_slot_path
                .first()
                .filter(|e| e.starts_with(|c: char| c.is_lowercase()))
                .cloned()
                .or_else(|| infer_root_element(child_profile));

            if let Some(ref root_el) = child_root {
                if expected_children.contains(&root_el.as_str()) {
                    new_edges.push(CompositionEdge {
                        parent: parent_name.clone(),
                        child: child_name.clone(),
                        relationship: ChildRelationship::DirectChild,
                        required: false,
                        bem_evidence: Some(format!(
                            "DOM nesting: {} wraps children in <{}>, {} renders <{}> as root",
                            parent_name, slot_el, child_name, root_el
                        )),
                        strength: EdgeStrength::Required,
                        prop_name: None,
                    });
                }
            }
        }
    }

    tree.edges.extend(new_edges);

    // ── Flow container fallback ─────────────────────────────────────
    //
    // Last-resort: for components still only connected to the root,
    // check if a sibling renders a flow content container (<section>,
    // <div>, etc.) wrapping {children}. If so, a <ul>/<ol>-rendering
    // component likely goes inside it (e.g., MenuGroup <section> wraps
    // MenuList <ul>).
    //
    // Only applies when:
    //   - The child is NOT the family root (prevents DataList root edge)
    //   - The child is still a direct child of root (no other parent)
    //   - The child renders <ul> or <ol> as its root element
    //   - The parent renders a flow container wrapping {children}
    //   - No existing edge from parent to child
    let flow_containers = [
        "section", "div", "article", "aside", "main", "nav", "header", "footer",
    ];
    let list_tags = ["ul", "ol"];

    let root_children: Vec<String> = tree
        .edges
        .iter()
        .filter(|e| e.parent == tree.root)
        .map(|e| e.child.clone())
        .collect();

    let mut flow_edges = Vec::new();
    for child_name in &root_children {
        if child_name == &tree.root {
            continue;
        }
        // Only consider children that have no other parent (still root-level)
        let has_other_parent = tree
            .edges
            .iter()
            .any(|e| e.child == *child_name && e.parent != tree.root);
        if has_other_parent {
            continue;
        }

        let child_profile = match profiles.get(child_name) {
            Some(p) => p,
            None => continue,
        };

        // Check if this child renders <ul> or <ol>
        let child_root = child_profile
            .children_slot_path
            .first()
            .filter(|e| e.starts_with(|c: char| c.is_lowercase()))
            .cloned()
            .or_else(|| infer_root_element(child_profile));

        let is_list = child_root
            .as_ref()
            .is_some_and(|r| list_tags.contains(&r.as_str()));
        if !is_list {
            continue;
        }

        // Find a flow container sibling that could wrap this child
        for parent_name in family_exports {
            if parent_name == child_name || parent_name == &tree.root {
                continue;
            }
            let existing_edge = tree
                .edges
                .iter()
                .any(|e| e.parent == *parent_name && e.child == *child_name);
            if existing_edge {
                continue;
            }

            let parent_profile = match profiles.get(parent_name) {
                Some(p) => p,
                None => continue,
            };
            if !parent_profile.has_children_prop {
                continue;
            }

            let parent_slot = parent_profile
                .children_slot_path
                .iter()
                .rev()
                .find(|e| e.starts_with(|c: char| c.is_lowercase()));

            let is_flow = parent_slot.is_some_and(|s| flow_containers.contains(&s.as_str()));
            if !is_flow {
                continue;
            }

            // Verify this parent is itself a root-level child (not deeply nested)
            let parent_is_root_child = tree
                .edges
                .iter()
                .any(|e| e.parent == tree.root && e.child == *parent_name);
            if !parent_is_root_child {
                continue;
            }

            flow_edges.push((parent_name.clone(), child_name.clone()));
            break; // Only assign to first matching flow container
        }
    }

    for (parent, child) in flow_edges {
        tree.edges
            .retain(|e| !(e.parent == tree.root && e.child == child));
        tree.edges.push(CompositionEdge {
            parent: parent.clone(),
            child: child.clone(),
            relationship: ChildRelationship::DirectChild,
            required: false,
            bem_evidence: Some(format!(
                "Flow container nesting: {} renders <ul>/<ol>, {} wraps {{children}} in flow container",
                child, parent
            )),
            strength: EdgeStrength::Allowed, prop_name: None,
        });
    }
}

/// Infer parent→child edges from React Context provider→consumer relationships.
///
/// If a family member renders `<XContext.Provider>` (visible in its
/// `rendered_components`) and another family member calls `useContext(XContext)`
/// (visible in its `consumed_contexts`), the consumer must be nested somewhere
/// under the provider.
fn infer_context_nesting(
    tree: &mut CompositionTree,
    profiles: &HashMap<String, ComponentSourceProfile>,
    family_exports: &[String],
) {
    let existing: HashSet<(String, String)> = tree
        .edges
        .iter()
        .map(|e| (e.parent.clone(), e.child.clone()))
        .collect();

    let mut new_edges = Vec::new();

    // Build map: context_name → provider component
    // Detect from rendered_components entries like "XContext.Provider"
    let mut context_providers: HashMap<String, Vec<String>> = HashMap::new();
    for name in family_exports {
        let Some(profile) = profiles.get(name) else {
            continue;
        };
        for rc in &profile.rendered_components {
            if let Some(ctx_name) = rc.strip_suffix(".Provider") {
                context_providers
                    .entry(ctx_name.to_string())
                    .or_default()
                    .push(name.clone());
            }
        }
    }

    if context_providers.is_empty() {
        return;
    }

    // For each consumer, find which provider(s) it depends on
    for child_name in family_exports {
        let Some(child_profile) = profiles.get(child_name) else {
            continue;
        };

        for consumed_ctx in &child_profile.consumed_contexts {
            if let Some(providers) = context_providers.get(consumed_ctx) {
                for provider_name in providers {
                    if provider_name == child_name {
                        continue;
                    }

                    // Skip re-providers: if the provider also CONSUMES
                    // the same context, it's re-providing for a nested
                    // scope (e.g., MenuItem re-provides MenuContext for
                    // flyout submenus). Only root providers create edges.
                    let Some(provider_profile) = profiles.get(provider_name) else {
                        continue;
                    };
                    if provider_profile.consumed_contexts.contains(consumed_ctx) {
                        debug!(
                            provider = %provider_name,
                            consumer = %child_name,
                            context = %consumed_ctx,
                            "skipping re-provider context nesting"
                        );
                        continue;
                    }
                    if existing.contains(&(provider_name.clone(), child_name.clone())) {
                        continue;
                    }
                    // Avoid duplicate with edges we're about to add
                    if new_edges.iter().any(|e: &CompositionEdge| {
                        e.parent == *provider_name && e.child == *child_name
                    }) {
                        continue;
                    }

                    debug!(
                        provider = %provider_name,
                        consumer = %child_name,
                        context = %consumed_ctx,
                        "context nesting inferred"
                    );

                    // Context availability does NOT mean mandatory children.
                    // A parent providing context means "these children CAN use
                    // this context if present", not "these children MUST be
                    // present". For example, ToolbarContent provides
                    // ToolbarContentContext for ToolbarFilter, but
                    // <ToolbarContent> with just <ToolbarGroup> children is
                    // perfectly valid.
                    new_edges.push(CompositionEdge {
                        parent: provider_name.clone(),
                        child: child_name.clone(),
                        relationship: ChildRelationship::DirectChild,
                        required: false,
                        bem_evidence: Some(format!(
                            "Context nesting: {} provides {}, {} consumes it via useContext",
                            provider_name, consumed_ctx, child_name
                        )),
                        strength: EdgeStrength::Allowed,
                        prop_name: None,
                    });
                }
            }
        }
    }

    tree.edges.extend(new_edges);
}

/// Get expected child element tags for a given HTML parent element.
fn html_expected_children(parent_tag: &str) -> Vec<&'static str> {
    match parent_tag {
        "ul" | "ol" => vec!["li"],
        "table" => vec!["thead", "tbody", "tfoot", "tr", "caption", "colgroup"],
        "thead" | "tbody" | "tfoot" => vec!["tr"],
        "tr" => vec!["td", "th"],
        "dl" => vec!["dt", "dd"],
        "select" => vec!["option", "optgroup"],
        "optgroup" => vec!["option"],
        _ => vec![],
    }
}

/// Infer the root HTML element from a component's rendered_elements
/// or prop defaults.
///
/// Heuristic: if the component only renders one type of block-level
/// element, that's likely the root. For components like MenuItem that
/// render `<li>` as the wrapper, this picks up `li`.
///
/// Fallback: if the component has a `component` or `as` prop with an
/// HTML element string default (e.g., `component = 'th'`), use that.
/// This handles polymorphic components that render via a dynamic variable
/// (e.g., `<MergedComponent>` defaulting to `'th'`).
fn infer_root_element(profile: &ComponentSourceProfile) -> Option<String> {
    // Check rendered_elements for common root candidates
    let root_candidates = [
        "li", "tr", "td", "th", "dt", "dd", "option", "section", "article", "div",
    ];
    for candidate in &root_candidates {
        if profile.rendered_elements.contains_key(*candidate) {
            return Some(candidate.to_string());
        }
    }

    // Fallback: check prop defaults for `component` or `as` with an HTML
    // element value. This covers polymorphic components like PatternFly's
    // Td/Th that render via `<MergedComponent>` with `component = 'td'`.
    for prop_name in &["component", "as"] {
        if let Some(default_val) = profile.prop_defaults.get(*prop_name) {
            // Strip quotes: 'td' or "td" → td
            let tag = default_val.trim_matches(|c| c == '\'' || c == '"');
            if !tag.is_empty()
                && tag.starts_with(|c: char| c.is_lowercase())
                && tag.chars().all(|c| c.is_ascii_alphanumeric())
            {
                return Some(tag.to_string());
            }
        }
    }

    None
}

/// Suppress root→child BEM edges when a more specific intermediate→child
/// edge exists from DOM nesting, context, or delegation projection.
///
/// BEM analysis creates edges from the block owner to every component
/// that uses its CSS tokens. But DOM/context/projection analysis discovers
/// the actual JSX nesting, which may have an intermediate wrapper between
/// the root and the child.
///
/// When both exist, the root edge is suppressed because the intermediate
/// is the correct JSX parent. This prevents conformance rules from
/// incorrectly requiring components to be direct children of the root.
fn suppress_root_edges_with_intermediate(tree: &mut CompositionTree) {
    let root = &tree.root;

    // Collect children that have a direct_child edge from a non-root
    // intermediate family member.
    let children_with_intermediate: HashSet<String> = tree
        .edges
        .iter()
        .filter(|e| e.parent != *root && matches!(e.relationship, ChildRelationship::DirectChild))
        .map(|e| e.child.clone())
        .collect();

    if children_with_intermediate.is_empty() {
        return;
    }

    let before = tree.edges.len();
    tree.edges.retain(|edge| {
        // Keep all non-root edges
        if edge.parent != *root {
            return true;
        }
        // Keep root edges for children that have NO intermediate
        if !children_with_intermediate.contains(&edge.child) {
            return true;
        }
        // Suppress root→child direct_child edges when intermediate exists
        if matches!(edge.relationship, ChildRelationship::DirectChild) {
            tracing::debug!(
                root = %root,
                child = %edge.child,
                "suppressing root BEM edge — intermediate parent exists"
            );
            return false;
        }
        true
    });

    let suppressed = before - tree.edges.len();
    if suppressed > 0 {
        tracing::debug!(
            root = %root,
            suppressed,
            "suppressed root→child BEM edges with intermediate parents"
        );
    }
}

/// Remove redundant edges. If both parent→child (Internal) and
/// parent→child (DirectChild) exist, keep only the more specific one.
fn deduplicate_edges(tree: &mut CompositionTree) {
    let mut seen = HashSet::new();
    tree.edges.retain(|edge| {
        let key = (edge.parent.clone(), edge.child.clone());
        seen.insert(key)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn make_profile(name: &str) -> ComponentSourceProfile {
        ComponentSourceProfile {
            name: name.to_string(),
            ..Default::default()
        }
    }

    /// Helper to create a BTreeSet<String> from a slice of &str.
    fn tokens(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_context_nesting_provider_consumer() {
        // Menu renders <MenuContext.Provider>, MenuList calls
        // useContext(MenuContext). MenuList must be nested under Menu.
        let mut menu = make_profile("Menu");
        menu.has_children_prop = true;
        menu.rendered_components = vec!["MenuContext.Provider".into()];

        let mut menu_list = make_profile("MenuList");
        menu_list.has_children_prop = true;
        menu_list.consumed_contexts = vec!["MenuContext".into()];

        let mut profiles = HashMap::new();
        profiles.insert("Menu".into(), menu);
        profiles.insert("MenuList".into(), menu_list);

        let family = vec!["Menu".into(), "MenuList".into()];

        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[], None).unwrap();

        let menu_to_list = tree
            .edges
            .iter()
            .find(|e| e.parent == "Menu" && e.child == "MenuList");
        assert!(
            menu_to_list.is_some(),
            "Expected Menu → MenuList from context nesting, got edges: {:?}",
            tree.edges
        );
        assert!(menu_to_list
            .unwrap()
            .bem_evidence
            .as_ref()
            .unwrap()
            .contains("Context nesting"));
    }

    #[test]
    fn test_dom_nesting_ul_li() {
        // MenuList wraps children in <ul>, MenuItem renders <li> as root.
        // DOM nesting inference should create MenuList → MenuItem edge.
        let mut menu_list = make_profile("MenuList");
        menu_list.has_children_prop = true;
        menu_list.children_slot_path = vec!["ul".into()];
        menu_list.rendered_elements.insert("ul".into(), 1);

        let mut menu_item = make_profile("MenuItem");
        menu_item.has_children_prop = true;
        menu_item.children_slot_path = vec!["li".into(), "button".into(), "span".into()];
        menu_item.rendered_elements.insert("li".into(), 1);
        menu_item.rendered_elements.insert("button".into(), 1);
        menu_item.rendered_elements.insert("span".into(), 3);

        let mut profiles = HashMap::new();
        profiles.insert("MenuList".into(), menu_list);
        profiles.insert("MenuItem".into(), menu_item);

        let family = vec!["MenuList".into(), "MenuItem".into()];

        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[], None).unwrap();

        let list_to_item = tree
            .edges
            .iter()
            .find(|e| e.parent == "MenuList" && e.child == "MenuItem");
        assert!(
            list_to_item.is_some(),
            "Expected MenuList → MenuItem from DOM nesting (ul→li), got edges: {:?}",
            tree.edges
        );
        assert!(list_to_item
            .unwrap()
            .bem_evidence
            .as_ref()
            .unwrap()
            .contains("DOM nesting"));
    }

    #[test]
    fn test_suppress_root_edges_with_intermediate() {
        // Simulate Accordion: root has BEM edges to AccordionContent and
        // AccordionToggle, but context nesting proves they go inside
        // AccordionItem.
        let mut tree = CompositionTree {
            root: "Accordion".into(),
            family_members: vec![
                "Accordion".into(),
                "AccordionItem".into(),
                "AccordionContent".into(),
                "AccordionToggle".into(),
            ],
            edges: vec![
                // BEM-derived root edges (should be suppressed)
                CompositionEdge {
                    parent: "Accordion".into(),
                    child: "AccordionContent".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: Some("BEM element of accordion block".into()),
                    strength: EdgeStrength::Allowed,
                    prop_name: None,
                },
                CompositionEdge {
                    parent: "Accordion".into(),
                    child: "AccordionToggle".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: Some("BEM element of accordion block".into()),
                    strength: EdgeStrength::Allowed,
                    prop_name: None,
                },
                // Correct root edge (no intermediate for AccordionItem)
                CompositionEdge {
                    parent: "Accordion".into(),
                    child: "AccordionItem".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: Some("BEM element of accordion block".into()),
                    strength: EdgeStrength::Allowed,
                    prop_name: None,
                },
                // Context-derived intermediate edges (should be kept)
                CompositionEdge {
                    parent: "AccordionItem".into(),
                    child: "AccordionContent".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: Some("Context nesting".into()),
                    strength: EdgeStrength::Allowed,
                    prop_name: None,
                },
                CompositionEdge {
                    parent: "AccordionItem".into(),
                    child: "AccordionToggle".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: Some("Context nesting".into()),
                    strength: EdgeStrength::Allowed,
                    prop_name: None,
                },
            ],
        };

        suppress_root_edges_with_intermediate(&mut tree);

        // Root → AccordionItem should be kept (no intermediate)
        assert!(
            tree.edges
                .iter()
                .any(|e| e.parent == "Accordion" && e.child == "AccordionItem"),
            "Accordion → AccordionItem should be kept"
        );

        // Root → AccordionContent should be suppressed
        assert!(
            !tree
                .edges
                .iter()
                .any(|e| e.parent == "Accordion" && e.child == "AccordionContent"),
            "Accordion → AccordionContent should be suppressed (intermediate exists)"
        );

        // Root → AccordionToggle should be suppressed
        assert!(
            !tree
                .edges
                .iter()
                .any(|e| e.parent == "Accordion" && e.child == "AccordionToggle"),
            "Accordion → AccordionToggle should be suppressed (intermediate exists)"
        );

        // Intermediate edges should be kept
        assert!(
            tree.edges
                .iter()
                .any(|e| e.parent == "AccordionItem" && e.child == "AccordionContent"),
            "AccordionItem → AccordionContent should be kept"
        );
        assert!(
            tree.edges
                .iter()
                .any(|e| e.parent == "AccordionItem" && e.child == "AccordionToggle"),
            "AccordionItem → AccordionToggle should be kept"
        );

        assert_eq!(tree.edges.len(), 3, "Should have 3 edges remaining");
    }

    #[test]
    fn test_suppress_no_false_positives_masthead() {
        // Masthead: all edges are root→child only, no intermediates.
        // Nothing should be suppressed.
        let mut tree = CompositionTree {
            root: "Masthead".into(),
            family_members: vec![
                "Masthead".into(),
                "MastheadBrand".into(),
                "MastheadContent".into(),
                "MastheadMain".into(),
            ],
            edges: vec![
                CompositionEdge {
                    parent: "Masthead".into(),
                    child: "MastheadBrand".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: None,
                    strength: EdgeStrength::Allowed,
                    prop_name: None,
                },
                CompositionEdge {
                    parent: "Masthead".into(),
                    child: "MastheadContent".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: None,
                    strength: EdgeStrength::Allowed,
                    prop_name: None,
                },
                CompositionEdge {
                    parent: "Masthead".into(),
                    child: "MastheadMain".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: None,
                    strength: EdgeStrength::Allowed,
                    prop_name: None,
                },
            ],
        };

        suppress_root_edges_with_intermediate(&mut tree);

        assert_eq!(
            tree.edges.len(),
            3,
            "No edges should be suppressed for Masthead"
        );
    }

    /// Verify that BEM edges are created with required=false.
    /// Only internally rendered children should be required.
    #[test]
    fn test_bem_edges_are_not_required() {
        let mut parent = make_profile("Dropdown");
        parent.has_children_prop = true;
        parent.bem_block = Some("dropdown".into());
        parent.css_tokens_used.insert("styles.dropdown".into());

        let mut list = make_profile("DropdownList");
        list.has_children_prop = true;
        list.bem_block = Some("dropdown".into());
        list.css_tokens_used.insert("styles.dropdownList".into());

        let mut group = make_profile("DropdownGroup");
        group.has_children_prop = true;
        group.bem_block = Some("dropdown".into());
        group.css_tokens_used.insert("styles.dropdownGroup".into());

        let mut profiles = HashMap::new();
        profiles.insert("Dropdown".into(), parent);
        profiles.insert("DropdownList".into(), list);
        profiles.insert("DropdownGroup".into(), group);

        let family = vec![
            "Dropdown".into(),
            "DropdownList".into(),
            "DropdownGroup".into(),
        ];

        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[], None).unwrap();

        // All BEM-derived edges should be required=false
        for edge in &tree.edges {
            if edge.relationship == ChildRelationship::DirectChild {
                assert!(
                    !edge.required,
                    "BEM edge {} → {} should not be required (BEM proves membership, not requirement)",
                    edge.parent, edge.child
                );
            }
        }
    }

    // ── Hyphen boundary tests for infer_ownership_by_name_prefix ─────

    #[test]
    fn test_label_labelgroup_no_ownership_edge() {
        // LabelGroup has BEM block "labelGroup" (camelCase of "label-group")
        // — a SEPARATE block from Label's "label" block. Label should NOT
        // own LabelGroup via name-prefix inference.
        let mut label = make_profile("Label");
        label.has_children_prop = true;
        label.bem_block = Some("label".into());
        label.css_tokens_used = [
            "styles.label".to_string(),
            "styles.labelText".to_string(),
            "styles.labelIcon".to_string(),
        ]
        .into_iter()
        .collect();

        let mut label_group = make_profile("LabelGroup");
        label_group.has_children_prop = true;
        label_group.bem_block = Some("labelGroup".into()); // camelCase of "label-group"
        label_group.css_tokens_used = [
            "styles.labelGroup".to_string(),
            "styles.labelGroupList".to_string(),
            "styles.labelGroupMain".to_string(),
            "styles.labelGroupClose".to_string(),
        ]
        .into_iter()
        .collect();

        let mut profiles = HashMap::new();
        profiles.insert("Label".into(), label);
        profiles.insert("LabelGroup".into(), label_group);

        let family = vec!["Label".into(), "LabelGroup".into()];

        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[], None).unwrap();

        // There should be NO edge from Label -> LabelGroup
        let bad_edge = tree
            .edges
            .iter()
            .find(|e| e.parent == "Label" && e.child == "LabelGroup");
        assert!(
            bad_edge.is_none(),
            "Label should NOT own LabelGroup — 'label-group' is a separate BEM block \
             (hyphen boundary after 'label'). Found edge: {:?}",
            bad_edge
        );
    }

    #[test]
    fn test_alert_alertgroup_no_ownership_edge() {
        // AlertGroup has BEM block "alert-group" — separate from Alert's
        // "alert" block.
        let mut alert = make_profile("Alert");
        alert.has_children_prop = true;
        alert.bem_block = Some("alert".into());
        alert.css_tokens_used = ["styles.alert".to_string(), "styles.alertTitle".to_string()]
            .into_iter()
            .collect();

        let mut alert_group = make_profile("AlertGroup");
        alert_group.has_children_prop = true;
        alert_group.bem_block = Some("alertGroup".into()); // camelCase of "alert-group"
        alert_group.css_tokens_used = [
            "styles.alertGroup".to_string(),
            "styles.alertGroupItem".to_string(),
        ]
        .into_iter()
        .collect();

        let mut profiles = HashMap::new();
        profiles.insert("Alert".into(), alert);
        profiles.insert("AlertGroup".into(), alert_group);

        let family = vec!["Alert".into(), "AlertGroup".into()];

        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[], None).unwrap();

        let bad_edge = tree
            .edges
            .iter()
            .find(|e| e.parent == "Alert" && e.child == "AlertGroup");
        assert!(
            bad_edge.is_none(),
            "Alert should NOT own AlertGroup — 'alert-group' is a separate BEM block. \
             Found edge: {:?}",
            bad_edge
        );
    }

    #[test]
    fn test_modal_modalbox_no_false_ownership() {
        // Modal's own BEM block is "backdrop", children use "modalBox".
        // Even though "modal" is a prefix of "modalBox", the name-prefix
        // inference should NOT create ownership edges because "modalBox"
        // is a different block from "modal" (it's a separate CSS file
        // pf-v6-c-modal-box, not an element of a "modal" block).
        //
        // In practice, Modal has zero composition tree edges because its
        // children (ModalBody, ModalHeader, ModalFooter) are consumer-
        // provided via {children}, not internally rendered.
        let mut modal = make_profile("Modal");
        modal.has_children_prop = true;
        modal.bem_block = Some("backdrop".into());
        modal.css_tokens_used = ["styles.backdrop".to_string()].into_iter().collect();

        let mut modal_box = make_profile("ModalBox");
        modal_box.has_children_prop = true;
        modal_box.bem_block = Some("modalBox".into()); // camelCase of "modal-box"
        modal_box.css_tokens_used = [
            "styles.modalBox".to_string(),
            "styles.modalBoxBody".to_string(),
            "styles.modalBoxHeader".to_string(),
        ]
        .into_iter()
        .collect();

        let mut modal_body = make_profile("ModalBoxBody");
        modal_body.has_children_prop = true;
        modal_body.bem_block = None; // Shares ModalBox's block
        modal_body.css_tokens_used = ["styles.modalBoxBody".to_string()].into_iter().collect();

        let mut profiles = HashMap::new();
        profiles.insert("Modal".into(), modal);
        profiles.insert("ModalBox".into(), modal_box);
        profiles.insert("ModalBoxBody".into(), modal_body);

        let family = vec!["Modal".into(), "ModalBox".into(), "ModalBoxBody".into()];

        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[], None).unwrap();

        // Name-prefix inference should NOT create Modal → ModalBox because
        // "modalBox" != "modal" (different block). ModalBox → ModalBoxBody
        // edges may be created via BEM element matching (modalBoxBody is an
        // element of the modalBox block).
        let modal_owns_box = tree
            .edges
            .iter()
            .any(|e| e.parent == "Modal" && e.child == "ModalBox");
        assert!(
            !modal_owns_box,
            "Modal should NOT own ModalBox via name-prefix — 'modalBox' is a different \
             BEM block from 'modal'. Modal's children are consumer-provided. \
             Edges: {:?}",
            tree.edges
        );
    }

    /// Components with outgoing edges but no incoming edges should be
    /// retained as secondary roots, not dropped by Step 10.
    ///
    /// Models the JumpLinks pattern: JumpLinksList wraps <ul> and
    /// JumpLinksItem renders <li>. JumpLinksList has no parent in the
    /// tree, making it a secondary root alongside JumpLinks.
    #[test]
    fn test_secondary_root_retained_for_dom_nesting() {
        // JumpLinks is the primary root (first export).
        // JumpLinksList wraps <ul>, JumpLinksItem renders <li>.
        // No signal creates an edge INTO JumpLinksList, but DOM nesting
        // creates JumpLinksList → JumpLinksItem. JumpLinksList should
        // survive as a secondary root.
        let jump_links = make_profile("JumpLinks");

        let mut jump_links_list = make_profile("JumpLinksList");
        jump_links_list.has_children_prop = true;
        jump_links_list.children_slot_path = vec!["ul".into()];
        jump_links_list.rendered_elements.insert("ul".into(), 1);

        let mut jump_links_item = make_profile("JumpLinksItem");
        jump_links_item.has_children_prop = true;
        jump_links_item.children_slot_path = vec!["li".into(), "a".into()];
        jump_links_item.rendered_elements.insert("li".into(), 1);

        let mut profiles = HashMap::new();
        profiles.insert("JumpLinks".into(), jump_links);
        profiles.insert("JumpLinksList".into(), jump_links_list);
        profiles.insert("JumpLinksItem".into(), jump_links_item);

        let family = vec![
            "JumpLinks".into(),
            "JumpLinksList".into(),
            "JumpLinksItem".into(),
        ];

        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[], None).unwrap();

        // JumpLinksList should be retained as a member (secondary root)
        assert!(
            tree.family_members.contains(&"JumpLinksList".into()),
            "JumpLinksList should be retained as a secondary root. Members: {:?}",
            tree.family_members
        );

        // The DOM nesting edge JumpLinksList → JumpLinksItem should exist
        let list_to_item = tree
            .edges
            .iter()
            .find(|e| e.parent == "JumpLinksList" && e.child == "JumpLinksItem");
        assert!(
            list_to_item.is_some(),
            "Expected JumpLinksList → JumpLinksItem from DOM nesting. Edges: {:?}",
            tree.edges
        );

        // JumpLinksItem should also be retained (it has an incoming edge)
        assert!(
            tree.family_members.contains(&"JumpLinksItem".into()),
            "JumpLinksItem should be retained (has incoming edge). Members: {:?}",
            tree.family_members
        );
    }

    /// Components with no edges at all should still be dropped by Step 10.
    #[test]
    fn test_truly_unconnected_member_still_dropped() {
        let mut root = make_profile("Root");
        root.has_children_prop = true;

        let mut child = make_profile("Child");
        child.has_children_prop = true;

        // Orphan has no edges at all — no structural evidence
        let orphan = make_profile("Orphan");

        // Create a context edge Root → Child to give them structure
        root.rendered_components = vec!["RootContext.Provider".into()];
        child.consumed_contexts = vec!["RootContext".into()];

        let mut profiles = HashMap::new();
        profiles.insert("Root".into(), root);
        profiles.insert("Child".into(), child);
        profiles.insert("Orphan".into(), orphan);

        let family = vec!["Root".into(), "Child".into(), "Orphan".into()];

        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[], None).unwrap();

        // Orphan has no edges, should be dropped
        assert!(
            !tree.family_members.contains(&"Orphan".into()),
            "Orphan with no edges should be dropped. Members: {:?}",
            tree.family_members
        );

        // Root and Child should still be present
        assert!(tree.family_members.contains(&"Root".into()));
        assert!(tree.family_members.contains(&"Child".into()));
    }

    #[test]
    fn test_menu_menutoggle_no_ownership_edge() {
        // MenuToggle has BEM block "menu-toggle" — separate from Menu.
        let mut menu = make_profile("Menu");
        menu.has_children_prop = true;
        menu.bem_block = Some("menu".into());
        menu.css_tokens_used = ["styles.menu".to_string()].into_iter().collect();

        let mut menu_toggle = make_profile("MenuToggle");
        menu_toggle.has_children_prop = true;
        menu_toggle.bem_block = Some("menuToggle".into()); // camelCase of "menu-toggle"
        menu_toggle.css_tokens_used = [
            "styles.menuToggle".to_string(),
            "styles.menuToggleIcon".to_string(),
        ]
        .into_iter()
        .collect();

        let mut profiles = HashMap::new();
        profiles.insert("Menu".into(), menu);
        profiles.insert("MenuToggle".into(), menu_toggle);

        let family = vec!["Menu".into(), "MenuToggle".into()];

        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[], None).unwrap();

        let bad_edge = tree
            .edges
            .iter()
            .find(|e| e.parent == "Menu" && e.child == "MenuToggle");
        assert!(
            bad_edge.is_none(),
            "Menu should NOT own MenuToggle — 'menu-toggle' is a separate BEM block. \
             Found edge: {:?}",
            bad_edge
        );
    }

    /// Bidirectional cloneElement edges (A→B and B→A) should be removed.
    /// These indicate peers with shared prop vocabulary, not hierarchy.
    #[test]
    fn test_clone_element_bidirectional_pairs_removed() {
        use semver_analyzer_core::types::sd::CloneElementInjection;

        // Three components: Root, SubA, SubB.
        // SubA and SubB both inject the same props (height, width)
        // and both declare the same props. This creates bidirectional
        // cloneElement edges SubA→SubB and SubB→SubA.
        let mut root = make_profile("Root");
        root.has_children_prop = true;
        // Root renders SubA and SubB via Step 1
        root.rendered_components = vec!["SubA".into(), "SubB".into()];

        let mut sub_a = make_profile("SubA");
        sub_a.clone_element_injections = vec![CloneElementInjection {
            injected_props: vec!["height".into(), "width".into(), "theme".into()],
        }];
        sub_a.all_props = vec!["height".into(), "width".into(), "theme".into()]
            .into_iter()
            .collect();

        let mut sub_b = make_profile("SubB");
        sub_b.clone_element_injections = vec![CloneElementInjection {
            injected_props: vec!["height".into(), "width".into(), "theme".into()],
        }];
        sub_b.all_props = vec!["height".into(), "width".into(), "theme".into()]
            .into_iter()
            .collect();

        let mut profiles = HashMap::new();
        profiles.insert("Root".into(), root);
        profiles.insert("SubA".into(), sub_a);
        profiles.insert("SubB".into(), sub_b);

        let family = vec!["Root".into(), "SubA".into(), "SubB".into()];

        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[], None).unwrap();

        // Step 1 edges Root→SubA and Root→SubB should exist
        assert!(
            tree.edges
                .iter()
                .any(|e| e.parent == "Root" && e.child == "SubA"),
            "Root → SubA should exist from internal rendering"
        );
        assert!(
            tree.edges
                .iter()
                .any(|e| e.parent == "Root" && e.child == "SubB"),
            "Root → SubB should exist from internal rendering"
        );

        // Bidirectional cloneElement edges SubA↔SubB should NOT exist
        let sub_a_to_b = tree
            .edges
            .iter()
            .find(|e| e.parent == "SubA" && e.child == "SubB");
        assert!(
            sub_a_to_b.is_none(),
            "SubA → SubB should be removed as bidirectional cloneElement pair. Edges: {:?}",
            tree.edges
        );

        let sub_b_to_a = tree
            .edges
            .iter()
            .find(|e| e.parent == "SubB" && e.child == "SubA");
        assert!(
            sub_b_to_a.is_none(),
            "SubB → SubA should be removed as bidirectional cloneElement pair. Edges: {:?}",
            tree.edges
        );
    }

    /// cloneElement edge A→B should be skipped when B→A already exists
    /// from a prior step (e.g., Step 1 internal rendering).
    #[test]
    fn test_clone_element_skipped_when_reverse_exists() {
        use semver_analyzer_core::types::sd::CloneElementInjection;

        // Root renders Sub (Step 1). Sub uses cloneElement to inject
        // props that Root declares. Without the reverse-edge check,
        // Sub→Root would be created (wrong). With it, Sub→Root is skipped.
        let mut root = make_profile("Root");
        root.has_children_prop = true;
        root.rendered_components = vec!["Sub".into()];
        root.all_props = vec!["height".into(), "width".into()].into_iter().collect();

        let mut sub = make_profile("Sub");
        sub.clone_element_injections = vec![CloneElementInjection {
            injected_props: vec!["height".into(), "width".into()],
        }];

        let mut profiles = HashMap::new();
        profiles.insert("Root".into(), root);
        profiles.insert("Sub".into(), sub);

        let family = vec!["Root".into(), "Sub".into()];

        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[], None).unwrap();

        // Root→Sub should exist from Step 1
        assert!(
            tree.edges
                .iter()
                .any(|e| e.parent == "Root" && e.child == "Sub"),
            "Root → Sub should exist from internal rendering"
        );

        // Sub→Root should NOT exist (reverse of Step 1 edge)
        let bad_edge = tree
            .edges
            .iter()
            .find(|e| e.parent == "Sub" && e.child == "Root");
        assert!(
            bad_edge.is_none(),
            "Sub → Root should be skipped (reverse of prior Root → Sub). Edges: {:?}",
            tree.edges
        );
    }

    // ── Signal A (Step 5.5): CSS layout_children tests ──────────────

    #[test]
    fn test_layout_children_creates_intermediate_edge() {
        // EmptyState scenario: CSS shows footer wraps actions (shared
        // rule with flex-wrap). Signal A should create
        // EmptyStateFooter → EmptyStateActions.
        let mut root_prof = make_profile("EmptyState");
        root_prof.has_children_prop = true;
        root_prof.bem_block = Some("emptyState".into());
        root_prof.css_tokens_used = tokens(&["styles.emptyState"]);

        let mut footer = make_profile("EmptyStateFooter");
        footer.has_children_prop = true;
        footer.bem_block = Some("emptyState".into());
        footer.css_tokens_used = tokens(&["styles.emptyStateFooter"]);

        let mut actions = make_profile("EmptyStateActions");
        actions.bem_block = Some("emptyState".into());
        actions.css_tokens_used = tokens(&["styles.emptyStateActions"]);

        let mut profiles = HashMap::new();
        profiles.insert("EmptyState".into(), root_prof);
        profiles.insert("EmptyStateFooter".into(), footer);
        profiles.insert("EmptyStateActions".into(), actions);

        let family = vec![
            "EmptyState".into(),
            "EmptyStateFooter".into(),
            "EmptyStateActions".into(),
        ];

        // CSS profile with layout_children: footer wraps actions
        let css_prof = CssBlockProfile {
            block: "emptyState".into(),
            layout_children: vec![("footer".into(), "actions".into())],
            ..Default::default()
        };

        let tree = {
            let css_map = HashMap::from([(css_prof.block.clone(), css_prof)]);
            let block_key = css_map.keys().next().unwrap().clone();
            build_composition_tree_v2(
                &profiles,
                &family,
                Some(&css_map),
                Some(&block_key),
                &[],
                None,
            )
        }
        .unwrap();

        // Signal A: EmptyStateFooter → EmptyStateActions (from layout_children)
        let footer_to_actions = tree
            .edges
            .iter()
            .find(|e| e.parent == "EmptyStateFooter" && e.child == "EmptyStateActions");
        assert!(
            footer_to_actions.is_some(),
            "Expected EmptyStateFooter → EmptyStateActions from layout_children. Edges: {:?}",
            tree.edges
        );
        assert_eq!(footer_to_actions.unwrap().strength, EdgeStrength::Allowed);
        assert!(footer_to_actions
            .unwrap()
            .bem_evidence
            .as_ref()
            .unwrap()
            .contains("CSS layout container"));

        // Signal B: EmptyStateFooter should get root edge (orphan with outgoing
        // from Signal A but no incoming). EmptyStateActions should NOT get
        // root edge (already has parent from Signal A).
        let root_to_footer = tree
            .edges
            .iter()
            .find(|e| e.parent == "EmptyState" && e.child == "EmptyStateFooter");
        assert!(
            root_to_footer.is_some(),
            "Expected EmptyState → EmptyStateFooter from BEM orphan fallback. Edges: {:?}",
            tree.edges
        );

        // EmptyStateActions should NOT be a direct child of root (has parent from Signal A)
        let root_to_actions = tree
            .edges
            .iter()
            .find(|e| e.parent == "EmptyState" && e.child == "EmptyStateActions");
        assert!(
            root_to_actions.is_none(),
            "EmptyState → EmptyStateActions should NOT exist (has parent from Signal A). Edges: {:?}",
            tree.edges
        );
    }

    // ── Signal B (Step 8.5): BEM element orphan fallback tests ──────

    #[test]
    fn test_bem_orphan_fallback_connects_orphans_to_root() {
        // Panel scenario: root has children, sub-components are BEM
        // elements with no other signals connecting them.
        let mut root_prof = make_profile("Panel");
        root_prof.has_children_prop = true;
        root_prof.bem_block = Some("panel".into());
        root_prof.css_tokens_used = tokens(&["styles.panel"]);

        let mut header = make_profile("PanelHeader");
        header.bem_block = Some("panel".into());
        header.css_tokens_used = tokens(&["styles.panelHeader"]);

        let mut main = make_profile("PanelMain");
        main.has_children_prop = true;
        main.bem_block = Some("panel".into());
        main.css_tokens_used = tokens(&["styles.panelMain"]);

        let mut footer = make_profile("PanelFooter");
        footer.bem_block = Some("panel".into());
        footer.css_tokens_used = tokens(&["styles.panelFooter"]);

        let mut profiles = HashMap::new();
        profiles.insert("Panel".into(), root_prof);
        profiles.insert("PanelHeader".into(), header);
        profiles.insert("PanelMain".into(), main);
        profiles.insert("PanelFooter".into(), footer);

        let family = vec![
            "Panel".into(),
            "PanelHeader".into(),
            "PanelMain".into(),
            "PanelFooter".into(),
        ];

        // Minimal CSS profile — just the block name, no nesting selectors
        let css_prof = CssBlockProfile {
            block: "panel".into(),
            ..Default::default()
        };

        let tree = {
            let css_map = HashMap::from([(css_prof.block.clone(), css_prof)]);
            let block_key = css_map.keys().next().unwrap().clone();
            build_composition_tree_v2(
                &profiles,
                &family,
                Some(&css_map),
                Some(&block_key),
                &[],
                None,
            )
        }
        .unwrap();

        // All 3 sub-components should be connected to root
        for child in &["PanelHeader", "PanelMain", "PanelFooter"] {
            let edge = tree
                .edges
                .iter()
                .find(|e| e.parent == "Panel" && e.child == *child);
            assert!(
                edge.is_some(),
                "Expected Panel → {} from BEM orphan fallback. Edges: {:?}",
                child,
                tree.edges
            );
            assert_eq!(edge.unwrap().strength, EdgeStrength::Allowed);
            assert!(edge
                .unwrap()
                .bem_evidence
                .as_ref()
                .unwrap()
                .contains("BEM element fallback"));
        }

        // All 3 should be retained as family members
        for member in &["PanelHeader", "PanelMain", "PanelFooter"] {
            assert!(
                tree.family_members.contains(&member.to_string()),
                "{} should be in family_members. Members: {:?}",
                member,
                tree.family_members
            );
        }
    }

    #[test]
    fn test_bem_orphan_fallback_skips_independent_block() {
        // Label/LabelGroup scenario: LabelGroup has its own BEM block
        // ("labelGroup") different from Label's ("label"). Signal B
        // should NOT create Label → LabelGroup.
        let mut label = make_profile("Label");
        label.has_children_prop = true;
        label.bem_block = Some("label".into());
        label.css_tokens_used = tokens(&["styles.label"]);

        let mut label_group = make_profile("LabelGroup");
        label_group.has_children_prop = true;
        label_group.bem_block = Some("labelGroup".into());
        // This token would match the prefix strip: "labelGroup" starts
        // with "label", producing false map entry "group" → "LabelGroup"
        label_group.css_tokens_used = tokens(&["styles.labelGroup"]);

        let mut profiles = HashMap::new();
        profiles.insert("Label".into(), label);
        profiles.insert("LabelGroup".into(), label_group);

        let family = vec!["Label".into(), "LabelGroup".into()];

        let css_prof = CssBlockProfile {
            block: "label".into(),
            ..Default::default()
        };

        let tree = {
            let css_map = HashMap::from([(css_prof.block.clone(), css_prof)]);
            let block_key = css_map.keys().next().unwrap().clone();
            build_composition_tree_v2(
                &profiles,
                &family,
                Some(&css_map),
                Some(&block_key),
                &[],
                None,
            )
        }
        .unwrap();

        // Label → LabelGroup should NOT exist (independent BEM block)
        let bad_edge = tree
            .edges
            .iter()
            .find(|e| e.parent == "Label" && e.child == "LabelGroup");
        assert!(
            bad_edge.is_none(),
            "Label → LabelGroup should NOT exist (independent BEM block). Edges: {:?}",
            tree.edges
        );
    }

    #[test]
    fn test_bem_orphan_fallback_skips_already_parented() {
        // If a component already has a parent from another signal,
        // Signal B should not create a duplicate root edge.
        let mut root_prof = make_profile("Menu");
        root_prof.has_children_prop = true;
        root_prof.bem_block = Some("menu".into());
        root_prof.css_tokens_used = tokens(&["styles.menu"]);
        root_prof.rendered_components = vec!["MenuContext.Provider".into()];

        let mut menu_list = make_profile("MenuList");
        menu_list.has_children_prop = true;
        menu_list.bem_block = Some("menu".into());
        menu_list.css_tokens_used = tokens(&["styles.menuList"]);
        menu_list.consumed_contexts = vec!["MenuContext".into()];

        let mut profiles = HashMap::new();
        profiles.insert("Menu".into(), root_prof);
        profiles.insert("MenuList".into(), menu_list);

        let family = vec!["Menu".into(), "MenuList".into()];

        let css_prof = CssBlockProfile {
            block: "menu".into(),
            ..Default::default()
        };

        let tree = {
            let css_map = HashMap::from([(css_prof.block.clone(), css_prof)]);
            let block_key = css_map.keys().next().unwrap().clone();
            build_composition_tree_v2(
                &profiles,
                &family,
                Some(&css_map),
                Some(&block_key),
                &[],
                None,
            )
        }
        .unwrap();

        // MenuList should have a parent from context nesting (Step 6)
        let context_edge = tree
            .edges
            .iter()
            .find(|e| e.parent == "Menu" && e.child == "MenuList");
        assert!(
            context_edge.is_some(),
            "Expected Menu → MenuList from context nesting. Edges: {:?}",
            tree.edges
        );

        // Should be exactly ONE edge from Menu → MenuList (not duplicated
        // by Signal B)
        let edge_count = tree
            .edges
            .iter()
            .filter(|e| e.parent == "Menu" && e.child == "MenuList")
            .count();
        assert_eq!(
            edge_count, 1,
            "Should have exactly 1 Menu → MenuList edge, not duplicated by Signal B. Edges: {:?}",
            tree.edges
        );
    }

    #[test]
    fn test_bem_orphan_fallback_skips_when_root_has_no_children() {
        // If root doesn't have has_children_prop, Signal B should not fire.
        let mut root_prof = make_profile("Widget");
        root_prof.has_children_prop = false; // no children!
        root_prof.bem_block = Some("widget".into());
        root_prof.css_tokens_used = tokens(&["styles.widget"]);

        let mut sub = make_profile("WidgetBody");
        sub.bem_block = Some("widget".into());
        sub.css_tokens_used = tokens(&["styles.widgetBody"]);

        let mut profiles = HashMap::new();
        profiles.insert("Widget".into(), root_prof);
        profiles.insert("WidgetBody".into(), sub);

        let family = vec!["Widget".into(), "WidgetBody".into()];

        let css_prof = CssBlockProfile {
            block: "widget".into(),
            ..Default::default()
        };

        let tree = {
            let css_map = HashMap::from([(css_prof.block.clone(), css_prof)]);
            let block_key = css_map.keys().next().unwrap().clone();
            build_composition_tree_v2(
                &profiles,
                &family,
                Some(&css_map),
                Some(&block_key),
                &[],
                None,
            )
        }
        .unwrap();

        // WidgetBody should NOT be connected (root has no children prop)
        let edge = tree
            .edges
            .iter()
            .find(|e| e.parent == "Widget" && e.child == "WidgetBody");
        assert!(
            edge.is_none(),
            "Widget → WidgetBody should NOT exist (root has no children). Edges: {:?}",
            tree.edges
        );
    }

    #[test]
    fn test_bem_orphan_fallback_promotes_secondary_root() {
        // JumpLinks scenario: JumpLinksList is a secondary root (has
        // outgoing edge to JumpLinksItem via DOM nesting but no incoming).
        // Signal B should create JumpLinks → JumpLinksList.
        let mut root_prof = make_profile("JumpLinks");
        root_prof.has_children_prop = true;
        root_prof.bem_block = Some("jumpLinks".into());
        root_prof.css_tokens_used = tokens(&["styles.jumpLinks"]);

        let mut list = make_profile("JumpLinksList");
        list.has_children_prop = true;
        list.bem_block = Some("jumpLinks".into());
        list.css_tokens_used = tokens(&["styles.jumpLinksList"]);
        list.children_slot_path = vec!["ul".into()];
        list.rendered_elements.insert("ul".into(), 1);

        let mut item = make_profile("JumpLinksItem");
        item.bem_block = Some("jumpLinks".into());
        item.css_tokens_used = tokens(&["styles.jumpLinksItem"]);
        item.children_slot_path = vec!["li".into(), "button".into()];
        item.rendered_elements.insert("li".into(), 1);

        let mut profiles = HashMap::new();
        profiles.insert("JumpLinks".into(), root_prof);
        profiles.insert("JumpLinksList".into(), list);
        profiles.insert("JumpLinksItem".into(), item);

        let family = vec![
            "JumpLinks".into(),
            "JumpLinksList".into(),
            "JumpLinksItem".into(),
        ];

        let css_prof = CssBlockProfile {
            block: "jumpLinks".into(),
            ..Default::default()
        };

        let tree = {
            let css_map = HashMap::from([(css_prof.block.clone(), css_prof)]);
            let block_key = css_map.keys().next().unwrap().clone();
            build_composition_tree_v2(
                &profiles,
                &family,
                Some(&css_map),
                Some(&block_key),
                &[],
                None,
            )
        }
        .unwrap();

        // JumpLinksList → JumpLinksItem from DOM nesting (Step 7)
        let list_to_item = tree
            .edges
            .iter()
            .find(|e| e.parent == "JumpLinksList" && e.child == "JumpLinksItem");
        assert!(
            list_to_item.is_some(),
            "Expected JumpLinksList → JumpLinksItem from DOM nesting. Edges: {:?}",
            tree.edges
        );

        // JumpLinks → JumpLinksList from Signal B (secondary root promotion)
        let root_to_list = tree
            .edges
            .iter()
            .find(|e| e.parent == "JumpLinks" && e.child == "JumpLinksList");
        assert!(
            root_to_list.is_some(),
            "Expected JumpLinks → JumpLinksList from BEM orphan fallback. Edges: {:?}",
            tree.edges
        );

        // The full chain: JumpLinks → JumpLinksList → JumpLinksItem
        // After Step 9 (suppress), root→JumpLinksItem should not exist
        // (because JumpLinksList is an intermediate).
        let root_to_item = tree
            .edges
            .iter()
            .find(|e| e.parent == "JumpLinks" && e.child == "JumpLinksItem");
        assert!(
            root_to_item.is_none(),
            "JumpLinks → JumpLinksItem should not exist (JumpLinksList is intermediate). Edges: {:?}",
            tree.edges
        );

        // All 3 should be retained
        assert_eq!(tree.family_members.len(), 3);
    }

    #[test]
    fn test_layout_children_and_orphan_fallback_interaction() {
        // Verifies Signal A runs before Signal B. With layout_children
        // creating an intermediate edge, Signal B should not create a
        // redundant root→leaf edge.
        let mut root_prof = make_profile("EmptyState");
        root_prof.has_children_prop = true;
        root_prof.bem_block = Some("emptyState".into());
        root_prof.css_tokens_used = tokens(&["styles.emptyState"]);

        let mut body = make_profile("EmptyStateBody");
        body.bem_block = Some("emptyState".into());
        body.css_tokens_used = tokens(&["styles.emptyStateBody"]);

        let mut footer = make_profile("EmptyStateFooter");
        footer.has_children_prop = true;
        footer.bem_block = Some("emptyState".into());
        footer.css_tokens_used = tokens(&["styles.emptyStateFooter"]);

        let mut actions = make_profile("EmptyStateActions");
        actions.bem_block = Some("emptyState".into());
        actions.css_tokens_used = tokens(&["styles.emptyStateActions"]);

        let mut profiles = HashMap::new();
        profiles.insert("EmptyState".into(), root_prof);
        profiles.insert("EmptyStateBody".into(), body);
        profiles.insert("EmptyStateFooter".into(), footer);
        profiles.insert("EmptyStateActions".into(), actions);

        let family = vec![
            "EmptyState".into(),
            "EmptyStateBody".into(),
            "EmptyStateFooter".into(),
            "EmptyStateActions".into(),
        ];

        let css_prof = CssBlockProfile {
            block: "emptyState".into(),
            layout_children: vec![("footer".into(), "actions".into())],
            ..Default::default()
        };

        let tree = {
            let css_map = HashMap::from([(css_prof.block.clone(), css_prof)]);
            let block_key = css_map.keys().next().unwrap().clone();
            build_composition_tree_v2(
                &profiles,
                &family,
                Some(&css_map),
                Some(&block_key),
                &[],
                None,
            )
        }
        .unwrap();

        // Expected tree:
        // EmptyState
        // ├── EmptyStateBody       (Signal B: orphan)
        // └── EmptyStateFooter     (Signal B: orphan with outgoing from Signal A)
        //     └── EmptyStateActions (Signal A: layout_children)

        // Signal A: Footer → Actions
        assert!(tree
            .edges
            .iter()
            .any(|e| e.parent == "EmptyStateFooter" && e.child == "EmptyStateActions"));

        // Signal B: Root → Body (orphan)
        assert!(tree
            .edges
            .iter()
            .any(|e| e.parent == "EmptyState" && e.child == "EmptyStateBody"));

        // Signal B: Root → Footer (orphan — no incoming, only outgoing from A)
        assert!(tree
            .edges
            .iter()
            .any(|e| e.parent == "EmptyState" && e.child == "EmptyStateFooter"));

        // Signal B should NOT create Root → Actions (already parented by Footer)
        assert!(
            !tree
                .edges
                .iter()
                .any(|e| e.parent == "EmptyState" && e.child == "EmptyStateActions"),
            "EmptyState → EmptyStateActions should NOT exist. Edges: {:?}",
            tree.edges
        );

        // All 4 members retained
        assert_eq!(tree.family_members.len(), 4);
    }

    #[test]
    fn test_bem_orphan_fallback_no_css_profile() {
        // When no CSS profile is provided, Signal B should not fire
        // (css_to_component is empty).
        let mut root_prof = make_profile("Panel");
        root_prof.has_children_prop = true;
        root_prof.bem_block = Some("panel".into());
        root_prof.css_tokens_used = tokens(&["styles.panel"]);

        let mut header = make_profile("PanelHeader");
        header.bem_block = Some("panel".into());
        header.css_tokens_used = tokens(&["styles.panelHeader"]);

        let mut profiles = HashMap::new();
        profiles.insert("Panel".into(), root_prof);
        profiles.insert("PanelHeader".into(), header);

        let family = vec!["Panel".into(), "PanelHeader".into()];

        // No CSS profile — Signal B should not fire
        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[], None).unwrap();

        // PanelHeader should be dropped (no edges, no CSS profile)
        assert!(
            !tree.family_members.contains(&"PanelHeader".to_string()),
            "PanelHeader should be dropped without CSS profile. Members: {:?}",
            tree.family_members
        );
    }

    // ── Step 1.5: Delegate tree projection tests ────────────────────

    #[test]
    fn test_delegate_projection_dropdown_menu() {
        // Dropdown wraps Menu. Menu tree has Menu → MenuList → MenuItem.
        // Projection should produce Dropdown → DropdownList → DropdownItem.

        // Build the Menu "delegate" tree
        let menu_tree = CompositionTree {
            root: "Menu".into(),
            family_members: vec![
                "Menu".into(),
                "MenuList".into(),
                "MenuItem".into(),
                "MenuGroup".into(),
            ],
            edges: vec![
                CompositionEdge {
                    parent: "Menu".into(),
                    child: "MenuList".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: Some("context nesting".into()),
                    strength: EdgeStrength::Required,
                    prop_name: None,
                },
                CompositionEdge {
                    parent: "MenuList".into(),
                    child: "MenuItem".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: Some("DOM nesting: ul → li".into()),
                    strength: EdgeStrength::Required,
                    prop_name: None,
                },
                CompositionEdge {
                    parent: "Menu".into(),
                    child: "MenuGroup".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: Some("CSS descendant".into()),
                    strength: EdgeStrength::Allowed,
                    prop_name: None,
                },
            ],
        };

        // Dropdown family profiles (thin wrappers, no CSS)
        let mut dropdown = make_profile("Dropdown");
        dropdown.has_children_prop = true;

        let mut dropdown_list = make_profile("DropdownList");
        dropdown_list.has_children_prop = true;

        let mut dropdown_item = make_profile("DropdownItem");
        dropdown_item.has_children_prop = true;

        let mut dropdown_group = make_profile("DropdownGroup");
        dropdown_group.has_children_prop = true;

        let mut profiles = HashMap::new();
        profiles.insert("Dropdown".into(), dropdown);
        profiles.insert("DropdownList".into(), dropdown_list);
        profiles.insert("DropdownItem".into(), dropdown_item);
        profiles.insert("DropdownGroup".into(), dropdown_group);

        let family = vec![
            "Dropdown".into(),
            "DropdownList".into(),
            "DropdownItem".into(),
            "DropdownGroup".into(),
        ];

        // Wrapper → delegate mapping
        let mut wrapper_map = HashMap::new();
        wrapper_map.insert("Dropdown".into(), "Menu".into());
        wrapper_map.insert("DropdownList".into(), "MenuList".into());
        wrapper_map.insert("DropdownItem".into(), "MenuItem".into());
        wrapper_map.insert("DropdownGroup".into(), "MenuGroup".into());

        let ctx = DelegateContext {
            delegate_tree: &menu_tree,
            wrapper_to_delegate: wrapper_map,
        };

        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[ctx], None).unwrap();

        // Dropdown → DropdownList (from Menu → MenuList)
        assert!(
            tree.edges
                .iter()
                .any(|e| e.parent == "Dropdown" && e.child == "DropdownList"),
            "Expected Dropdown → DropdownList. Edges: {:?}",
            tree.edges
        );

        // DropdownList → DropdownItem (from MenuList → MenuItem)
        assert!(
            tree.edges
                .iter()
                .any(|e| e.parent == "DropdownList" && e.child == "DropdownItem"),
            "Expected DropdownList → DropdownItem. Edges: {:?}",
            tree.edges
        );

        // Dropdown → DropdownGroup (from Menu → MenuGroup)
        assert!(
            tree.edges
                .iter()
                .any(|e| e.parent == "Dropdown" && e.child == "DropdownGroup"),
            "Expected Dropdown → DropdownGroup. Edges: {:?}",
            tree.edges
        );

        // All edges should be Allowed (delegation is optional)
        for edge in &tree.edges {
            assert_eq!(
                edge.strength,
                EdgeStrength::Allowed,
                "Projected edge {} → {} should be Allowed",
                edge.parent,
                edge.child
            );
        }

        // All 4 members retained (not dropped by Step 10)
        assert_eq!(
            tree.family_members.len(),
            4,
            "All 4 members should be retained. Members: {:?}",
            tree.family_members
        );

        // Evidence should reference delegate projection
        let dd_to_dl = tree
            .edges
            .iter()
            .find(|e| e.parent == "Dropdown" && e.child == "DropdownList")
            .unwrap();
        assert!(
            dd_to_dl
                .bem_evidence
                .as_ref()
                .unwrap()
                .contains("Delegate projection"),
            "Evidence should reference delegate projection: {:?}",
            dd_to_dl.bem_evidence
        );
    }

    #[test]
    fn test_delegate_projection_no_edge_for_unmapped_members() {
        // If a delegate tree edge has one side unmapped, no edge is created.
        let delegate_tree = CompositionTree {
            root: "Menu".into(),
            family_members: vec!["Menu".into(), "MenuList".into(), "MenuItem".into()],
            edges: vec![
                CompositionEdge {
                    parent: "Menu".into(),
                    child: "MenuList".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: EdgeStrength::Required,
                    prop_name: None,
                },
                CompositionEdge {
                    parent: "MenuList".into(),
                    child: "MenuItem".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: None,
                    strength: EdgeStrength::Required,
                    prop_name: None,
                },
            ],
        };

        // Only map root and list, NOT item
        let mut wrapper_map = HashMap::new();
        wrapper_map.insert("Wrapper".into(), "Menu".into());
        wrapper_map.insert("WrapperList".into(), "MenuList".into());
        // WrapperItem is NOT mapped

        let ctx = DelegateContext {
            delegate_tree: &delegate_tree,
            wrapper_to_delegate: wrapper_map,
        };

        let wrapper = make_profile("Wrapper");
        let wrapper_list = make_profile("WrapperList");
        let wrapper_item = make_profile("WrapperItem");

        let mut profiles = HashMap::new();
        profiles.insert("Wrapper".into(), wrapper);
        profiles.insert("WrapperList".into(), wrapper_list);
        profiles.insert("WrapperItem".into(), wrapper_item);

        let family = vec!["Wrapper".into(), "WrapperList".into(), "WrapperItem".into()];

        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[ctx], None).unwrap();

        // Wrapper → WrapperList should exist (both mapped)
        assert!(tree
            .edges
            .iter()
            .any(|e| e.parent == "Wrapper" && e.child == "WrapperList"));

        // WrapperList → WrapperItem should NOT exist (WrapperItem not mapped)
        assert!(
            !tree
                .edges
                .iter()
                .any(|e| e.parent == "WrapperList" && e.child == "WrapperItem"),
            "WrapperList → WrapperItem should not exist (unmapped). Edges: {:?}",
            tree.edges
        );

        // WrapperItem should be dropped (no edges)
        assert!(
            !tree.family_members.contains(&"WrapperItem".to_string()),
            "WrapperItem should be dropped. Members: {:?}",
            tree.family_members
        );
    }

    #[test]
    fn test_delegate_projection_empty_delegate_tree() {
        // If the delegate tree has no edges (e.g., Button), nothing is projected.
        let delegate_tree = CompositionTree {
            root: "Button".into(),
            family_members: vec!["Button".into()],
            edges: vec![],
        };

        let mut wrapper_map = HashMap::new();
        wrapper_map.insert("MyButton".into(), "Button".into());

        let ctx = DelegateContext {
            delegate_tree: &delegate_tree,
            wrapper_to_delegate: wrapper_map,
        };

        let my_button = make_profile("MyButton");
        let mut profiles = HashMap::new();
        profiles.insert("MyButton".into(), my_button);

        let family = vec!["MyButton".into()];

        let tree = build_composition_tree_v2(&profiles, &family, None, None, &[ctx], None).unwrap();

        // No edges projected (Button has no edges)
        assert!(tree.edges.is_empty());
        assert_eq!(tree.family_members.len(), 1);
    }

    /// Test Step 8.6: Secondary BEM block sub-root fallback.
    ///
    /// Simulates the Modal family pattern where the root (Modal) uses BEM
    /// block "backdrop" while sub-components (ModalBody, ModalFooter) use
    /// BEM block "modalBox". An internal component (ModalBox) acts as the
    /// sub-root for the "modalBox" block.
    ///
    /// Step 8.6 should connect ModalBody and ModalFooter to ModalBox as
    /// orphan BEM elements of the secondary block.
    #[test]
    fn test_secondary_block_subroot_fallback() {
        let mut profiles = HashMap::new();

        // Modal: root, uses "backdrop" block, renders ModalContent internally
        let mut modal = make_profile("Modal");
        modal.bem_block = Some("backdrop".into());
        modal.rendered_components = vec!["ModalContent".into()];
        profiles.insert("Modal".into(), modal);

        // ModalContent: internal, renders ModalBox
        let mut modal_content = make_profile("ModalContent");
        modal_content.rendered_components = vec!["ModalBox".into()];
        profiles.insert("ModalContent".into(), modal_content);

        // ModalBox: internal, sub-root for "modalBox" block, has children
        let mut modal_box = make_profile("ModalBox");
        modal_box.bem_block = Some("modalBox".into());
        modal_box.css_tokens_used = ["styles.modalBox".to_string()].into_iter().collect();
        modal_box.has_children_prop = true;
        profiles.insert("ModalBox".into(), modal_box);

        // ModalBody: uses "modalBox" block, orphan (renders only HTML)
        let mut modal_body = make_profile("ModalBody");
        modal_body.bem_block = Some("modalBox".into());
        modal_body.css_tokens_used = ["styles.modalBoxBody".to_string()].into_iter().collect();
        profiles.insert("ModalBody".into(), modal_body);

        // ModalFooter: uses "modalBox" block, orphan (renders only HTML)
        let mut modal_footer = make_profile("ModalFooter");
        modal_footer.bem_block = Some("modalBox".into());
        modal_footer.css_tokens_used = ["styles.modalBoxFooter".to_string()].into_iter().collect();
        profiles.insert("ModalFooter".into(), modal_footer);

        let family = vec![
            "Modal".into(),
            "ModalContent".into(),
            "ModalBox".into(),
            "ModalBody".into(),
            "ModalFooter".into(),
        ];

        // CSS profiles: we need "modalBox" block to exist so Step 8.6 fires
        let modal_box_css = CssBlockProfile {
            block: "modalBox".into(),
            ..Default::default()
        };
        let css_map = HashMap::from([("modalBox".to_string(), modal_box_css)]);

        // Primary block is "backdrop" (from root) but no CSS profile for it
        let tree = build_composition_tree_v2(
            &profiles,
            &family,
            Some(&css_map),
            None, // no primary CSS profile for "backdrop"
            &[],
            None,
        )
        .unwrap();

        // Step 1 should create: Modal → ModalContent, ModalContent → ModalBox
        // Step 8.6 should create: ModalBox → ModalBody, ModalBox → ModalFooter
        let box_to_body = tree
            .edges
            .iter()
            .any(|e| e.parent == "ModalBox" && e.child == "ModalBody");
        let box_to_footer = tree
            .edges
            .iter()
            .any(|e| e.parent == "ModalBox" && e.child == "ModalFooter");

        assert!(
            box_to_body,
            "Expected ModalBox → ModalBody edge from secondary block fallback. Edges: {:?}",
            tree.edges
        );
        assert!(
            box_to_footer,
            "Expected ModalBox → ModalFooter edge from secondary block fallback. Edges: {:?}",
            tree.edges
        );

        // All 5 members should be retained
        assert_eq!(
            tree.family_members.len(),
            5,
            "All members should be retained. Members: {:?}",
            tree.family_members
        );
    }
}
