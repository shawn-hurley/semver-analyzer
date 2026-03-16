//! Deterministic JSX render output differ.
//!
//! Compares the JSX trees returned by a component function at two git refs
//! to detect DOM structure, CSS class, ARIA attribute, and data attribute
//! changes that are invisible to `.d.ts` type signature analysis.
//!
//! This plugs into the BU pipeline: for each `ChangedFunction` whose body
//! contains JSX, we parse both versions, extract the JSX return tree, and
//! diff element names, attributes, and structure.

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;
use semver_analyzer_core::{BehavioralCategory, JsxChange};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Compare JSX render output between old and new function bodies.
///
/// Returns a list of `JsxChange` entries for each detected difference
/// in element names, ARIA attributes, CSS classes, roles, and data attributes.
///
/// This is deterministic — no LLM involved. Confidence is 0.90.
pub fn diff_jsx_bodies(
    old_body: &str,
    new_body: &str,
    symbol: &str,
    file: &Path,
) -> Vec<JsxChange> {
    let old_info = extract_jsx_info(old_body);
    let new_info = extract_jsx_info(new_body);

    let mut changes = Vec::new();

    // 1. Element tag changes (DOM structure)
    diff_element_tags(&old_info, &new_info, symbol, file, &mut changes);

    // 2. ARIA attribute changes (accessibility)
    diff_aria_attrs(&old_info, &new_info, symbol, file, &mut changes);

    // 3. Role attribute changes (accessibility)
    diff_role_attrs(&old_info, &new_info, symbol, file, &mut changes);

    // 4. CSS class changes
    diff_css_classes(&old_info, &new_info, symbol, file, &mut changes);

    // 5. Data attribute changes
    diff_data_attrs(&old_info, &new_info, symbol, file, &mut changes);

    changes
}

// ── JSX info extraction ─────────────────────────────────────────────────

/// Aggregated information extracted from all JSX in a function body.
#[derive(Debug, Default)]
struct JsxInfo {
    /// All element tag names used, with count (e.g., "div" → 3, "Button" → 1).
    element_tags: BTreeMap<String, usize>,
    /// All ARIA attributes found: (element_context, attr_name) → attr_value.
    aria_attrs: BTreeMap<(String, String), String>,
    /// All role attributes: element_context → role_value.
    role_attrs: BTreeMap<String, String>,
    /// All CSS class names referenced in className attributes.
    css_classes: BTreeSet<String>,
    /// All data-* attributes: (element_context, attr_name) → attr_value.
    data_attrs: BTreeMap<(String, String), String>,
    /// Total JSX element count (structural complexity metric).
    element_count: usize,
}

/// Parse a function body and extract JSX information.
fn extract_jsx_info(body: &str) -> JsxInfo {
    let allocator = Allocator::default();
    let source_type = SourceType::tsx();

    // Wrap the body in a function so it parses as a complete program
    let wrapped = format!("function __wrapper() {}", body);
    let parsed = Parser::new(&allocator, &wrapped, source_type).parse();

    let mut info = JsxInfo::default();
    walk_statements(&parsed.program.body, &wrapped, &mut info);
    info
}

/// Walk statements to find JSX elements.
fn walk_statements<'a>(stmts: &'a [Statement<'a>], source: &str, info: &mut JsxInfo) {
    for stmt in stmts {
        walk_statement(stmt, source, info);
    }
}

fn walk_statement<'a>(stmt: &'a Statement<'a>, source: &str, info: &mut JsxInfo) {
    match stmt {
        Statement::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                walk_statements(&body.statements, source, info);
            }
        }
        Statement::ReturnStatement(ret) => {
            if let Some(expr) = &ret.argument {
                walk_expression(expr, source, info);
            }
        }
        Statement::ExpressionStatement(expr_stmt) => {
            walk_expression(&expr_stmt.expression, source, info);
        }
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                if let Some(init) = &declarator.init {
                    walk_expression(init, source, info);
                }
            }
        }
        Statement::IfStatement(if_stmt) => {
            walk_statement(&if_stmt.consequent, source, info);
            if let Some(alt) = &if_stmt.alternate {
                walk_statement(alt, source, info);
            }
        }
        Statement::BlockStatement(block) => {
            walk_statements(&block.body, source, info);
        }
        _ => {}
    }
}

