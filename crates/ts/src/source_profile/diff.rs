//! Diff two `ComponentSourceProfile`s to produce `SourceLevelChange` entries.
//!
//! Each change is deterministic — a fact derived from comparing two AST-extracted
//! profiles. No confidence scores, no LLM involvement.

use crate::sd_types::{ComponentSourceProfile, SourceLevelCategory, SourceLevelChange};
use std::collections::BTreeSet;

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
    diff_attribute_conditionality(old, new, component, &mut changes);
    diff_css_tokens(old, new, component, &mut changes);
    diff_prop_style_bindings(old, new, component, &mut changes);
    diff_managed_attributes(old, new, component, &mut changes);
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
                Some("screen.getByText() and similar queries cannot find content rendered via portal. \
                     Use within(document.body).getByText() or configure baseElement in render options.".to_string()),
            )
        } else {
            (
                format!(
                    "{component} no longer uses createPortal — content renders inline in the component tree"
                ),
                Some("Content now renders inside the component tree. \
                     Remove any within(document.body) workarounds if they were used.".to_string()),
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
            element: None,
            migration_from: None,
            dependency_chain: None,
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
                element: None,
                migration_from: None,
                dependency_chain: None,
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
                element: None,
                migration_from: None,
                dependency_chain: None,
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
                element: None,
                migration_from: None,
                dependency_chain: None,
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
                element: None,
                migration_from: None,
                dependency_chain: None,
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
            element: None,
            migration_from: None,
            dependency_chain: None,
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
            element: None,
            migration_from: None,
            dependency_chain: None,
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
                    element: None,
                    migration_from: None,
                    dependency_chain: None,
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
                    element: None,
                    migration_from: None,
                    dependency_chain: None,
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
                element: None,
                migration_from: None,
                dependency_chain: None,
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
    let old_names: BTreeSet<&str> = old
        .rendered_components
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    let new_names: BTreeSet<&str> = new
        .rendered_components
        .iter()
        .map(|r| r.name.as_str())
        .collect();

    for name in &new_names {
        if !old_names.contains(name) {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::RenderedComponent,
                description: format!("{component} now internally renders {name}"),
                old_value: None,
                new_value: Some(name.to_string()),
                has_test_implications: false,
                test_description: None,
                element: None,
                migration_from: None,
                dependency_chain: None,
            });
        }
    }

    for name in &old_names {
        if !new_names.contains(name) {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::RenderedComponent,
                description: format!("{component} no longer internally renders {name}"),
                old_value: Some(name.to_string()),
                new_value: None,
                has_test_implications: false,
                test_description: None,
                element: None,
                migration_from: None,
                dependency_chain: None,
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
                element: Some(elem.clone()),
                migration_from: None,
                dependency_chain: None,
            });
        }
    }

    // Elements removed
    for elem in old.rendered_elements.keys() {
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
                element: Some(elem.clone()),
                migration_from: None,
                dependency_chain: None,
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
    for ((elem, attr), val) in new.aria_attributes.iter() {
        let key = (elem.clone(), attr.clone());
        if !old.aria_attributes.contains_key(&key) {
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
                element: Some(elem.clone()),
                migration_from: None,
                dependency_chain: None,
            });
        } else if let Some(old_val) = old.aria_attributes.get(&key) {
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
                    element: Some(elem.clone()),
                    migration_from: None,
                    dependency_chain: None,
                });
            }
        }
    }

    // Removed
    for ((elem, attr), old_val) in old.aria_attributes.iter() {
        let key = (elem.clone(), attr.clone());
        if !new.aria_attributes.contains_key(&key) {
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
                element: Some(elem.clone()),
                migration_from: None,
                dependency_chain: None,
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
    for (elem, new_role) in new.role_attributes.iter() {
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
                    element: Some(elem.clone()),
                    migration_from: None,
                    dependency_chain: None,
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
                    element: Some(elem.clone()),
                    migration_from: None,
                    dependency_chain: None,
                });
            }
            _ => {}
        }
    }

    for (elem, old_role) in old.role_attributes.iter() {
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
                element: Some(elem.clone()),
                migration_from: None,
                dependency_chain: None,
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
    for ((elem, attr), val) in new.data_attributes.iter() {
        let key = (elem.clone(), attr.clone());
        if !old.data_attributes.contains_key(&key) {
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
                element: Some(elem.clone()),
                migration_from: None,
                dependency_chain: None,
            });
        }
    }

    for ((elem, attr), old_val) in old.data_attributes.iter() {
        let key = (elem.clone(), attr.clone());
        if !new.data_attributes.contains_key(&key) {
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
                element: Some(elem.clone()),
                migration_from: None,
                dependency_chain: None,
            });
        }
    }
}

