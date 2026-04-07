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

use crate::source_profile::bem::{classify_bem_relationship, BemRelationship};
use semver_analyzer_core::types::sd::{
    ChildRelationship, ComponentSourceProfile, CompositionEdge, CompositionTree,
};
use std::collections::{HashMap, HashSet};
use tracing::debug;

/// Build a composition tree from a set of component profiles in the same family.
///
/// `profiles` contains the extracted profile for each family member.
/// `family_exports` is the list of component names exported from the family's
/// index file (e.g., ["Dropdown", "DropdownList", "DropdownItem", "DropdownGroup"]).
///
/// The root is typically the first export or the component matching the
/// directory name.
pub fn build_composition_tree(
    profiles: &HashMap<String, ComponentSourceProfile>,
    family_exports: &[String],
) -> Option<CompositionTree> {
    if family_exports.is_empty() {
        return None;
    }

    let root = family_exports[0].clone();

    let mut tree = CompositionTree {
        root: root.clone(),
        family_members: family_exports.to_vec(),
        edges: Vec::new(),
    };

    let family_set: HashSet<&str> = family_exports.iter().map(|s| s.as_str()).collect();

    // For each family member, determine its expected children from:
    // 1. Its rendered_components (which family members it renders internally)
    // 2. BEM relationships between its tokens and child tokens
    // 3. Whether the child is internal or consumer-provided
    for parent_name in family_exports {
        let Some(parent_profile) = profiles.get(parent_name) else {
            continue;
        };

        // Which family members does this component render?
        let internal_children: Vec<&str> = parent_profile
            .rendered_components
            .iter()
            .filter(|c| family_set.contains(c.as_str()))
            .map(|s| s.as_str())
            .collect();

        // For each rendered family member, classify the relationship
        for child_name in &internal_children {
            let child_profile = profiles.get(*child_name);

            let (relationship, bem_evidence) = if let Some(child_prof) = child_profile {
                classify_child_relationship(parent_profile, child_prof)
            } else {
                (ChildRelationship::Unknown, None)
            };

            tree.edges.push(CompositionEdge {
                parent: parent_name.clone(),
                child: child_name.to_string(),
                relationship: relationship.clone(),
                required: relationship == ChildRelationship::BemElement,
                bem_evidence,
            });
        }

        // Also check: which family members does this component NOT render
        // internally, but could accept as children (via the children slot)?
        // These are the consumer-provided children.
        //
        // We only add an edge when there's positive evidence for the
        // parent→child relationship (BEM tokens). Without evidence, we
        // skip — wrapper families like Dropdown (wraps Menu) will get
        // their edges projected from the delegate family's tree in a
        // separate post-processing pass (see `project_delegate_trees`).
        if parent_profile.has_children_prop {
            for sibling_name in family_exports {
                if sibling_name == parent_name {
                    continue;
                }
                if internal_children.contains(&sibling_name.as_str()) {
                    continue;
                }
                if is_transitively_internal(sibling_name, parent_name, profiles, &family_set) {
                    continue;
                }

                let child_profile = profiles.get(sibling_name);
                let (relationship, bem_evidence) = if let Some(child_prof) = child_profile {
                    classify_child_relationship(parent_profile, child_prof)
                } else {
                    (ChildRelationship::Unknown, None)
                };

                debug!(
                    parent = %parent_name,
                    child = %sibling_name,
                    ?relationship,
                    ?bem_evidence,
                    parent_block = ?parent_profile.bem_block,
                    "consumer-child BEM classification"
                );

                // Only add edge with BEM element evidence.
                // IndependentBlock means the child has its OWN block —
                // that doesn't imply containment (it could go anywhere).
                // Only BemElement proves the child IS a structural part
                // of the parent's block.
                if matches!(relationship, ChildRelationship::BemElement) {
                    // Skip components that are passed via a ReactNode prop
                    // on the parent rather than placed as JSX children.
                    // e.g., AlertActionCloseButton is passed via Alert's
                    // `actionClose` prop, not as a child of <Alert>.
                    if is_prop_passed_component(parent_profile, sibling_name, parent_name) {
                        debug!(
                            parent = %parent_name,
                            child = %sibling_name,
                            "skipping BEM edge — child is prop-passed, not a JSX child"
                        );
                        continue;
                    }

                    // BEM evidence proves structural membership in the
                    // parent's block but NOT that the child is required.
                    // Only internally rendered children are required.
                    // Consumer-provided children (via {children} slot) are
                    // optional unless proven otherwise (e.g., DropdownGroup
                    // is optional; DropdownList is conventional but not
                    // enforced by the component).
                    tree.edges.push(CompositionEdge {
                        parent: parent_name.clone(),
                        child: sibling_name.clone(),
                        relationship: ChildRelationship::DirectChild,
                        required: false,
                        bem_evidence,
                    });
                }
            }
        }
    }

    // ── Block ownership by name prefix ────────────────────────────
    //
    // When the root component doesn't share a BEM block with its children
    // (e.g., Modal imports from Backdrop, but ModalHeader/Body/Footer
    // import from ModalBox), check if the root's name is a prefix of the
    // children's shared block. If so, claim them as BEM elements.
    infer_ownership_by_name_prefix(&mut tree, profiles, family_exports, &root);

    // ── DOM + Context nesting inference ────────────────────────────
    //
    // Infer nesting from two signals BEM can't express:
    //
    // 1. HTML element semantics: If parent wraps children in <ul> and
    //    child renders <li> → child goes inside parent. Catches
    //    MenuList → MenuItem (ul → li).
    //
    // 2. React Context: If parent renders <XContext.Provider> and
    //    child calls useContext(XContext) → child must be nested
    //    somewhere under parent.
    infer_dom_nesting(&mut tree, profiles, family_exports);
    infer_context_nesting(&mut tree, profiles, family_exports);

    // Remove edges where the child is internal to a parent that is itself
    // internal. We want consumer-facing edges only.
    deduplicate_edges(&mut tree);

    // Suppress BEM-derived root→child edges when a more specific
    // intermediate→child edge exists from DOM nesting, React context,
    // or delegation projection. BEM tells us a component uses CSS tokens
    // from the root's block, but that doesn't mean it's a direct JSX
    // child of the root — it may be nested inside an intermediate wrapper.
    //
    // Example: BEM says Accordion→AccordionContent (uses accordion
    // block tokens), but context nesting proves AccordionItem is the
    // actual JSX parent. We suppress the root edge so conformance rules
    // generate "must be in AccordionItem" instead of "must be in Accordion".
    //
    // Only suppresses when the child has a direct_child edge from BOTH
    // the root AND a non-root intermediate family member.
    suppress_root_edges_with_intermediate(&mut tree);

    Some(tree)
}