fn walk_expression<'a>(expr: &'a Expression<'a>, source: &str, info: &mut JsxInfo) {
    match expr {
        Expression::JSXElement(el) => {
            visit_jsx_element(el, source, info);
        }
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                walk_jsx_child(child, source, info);
            }
        }
        Expression::ParenthesizedExpression(paren) => {
            walk_expression(&paren.expression, source, info);
        }
        Expression::ConditionalExpression(cond) => {
            walk_expression(&cond.consequent, source, info);
            walk_expression(&cond.alternate, source, info);
        }
        Expression::LogicalExpression(logical) => {
            walk_expression(&logical.right, source, info);
        }
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Argument::SpreadElement(spread) = arg {
                    walk_expression(&spread.argument, source, info);
                } else if let Some(expr) = arg.as_expression() {
                    walk_expression(expr, source, info);
                }
            }
        }
        Expression::ArrowFunctionExpression(arrow) => {
            if arrow.expression {
                walk_statements(&arrow.body.statements, source, info);
            } else {
                walk_statements(&arrow.body.statements, source, info);
            }
        }
        _ => {}
    }
}

fn walk_jsx_child<'a>(child: &'a JSXChild<'a>, source: &str, info: &mut JsxInfo) {
    match child {
        JSXChild::Element(el) => visit_jsx_element(el, source, info),
        JSXChild::Fragment(frag) => {
            for c in &frag.children {
                walk_jsx_child(c, source, info);
            }
        }
        JSXChild::ExpressionContainer(container) => {
            if let Some(expr) = container.expression.as_expression() {
                walk_expression(expr, source, info);
            }
        }
        JSXChild::Spread(spread) => {
            walk_expression(&spread.expression, source, info);
        }
        _ => {}
    }
}

/// Visit a JSX element, extracting its tag name, attributes, and children.
fn visit_jsx_element<'a>(el: &'a JSXElement<'a>, source: &str, info: &mut JsxInfo) {
    let tag_name = jsx_element_name(&el.opening_element.name);
    info.element_count += 1;

    // Count element tags
    *info.element_tags.entry(tag_name.clone()).or_insert(0) += 1;

    // Extract attributes
    for attr_item in &el.opening_element.attributes {
        if let JSXAttributeItem::Attribute(attr) = attr_item {
            let attr_name = jsx_attr_name(&attr.name);
            let attr_value = attr
                .value
                .as_ref()
                .map(|v| jsx_attr_value(v, source))
                .unwrap_or_default();

            if attr_name.starts_with("aria-") {
                info.aria_attrs
                    .insert((tag_name.clone(), attr_name), attr_value);
            } else if attr_name == "role" {
                info.role_attrs.insert(tag_name.clone(), attr_value);
            } else if attr_name == "className" || attr_name == "class" {
                // Extract individual CSS class names from the value
                for class in extract_css_classes(&attr_value) {
                    info.css_classes.insert(class);
                }
            } else if attr_name.starts_with("data-") {
                info.data_attrs
                    .insert((tag_name.clone(), attr_name), attr_value);
            }
        }
    }

    // Recurse into children
    for child in &el.children {
        walk_jsx_child(child, source, info);
    }
}

// ── Diffing functions ───────────────────────────────────────────────────