// ── Attribute conditionality ────────────────────────────────────────────
//
// Detects when an attribute transitions from unconditional (always-present)
// to conditional (sometimes-absent) rendering. This catches the common PFv6
// pattern where an attribute like `aria-disabled` was always rendered in v5
// (set to "false" when not disabled) but is omitted entirely in v6 when
// not active. Tests using `getAttribute()` expecting a string value will
// now receive `null`.

fn diff_attribute_conditionality(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    // ARIA attributes: unconditional → conditional
    for key in old.aria_attributes.keys() {
        if !new.aria_attributes.contains_key(key) {
            continue; // Removed entirely — handled by diff_aria_attributes
        }
        let (ref elem, ref attr) = key;
        if old.aria_attributes.is_unconditional(key) && !new.aria_attributes.is_unconditional(key) {
            let old_val = old.aria_attributes.get(key).cloned().unwrap_or_default();
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::AttributeConditionality,
                description: format!(
                    "{attr} on <{elem}> in {component} changed from always-present \
                     to conditional — getAttribute('{attr}') may now return null"
                ),
                old_value: Some(format!("always-present (value: {old_val})")),
                new_value: Some("conditional".into()),
                has_test_implications: true,
                test_description: Some(format!(
                    "getAttribute('{attr}') on <{elem}> may now return null instead of \
                     a string value. Update assertions from .toBe('false') to \
                     .toBeNull() or .not.toHaveAttribute('{attr}')"
                )),
                element: Some(elem.clone()),
                migration_from: None,
                dependency_chain: None,
            });
        }
    }

    // Role attributes: unconditional → conditional
    for key in old.role_attributes.keys() {
        if !new.role_attributes.contains_key(key) {
            continue; // Removed — handled by diff_role_attributes
        }
        if old.role_attributes.is_unconditional(key) && !new.role_attributes.is_unconditional(key) {
            let old_val = old.role_attributes.get(key).cloned().unwrap_or_default();
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::AttributeConditionality,
                description: format!(
                    "role on <{key}> in {component} changed from always-present \
                     to conditional — getAttribute('role') may now return null"
                ),
                old_value: Some(format!("always-present (value: {old_val})")),
                new_value: Some("conditional".into()),
                has_test_implications: true,
                test_description: Some(format!(
                    "getAttribute('role') on <{key}> may now return null. \
                     Update assertions accordingly."
                )),
                element: Some(key.clone()),
                migration_from: None,
                dependency_chain: None,
            });
        }
    }

    // Data attributes: unconditional → conditional
    for key in old.data_attributes.keys() {
        if !new.data_attributes.contains_key(key) {
            continue; // Removed — handled by diff_data_attributes
        }
        let (ref elem, ref attr) = key;
        if old.data_attributes.is_unconditional(key) && !new.data_attributes.is_unconditional(key) {
            let old_val = old.data_attributes.get(key).cloned().unwrap_or_default();
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::AttributeConditionality,
                description: format!(
                    "{attr} on <{elem}> in {component} changed from always-present \
                     to conditional"
                ),
                old_value: Some(format!("always-present (value: {old_val})")),
                new_value: Some("conditional".into()),
                has_test_implications: true,
                test_description: Some(format!(
                    "getAttribute('{attr}') on <{elem}> may now return null. \
                     Update selectors and assertions accordingly."
                )),
                element: Some(elem.clone()),
                migration_from: None,
                dependency_chain: None,
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
            element: None,
            migration_from: None,
            dependency_chain: None,
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
            element: None,
            migration_from: None,
            dependency_chain: None,
        });
    }
}

// ── Prop-to-style bindings ──────────────────────────────────────────────

