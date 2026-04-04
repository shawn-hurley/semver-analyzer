//! Individual comparison functions for the diff engine.
//!
//! Each function compares a specific aspect of two matched symbols
//! (visibility, modifiers, hierarchy, signatures, members) and emits
//! `StructuralChange` entries for detected differences.

use crate::traits::LanguageSemantics;
use crate::types::{
    ChangeSubject, Parameter, Signature, StructuralChange, StructuralChangeType, Symbol, SymbolKind,
};
use std::collections::HashMap;

use super::helpers::{change, kind_label, param_summary, symbol_summary, type_param_summary};
use super::rename::detect_renames;

// ─── Symbol-level diff ───────────────────────────────────────────────────

/// Compare all aspects of two matched symbols and emit changes.
///
/// This is the central dispatch for symbol-level comparison. It calls
/// individual comparison functions for each aspect. Note the mutual
/// recursion with `diff_members` (which calls back into `diff_symbol`
/// for matched member pairs).
pub(super) fn diff_symbol<M: Default + Clone, S: LanguageSemantics<M>>(
    old: &Symbol<M>,
    new: &Symbol<M>,
    changes: &mut Vec<StructuralChange>,
    semantics: &S,
) {
    diff_visibility(old, new, changes, semantics);
    diff_modifiers(old, new, changes);
    diff_hierarchy(old, new, changes);
    diff_signatures(old, new, changes, semantics);
    diff_members(old, new, changes, semantics);

    // Ask the language for any additional changes from opaque language_data.
    // This hook lets languages detect annotation changes, throws clause changes,
    // rendered_components/css changes, etc. without leaking into core.
    let lang_changes = semantics.diff_language_data(old, new);
    if !lang_changes.is_empty() {
        changes.extend(lang_changes);
    }
}

// ─── Visibility diff ─────────────────────────────────────────────────────

fn diff_visibility<M: Default + Clone, S: LanguageSemantics<M>>(
    old: &Symbol<M>,
    new: &Symbol<M>,
    changes: &mut Vec<StructuralChange>,
    semantics: &S,
) {
    if old.visibility == new.visibility {
        return;
    }

    let old_rank = semantics.visibility_rank(old.visibility);
    let new_rank = semantics.visibility_rank(new.visibility);

    if new_rank < old_rank {
        changes.push(change(
            old,
            StructuralChangeType::Changed(ChangeSubject::Visibility),
            Some(format!("{:?}", old.visibility)),
            Some(format!("{:?}", new.visibility)),
            format!(
                "Visibility of `{}` was reduced from {:?} to {:?}",
                old.name, old.visibility, new.visibility
            ),
            true,
        ));
    } else {
        changes.push(change(
            old,
            StructuralChangeType::Changed(ChangeSubject::Visibility),
            Some(format!("{:?}", old.visibility)),
            Some(format!("{:?}", new.visibility)),
            format!(
                "Visibility of `{}` was increased from {:?} to {:?}",
                old.name, old.visibility, new.visibility
            ),
            false,
        ));
    }
}

// ─── Modifier diff ───────────────────────────────────────────────────────

