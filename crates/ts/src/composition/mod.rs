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
use crate::sd_types::{
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
                    tree.edges.push(CompositionEdge {
                        parent: parent_name.clone(),
                        child: sibling_name.clone(),
                        relationship: ChildRelationship::DirectChild,
                        required: true,
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
    let root_profile = match profiles.get(root) {
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
    if !child_block.to_lowercase().starts_with(&root_name_lower) {
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

/// Infer the root HTML element from a component's rendered_elements.
///
/// Heuristic: if the component only renders one type of block-level
/// element, that's likely the root. For components like MenuItem that
/// render `<li>` as the wrapper, this picks up `li`.
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
    None
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
        assert!(tree
            .edges
            .iter()
            .any(|e| e.parent == "Masthead" && e.child == "MastheadBrand" && e.required));
        // Masthead → MastheadContent
        assert!(tree
            .edges
            .iter()
            .any(|e| e.parent == "Masthead" && e.child == "MastheadContent" && e.required));

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
        assert!(menu_to_list.unwrap().required);

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
}
