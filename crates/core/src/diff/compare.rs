//! Individual comparison functions for the diff engine.
//!
//! Each function compares a specific aspect of two matched symbols
//! (visibility, modifiers, hierarchy, signatures, members) and emits
//! `StructuralChange` entries for detected differences.

use crate::traits::LanguageSemantics;
use crate::types::{
    Parameter, Signature, StructuralChange, StructuralChangeType, Symbol, SymbolKind,
};
use std::collections::{BTreeSet, HashMap};

use super::helpers::{
    change, kind_label, param_summary, symbol_summary, type_param_summary, visibility_rank,
};
use super::rename::detect_renames;

// ─── Symbol-level diff ───────────────────────────────────────────────────

/// Compare all aspects of two matched symbols and emit changes.
///
/// This is the central dispatch for symbol-level comparison. It calls
/// individual comparison functions for each aspect. Note the mutual
/// recursion with `diff_members` (which calls back into `diff_symbol`
/// for matched member pairs).
pub(super) fn diff_symbol(
    old: &Symbol,
    new: &Symbol,
    changes: &mut Vec<StructuralChange>,
    semantics: &dyn LanguageSemantics,
) {
    diff_visibility(old, new, changes, semantics);
    diff_modifiers(old, new, changes);
    diff_hierarchy(old, new, changes);
    diff_signatures(old, new, changes, semantics);
    diff_members(old, new, changes, semantics);
}

// ─── Visibility diff ─────────────────────────────────────────────────────

