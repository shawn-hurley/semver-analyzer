//! Shared utility functions for the diff engine.

use crate::types::{
    Parameter, StructuralChange, StructuralChangeType, Symbol, SymbolKind, TypeParameter,
    Visibility,
};
use std::fmt::Write;

/// Check if a symbol represents a star re-export (`export * from './module'`).
///
/// These symbols have name `"*"` and represent barrel-file re-export directives
/// rather than actual API symbols. They are filtered from diffing because:
/// - Multiple `export *` in the same file share the same qualified_name
/// - The individual symbols they re-export are tracked via their source files
/// - Star re-export changes are noise in the output (v2 harness excludes them)
pub(super) fn is_star_reexport<M: Default + Clone>(sym: &Symbol<M>) -> bool {
    sym.name == "*"
}

/// Create a StructuralChange from common fields.
pub(super) fn change<M: Default + Clone>(
    sym: &Symbol<M>,
    change_type: StructuralChangeType,
    before: Option<String>,
    after: Option<String>,
    description: String,
    is_breaking: bool,
) -> StructuralChange {
    StructuralChange {
        symbol: sym.name.clone(),
        qualified_name: sym.qualified_name.clone(),
        kind: sym.kind,
        package: sym.package.clone(),
        change_type,
        before,
        after,
        description,
        is_breaking,
        impact: None,
        migration_target: None,
    }
}

/// Human-readable label for a symbol kind.
pub(super) fn kind_label(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Class => "class",
        SymbolKind::Struct => "struct",
        SymbolKind::Interface => "interface",
        SymbolKind::TypeAlias => "type alias",
        SymbolKind::Enum => "enum",
        SymbolKind::EnumMember => "enum member",
        SymbolKind::Constant => "constant",
        SymbolKind::Variable => "variable",
        SymbolKind::Property => "property",
        SymbolKind::Constructor => "constructor",
        SymbolKind::GetAccessor => "getter",
        SymbolKind::SetAccessor => "setter",
        SymbolKind::Namespace => "namespace",
    }
}

/// Brief summary of a symbol for before/after display.
pub(super) fn symbol_summary<M: Default + Clone>(sym: &Symbol<M>) -> String {
    let mut s = format!("{}: {}", kind_label(sym.kind), sym.name);
    if let Some(sig) = &sym.signature {
        if let Some(ret) = &sig.return_type {
            write!(s, ": {}", ret).unwrap();
        }
    }
    s
}

/// Brief summary of a parameter.
pub(super) fn param_summary(p: &Parameter) -> String {
    let mut s = p.name.clone();
    if p.optional {
        s.push('?');
    }
    if let Some(ta) = &p.type_annotation {
        write!(s, ": {}", ta).unwrap();
    }
    if let Some(dv) = &p.default_value {
        write!(s, " = {}", dv).unwrap();
    }
    if p.is_variadic {
        s = format!("...{}", s);
    }
    s
}

/// Brief summary of a type parameter.
pub(super) fn type_param_summary(tp: &TypeParameter) -> String {
    let mut s = tp.name.clone();
    if let Some(c) = &tp.constraint {
        write!(s, " extends {}", c).unwrap();
    }
    if let Some(d) = &tp.default {
        write!(s, " = {}", d).unwrap();
    }
    s
}

/// Numeric rank for visibility levels (higher = more visible).
///
/// NOTE: This hardcoded ranking will move to `LanguageSemantics::visibility_rank`
/// in Phase 3, since the ordering differs by language (e.g., Java's `protected`
/// is more visible than package-private).
pub(super) fn visibility_rank(v: Visibility) -> u8 {
    match v {
        Visibility::Private => 0,
        Visibility::Internal => 1,
        Visibility::Protected => 2,
        Visibility::Public => 3,
        Visibility::Exported => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_rank_ordering() {
        assert!(visibility_rank(Visibility::Private) < visibility_rank(Visibility::Internal));
        assert!(visibility_rank(Visibility::Internal) < visibility_rank(Visibility::Protected));
        assert!(visibility_rank(Visibility::Protected) < visibility_rank(Visibility::Public));
        assert!(visibility_rank(Visibility::Public) < visibility_rank(Visibility::Exported));
    }

    #[test]
    fn kind_label_includes_struct() {
        assert_eq!(kind_label(SymbolKind::Struct), "struct");
    }
}
