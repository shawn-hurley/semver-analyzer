//! Tests for the diff engine.
//!
//! All tests exercise the public `diff_surfaces` API, which in turn
//! exercises the internal comparison, rename detection, and helper functions.

use super::*;
use crate::types::*;

fn sym(name: &str, kind: SymbolKind) -> Symbol {
    Symbol::new(name, name, kind, Visibility::Exported, "test.d.ts", 1)
}

fn func(name: &str, params: Vec<Parameter>, ret: &str) -> Symbol {
    let mut s = sym(name, SymbolKind::Function);
    s.signature = Some(Signature {
        parameters: params,
        return_type: Some(ret.to_string()),
        type_parameters: Vec::new(),
        is_async: false,
    });
    s
}

fn param(name: &str, ty: &str) -> Parameter {
    Parameter {
        name: name.to_string(),
        type_annotation: Some(ty.to_string()),
        optional: false,
        has_default: false,
        default_value: None,
        is_variadic: false,
    }
}

fn opt_param(name: &str, ty: &str) -> Parameter {
    Parameter {
        name: name.to_string(),
        type_annotation: Some(ty.to_string()),
        optional: true,
        has_default: false,
        default_value: None,
        is_variadic: false,
    }
}

fn rest_param(name: &str, ty: &str) -> Parameter {
    Parameter {
        name: name.to_string(),
        type_annotation: Some(ty.to_string()),
        optional: false,
        has_default: false,
        default_value: None,
        is_variadic: true,
    }
}

fn surface(symbols: Vec<Symbol>) -> ApiSurface {
    ApiSurface { symbols }
}

fn find_change<'a>(
    changes: &'a [StructuralChange],
    predicate: impl Fn(&StructuralChangeType) -> bool,
) -> &'a StructuralChange {
    changes
        .iter()
        .find(|c| predicate(&c.change_type))
        .unwrap_or_else(|| {
            panic!(
                "No matching change found in: {:?}",
                changes.iter().map(|c| &c.change_type).collect::<Vec<_>>()
            )
        })
}

fn has_change(
    changes: &[StructuralChange],
    predicate: impl Fn(&StructuralChangeType) -> bool,
) -> bool {
    changes.iter().any(|c| predicate(&c.change_type))
}

// ── Symbol-level ─────────────────────────────────────────────────

#[test]
fn detect_symbol_removed() {
    let old = surface(vec![func("greet", vec![param("name", "string")], "void")]);
    let new = surface(vec![]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Removed(ChangeSubject::Symbol { .. })
        )
    });
    assert_eq!(c.symbol, "greet");
    assert!(c.is_breaking);
}

#[test]
fn detect_symbol_added() {
    let old = surface(vec![]);
    let new = surface(vec![func("greet", vec![param("name", "string")], "void")]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Added(ChangeSubject::Symbol { .. })
        )
    });
    assert_eq!(c.symbol, "greet");
    assert!(!c.is_breaking);
}

#[test]
fn no_changes_for_identical_surfaces() {
    let s = surface(vec![func("greet", vec![param("name", "string")], "void")]);
    let changes = diff_surfaces(&s, &s);
    assert!(changes.is_empty());
}

// ── Parameter changes ────────────────────────────────────────────

#[test]
fn detect_required_parameter_added() {
    let old = surface(vec![func("f", vec![param("a", "string")], "void")]);
    let new = surface(vec![func(
        "f",
        vec![param("a", "string"), param("b", "number")],
        "void",
    )]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Added(ChangeSubject::Parameter { .. })
        )
    });
    assert!(c.is_breaking);
    assert!(c.description.contains("Required"));
}

#[test]
fn detect_optional_parameter_added() {
    let old = surface(vec![func("f", vec![param("a", "string")], "void")]);
    let new = surface(vec![func(
        "f",
        vec![param("a", "string"), opt_param("b", "number")],
        "void",
    )]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Added(ChangeSubject::Parameter { .. })
        )
    });
    assert!(!c.is_breaking);
    assert!(c.description.contains("Optional"));
}

#[test]
fn detect_parameter_removed() {
    let old = surface(vec![func(
        "f",
        vec![param("a", "string"), param("b", "number")],
        "void",
    )]);
    let new = surface(vec![func("f", vec![param("a", "string")], "void")]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Removed(ChangeSubject::Parameter { .. })
        )
    });
    assert!(c.is_breaking);
}

#[test]
fn detect_parameter_type_changed() {
    let old = surface(vec![func("f", vec![param("a", "string")], "void")]);
    let new = surface(vec![func("f", vec![param("a", "number")], "void")]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Changed(ChangeSubject::Parameter { .. })
        )
    });
    assert!(c.is_breaking);
    assert_eq!(c.before.as_deref(), Some("string"));
    assert_eq!(c.after.as_deref(), Some("number"));
}

#[test]
fn detect_parameter_made_required() {
    let old = surface(vec![func("f", vec![opt_param("a", "string")], "void")]);
    let new = surface(vec![func("f", vec![param("a", "string")], "void")]);
    let changes = diff_surfaces(&old, &new);

    assert!(has_change(&changes, |ct| matches!(
        ct,
        StructuralChangeType::Changed(ChangeSubject::Parameter { .. })
    )));
}

#[test]
fn detect_parameter_made_optional() {
    let old = surface(vec![func("f", vec![param("a", "string")], "void")]);
    let new = surface(vec![func("f", vec![opt_param("a", "string")], "void")]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Changed(ChangeSubject::Parameter { .. })
        )
    });
    assert!(!c.is_breaking);
}

#[test]
fn detect_rest_parameter_added() {
    let old = surface(vec![func("f", vec![param("a", "string")], "void")]);
    let new = surface(vec![func(
        "f",
        vec![param("a", "string"), rest_param("args", "unknown[]")],
        "void",
    )]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Added(ChangeSubject::Parameter { .. })
        )
    });
    assert!(!c.is_breaking);
}

#[test]
fn detect_rest_parameter_removed() {
    let old = surface(vec![func(
        "f",
        vec![param("a", "string"), rest_param("args", "unknown[]")],
        "void",
    )]);
    let new = surface(vec![func("f", vec![param("a", "string")], "void")]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Removed(ChangeSubject::Parameter { .. })
        )
    });
    assert!(c.is_breaking);
}

// ── Return type changes ──────────────────────────────────────────

#[test]
fn detect_return_type_changed() {
    let old = surface(vec![func("f", vec![], "string")]);
    let new = surface(vec![func("f", vec![], "number")]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(ct, StructuralChangeType::Changed(ChangeSubject::ReturnType))
    });
    assert!(c.is_breaking);
}

