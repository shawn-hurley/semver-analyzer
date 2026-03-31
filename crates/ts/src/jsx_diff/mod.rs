//! Deterministic JSX render output differ.
//!
//! Compares the JSX trees returned by a component function at two git refs
//! to detect DOM structure, CSS class, ARIA attribute, and data attribute
//! changes that are invisible to `.d.ts` type signature analysis.
//!
//! This plugs into the BU pipeline: for each `ChangedFunction` whose body
//! contains JSX, we parse both versions, extract the JSX return tree, and
//! diff element names, attributes, and structure.

use crate::language::TsCategory;
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// A change detected by comparing JSX render output between two versions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JsxChange {
    pub symbol: String,
    pub file: std::path::PathBuf,
    pub category: TsCategory,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

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

/// Extract the names of React components rendered in a function body's JSX tree.
///
/// Parses the function body as TSX, walks all JSX elements, and returns
/// all uppercase tag names (React components). Lowercase tags (`div`, `span`)
/// are HTML elements and are excluded.
///
/// This is the JSX language spec: uppercase tags are always component
/// references, lowercase are always HTML elements. This is enforced by
/// React at runtime.
///
/// The results are stored on the `Symbol` as raw data. Filtering against
/// family/package exports happens later during hierarchy computation.
pub fn extract_rendered_components(body: &str) -> Vec<String> {
    let info = extract_jsx_info(body);
    let mut components: Vec<String> = info
        .element_tags
        .keys()
        .filter(|tag| tag.starts_with(|c: char| c.is_uppercase()))
        .cloned()
        .collect();
    components.sort();
    components
}

/// Extract internally rendered components from a full `.tsx` source file.
///
/// Parses the entire source file, walks all function bodies for JSX elements,
/// and returns all uppercase tag names (React components) found in any
/// render tree in the file.
///
/// This is a file-level operation -- it aggregates across all function bodies
/// in the file. For a component file like `Dropdown.tsx`, this captures
/// everything the component renders internally.
pub fn extract_rendered_components_from_source(source: &str) -> Vec<String> {
    let allocator = Allocator::default();
    let source_type = SourceType::tsx();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    let mut info = JsxInfo::default();
    walk_statements(&parsed.program.body, source, &mut info);

    let mut components: Vec<String> = info
        .element_tags
        .keys()
        .filter(|tag| tag.starts_with(|c: char| c.is_uppercase()))
        .cloned()
        .collect();
    components.sort();
    components.dedup();
    components
}

/// Walk a declaration (inner content of export statements).
fn walk_declaration<'a>(decl: &'a Declaration<'a>, source: &str, info: &mut JsxInfo) {
    match decl {
        Declaration::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                walk_statements(&body.statements, source, info);
            }
        }
        Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    walk_expression(init, source, info);
                }
            }
        }
        Declaration::ClassDeclaration(cls) => {
            for item in &cls.body.body {
                if let oxc_ast::ast::ClassElement::MethodDefinition(method) = item {
                    if let Some(body) = &method.value.body {
                        walk_statements(&body.statements, source, info);
                    }
                }
            }
        }
        _ => {}
    }
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
        // Handle export declarations — unwrap to the inner declaration/expression
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                walk_declaration(decl, source, info);
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let Some(expr) = export.declaration.as_expression() {
                walk_expression(expr, source, info);
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
            walk_statements(&arrow.body.statements, source, info);
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

            if attr_name == "className" || attr_name == "class" {
                // Extract CSS classes — use AST-aware extraction to avoid
                // false positives from JS expressions like `styles.modifiers.xxx`
                if let Some(value) = &attr.value {
                    extract_classes_from_jsx_value(value, info);
                }
            } else {
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
                } else if attr_name.starts_with("data-") {
                    info.data_attrs
                        .insert((tag_name.clone(), attr_name), attr_value);
                }
            }

            // Walk JSX elements rendered inside attribute expressions
            // (e.g., menu={<Menu>...</Menu>}). These are internally
            // rendered components, not consumer-provided children.
            if let Some(JSXAttributeValue::ExpressionContainer(container)) = &attr.value {
                if let Some(expr) = container.expression.as_expression() {
                    walk_expression(expr, source, info);
                }
            }
        }
    }

    // Recurse into children
    for child in &el.children {
        walk_jsx_child(child, source, info);
    }
}

