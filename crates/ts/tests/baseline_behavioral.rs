//! Baseline integration tests for JSX diff and CSS scan analysis.
//!
//! These tests capture the output of `diff_jsx_bodies` and `diff_css_references`
//! as insta snapshots.

mod helpers;

use helpers::*;
use semver_analyzer_ts::css_scan::diff_css_references;
use semver_analyzer_ts::jsx_diff::diff_jsx_bodies;
use std::path::PathBuf;

fn jsx_diff(old: &str, new: &str, name: &str) -> Vec<NormalizedBehavioralChange> {
    normalize_jsx(&diff_jsx_bodies(old, new, name, &PathBuf::from("test.tsx")))
}

fn css_diff(old: &str, new: &str, name: &str) -> Vec<NormalizedBehavioralChange> {
    normalize_jsx(&diff_css_references(
        old,
        new,
        name,
        &PathBuf::from("test.tsx"),
    ))
}

// ── JSX diff ─────────────────────────────────────────────────────

#[test]
fn baseline_jsx_no_jsx_returns_empty() {
    insta::assert_yaml_snapshot!(jsx_diff("{ return 42; }", "{ return 43; }", "foo",));
}

#[test]
fn baseline_jsx_element_added() {
    insta::assert_yaml_snapshot!(jsx_diff(
        "{ return <div>hello</div>; }",
        "{ return <div><section>hello</section></div>; }",
        "MyComponent",
    ));
}

#[test]
fn baseline_jsx_element_removed() {
    insta::assert_yaml_snapshot!(jsx_diff(
        "{ return <div><span>text</span></div>; }",
        "{ return <div>text</div>; }",
        "Comp",
    ));
}

#[test]
fn baseline_jsx_aria_attribute_removed() {
    insta::assert_yaml_snapshot!(jsx_diff(
        r#"{ return <button aria-labelledby="title">Click</button>; }"#,
        "{ return <button>Click</button>; }",
        "MyButton",
    ));
}

#[test]
fn baseline_jsx_aria_attribute_added() {
    insta::assert_yaml_snapshot!(jsx_diff(
        "{ return <div>content</div>; }",
        r#"{ return <div aria-hidden="true">content</div>; }"#,
        "Panel",
    ));
}

#[test]
fn baseline_jsx_role_changed() {
    insta::assert_yaml_snapshot!(jsx_diff(
        r#"{ return <li role="separator"></li>; }"#,
        r#"{ return <li role="presentation"></li>; }"#,
        "NavSeparator",
    ));
}

#[test]
fn baseline_jsx_css_class_removed() {
    insta::assert_yaml_snapshot!(jsx_diff(
        r#"{ return <div className="pf-v5-c-button pf-m-primary">btn</div>; }"#,
        r#"{ return <div className="pf-v6-c-button pf-m-primary">btn</div>; }"#,
        "Button",
    ));
}

#[test]
fn baseline_jsx_data_attribute_changed() {
    insta::assert_yaml_snapshot!(jsx_diff(
        r#"{ return <div data-ouia-component-type="PF4/Button">btn</div>; }"#,
        r#"{ return <div data-ouia-component-type="PF5/Button">btn</div>; }"#,
        "Button",
    ));
}

#[test]
fn baseline_jsx_wrapper_div_added() {
    insta::assert_yaml_snapshot!(jsx_diff(
        "{ return <button>Click</button>; }",
        "{ return <div><button>Click</button></div>; }",
        "Toggle",
    ));
}

#[test]
fn baseline_jsx_conditional() {
    insta::assert_yaml_snapshot!(jsx_diff(
        r#"{ return isOpen ? <div role="dialog">content</div> : null; }"#,
        r#"{ return isOpen ? <section role="dialog">content</section> : null; }"#,
        "Modal",
    ));
}

#[test]
fn baseline_jsx_multiple_categories() {
    insta::assert_yaml_snapshot!(jsx_diff(
        r#"{ return <div role="separator" className="pf-v5-sep" aria-label="sep">line</div>; }"#,
        r#"{ return <hr className="pf-v6-sep">line</hr>; }"#,
        "Sep",
    ));
}

#[test]
fn baseline_jsx_expression_classname_skips_identifiers() {
    insta::assert_yaml_snapshot!(jsx_diff(
        r#"{ return <div className={css(styles.button, isBlock && styles.modifiers.block)}>btn</div>; }"#,
        r#"{ return <div className={css(styles.button, isBlock && styles.modifiers.fill)}>btn</div>; }"#,
        "Button",
    ));
}

#[test]
fn baseline_jsx_expression_classname_extracts_string_literals() {
    insta::assert_yaml_snapshot!(jsx_diff(
        r#"{ return <div className={css("pf-v5-c-button", isActive && "pf-m-active")}>btn</div>; }"#,
        r#"{ return <div className={css("pf-v6-c-button", isActive && "pf-m-active")}>btn</div>; }"#,
        "Button",
    ));
}

#[test]
fn baseline_jsx_template_literal_classname() {
    insta::assert_yaml_snapshot!(jsx_diff(
        r#"{ return <div className={`pf-v5-c-button ${cond}`}>btn</div>; }"#,
        r#"{ return <div className={`pf-v6-c-button ${cond}`}>btn</div>; }"#,
        "Button",
    ));
}

// ── CSS scan ─────────────────────────────────────────────────────

#[test]
fn baseline_css_var_removed() {
    insta::assert_yaml_snapshot!(css_diff(
        r#"const color = "var(--pf-v5-global--Color--100)";"#,
        r#"const color = "var(--pf-v6-global--Color--100)";"#,
        "MyComponent",
    ));
}

#[test]
fn baseline_css_class_prefix_changed() {
    insta::assert_yaml_snapshot!(css_diff(
        r#"className="pf-v5-c-button pf-v5-c-button--primary""#,
        r#"className="pf-v6-c-button pf-v6-c-button--primary""#,
        "Button",
    ));
}

#[test]
fn baseline_css_no_refs_returns_empty() {
    insta::assert_yaml_snapshot!(css_diff("const x = 42;", "const x = 43;", "foo",));
}

#[test]
fn baseline_css_var_unchanged() {
    insta::assert_yaml_snapshot!(css_diff(
        r#"const color = "var(--pf-v5-global--Color--100)";"#,
        r#"const color = "var(--pf-v5-global--Color--100)";"#,
        "Comp",
    ));
}

#[test]
fn baseline_css_multiple_vars_mixed() {
    insta::assert_yaml_snapshot!(css_diff(
        r#"
            const a = "var(--pf-v5-global--Color--100)";
            const b = "var(--pf-v5-global--spacer--md)";
            const c = "var(--pf-v5-global--FontSize--sm)";
        "#,
        r#"
            const a = "var(--pf-v6-global--Color--100)";
            const b = "var(--pf-v5-global--spacer--md)";
        "#,
        "Comp",
    ));
}