fn diff_modifiers<M: Default + Clone>(
    old: &Symbol<M>,
    new: &Symbol<M>,
    changes: &mut Vec<StructuralChange>,
) {
    // readonly
    if !old.is_readonly && new.is_readonly {
        changes.push(change(
            old,
            StructuralChangeType::Added(ChangeSubject::Modifier {
                modifier: "readonly".into(),
            }),
            Some("mutable".into()),
            Some("readonly".into()),
            format!("`{}` was made readonly", old.name),
            true,
        ));
    } else if old.is_readonly && !new.is_readonly {
        changes.push(change(
            old,
            StructuralChangeType::Removed(ChangeSubject::Modifier {
                modifier: "readonly".into(),
            }),
            Some("readonly".into()),
            Some("mutable".into()),
            format!("`{}` is no longer readonly", old.name),
            false,
        ));
    }

    // abstract
    if !old.is_abstract && new.is_abstract {
        changes.push(change(
            old,
            StructuralChangeType::Added(ChangeSubject::Modifier {
                modifier: "abstract".into(),
            }),
            Some("concrete".into()),
            Some("abstract".into()),
            format!("`{}` was made abstract", old.name),
            true,
        ));
    } else if old.is_abstract && !new.is_abstract {
        changes.push(change(
            old,
            StructuralChangeType::Removed(ChangeSubject::Modifier {
                modifier: "abstract".into(),
            }),
            Some("abstract".into()),
            Some("concrete".into()),
            format!("`{}` is no longer abstract", old.name),
            false,
        ));
    }

    // static <-> instance
    if old.is_static != new.is_static {
        let (before, after) = if old.is_static {
            ("static", "instance")
        } else {
            ("instance", "static")
        };
        changes.push(change(
            old,
            StructuralChangeType::Changed(ChangeSubject::Modifier {
                modifier: "static".into(),
            }),
            Some(before.into()),
            Some(after.into()),
            format!("`{}` changed from {} to {} member", old.name, before, after),
            true,
        ));
    }

    // accessor kind changes
    if old.accessor_kind != new.accessor_kind {
        changes.push(change(
            old,
            StructuralChangeType::Changed(ChangeSubject::Modifier {
                modifier: "accessor".into(),
            }),
            Some(format!("{:?}", old.accessor_kind)),
            Some(format!("{:?}", new.accessor_kind)),
            format!(
                "`{}` accessor changed from {:?} to {:?}",
                old.name, old.accessor_kind, new.accessor_kind
            ),
            true,
        ));
    }
}

// ─── Class hierarchy diff ────────────────────────────────────────────────

fn diff_hierarchy<M: Default + Clone>(
    old: &Symbol<M>,
    new: &Symbol<M>,
    changes: &mut Vec<StructuralChange>,
) {
    // extends (base class)
    if old.extends != new.extends {
        changes.push(change(
            old,
            StructuralChangeType::Changed(ChangeSubject::BaseClass),
            old.extends.clone(),
            new.extends.clone(),
            format!(
                "`{}` base class changed from {} to {}",
                old.name,
                old.extends.as_deref().unwrap_or("none"),
                new.extends.as_deref().unwrap_or("none")
            ),
            true,
        ));
    }

    // implements (interfaces) — detect added and removed
    let old_impls: std::collections::HashSet<&str> =
        old.implements.iter().map(|s| s.as_str()).collect();
    let new_impls: std::collections::HashSet<&str> =
        new.implements.iter().map(|s| s.as_str()).collect();

    for added in new_impls.difference(&old_impls) {
        changes.push(change(
            old,
            StructuralChangeType::Added(ChangeSubject::InterfaceImpl {
                interface_name: added.to_string(),
            }),
            None,
            Some(added.to_string()),
            format!("`{}` now implements `{}`", old.name, added),
            false,
        ));
    }

    for removed in old_impls.difference(&new_impls) {
        changes.push(change(
            old,
            StructuralChangeType::Removed(ChangeSubject::InterfaceImpl {
                interface_name: removed.to_string(),
            }),
            Some(removed.to_string()),
            None,
            format!("`{}` no longer implements `{}`", old.name, removed),
            true,
        ));
    }
}

// ─── Signature diff ──────────────────────────────────────────────────────

fn diff_signatures<M: Default + Clone, S: LanguageSemantics<M>>(
    old: &Symbol<M>,
    new: &Symbol<M>,
    changes: &mut Vec<StructuralChange>,
    semantics: &S,
) {
    match (&old.signature, &new.signature) {
        (Some(old_sig), Some(new_sig)) => {
            diff_parameters(old, old_sig, new_sig, changes, semantics);
            diff_return_type(old, old_sig, new_sig, changes, semantics);
            diff_type_parameters(old, old_sig, new_sig, changes);
        }
        (Some(_), None) => {
            // Signature was removed — symbol changed kind (e.g., function → variable)
            changes.push(change(
                old,
                StructuralChangeType::Changed(ChangeSubject::ReturnType),
                Some("(has signature)".into()),
                Some("(no signature)".into()),
                format!("`{}` no longer has a callable signature", old.name),
                true,
            ));
        }
        (None, Some(_)) => {
            // Signature was added — symbol became callable
            // This is informational, not necessarily breaking
        }
        (None, None) => {
            // Neither has a signature — compare return_type if stored in signature
        }
    }
}

