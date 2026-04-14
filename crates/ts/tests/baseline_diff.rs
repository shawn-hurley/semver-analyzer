//! Baseline integration tests for the diff engine.
//!
//! These tests capture the current behavior of `diff_surfaces` as insta
//! snapshots. They are the safety net for the refactoring: if any phase
//! produces a snapshot change, the refactoring has a behavioral regression.
//!
//! Each test mirrors an existing unit test in `core/diff/tests.rs` but
//! captures the full output as a normalized snapshot rather than asserting
//! on specific enum variants.

mod helpers;

use helpers::*;
use semver_analyzer_core::diff::diff_surfaces_with_semantics;
use semver_analyzer_core::{Signature, SymbolKind, TypeParameter, Visibility};
use semver_analyzer_ts::TypeScript;

fn diff(old: &ApiSurface, new: &ApiSurface) -> Vec<NormalizedChange> {
    normalize(&diff_surfaces_with_semantics(
        old,
        new,
        &TypeScript::default(),
    ))
}

// ── Symbol-level ─────────────────────────────────────────────────

#[test]
fn baseline_symbol_removed() {
    let old = surface(vec![func("greet", vec![param("name", "string")], "void")]);
    let new = surface(vec![]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_symbol_added() {
    let old = surface(vec![]);
    let new = surface(vec![func("greet", vec![param("name", "string")], "void")]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_no_changes_for_identical_surfaces() {
    let s = surface(vec![func("greet", vec![param("name", "string")], "void")]);
    insta::assert_yaml_snapshot!(diff(&s, &s));
}

// ── Parameter changes ────────────────────────────────────────────

#[test]
fn baseline_required_parameter_added() {
    let old = surface(vec![func("f", vec![param("a", "string")], "void")]);
    let new = surface(vec![func(
        "f",
        vec![param("a", "string"), param("b", "number")],
        "void",
    )]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_optional_parameter_added() {
    let old = surface(vec![func("f", vec![param("a", "string")], "void")]);
    let new = surface(vec![func(
        "f",
        vec![param("a", "string"), opt_param("b", "number")],
        "void",
    )]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_parameter_removed() {
    let old = surface(vec![func(
        "f",
        vec![param("a", "string"), param("b", "number")],
        "void",
    )]);
    let new = surface(vec![func("f", vec![param("a", "string")], "void")]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_parameter_type_changed() {
    let old = surface(vec![func("f", vec![param("a", "string")], "void")]);
    let new = surface(vec![func("f", vec![param("a", "number")], "void")]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_parameter_made_required() {
    let old = surface(vec![func("f", vec![opt_param("a", "string")], "void")]);
    let new = surface(vec![func("f", vec![param("a", "string")], "void")]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_parameter_made_optional() {
    let old = surface(vec![func("f", vec![param("a", "string")], "void")]);
    let new = surface(vec![func("f", vec![opt_param("a", "string")], "void")]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_rest_parameter_added() {
    let old = surface(vec![func("f", vec![param("a", "string")], "void")]);
    let new = surface(vec![func(
        "f",
        vec![param("a", "string"), rest_param("args", "unknown[]")],
        "void",
    )]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_rest_parameter_removed() {
    let old = surface(vec![func(
        "f",
        vec![param("a", "string"), rest_param("args", "unknown[]")],
        "void",
    )]);
    let new = surface(vec![func("f", vec![param("a", "string")], "void")]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

// ── Return type changes ──────────────────────────────────────────

#[test]
fn baseline_return_type_changed() {
    let old = surface(vec![func("f", vec![], "string")]);
    let new = surface(vec![func("f", vec![], "number")]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_made_async() {
    let old = surface(vec![func("f", vec![], "string")]);
    let new = surface(vec![func("f", vec![], "Promise<string>")]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_made_sync() {
    let old = surface(vec![func("f", vec![], "Promise<string>")]);
    let new = surface(vec![func("f", vec![], "string")]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

// ── Visibility changes ───────────────────────────────────────────

#[test]
fn baseline_visibility_reduced() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_visibility_increased() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

// ── Modifier changes ─────────────────────────────────────────────

#[test]
fn baseline_readonly_added() {
    let old = surface(vec![sym("prop", SymbolKind::Property)]);
    let new = surface(vec![{
        let mut s = sym("prop", SymbolKind::Property);
        s.is_readonly = true;
        s
    }]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_readonly_removed() {
    let old = surface(vec![{
        let mut s = sym("prop", SymbolKind::Property);
        s.is_readonly = true;
        s
    }]);
    let new = surface(vec![sym("prop", SymbolKind::Property)]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_abstract_added() {
    let old = surface(vec![sym("validate", SymbolKind::Method)]);
    let new = surface(vec![{
        let mut s = sym("validate", SymbolKind::Method);
        s.is_abstract = true;
        s
    }]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_static_instance_changed() {
    let old = surface(vec![sym("method", SymbolKind::Method)]);
    let new = surface(vec![{
        let mut s = sym("method", SymbolKind::Method);
        s.is_static = true;
        s
    }]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

// ── Class hierarchy ──────────────────────────────────────────────

#[test]
fn baseline_base_class_changed() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_interface_implementation_added() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_interface_implementation_removed() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

// ── Type parameter changes ───────────────────────────────────────

#[test]
fn baseline_type_parameter_added_required() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_type_parameter_added_with_default() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_type_parameter_removed() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_type_parameter_constraint_changed() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

// ── Enum member changes ──────────────────────────────────────────

#[test]
fn baseline_enum_member_added() {
    let old = surface(vec![{
        let mut e = sym("Color", SymbolKind::Enum);
        e.members = vec![enum_member("Red", "0")];
        e
    }]);
    let new = surface(vec![{
        let mut e = sym("Color", SymbolKind::Enum);
        e.members = vec![enum_member("Red", "0"), enum_member("Green", "1")];
        e
    }]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_enum_member_removed() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_enum_member_value_changed() {
    let old = surface(vec![{
        let mut e = sym("Color", SymbolKind::Enum);
        e.members = vec![enum_member("Red", "0")];
        e
    }]);
    let new = surface(vec![{
        let mut e = sym("Color", SymbolKind::Enum);
        e.members = vec![enum_member("Red", "1")];
        e
    }]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

// ── Interface member changes ─────────────────────────────────────

#[test]
fn baseline_interface_property_removed() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_interface_property_added() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

// ── Class member changes ─────────────────────────────────────────

#[test]
fn baseline_class_method_return_type_changed() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

// ── Rename detection ────────────────────────────────────────────

#[test]
fn baseline_property_renamed() {
    let old = surface(vec![{
        let mut i = sym("ButtonProps", SymbolKind::Interface);
        i.members = vec![
            mk_prop("isActive", "boolean"),
            mk_prop("variant", "boolean"),
        ];
        i
    }]);
    let new = surface(vec![{
        let mut i = sym("ButtonProps", SymbolKind::Interface);
        i.members = vec![
            mk_prop("isClicked", "boolean"),
            mk_prop("variant", "boolean"),
        ];
        i
    }]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_property_renamed_suffix_match() {
    let old = surface(vec![{
        let mut i = sym("ToolbarContextProps", SymbolKind::Interface);
        i.members = vec![mk_prop("chipGroupContentRef", "RefObject<HTMLDivElement>")];
        i
    }]);
    let new = surface(vec![{
        let mut i = sym("ToolbarContextProps", SymbolKind::Interface);
        i.members = vec![mk_prop("labelGroupContentRef", "RefObject<HTMLDivElement>")];
        i
    }]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_no_rename_for_different_types() {
    let old = surface(vec![{
        let mut i = sym("Props", SymbolKind::Interface);
        i.members = vec![mk_prop("isActive", "boolean")];
        i
    }]);
    let new = surface(vec![{
        let mut i = sym("Props", SymbolKind::Interface);
        i.members = vec![mk_prop("count", "number")];
        i
    }]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_no_rename_for_completely_different_names() {
    let old = surface(vec![{
        let mut i = sym("Props", SymbolKind::Interface);
        i.members = vec![mk_prop("x", "string")];
        i
    }]);
    let new = surface(vec![{
        let mut i = sym("Props", SymbolKind::Interface);
        i.members = vec![mk_prop("processDataHandler", "string")];
        i
    }]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_symbol_renamed_top_level() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_multiple_renames_greedy_matching() {
    let old = surface(vec![{
        let mut i = sym("Props", SymbolKind::Interface);
        i.members = vec![
            mk_prop("isActive", "boolean"),
            mk_prop("isExpanded", "boolean"),
        ];
        i
    }]);
    let new = surface(vec![{
        let mut i = sym("Props", SymbolKind::Interface);
        i.members = vec![
            mk_prop("isClicked", "boolean"),
            mk_prop("isOpened", "boolean"),
        ];
        i
    }]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

// ── Star re-export filtering ─────────────────────────────────────

#[test]
fn baseline_star_reexport_removed_filtered() {
    let mut star = sym("*", SymbolKind::Namespace);
    star.qualified_name = "index.*".into();
    let old = surface(vec![
        star,
        func("greet", vec![param("name", "string")], "void"),
    ]);
    let new = surface(vec![func("greet", vec![param("name", "string")], "void")]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_star_reexport_added_filtered() {
    let mut star = sym("*", SymbolKind::Namespace);
    star.qualified_name = "index.*".into();
    let old = surface(vec![func("greet", vec![param("name", "string")], "void")]);
    let new = surface(vec![
        star,
        func("greet", vec![param("name", "string")], "void"),
    ]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_multiple_star_reexports_filtered() {
    let mk_star = || {
        let mut s = sym("*", SymbolKind::Namespace);
        s.qualified_name = "index.*".into();
        s
    };
    let old = surface(vec![mk_star(), mk_star(), mk_star()]);
    let new = surface(vec![mk_star()]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_star_reexport_filtered_real_symbols_still_diff() {
    let mut star = sym("*", SymbolKind::Namespace);
    star.qualified_name = "index.*".into();
    let old = surface(vec![star.clone(), func("oldFunc", vec![], "void")]);
    let new = surface(vec![func("newFunc", vec![], "void")]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_named_namespace_reexport_not_filtered() {
    let mut ns = sym("utils", SymbolKind::Namespace);
    ns.qualified_name = "index.utils".into();
    let old = surface(vec![ns]);
    let new = surface(vec![]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

// ── Relocation / moved to deprecated ─────────────────────────────

#[test]
fn baseline_moved_to_deprecated() {
    let mut old_sym = sym("Chip", SymbolKind::Variable);
    old_sym.qualified_name = "pkg/dist/esm/components/Chip/Chip.Chip".into();
    let mut new_sym = sym("Chip", SymbolKind::Variable);
    new_sym.qualified_name = "pkg/dist/esm/deprecated/components/Chip/Chip.Chip".into();
    let old = surface(vec![old_sym]);
    let new = surface(vec![new_sym]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_moved_to_deprecated_with_member_changes() {
    let mut old_iface = sym("ChipProps", SymbolKind::Interface);
    old_iface.qualified_name = "pkg/dist/esm/components/Chip/Chip.ChipProps".into();
    old_iface.members = vec![
        mk_prop("isActive", "boolean"),
        mk_prop("variant", "boolean"),
    ];

    let mut new_iface = sym("ChipProps", SymbolKind::Interface);
    new_iface.qualified_name = "pkg/dist/esm/deprecated/components/Chip/Chip.ChipProps".into();
    new_iface.members = vec![mk_prop("variant", "boolean")];

    let old = surface(vec![old_iface]);
    let new = surface(vec![new_iface]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_moved_from_next_to_deprecated() {
    let mut old_sym = sym("DualListSelector", SymbolKind::Variable);
    old_sym.qualified_name = "pkg/dist/esm/next/components/DLS/DLS.DualListSelector".into();
    let mut new_sym = sym("DualListSelector", SymbolKind::Variable);
    new_sym.qualified_name = "pkg/dist/esm/deprecated/components/DLS/DLS.DualListSelector".into();
    let old = surface(vec![old_sym]);
    let new = surface(vec![new_sym]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_promoted_from_deprecated_not_breaking() {
    let mut old_sym = sym("Modal", SymbolKind::Variable);
    old_sym.qualified_name = "pkg/dist/esm/deprecated/components/Modal/Modal.Modal".into();
    let mut new_sym = sym("Modal", SymbolKind::Variable);
    new_sym.qualified_name = "pkg/dist/esm/components/Modal/Modal.Modal".into();
    let old = surface(vec![old_sym]);
    let new = surface(vec![new_sym]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_relocation_does_not_interfere_with_rename() {
    let mut old_chip = sym("Chip", SymbolKind::Variable);
    old_chip.qualified_name = "pkg/dist/esm/components/Chip/Chip.Chip".into();
    let mut new_chip = sym("Chip", SymbolKind::Variable);
    new_chip.qualified_name = "pkg/dist/esm/deprecated/components/Chip/Chip.Chip".into();

    let old_widget = func("OldWidget", vec![param("x", "number")], "void");
    let new_widget = func("NewWidget", vec![param("x", "number")], "void");

    let old = surface(vec![old_chip, old_widget]);
    let new = surface(vec![new_chip, new_widget]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_multiple_symbols_moved_to_deprecated() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

// ── Default export deduplication ──────────────────────────────────

#[test]
fn baseline_dedup_default_export_removal() {
    let mk = |name: &str, file: &str| {
        let mut s = sym(name, SymbolKind::Constant);
        s.qualified_name = format!("pkg/dist/{}.{}", file, name);
        s
    };
    let old = surface(vec![mk("c_button", "c_button"), mk("default", "c_button")]);
    let new = surface(vec![]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_dedup_default_export_addition() {
    let mk = |name: &str, file: &str| {
        let mut s = sym(name, SymbolKind::Constant);
        s.qualified_name = format!("pkg/dist/{}.{}", file, name);
        s
    };
    let old = surface(vec![]);
    let new = surface(vec![mk("c_button", "c_button"), mk("default", "c_button")]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_keep_default_when_no_named_sibling() {
    let mut s = sym("default", SymbolKind::Constant);
    s.qualified_name = "pkg/dist/utils.default".into();
    let old = surface(vec![s]);
    let new = surface(vec![]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_keep_default_when_different_change_type() {
    let mut old_named = sym("Foo", SymbolKind::Constant);
    old_named.qualified_name = "pkg/dist/foo.Foo".into();

    let mut old_default = func("default", vec![param("x", "string")], "void");
    old_default.qualified_name = "pkg/dist/foo.default".into();

    let mut new_default = func("default", vec![param("x", "number")], "void");
    new_default.qualified_name = "pkg/dist/foo.default".into();

    let old = surface(vec![old_named, old_default]);
    let new = surface(vec![new_default]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

// ── No false positives ───────────────────────────────────────────

#[test]
fn baseline_identical_complex_surface_no_changes() {
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
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

// ── No-op rename detection ─────────────────────────────────────────

#[test]
fn baseline_no_op_rename_is_skipped() {
    let mut old_sym = sym("Chart", SymbolKind::Variable);
    old_sym.qualified_name = "packages/react-charts/src/components/Chart/Chart.Chart".to_string();
    old_sym.file = "packages/react-charts/src/components/Chart/Chart.d.ts".into();

    let mut new_sym = sym("Chart", SymbolKind::Variable);
    new_sym.qualified_name =
        "packages/react-charts/src/victory/components/Chart/Chart.Chart".to_string();
    new_sym.file = "packages/react-charts/src/victory/components/Chart/Chart.d.ts".into();

    let old = surface(vec![old_sym]);
    let new = surface(vec![new_sym]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_real_rename_is_detected() {
    let mut old_sym = sym("isFlat", SymbolKind::Property);
    old_sym.qualified_name =
        "packages/react-core/src/components/Card/Card.CardProps.isFlat".to_string();

    let mut new_sym = sym("isPlain", SymbolKind::Property);
    new_sym.qualified_name =
        "packages/react-core/src/components/Card/Card.CardProps.isPlain".to_string();

    let old = surface(vec![old_sym]);
    let new = surface(vec![new_sym]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_import_path_relocation_detected() {
    let mut old_sym = sym("Chart", SymbolKind::Variable);
    old_sym.qualified_name = "packages/react-charts/src/components/Chart/Chart.Chart".to_string();
    old_sym.file = "packages/react-charts/src/components/Chart/Chart.d.ts".into();
    old_sym.package = Some("@patternfly/react-charts".to_string());
    // Root entry point — import_path is None (same as package)

    let mut new_sym = sym("Chart", SymbolKind::Variable);
    new_sym.qualified_name =
        "packages/react-charts/src/victory/components/Chart/Chart.Chart".to_string();
    new_sym.file = "packages/react-charts/src/victory/components/Chart/Chart.d.ts".into();
    new_sym.package = Some("@patternfly/react-charts".to_string());
    new_sym.import_path = Some("@patternfly/react-charts/victory".to_string());

    let old = surface(vec![old_sym]);
    let new = surface(vec![new_sym]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}
