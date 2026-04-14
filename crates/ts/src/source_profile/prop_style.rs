//! Prop-to-style binding extraction.
//!
//! Traces which component props control the application of CSS class tokens
//! in the component's className expression. This identifies patterns like:
//!
//! ```tsx
//! className={css(
//!   styles.menu,
//!   isScrollable && styles.modifiers.scrollable,
//!   isPlain && styles.modifiers.plain,
//! )}
//! ```
//!
//! Here, `isScrollable` controls `styles.modifiers.scrollable` and `isPlain`
//! controls `styles.modifiers.plain`. When the CSS token is removed in a new
//! version but the prop remains, the prop becomes a silent no-op.
//!
//! Supported patterns:
//! - `prop && styles.xxx` — logical AND
//! - `prop ? styles.xxx : styles.yyy` — ternary (both branches captured)
//! - `{ [styles.xxx]: prop }` — computed property in classnames object
//! - `!prop && styles.xxx` — negated prop
//! - `prop || styles.xxx` — logical OR (less common)

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use std::collections::{BTreeMap, BTreeSet};

/// Extract prop-to-style bindings from a component's source.
///
/// Returns a map of prop_name → set of CSS token strings that the prop
/// conditionally controls (e.g., `"isScrollable"` → `{"styles.modifiers.scrollable"}`).
///
/// Only considers identifiers that appear in the provided `known_props` set,
/// to avoid mapping arbitrary variables.
pub fn extract_prop_style_bindings(
    source: &str,
    known_props: &BTreeSet<String>,
) -> BTreeMap<String, BTreeSet<String>> {
    let allocator = Allocator::default();
    let source_type = SourceType::tsx();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    let mut bindings: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    // Walk all statements looking for className attributes in JSX
    for stmt in &parsed.program.body {
        collect_from_statement(stmt, source, known_props, &mut bindings);
    }

    bindings
}

fn collect_from_statement(
    stmt: &Statement<'_>,
    source: &str,
    known_props: &BTreeSet<String>,
    bindings: &mut BTreeMap<String, BTreeSet<String>>,
) {
    match stmt {
        Statement::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                for s in &body.statements {
                    collect_from_statement(s, source, known_props, bindings);
                }
            }
        }
        Statement::VariableDeclaration(decl) => {
            for d in &decl.declarations {
                if let Some(init) = &d.init {
                    collect_from_expression(init, source, known_props, bindings);
                }
            }
        }
        Statement::ReturnStatement(ret) => {
            if let Some(arg) = &ret.argument {
                collect_from_expression(arg, source, known_props, bindings);
            }
        }
        Statement::ExpressionStatement(expr) => {
            collect_from_expression(&expr.expression, source, known_props, bindings);
        }
        Statement::ExportNamedDeclaration(decl) => {
            if let Some(d) = &decl.declaration {
                match d {
                    Declaration::FunctionDeclaration(f) => {
                        if let Some(body) = &f.body {
                            for s in &body.statements {
                                collect_from_statement(s, source, known_props, bindings);
                            }
                        }
                    }
                    Declaration::VariableDeclaration(v) => {
                        for d in &v.declarations {
                            if let Some(init) = &d.init {
                                collect_from_expression(init, source, known_props, bindings);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Statement::ExportDefaultDeclaration(decl) => {
            if let Some(expr) = decl.declaration.as_expression() {
                collect_from_expression(expr, source, known_props, bindings);
            }
        }
        Statement::BlockStatement(block) => {
            for s in &block.body {
                collect_from_statement(s, source, known_props, bindings);
            }
        }
        Statement::IfStatement(if_stmt) => {
            collect_from_statement(&if_stmt.consequent, source, known_props, bindings);
            if let Some(alt) = &if_stmt.alternate {
                collect_from_statement(alt, source, known_props, bindings);
            }
        }
        _ => {}
    }
}

fn collect_from_expression(
    expr: &Expression<'_>,
    source: &str,
    known_props: &BTreeSet<String>,
    bindings: &mut BTreeMap<String, BTreeSet<String>>,
) {
    match expr {
        Expression::JSXElement(el) => {
            collect_from_jsx_element(el, source, known_props, bindings);
        }
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                if let JSXChild::Element(el) = child {
                    collect_from_jsx_element(el, source, known_props, bindings);
                }
            }
        }
        Expression::ArrowFunctionExpression(arrow) => {
            for s in &arrow.body.statements {
                collect_from_statement(s, source, known_props, bindings);
            }
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                for s in &body.statements {
                    collect_from_statement(s, source, known_props, bindings);
                }
            }
        }
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    collect_from_expression(e, source, known_props, bindings);
                }
            }
        }
        Expression::ParenthesizedExpression(p) => {
            collect_from_expression(&p.expression, source, known_props, bindings);
        }
        Expression::ConditionalExpression(c) => {
            collect_from_expression(&c.consequent, source, known_props, bindings);
            collect_from_expression(&c.alternate, source, known_props, bindings);
        }
        _ => {}
    }
}