#[test]
fn detect_made_async() {
    let old = surface(vec![func("f", vec![], "string")]);
    let new = surface(vec![func("f", vec![], "Promise<string>")]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(ct, StructuralChangeType::Changed(ChangeSubject::ReturnType))
    });
    assert!(c.is_breaking);
}

#[test]
fn detect_made_sync() {
    let old = surface(vec![func("f", vec![], "Promise<string>")]);
    let new = surface(vec![func("f", vec![], "string")]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(ct, StructuralChangeType::Changed(ChangeSubject::ReturnType))
    });
    assert!(c.is_breaking);
}

// ── Visibility changes ───────────────────────────────────────────

#[test]
fn detect_visibility_reduced() {
    let old = surface(vec![{
        let mut s = func("f", vec![], "void");
        s.visibility = Visibility::Exported;
        s
    }]);
    let new = surface(vec![{
        let mut s = func("f", vec![], "void");
        s.visibility = Visibility::Internal;
        s
    }]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(ct, StructuralChangeType::Changed(ChangeSubject::Visibility))
    });
    assert!(c.is_breaking);
}

#[test]
fn detect_visibility_increased() {
    let old = surface(vec![{
        let mut s = func("f", vec![], "void");
        s.visibility = Visibility::Internal;
        s
    }]);
    let new = surface(vec![{
        let mut s = func("f", vec![], "void");
        s.visibility = Visibility::Exported;
        s
    }]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(ct, StructuralChangeType::Changed(ChangeSubject::Visibility))
    });
    assert!(!c.is_breaking);
}

// ── Modifier changes ─────────────────────────────────────────────

#[test]
fn detect_readonly_added() {
    let old = surface(vec![sym("prop", SymbolKind::Property)]);
    let new = surface(vec![{
        let mut s = sym("prop", SymbolKind::Property);
        s.is_readonly = true;
        s
    }]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Added(ChangeSubject::Modifier { .. })
        )
    });
    assert!(c.is_breaking);
}

#[test]
fn detect_readonly_removed() {
    let old = surface(vec![{
        let mut s = sym("prop", SymbolKind::Property);
        s.is_readonly = true;
        s
    }]);
    let new = surface(vec![sym("prop", SymbolKind::Property)]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Removed(ChangeSubject::Modifier { .. })
        )
    });
    assert!(!c.is_breaking);
}

#[test]
fn detect_abstract_added() {
    let old = surface(vec![sym("validate", SymbolKind::Method)]);
    let new = surface(vec![{
        let mut s = sym("validate", SymbolKind::Method);
        s.is_abstract = true;
        s
    }]);
    let changes = diff_surfaces(&old, &new);

    assert!(has_change(&changes, |ct| matches!(
        ct,
        StructuralChangeType::Added(ChangeSubject::Modifier { .. })
    )));
}

#[test]
fn detect_static_instance_changed() {
    let old = surface(vec![sym("method", SymbolKind::Method)]);
    let new = surface(vec![{
        let mut s = sym("method", SymbolKind::Method);
        s.is_static = true;
        s
    }]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Changed(ChangeSubject::Modifier { .. })
        )
    });
    assert!(c.is_breaking);
}

// ── Class hierarchy ──────────────────────────────────────────────

#[test]
fn detect_base_class_changed() {
    let old = surface(vec![{
        let mut s = sym("MyClass", SymbolKind::Class);
        s.extends = Some("BaseA".into());
        s
    }]);
    let new = surface(vec![{
        let mut s = sym("MyClass", SymbolKind::Class);
        s.extends = Some("BaseB".into());
        s
    }]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(ct, StructuralChangeType::Changed(ChangeSubject::BaseClass))
    });
    assert!(c.is_breaking);
}

#[test]
fn detect_interface_added() {
    let old = surface(vec![{
        let mut s = sym("MyClass", SymbolKind::Class);
        s.implements = vec!["Foo".into()];
        s
    }]);
    let new = surface(vec![{
        let mut s = sym("MyClass", SymbolKind::Class);
        s.implements = vec!["Foo".into(), "Bar".into()];
        s
    }]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Added(ChangeSubject::InterfaceImpl { .. })
        )
    });
    assert!(!c.is_breaking);
}

#[test]
fn detect_interface_removed() {
    let old = surface(vec![{
        let mut s = sym("MyClass", SymbolKind::Class);
        s.implements = vec!["Foo".into(), "Bar".into()];
        s
    }]);
    let new = surface(vec![{
        let mut s = sym("MyClass", SymbolKind::Class);
        s.implements = vec!["Foo".into()];
        s
    }]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Removed(ChangeSubject::InterfaceImpl { .. })
        )
    });
    assert!(c.is_breaking);
}

// ── Type parameter changes ───────────────────────────────────────

#[test]
fn detect_type_parameter_added_required() {
    let old = surface(vec![{
        let mut s = func("f", vec![], "void");
        s.signature.as_mut().unwrap().type_parameters = vec![TypeParameter {
            name: "T".into(),
            constraint: None,
            default: None,
        }];
        s
    }]);
    let new = surface(vec![{
        let mut s = func("f", vec![], "void");
        s.signature.as_mut().unwrap().type_parameters = vec![
            TypeParameter {
                name: "T".into(),
                constraint: None,
                default: None,
            },
            TypeParameter {
                name: "U".into(),
                constraint: None,
                default: None,
            },
        ];
        s
    }]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Added(ChangeSubject::TypeParameter { .. })
        )
    });
    assert!(c.is_breaking);
}

#[test]
fn detect_type_parameter_added_with_default() {
    let old = surface(vec![{
        let mut s = func("f", vec![], "void");
        s.signature.as_mut().unwrap().type_parameters = vec![TypeParameter {
            name: "T".into(),
            constraint: None,
            default: None,
        }];
        s
    }]);
    let new = surface(vec![{
        let mut s = func("f", vec![], "void");
        s.signature.as_mut().unwrap().type_parameters = vec![
            TypeParameter {
                name: "T".into(),
                constraint: None,
                default: None,
            },
            TypeParameter {
                name: "U".into(),
                constraint: None,
                default: Some("unknown".into()),
            },
        ];
        s
    }]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Added(ChangeSubject::TypeParameter { .. })
        )
    });
    assert!(!c.is_breaking);
}

