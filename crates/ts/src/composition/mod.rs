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

/// Build a composition tree using CSS structure, React patterns, and HTML
/// semantics instead of BEM-based edge creation.
///
/// BEM determines family membership only. All parent-child edges come from:
/// 1. Internal rendering (A renders B in JSX)
/// 2. CSS direct-child selectors (`.A > .B`)
/// 3. CSS grid parent-child (`A` has grid-template, `B` has grid-column)
/// 4. CSS flex context (A wraps children in flex container, B is not a grid child)
/// 5. CSS descendant selectors (`.A .B`)
/// 6. React context (A provides, B consumes)
/// 7. DOM nesting (A wraps children in `<ul>`, B renders `<li>`)
/// 8. cloneElement threading (A injects props into children that B declares)
/// 9. Default root (unparented members → root→member)
/// 10. Suppress root edges when intermediate exists
pub fn build_composition_tree_v2(
    profiles: &HashMap<String, ComponentSourceProfile>,
    family_exports: &[String],
    css_profile: Option<&CssBlockProfile>,
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
                    });
                }
            }
        }
    }

    if let Some(css_prof) = css_profile {
        // ── Step 2: CSS direct-child selectors ──────────────────────
        for (css_parent, css_child) in &css_prof.direct_child_nesting {
            if let (Some(parent_comp), Some(child_comp)) = (
                css_to_component.get(css_parent.as_str()),
                css_to_component.get(css_child.as_str()),
            ) {
                let key = (parent_comp.clone(), child_comp.clone());
                if parent_comp != child_comp && edge_set.insert(key) {
                    tree.edges.push(CompositionEdge {
                        parent: parent_comp.clone(),
                        child: child_comp.clone(),
                        relationship: ChildRelationship::DirectChild,
                        required: false,
                        bem_evidence: Some(format!(
                            "CSS direct child: .{} > .{}",
                            css_parent, css_child
                        )),
                        strength: EdgeStrength::Required,
                    });
                }
            }
        }

        // ── Step 3: CSS grid parent-child ───────────────────────────
        // Find grid containers (has_grid_template) and grid children
        // (has_grid_column/grid_row). Map to components.
        let grid_containers: Vec<(&str, &str)> = css_prof
            .elements
            .iter()
            .filter(|(_, info)| info.has_grid_template && info.display_values.contains("grid"))
            .filter_map(|(el, _)| {
                css_to_component
                    .get(el.as_str())
                    .map(|comp| (el.as_str(), comp.as_str()))
            })
            .collect();

        for (child_el, child_info) in &css_prof.elements {
            if !child_info.has_grid_column && !child_info.has_grid_row {
                continue;
            }
            let Some(child_comp) = css_to_component.get(child_el.as_str()) else {
                continue;
            };

            // Find the best grid container for this child.
            // Prefer CSS selector evidence, then fall back to the most
            // specific (longest name) grid container.
            let mut best_parent: Option<&str> = None;

            // Check direct-child selectors first
            for (container_el, container_comp) in &grid_containers {
                if *container_comp == child_comp.as_str() {
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

            // Then check descendant selectors
            if best_parent.is_none() {
                for (container_el, container_comp) in &grid_containers {
                    if *container_comp == child_comp.as_str() {
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

            // Fall back to most specific grid container
            if best_parent.is_none() && grid_containers.len() == 1 {
                let (_, container_comp) = grid_containers[0];
                if container_comp != child_comp.as_str() {
                    best_parent = Some(container_comp);
                }
            }

            if let Some(parent_comp) = best_parent {
                let key = (parent_comp.to_string(), child_comp.clone());
                if edge_set.insert(key) {
                    tree.edges.push(CompositionEdge {
                        parent: parent_comp.to_string(),
                        child: child_comp.clone(),
                        relationship: ChildRelationship::DirectChild,
                        required: false,
                        bem_evidence: Some(format!(
                            "CSS grid: {} has grid-template, {} has grid-column/row",
                            parent_comp, child_comp
                        )),
                        strength: EdgeStrength::Required,
                    });
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
        let non_root_grid_containers: Vec<(&str, &str)> = grid_containers
            .iter()
            .filter(|(el, _)| {
                // Must not be root and must itself be a grid child
                !el.is_empty()
                    && css_prof
                        .elements
                        .get(*el)
                        .is_some_and(|info| info.has_grid_column || info.has_grid_row)
            })
            .copied()
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
                let Some(child_comp) = css_to_component.get(child_el.as_str()) else {
                    continue;
                };
                // Skip if already has a non-root parent
                if tree
                    .edges
                    .iter()
                    .any(|e| e.child == *child_comp && e.parent != root)
                {
                    continue;
                }

                // Find the best non-root grid container for this element.
                // Use CSS selector evidence, then fall back.
                let mut best_parent: Option<&str> = None;

                for (container_el, container_comp) in &non_root_grid_containers {
                    if *container_comp == child_comp.as_str() {
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

                // Fall back: if only one non-root grid container, use it
                if best_parent.is_none() && non_root_grid_containers.len() == 1 {
                    let (_, comp) = non_root_grid_containers[0];
                    if comp != child_comp.as_str() {
                        best_parent = Some(comp);
                    }
                }

                if let Some(parent_comp) = best_parent {
                    let key = (parent_comp.to_string(), child_comp.clone());
                    if edge_set.insert(key) {
                        tree.edges.push(CompositionEdge {
                            parent: parent_comp.to_string(),
                            child: child_comp.clone(),
                            relationship: ChildRelationship::DirectChild,
                            required: false,
                            bem_evidence: Some(format!(
                                "CSS grid: {} is grid container, {} is implicit grid child",
                                parent_comp, child_comp
                            )),
                            strength: EdgeStrength::Required,
                        });
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
                            });
                        }
                    }
                }
            }
        }

        // ── Step 5: CSS descendant selectors ────────────────────────
        for (css_parent, css_child) in &css_prof.descendant_nesting {
            if let (Some(parent_comp), Some(child_comp)) = (
                css_to_component.get(css_parent.as_str()),
                css_to_component.get(css_child.as_str()),
            ) {
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
                    });
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

    // ── Step 9: Suppress + dedup ───────────────────────────────────
    deduplicate_edges(&mut tree);
    suppress_root_edges_with_intermediate(&mut tree);

    // ── Step 10: Drop unconnected members ───────────────────────────
    // Members with no incoming edge from any signal are dropped from
    // the tree entirely. No "default to root" guessing — every edge
    // must have structural evidence. Unconnected members may be
    // standalone components, context objects, type exports, or members
    // that need stronger signals to connect.
    let parented: HashSet<&str> = tree.edges.iter().map(|e| e.child.as_str()).collect();
    tree.family_members
        .retain(|m| m == &root || parented.contains(m.as_str()));

    Some(tree)
}

/// Infer parent→child edges from cloneElement prop injection chains.
///
/// If component A uses `cloneElement(child, { prop1 })` and family member B
/// declares `prop1` in its interface, then B is a child of A.
fn infer_clone_element_nesting(
    tree: &mut CompositionTree,
    profiles: &HashMap<String, ComponentSourceProfile>,
    family_exports: &[String],
) {
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
                });
            }
        }
    }

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
) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();

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

                // Don't overwrite — first component to claim an element wins.
                // This handles cases where multiple components use the same
                // block token (e.g., both Toolbar and ToolbarContent use
                // styles.toolbar for the root).
                map.entry(element_name).or_insert_with(|| comp_name.clone());
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
            strength: EdgeStrength::Allowed,
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

                    new_edges.push(CompositionEdge {
                        parent: provider_name.clone(),
                        child: child_name.clone(),
                        relationship: ChildRelationship::DirectChild,
                        required: false,
                        bem_evidence: Some(format!(
                            "Context nesting: {} provides {}, {} consumes it via useContext",
                            provider_name, consumed_ctx, child_name
                        )),
                        strength: EdgeStrength::Required,
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

    fn make_profile(name: &str) -> ComponentSourceProfile {
        ComponentSourceProfile {
            name: name.to_string(),
            ..Default::default()
        }
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

        let tree = build_composition_tree_v2(&profiles, &family, None).unwrap();

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

        let tree = build_composition_tree_v2(&profiles, &family, None).unwrap();

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
                },
                CompositionEdge {
                    parent: "Accordion".into(),
                    child: "AccordionToggle".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: Some("BEM element of accordion block".into()),
                    strength: EdgeStrength::Allowed,
                },
                // Correct root edge (no intermediate for AccordionItem)
                CompositionEdge {
                    parent: "Accordion".into(),
                    child: "AccordionItem".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: Some("BEM element of accordion block".into()),
                    strength: EdgeStrength::Allowed,
                },
                // Context-derived intermediate edges (should be kept)
                CompositionEdge {
                    parent: "AccordionItem".into(),
                    child: "AccordionContent".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: Some("Context nesting".into()),
                    strength: EdgeStrength::Allowed,
                },
                CompositionEdge {
                    parent: "AccordionItem".into(),
                    child: "AccordionToggle".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: Some("Context nesting".into()),
                    strength: EdgeStrength::Allowed,
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
                },
                CompositionEdge {
                    parent: "Masthead".into(),
                    child: "MastheadContent".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: None,
                    strength: EdgeStrength::Allowed,
                },
                CompositionEdge {
                    parent: "Masthead".into(),
                    child: "MastheadMain".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: None,
                    strength: EdgeStrength::Allowed,
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

        let tree = build_composition_tree_v2(&profiles, &family, None).unwrap();

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

        let tree = build_composition_tree_v2(&profiles, &family, None).unwrap();

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

        let tree = build_composition_tree_v2(&profiles, &family, None).unwrap();

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

        let tree = build_composition_tree_v2(&profiles, &family, None).unwrap();

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

        let tree = build_composition_tree_v2(&profiles, &family, None).unwrap();

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
}
