//! The `ChangeSubject` enum and collapsed `StructuralChangeTypeV2`.
//!
//! These types represent the target architecture for structural change
//! reporting, collapsing the current 37-variant `StructuralChangeType`
//! into 5 lifecycle variants + a `ChangeSubject` that describes what
//! aspect of a symbol was affected.
//!
//! These types are additive -- the existing `StructuralChangeType` is
//! unchanged and continues to be used by the diff engine until Phase 4.

use super::surface::SymbolKind;
use serde::{Deserialize, Serialize};

/// What aspect of a symbol was affected by a change.
///
/// The parent `StructuralChange` carries the top-level symbol identity
/// (`symbol`, `qualified_name`). The `ChangeSubject` adds the specific
/// sub-element context -- "it was the `email` parameter" or "it was the
/// `readonly` modifier."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChangeSubject {
    /// The symbol itself (added, removed, renamed, relocated).
    Symbol { kind: SymbolKind },

    /// A member of a container (property on interface, method on class,
    /// field on struct, variant on enum).
    Member { name: String, kind: SymbolKind },

    /// A parameter on a function/method.
    Parameter { name: String },

    /// The return type of a function/method.
    ReturnType,

    /// The visibility of a symbol.
    Visibility,

    /// A modifier on a symbol (readonly, abstract, static, accessor kind).
    Modifier { modifier: String },

    /// A generic type parameter.
    TypeParameter { name: String },

    /// The base class (`extends` clause).
    BaseClass,

    /// An interface implementation.
    InterfaceImpl { interface_name: String },

    /// A value in a union/constrained type.
    UnionValue { value: String },
}

/// Collapsed structural change type (target architecture).
///
/// 5 variants instead of 37. The `ChangeSubject` carried inside each
/// variant describes what was affected. `before`/`after` on the parent
/// `StructuralChange` carry the values.
///
/// Named `V2` to coexist with the current `StructuralChangeType` until
/// Phase 4 completes the migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum StructuralChangeTypeV2 {
    Added(ChangeSubject),
    Removed(ChangeSubject),
    Changed(ChangeSubject),
    Renamed {
        from: ChangeSubject,
        to: ChangeSubject,
    },
    Relocated {
        from: ChangeSubject,
        to: ChangeSubject,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_subject_serialization_round_trip() {
        let subjects = vec![
            ChangeSubject::Symbol {
                kind: SymbolKind::Function,
            },
            ChangeSubject::Member {
                name: "onClick".into(),
                kind: SymbolKind::Property,
            },
            ChangeSubject::Parameter {
                name: "email".into(),
            },
            ChangeSubject::ReturnType,
            ChangeSubject::Visibility,
            ChangeSubject::Modifier {
                modifier: "readonly".into(),
            },
            ChangeSubject::TypeParameter { name: "T".into() },
            ChangeSubject::BaseClass,
            ChangeSubject::InterfaceImpl {
                interface_name: "Serializable".into(),
            },
            ChangeSubject::UnionValue {
                value: "primary".into(),
            },
        ];

        for subject in &subjects {
            let json = serde_json::to_string(subject).unwrap();
            let roundtrip: ChangeSubject = serde_json::from_str(&json).unwrap();
            assert_eq!(subject, &roundtrip, "Round-trip failed for {:?}", subject);
        }
    }

    #[test]
    fn change_type_v2_serialization_round_trip() {
        let types = vec![
            StructuralChangeTypeV2::Added(ChangeSubject::Symbol {
                kind: SymbolKind::Interface,
            }),
            StructuralChangeTypeV2::Removed(ChangeSubject::Member {
                name: "variant".into(),
                kind: SymbolKind::Property,
            }),
            StructuralChangeTypeV2::Changed(ChangeSubject::ReturnType),
            StructuralChangeTypeV2::Renamed {
                from: ChangeSubject::Symbol {
                    kind: SymbolKind::Class,
                },
                to: ChangeSubject::Symbol {
                    kind: SymbolKind::Struct,
                },
            },
            StructuralChangeTypeV2::Relocated {
                from: ChangeSubject::Symbol {
                    kind: SymbolKind::Function,
                },
                to: ChangeSubject::Symbol {
                    kind: SymbolKind::Function,
                },
            },
        ];

        for ct in &types {
            let json = serde_json::to_string(ct).unwrap();
            let roundtrip: StructuralChangeTypeV2 = serde_json::from_str(&json).unwrap();
            assert_eq!(ct, &roundtrip, "Round-trip failed for {:?}", ct);
        }
    }

    #[test]
    fn change_subject_json_has_type_tag() {
        let subject = ChangeSubject::Parameter {
            name: "email".into(),
        };
        let json = serde_json::to_string(&subject).unwrap();
        assert!(json.contains(r#""type":"parameter""#), "JSON: {}", json);
    }

    #[test]
    fn struct_symbol_kind_serializes() {
        let subject = ChangeSubject::Symbol {
            kind: SymbolKind::Struct,
        };
        let json = serde_json::to_string(&subject).unwrap();
        assert!(json.contains(r#""struct""#), "JSON: {}", json);
    }
}