// ─── Parameter diff ──────────────────────────────────────────────────────

fn diff_parameters<M: Default + Clone, S: LanguageSemantics<M>>(
    sym: &Symbol<M>,
    old_sig: &Signature,
    new_sig: &Signature,
    changes: &mut Vec<StructuralChange>,
    semantics: &S,
) {
    let old_params = &old_sig.parameters;
    let new_params = &new_sig.parameters;

    let old_non_rest: Vec<&Parameter> = old_params.iter().filter(|p| !p.is_variadic).collect();
    let new_non_rest: Vec<&Parameter> = new_params.iter().filter(|p| !p.is_variadic).collect();
    let old_rest = old_params.iter().find(|p| p.is_variadic);
    let new_rest = new_params.iter().find(|p| p.is_variadic);

    // Compare matched parameters (by position)
    let common_len = old_non_rest.len().min(new_non_rest.len());
    for i in 0..common_len {
        let old_p = old_non_rest[i];
        let new_p = new_non_rest[i];

        // Type change
        if old_p.type_annotation != new_p.type_annotation {
            changes.push(change(
                sym,
                StructuralChangeType::Changed(ChangeSubject::Parameter {
                    name: old_p.name.clone(),
                }),
                old_p.type_annotation.clone(),
                new_p.type_annotation.clone(),
                format!(
                    "Parameter `{}` of `{}` changed type from `{}` to `{}`",
                    old_p.name,
                    sym.name,
                    old_p.type_annotation.as_deref().unwrap_or("untyped"),
                    new_p.type_annotation.as_deref().unwrap_or("untyped")
                ),
                true,
            ));

            // Also emit per-value union literal changes
            if let (Some(old_ta), Some(new_ta)) = (&old_p.type_annotation, &new_p.type_annotation) {
                diff_union_literals(sym, &old_p.name, old_ta, new_ta, changes, semantics);
            }
        }

        // Optionality change
        if old_p.optional && !new_p.optional {
            changes.push(change(
                sym,
                StructuralChangeType::Changed(ChangeSubject::Parameter {
                    name: old_p.name.clone(),
                }),
                Some("optional".into()),
                Some("required".into()),
                format!(
                    "Parameter `{}` of `{}` was made required",
                    old_p.name, sym.name
                ),
                true,
            ));
        } else if !old_p.optional && new_p.optional {
            changes.push(change(
                sym,
                StructuralChangeType::Changed(ChangeSubject::Parameter {
                    name: old_p.name.clone(),
                }),
                Some("required".into()),
                Some("optional".into()),
                format!(
                    "Parameter `{}` of `{}` was made optional",
                    old_p.name, sym.name
                ),
                false,
            ));
        }

        // Default value change
        if old_p.default_value != new_p.default_value && old_p.has_default && new_p.has_default {
            changes.push(change(
                sym,
                StructuralChangeType::Changed(ChangeSubject::Parameter {
                    name: old_p.name.clone(),
                }),
                old_p.default_value.clone(),
                new_p.default_value.clone(),
                format!(
                    "Default value of parameter `{}` in `{}` changed",
                    old_p.name, sym.name
                ),
                true,
            ));
        }
    }

    // Parameters removed (old has more non-rest params than new)
    for p in old_non_rest.iter().skip(common_len) {
        changes.push(change(
            sym,
            StructuralChangeType::Removed(ChangeSubject::Parameter {
                name: p.name.clone(),
            }),
            Some(param_summary(p)),
            None,
            format!("Parameter `{}` was removed from `{}`", p.name, sym.name),
            true,
        ));
    }

    // Parameters added (new has more non-rest params than old)
    for p in new_non_rest.iter().skip(common_len) {
        let is_breaking = !p.optional && !p.has_default;
        changes.push(change(
            sym,
            StructuralChangeType::Added(ChangeSubject::Parameter {
                name: p.name.clone(),
            }),
            None,
            Some(param_summary(p)),
            format!(
                "{} parameter `{}` was added to `{}`",
                if is_breaking { "Required" } else { "Optional" },
                p.name,
                sym.name
            ),
            is_breaking,
        ));
    }

    // Rest parameter changes
    match (old_rest, new_rest) {
        (Some(old_r), Some(new_r)) => {
            if old_r.type_annotation != new_r.type_annotation {
                changes.push(change(
                    sym,
                    StructuralChangeType::Changed(ChangeSubject::Parameter {
                        name: old_r.name.clone(),
                    }),
                    old_r.type_annotation.clone(),
                    new_r.type_annotation.clone(),
                    format!(
                        "Rest parameter `{}` of `{}` changed type",
                        old_r.name, sym.name
                    ),
                    true,
                ));
            }
        }
        (None, Some(new_r)) => {
            changes.push(change(
                sym,
                StructuralChangeType::Added(ChangeSubject::Parameter {
                    name: new_r.name.clone(),
                }),
                None,
                Some(param_summary(new_r)),
                format!(
                    "Rest parameter `{}` was added to `{}`",
                    new_r.name, sym.name
                ),
                false,
            ));
        }
        (Some(old_r), None) => {
            changes.push(change(
                sym,
                StructuralChangeType::Removed(ChangeSubject::Parameter {
                    name: old_r.name.clone(),
                }),
                Some(param_summary(old_r)),
                None,
                format!(
                    "Rest parameter `{}` was removed from `{}`",
                    old_r.name, sym.name
                ),
                true,
            ));
        }
        (None, None) => {}
    }
}