#[test]
fn detect_type_parameter_removed() {
    let old = surface(vec![{
        let mut s = func("f", vec![], "void");
        s.signature.as_mut().unwrap().type_parameters = vec![
            TypeParameter {
                name: "T".into(),
                constraint: None,
                default: None,
            },
            TypeParameter {
                name: "U".into(),
                constraint: None,
                default: None,
            },
        ];
        s
    }]);
    let new = surface(vec![{
        let mut s = func("f", vec![], "void");
        s.signature.as_mut().unwrap().type_parameters = vec![TypeParameter {
            name: "T".into(),
            constraint: None,
            default: None,
        }];
        s
    }]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Removed(ChangeSubject::TypeParameter { .. })
        )
    });
    assert!(c.is_breaking);
}

#[test]
fn detect_type_parameter_constraint_changed() {
    let old = surface(vec![{
        let mut s = func("f", vec![], "void");
        s.signature.as_mut().unwrap().type_parameters = vec![TypeParameter {
            name: "T".into(),
            constraint: Some("object".into()),
            default: None,
        }];
        s
    }]);
    let new = surface(vec![{
        let mut s = func("f", vec![], "void");
        s.signature.as_mut().unwrap().type_parameters = vec![TypeParameter {
            name: "T".into(),
            constraint: Some("Record<string, unknown>".into()),
            default: None,
        }];
        s
    }]);
    let changes = diff_surfaces(&old, &new);

    assert!(has_change(&changes, |ct| matches!(
        ct,
        StructuralChangeType::Changed(ChangeSubject::TypeParameter { .. })
    )));
}

// ── Enum member changes ──────────────────────────────────────────

#[test]
fn detect_enum_member_added() {
    let old = surface(vec![{
        let mut e = sym("Color", SymbolKind::Enum);
        let mut red = sym("Red", SymbolKind::EnumMember);
        red.signature = Some(Signature {
            parameters: vec![],
            return_type: Some("0".into()),
            type_parameters: vec![],
            is_async: false,
        });
        e.members = vec![red];
        e
    }]);
    let new = surface(vec![{
        let mut e = sym("Color", SymbolKind::Enum);
        let mut red = sym("Red", SymbolKind::EnumMember);
        red.signature = Some(Signature {
            parameters: vec![],
            return_type: Some("0".into()),
            type_parameters: vec![],
            is_async: false,
        });
        let mut green = sym("Green", SymbolKind::EnumMember);
        green.signature = Some(Signature {
            parameters: vec![],
            return_type: Some("1".into()),
            type_parameters: vec![],
            is_async: false,
        });
        e.members = vec![red, green];
        e
    }]);
    let changes = diff_surfaces(&old, &new);

    assert!(has_change(&changes, |ct| matches!(
        ct,
        StructuralChangeType::Added(ChangeSubject::Member {
            kind: SymbolKind::EnumMember,
            ..
        })
    )));
}

#[test]
fn detect_enum_member_removed() {
    let old = surface(vec![{
        let mut e = sym("Color", SymbolKind::Enum);
        e.members = vec![
            sym("Red", SymbolKind::EnumMember),
            sym("Green", SymbolKind::EnumMember),
        ];
        e
    }]);
    let new = surface(vec![{
        let mut e = sym("Color", SymbolKind::Enum);
        e.members = vec![sym("Red", SymbolKind::EnumMember)];
        e
    }]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Removed(ChangeSubject::Member {
                kind: SymbolKind::EnumMember,
                ..
            })
        )
    });
    assert!(c.is_breaking);
}

#[test]
fn detect_enum_member_value_changed() {
    let mk = |val: &str| {
        let mut m = sym("Red", SymbolKind::EnumMember);
        m.signature = Some(Signature {
            parameters: vec![],
            return_type: Some(val.into()),
            type_parameters: vec![],
            is_async: false,
        });
        m
    };
    let old = surface(vec![{
        let mut e = sym("Color", SymbolKind::Enum);
        e.members = vec![mk("0")];
        e
    }]);
    let new = surface(vec![{
        let mut e = sym("Color", SymbolKind::Enum);
        e.members = vec![mk("1")];
        e
    }]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Changed(ChangeSubject::Member {
                kind: SymbolKind::EnumMember,
                ..
            })
        )
    });
    assert!(c.is_breaking);
}

// ── Interface member changes ─────────────────────────────────────

#[test]
fn detect_interface_property_removed() {
    let old = surface(vec![{
        let mut i = sym("Options", SymbolKind::Interface);
        i.members = vec![
            sym("name", SymbolKind::Property),
            sym("age", SymbolKind::Property),
        ];
        i
    }]);
    let new = surface(vec![{
        let mut i = sym("Options", SymbolKind::Interface);
        i.members = vec![sym("name", SymbolKind::Property)];
        i
    }]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Removed(ChangeSubject::Member { .. })
        )
    });
    assert!(c.is_breaking);
    assert_eq!(c.symbol, "age");
}

#[test]
fn detect_interface_property_added() {
    let old = surface(vec![{
        let mut i = sym("Options", SymbolKind::Interface);
        i.members = vec![sym("name", SymbolKind::Property)];
        i
    }]);
    let new = surface(vec![{
        let mut i = sym("Options", SymbolKind::Interface);
        i.members = vec![
            sym("name", SymbolKind::Property),
            sym("age", SymbolKind::Property),
        ];
        i
    }]);
    let changes = diff_surfaces(&old, &new);

    let c = find_change(&changes, |ct| {
        matches!(
            ct,
            StructuralChangeType::Added(ChangeSubject::Member { .. })
        )
    });
    assert_eq!(c.symbol, "age");
    // With MinimalSemantics, member additions are never breaking.
    // Language-specific breaking rules (e.g., TS required interface members)
    // are tested in the language crate.
    assert!(!c.is_breaking);
}

// ── Class member changes ─────────────────────────────────────────

#[test]
fn detect_class_method_return_type_changed() {
    let mk_method = |ret: &str| {
        let mut m = sym("getUser", SymbolKind::Method);
        m.signature = Some(Signature {
            parameters: vec![param("id", "string")],
            return_type: Some(ret.into()),
            type_parameters: vec![],
            is_async: false,
        });
        m
    };
    let old = surface(vec![{
        let mut c = sym("UserService", SymbolKind::Class);
        c.members = vec![mk_method("User")];
        c
    }]);
    let new = surface(vec![{
        let mut c = sym("UserService", SymbolKind::Class);
        c.members = vec![mk_method("Promise<User>")];
        c
    }]);
    let changes = diff_surfaces(&old, &new);

    assert!(has_change(&changes, |ct| matches!(
        ct,
        StructuralChangeType::Changed(ChangeSubject::ReturnType)
    )));
}