/// Extract CSS class names from a JSX attribute value by walking the AST.
///
/// For string literals (`className="pf-c-button pf-m-primary"`), extracts
/// class names directly from the string value.
///
/// For expression containers (`className={css(styles.button, ...)}`), walks
/// into the expression to find only string literals and template literal
/// quasi-elements — skipping JS identifiers, property accesses, and operators
/// that are NOT actual CSS class names.
fn extract_classes_from_jsx_value<'a>(value: &'a JSXAttributeValue<'a>, info: &mut JsxInfo) {
    match value {
        JSXAttributeValue::StringLiteral(s) => {
            // Direct string: className="pf-c-button pf-m-primary"
            for class in extract_css_classes(&s.value) {
                info.css_classes.insert(class);
            }
        }
        JSXAttributeValue::ExpressionContainer(container) => {
            // Expression: className={expr} — walk the AST for string literals only
            if let Some(expr) = container.expression.as_expression() {
                extract_classes_from_expr(expr, info);
            }
        }
        _ => {}
    }
}

/// Recursively extract CSS class name strings from a JS expression.
///
/// Only extracts from:
/// - String literals: `"pf-c-button"`
/// - Template literal quasis: `` `pf-c-button ${dynamic}` ``
/// - Call expression arguments: `css("pf-class", ...)`, `classNames("pf-class")`
/// - Conditional branches: `isActive ? "pf-m-active" : ""`
/// - Logical right-hand: `isBlock && "pf-m-block"`
///
/// Skips:
/// - Identifiers (`className`, `cardWithActions`)
/// - Member expressions (`styles.modifiers.plain`)
/// - Computed access (`styles.modifiers[variant]`)
fn extract_classes_from_expr<'a>(expr: &'a Expression<'a>, info: &mut JsxInfo) {
    match expr {
        Expression::StringLiteral(s) => {
            for class in extract_css_classes(&s.value) {
                info.css_classes.insert(class);
            }
        }
        Expression::TemplateLiteral(tpl) => {
            // Extract from the static text parts (quasis)
            for quasi in &tpl.quasis {
                let raw = quasi.value.raw.as_str();
                for class in extract_css_classes(raw) {
                    info.css_classes.insert(class);
                }
            }
        }
        Expression::CallExpression(call) => {
            // Recurse into arguments: css("pf-class", cond && "other")
            for arg in &call.arguments {
                if let Some(expr) = arg.as_expression() {
                    extract_classes_from_expr(expr, info);
                }
            }
        }
        Expression::ConditionalExpression(cond) => {
            extract_classes_from_expr(&cond.consequent, info);
            extract_classes_from_expr(&cond.alternate, info);
        }
        Expression::LogicalExpression(logical) => {
            // `isBlock && "pf-m-block"` — the class is on the right
            extract_classes_from_expr(&logical.right, info);
        }
        Expression::ParenthesizedExpression(paren) => {
            extract_classes_from_expr(&paren.expression, info);
        }
        Expression::ArrayExpression(arr) => {
            // classNames(["pf-class", cond && "other"])
            for elem in &arr.elements {
                if let Some(expr) = elem.as_expression() {
                    extract_classes_from_expr(expr, info);
                }
            }
        }
        // Skip: identifiers, member expressions, computed access, etc.
        // These are JS variable references (styles.modifiers.xxx), not CSS classes.
        _ => {}
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
                category: TsCategory::DomStructure,
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
                category: TsCategory::DomStructure,
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
                || (diff.abs() >= 1 && tag.chars().next().is_some_and(|c| c.is_lowercase()))
            {
                // Only report for HTML elements (lowercase), not components
                if tag.chars().next().is_some_and(|c| c.is_lowercase()) {
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
                        category: TsCategory::DomStructure,
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
                category: TsCategory::Accessibility,
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
                category: TsCategory::Accessibility,
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
                    category: TsCategory::Accessibility,
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
                category: TsCategory::Accessibility,
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
                    category: TsCategory::Accessibility,
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
            category: TsCategory::CssClass,
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
            category: TsCategory::CssClass,
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
                category: TsCategory::DataAttribute,
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
                    category: TsCategory::DataAttribute,
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

/// Extract individual CSS class names from a string value.
///
/// This is called only with actual string literal content (not JS expressions),
/// so it just needs to split on whitespace and filter out empty tokens and
/// any remaining template expression artifacts.
fn extract_css_classes(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .filter(|s| is_css_class_name(s))
        .map(|s| s.to_string())
        .collect()
}

/// Check if a token looks like a valid CSS class name.
///
/// Accepts: `pf-v5-c-button`, `pf-m-primary`, `my-component`, `active`
/// Rejects: JS identifiers (`className`, `cardWithActions`), operators (`&&`),
///          property access (`styles.modifiers.plain`), syntax (`)`, `,`, `]`)
fn is_css_class_name(s: &str) -> bool {
    if s.is_empty() || s.len() < 2 {
        return false;
    }

    // Reject tokens containing JS syntax characters
    if s.contains('.')
        || s.contains('(')
        || s.contains(')')
        || s.contains('[')
        || s.contains(']')
        || s.contains('{')
        || s.contains('}')
        || s.contains('?')
        || s.contains('!')
        || s.contains('=')
        || s.contains('&')
        || s.contains('|')
        || s.contains(';')
        || s.contains('`')
        || s.contains('$')
        || s.contains(',')
        || s.contains('"')
        || s.contains('\'')
    {
        return false;
    }

    // CSS class names are kebab-case or start with a known prefix
    // Accept: pf-v5-c-button, pf-m-primary, my-component, btn-lg
    // Also accept: single words that are lowercase (common class names)
    let first = s.chars().next().unwrap();

    // Must start with a letter or hyphen (CSS class convention)
    if !first.is_ascii_alphabetic() && first != '-' && first != '_' {
        return false;
    }

    // Reject camelCase identifiers (JS variables) — CSS classes are kebab-case
    // Exception: single-word all-lowercase is fine (e.g., "active", "hidden")
    if s.contains('-') || s.contains('_') {
        // Has separators — looks like a CSS class
        true
    } else {
        // No separators — only accept if all lowercase (not camelCase like "cardWithActions")
        s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    }
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
            .filter(|c| c.category == TsCategory::DomStructure)
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
            .filter(|c| c.category == TsCategory::DomStructure)
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
            .filter(|c| c.category == TsCategory::Accessibility)
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
            .filter(|c| c.category == TsCategory::Accessibility)
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
            .filter(|c| c.category == TsCategory::Accessibility)
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
            .filter(|c| c.category == TsCategory::CssClass)
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
            .filter(|c| c.category == TsCategory::DataAttribute)
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
            .filter(|c| c.category == TsCategory::DomStructure)
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
            .filter(|c| c.category == TsCategory::DomStructure)
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
        assert!(categories.contains(&TsCategory::DomStructure));
        assert!(categories.contains(&TsCategory::CssClass));
        assert!(categories.contains(&TsCategory::Accessibility));
    }

    #[test]
    fn test_expression_classname_skips_js_identifiers() {
        // className={css(styles.button, isBlock && styles.modifiers.block)}
        // Should NOT produce "styles.modifiers.block" or "isBlock" as CSS classes
        let old = r#"{ return <div className={css(styles.button, isBlock && styles.modifiers.block)}>btn</div>; }"#;
        let new = r#"{ return <div className={css(styles.button, isBlock && styles.modifiers.fill)}>btn</div>; }"#;
        let changes = diff_jsx_bodies(old, new, "Button", &PathBuf::from("test.tsx"));

        let css_changes: Vec<_> = changes
            .iter()
            .filter(|c| c.category == TsCategory::CssClass)
            .collect();
        // Should be empty — no actual string literal CSS classes changed
        assert!(
            css_changes.is_empty(),
            "Expression-based classNames should not produce CSS class changes, got: {:?}",
            css_changes
                .iter()
                .map(|c| &c.description)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_expression_classname_extracts_string_literals() {
        // className={css("pf-v5-c-button", isActive && "pf-m-active")}
        // Should extract "pf-v5-c-button" and "pf-m-active" as CSS classes
        let old = r#"{ return <div className={css("pf-v5-c-button", isActive && "pf-m-active")}>btn</div>; }"#;
        let new = r#"{ return <div className={css("pf-v6-c-button", isActive && "pf-m-active")}>btn</div>; }"#;
        let changes = diff_jsx_bodies(old, new, "Button", &PathBuf::from("test.tsx"));

        let css_changes: Vec<_> = changes
            .iter()
            .filter(|c| c.category == TsCategory::CssClass)
            .collect();
        assert!(
            !css_changes.is_empty(),
            "Should detect CSS class changes in string literals within expressions"
        );
        assert!(
            css_changes
                .iter()
                .any(|c| c.description.contains("pf-v5-c-button")
                    && c.description.contains("removed"))
        );
        assert!(css_changes
            .iter()
            .any(|c| c.description.contains("pf-v6-c-button") && c.description.contains("added")));
    }

    #[test]
    fn test_template_literal_classname() {
        // className={`pf-v5-c-button ${isActive ? 'pf-m-active' : ''}`}
        let old = r#"{ return <div className={`pf-v5-c-button ${cond}`}>btn</div>; }"#;
        let new = r#"{ return <div className={`pf-v6-c-button ${cond}`}>btn</div>; }"#;
        let changes = diff_jsx_bodies(old, new, "Button", &PathBuf::from("test.tsx"));

        let css_changes: Vec<_> = changes
            .iter()
            .filter(|c| c.category == TsCategory::CssClass)
            .collect();
        assert!(
            !css_changes.is_empty(),
            "Should detect CSS class changes in template literals"
        );
        assert!(css_changes
            .iter()
            .any(|c| c.description.contains("pf-v5-c-button")));
        assert!(css_changes
            .iter()
            .any(|c| c.description.contains("pf-v6-c-button")));
    }

    #[test]
    fn test_is_css_class_name() {
        // Valid CSS class names
        assert!(is_css_class_name("pf-v5-c-button"));
        assert!(is_css_class_name("pf-m-primary"));
        assert!(is_css_class_name("my-component"));
        assert!(is_css_class_name("active")); // single lowercase word OK

        // Invalid — JS identifiers and syntax
        assert!(!is_css_class_name("styles.modifiers.plain"));
        assert!(!is_css_class_name("cardWithActions")); // camelCase
        assert!(!is_css_class_name("className")); // camelCase
        assert!(!is_css_class_name("isBlock")); // camelCase
        assert!(!is_css_class_name("styles.modifiers.plain)"));
        assert!(!is_css_class_name("&&"));
        assert!(!is_css_class_name("("));
        assert!(!is_css_class_name(""));
        assert!(!is_css_class_name("x")); // too short
    }

    // ── extract_rendered_components tests ────────────────────────────

    #[test]
    fn test_rendered_components_dropdown() {
        // Dropdown renders Menu/MenuContent internally.
        // Consumers pass DropdownList/DropdownGroup/DropdownItem as children.
        let body = r#"{
            return (
                <Menu>
                    <MenuContent>
                        {children}
                    </MenuContent>
                </Menu>
            );
        }"#;
        let rendered = extract_rendered_components(body);
        assert!(rendered.contains(&"Menu".to_string()));
        assert!(rendered.contains(&"MenuContent".to_string()));
        // Consumer children are NOT in the internal render tree
        assert!(!rendered.contains(&"DropdownList".to_string()));
        assert!(!rendered.contains(&"DropdownItem".to_string()));
    }

    #[test]
    fn test_rendered_components_modal() {
        // Modal renders ModalContent internally.
        // Consumers pass ModalHeader/ModalBody/ModalFooter as children.
        let body = r#"{
            return (
                <ModalContent isOpen={isOpen} className={className}>
                    {children}
                </ModalContent>
            );
        }"#;
        let rendered = extract_rendered_components(body);
        assert!(rendered.contains(&"ModalContent".to_string()));
        assert!(!rendered.contains(&"ModalHeader".to_string()));
        assert!(!rendered.contains(&"ModalBody".to_string()));
        assert!(!rendered.contains(&"ModalFooter".to_string()));
    }

    #[test]
    fn test_rendered_components_formfieldgroup_prop_passed() {
        // FormFieldGroup renders header via a prop, not as a JSX child.
        // Only HTML elements (div) in the render tree — no components.
        let body = r#"{
            return (
                <div className={styles.formFieldGroup}>
                    {header && header}
                    <div className={styles.formFieldGroupBody}>
                        {children}
                    </div>
                </div>
            );
        }"#;
        let rendered = extract_rendered_components(body);
        assert!(
            rendered.is_empty(),
            "Expected no components, got: {:?}",
            rendered
        );
    }

    #[test]
    fn test_rendered_components_filters_html_elements() {
        let body = r#"{
            return (
                <div className="wrapper">
                    <span>{label}</span>
                    <Button onClick={onClick}>
                        <Icon />
                    </Button>
                </div>
            );
        }"#;
        let rendered = extract_rendered_components(body);
        assert!(rendered.contains(&"Button".to_string()));
        assert!(rendered.contains(&"Icon".to_string()));
        assert!(!rendered.contains(&"div".to_string()));
        assert!(!rendered.contains(&"span".to_string()));
    }

    #[test]
    fn test_rendered_components_conditional() {
        let body = r#"{
            return (
                <div>
                    {isLoading ? <Spinner /> : <Content>{children}</Content>}
                </div>
            );
        }"#;
        let rendered = extract_rendered_components(body);
        assert!(rendered.contains(&"Spinner".to_string()));
        assert!(rendered.contains(&"Content".to_string()));
    }

    #[test]
    fn test_rendered_components_logical_and() {
        let body = r#"{
            return (
                <div>
                    {showHeader && <PageHeader />}
                    <PageBody>{children}</PageBody>
                </div>
            );
        }"#;
        let rendered = extract_rendered_components(body);
        assert!(rendered.contains(&"PageHeader".to_string()));
        assert!(rendered.contains(&"PageBody".to_string()));
    }

    #[test]
    fn test_rendered_components_empty_body() {
        let body = r#"{ return null; }"#;
        let rendered = extract_rendered_components(body);
        assert!(rendered.is_empty());
    }

    #[test]
    fn test_rendered_components_prop_expression() {
        // Components inside prop expressions (menu={<Menu>...}) are internal
        let body = r#"{
            return (
                <MenuContainer
                    menu={
                        <Menu ref={menuRef} onSelect={onSelectHandler}>
                            <MenuContent>{children}</MenuContent>
                        </Menu>
                    }
                    toggle={toggle}
                    toggleRef={toggleRef}
                >
                </MenuContainer>
            );
        }"#;
        let rendered = extract_rendered_components(body);
        assert!(rendered.contains(&"MenuContainer".to_string()));
        assert!(
            rendered.contains(&"Menu".to_string()),
            "Menu should be detected in prop expression. Rendered: {:?}",
            rendered,
        );
        assert!(rendered.contains(&"MenuContent".to_string()));
    }

    // ── extract_rendered_components_from_source tests ────────────────

    #[test]
    fn test_from_source_simple_component() {
        let source = r#"
            import * as React from 'react';
            import { MenuList } from '../Menu';

            export const DropdownList = ({ children, className, ...props }) => (
                <MenuList className={className} {...props}>
                    {children}
                </MenuList>
            );
        "#;
        let rendered = extract_rendered_components_from_source(source);
        assert!(rendered.contains(&"MenuList".to_string()));
        assert!(!rendered.contains(&"DropdownItem".to_string()));
    }

    #[test]
    fn test_from_source_v5_emptystate() {
        // v5 EmptyState just passes children through — no components rendered
        let source = r#"
            import * as React from 'react';
            import { css } from '@patternfly/react-styles';
            import styles from '@patternfly/react-styles/css/components/EmptyState/empty-state';

            export const EmptyState = ({ children, className, variant, isFullHeight, ...props }) => (
                <div
                    className={css(styles.emptyState, className)}
                    {...props}
                >
                    <div className={css(styles.emptyStateContent)}>{children}</div>
                </div>
            );
        "#;
        let rendered = extract_rendered_components_from_source(source);
        // v5 EmptyState renders no React components internally
        assert!(
            rendered.is_empty(),
            "Expected no components, got: {:?}",
            rendered
        );
    }

    #[test]
    fn test_from_source_v6_emptystate() {
        // v6 EmptyState renders EmptyStateHeader internally
        let source = r#"
            import * as React from 'react';
            import { EmptyStateHeader } from './EmptyStateHeader';
            import { EmptyStateFooter } from './EmptyStateFooter';

            export const EmptyState = ({
                children, className, icon, titleText, headingLevel, status, ...props
            }) => {
                return (
                    <div className={className} {...props}>
                        <div>
                            <EmptyStateHeader icon={icon} titleText={titleText} headingLevel={headingLevel} />
                            {children}
                        </div>
                    </div>
                );
            };
        "#;
        let rendered = extract_rendered_components_from_source(source);
        assert!(
            rendered.contains(&"EmptyStateHeader".to_string()),
            "v6 EmptyState should render EmptyStateHeader internally. Got: {:?}",
            rendered,
        );
    }

    #[test]
    fn test_from_source_hierarchy_delta() {
        // Simulate computing hierarchy delta between v5 and v6
        let v5_source = r#"
            export const EmptyState = ({ children, ...props }) => (
                <div {...props}><div>{children}</div></div>
            );
        "#;
        let v6_source = r#"
            import { EmptyStateHeader } from './EmptyStateHeader';
            export const EmptyState = ({ children, icon, titleText, ...props }) => (
                <div {...props}>
                    <EmptyStateHeader icon={icon} titleText={titleText} />
                    {children}
                </div>
            );
        "#;

        let v5_rendered = extract_rendered_components_from_source(v5_source);
        let v6_rendered = extract_rendered_components_from_source(v6_source);

        // v5: no components rendered internally
        assert!(v5_rendered.is_empty());
        // v6: EmptyStateHeader rendered internally
        assert!(v6_rendered.contains(&"EmptyStateHeader".to_string()));

        // Delta: EmptyStateHeader was ADDED to internal render tree
        // This means it MOVED from consumer-child to internal
        let added_internal: Vec<&String> = v6_rendered
            .iter()
            .filter(|c| !v5_rendered.contains(c))
            .collect();
        assert_eq!(added_internal, vec!["EmptyStateHeader"]);
    }
}