fn collect_from_jsx_element(
    el: &JSXElement<'_>,
    source: &str,
    known_props: &BTreeSet<String>,
    bindings: &mut BTreeMap<String, BTreeSet<String>>,
) {
    // Check attributes for className
    for attr in &el.opening_element.attributes {
        if let JSXAttributeItem::Attribute(attr) = attr {
            let attr_name = match &attr.name {
                JSXAttributeName::Identifier(id) => id.name.as_str(),
                _ => continue,
            };
            if attr_name != "className" && attr_name != "class" {
                continue;
            }
            if let Some(JSXAttributeValue::ExpressionContainer(container)) = &attr.value {
                if let Some(expr) = container.expression.as_expression() {
                    extract_bindings_from_classname(expr, source, known_props, bindings);
                }
            }
        }
    }

    // Recurse into children
    for child in &el.children {
        if let JSXChild::Element(child_el) = child {
            collect_from_jsx_element(child_el, source, known_props, bindings);
        }
    }
}

/// Extract prop→style bindings from a className expression.
///
/// Handles:
/// - `css(styles.base, prop && styles.modifier, ...)` — css() call args
/// - `classNames({ [styles.modifier]: prop, ... })` — object expression
/// - Direct `prop && styles.modifier` or `prop ? styles.a : styles.b`
fn extract_bindings_from_classname(
    expr: &Expression<'_>,
    source: &str,
    known_props: &BTreeSet<String>,
    bindings: &mut BTreeMap<String, BTreeSet<String>>,
) {
    match expr {
        // css(...) or classNames(...) — process each argument
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    extract_bindings_from_classname(e, source, known_props, bindings);
                }
            }
        }
        // prop && styles.modifier
        Expression::LogicalExpression(logical) => {
            if let LogicalOperator::And = logical.operator {
                let prop = extract_prop_name(&logical.left, known_props);
                let token = extract_style_token(&logical.right, source);
                if let (Some(p), Some(t)) = (prop, token) {
                    bindings.entry(p).or_default().insert(t);
                }
                // Also check the negated form: !prop && styles.xxx
                if let Expression::UnaryExpression(unary) = &logical.left {
                    if let UnaryOperator::LogicalNot = unary.operator {
                        let prop = extract_prop_name(&unary.argument, known_props);
                        let token = extract_style_token(&logical.right, source);
                        if let (Some(p), Some(t)) = (prop, token) {
                            bindings.entry(p).or_default().insert(t);
                        }
                    }
                }
            }
        }
        // prop ? styles.a : styles.b — both branches are controlled by prop
        Expression::ConditionalExpression(cond) => {
            let prop = extract_prop_name(&cond.test, known_props);
            if let Some(p) = prop {
                if let Some(t) = extract_style_token(&cond.consequent, source) {
                    bindings.entry(p.clone()).or_default().insert(t);
                }
                if let Some(t) = extract_style_token(&cond.alternate, source) {
                    bindings.entry(p).or_default().insert(t);
                }
            }
        }
        // { [styles.modifier]: prop } — computed property with prop as value
        Expression::ObjectExpression(obj) => {
            for prop_item in &obj.properties {
                if let ObjectPropertyKind::ObjectProperty(prop) = prop_item {
                    if prop.computed {
                        // { [styles.xxx]: propName }
                        let token = extract_style_token_from_property_key(&prop.key, source);
                        let prop_name = extract_prop_name(&prop.value, known_props);
                        if let (Some(t), Some(p)) = (token, prop_name) {
                            bindings.entry(p).or_default().insert(t);
                        }
                    }
                }
            }
        }
        Expression::ParenthesizedExpression(p) => {
            extract_bindings_from_classname(&p.expression, source, known_props, bindings);
        }
        // Template literal: `${prop && styles.xxx} ...`
        Expression::TemplateLiteral(tmpl) => {
            for expr in &tmpl.expressions {
                extract_bindings_from_classname(expr, source, known_props, bindings);
            }
        }
        _ => {}
    }
}

/// Extract a prop name from an expression, if it's a known prop identifier.
fn extract_prop_name(expr: &Expression<'_>, known_props: &BTreeSet<String>) -> Option<String> {
    match expr {
        Expression::Identifier(id) => {
            let name = id.name.to_string();
            if known_props.contains(&name) {
                Some(name)
            } else {
                None
            }
        }
        // !prop
        Expression::UnaryExpression(unary) => {
            if let UnaryOperator::LogicalNot = unary.operator {
                extract_prop_name(&unary.argument, known_props)
            } else {
                None
            }
        }
        Expression::ParenthesizedExpression(p) => extract_prop_name(&p.expression, known_props),
        _ => None,
    }
}