// ─── Return type diff ────────────────────────────────────────────────────

fn diff_return_type<M: Default + Clone, S: LanguageSemantics<M>>(
    sym: &Symbol<M>,
    old_sig: &Signature,
    new_sig: &Signature,
    changes: &mut Vec<StructuralChange>,
    semantics: &S,
) {
    if old_sig.return_type == new_sig.return_type {
        return;
    }

    let old_ret = old_sig.return_type.as_deref().unwrap_or("void");
    let new_ret = new_sig.return_type.as_deref().unwrap_or("void");

    let old_is_async = semantics.is_async_wrapper(old_ret);
    let new_is_async = semantics.is_async_wrapper(new_ret);

    if !old_is_async && new_is_async {
        changes.push(change(
            sym,
            StructuralChangeType::Changed(ChangeSubject::ReturnType),
            old_sig.return_type.clone(),
            new_sig.return_type.clone(),
            format!(
                "`{}` was made async (return type wrapped in async wrapper)",
                sym.name
            ),
            true,
        ));
    } else if old_is_async && !new_is_async {
        changes.push(change(
            sym,
            StructuralChangeType::Changed(ChangeSubject::ReturnType),
            old_sig.return_type.clone(),
            new_sig.return_type.clone(),
            format!("`{}` was made sync (async wrapper removed)", sym.name),
            true,
        ));
    } else {
        changes.push(change(
            sym,
            StructuralChangeType::Changed(ChangeSubject::ReturnType),
            old_sig.return_type.clone(),
            new_sig.return_type.clone(),
            format!(
                "Return type of `{}` changed from `{}` to `{}`",
                sym.name, old_ret, new_ret
            ),
            true,
        ));

        // Also emit per-value union literal changes if both types are string literal unions
        diff_union_literals(sym, &sym.name, old_ret, new_ret, changes, semantics);
    }
}

// ─── Type parameter diff ─────────────────────────────────────────────────