// ── Rename detection ────────────────────────────────────────────

#[test]
fn detect_property_renamed() {
    // isActive?: boolean removed, isClicked?: boolean added → rename
    let mk_prop = |name: &str| {
        let mut p = sym(name, SymbolKind::Property);
        p.signature = Some(Signature {
            parameters: vec![],
            return_type: Some("boolean".into()),
            type_parameters: vec![],
            is_async: false,
        });
        p
    };

    let old = surface(vec![{
        let mut i = sym("ButtonProps", SymbolKind::Interface);
        i.members = vec![mk_prop("isActive"), mk_prop("variant")];
        i
    }]);
    let new = surface(vec![{
        let mut i = sym("ButtonProps", SymbolKind::Interface);
        i.members = vec![mk_prop("isClicked"), mk_prop("variant")];
        i
    }]);
    let changes = diff_surfaces(&old, &new);

    let rename = changes
        .iter()
        .find(|c| {
            matches!(
                &c.change_type,
                StructuralChangeType::Renamed {
                    from: ChangeSubject::Member { .. },
                    ..
                }
            )
        })
        .expect("Should detect rename");
    assert_eq!(rename.before.as_deref(), Some("isActive"));
    assert_eq!(rename.after.as_deref(), Some("isClicked"));
    assert!(rename.is_breaking);
    assert!(rename.description.contains("renamed"));

    // Should NOT also have PropertyRemoved or PropertyAdded for these
    assert!(
        !changes.iter().any(|c| matches!(
            c.change_type,
            StructuralChangeType::Removed(ChangeSubject::Member { .. })
        ) && c.symbol == "isActive"),
        "Renamed prop should not also appear as removed"
    );
    assert!(
        !changes.iter().any(|c| matches!(
            c.change_type,
            StructuralChangeType::Added(ChangeSubject::Member { .. })
        ) && c.symbol == "isClicked"),
        "Renamed prop should not also appear as added"
    );
}

#[test]
fn detect_property_renamed_with_suffix_match() {
    // chipGroupContentRef → labelGroupContentRef (share "GroupContentRef")
    let mk_prop = |name: &str| {
        let mut p = sym(name, SymbolKind::Property);
        p.signature = Some(Signature {
            parameters: vec![],
            return_type: Some("RefObject<HTMLDivElement>".into()),
            type_parameters: vec![],
            is_async: false,
        });
        p
    };

    let old = surface(vec![{
        let mut i = sym("ToolbarContextProps", SymbolKind::Interface);
        i.members = vec![mk_prop("chipGroupContentRef")];
        i
    }]);
    let new = surface(vec![{
        let mut i = sym("ToolbarContextProps", SymbolKind::Interface);
        i.members = vec![mk_prop("labelGroupContentRef")];
        i
    }]);
    let changes = diff_surfaces(&old, &new);

    assert!(
        has_change(&changes, |ct| matches!(
            ct,
            StructuralChangeType::Renamed {
                from: ChangeSubject::Member { .. },
                ..
            }
        )),
        "Should detect rename via suffix similarity"
    );
}

#[test]
fn no_rename_for_different_types() {
    // removed: isActive: boolean, added: count: number → NOT a rename
    let old = surface(vec![{
        let mut i = sym("Props", SymbolKind::Interface);
        let mut p1 = sym("isActive", SymbolKind::Property);
        p1.signature = Some(Signature {
            parameters: vec![],
            return_type: Some("boolean".into()),
            type_parameters: vec![],
            is_async: false,
        });
        i.members = vec![p1];
        i
    }]);
    let new = surface(vec![{
        let mut i = sym("Props", SymbolKind::Interface);
        let mut p1 = sym("count", SymbolKind::Property);
        p1.signature = Some(Signature {
            parameters: vec![],
            return_type: Some("number".into()),
            type_parameters: vec![],
            is_async: false,
        });
        i.members = vec![p1];
        i
    }]);
    let changes = diff_surfaces(&old, &new);

    // Should be remove + add, not rename
    assert!(has_change(&changes, |ct| matches!(
        ct,
        StructuralChangeType::Removed(ChangeSubject::Member { .. })
    )));
    assert!(has_change(&changes, |ct| matches!(
        ct,
        StructuralChangeType::Added(ChangeSubject::Member { .. })
    )));
    assert!(!has_change(&changes, |ct| matches!(
        ct,
        StructuralChangeType::Renamed {
            from: ChangeSubject::Member { .. },
            ..
        }
    )));
}

#[test]
fn no_rename_for_completely_different_names() {
    // Even with same type, names like "x" and "processDataHandler" are too different
    let mk_prop = |name: &str| {
        let mut p = sym(name, SymbolKind::Property);
        p.signature = Some(Signature {
            parameters: vec![],
            return_type: Some("string".into()),
            type_parameters: vec![],
            is_async: false,
        });
        p
    };

    let old = surface(vec![{
        let mut i = sym("Props", SymbolKind::Interface);
        i.members = vec![mk_prop("x")];
        i
    }]);
    let new = surface(vec![{
        let mut i = sym("Props", SymbolKind::Interface);
        i.members = vec![mk_prop("processDataHandler")];
        i
    }]);
    let changes = diff_surfaces(&old, &new);

    // "x" and "processDataHandler" share no meaningful similarity
    // Should be remove + add
    assert!(!has_change(&changes, |ct| matches!(
        ct,
        StructuralChangeType::Renamed {
            from: ChangeSubject::Member { .. },
            ..
        }
    )));
}

#[test]
fn detect_symbol_renamed_top_level() {
    // ChipGroup removed, LabelGroup added with same signature → rename
    let old = surface(vec![{
        let mut s = func("ChipGroup", vec![param("items", "Item[]")], "ReactNode");
        s.qualified_name = "test.ChipGroup".into();
        s
    }]);
    let new = surface(vec![{
        let mut s = func("LabelGroup", vec![param("items", "Item[]")], "ReactNode");
        s.qualified_name = "test.LabelGroup".into();
        s
    }]);
    let changes = diff_surfaces(&old, &new);

    let rename = changes
        .iter()
        .find(|c| {
            matches!(
                &c.change_type,
                StructuralChangeType::Renamed {
                    from: ChangeSubject::Symbol { .. },
                    ..
                }
            )
        })
        .expect("Should detect top-level symbol rename");
    assert_eq!(rename.before.as_deref(), Some("ChipGroup"));
    assert_eq!(rename.after.as_deref(), Some("LabelGroup"));
}

