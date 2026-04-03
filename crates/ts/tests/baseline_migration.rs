//! Baseline integration tests for migration detection.
//!
//! Migration detection runs as part of `diff_surfaces`. These tests construct
//! API surfaces where symbols are removed and replacements exist in the same
//! directory, then snapshot the full diff output (including migration targets).

mod helpers;

use helpers::*;
use semver_analyzer_core::diff::diff_surfaces_with_semantics;
use semver_analyzer_ts::TypeScript;

fn diff(old: &ApiSurface<semver_analyzer_ts::symbol_data::TsSymbolData>, new: &ApiSurface<semver_analyzer_ts::symbol_data::TsSymbolData>) -> Vec<NormalizedChange> {
    normalize(&diff_surfaces_with_semantics(
        old,
        new,
        &TypeScript::default(),
    ))
}

use semver_analyzer_core::*;

#[test]
fn baseline_merge_child_into_parent_emptystate() {
    let old_header = make_interface(
        "EmptyStateHeaderProps",
        "components/EmptyState/EmptyStateHeader.d.ts",
        &[
            "children",
            "className",
            "headingLevel",
            "icon",
            "titleClassName",
            "titleText",
        ],
    );
    let old_parent = make_interface(
        "EmptyStateProps",
        "components/EmptyState/EmptyState.d.ts",
        &["children", "className", "variant"],
    );
    let new_parent = make_interface(
        "EmptyStateProps",
        "components/EmptyState/EmptyState.d.ts",
        &[
            "children",
            "className",
            "variant",
            "titleText",
            "headingLevel",
            "icon",
            "status",
            "titleClassName",
            "headerClassName",
        ],
    );

    let old = surface(vec![old_header, old_parent]);
    let new = surface(vec![new_parent]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_same_name_replacement_select() {
    let old_select = make_interface(
        "SelectProps",
        "deprecated/components/Select/Select.d.ts",
        &[
            "children",
            "className",
            "isOpen",
            "isPlain",
            "onSelect",
            "isDisabled",
            "isGrouped",
            "isCreatable",
            "selections",
            "onToggle",
            "direction",
            "position",
            "toggleRef",
            "width",
            "zIndex",
            "variant",
        ],
    );
    let old_main_select = make_interface(
        "SelectProps",
        "components/Select/Select.d.ts",
        &[
            "children",
            "className",
            "isOpen",
            "isPlain",
            "onSelect",
            "direction",
            "position",
            "selected",
            "toggle",
            "toggleRef",
            "width",
            "zIndex",
            "onOpenChange",
            "innerRef",
            "isScrollable",
        ],
    );
    let new_main_select = make_interface(
        "SelectProps",
        "components/Select/Select.d.ts",
        &[
            "children",
            "className",
            "isOpen",
            "isPlain",
            "onSelect",
            "direction",
            "position",
            "selected",
            "toggle",
            "toggleRef",
            "width",
            "zIndex",
            "onOpenChange",
            "innerRef",
            "isScrollable",
            "variant",
            "focusTimeoutDelay",
            "onToggleKeydown",
            "shouldPreventScrollOnItemFocus",
        ],
    );

    let old = surface(vec![old_select, old_main_select]);
    let new = surface(vec![new_main_select]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_decompose_into_children_modal() {
    let old_modal = make_interface(
        "ModalProps",
        "components/Modal/Modal.d.ts",
        &[
            "children",
            "className",
            "isOpen",
            "variant",
            "title",
            "actions",
            "description",
            "footer",
            "header",
            "help",
            "titleIconVariant",
            "onClose",
            "showClose",
        ],
    );
    let new_modal = make_interface(
        "ModalProps",
        "components/Modal/Modal.d.ts",
        &["children", "className", "isOpen", "variant", "onClose"],
    );
    let new_header = make_interface(
        "ModalHeaderProps",
        "components/Modal/ModalHeader.d.ts",
        &[
            "children",
            "className",
            "title",
            "description",
            "help",
            "titleIconVariant",
            "labelId",
            "descriptorId",
            "titleScreenReaderText",
        ],
    );

    // ModalProps is not removed -- it survived but lost members.
    // No migration should be detected (different code path).
    let old = surface(vec![old_modal]);
    let new = surface(vec![new_modal, new_header]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_no_false_positive_unrelated_interfaces() {
    let removed_foo = make_interface(
        "FooProps",
        "components/Foo/Foo.d.ts",
        &["children", "className", "onClick"],
    );
    let new_bar = make_interface(
        "BarProps",
        "components/Bar/Bar.d.ts",
        &["children", "className", "onSubmit"],
    );

    let old = surface(vec![removed_foo]);
    let new = surface(vec![new_bar]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_small_overlap_adaptive_threshold() {
    let removed_header = make_interface(
        "FooHeaderProps",
        "components/Foo/FooHeader.d.ts",
        &["title", "subtitle"],
    );
    let new_foo = make_interface(
        "FooProps",
        "components/Foo/Foo.d.ts",
        &[
            "children",
            "className",
            "title",
            "variant",
            "size",
            "isOpen",
            "onClose",
            "isFullscreen",
        ],
    );

    let old = surface(vec![removed_header]);
    let new = surface(vec![new_foo]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_small_interface_absorption_emptystate_icon() {
    let old_icon_props = make_interface(
        "EmptyStateIconProps",
        "components/EmptyState/EmptyStateIcon.d.ts",
        &["icon", "className"],
    );
    let old_parent = make_interface(
        "EmptyStateProps",
        "components/EmptyState/EmptyState.d.ts",
        &["children", "className", "variant"],
    );
    let new_parent = make_interface(
        "EmptyStateProps",
        "components/EmptyState/EmptyState.d.ts",
        &[
            "children",
            "className",
            "variant",
            "icon",
            "titleText",
            "headingLevel",
            "status",
        ],
    );

    let old = surface(vec![old_icon_props, old_parent]);
    let new = surface(vec![new_parent]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}

#[test]
fn baseline_single_member_no_false_positive() {
    let old_tiny = make_interface("TinyProps", "components/Foo/Tiny.d.ts", &["uniqueProp"]);
    let old_parent = make_interface(
        "FooProps",
        "components/Foo/Foo.d.ts",
        &["children", "className"],
    );
    let new_parent = make_interface(
        "FooProps",
        "components/Foo/Foo.d.ts",
        &["children", "className", "newProp"],
    );

    let old = surface(vec![old_tiny, old_parent]);
    let new = surface(vec![new_parent]);
    insta::assert_yaml_snapshot!(diff(&old, &new));
}