fn diff_type_parameters<M: Default + Clone>(
    sym: &Symbol<M>,
    old_sig: &Signature,
    new_sig: &Signature,
    changes: &mut Vec<StructuralChange>,
) {
    let old_tps = &old_sig.type_parameters;
    let new_tps = &new_sig.type_parameters;

    if old_tps.is_empty() && new_tps.is_empty() {
        return;
    }

    let common_len = old_tps.len().min(new_tps.len());

    // Check for reordering (names at same positions differ)
    for i in 0..common_len {
        let old_tp = &old_tps[i];
        let new_tp = &new_tps[i];

        if old_tp.name != new_tp.name {
            let old_names: Vec<&str> = old_tps.iter().map(|t| t.name.as_str()).collect();
            let new_names: Vec<&str> = new_tps.iter().map(|t| t.name.as_str()).collect();

            let mut old_sorted = old_names.clone();
            let mut new_sorted = new_names.clone();
            old_sorted.sort();
            new_sorted.sort();

            if old_sorted == new_sorted && old_names != new_names {
                changes.push(change(
                    sym,
                    StructuralChangeType::Changed(ChangeSubject::TypeParameter {
                        name: old_tp.name.clone(),
                    }),
                    Some(format!("<{}>", old_names.join(", "))),
                    Some(format!("<{}>", new_names.join(", "))),
                    format!("Type parameters of `{}` were reordered", sym.name),
                    true,
                ));
                return;
            }
        }

        // Constraint change
        if old_tp.constraint != new_tp.constraint {
            changes.push(change(
                sym,
                StructuralChangeType::Changed(ChangeSubject::TypeParameter {
                    name: old_tp.name.clone(),
                }),
                old_tp.constraint.clone(),
                new_tp.constraint.clone(),
                format!(
                    "Constraint on type parameter `{}` of `{}` changed from `{}` to `{}`",
                    old_tp.name,
                    sym.name,
                    old_tp.constraint.as_deref().unwrap_or("unconstrained"),
                    new_tp.constraint.as_deref().unwrap_or("unconstrained")
                ),
                true,
            ));
        }

        // Default change
        if old_tp.default != new_tp.default {
            changes.push(change(
                sym,
                StructuralChangeType::Changed(ChangeSubject::TypeParameter {
                    name: old_tp.name.clone(),
                }),
                old_tp.default.clone(),
                new_tp.default.clone(),
                format!(
                    "Default for type parameter `{}` of `{}` changed",
                    old_tp.name, sym.name
                ),
                true,
            ));
        }
    }

    // Type parameters removed
    for tp in old_tps.iter().skip(common_len) {
        changes.push(change(
            sym,
            StructuralChangeType::Removed(ChangeSubject::TypeParameter {
                name: tp.name.clone(),
            }),
            Some(type_param_summary(tp)),
            None,
            format!(
                "Type parameter `{}` was removed from `{}`",
                tp.name, sym.name
            ),
            true,
        ));
    }

    // Type parameters added
    for tp in new_tps.iter().skip(common_len) {
        let is_breaking = tp.default.is_none();
        changes.push(change(
            sym,
            StructuralChangeType::Added(ChangeSubject::TypeParameter {
                name: tp.name.clone(),
            }),
            None,
            Some(type_param_summary(tp)),
            format!(
                "{} type parameter `{}` was added to `{}`",
                if is_breaking {
                    "Required"
                } else {
                    "Optional (has default)"
                },
                tp.name,
                sym.name
            ),
            is_breaking,
        ));
    }
}

// ─── Member diff (classes, interfaces, enums) ────────────────────────────