fn diff_element_tags(
    old: &JsxInfo,
    new: &JsxInfo,
    symbol: &str,
    file: &Path,
    changes: &mut Vec<JsxChange>,
) {
    // Elements removed entirely
    for (tag, count) in &old.element_tags {
        if !new.element_tags.contains_key(tag) {
            changes.push(JsxChange {
                symbol: symbol.to_string(),
                file: file.to_path_buf(),
                category: BehavioralCategory::DomStructure,
                description: format!(
                    "<{}> element removed from render output ({} instance{})",
                    tag,
                    count,
                    if *count > 1 { "s" } else { "" }
                ),
                before: Some(format!("<{}>", tag)),
                after: None,
            });
        }
    }

    // Elements added
    for (tag, count) in &new.element_tags {
        if !old.element_tags.contains_key(tag) {
            changes.push(JsxChange {
                symbol: symbol.to_string(),
                file: file.to_path_buf(),
                category: BehavioralCategory::DomStructure,
                description: format!(
                    "<{}> element added to render output ({} instance{})",
                    tag,
                    count,
                    if *count > 1 { "s" } else { "" }
                ),
                before: None,
                after: Some(format!("<{}>", tag)),
            });
        }
    }

    // Significant count changes (wrapper elements added/removed)
    for (tag, old_count) in &old.element_tags {
        if let Some(new_count) = new.element_tags.get(tag) {
            let diff = (*new_count as i64) - (*old_count as i64);
            if diff.abs() >= 2
                || (diff.abs() >= 1 && tag.chars().next().map_or(false, |c| c.is_lowercase()))
            {
                // Only report for HTML elements (lowercase), not components
                if tag.chars().next().map_or(false, |c| c.is_lowercase()) {
                    let desc = if diff > 0 {
                        format!(
                            "{} additional <{}> wrapper element{} added",
                            diff,
                            tag,
                            if diff > 1 { "s" } else { "" }
                        )
                    } else {
                        format!(
                            "{} <{}> element{} removed",
                            diff.abs(),
                            tag,
                            if diff.abs() > 1 { "s" } else { "" }
                        )
                    };
                    changes.push(JsxChange {
                        symbol: symbol.to_string(),
                        file: file.to_path_buf(),
                        category: BehavioralCategory::DomStructure,
                        description: desc,
                        before: Some(format!("{} × <{}>", old_count, tag)),
                        after: Some(format!("{} × <{}>", new_count, tag)),
                    });
                }
            }
        }
    }
}

fn diff_aria_attrs(
    old: &JsxInfo,
    new: &JsxInfo,
    symbol: &str,
    file: &Path,
    changes: &mut Vec<JsxChange>,
) {
    // Removed ARIA attributes
    for ((element, attr), value) in &old.aria_attrs {
        if !new
            .aria_attrs
            .contains_key(&(element.clone(), attr.clone()))
        {
            changes.push(JsxChange {
                symbol: symbol.to_string(),
                file: file.to_path_buf(),
                category: BehavioralCategory::Accessibility,
                description: format!("{} attribute removed from <{}>", attr, element),
                before: Some(format!("{}=\"{}\"", attr, value)),
                after: None,
            });
        }
    }

    // Added ARIA attributes
    for ((element, attr), value) in &new.aria_attrs {
        if !old
            .aria_attrs
            .contains_key(&(element.clone(), attr.clone()))
        {
            changes.push(JsxChange {
                symbol: symbol.to_string(),
                file: file.to_path_buf(),
                category: BehavioralCategory::Accessibility,
                description: format!("{} attribute added to <{}>", attr, element),
                before: None,
                after: Some(format!("{}=\"{}\"", attr, value)),
            });
        }
    }

    // Changed ARIA attribute values
    for ((element, attr), old_val) in &old.aria_attrs {
        if let Some(new_val) = new.aria_attrs.get(&(element.clone(), attr.clone())) {
            if old_val != new_val && !old_val.is_empty() && !new_val.is_empty() {
                changes.push(JsxChange {
                    symbol: symbol.to_string(),
                    file: file.to_path_buf(),
                    category: BehavioralCategory::Accessibility,
                    description: format!("{} value changed on <{}>", attr, element),
                    before: Some(format!("{}=\"{}\"", attr, old_val)),
                    after: Some(format!("{}=\"{}\"", attr, new_val)),
                });
            }
        }
    }
}

fn diff_role_attrs(
    old: &JsxInfo,
    new: &JsxInfo,
    symbol: &str,
    file: &Path,
    changes: &mut Vec<JsxChange>,
) {
    // Removed roles
    for (element, role) in &old.role_attrs {
        if !new.role_attrs.contains_key(element) {
            changes.push(JsxChange {
                symbol: symbol.to_string(),
                file: file.to_path_buf(),
                category: BehavioralCategory::Accessibility,
                description: format!("role=\"{}\" removed from <{}>", role, element),
                before: Some(format!("role=\"{}\"", role)),
                after: None,
            });
        }
    }

    // Changed roles
    for (element, old_role) in &old.role_attrs {
        if let Some(new_role) = new.role_attrs.get(element) {
            if old_role != new_role {
                changes.push(JsxChange {
                    symbol: symbol.to_string(),
                    file: file.to_path_buf(),
                    category: BehavioralCategory::Accessibility,
                    description: format!(
                        "role changed on <{}> from \"{}\" to \"{}\"",
                        element, old_role, new_role
                    ),
                    before: Some(format!("role=\"{}\"", old_role)),
                    after: Some(format!("role=\"{}\"", new_role)),
                });
            }
        }
    }
}

