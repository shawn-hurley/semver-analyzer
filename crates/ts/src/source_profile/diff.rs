//! Diff two `ComponentSourceProfile`s to produce `SourceLevelChange` entries.
//!
//! Each change is deterministic — a fact derived from comparing two AST-extracted
//! profiles. No confidence scores, no LLM involvement.

use crate::sd_types::{ComponentSourceProfile, SourceLevelCategory, SourceLevelChange};

/// Diff two component profiles and produce a list of source-level changes.
///
/// `old` is the profile from the previous version, `new` is the current version.
/// Both should be for the same component (same `name`).
pub fn diff_profiles(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
) -> Vec<SourceLevelChange> {
    let mut changes = Vec::new();
    let component = &new.name;

    diff_portal_usage(old, new, component, &mut changes);
    diff_context_dependencies(old, new, component, &mut changes);
    diff_context_providers(old, new, component, &mut changes);
    diff_forward_ref(old, new, component, &mut changes);
    diff_memo(old, new, component, &mut changes);
    diff_prop_defaults(old, new, component, &mut changes);
    diff_rendered_components(old, new, component, &mut changes);
    diff_dom_structure(old, new, component, &mut changes);
    diff_aria_attributes(old, new, component, &mut changes);
    diff_role_attributes(old, new, component, &mut changes);
    diff_data_attributes(old, new, component, &mut changes);
    diff_css_tokens(old, new, component, &mut changes);
    diff_children_slot(old, new, component, &mut changes);

    changes
}

// ── Portal usage ────────────────────────────────────────────────────────

fn diff_portal_usage(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    if old.uses_portal != new.uses_portal {
        let (desc, test_desc) = if new.uses_portal {
            (
                format!(
                    "{component} now uses createPortal — content renders outside the component's DOM subtree"
                ),
                Some(format!(
                    "screen.getByText() and similar queries cannot find content rendered via portal. \
                     Use within(document.body).getByText() or configure baseElement in render options."
                )),
            )
        } else {
            (
                format!(
                    "{component} no longer uses createPortal — content renders inline in the component tree"
                ),
                Some(format!(
                    "Content now renders inside the component tree. \
                     Remove any within(document.body) workarounds if they were used."
                )),
            )
        };

        changes.push(SourceLevelChange {
            component: component.to_string(),
            category: SourceLevelCategory::PortalUsage,
            description: desc,
            old_value: Some(format!("uses_portal: {}", old.uses_portal)),
            new_value: Some(format!("uses_portal: {}", new.uses_portal)),
            has_test_implications: true,
            test_description: test_desc,
        });
    }
}

// ── Context dependencies ────────────────────────────────────────────────

fn diff_context_dependencies(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    // Contexts added
    for ctx in &new.consumed_contexts {
        if !old.consumed_contexts.contains(ctx) {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::ContextDependency,
                description: format!(
                    "{component} now requires {ctx} context provider. \
                     Rendering without this provider may cause runtime errors or incorrect behavior."
                ),
                old_value: None,
                new_value: Some(format!("useContext({ctx})")),
                has_test_implications: false,
                test_description: None,
            });
        }
    }

    // Contexts removed
    for ctx in &old.consumed_contexts {
        if !new.consumed_contexts.contains(ctx) {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::ContextDependency,
                description: format!(
                    "{component} no longer consumes {ctx} context. \
                     It will no longer respond to changes from this provider."
                ),
                old_value: Some(format!("useContext({ctx})")),
                new_value: None,
                has_test_implications: false,
                test_description: None,
            });
        }
    }
}

// ── Context providers ───────────────────────────────────────────────────

fn diff_context_providers(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    // Provider added
    for ctx in &new.provided_contexts {
        if !old.provided_contexts.contains(ctx) {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::ContextDependency,
                description: format!(
                    "{component} now provides {ctx} context to its children. \
                     Child components may now depend on this provider being present."
                ),
                old_value: None,
                new_value: Some(format!("<{ctx}.Provider>")),
                has_test_implications: true,
                test_description: Some(format!(
                    "Tests rendering children of {component} may need to account for \
                     the new {ctx} context provider."
                )),
            });
        }
    }

    // Provider removed — breaking for children that consumed it
    for ctx in &old.provided_contexts {
        if !new.provided_contexts.contains(ctx) {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::ContextDependency,
                description: format!(
                    "{component} no longer provides {ctx} context. \
                     Child components that use useContext({ctx}) will receive the \
                     default value instead, which may cause runtime errors."
                ),
                old_value: Some(format!("<{ctx}.Provider>")),
                new_value: None,
                has_test_implications: true,
                test_description: Some(format!(
                    "Tests for child components of {component} that depend on {ctx} \
                     context will need to provide their own context wrapper."
                )),
            });
        }
    }
}

// ── forwardRef ──────────────────────────────────────────────────────────