#[test]
fn multiple_renames_greedy_matching() {
    // Two removed and two added with same type → should match best pairs
    let mk_prop = |name: &str| {
        let mut p = sym(name, SymbolKind::Property);
        p.signature = Some(Signature {
            parameters: vec![],
            return_type: Some("boolean".into()),
            type_parameters: vec![],
            is_async: false,
        });
        p
    };

    let old = surface(vec![{
        let mut i = sym("Props", SymbolKind::Interface);
        i.members = vec![mk_prop("isActive"), mk_prop("isExpanded")];
        i
    }]);
    let new = surface(vec![{
        let mut i = sym("Props", SymbolKind::Interface);
        i.members = vec![mk_prop("isClicked"), mk_prop("isOpened")];
        i
    }]);
    let changes = diff_surfaces(&old, &new);

    let renames: Vec<_> = changes
        .iter()
        .filter(|c| {
            matches!(
                &c.change_type,
                StructuralChangeType::Renamed {
                    from: ChangeSubject::Member { .. },
                    ..
                }
            )
        })
        .collect();
    assert_eq!(renames.len(), 2, "Should detect both renames");

    // No removes or adds should remain
    assert!(
        !has_change(&changes, |ct| matches!(
            ct,
            StructuralChangeType::Removed(ChangeSubject::Member { .. })
        )),
        "All removed props should be matched as renames"
    );
    assert!(
        !has_change(&changes, |ct| matches!(
            ct,
            StructuralChangeType::Added(ChangeSubject::Member { .. })
        )),
        "All added props should be matched as renames"
    );
}

#[test]
fn name_similarity_identical() {
    assert_eq!(rename::name_similarity("foo", "foo"), 1.0);
}

#[test]
fn name_similarity_prefix() {
    // isActive / isClicked share "is" + some letters
    let sim = rename::name_similarity("isActive", "isClicked");
    assert!(sim > 0.15, "Should have meaningful similarity: {}", sim);
}

#[test]
fn name_similarity_suffix() {
    let sim = rename::name_similarity("chipGroupContentRef", "labelGroupContentRef");
    assert!(sim > 0.5, "Should have high suffix similarity: {}", sim);
}

#[test]
fn name_similarity_empty() {
    assert_eq!(rename::name_similarity("", "foo"), 0.0);
    assert_eq!(rename::name_similarity("foo", ""), 0.0);
}

#[test]
fn lcs_basic() {
    assert_eq!(
        rename::longest_common_subsequence_len("ABCBDAB", "BDCAB"),
        4
    );
}

// ── should_skip_symbol filtering ─────────────────────────────────
//
// These tests verify the LanguageSemantics::should_skip_symbol() contract.
// They use a custom semantics that skips "*" symbols (matching TypeScript behavior).

/// Semantics that skips star-reexport symbols (for testing should_skip_symbol).
struct StarSkipSemantics;
impl crate::traits::LanguageSemantics for StarSkipSemantics {
    fn is_member_addition_breaking(&self, _c: &Symbol, _m: &Symbol) -> bool {
        false
    }
    fn same_family(&self, a: &Symbol, b: &Symbol) -> bool {
        a.file.parent() == b.file.parent()
    }
    fn same_identity(&self, a: &Symbol, b: &Symbol) -> bool {
        a.name == b.name
    }
    fn visibility_rank(&self, _v: Visibility) -> u8 {
        0
    }
    fn should_skip_symbol(&self, sym: &Symbol) -> bool {
        sym.name == "*"
    }
}

#[test]
fn star_reexport_removed_is_filtered() {
    let mut star = sym("*", SymbolKind::Namespace);
    star.qualified_name = "index.*".into();

    let old = surface(vec![
        star,
        func("greet", vec![param("name", "string")], "void"),
    ]);
    let new = surface(vec![func("greet", vec![param("name", "string")], "void")]);
    let changes = diff_surfaces_with_semantics(&old, &new, &StarSkipSemantics);

    assert!(
        !changes.iter().any(|c| c.symbol == "*"),
        "Star re-export removal should be filtered out"
    );
    assert!(changes.is_empty(), "No changes expected");
}

#[test]
fn star_reexport_added_is_filtered() {
    // `export * from './utils'` added — should NOT appear as SymbolAdded
    let mut star = sym("*", SymbolKind::Namespace);
    star.qualified_name = "index.*".into();

    let old = surface(vec![func("greet", vec![param("name", "string")], "void")]);
    let new = surface(vec![
        star,
        func("greet", vec![param("name", "string")], "void"),
    ]);
    let changes = diff_surfaces_with_semantics(&old, &new, &StarSkipSemantics);

    assert!(
        !changes.iter().any(|c| c.symbol == "*"),
        "Star re-export addition should be filtered out"
    );
    assert!(changes.is_empty(), "No changes expected");
}

#[test]
fn multiple_star_reexports_same_file_filtered() {
    // Multiple `export *` in same barrel file — all should be filtered
    let mk_star = || {
        let mut s = sym("*", SymbolKind::Namespace);
        s.qualified_name = "index.*".into();
        s
    };

    let old = surface(vec![mk_star(), mk_star(), mk_star()]);
    let new = surface(vec![mk_star()]); // Two removed, one kept
    let changes = diff_surfaces_with_semantics(&old, &new, &StarSkipSemantics);

    assert!(
        !changes.iter().any(|c| c.symbol == "*"),
        "All star re-export changes should be filtered"
    );
}

#[test]
fn star_reexport_filtered_but_real_symbols_still_diff() {
    // Star re-exports filtered, but named symbols are still diffed normally
    let mut star = sym("*", SymbolKind::Namespace);
    star.qualified_name = "index.*".into();

    let old = surface(vec![star.clone(), func("oldFunc", vec![], "void")]);
    let new = surface(vec![func("newFunc", vec![], "void")]);
    let changes = diff_surfaces_with_semantics(&old, &new, &StarSkipSemantics);

    // Star should NOT appear
    assert!(!changes.iter().any(|c| c.symbol == "*"));

    // oldFunc should be removed (or renamed to newFunc), newFunc added (or part of rename)
    // Since they have same type (void, 0 params), rename detection might match them
    let has_removal_or_rename = changes.iter().any(|c| {
        matches!(
            c.change_type,
            StructuralChangeType::Removed(ChangeSubject::Symbol { .. })
        ) || matches!(
            c.change_type,
            StructuralChangeType::Renamed {
                from: ChangeSubject::Symbol { .. },
                ..
            }
        )
    });
    assert!(
        has_removal_or_rename,
        "Real symbol changes should still be detected"
    );
}