fn diff_css_classes(
    old: &JsxInfo,
    new: &JsxInfo,
    symbol: &str,
    file: &Path,
    changes: &mut Vec<JsxChange>,
) {
    // Removed classes
    for class in old.css_classes.difference(&new.css_classes) {
        changes.push(JsxChange {
            symbol: symbol.to_string(),
            file: file.to_path_buf(),
            category: BehavioralCategory::CssClass,
            description: format!("CSS class '{}' removed from render output", class),
            before: Some(class.clone()),
            after: None,
        });
    }

    // Added classes
    for class in new.css_classes.difference(&old.css_classes) {
        changes.push(JsxChange {
            symbol: symbol.to_string(),
            file: file.to_path_buf(),
            category: BehavioralCategory::CssClass,
            description: format!("CSS class '{}' added to render output", class),
            before: None,
            after: Some(class.clone()),
        });
    }
}

fn diff_data_attrs(
    old: &JsxInfo,
    new: &JsxInfo,
    symbol: &str,
    file: &Path,
    changes: &mut Vec<JsxChange>,
) {
    // Removed data attributes
    for ((element, attr), value) in &old.data_attrs {
        if !new
            .data_attrs
            .contains_key(&(element.clone(), attr.clone()))
        {
            changes.push(JsxChange {
                symbol: symbol.to_string(),
                file: file.to_path_buf(),
                category: BehavioralCategory::DataAttribute,
                description: format!("{} removed from <{}>", attr, element),
                before: Some(format!("{}=\"{}\"", attr, value)),
                after: None,
            });
        }
    }

    // Changed data attributes
    for ((element, attr), old_val) in &old.data_attrs {
        if let Some(new_val) = new.data_attrs.get(&(element.clone(), attr.clone())) {
            if old_val != new_val && !old_val.is_empty() && !new_val.is_empty() {
                changes.push(JsxChange {
                    symbol: symbol.to_string(),
                    file: file.to_path_buf(),
                    category: BehavioralCategory::DataAttribute,
                    description: format!("{} value changed on <{}>", attr, element),
                    before: Some(format!("{}=\"{}\"", attr, old_val)),
                    after: Some(format!("{}=\"{}\"", attr, new_val)),
                });
            }
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Extract the tag name from a JSX element name node.
fn jsx_element_name(name: &JSXElementName<'_>) -> String {
    match name {
        JSXElementName::Identifier(id) => id.name.to_string(),
        JSXElementName::IdentifierReference(id) => id.name.to_string(),
        JSXElementName::NamespacedName(ns) => {
            format!("{}:{}", ns.namespace.name, ns.name.name)
        }
        JSXElementName::MemberExpression(member) => jsx_member_expr_name(member),
        JSXElementName::ThisExpression(_) => "this".to_string(),
    }
}

/// Extract the name from a JSX member expression (e.g., `Foo.Bar`).
fn jsx_member_expr_name(member: &JSXMemberExpression<'_>) -> String {
    let object = match &member.object {
        JSXMemberExpressionObject::IdentifierReference(id) => id.name.to_string(),
        JSXMemberExpressionObject::MemberExpression(inner) => jsx_member_expr_name(inner),
        JSXMemberExpressionObject::ThisExpression(_) => "this".to_string(),
    };
    format!("{}.{}", object, member.property.name)
}

/// Extract the name from a JSX attribute name node.
fn jsx_attr_name(name: &JSXAttributeName<'_>) -> String {
    match name {
        JSXAttributeName::Identifier(id) => id.name.to_string(),
        JSXAttributeName::NamespacedName(ns) => {
            format!("{}:{}", ns.namespace.name, ns.name.name)
        }
    }
}

/// Extract a string value from a JSX attribute value.
fn jsx_attr_value(value: &JSXAttributeValue<'_>, source: &str) -> String {
    match value {
        JSXAttributeValue::StringLiteral(s) => s.value.to_string(),
        JSXAttributeValue::ExpressionContainer(container) => {
            // For expressions, return the source text as-is
            let span = container.span;
            source
                .get(span.start as usize..span.end as usize)
                .unwrap_or("{...}")
                .trim_start_matches('{')
                .trim_end_matches('}')
                .trim()
                .to_string()
        }
        _ => String::new(),
    }
}

/// Extract individual CSS class names from a className attribute value.
///
/// Handles:
/// - String literals: `"pf-v5-c-button pf-m-primary"` → `["pf-v5-c-button", "pf-m-primary"]`
/// - Template literals and expressions are kept as-is if they look like class names
fn extract_css_classes(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .filter(|s| {
            // Keep strings that look like CSS class names
            !s.is_empty()
                && !s.starts_with('{')
                && !s.starts_with('$')
                && !s.contains('(')
                && !s.contains('?')
        })
        .map(|s| s.to_string())
        .collect()
}

/// Returns true if the body contains JSX (quick check before full parsing).
pub fn body_contains_jsx(body: &str) -> bool {
    // Quick heuristic: check for JSX-like angle brackets that aren't comparison operators
    body.contains("</") || body.contains("/>") || body.contains("React.createElement")
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_no_jsx_returns_empty() {
        let changes = diff_jsx_bodies(
            "{ return 42; }",
            "{ return 43; }",
            "foo",
            &PathBuf::from("test.tsx"),
        );
        assert!(changes.is_empty());
    }

    #[test]
    fn test_element_added() {
        let old = "{ return <div>hello</div>; }";
        let new = "{ return <div><section>hello</section></div>; }";
        let changes = diff_jsx_bodies(old, new, "MyComponent", &PathBuf::from("test.tsx"));

        let dom_changes: Vec<_> = changes
            .iter()
            .filter(|c| c.category == BehavioralCategory::DomStructure)
            .collect();
        assert!(!dom_changes.is_empty());
        assert!(dom_changes
            .iter()
            .any(|c| c.description.contains("section")));
    }

    #[test]
    fn test_element_removed() {
        let old = "{ return <div><span>text</span></div>; }";
        let new = "{ return <div>text</div>; }";
        let changes = diff_jsx_bodies(old, new, "Comp", &PathBuf::from("test.tsx"));

        let dom = changes
            .iter()
            .filter(|c| c.category == BehavioralCategory::DomStructure)
            .collect::<Vec<_>>();
        assert!(!dom.is_empty());
        assert!(dom
            .iter()
            .any(|c| c.description.contains("span") && c.description.contains("removed")));
    }

    #[test]
    fn test_aria_attribute_removed() {
        let old = r#"{ return <button aria-labelledby="title">Click</button>; }"#;
        let new = "{ return <button>Click</button>; }";
        let changes = diff_jsx_bodies(old, new, "MyButton", &PathBuf::from("test.tsx"));

        let a11y = changes
            .iter()
            .filter(|c| c.category == BehavioralCategory::Accessibility)
            .collect::<Vec<_>>();
        assert!(!a11y.is_empty());
        assert!(a11y.iter().any(
            |c| c.description.contains("aria-labelledby") && c.description.contains("removed")
        ));
    }

    #[test]
    fn test_aria_attribute_added() {
        let old = "{ return <div>content</div>; }";
        let new = r#"{ return <div aria-hidden="true">content</div>; }"#;
        let changes = diff_jsx_bodies(old, new, "Panel", &PathBuf::from("test.tsx"));

        let a11y: Vec<_> = changes
            .iter()
            .filter(|c| c.category == BehavioralCategory::Accessibility)
            .collect();
        assert!(!a11y.is_empty());
        assert!(a11y
            .iter()
            .any(|c| c.description.contains("aria-hidden") && c.description.contains("added")));
    }

    #[test]
    fn test_role_changed() {
        let old = r#"{ return <li role="separator"></li>; }"#;
        let new = r#"{ return <li role="presentation"></li>; }"#;
        let changes = diff_jsx_bodies(old, new, "NavSeparator", &PathBuf::from("test.tsx"));

        let a11y: Vec<_> = changes
            .iter()
            .filter(|c| c.category == BehavioralCategory::Accessibility)
            .collect();
        assert_eq!(a11y.len(), 1);
        assert!(a11y[0].description.contains("separator"));
        assert!(a11y[0].description.contains("presentation"));
    }

    #[test]
    fn test_css_class_removed() {
        let old = r#"{ return <div className="pf-v5-c-button pf-m-primary">btn</div>; }"#;
        let new = r#"{ return <div className="pf-v6-c-button pf-m-primary">btn</div>; }"#;
        let changes = diff_jsx_bodies(old, new, "Button", &PathBuf::from("test.tsx"));

        let css: Vec<_> = changes
            .iter()
            .filter(|c| c.category == BehavioralCategory::CssClass)
            .collect();
        assert!(!css.is_empty());
        // pf-v5-c-button removed, pf-v6-c-button added
        assert!(
            css.iter()
                .any(|c| c.description.contains("pf-v5-c-button")
                    && c.description.contains("removed"))
        );
        assert!(css
            .iter()
            .any(|c| c.description.contains("pf-v6-c-button") && c.description.contains("added")));
    }

    #[test]
    fn test_data_attribute_changed() {
        let old = r#"{ return <div data-ouia-component-type="PF4/Button">btn</div>; }"#;
        let new = r#"{ return <div data-ouia-component-type="PF5/Button">btn</div>; }"#;
        let changes = diff_jsx_bodies(old, new, "Button", &PathBuf::from("test.tsx"));

        let data: Vec<_> = changes
            .iter()
            .filter(|c| c.category == BehavioralCategory::DataAttribute)
            .collect();
        assert_eq!(data.len(), 1);
        assert!(data[0].description.contains("data-ouia-component-type"));
    }

    #[test]
    fn test_wrapper_div_added() {
        let old = "{ return <button>Click</button>; }";
        let new = "{ return <div><button>Click</button></div>; }";
        let changes = diff_jsx_bodies(old, new, "Toggle", &PathBuf::from("test.tsx"));

        let dom: Vec<_> = changes
            .iter()
            .filter(|c| c.category == BehavioralCategory::DomStructure)
            .collect();
        assert!(!dom.is_empty());
        assert!(dom
            .iter()
            .any(|c| c.description.contains("div") && c.description.contains("added")));
    }

    #[test]
    fn test_conditional_jsx() {
        // Handles JSX inside ternary expressions
        let old = r#"{ return isOpen ? <div role="dialog">content</div> : null; }"#;
        let new = r#"{ return isOpen ? <section role="dialog">content</section> : null; }"#;
        let changes = diff_jsx_bodies(old, new, "Modal", &PathBuf::from("test.tsx"));

        let dom: Vec<_> = changes
            .iter()
            .filter(|c| c.category == BehavioralCategory::DomStructure)
            .collect();
        // div removed, section added
        assert!(dom
            .iter()
            .any(|c| c.description.contains("div") && c.description.contains("removed")));
        assert!(dom
            .iter()
            .any(|c| c.description.contains("section") && c.description.contains("added")));
    }

    #[test]
    fn test_body_contains_jsx_detection() {
        assert!(body_contains_jsx("{ return <div>hello</div>; }"));
        assert!(body_contains_jsx("{ return <Component />; }"));
        assert!(body_contains_jsx("{ return React.createElement('div'); }"));
        assert!(!body_contains_jsx("{ return 42; }"));
        assert!(!body_contains_jsx("{ if (x < 3) return x; }"));
    }

    #[test]
    fn test_multiple_categories_in_one_diff() {
        let old = r#"{ return <div role="separator" className="pf-v5-sep" aria-label="sep">line</div>; }"#;
        let new = r#"{ return <hr className="pf-v6-sep">line</hr>; }"#;
        let changes = diff_jsx_bodies(old, new, "Sep", &PathBuf::from("test.tsx"));

        let categories: BTreeSet<_> = changes.iter().map(|c| &c.category).collect();
        // Should detect DOM structure (div→hr), CSS class, and accessibility changes
        assert!(categories.contains(&BehavioralCategory::DomStructure));
        assert!(categories.contains(&BehavioralCategory::CssClass));
        assert!(categories.contains(&BehavioralCategory::Accessibility));
    }
}