/// Compare members of two matched symbols (class/interface/enum).
///
/// This function is mutually recursive with `diff_symbol` — matched members
/// are compared by calling `diff_symbol` again. This is why both functions
/// live in the same module visibility scope.
pub(super) fn diff_members<M: Default + Clone, S: LanguageSemantics<M>>(
    old: &Symbol<M>,
    new: &Symbol<M>,
    changes: &mut Vec<StructuralChange>,
    semantics: &S,
) {
    if old.members.is_empty() && new.members.is_empty() {
        return;
    }

    let old_map: HashMap<&str, &Symbol<M>> =
        old.members.iter().map(|m| (m.name.as_str(), m)).collect();
    let new_map: HashMap<&str, &Symbol<M>> =
        new.members.iter().map(|m| (m.name.as_str(), m)).collect();

    // Collect removed and added members
    let removed: Vec<&Symbol<M>> = old
        .members
        .iter()
        .filter(|m| !new_map.contains_key(m.name.as_str()))
        .collect();
    let added: Vec<&Symbol<M>> = new
        .members
        .iter()
        .filter(|m| !old_map.contains_key(m.name.as_str()))
        .collect();

    // Detect renames (skip for enums — enum member renames are rare and
    // would be confusing since values matter more than names)
    let renames = if old.kind != SymbolKind::Enum {
        detect_renames(&removed, &added)
    } else {
        Vec::new()
    };

    let renamed_old: std::collections::HashSet<&str> =
        renames.iter().map(|r| r.old.name.as_str()).collect();
    let renamed_new: std::collections::HashSet<&str> =
        renames.iter().map(|r| r.new.name.as_str()).collect();

    // Emit rename changes
    for rm in &renames {
        changes.push(StructuralChange {
            symbol: rm.old.name.clone(),
            qualified_name: format!("{}.{}", old.qualified_name, rm.old.name),
            kind: rm.old.kind,
            package: rm.old.package.clone(),
            change_type: StructuralChangeType::Renamed {
                from: ChangeSubject::Member {
                    name: rm.old.name.clone(),
                    kind: rm.old.kind,
                },
                to: ChangeSubject::Member {
                    name: rm.new.name.clone(),
                    kind: rm.new.kind,
                },
            },
            before: Some(rm.old.name.clone()),
            after: Some(rm.new.name.clone()),
            description: format!(
                "{} `{}` was renamed to `{}` in `{}`",
                kind_label(rm.old.kind),
                rm.old.name,
                rm.new.name,
                old.name
            ),
            is_breaking: true,
            impact: None,
            migration_target: None,
        });
    }

    // Removed members (not part of a rename)
    for member in &removed {
        if renamed_old.contains(member.name.as_str()) {
            continue;
        }
        let (change_type, description, is_breaking) = match old.kind {
            SymbolKind::Enum => (
                StructuralChangeType::Removed(ChangeSubject::Member {
                    name: member.name.clone(),
                    kind: SymbolKind::EnumMember,
                }),
                format!(
                    "Enum member `{}` was removed from `{}`",
                    member.name, old.name
                ),
                true,
            ),
            _ => (
                StructuralChangeType::Removed(ChangeSubject::Member {
                    name: member.name.clone(),
                    kind: member.kind,
                }),
                format!(
                    "{} `{}` was removed from `{}`",
                    kind_label(member.kind),
                    member.name,
                    old.name
                ),
                true,
            ),
        };
        changes.push(change(
            member,
            change_type,
            Some(symbol_summary(member)),
            None,
            description,
            is_breaking,
        ));
    }

    // Added members (not part of a rename)
    for member in &added {
        if renamed_new.contains(member.name.as_str()) {
            continue;
        }
        let is_breaking = semantics.is_member_addition_breaking(new, member);
        let (change_type, description) = match new.kind {
            SymbolKind::Enum => (
                StructuralChangeType::Added(ChangeSubject::Member {
                    name: member.name.clone(),
                    kind: SymbolKind::EnumMember,
                }),
                format!("Enum member `{}` was added to `{}`", member.name, new.name),
            ),
            _ => (
                StructuralChangeType::Added(ChangeSubject::Member {
                    name: member.name.clone(),
                    kind: member.kind,
                }),
                format!(
                    "{} `{}` was added to `{}`",
                    kind_label(member.kind),
                    member.name,
                    new.name
                ),
            ),
        };
        changes.push(change(
            member,
            change_type,
            None,
            Some(symbol_summary(member)),
            description,
            is_breaking,
        ));
    }

    // Matched members — diff recursively
    for old_member in &old.members {
        if let Some(new_member) = new_map.get(old_member.name.as_str()) {
            if old.kind == SymbolKind::Enum {
                diff_enum_member_value(old, old_member, new_member, changes);
            } else {
                diff_symbol(old_member, new_member, changes, semantics);
            }
        }
    }
}