#[test]
fn named_namespace_reexport_not_filtered() {
    // `export * as utils from './utils'` produces name="utils", NOT "*"
    // These should NOT be filtered
    let mut ns = sym("utils", SymbolKind::Namespace);
    ns.qualified_name = "index.utils".into();

    let old = surface(vec![ns]);
    let new = surface(vec![]);
    let changes = diff_surfaces(&old, &new);

    assert!(
        has_change(&changes, |ct| matches!(
            ct,
            StructuralChangeType::Removed(ChangeSubject::Symbol { .. })
        )),
        "Named namespace re-exports should still be tracked"
    );
    assert_eq!(changes[0].symbol, "utils");
}

// ── Relocation / moved to deprecated ─────────────────────────────

#[test]
fn detect_moved_to_deprecated() {
    // Chip at components/Chip/ in v5, deprecated/components/Chip/ in v6
    let mut old_sym = sym("Chip", SymbolKind::Variable);
    old_sym.qualified_name = "pkg/dist/esm/components/Chip/Chip.Chip".into();

    let mut new_sym = sym("Chip", SymbolKind::Variable);
    new_sym.qualified_name = "pkg/dist/esm/deprecated/components/Chip/Chip.Chip".into();

    let old = surface(vec![old_sym]);
    let new = surface(vec![new_sym]);
    let changes = diff_surfaces(&old, &new);

    let moved = changes
        .iter()
        .find(|c| matches!(&c.change_type, StructuralChangeType::Relocated { .. }))
        .expect("Should detect moved to deprecated");
    assert_eq!(moved.symbol, "Chip");
    assert!(moved.is_breaking);
    assert!(moved.description.contains("deprecated"));

    // Should NOT appear as removed or added
    assert!(
        !has_change(&changes, |ct| matches!(
            ct,
            StructuralChangeType::Removed(ChangeSubject::Symbol { .. })
        )),
        "Relocated symbol should not also appear as removed"
    );
    assert!(
        !has_change(&changes, |ct| matches!(
            ct,
            StructuralChangeType::Added(ChangeSubject::Symbol { .. })
        )),
        "Relocated symbol should not also appear as added"
    );
}

#[test]
fn detect_moved_to_deprecated_with_member_changes() {
    // ChipProps moves to deprecated AND loses a member
    let mk_prop = |name: &str| {
        let mut p = sym(name, SymbolKind::Property);
        p.signature = Some(Signature {
            parameters: vec![],
            return_type: Some("boolean".into()),
            type_parameters: vec![],
            is_async: false,
        });
        p
    };

    let mut old_iface = sym("ChipProps", SymbolKind::Interface);
    old_iface.qualified_name = "pkg/dist/esm/components/Chip/Chip.ChipProps".into();
    old_iface.members = vec![mk_prop("isActive"), mk_prop("variant")];

    let mut new_iface = sym("ChipProps", SymbolKind::Interface);
    new_iface.qualified_name = "pkg/dist/esm/deprecated/components/Chip/Chip.ChipProps".into();
    new_iface.members = vec![mk_prop("variant")]; // isActive removed

    let old = surface(vec![old_iface]);
    let new = surface(vec![new_iface]);
    let changes = diff_surfaces(&old, &new);

    // Should detect the deprecation move
    assert!(
        has_change(&changes, |ct| matches!(
            ct,
            StructuralChangeType::Relocated { .. }
        )),
        "Should detect move to deprecated"
    );

    // Should ALSO detect the member removal
    assert!(
        has_change(&changes, |ct| matches!(
            ct,
            StructuralChangeType::Removed(ChangeSubject::Member { .. })
        )),
        "Should detect property removal within deprecated move"
    );
    let prop_removed = changes
        .iter()
        .find(|c| {
            matches!(
                c.change_type,
                StructuralChangeType::Removed(ChangeSubject::Member { .. })
            )
        })
        .unwrap();
    assert_eq!(prop_removed.symbol, "isActive");
}

#[test]
fn detect_moved_from_next_to_deprecated() {
    // DualListSelector moves from next/components/ to deprecated/components/
    let mut old_sym = sym("DualListSelector", SymbolKind::Variable);
    old_sym.qualified_name = "pkg/dist/esm/next/components/DLS/DLS.DualListSelector".into();

    let mut new_sym = sym("DualListSelector", SymbolKind::Variable);
    new_sym.qualified_name = "pkg/dist/esm/deprecated/components/DLS/DLS.DualListSelector".into();

    let old = surface(vec![old_sym]);
    let new = surface(vec![new_sym]);
    let changes = diff_surfaces(&old, &new);

    assert!(
        has_change(&changes, |ct| matches!(
            ct,
            StructuralChangeType::Relocated { .. }
        )),
        "next/ → deprecated/ should be detected as moved to deprecated"
    );
}

#[test]
fn promoted_from_deprecated_not_breaking() {
    // Symbol promoted from deprecated to main — not breaking
    let mut old_sym = sym("Modal", SymbolKind::Variable);
    old_sym.qualified_name = "pkg/dist/esm/deprecated/components/Modal/Modal.Modal".into();

    let mut new_sym = sym("Modal", SymbolKind::Variable);
    new_sym.qualified_name = "pkg/dist/esm/components/Modal/Modal.Modal".into();

    let old = surface(vec![old_sym]);
    let new = surface(vec![new_sym]);
    let changes = diff_surfaces(&old, &new);

    // Promotion is non-breaking
    assert!(
        !changes.iter().any(|c| c.is_breaking),
        "Promotion from deprecated should not be breaking"
    );

    // Should not be marked as removed
    assert!(!has_change(&changes, |ct| matches!(
        ct,
        StructuralChangeType::Removed(ChangeSubject::Symbol { .. })
    )));
}