/// Extract a CSS style token string from an expression like `styles.modifiers.scrollable`.
fn extract_style_token(expr: &Expression<'_>, source: &str) -> Option<String> {
    match expr {
        Expression::StaticMemberExpression(_) | Expression::ComputedMemberExpression(_) => {
            let span = expr.span();
            let text = &source[span.start as usize..span.end as usize];
            // Only return if it looks like a styles.xxx reference
            if text.starts_with("styles.") || text.starts_with("styles[") {
                Some(text.to_string())
            } else {
                None
            }
        }
        Expression::Identifier(id) => {
            let name = id.name.as_str();
            if name.starts_with("styles") {
                Some(name.to_string())
            } else {
                None
            }
        }
        Expression::ParenthesizedExpression(p) => extract_style_token(&p.expression, source),
        _ => None,
    }
}

/// Extract a CSS style token from a computed property key like `[styles.modifiers.scrollable]`.
fn extract_style_token_from_property_key(key: &PropertyKey<'_>, source: &str) -> Option<String> {
    match key {
        PropertyKey::StaticMemberExpression(member) => {
            let span = member.span();
            let text = &source[span.start as usize..span.end as usize];
            if text.starts_with("styles.") {
                Some(text.to_string())
            } else {
                None
            }
        }
        _ => {
            // PropertyKey variants that can be treated as expressions
            match key {
                PropertyKey::StaticMemberExpression(_) => unreachable!(), // handled above
                PropertyKey::ComputedMemberExpression(m) => {
                    let span = m.span();
                    let text = &source[span.start as usize..span.end as usize];
                    if text.starts_with("styles.") {
                        Some(text.to_string())
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(source: &str, props: &[&str]) -> BTreeMap<String, BTreeSet<String>> {
        let known: BTreeSet<String> = props.iter().map(|s| s.to_string()).collect();
        extract_prop_style_bindings(source, &known)
    }

    #[test]
    fn test_logical_and_binding() {
        let source = r#"
            const Menu = ({ isScrollable, isPlain }) => (
                <div className={css(
                    styles.menu,
                    isScrollable && styles.modifiers.scrollable,
                    isPlain && styles.modifiers.plain,
                )} />
            );
        "#;

        let bindings = extract(source, &["isScrollable", "isPlain"]);
        assert_eq!(
            bindings.get("isScrollable"),
            Some(&BTreeSet::from(["styles.modifiers.scrollable".into()]))
        );
        assert_eq!(
            bindings.get("isPlain"),
            Some(&BTreeSet::from(["styles.modifiers.plain".into()]))
        );
    }

    #[test]
    fn test_ternary_binding() {
        let source = r#"
            const Button = ({ isActive }) => (
                <button className={css(
                    isActive ? styles.modifiers.active : styles.modifiers.inactive,
                )} />
            );
        "#;

        let bindings = extract(source, &["isActive"]);
        let tokens = bindings.get("isActive").unwrap();
        assert!(tokens.contains("styles.modifiers.active"));
        assert!(tokens.contains("styles.modifiers.inactive"));
    }

    #[test]
    fn test_object_computed_binding() {
        let source = r#"
            const Card = ({ isFlat }) => (
                <div className={classNames({
                    [styles.modifiers.flat]: isFlat,
                })} />
            );
        "#;

        let bindings = extract(source, &["isFlat"]);
        assert_eq!(
            bindings.get("isFlat"),
            Some(&BTreeSet::from(["styles.modifiers.flat".into()]))
        );
    }

    #[test]
    fn test_negated_prop() {
        let source = r#"
            const Nav = ({ isDisabled }) => (
                <nav className={css(
                    !isDisabled && styles.modifiers.enabled,
                )} />
            );
        "#;

        let bindings = extract(source, &["isDisabled"]);
        assert_eq!(
            bindings.get("isDisabled"),
            Some(&BTreeSet::from(["styles.modifiers.enabled".into()]))
        );
    }

    #[test]
    fn test_non_prop_ignored() {
        let source = r#"
            const Menu = ({ isScrollable }) => {
                const localVar = true;
                return (
                    <div className={css(
                        localVar && styles.modifiers.local,
                        isScrollable && styles.modifiers.scrollable,
                    )} />
                );
            };
        "#;

        let bindings = extract(source, &["isScrollable"]);
        assert!(
            !bindings.contains_key("localVar"),
            "localVar is not a known prop"
        );
        assert!(bindings.contains_key("isScrollable"));
    }

    #[test]
    fn test_no_style_tokens() {
        let source = r#"
            const Box = ({ isActive }) => (
                <div className={isActive ? "active" : "inactive"} />
            );
        "#;

        let bindings = extract(source, &["isActive"]);
        assert!(bindings.is_empty(), "String literals are not style tokens");
    }
}