/// Classify the BEM relationship between a parent and child component.
///
/// BEM element edges are only valid when the parent IS the block owner
/// (has a raw token that exactly equals its block name, e.g., `styles.masthead`
/// for block "masthead"). Components that only have element-level tokens
/// (e.g., `styles.mastheadBrand`) are elements themselves and should not
/// claim other elements as children.
fn classify_child_relationship(
    parent: &ComponentSourceProfile,
    child: &ComponentSourceProfile,
) -> (ChildRelationship, Option<String>) {
    let parent_block = match &parent.bem_block {
        Some(b) => b.as_str(),
        None => {
            if parent.rendered_components.contains(&child.name) {
                return (ChildRelationship::Internal, None);
            }
            return (ChildRelationship::Unknown, None);
        }
    };

    // Only allow BEM element classification if the parent is the block
    // OWNER — its component name matches (or is a prefix of) the block name.
    //
    // In PF (and BEM generally), the root component IS named after the
    // block: Menu → "menu", Masthead → "masthead", Modal → "modalBox".
    // Element components have suffixed names: MenuItem, MastheadBrand.
    //
    // We check that the block name starts with the component name
    // (case-insensitive), so "modal" matches "modalBox" but "menuItem"
    // does not match "menu".
    let parent_name_lower = parent.name.to_lowercase();
    let block_lower = parent_block.to_lowercase();
    let parent_is_block_owner = block_lower.starts_with(&parent_name_lower);

    if !parent_is_block_owner {
        // Parent is itself a BEM element — it cannot claim children
        if parent.rendered_components.contains(&child.name) {
            return (ChildRelationship::Internal, None);
        }
        return (ChildRelationship::Unknown, None);
    }

    // Get child's raw tokens for BEM analysis
    let child_raw_tokens = &child
        .css_tokens_used
        .iter()
        .filter(|t| t.starts_with("styles.") && !t.contains("modifiers"))
        .map(|t| t.strip_prefix("styles.").unwrap_or(t).to_string())
        .collect();

    let bem_rel =
        classify_bem_relationship(child.bem_block.as_deref(), child_raw_tokens, parent_block);

    match bem_rel {
        BemRelationship::Element { element_name } => {
            let evidence = format!(
                "{} is BEM element '{}' of {} block",
                child.name, element_name, parent_block
            );
            (ChildRelationship::BemElement, Some(evidence))
        }
        BemRelationship::Independent { block_name } => {
            let evidence = format!("{} has independent BEM block '{}'", child.name, block_name);
            (ChildRelationship::IndependentBlock, Some(evidence))
        }
        BemRelationship::Unknown => {
            if parent.rendered_components.contains(&child.name) {
                (ChildRelationship::Internal, None)
            } else {
                (ChildRelationship::Unknown, None)
            }
        }
    }
}