#[test]
fn relocation_does_not_interfere_with_rename_detection() {
    // Relocated symbol should be removed from rename candidate pool
    let mut old_chip = sym("Chip", SymbolKind::Variable);
    old_chip.qualified_name = "pkg/dist/esm/components/Chip/Chip.Chip".into();
    let mut new_chip = sym("Chip", SymbolKind::Variable);
    new_chip.qualified_name = "pkg/dist/esm/deprecated/components/Chip/Chip.Chip".into();

    // Also has a genuine rename: OldWidget → NewWidget (same signature)
    let old_widget = func("OldWidget", vec![param("x", "number")], "void");
    let new_widget = func("NewWidget", vec![param("x", "number")], "void");

    let old = surface(vec![old_chip, old_widget]);
    let new = surface(vec![new_chip, new_widget]);
    let changes = diff_surfaces(&old, &new);

    // Chip should be moved to deprecated
    assert!(has_change(&changes, |ct| matches!(
        ct,
        StructuralChangeType::Relocated { .. }
    )));

    // OldWidget → NewWidget should be detected as rename
    assert!(has_change(&changes, |ct| matches!(
        ct,
        StructuralChangeType::Renamed {
            from: ChangeSubject::Symbol { .. },
            ..
        }
    )));
    let rename = changes
        .iter()
        .find(|c| {
            matches!(
                &c.change_type,
                StructuralChangeType::Renamed {
                    from: ChangeSubject::Symbol { .. },
                    ..
                }
            )
        })
        .unwrap();
    assert_eq!(rename.before.as_deref(), Some("OldWidget"));
    assert_eq!(rename.after.as_deref(), Some("NewWidget"));
}

#[test]
fn multiple_symbols_moved_to_deprecated() {
    // Multiple symbols moved at once
    let mk = |name: &str, path_prefix: &str| {
        let mut s = sym(name, SymbolKind::Constant);
        s.qualified_name = format!("pkg/dist/esm/{}/Chip/{}.{}", path_prefix, name, name);
        s
    };

    let old = surface(vec![
        mk("Chip", "components"),
        mk("ChipGroup", "components"),
        mk("ChipProps", "components"),
    ]);
    let new = surface(vec![
        mk("Chip", "deprecated/components"),
        mk("ChipGroup", "deprecated/components"),
        mk("ChipProps", "deprecated/components"),
    ]);
    let changes = diff_surfaces(&old, &new);

    let moved_count = changes
        .iter()
        .filter(|c| matches!(&c.change_type, StructuralChangeType::Relocated { .. }))
        .count();
    assert_eq!(moved_count, 3, "All three should be detected as moved");
    assert!(
        !has_change(&changes, |ct| matches!(
            ct,
            StructuralChangeType::Removed(ChangeSubject::Symbol { .. })
        )),
        "None should be reported as removed"
    );
}

// ── Default export deduplication ──────────────────────────────────

// NOTE: dedup_default_export_removal and dedup_default_export_addition tests
// were removed — they test TypeScript-specific post_process behavior
// (deduplicating JS default exports). This behavior is tested in the ts
// crate's baseline snapshot tests with TypeScript semantics.

#[test]
fn keep_default_when_no_named_sibling() {
    // If a file only has a default export, keep it
    let mut s = sym("default", SymbolKind::Constant);
    s.qualified_name = "pkg/dist/utils.default".into();

    let old = surface(vec![s]);
    let new = surface(vec![]);
    let changes = diff_surfaces(&old, &new);

    assert!(
        changes.iter().any(|c| c.symbol == "default"
            && matches!(
                c.change_type,
                StructuralChangeType::Removed(ChangeSubject::Symbol { .. })
            )),
        "Default-only export should NOT be suppressed"
    );
}

#[test]
fn keep_default_when_different_change_type() {
    // Named is removed but default has a different change (e.g., type changed)
    // Both should be kept since they represent different changes
    let mut old_named = sym("Foo", SymbolKind::Constant);
    old_named.qualified_name = "pkg/dist/foo.Foo".into();

    let mut old_default = func("default", vec![param("x", "string")], "void");
    old_default.qualified_name = "pkg/dist/foo.default".into();

    let mut new_default = func("default", vec![param("x", "number")], "void");
    new_default.qualified_name = "pkg/dist/foo.default".into();

    let old = surface(vec![old_named, old_default]);
    let new = surface(vec![new_default]); // named removed, default type changed

    let changes = diff_surfaces(&old, &new);

    // Named is removed
    assert!(changes.iter().any(|c| c.symbol == "Foo"
        && matches!(
            c.change_type,
            StructuralChangeType::Removed(ChangeSubject::Symbol { .. })
        )));

    // Default has a type change — different change_type from the removal, so keep it
    assert!(
        changes.iter().any(|c| c.symbol == "default"
            && matches!(
                c.change_type,
                StructuralChangeType::Changed(ChangeSubject::Parameter { .. })
            )),
        "Default with different change type should be kept"
    );
}

// ── No false positives ───────────────────────────────────────────

#[test]
fn identical_complex_surface_no_changes() {
    let mk = || {
        let mut cls = sym("Service", SymbolKind::Class);
        cls.extends = Some("Base".into());
        cls.implements = vec!["Serializable".into()];
        cls.is_abstract = false;

        let mut method = sym("handle", SymbolKind::Method);
        method.signature = Some(Signature {
            parameters: vec![param("req", "Request"), opt_param("opts", "Options")],
            return_type: Some("Promise<Response>".into()),
            type_parameters: vec![TypeParameter {
                name: "T".into(),
                constraint: Some("object".into()),
                default: None,
            }],
            is_async: true,
        });
        method.is_readonly = false;
        method.is_static = false;

        cls.members = vec![method];
        cls
    };
    let old = surface(vec![mk()]);
    let new = surface(vec![mk()]);
    let changes = diff_surfaces(&old, &new);
    assert!(
        changes.is_empty(),
        "Expected no changes but got: {:?}",
        changes.iter().map(|c| &c.change_type).collect::<Vec<_>>()
    );
}

// ── No-op rename detection ─────────────────────────────────────────