// ─── Union literal value diffing ─────────────────────────────────────────

/// Emit per-member union literal value changes.
///
/// When a property's type changes from `'a' | 'b' | 'c'` to `'a' | 'd'`,
/// emits:
///   - `UnionMemberRemoved` for `'b'` and `'c'`
///   - `UnionMemberAdded` for `'d'`
///
/// The parent symbol provides context (e.g., `Button.variant`).
fn diff_union_literals<M: Default + Clone, S: LanguageSemantics<M>>(
    sym: &Symbol<M>,
    prop_name: &str,
    old_type: &str,
    new_type: &str,
    changes: &mut Vec<StructuralChange>,
    semantics: &S,
) {
    let old_literals = match semantics.parse_union_values(old_type) {
        Some(l) => l,
        None => return,
    };
    let new_literals = match semantics.parse_union_values(new_type) {
        Some(l) => l,
        None => return,
    };

    // Skip if identical
    if old_literals == new_literals {
        return;
    }

    // Removed values (breaking)
    for removed in old_literals.difference(&new_literals) {
        changes.push(StructuralChange {
            symbol: format!("{}.{}", sym.name, prop_name),
            qualified_name: format!("{}.{}", sym.qualified_name, prop_name),
            kind: SymbolKind::Property,
            package: sym.package.clone(),
            change_type: StructuralChangeType::Removed(ChangeSubject::UnionValue {
                value: removed.clone(),
            }),
            before: Some(format!("'{}'", removed)),
            after: None,
            description: format!(
                "Value '{}' was removed from the `{}` prop on `{}`",
                removed, prop_name, sym.name
            ),
            is_breaking: true,
            impact: None,
            migration_target: None,
        });
    }

    // Added values (non-breaking, but useful for migration)
    for added in new_literals.difference(&old_literals) {
        changes.push(StructuralChange {
            symbol: format!("{}.{}", sym.name, prop_name),
            qualified_name: format!("{}.{}", sym.qualified_name, prop_name),
            kind: SymbolKind::Property,
            package: sym.package.clone(),
            change_type: StructuralChangeType::Added(ChangeSubject::UnionValue {
                value: added.clone(),
            }),
            before: None,
            after: Some(format!("'{}'", added)),
            description: format!(
                "Value '{}' was added to the `{}` prop on `{}`",
                added, prop_name, sym.name
            ),
            is_breaking: false,
            impact: None,
            migration_target: None,
        });
    }
}

fn diff_enum_member_value<M: Default + Clone>(
    parent: &Symbol<M>,
    old_member: &Symbol<M>,
    new_member: &Symbol<M>,
    changes: &mut Vec<StructuralChange>,
) {
    let old_val = old_member
        .signature
        .as_ref()
        .and_then(|s| s.return_type.as_deref());
    let new_val = new_member
        .signature
        .as_ref()
        .and_then(|s| s.return_type.as_deref());

    if old_val != new_val {
        changes.push(change(
            old_member,
            StructuralChangeType::Changed(ChangeSubject::Member {
                name: old_member.name.clone(),
                kind: SymbolKind::EnumMember,
            }),
            old_val.map(|s| s.to_string()),
            new_val.map(|s| s.to_string()),
            format!(
                "Value of enum member `{}.{}` changed from `{}` to `{}`",
                parent.name,
                old_member.name,
                old_val.unwrap_or("undefined"),
                new_val.unwrap_or("undefined")
            ),
            true,
        ));
    }
}