fn diff_visibility(
    old: &Symbol,
    new: &Symbol,
    changes: &mut Vec<StructuralChange>,
    semantics: &dyn LanguageSemantics,
) {
    if old.visibility == new.visibility {
        return;
    }

    let old_rank = semantics.visibility_rank(old.visibility);
    let new_rank = semantics.visibility_rank(new.visibility);

    if new_rank < old_rank {
        changes.push(change(
            old,
            StructuralChangeType::VisibilityReduced,
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
            StructuralChangeType::VisibilityIncreased,
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

fn diff_modifiers(old: &Symbol, new: &Symbol, changes: &mut Vec<StructuralChange>) {
    // readonly
    if !old.is_readonly && new.is_readonly {
        changes.push(change(
            old,
            StructuralChangeType::ReadonlyAdded,
            Some("mutable".into()),
            Some("readonly".into()),
            format!("`{}` was made readonly", old.name),
            true,
        ));
    } else if old.is_readonly && !new.is_readonly {
        changes.push(change(
            old,
            StructuralChangeType::ReadonlyRemoved,
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
            StructuralChangeType::AbstractAdded,
            Some("concrete".into()),
            Some("abstract".into()),
            format!("`{}` was made abstract", old.name),
            true,
        ));
    } else if old.is_abstract && !new.is_abstract {
        changes.push(change(
            old,
            StructuralChangeType::AbstractRemoved,
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
            StructuralChangeType::StaticInstanceChanged,
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
            StructuralChangeType::AccessorKindChanged,
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

fn diff_hierarchy(old: &Symbol, new: &Symbol, changes: &mut Vec<StructuralChange>) {
    // extends (base class)
    if old.extends != new.extends {
        changes.push(change(
            old,
            StructuralChangeType::BaseClassChanged,
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
            StructuralChangeType::InterfaceImplementationAdded,
            None,
            Some(added.to_string()),
            format!("`{}` now implements `{}`", old.name, added),
            false,
        ));
    }

    for removed in old_impls.difference(&new_impls) {
        changes.push(change(
            old,
            StructuralChangeType::InterfaceImplementationRemoved,
            Some(removed.to_string()),
            None,
            format!("`{}` no longer implements `{}`", old.name, removed),
            true,
        ));
    }
}

// ─── Signature diff ──────────────────────────────────────────────────────

fn diff_signatures(
    old: &Symbol,
    new: &Symbol,
    changes: &mut Vec<StructuralChange>,
    semantics: &dyn LanguageSemantics,
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
                StructuralChangeType::ReturnTypeChanged,
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

fn diff_parameters(
    sym: &Symbol,
    old_sig: &Signature,
    new_sig: &Signature,
    changes: &mut Vec<StructuralChange>,
    semantics: &dyn LanguageSemantics,
) {
    let old_params = &old_sig.parameters;
    let new_params = &new_sig.parameters;

    let old_non_rest: Vec<&Parameter> = old_params.iter().filter(|p| !p.is_rest).collect();
    let new_non_rest: Vec<&Parameter> = new_params.iter().filter(|p| !p.is_rest).collect();
    let old_rest = old_params.iter().find(|p| p.is_rest);
    let new_rest = new_params.iter().find(|p| p.is_rest);

    // Compare matched parameters (by position)
    let common_len = old_non_rest.len().min(new_non_rest.len());
    for i in 0..common_len {
        let old_p = old_non_rest[i];
        let new_p = new_non_rest[i];

        // Type change
        if old_p.type_annotation != new_p.type_annotation {
            changes.push(change(
                sym,
                StructuralChangeType::ParameterTypeChanged,
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
                StructuralChangeType::ParameterMadeRequired,
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
                StructuralChangeType::ParameterMadeOptional,
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
                StructuralChangeType::ParameterDefaultValueChanged,
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
    for i in common_len..old_non_rest.len() {
        let p = old_non_rest[i];
        changes.push(change(
            sym,
            StructuralChangeType::ParameterRemoved,
            Some(param_summary(p)),
            None,
            format!("Parameter `{}` was removed from `{}`", p.name, sym.name),
            true,
        ));
    }

    // Parameters added (new has more non-rest params than old)
    for i in common_len..new_non_rest.len() {
        let p = new_non_rest[i];
        let is_breaking = !p.optional && !p.has_default;
        changes.push(change(
            sym,
            StructuralChangeType::ParameterAdded,
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
                    StructuralChangeType::ParameterTypeChanged,
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
                StructuralChangeType::RestParameterAdded,
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
                StructuralChangeType::RestParameterRemoved,
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

fn diff_return_type(
    sym: &Symbol,
    old_sig: &Signature,
    new_sig: &Signature,
    changes: &mut Vec<StructuralChange>,
    semantics: &dyn LanguageSemantics,
) {
    if old_sig.return_type == new_sig.return_type {
        return;
    }

    let old_ret = old_sig.return_type.as_deref().unwrap_or("void");
    let new_ret = new_sig.return_type.as_deref().unwrap_or("void");

    let old_is_promise = old_ret.starts_with("Promise<");
    let new_is_promise = new_ret.starts_with("Promise<");

    if !old_is_promise && new_is_promise {
        changes.push(change(
            sym,
            StructuralChangeType::MadeAsync,
            old_sig.return_type.clone(),
            new_sig.return_type.clone(),
            format!(
                "`{}` was made async (return type wrapped in Promise)",
                sym.name
            ),
            true,
        ));
    } else if old_is_promise && !new_is_promise {
        changes.push(change(
            sym,
            StructuralChangeType::MadeSync,
            old_sig.return_type.clone(),
            new_sig.return_type.clone(),
            format!("`{}` was made sync (Promise wrapper removed)", sym.name),
            true,
        ));
    } else {
        changes.push(change(
            sym,
            StructuralChangeType::ReturnTypeChanged,
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

fn diff_type_parameters(
    sym: &Symbol,
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
                    StructuralChangeType::TypeParameterReordered,
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
                StructuralChangeType::TypeParameterConstraintChanged,
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
                StructuralChangeType::TypeParameterDefaultChanged,
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
    for i in common_len..old_tps.len() {
        changes.push(change(
            sym,
            StructuralChangeType::TypeParameterRemoved,
            Some(type_param_summary(&old_tps[i])),
            None,
            format!(
                "Type parameter `{}` was removed from `{}`",
                old_tps[i].name, sym.name
            ),
            true,
        ));
    }

    // Type parameters added
    for i in common_len..new_tps.len() {
        let tp = &new_tps[i];
        let is_breaking = tp.default.is_none();
        changes.push(change(
            sym,
            StructuralChangeType::TypeParameterAdded,
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
pub(super) fn diff_members(
    old: &Symbol,
    new: &Symbol,
    changes: &mut Vec<StructuralChange>,
    semantics: &dyn LanguageSemantics,
) {
    if old.members.is_empty() && new.members.is_empty() {
        return;
    }

    let old_map: HashMap<&str, &Symbol> =
        old.members.iter().map(|m| (m.name.as_str(), m)).collect();
    let new_map: HashMap<&str, &Symbol> =
        new.members.iter().map(|m| (m.name.as_str(), m)).collect();

    // Collect removed and added members
    let removed: Vec<&Symbol> = old
        .members
        .iter()
        .filter(|m| !new_map.contains_key(m.name.as_str()))
        .collect();
    let added: Vec<&Symbol> = new
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
            kind: format!("{:?}", rm.old.kind),
            change_type: StructuralChangeType::PropertyRenamed,
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
                StructuralChangeType::EnumMemberRemoved,
                format!(
                    "Enum member `{}` was removed from `{}`",
                    member.name, old.name
                ),
                true,
            ),
            _ => (
                StructuralChangeType::PropertyRemoved,
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
                StructuralChangeType::EnumMemberAdded,
                format!("Enum member `{}` was added to `{}`", member.name, new.name),
            ),
            _ => (
                StructuralChangeType::PropertyAdded,
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

/// Parse a TypeScript string literal union type into its individual members.
///
/// Handles: `'primary' | 'secondary' | 'tertiary'` → `{"primary", "secondary", "tertiary"}`
///
/// Also handles mixed unions like `'primary' | ButtonVariant | undefined` by
/// extracting only the string literal members (quoted with single or double quotes).
///
/// This is generic — works for any TypeScript union of string literals.
fn parse_union_literals(type_str: &str) -> Option<BTreeSet<String>> {
    // Quick check: must contain at least one string literal (quoted value)
    if !type_str.contains('\'') && !type_str.contains('"') {
        return None;
    }

    // Must look like a union (contains |)
    if !type_str.contains('|') {
        // Single literal — still valid but not a union to diff
        return None;
    }

    let mut literals = BTreeSet::new();

    for part in type_str.split('|') {
        let trimmed = part.trim();
        // Extract string literal values (single or double quoted)
        if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
            || (trimmed.starts_with('"') && trimmed.ends_with('"'))
        {
            let value = &trimmed[1..trimmed.len() - 1];
            if !value.is_empty() {
                literals.insert(value.to_string());
            }
        }
        // Skip non-literal members (type references, undefined, null, etc.)
    }

    // Only return if we found at least 2 literal members
    // (a single literal in a union with types isn't a value-level concern)
    if literals.len() >= 2 {
        Some(literals)
    } else {
        None
    }
}

/// Emit per-member union literal value changes.
///
/// When a property's type changes from `'a' | 'b' | 'c'` to `'a' | 'd'`,
/// emits:
///   - `UnionMemberRemoved` for `'b'` and `'c'`
///   - `UnionMemberAdded` for `'d'`
///
/// The parent symbol provides context (e.g., `Button.variant`).
fn diff_union_literals(
    sym: &Symbol,
    prop_name: &str,
    old_type: &str,
    new_type: &str,
    changes: &mut Vec<StructuralChange>,
    semantics: &dyn LanguageSemantics,
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
            kind: "property_value".to_string(),
            change_type: StructuralChangeType::UnionMemberRemoved,
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
            kind: "property_value".to_string(),
            change_type: StructuralChangeType::UnionMemberAdded,
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

fn diff_enum_member_value(
    parent: &Symbol,
    old_member: &Symbol,
    new_member: &Symbol,
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
            StructuralChangeType::EnumMemberValueChanged,
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