fn diff_prop_style_bindings(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    // Check each old binding: did the token disappear while the prop survived?
    for (prop, old_tokens) in &old.prop_style_bindings {
        let prop_still_exists = new.all_props.contains(prop);

        for token in old_tokens {
            let token_still_used = new.css_tokens_used.contains(token);
            let still_bound = new
                .prop_style_bindings
                .get(prop)
                .is_some_and(|t| t.contains(token));

            if prop_still_exists && !token_still_used {
                // The CSS token was removed entirely — prop is now a no-op
                changes.push(SourceLevelChange {
                    component: component.to_string(),
                    category: SourceLevelCategory::CssToken,
                    description: format!(
                        "{component} prop `{prop}` controlled CSS token `{token}` which has been removed — \
                         setting `{prop}` will have no visual effect"
                    ),
                    old_value: Some(format!("{prop} → {token}")),
                    new_value: None,
                    has_test_implications: true,
                    test_description: Some(format!(
                        "Tests relying on `{prop}` to apply CSS class from `{token}` will no longer see that class"
                    )),
                    element: None,
                    migration_from: None,
                    dependency_chain: None,
                });
            } else if prop_still_exists && token_still_used && !still_bound {
                // The token still exists but the prop no longer controls it
                changes.push(SourceLevelChange {
                    component: component.to_string(),
                    category: SourceLevelCategory::CssToken,
                    description: format!(
                        "{component} prop `{prop}` no longer controls CSS token `{token}` — \
                         the class may now be applied unconditionally or via a different mechanism"
                    ),
                    old_value: Some(format!("{prop} → {token}")),
                    new_value: None,
                    has_test_implications: true,
                    test_description: Some(format!(
                        "Tests toggling `{prop}` to control `{token}` may need updating"
                    )),
                    element: None,
                    migration_from: None,
                    dependency_chain: None,
                });
            }
        }
    }

    // Check for newly introduced bindings (informational)
    for (prop, new_tokens) in &new.prop_style_bindings {
        let is_new_prop = !old.all_props.contains(prop);
        let old_tokens = old.prop_style_bindings.get(prop);

        for token in new_tokens {
            let was_bound = old_tokens.is_some_and(|t| t.contains(token));

            if !is_new_prop && !was_bound {
                // Existing prop now controls a new style token
                changes.push(SourceLevelChange {
                    component: component.to_string(),
                    category: SourceLevelCategory::CssToken,
                    description: format!(
                        "{component} prop `{prop}` now controls CSS token `{token}`"
                    ),
                    old_value: None,
                    new_value: Some(format!("{prop} → {token}")),
                    has_test_implications: true,
                    test_description: Some(format!(
                        "Setting `{prop}` will now apply CSS class from `{token}`"
                    )),
                    element: None,
                    migration_from: None,
                    dependency_chain: None,
                });
            }
        }
    }
}

// ── Managed attributes (prop overrides HTML attribute) ───────────────────