fn diff_forward_ref(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    if old.is_forward_ref != new.is_forward_ref {
        let desc = if new.is_forward_ref {
            format!("{component} now forwards refs via forwardRef. Consumers can attach refs to the underlying DOM element.")
        } else {
            format!("{component} no longer forwards refs. Existing ref usage will stop working.")
        };

        changes.push(SourceLevelChange {
            component: component.to_string(),
            category: SourceLevelCategory::ForwardRef,
            description: desc,
            old_value: Some(format!("is_forward_ref: {}", old.is_forward_ref)),
            new_value: Some(format!("is_forward_ref: {}", new.is_forward_ref)),
            has_test_implications: false,
            test_description: None,
        });
    }
}

// ── memo ────────────────────────────────────────────────────────────────

fn diff_memo(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    if old.is_memo != new.is_memo {
        let desc = if new.is_memo {
            format!("{component} is now wrapped in React.memo. It will skip re-renders when props are shallow-equal.")
        } else {
            format!("{component} is no longer wrapped in React.memo. It will re-render on every parent render.")
        };

        changes.push(SourceLevelChange {
            component: component.to_string(),
            category: SourceLevelCategory::Memo,
            description: desc,
            old_value: Some(format!("is_memo: {}", old.is_memo)),
            new_value: Some(format!("is_memo: {}", new.is_memo)),
            has_test_implications: false,
            test_description: None,
        });
    }
}

// ── Prop defaults ───────────────────────────────────────────────────────

fn diff_prop_defaults(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    // Check for changed defaults
    for (prop, new_val) in &new.prop_defaults {
        match old.prop_defaults.get(prop) {
            Some(old_val) if old_val != new_val => {
                changes.push(SourceLevelChange {
                    component: component.to_string(),
                    category: SourceLevelCategory::PropDefault,
                    description: format!(
                        "Default value for '{prop}' prop on {component} changed from {old_val} to {new_val}"
                    ),
                    old_value: Some(old_val.clone()),
                    new_value: Some(new_val.clone()),
                    has_test_implications: false,
                    test_description: None,
                });
            }
            None => {
                // New default added (prop existed but had no default, or prop is new)
                changes.push(SourceLevelChange {
                    component: component.to_string(),
                    category: SourceLevelCategory::PropDefault,
                    description: format!(
                        "Prop '{prop}' on {component} now has default value {new_val}"
                    ),
                    old_value: None,
                    new_value: Some(new_val.clone()),
                    has_test_implications: false,
                    test_description: None,
                });
            }
            _ => {} // Same value, no change
        }
    }

    // Check for removed defaults
    for (prop, old_val) in &old.prop_defaults {
        if !new.prop_defaults.contains_key(prop) {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::PropDefault,
                description: format!(
                    "Default value for '{prop}' prop on {component} removed (was {old_val})"
                ),
                old_value: Some(old_val.clone()),
                new_value: None,
                has_test_implications: false,
                test_description: None,
            });
        }
    }
}

// ── Rendered components ─────────────────────────────────────────────────

fn diff_rendered_components(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    for comp in &new.rendered_components {
        if !old.rendered_components.contains(comp) {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::RenderedComponent,
                description: format!("{component} now internally renders {comp}"),
                old_value: None,
                new_value: Some(comp.clone()),
                has_test_implications: false,
                test_description: None,
            });
        }
    }

    for comp in &old.rendered_components {
        if !new.rendered_components.contains(comp) {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::RenderedComponent,
                description: format!("{component} no longer internally renders {comp}"),
                old_value: Some(comp.clone()),
                new_value: None,
                has_test_implications: false,
                test_description: None,
            });
        }
    }
}

// ── DOM structure ───────────────────────────────────────────────────────

fn diff_dom_structure(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    // Elements added
    for (elem, count) in &new.rendered_elements {
        if !old.rendered_elements.contains_key(elem) {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::DomStructure,
                description: format!("{component} now renders <{elem}> element"),
                old_value: None,
                new_value: Some(format!("<{elem}> (×{count})")),
                has_test_implications: true,
                test_description: Some(format!(
                    "New <{elem}> element may affect snapshot tests and DOM query selectors"
                )),
            });
        }
    }

    // Elements removed
    for (elem, _count) in &old.rendered_elements {
        if !new.rendered_elements.contains_key(elem) {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::DomStructure,
                description: format!("{component} no longer renders <{elem}> element"),
                old_value: Some(format!("<{elem}>")),
                new_value: None,
                has_test_implications: true,
                test_description: Some(format!(
                    "Removed <{elem}> element will break queries using this element type"
                )),
            });
        }
    }
}

// ── ARIA attributes ─────────────────────────────────────────────────────