/// Check if a component is transitively internal (rendered by a component
/// that is itself rendered internally by the parent).
/// Check if a child component is passed via a ReactNode/ComponentType prop
/// on the parent rather than placed as a JSX child.
///
/// Matches by stripping the parent name prefix from the child name and
/// checking if any ReactNode prop on the parent starts with the remainder.
///
/// Example: parent="Alert", child="AlertActionCloseButton"
///   → suffix = "ActionCloseButton" → lowercase = "actionclosebutton"
///   → Alert has prop "actionClose: React.ReactNode"
///   → "actionclosebutton" starts with "actionclose" → match → prop-passed
fn is_prop_passed_component(
    parent_profile: &ComponentSourceProfile,
    child_name: &str,
    parent_name: &str,
) -> bool {
    // Strip parent name prefix to get the child's role suffix
    let suffix = if let Some(stripped) = child_name.strip_prefix(parent_name) {
        stripped
    } else {
        return false;
    };

    if suffix.is_empty() {
        return false;
    }

    let suffix_lower = suffix.to_lowercase();

    // Check if any ReactNode/ComponentType prop matches
    for (prop_name, prop_type) in &parent_profile.prop_types {
        if prop_name == "children" || prop_name == "className" {
            continue;
        }
        if !is_react_renderable_type(prop_type) {
            continue;
        }
        let prop_lower = prop_name.to_lowercase();
        // Check bidirectionally: "actionclosebutton" starts with "actionclose"
        // OR "actionlinks" starts with "actionlink"
        if suffix_lower.starts_with(&prop_lower) || prop_lower.starts_with(&suffix_lower) {
            return true;
        }
    }

    false
}

/// Check if a type string represents a React renderable type
/// (ReactNode, ReactElement, ComponentType, JSX.Element).
fn is_react_renderable_type(type_str: &str) -> bool {
    let t = type_str.trim();
    t.contains("ReactNode")
        || t.contains("ReactElement")
        || t.contains("ComponentType")
        || t.contains("JSX.Element")
}

fn is_transitively_internal(
    target: &str,
    parent: &str,
    profiles: &HashMap<String, ComponentSourceProfile>,
    family_set: &HashSet<&str>,
) -> bool {
    let Some(parent_profile) = profiles.get(parent) else {
        return false;
    };

    for internal_comp in &parent_profile.rendered_components {
        if !family_set.contains(internal_comp.as_str()) {
            continue;
        }
        if let Some(internal_profile) = profiles.get(internal_comp.as_str()) {
            if internal_profile
                .rendered_components
                .iter()
                .any(|c| c == target)
            {
                return true;
            }
        }
    }

    false
}

