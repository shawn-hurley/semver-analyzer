//! Shared test helpers for baseline integration tests.
//!
//! Provides a normalized representation of diff changes that is
//! independent of internal enum variant names. This representation
//! survives the refactoring from 37 StructuralChangeType variants
//! to 5 + ChangeSubject.

use semver_analyzer_core::*;
use serde::Serialize;

/// Semantic representation of a structural change, independent of
/// internal enum types. Used for snapshot comparison across the
/// refactoring.
#[derive(Debug, Serialize)]
pub struct NormalizedChange {
    pub symbol: String,
    pub qualified_name: String,
    pub kind: String,
    pub change_type: String,
    pub is_breaking: bool,
    pub description: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub has_migration_target: bool,
}

impl From<&StructuralChange> for NormalizedChange {
    fn from(c: &StructuralChange) -> Self {
        NormalizedChange {
            symbol: c.symbol.clone(),
            qualified_name: c.qualified_name.clone(),
            kind: c.kind.clone(),
            change_type: format!("{:?}", c.change_type),
            is_breaking: c.is_breaking,
            description: c.description.clone(),
            before: c.before.clone(),
            after: c.after.clone(),
            has_migration_target: c.migration_target.is_some(),
        }
    }
}

/// Convert a list of structural changes to normalized form for snapshotting.
pub fn normalize(changes: &[StructuralChange]) -> Vec<NormalizedChange> {
    changes.iter().map(NormalizedChange::from).collect()
}

// ── API Surface construction helpers ──────────────────────────────────
// Mirrors the helpers in core/diff/tests.rs but accessible from
// integration tests.

pub fn sym(name: &str, kind: SymbolKind) -> Symbol {
    Symbol::new(name, name, kind, Visibility::Exported, "test.d.ts", 1)
}

pub fn func(name: &str, params: Vec<Parameter>, ret: &str) -> Symbol {
    let mut s = sym(name, SymbolKind::Function);
    s.signature = Some(Signature {
        parameters: params,
        return_type: Some(ret.to_string()),
        type_parameters: Vec::new(),
        is_async: false,
    });
    s
}

pub fn param(name: &str, ty: &str) -> Parameter {
    Parameter {
        name: name.to_string(),
        type_annotation: Some(ty.to_string()),
        optional: false,
        has_default: false,
        default_value: None,
        is_rest: false,
    }
}

pub fn opt_param(name: &str, ty: &str) -> Parameter {
    Parameter {
        name: name.to_string(),
        type_annotation: Some(ty.to_string()),
        optional: true,
        has_default: false,
        default_value: None,
        is_rest: false,
    }
}

pub fn rest_param(name: &str, ty: &str) -> Parameter {
    Parameter {
        name: name.to_string(),
        type_annotation: Some(ty.to_string()),
        optional: false,
        has_default: false,
        default_value: None,
        is_rest: true,
    }
}

pub fn surface(symbols: Vec<Symbol>) -> ApiSurface {
    ApiSurface { symbols }
}

/// Convenience: create a property symbol with a type annotation (stored in signature.return_type).
pub fn mk_prop(name: &str, ty: &str) -> Symbol {
    let mut p = sym(name, SymbolKind::Property);
    p.signature = Some(Signature {
        parameters: vec![],
        return_type: Some(ty.to_string()),
        type_parameters: vec![],
        is_async: false,
    });
    p
}

/// Create an enum member with a value.
pub fn enum_member(name: &str, value: &str) -> Symbol {
    let mut m = sym(name, SymbolKind::EnumMember);
    m.signature = Some(Signature {
        parameters: vec![],
        return_type: Some(value.to_string()),
        type_parameters: vec![],
        is_async: false,
    });
    m
}

/// Create an interface with named property members.
pub fn make_interface(name: &str, file: &str, members: &[&str]) -> Symbol {
    let mut s = Symbol::new(
        name,
        &format!("{}.{}", file, name),
        SymbolKind::Interface,
        Visibility::Exported,
        file,
        1,
    );
    for &member_name in members {
        s.members.push(Symbol::new(
            member_name,
            &format!("{}.{}.{}", file, name, member_name),
            SymbolKind::Property,
            Visibility::Public,
            file,
            1,
        ));
    }
    s
}

// ── Normalized types for manifest changes ───────────────────────────

/// Normalized manifest change for snapshotting.
#[derive(Debug, Serialize)]
pub struct NormalizedManifestChange {
    pub field: String,
    pub change_type: String,
    pub is_breaking: bool,
    pub description: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

impl From<&ManifestChange> for NormalizedManifestChange {
    fn from(c: &ManifestChange) -> Self {
        NormalizedManifestChange {
            field: c.field.clone(),
            change_type: format!("{:?}", c.change_type),
            is_breaking: c.is_breaking,
            description: c.description.clone(),
            before: c.before.clone(),
            after: c.after.clone(),
        }
    }
}

pub fn normalize_manifest(changes: &[ManifestChange]) -> Vec<NormalizedManifestChange> {
    changes.iter().map(NormalizedManifestChange::from).collect()
}

// ── Normalized types for behavioral changes (JSX/CSS) ───────────────

/// Normalized behavioral change for snapshotting.
#[derive(Debug, Serialize)]
pub struct NormalizedBehavioralChange {
    pub symbol: String,
    pub category: String,
    pub description: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

impl From<&semver_analyzer_core::JsxChange> for NormalizedBehavioralChange {
    fn from(c: &semver_analyzer_core::JsxChange) -> Self {
        NormalizedBehavioralChange {
            symbol: c.symbol.clone(),
            category: format!("{:?}", c.category),
            description: c.description.clone(),
            before: c.before.clone(),
            after: c.after.clone(),
        }
    }
}

pub fn normalize_jsx(
    changes: &[semver_analyzer_core::JsxChange],
) -> Vec<NormalizedBehavioralChange> {
    changes
        .iter()
        .map(NormalizedBehavioralChange::from)
        .collect()
}