fn diff_managed_attributes(
    old: &ComponentSourceProfile,
    new: &ComponentSourceProfile,
    component: &str,
    changes: &mut Vec<SourceLevelChange>,
) {
    // Build lookup maps keyed by (prop_name, generator_function) for efficient diff
    let old_bindings: std::collections::HashSet<_> = old
        .managed_attributes
        .iter()
        .map(|b| (&b.prop_name, &b.generator_function))
        .collect();
    let new_bindings: std::collections::HashSet<_> = new
        .managed_attributes
        .iter()
        .map(|b| (&b.prop_name, &b.generator_function))
        .collect();

    // New managed attributes — component now overrides consumer-provided HTML attrs.
    // Only emit PropAttributeOverride changes for component-wins bindings.
    // Consumer-wins bindings (managed spread before rest) are tracked for
    // transitive behavioral change detection (Phase A.7) but don't generate
    // prop-override rules since the consumer can override the managed value.
    //
    // Also detects spread order transitions: when a binding existed in the old
    // version with `component_overrides: false` (consumer wins) and now has
    // `component_overrides: true` (component wins). This is the scenario where
    // consumer's explicit attribute values that worked before are now silently
    // overridden. (e.g., PF 5.3→5.4 changed OUIA spread order.)
    for binding in &new.managed_attributes {
        if !binding.component_overrides {
            continue;
        }
        let key = (&binding.prop_name, &binding.generator_function);

        // Check if the binding is brand new OR if it transitioned from consumer-wins
        let is_new_binding = !old_bindings.contains(&key);
        let old_was_consumer_wins = old.managed_attributes.iter().any(|b| {
            b.prop_name == binding.prop_name
                && b.generator_function == binding.generator_function
                && !b.component_overrides
        });

        if is_new_binding || old_was_consumer_wins {
            let attrs_list = if binding.overridden_attributes.is_empty() {
                "HTML attributes".to_string()
            } else {
                binding.overridden_attributes.join(", ")
            };

            let description = if old_was_consumer_wins {
                format!(
                    "{component}'s `{prop}` prop now silently overrides {attrs} via {func}(). \
                     Previously, consumer-provided values took precedence. \
                     Any explicit `{attrs}` attributes on this component will be ignored.",
                    prop = binding.prop_name,
                    attrs = attrs_list,
                    func = binding.generator_function,
                )
            } else {
                format!(
                    "{component}'s `{prop}` prop overrides {attrs} via {func}(). \
                     Use the `{prop}` prop instead of setting these HTML attributes directly.",
                    prop = binding.prop_name,
                    attrs = attrs_list,
                    func = binding.generator_function,
                )
            };

            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::PropAttributeOverride,
                description,
                old_value: if old_was_consumer_wins {
                    Some(format!(
                        "{} → {} (consumer wins)",
                        binding.prop_name,
                        binding.overridden_attributes.join(", ")
                    ))
                } else {
                    None
                },
                new_value: Some(format!(
                    "{} → {}{}",
                    binding.prop_name,
                    binding.overridden_attributes.join(", "),
                    if old_was_consumer_wins {
                        " (component wins)"
                    } else {
                        ""
                    }
                )),
                has_test_implications: true,
                test_description: Some(format!(
                    "DOM queries using {} will still work, but consumer code should \
                     use the `{}` prop for correct lifecycle management",
                    binding
                        .overridden_attributes
                        .first()
                        .unwrap_or(&"the managed attribute".to_string()),
                    binding.prop_name,
                )),
                element: None,
                migration_from: None,
                dependency_chain: None,
            });
        }
    }

    // Removed managed attributes — component no longer overrides.
    // Only emit for component-wins bindings (same filter as above).
    for binding in &old.managed_attributes {
        if !binding.component_overrides {
            continue;
        }
        let key = (&binding.prop_name, &binding.generator_function);
        if !new_bindings.contains(&key) {
            changes.push(SourceLevelChange {
                component: component.to_string(),
                category: SourceLevelCategory::PropAttributeOverride,
                description: format!(
                    "{component} no longer manages `{prop}` via {func}(). \
                     HTML attributes previously overridden by this prop can now be set directly.",
                    prop = binding.prop_name,
                    func = binding.generator_function,
                ),
                old_value: Some(format!(
                    "{} → {}",
                    binding.prop_name,
                    binding.overridden_attributes.join(", ")
                )),
                new_value: None,
                has_test_implications: false,
                test_description: None,
                element: None,
                migration_from: None,
                dependency_chain: None,
            });
        }
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
            element: None,
            migration_from: None,
            dependency_chain: None,
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
            element: None,
            migration_from: None,
            dependency_chain: None,
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
            element: None,
            migration_from: None,
            dependency_chain: None,
        });
    }
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
        old.role_attributes
            .insert("ul".into(), "menu".into(), false);

        let mut new = make_profile("Menu");
        new.role_attributes
            .insert("ul".into(), "listbox".into(), false);

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

    // ── Prop-to-style binding diff tests ───────────────────────────────

    /// Simulates the real PatternFly scenario: Menu's `isScrollable` prop
    /// controls `styles.modifiers.scrollable`. In the new version, the CSS
    /// token is removed but the prop remains — making it a silent no-op.
    #[test]
    fn test_diff_prop_style_binding_token_removed() {
        let mut old = make_profile("Menu");
        old.all_props.insert("isScrollable".into());
        old.all_props.insert("isPlain".into());
        old.css_tokens_used.insert("styles.menu".into());
        old.css_tokens_used
            .insert("styles.modifiers.scrollable".into());
        old.css_tokens_used.insert("styles.modifiers.plain".into());
        old.prop_style_bindings.insert(
            "isScrollable".into(),
            BTreeSet::from(["styles.modifiers.scrollable".to_string()]),
        );
        old.prop_style_bindings.insert(
            "isPlain".into(),
            BTreeSet::from(["styles.modifiers.plain".to_string()]),
        );

        // New version: `isScrollable` prop still exists but its CSS token is gone.
        // `isPlain` is unchanged.
        let mut new = make_profile("Menu");
        new.all_props.insert("isScrollable".into());
        new.all_props.insert("isPlain".into());
        new.css_tokens_used.insert("styles.menu".into());
        // styles.modifiers.scrollable REMOVED from css_tokens_used
        new.css_tokens_used.insert("styles.modifiers.plain".into());
        new.prop_style_bindings.insert(
            "isPlain".into(),
            BTreeSet::from(["styles.modifiers.plain".to_string()]),
        );

        let changes = diff_profiles(&old, &new);

        // Should have changes for:
        // 1. The css_tokens_used diff (scrollable removed) — from diff_css_tokens
        // 2. The prop-style binding break — from diff_prop_style_bindings
        let binding_changes: Vec<_> = changes
            .iter()
            .filter(|c| {
                c.description.contains("isScrollable") && c.description.contains("no visual effect")
            })
            .collect();
        assert_eq!(
            binding_changes.len(),
            1,
            "Expected one no-op prop change for isScrollable, got: {binding_changes:?}"
        );

        let change = &binding_changes[0];
        assert_eq!(change.category, SourceLevelCategory::CssToken);
        assert!(change.has_test_implications);
        assert!(
            change.description.contains("styles.modifiers.scrollable"),
            "Description should reference the removed token"
        );
    }

    /// The prop is removed along with the token — this is a clean removal,
    /// NOT a no-op. The prop-style diff should produce no change because
    /// the prop itself is gone (the structural diff handles prop removals).
    #[test]
    fn test_diff_prop_style_binding_both_prop_and_token_removed() {
        let mut old = make_profile("Menu");
        old.all_props.insert("isScrollable".into());
        old.css_tokens_used
            .insert("styles.modifiers.scrollable".into());
        old.prop_style_bindings.insert(
            "isScrollable".into(),
            BTreeSet::from(["styles.modifiers.scrollable".to_string()]),
        );

        // New version: prop AND token both removed
        let new = make_profile("Menu");

        let changes = diff_profiles(&old, &new);

        // The prop-style diff should NOT emit a no-op warning because the
        // prop itself was removed. Only the css_tokens diff should fire.
        let noop_changes: Vec<_> = changes
            .iter()
            .filter(|c| c.description.contains("no visual effect"))
            .collect();
        assert!(
            noop_changes.is_empty(),
            "No no-op warning expected when prop is also removed: {noop_changes:?}"
        );
    }

    /// The binding is removed but the token still exists — the prop no
    /// longer controls the class (it might be applied unconditionally now).
    #[test]
    fn test_diff_prop_style_binding_decoupled() {
        let mut old = make_profile("Menu");
        old.all_props.insert("isScrollable".into());
        old.css_tokens_used.insert("styles.menu".into());
        old.css_tokens_used
            .insert("styles.modifiers.scrollable".into());
        old.prop_style_bindings.insert(
            "isScrollable".into(),
            BTreeSet::from(["styles.modifiers.scrollable".to_string()]),
        );

        // New version: token still exists, prop still exists, but the
        // binding is gone (class now applied unconditionally)
        let mut new = make_profile("Menu");
        new.all_props.insert("isScrollable".into());
        new.css_tokens_used.insert("styles.menu".into());
        new.css_tokens_used
            .insert("styles.modifiers.scrollable".into());
        // No prop_style_bindings entry for isScrollable

        let changes = diff_profiles(&old, &new);

        let decoupled: Vec<_> = changes
            .iter()
            .filter(|c| {
                c.description.contains("isScrollable")
                    && c.description.contains("no longer controls")
            })
            .collect();
        assert_eq!(
            decoupled.len(),
            1,
            "Expected one decoupled change for isScrollable: {decoupled:?}"
        );
    }

    /// New binding introduced on an existing prop — informational change.
    #[test]
    fn test_diff_prop_style_binding_new_binding() {
        let mut old = make_profile("Card");
        old.all_props.insert("isCompact".into());
        old.css_tokens_used.insert("styles.card".into());
        // No binding in old version

        let mut new = make_profile("Card");
        new.all_props.insert("isCompact".into());
        new.css_tokens_used.insert("styles.card".into());
        new.css_tokens_used
            .insert("styles.modifiers.compact".into());
        new.prop_style_bindings.insert(
            "isCompact".into(),
            BTreeSet::from(["styles.modifiers.compact".to_string()]),
        );

        let changes = diff_profiles(&old, &new);

        let new_binding: Vec<_> = changes
            .iter()
            .filter(|c| {
                c.description.contains("isCompact") && c.description.contains("now controls")
            })
            .collect();
        assert_eq!(
            new_binding.len(),
            1,
            "Expected one new-binding change for isCompact: {new_binding:?}"
        );
    }

    /// No changes when both profiles have identical bindings.
    #[test]
    fn test_diff_prop_style_binding_no_changes() {
        let mut profile = make_profile("Menu");
        profile.all_props.insert("isScrollable".into());
        profile
            .css_tokens_used
            .insert("styles.modifiers.scrollable".into());
        profile.prop_style_bindings.insert(
            "isScrollable".into(),
            BTreeSet::from(["styles.modifiers.scrollable".to_string()]),
        );

        let changes = diff_profiles(&profile, &profile);

        let binding_changes: Vec<_> = changes
            .iter()
            .filter(|c| c.description.contains("isScrollable"))
            .collect();
        assert!(
            binding_changes.is_empty(),
            "No binding changes expected for identical profiles"
        );
    }

    // ── Managed attribute diff tests ────────────────────────────────────

    #[test]
    fn test_diff_managed_attribute_added() {
        use crate::sd_types::ManagedAttributeBinding;

        let old = make_profile("MenuToggle");
        let mut new = make_profile("MenuToggle");
        new.managed_attributes.push(ManagedAttributeBinding {
            prop_name: "ouiaId".into(),
            generator_function: "getOUIAProps".into(),
            target_element: "button".into(),
            overridden_attributes: vec![
                "data-ouia-component-id".into(),
                "data-ouia-component-type".into(),
            ],
            component_overrides: true,
        });

        let changes = diff_profiles(&old, &new);
        let managed: Vec<_> = changes
            .iter()
            .filter(|c| c.category == SourceLevelCategory::PropAttributeOverride)
            .collect();
        assert_eq!(
            managed.len(),
            1,
            "Expected one PropAttributeOverride change"
        );
        assert!(managed[0].description.contains("ouiaId"));
        assert!(managed[0].description.contains("getOUIAProps"));
        assert!(managed[0].has_test_implications);
    }

    #[test]
    fn test_diff_managed_attribute_removed() {
        use crate::sd_types::ManagedAttributeBinding;

        let mut old = make_profile("MenuToggle");
        old.managed_attributes.push(ManagedAttributeBinding {
            prop_name: "ouiaId".into(),
            generator_function: "getOUIAProps".into(),
            target_element: "button".into(),
            overridden_attributes: vec!["data-ouia-component-id".into()],
            component_overrides: true,
        });
        let new = make_profile("MenuToggle");

        let changes = diff_profiles(&old, &new);
        let managed: Vec<_> = changes
            .iter()
            .filter(|c| c.category == SourceLevelCategory::PropAttributeOverride)
            .collect();
        assert_eq!(managed.len(), 1);
        assert!(managed[0].description.contains("no longer manages"));
    }

    #[test]
    fn test_diff_managed_attribute_no_change() {
        use crate::sd_types::ManagedAttributeBinding;

        let binding = ManagedAttributeBinding {
            prop_name: "ouiaId".into(),
            generator_function: "getOUIAProps".into(),
            target_element: "button".into(),
            overridden_attributes: vec!["data-ouia-component-id".into()],
            component_overrides: true,
        };

        let mut profile = make_profile("MenuToggle");
        profile.managed_attributes.push(binding);

        let changes = diff_profiles(&profile, &profile);
        let managed: Vec<_> = changes
            .iter()
            .filter(|c| c.category == SourceLevelCategory::PropAttributeOverride)
            .collect();
        assert!(
            managed.is_empty(),
            "Expected no changes for identical managed attributes"
        );
    }

    // ── Attribute conditionality tests ──────────────────────────────

    #[test]
    fn test_diff_aria_unconditional_to_conditional() {
        let mut old = make_profile("Button");
        old.aria_attributes.insert(
            ("button".into(), "aria-disabled".into()),
            "{isDisabled}".into(),
            false, // unconditional in v5
        );

        let mut new = make_profile("Button");
        new.aria_attributes.insert(
            ("button".into(), "aria-disabled".into()),
            "true".into(),
            true, // conditional in v6
        );

        let changes = diff_profiles(&old, &new);
        let cond_changes: Vec<_> = changes
            .iter()
            .filter(|c| c.category == SourceLevelCategory::AttributeConditionality)
            .collect();
        assert_eq!(cond_changes.len(), 1);
        assert!(cond_changes[0].description.contains("aria-disabled"));
        assert!(cond_changes[0].description.contains("always-present"));
        assert!(cond_changes[0].description.contains("conditional"));
        assert!(cond_changes[0].has_test_implications);
        assert!(cond_changes[0].element.as_deref() == Some("button"));
    }

    #[test]
    fn test_diff_aria_conditional_to_conditional_no_change() {
        let mut old = make_profile("Button");
        old.aria_attributes.insert(
            ("button".into(), "aria-disabled".into()),
            "true".into(),
            true,
        );

        let mut new = make_profile("Button");
        new.aria_attributes.insert(
            ("button".into(), "aria-disabled".into()),
            "true".into(),
            true,
        );

        let changes = diff_profiles(&old, &new);
        let cond_changes: Vec<_> = changes
            .iter()
            .filter(|c| c.category == SourceLevelCategory::AttributeConditionality)
            .collect();
        assert!(cond_changes.is_empty());
    }

    #[test]
    fn test_diff_aria_removed_not_conditionality() {
        // When attribute is fully removed, it should be AriaChange, not AttributeConditionality
        let mut old = make_profile("Button");
        old.aria_attributes.insert(
            ("button".into(), "aria-disabled".into()),
            "false".into(),
            false,
        );

        let new = make_profile("Button");
        // aria-disabled not present at all in new

        let changes = diff_profiles(&old, &new);
        assert!(changes
            .iter()
            .all(|c| c.category != SourceLevelCategory::AttributeConditionality));
        assert!(changes
            .iter()
            .any(|c| c.category == SourceLevelCategory::AriaChange));
    }

    #[test]
    fn test_diff_role_unconditional_to_conditional() {
        let mut old = make_profile("Menu");
        old.role_attributes
            .insert("ul".into(), "menu".into(), false);

        let mut new = make_profile("Menu");
        new.role_attributes.insert("ul".into(), "menu".into(), true); // same value, now conditional

        let changes = diff_profiles(&old, &new);
        let cond_changes: Vec<_> = changes
            .iter()
            .filter(|c| c.category == SourceLevelCategory::AttributeConditionality)
            .collect();
        assert_eq!(cond_changes.len(), 1);
        assert!(cond_changes[0].description.contains("role"));
        assert!(cond_changes[0].description.contains("always-present"));
    }

    #[test]
    fn test_diff_data_unconditional_to_conditional() {
        let mut old = make_profile("Button");
        old.data_attributes.insert(
            ("button".into(), "data-ouia-component-type".into()),
            "Button".into(),
            false,
        );

        let mut new = make_profile("Button");
        new.data_attributes.insert(
            ("button".into(), "data-ouia-component-type".into()),
            "Button".into(),
            true,
        );

        let changes = diff_profiles(&old, &new);
        let cond_changes: Vec<_> = changes
            .iter()
            .filter(|c| c.category == SourceLevelCategory::AttributeConditionality)
            .collect();
        assert_eq!(cond_changes.len(), 1);
        assert!(cond_changes[0]
            .description
            .contains("data-ouia-component-type"));
    }

    #[test]
    fn test_diff_unconditional_both_sides_no_change() {
        // Attribute is unconditional in both versions — no conditionality change
        let mut old = make_profile("Button");
        old.aria_attributes.insert(
            ("button".into(), "aria-disabled".into()),
            "{isDisabled}".into(),
            false,
        );

        let mut new = make_profile("Button");
        new.aria_attributes.insert(
            ("button".into(), "aria-disabled".into()),
            "{isDisabled}".into(),
            false,
        );

        let changes = diff_profiles(&old, &new);
        let cond_changes: Vec<_> = changes
            .iter()
            .filter(|c| c.category == SourceLevelCategory::AttributeConditionality)
            .collect();
        assert!(cond_changes.is_empty());
    }
}