/// Infer block ownership when the root component's BEM block differs
/// from its children's block but the root name is a prefix of the
/// children's block name.
///
/// Example: Modal (block "backdrop") with children ModalHeader, ModalBody,
/// ModalFooter (block "modalBox"). "modal" is a prefix of "modalBox", so
/// Modal is the owner of the modalBox block for composition purposes.
fn infer_ownership_by_name_prefix(
    tree: &mut CompositionTree,
    profiles: &HashMap<String, ComponentSourceProfile>,
    family_exports: &[String],
    root: &str,
) {
    let _root_profile = match profiles.get(root) {
        Some(p) => p,
        None => return,
    };

    let root_name_lower = root.to_lowercase();

    // Check if root already has BEM edges — if so, skip
    let has_bem_edges = tree.edges.iter().any(|e| {
        e.parent == root
            && matches!(
                e.relationship,
                ChildRelationship::DirectChild | ChildRelationship::BemElement
            )
    });
    if has_bem_edges {
        return;
    }

    // Find the most common BEM block among non-root family members
    let mut block_counts: HashMap<&str, usize> = HashMap::new();
    for name in family_exports {
        if name == root {
            continue;
        }
        if let Some(profile) = profiles.get(name) {
            if let Some(ref block) = profile.bem_block {
                *block_counts.entry(block.as_str()).or_default() += 1;
            }
        }
    }

    let dominant_block = block_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(block, _)| block);

    let Some(child_block) = dominant_block else {
        return;
    };

    // Check if root name is a prefix of the children's block
    let child_block_lower = child_block.to_lowercase();
    if !child_block_lower.starts_with(&root_name_lower) {
        return;
    }

    // Reject if the block boundary is at a hyphen — this indicates a
    // separate BEM block, not a sub-element of the root's block.
    // e.g., root "label" with child block "label-group":
    //   remainder = "-group" → starts with '-' → separate block → reject
    // vs. root "modal" with child block "modalBox":
    //   remainder = "box" → no hyphen → camelCase element → allow
    let remainder = &child_block_lower[root_name_lower.len()..];
    if remainder.starts_with('-') {
        debug!(
            root = %root,
            child_block = %child_block,
            "Rejecting name-prefix ownership — hyphen boundary indicates separate BEM block"
        );
        return;
    }

    // Root owns this block — add edges for all children that are
    // BEM elements of the children's block
    let existing: HashSet<(String, String)> = tree
        .edges
        .iter()
        .map(|e| (e.parent.clone(), e.child.clone()))
        .collect();

    for child_name in family_exports {
        if child_name == root {
            continue;
        }
        if existing.contains(&(root.to_string(), child_name.clone())) {
            continue;
        }
        let Some(child_profile) = profiles.get(child_name) else {
            continue;
        };

        // Check if child has tokens that are elements of the children's block
        let is_element = child_profile.css_tokens_used.iter().any(|t| {
            if let Some(token) = t.strip_prefix("styles.") {
                token.starts_with(child_block)
                    && token.len() > child_block.len()
                    && token[child_block.len()..].starts_with(|c: char| c.is_uppercase())
            } else {
                false
            }
        });

        if is_element {
            tree.edges.push(CompositionEdge {
                parent: root.to_string(),
                child: child_name.clone(),
                relationship: ChildRelationship::DirectChild,
                required: false,
                bem_evidence: Some(format!(
                    "{} name is prefix of {} block, {} uses {} tokens",
                    root, child_block, child_name, child_block
                )),
            });
        }
    }
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
    fn test_build_dropdown_tree_no_bem_edges() {
        // Dropdown family is a thin wrapper over Menu — no BEM tokens,
        // no internal rendering of family members. The builder alone
        // produces an empty tree; edges are filled in by
        // `project_delegate_trees` at the pipeline level.
        let mut dropdown = make_profile("Dropdown");
        dropdown.has_children_prop = true;
        dropdown.rendered_components = vec!["Menu".into(), "MenuContent".into(), "Popper".into()];

        let mut dropdown_list = make_profile("DropdownList");
        dropdown_list.has_children_prop = true;
        dropdown_list.rendered_components = vec!["MenuList".into()];

        let mut dropdown_item = make_profile("DropdownItem");
        dropdown_item.has_children_prop = true;
        dropdown_item.rendered_components = vec!["MenuItem".into()];

        let mut dropdown_group = make_profile("DropdownGroup");
        dropdown_group.has_children_prop = true;
        dropdown_group.rendered_components = vec!["MenuGroup".into()];

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

        let tree = build_composition_tree(&profiles, &family).unwrap();

        assert_eq!(tree.root, "Dropdown");
        assert_eq!(tree.family_members.len(), 4);
        // No BEM evidence → no edges from the builder alone
        // (delegation projection fills these in at the pipeline level)
        assert!(tree.edges.is_empty());
    }

    #[test]
    fn test_build_modal_tree() {
        // Simulate the v6 Modal family
        let mut modal = make_profile("Modal");
        modal.has_children_prop = true;
        modal.rendered_components = vec!["ModalContent".into()];
        modal.uses_portal = true;
        // Modal has no styles.* tokens

        let mut modal_header = make_profile("ModalHeader");
        modal_header.has_children_prop = true;
        modal_header
            .css_tokens_used
            .insert("styles.modalBoxHeader".into());
        modal_header
            .css_tokens_used
            .insert("styles.modalBoxHeaderMain".into());
        modal_header.bem_block = Some("modalBox".into());
        modal_header.bem_elements.insert("header".into());

        let mut modal_body = make_profile("ModalBody");
        modal_body.has_children_prop = true;
        modal_body
            .css_tokens_used
            .insert("styles.modalBoxBody".into());
        modal_body.bem_block = Some("modalBox".into());
        modal_body.bem_elements.insert("body".into());

        let mut modal_footer = make_profile("ModalFooter");
        modal_footer.has_children_prop = true;
        modal_footer
            .css_tokens_used
            .insert("styles.modalBoxFooter".into());
        modal_footer.bem_block = Some("modalBox".into());
        modal_footer.bem_elements.insert("footer".into());

        let mut profiles = HashMap::new();
        profiles.insert("Modal".into(), modal);
        profiles.insert("ModalHeader".into(), modal_header);
        profiles.insert("ModalBody".into(), modal_body);
        profiles.insert("ModalFooter".into(), modal_footer);

        let family = vec![
            "Modal".into(),
            "ModalHeader".into(),
            "ModalBody".into(),
            "ModalFooter".into(),
        ];

        let tree = build_composition_tree(&profiles, &family).unwrap();

        assert_eq!(tree.root, "Modal");
        assert_eq!(tree.family_members.len(), 4);
        // Modal doesn't render ModalHeader/Body/Footer internally,
        // they should appear as consumer-provided children
    }

    #[test]
    fn test_only_block_owner_parents_bem_elements() {
        // Only the block owner (component name == block name) should
        // create BEM element edges. Sub-components like MastheadBrand
        // share the same import-derived block but are NOT owners.
        let mut masthead = make_profile("Masthead");
        masthead.has_children_prop = true;
        masthead.bem_block = Some("masthead".into());
        masthead.css_tokens_used.insert("styles.masthead".into());

        let mut brand = make_profile("MastheadBrand");
        brand.has_children_prop = true;
        brand.bem_block = Some("masthead".into()); // same import-derived block
        brand.css_tokens_used.insert("styles.mastheadBrand".into());

        let mut content = make_profile("MastheadContent");
        content.has_children_prop = true;
        content.bem_block = Some("masthead".into());
        content
            .css_tokens_used
            .insert("styles.mastheadContent".into());

        let mut profiles = HashMap::new();
        profiles.insert("Masthead".into(), masthead);
        profiles.insert("MastheadBrand".into(), brand);
        profiles.insert("MastheadContent".into(), content);

        let family = vec![
            "Masthead".into(),
            "MastheadBrand".into(),
            "MastheadContent".into(),
        ];

        let tree = build_composition_tree(&profiles, &family).unwrap();

        // Masthead → MastheadBrand (BEM element "brand" of block "masthead")
        // BEM edges are not required — BEM proves structural membership only.
        assert!(tree
            .edges
            .iter()
            .any(|e| e.parent == "Masthead" && e.child == "MastheadBrand" && !e.required));
        // Masthead → MastheadContent
        assert!(tree
            .edges
            .iter()
            .any(|e| e.parent == "Masthead" && e.child == "MastheadContent" && !e.required));

        // MastheadBrand should NOT parent MastheadContent (Brand is not the block owner)
        assert!(
            !tree
                .edges
                .iter()
                .any(|e| e.parent == "MastheadBrand" && e.child == "MastheadContent"),
            "Non-owner MastheadBrand should not claim MastheadContent as child"
        );
        // MastheadContent should NOT parent MastheadBrand
        assert!(
            !tree
                .edges
                .iter()
                .any(|e| e.parent == "MastheadContent" && e.child == "MastheadBrand"),
            "Non-owner MastheadContent should not claim MastheadBrand as child"
        );
    }

    #[test]
    fn test_element_with_block_token_is_not_owner() {
        // MenuItem references styles.menu for flyout detection
        // (classList.contains), but it's NOT the block owner — Menu is.
        // The block owner check uses component name == block name.
        let mut menu = make_profile("Menu");
        menu.has_children_prop = true;
        menu.bem_block = Some("menu".into());
        menu.css_tokens_used.insert("styles.menu".into());

        let mut menu_item = make_profile("MenuItem");
        menu_item.has_children_prop = true;
        menu_item.bem_block = Some("menu".into());
        menu_item.css_tokens_used.insert("styles.menuItem".into());
        menu_item
            .css_tokens_used
            .insert("styles.menuListItem".into());
        // MenuItem also has styles.menu (for flyout classList check)
        menu_item.css_tokens_used.insert("styles.menu".into());

        let mut menu_list = make_profile("MenuList");
        menu_list.has_children_prop = true;
        menu_list.bem_block = Some("menu".into());
        menu_list.css_tokens_used.insert("styles.menuList".into());

        let mut profiles = HashMap::new();
        profiles.insert("Menu".into(), menu);
        profiles.insert("MenuItem".into(), menu_item);
        profiles.insert("MenuList".into(), menu_list);

        let family = vec!["Menu".into(), "MenuItem".into(), "MenuList".into()];

        let tree = build_composition_tree(&profiles, &family).unwrap();

        // Menu → MenuItem (Menu is the block owner, name matches block)
        assert!(tree
            .edges
            .iter()
            .any(|e| e.parent == "Menu" && e.child == "MenuItem"));

        // Menu → MenuList
        assert!(tree
            .edges
            .iter()
            .any(|e| e.parent == "Menu" && e.child == "MenuList"));

        // MenuItem should NOT parent MenuList or Menu — it has styles.menu
        // in its tokens but its name "MenuItem" != block "menu"
        assert!(
            !tree
                .edges
                .iter()
                .any(|e| e.parent == "MenuItem" && e.child == "MenuList"),
            "MenuItem should not claim MenuList — it's not the block owner"
        );
        assert!(
            !tree
                .edges
                .iter()
                .any(|e| e.parent == "MenuItem" && e.child == "Menu"),
            "MenuItem should not claim Menu — it's not the block owner"
        );
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

        let tree = build_composition_tree(&profiles, &family).unwrap();

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

        let tree = build_composition_tree(&profiles, &family).unwrap();

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
    fn test_build_menu_tree_bem_elements() {
        // Menu has block "menu" and IS the block owner (has styles.menu).
        // MenuList has import-derived block "menu" but only has element
        // token styles.menuList → NOT the block owner.
        // Menu renders {children}, not MenuList directly, so the edge
        // comes from consumer-provided BEM element evidence.
        let mut menu = make_profile("Menu");
        menu.has_children_prop = true;
        menu.bem_block = Some("menu".into());
        menu.css_tokens_used.insert("styles.menu".into());
        menu.css_tokens_used.insert("styles.divider".into());

        let mut menu_list = make_profile("MenuList");
        menu_list.has_children_prop = true;
        menu_list.bem_block = Some("menu".into()); // import-derived, not "menuList"
        menu_list.css_tokens_used.insert("styles.menuList".into());

        let mut menu_item = make_profile("MenuItem");
        menu_item.has_children_prop = true;
        menu_item.bem_block = Some("menu".into()); // import-derived
        menu_item
            .css_tokens_used
            .insert("styles.menuListItem".into());
        menu_item
            .css_tokens_used
            .insert("styles.menuItemMain".into());

        let mut profiles = HashMap::new();
        profiles.insert("Menu".into(), menu);
        profiles.insert("MenuList".into(), menu_list);
        profiles.insert("MenuItem".into(), menu_item);

        let family = vec!["Menu".into(), "MenuList".into(), "MenuItem".into()];

        let tree = build_composition_tree(&profiles, &family).unwrap();

        // Menu → MenuList (BEM element "list" of block "menu")
        let menu_to_list = tree
            .edges
            .iter()
            .find(|e| e.parent == "Menu" && e.child == "MenuList");
        assert!(
            menu_to_list.is_some(),
            "Expected Menu → MenuList edge, got edges: {:?}",
            tree.edges
        );
        // BEM edges are no longer required — BEM proves structural membership
        // but not that the child is required for the parent to function.
        assert!(!menu_to_list.unwrap().required);

        // Menu → MenuItem (also a BEM element of "menu" block)
        // BEM only tells us both are children of Menu, not that
        // MenuItem goes inside MenuList specifically.
        let menu_to_item = tree
            .edges
            .iter()
            .find(|e| e.parent == "Menu" && e.child == "MenuItem");
        assert!(
            menu_to_item.is_some(),
            "Expected Menu → MenuItem edge, got edges: {:?}",
            tree.edges
        );

        // MenuList should NOT parent MenuItem — MenuList is not the
        // block owner (it only has styles.menuList, not styles.menu)
        assert!(
            !tree
                .edges
                .iter()
                .any(|e| e.parent == "MenuList" && e.child == "MenuItem"),
            "MenuList is not the block owner and should not claim MenuItem"
        );
    }

    #[test]
    fn test_is_prop_passed_component() {
        // Alert has actionClose: React.ReactNode
        let mut alert = make_profile("Alert");
        alert
            .prop_types
            .insert("actionClose".into(), "React.ReactNode".into());
        alert
            .prop_types
            .insert("actionLinks".into(), "React.ReactNode".into());
        alert
            .prop_types
            .insert("children".into(), "React.ReactNode".into());
        alert
            .prop_types
            .insert("variant".into(), "'success' | 'danger'".into());

        // AlertActionCloseButton → strip "Alert" → "ActionCloseButton"
        // → lowercase "actionclosebutton" starts with "actionclose" → match
        assert!(
            is_prop_passed_component(&alert, "AlertActionCloseButton", "Alert"),
            "AlertActionCloseButton should be detected as prop-passed via actionClose"
        );

        // AlertActionLink → strip "Alert" → "ActionLink"
        // → lowercase "actionlink", prop "actionLinks" → lowercase "actionlinks"
        // → "actionlinks".starts_with("actionlink") → match (bidirectional check)
        assert!(
            is_prop_passed_component(&alert, "AlertActionLink", "Alert"),
            "AlertActionLink should be detected as prop-passed via actionLinks"
        );

        // Non-matching: AlertGroup has no prop suffix match
        assert!(
            !is_prop_passed_component(&alert, "AlertGroup", "Alert"),
            "AlertGroup should not be detected as prop-passed"
        );

        // Non-family child (doesn't start with parent name)
        assert!(
            !is_prop_passed_component(&alert, "Button", "Alert"),
            "Button should not match (no parent prefix)"
        );
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
                },
                CompositionEdge {
                    parent: "Accordion".into(),
                    child: "AccordionToggle".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: Some("BEM element of accordion block".into()),
                },
                // Correct root edge (no intermediate for AccordionItem)
                CompositionEdge {
                    parent: "Accordion".into(),
                    child: "AccordionItem".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: Some("BEM element of accordion block".into()),
                },
                // Context-derived intermediate edges (should be kept)
                CompositionEdge {
                    parent: "AccordionItem".into(),
                    child: "AccordionContent".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: Some("Context nesting".into()),
                },
                CompositionEdge {
                    parent: "AccordionItem".into(),
                    child: "AccordionToggle".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: false,
                    bem_evidence: Some("Context nesting".into()),
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
                },
                CompositionEdge {
                    parent: "Masthead".into(),
                    child: "MastheadContent".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: None,
                },
                CompositionEdge {
                    parent: "Masthead".into(),
                    child: "MastheadMain".into(),
                    relationship: ChildRelationship::DirectChild,
                    required: true,
                    bem_evidence: None,
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

    /// Build a full composition tree where BEM creates root→child edges
    /// but context nesting proves an intermediate parent. Verify that
    /// suppress_root_edges_with_intermediate removes the root edge.
    ///
    /// Simulates: Accordion → AccordionContent (BEM) should be suppressed
    /// because AccordionItem → AccordionContent (context) exists.
    #[test]
    fn test_full_tree_build_suppresses_root_bem_when_context_intermediate() {
        // Accordion: block owner, has children prop
        let mut accordion = make_profile("Accordion");
        accordion.has_children_prop = true;
        accordion.bem_block = Some("accordion".into());
        accordion.css_tokens_used.insert("styles.accordion".into());

        // AccordionItem: intermediate wrapper, provides AccordionItemContext
        let mut item = make_profile("AccordionItem");
        item.has_children_prop = true;
        item.bem_block = Some("accordion".into());
        item.css_tokens_used.insert("styles.accordionItem".into());
        // Context providers are detected from rendered_components entries
        item.rendered_components
            .push("AccordionItemContext.Provider".into());

        // AccordionContent: consumes AccordionItemContext, uses accordion BEM tokens
        let mut content = make_profile("AccordionContent");
        content.has_children_prop = true;
        content.bem_block = Some("accordion".into());
        content
            .css_tokens_used
            .insert("styles.accordionExpandableContent".into());
        content
            .consumed_contexts
            .push("AccordionItemContext".into());

        // AccordionToggle: also consumes AccordionItemContext
        let mut toggle = make_profile("AccordionToggle");
        toggle.has_children_prop = true;
        toggle.bem_block = Some("accordion".into());
        toggle
            .css_tokens_used
            .insert("styles.accordionToggle".into());
        toggle.consumed_contexts.push("AccordionItemContext".into());

        let mut profiles = HashMap::new();
        profiles.insert("Accordion".into(), accordion);
        profiles.insert("AccordionItem".into(), item);
        profiles.insert("AccordionContent".into(), content);
        profiles.insert("AccordionToggle".into(), toggle);

        let family = vec![
            "Accordion".into(),
            "AccordionItem".into(),
            "AccordionContent".into(),
            "AccordionToggle".into(),
        ];

        let tree = build_composition_tree(&profiles, &family).unwrap();

        // AccordionItem should be a direct child of Accordion (no intermediate)
        assert!(
            tree.edges
                .iter()
                .any(|e| e.parent == "Accordion" && e.child == "AccordionItem"),
            "Accordion → AccordionItem should exist"
        );

        // AccordionContent should be under AccordionItem (context nesting),
        // NOT under Accordion (BEM was suppressed)
        assert!(
            tree.edges
                .iter()
                .any(|e| e.parent == "AccordionItem" && e.child == "AccordionContent"),
            "AccordionItem → AccordionContent should exist (context nesting)"
        );
        assert!(
            !tree
                .edges
                .iter()
                .any(|e| e.parent == "Accordion" && e.child == "AccordionContent"),
            "Accordion → AccordionContent should be suppressed (intermediate exists)"
        );

        // Same for AccordionToggle
        assert!(
            tree.edges
                .iter()
                .any(|e| e.parent == "AccordionItem" && e.child == "AccordionToggle"),
            "AccordionItem → AccordionToggle should exist (context nesting)"
        );
        assert!(
            !tree
                .edges
                .iter()
                .any(|e| e.parent == "Accordion" && e.child == "AccordionToggle"),
            "Accordion → AccordionToggle should be suppressed (intermediate exists)"
        );
    }

    /// Build a tree where a component is passed via a ReactNode prop
    /// on the parent. Verify it does NOT get a BEM edge.
    ///
    /// Simulates: Alert has actionClose: ReactNode prop.
    /// AlertActionCloseButton uses alert BEM tokens but should NOT
    /// be a child of Alert — it's prop-passed.
    #[test]
    fn test_full_tree_build_skips_prop_passed_bem_component() {
        let mut alert = make_profile("Alert");
        alert.has_children_prop = true;
        alert.bem_block = Some("alert".into());
        alert.css_tokens_used.insert("styles.alert".into());
        alert
            .prop_types
            .insert("actionClose".into(), "React.ReactNode".into());
        alert
            .prop_types
            .insert("children".into(), "React.ReactNode".into());

        // AlertActionCloseButton uses alert BEM tokens
        let mut close_btn = make_profile("AlertActionCloseButton");
        close_btn.bem_block = Some("alert".into());
        close_btn
            .css_tokens_used
            .insert("styles.alertActionClose".into());

        // AlertBody — a regular child (not prop-passed)
        let mut body = make_profile("AlertBody");
        body.bem_block = Some("alert".into());
        body.css_tokens_used.insert("styles.alertBody".into());

        let mut profiles = HashMap::new();
        profiles.insert("Alert".into(), alert);
        profiles.insert("AlertActionCloseButton".into(), close_btn);
        profiles.insert("AlertBody".into(), body);

        let family = vec![
            "Alert".into(),
            "AlertActionCloseButton".into(),
            "AlertBody".into(),
        ];

        let tree = build_composition_tree(&profiles, &family).unwrap();

        // AlertActionCloseButton should NOT be a child of Alert (prop-passed)
        assert!(
            !tree
                .edges
                .iter()
                .any(|e| e.parent == "Alert" && e.child == "AlertActionCloseButton"),
            "AlertActionCloseButton should be skipped — it's prop-passed via actionClose"
        );

        // AlertBody SHOULD be a child of Alert (not prop-passed)
        assert!(
            tree.edges
                .iter()
                .any(|e| e.parent == "Alert" && e.child == "AlertBody"),
            "AlertBody should be a child of Alert (regular BEM element)"
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

        let tree = build_composition_tree(&profiles, &family).unwrap();

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
        // LabelGroup has BEM block "label-group" — a SEPARATE block from
        // Label's "label" block. The hyphen boundary means Label should NOT
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
        label_group.bem_block = Some("label-group".into());
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

        let tree = build_composition_tree(&profiles, &family).unwrap();

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
        alert_group.bem_block = Some("alert-group".into());
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

        let tree = build_composition_tree(&profiles, &family).unwrap();

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
    fn test_modal_modalbox_ownership_allowed() {
        // Modal owns ModalBox because "modalBox" has no hyphen at the
        // boundary — it's a camelCase sub-block, not a separate BEM block.
        let mut modal = make_profile("Modal");
        modal.has_children_prop = true;
        modal.bem_block = Some("backdrop".into()); // Modal's own block is different
        modal.css_tokens_used = ["styles.backdrop".to_string()].into_iter().collect();

        let mut modal_box = make_profile("ModalBox");
        modal_box.has_children_prop = true;
        modal_box.bem_block = Some("modalBox".into());
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

        let tree = build_composition_tree(&profiles, &family).unwrap();

        // Modal should own ModalBox (no hyphen at boundary: "modalBox")
        // ModalBox and ModalBoxBody are camelCase elements
        let modal_owns_box = tree
            .edges
            .iter()
            .any(|e| e.parent == "Modal" && e.child == "ModalBox");
        assert!(
            modal_owns_box,
            "Modal should own ModalBox — 'modalBox' is a camelCase sub-block \
             (no hyphen boundary). Edges: {:?}",
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
        menu_toggle.bem_block = Some("menu-toggle".into());
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

        let tree = build_composition_tree(&profiles, &family).unwrap();

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