fn diff_aria_attributes(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    // Added
    for ((elem, attr), val) in &new.aria_attributes {
        if !old
            .aria_attributes
            .contains_key(&(elem.clone(), attr.clone()))
        {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::AriaChange,
                description: format!("{attr} attribute added to <{elem}> in {component}"),
                old_value: None,
                new_value: Some(val.clone()),
                has_test_implications: true,
                test_description: Some(format!(
                    "New {attr} on <{elem}> may affect getByRole/getByLabelText queries"
                )),
            });
        } else if let Some(old_val) = old.aria_attributes.get(&(elem.clone(), attr.clone())) {
            if old_val != val {
                changes.push(SourceLevelChange {
                    component: component.to_string(),
                    category: SourceLevelCategory::AriaChange,
                    description: format!(
                        "{attr} on <{elem}> in {component} changed from '{old_val}' to '{val}'"
                    ),
                    old_value: Some(old_val.clone()),
                    new_value: Some(val.clone()),
                    has_test_implications: true,
                    test_description: Some(format!(
                        "Changed {attr} value will affect accessibility queries"
                    )),
                });
            }
        }
    }

    // Removed
    for ((elem, attr), old_val) in &old.aria_attributes {
        if !new
            .aria_attributes
            .contains_key(&(elem.clone(), attr.clone()))
        {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::AriaChange,
                description: format!("{attr} attribute removed from <{elem}> in {component}"),
                old_value: Some(old_val.clone()),
                new_value: None,
                has_test_implications: true,
                test_description: Some(format!(
                    "Removed {attr} from <{elem}> will break queries using this attribute"
                )),
            });
        }
    }
}

// ── Role attributes ─────────────────────────────────────────────────────

fn diff_role_attributes(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    for (elem, new_role) in &new.role_attributes {
        match old.role_attributes.get(elem) {
            Some(old_role) if old_role != new_role => {
                changes.push(SourceLevelChange {
                    component: component.to_string(),
                    category: SourceLevelCategory::RoleChange,
                    description: format!(
                        "role on <{elem}> in {component} changed from '{old_role}' to '{new_role}'"
                    ),
                    old_value: Some(old_role.clone()),
                    new_value: Some(new_role.clone()),
                    has_test_implications: true,
                    test_description: Some(format!(
                        "getByRole('{old_role}') must change to getByRole('{new_role}')"
                    )),
                });
            }
            None => {
                changes.push(SourceLevelChange {
                    component: component.to_string(),
                    category: SourceLevelCategory::RoleChange,
                    description: format!("role='{new_role}' added to <{elem}> in {component}"),
                    old_value: None,
                    new_value: Some(new_role.clone()),
                    has_test_implications: true,
                    test_description: Some(format!(
                        "New role='{new_role}' on <{elem}> enables getByRole('{new_role}') queries"
                    )),
                });
            }
            _ => {}
        }
    }

    for (elem, old_role) in &old.role_attributes {
        if !new.role_attributes.contains_key(elem) {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::RoleChange,
                description: format!("role='{old_role}' removed from <{elem}> in {component}"),
                old_value: Some(old_role.clone()),
                new_value: None,
                has_test_implications: true,
                test_description: Some(format!(
                    "getByRole('{old_role}') will no longer find this element"
                )),
            });
        }
    }
}

// ── Data attributes ─────────────────────────────────────────────────────

fn diff_data_attributes(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    for ((elem, attr), val) in &new.data_attributes {
        if !old
            .data_attributes
            .contains_key(&(elem.clone(), attr.clone()))
        {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::DataAttribute,
                description: format!("{attr} added to <{elem}> in {component}"),
                old_value: None,
                new_value: Some(val.clone()),
                has_test_implications: true,
                test_description: Some(format!(
                    "New {attr} on <{elem}> may affect getByTestId or OUIA selectors"
                )),
            });
        }
    }

    for ((elem, attr), old_val) in &old.data_attributes {
        if !new
            .data_attributes
            .contains_key(&(elem.clone(), attr.clone()))
        {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::DataAttribute,
                description: format!("{attr} removed from <{elem}> in {component}"),
                old_value: Some(old_val.clone()),
                new_value: None,
                has_test_implications: true,
                test_description: Some(format!(
                    "Removed {attr} from <{elem}> will break selectors using this attribute"
                )),
            });
        }
    }
}

// ── CSS tokens ──────────────────────────────────────────────────────────

fn diff_css_tokens(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    for token in new.css_tokens_used.difference(&old.css_tokens_used) {
        changes.push(SourceLevelChange {
            component: component.to_string(),
            category: SourceLevelCategory::CssToken,
            description: format!("{component} now uses CSS token {token}"),
            old_value: None,
            new_value: Some(token.clone()),
            has_test_implications: true,
            test_description: Some(format!(
                "New CSS class from {token} may affect toHaveClass assertions"
            )),
        });
    }

    for token in old.css_tokens_used.difference(&new.css_tokens_used) {
        changes.push(SourceLevelChange {
            component: component.to_string(),
            category: SourceLevelCategory::CssToken,
            description: format!("{component} no longer uses CSS token {token}"),
            old_value: Some(token.clone()),
            new_value: None,
            has_test_implications: true,
            test_description: Some(format!(
                "Removed CSS class from {token} will break toHaveClass assertions"
            )),
        });
    }
}