/// When a symbol moves to a different file path but keeps the same export
/// name (e.g., `src/components/Chart/Chart` → `src/victory/components/Chart/Chart`),
/// it should NOT produce a SymbolRenamed change. The consumer's import doesn't
/// change, so there's nothing to report.
#[test]
fn no_op_rename_is_skipped() {
    // Old surface: Chart at path A
    let mut old_sym = sym("Chart", SymbolKind::Variable);
    old_sym.qualified_name = "packages/react-charts/src/components/Chart/Chart.Chart".to_string();
    old_sym.file = "packages/react-charts/src/components/Chart/Chart.d.ts".into();

    // New surface: Chart at path B (different directory, same name)
    let mut new_sym = sym("Chart", SymbolKind::Variable);
    new_sym.qualified_name =
        "packages/react-charts/src/victory/components/Chart/Chart.Chart".to_string();
    new_sym.file = "packages/react-charts/src/victory/components/Chart/Chart.d.ts".into();

    let old = surface(vec![old_sym]);
    let new = surface(vec![new_sym]);
    let changes = diff_surfaces(&old, &new);

    // Should NOT have a SymbolRenamed change
    let renames: Vec<_> = changes
        .iter()
        .filter(|c| {
            matches!(
                &c.change_type,
                StructuralChangeType::Renamed {
                    from: ChangeSubject::Symbol { .. },
                    ..
                }
            )
        })
        .collect();
    assert!(
        renames.is_empty(),
        "No-op rename (same name, different path) should be skipped. Got: {:?}",
        renames
            .iter()
            .map(|c| format!(
                "{}: {} -> {:?}",
                c.symbol,
                c.before.as_deref().unwrap_or("?"),
                c.after
            ))
            .collect::<Vec<_>>()
    );
}

/// When import_path differs (e.g., root → subpath export), emit a Relocated change.
#[test]
fn no_op_rename_with_different_import_path_emits_relocated() {
    // Old surface: Chart reachable from root entry (@patternfly/react-charts)
    let mut old_sym = sym("Chart", SymbolKind::Variable);
    old_sym.qualified_name = "packages/react-charts/src/components/Chart/Chart.Chart".to_string();
    old_sym.file = "packages/react-charts/src/components/Chart/Chart.d.ts".into();
    old_sym.package = Some("@patternfly/react-charts".to_string());
    old_sym.import_path = None; // Root — same as package

    // New surface: Chart reachable only from victory subpath
    let mut new_sym = sym("Chart", SymbolKind::Variable);
    new_sym.qualified_name =
        "packages/react-charts/src/victory/components/Chart/Chart.Chart".to_string();
    new_sym.file = "packages/react-charts/src/victory/components/Chart/Chart.d.ts".into();
    new_sym.package = Some("@patternfly/react-charts".to_string());
    new_sym.import_path = Some("@patternfly/react-charts/victory".to_string());

    let old = surface(vec![old_sym]);
    let new = surface(vec![new_sym]);
    let changes = diff_surfaces(&old, &new);

    // Should NOT have a SymbolRenamed change
    let renames: Vec<_> = changes
        .iter()
        .filter(|c| {
            matches!(
                &c.change_type,
                StructuralChangeType::Renamed {
                    from: ChangeSubject::Symbol { .. },
                    ..
                }
            )
        })
        .collect();
    assert!(
        renames.is_empty(),
        "Should not emit Renamed for same-name symbol. Got: {:?}",
        renames.iter().map(|c| &c.symbol).collect::<Vec<_>>()
    );

    // Should have a Relocated change
    let relocated: Vec<_> = changes
        .iter()
        .filter(|c| matches!(&c.change_type, StructuralChangeType::Relocated { .. }))
        .collect();
    assert!(
        !relocated.is_empty(),
        "Should emit Relocated when import_path changes (root → victory)"
    );
    assert_eq!(relocated[0].symbol, "Chart");
    assert_eq!(
        relocated[0].before.as_deref(),
        Some("@patternfly/react-charts")
    );
    assert_eq!(
        relocated[0].after.as_deref(),
        Some("@patternfly/react-charts/victory")
    );

    // Should NOT appear as Removed (it was matched by rename detection)
    let removed: Vec<_> = changes
        .iter()
        .filter(|c| {
            matches!(
                &c.change_type,
                StructuralChangeType::Removed(ChangeSubject::Symbol { .. })
            ) && c.symbol == "Chart"
        })
        .collect();
    assert!(
        removed.is_empty(),
        "Relocated symbol should not also appear as removed"
    );
}

/// When import_path is None on both sides, no-op rename is still skipped.
#[test]
fn no_op_rename_same_import_path_is_skipped() {
    let mut old_sym = sym("Button", SymbolKind::Variable);
    old_sym.qualified_name = "packages/react-core/src/components/Button/Button.Button".to_string();
    old_sym.file = "packages/react-core/src/components/Button/Button.d.ts".into();
    old_sym.package = Some("@patternfly/react-core".to_string());
    old_sym.import_path = Some("@patternfly/react-core".to_string()); // Explicit root

    let mut new_sym = sym("Button", SymbolKind::Variable);
    new_sym.qualified_name =
        "packages/react-core/src/new-layout/components/Button/Button.Button".to_string();
    new_sym.file = "packages/react-core/src/new-layout/components/Button/Button.d.ts".into();
    new_sym.package = Some("@patternfly/react-core".to_string());
    new_sym.import_path = Some("@patternfly/react-core".to_string()); // Same root

    let old = surface(vec![old_sym]);
    let new = surface(vec![new_sym]);
    let changes = diff_surfaces(&old, &new);

    // Should NOT have Renamed or Relocated changes
    let breaking: Vec<_> = changes
        .iter()
        .filter(|c| {
            matches!(
                &c.change_type,
                StructuralChangeType::Renamed { .. } | StructuralChangeType::Relocated { .. }
            )
        })
        .collect();
    assert!(
        breaking.is_empty(),
        "No-op rename with same import_path should be skipped. Got: {:?}",
        breaking.iter().map(|c| &c.symbol).collect::<Vec<_>>()
    );
}

/// A real rename (different names) should still produce a SymbolRenamed change.
#[test]
fn real_rename_is_detected() {
    // Old: isFlat
    let mut old_sym = sym("isFlat", SymbolKind::Property);
    old_sym.qualified_name =
        "packages/react-core/src/components/Card/Card.CardProps.isFlat".to_string();

    // New: isPlain (different name, same fingerprint)
    let mut new_sym = sym("isPlain", SymbolKind::Property);
    new_sym.qualified_name =
        "packages/react-core/src/components/Card/Card.CardProps.isPlain".to_string();

    let old = surface(vec![old_sym]);
    let new = surface(vec![new_sym]);
    let changes = diff_surfaces(&old, &new);

    let renames: Vec<_> = changes
        .iter()
        .filter(|c| {
            matches!(
                &c.change_type,
                StructuralChangeType::Renamed {
                    from: ChangeSubject::Symbol { .. },
                    ..
                }
            )
        })
        .collect();
    assert!(
        !renames.is_empty(),
        "Real rename (isFlat → isPlain) should produce SymbolRenamed"
    );
    assert_eq!(renames[0].before.as_deref(), Some("isFlat"));
    assert_eq!(renames[0].after.as_deref(), Some("isPlain"));
}