// ── Children slot ───────────────────────────────────────────────────────

fn diff_children_slot(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    if old.children_slot_path != new.children_slot_path
        && !old.children_slot_path.is_empty()
        && !new.children_slot_path.is_empty()
    {
        changes.push(SourceLevelChange {
            component: component.to_string(),
            category: SourceLevelCategory::Composition,
            description: format!(
                "Internal wrapper structure around children in {component} changed from {} to {}",
                old.children_slot_path.join(" > "),
                new.children_slot_path.join(" > "),
            ),
            old_value: Some(old.children_slot_path.join(" > ")),
            new_value: Some(new.children_slot_path.join(" > ")),
            has_test_implications: false,
            test_description: None,
        });
    }

    if old.has_children_prop && !new.has_children_prop {
        changes.push(SourceLevelChange {
            component: component.to_string(),
            category: SourceLevelCategory::Composition,
            description: format!("{component} no longer accepts children"),
            old_value: Some("children: React.ReactNode".into()),
            new_value: None,
            has_test_implications: false,
            test_description: None,
        });
    }

    if !old.has_children_prop && new.has_children_prop {
        changes.push(SourceLevelChange {
            component: component.to_string(),
            category: SourceLevelCategory::Composition,
            description: format!("{component} now accepts children"),
            old_value: None,
            new_value: Some("children: React.ReactNode".into()),
            has_test_implications: false,
            test_description: None,
        });
    }
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
    fn test_diff_portal_added() {
        let old = make_profile("Dropdown");
        let mut new = make_profile("Dropdown");
        new.uses_portal = true;

        let changes = diff_profiles(&old, &new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].category, SourceLevelCategory::PortalUsage);
        assert!(changes[0].has_test_implications);
        assert!(changes[0].test_description.is_some());
    }

    #[test]
    fn test_diff_context_added() {
        let old = make_profile("AccordionContent");
        let mut new = make_profile("AccordionContent");
        new.consumed_contexts = vec!["AccordionItemContext".into()];

        let changes = diff_profiles(&old, &new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].category, SourceLevelCategory::ContextDependency);
        assert!(changes[0].description.contains("AccordionItemContext"));
    }

    #[test]
    fn test_diff_prop_default_changed() {
        let mut old = make_profile("Button");
        old.prop_defaults
            .insert("variant".into(), "'primary'".into());

        let mut new = make_profile("Button");
        new.prop_defaults
            .insert("variant".into(), "'secondary'".into());

        let changes = diff_profiles(&old, &new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].category, SourceLevelCategory::PropDefault);
        assert!(changes[0].description.contains("primary"));
        assert!(changes[0].description.contains("secondary"));
    }

    #[test]
    fn test_diff_no_changes() {
        let mut profile = make_profile("Button");
        profile.uses_portal = false;
        profile
            .prop_defaults
            .insert("variant".into(), "'primary'".into());

        let changes = diff_profiles(&profile, &profile);
        assert!(changes.is_empty());
    }

    #[test]
    fn test_diff_role_changed() {
        let mut old = make_profile("Menu");
        old.role_attributes.insert("ul".into(), "menu".into());

        let mut new = make_profile("Menu");
        new.role_attributes.insert("ul".into(), "listbox".into());

        let changes = diff_profiles(&old, &new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].category, SourceLevelCategory::RoleChange);
        assert!(changes[0]
            .test_description
            .as_ref()
            .unwrap()
            .contains("getByRole"));
    }

    #[test]
    fn test_diff_forward_ref_added() {
        let old = make_profile("Input");
        let mut new = make_profile("Input");
        new.is_forward_ref = true;

        let changes = diff_profiles(&old, &new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].category, SourceLevelCategory::ForwardRef);
    }

    #[test]
    fn test_diff_css_token_changes() {
        let mut old = make_profile("Menu");
        old.css_tokens_used.insert("styles.menu".into());
        old.css_tokens_used.insert("styles.menuOldToken".into());

        let mut new = make_profile("Menu");
        new.css_tokens_used.insert("styles.menu".into());
        new.css_tokens_used.insert("styles.menuNewToken".into());

        let changes = diff_profiles(&old, &new);
        assert_eq!(changes.len(), 2); // one removed, one added
        let categories: Vec<_> = changes.iter().map(|c| &c.category).collect();
        assert!(categories
            .iter()
            .all(|c| **c == SourceLevelCategory::CssToken));
    }
}
