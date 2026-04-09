//! Children slot tracing through JSX return trees.
//!
//! Traces where `{children}` or `{props.children}` appears in a component's
//! JSX return tree, recording the chain of wrapper components/elements from
//! the root of the return expression down to the children slot.
//!
//! This tells us what internal structure the component wraps around
//! consumer-provided children.
//!
//! Example for Dropdown:
//! ```tsx
//! return (
//!     <Popper popper={
//!         <Menu>
//!             <MenuContent>
//!                 {children}        ← children slot
//!             </MenuContent>
//!         </Menu>
//!     } />
//! );
//! ```
//! Result: children_slot_path = ["Popper", "Menu", "MenuContent"]

use std::collections::HashMap;

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;

/// Trace the path from JSX root to `{children}` in a component's source.
///
/// Returns the chain of component/element names that wrap the children slot.
/// Returns an empty vec if `{children}` is not found in the JSX tree.
pub fn trace_children_slot(source: &str) -> Vec<String> {
    // Use the combined function and discard the detail
    trace_children_slot_both(source).0
}

/// Trace the path from JSX root to `{children}` with CSS token detail.
///
/// Like `trace_children_slot` but also captures `className={styles.xxx}`
/// tokens at each level. Returns `Vec<(tag_name, Option<css_token>)>`.
pub fn trace_children_slot_detail(source: &str) -> Vec<(String, Option<String>)> {
    // Use the combined function and discard the path
    trace_children_slot_both(source).1
}

/// Trace both the simple path and the CSS-token-detailed path in a single
/// AST parse. Returns `(children_slot_path, children_slot_detail)`.
pub fn trace_children_slot_both(source: &str) -> (Vec<String>, Vec<(String, Option<String>)>) {
    let allocator = Allocator::default();
    let source_type = SourceType::tsx();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    let aliases = collect_component_aliases(&parsed.program.body);

    // Run the detail variant (which captures both tag name and CSS token).
    // We derive the simple path from the detail path to avoid a second walk.
    let mut detail_path: Vec<(String, Option<String>)> = Vec::new();
    for stmt in &parsed.program.body {
        if find_children_in_statement_detail(stmt, source, &mut detail_path, &aliases) {
            let simple_path: Vec<String> = detail_path.iter().map(|(tag, _)| tag.clone()).collect();
            return (simple_path, detail_path);
        }
    }

    (Vec::new(), Vec::new())
}

// ── Dynamic component alias resolution ──────────────────────────────────
//
// Design systems commonly use a "polymorphic component" pattern where the
// rendered HTML element comes from a prop with a string default:
//
//   const TdBase = ({ component = 'td', ... }) => {
//     const { component: MergedComponent = component, ... } = merged;
//     return <MergedComponent>{children}</MergedComponent>;
//   };
//
// The JSX AST sees `MergedComponent` (PascalCase), not `td`. We resolve
// these aliases by scanning destructuring patterns for string literal
// defaults, then following identifier-to-identifier chains.

/// Collect a map of variable names to their resolved HTML element defaults.
///
/// Scans all destructuring patterns in the source for assignments like:
///   `component = 'td'`              →  component → "td"
///   `component: Alias = component`  →  Alias → resolve("component") → "td"
///   `const Tag = 'section'`         →  Tag → "section"
fn collect_component_aliases<'a>(body: &'a [Statement<'a>]) -> HashMap<String, String> {
    let mut raw: HashMap<String, AliasValue> = HashMap::new();

    for stmt in body {
        collect_aliases_from_statement(stmt, &mut raw);
    }

    // Resolve chains: if a value is an Ident pointing to another entry, follow it
    resolve_alias_chains(&raw)
}

/// Intermediate representation for alias values before resolution.
#[derive(Clone)]
enum AliasValue {
    /// A resolved string literal like `'td'`
    Literal(String),
    /// An identifier reference like `component` (needs chain resolution)
    Ident(String),
}

fn resolve_alias_chains(raw: &HashMap<String, AliasValue>) -> HashMap<String, String> {
    let mut resolved = HashMap::new();
    for (name, value) in raw {
        if let Some(lit) = resolve_one(value, raw, 0) {
            resolved.insert(name.clone(), lit);
        }
    }
    resolved
}

fn resolve_one(value: &AliasValue, raw: &HashMap<String, AliasValue>, depth: u8) -> Option<String> {
    if depth > 5 {
        return None; // prevent infinite loops
    }
    match value {
        AliasValue::Literal(s) => Some(s.clone()),
        AliasValue::Ident(name) => raw.get(name).and_then(|v| resolve_one(v, raw, depth + 1)),
    }
}

fn collect_aliases_from_statement<'a>(
    stmt: &'a Statement<'a>,
    map: &mut HashMap<String, AliasValue>,
) {
    match stmt {
        Statement::FunctionDeclaration(f) => {
            collect_aliases_from_params(&f.params, map);
            if let Some(body) = &f.body {
                for inner in &body.statements {
                    collect_aliases_from_statement(inner, map);
                }
            }
        }
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                collect_aliases_from_binding(&declarator.id, map);
                // Handle `const Tag = 'section'`
                if let Some(init) = &declarator.init {
                    if let BindingPattern::BindingIdentifier(id) = &declarator.id {
                        match init {
                            Expression::StringLiteral(s) => {
                                map.insert(
                                    id.name.to_string(),
                                    AliasValue::Literal(s.value.to_string()),
                                );
                            }
                            Expression::Identifier(ident) => {
                                map.insert(
                                    id.name.to_string(),
                                    AliasValue::Ident(ident.name.to_string()),
                                );
                            }
                            _ => {
                                // Recurse into call expressions (forwardRef, memo, etc.)
                                collect_aliases_from_expression(init, map);
                            }
                        }
                    } else {
                        // Destructuring: `const { component: X = y } = expr`
                        collect_aliases_from_binding(&declarator.id, map);
                        collect_aliases_from_expression(init, map);
                    }
                }
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                collect_aliases_from_declaration(decl, map);
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let Some(expr) = export.declaration.as_expression() {
                collect_aliases_from_expression(expr, map);
            }
        }
        Statement::BlockStatement(block) => {
            for inner in &block.body {
                collect_aliases_from_statement(inner, map);
            }
        }
        Statement::ReturnStatement(_) | Statement::ExpressionStatement(_) => {
            // These don't introduce bindings
        }
        _ => {}
    }
}

fn collect_aliases_from_declaration<'a>(
    decl: &'a Declaration<'a>,
    map: &mut HashMap<String, AliasValue>,
) {
    match decl {
        Declaration::FunctionDeclaration(f) => {
            collect_aliases_from_params(&f.params, map);
            if let Some(body) = &f.body {
                for stmt in &body.statements {
                    collect_aliases_from_statement(stmt, map);
                }
            }
        }
        Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                collect_aliases_from_binding(&declarator.id, map);
                if let Some(init) = &declarator.init {
                    collect_aliases_from_expression(init, map);
                }
            }
        }
        _ => {}
    }
}

fn collect_aliases_from_expression<'a>(
    expr: &'a Expression<'a>,
    map: &mut HashMap<String, AliasValue>,
) {
    match expr {
        Expression::ArrowFunctionExpression(arrow) => {
            collect_aliases_from_params(&arrow.params, map);
            for stmt in &arrow.body.statements {
                collect_aliases_from_statement(stmt, map);
            }
        }
        Expression::FunctionExpression(func) => {
            collect_aliases_from_params(&func.params, map);
            if let Some(body) = &func.body {
                for stmt in &body.statements {
                    collect_aliases_from_statement(stmt, map);
                }
            }
        }
        Expression::CallExpression(call) => {
            // Handle forwardRef((props) => ...), memo((...) => ...), etc.
            for arg in &call.arguments {
                if let Some(expr) = arg.as_expression() {
                    collect_aliases_from_expression(expr, map);
                }
            }
        }
        Expression::ParenthesizedExpression(paren) => {
            collect_aliases_from_expression(&paren.expression, map);
        }
        _ => {}
    }
}

fn collect_aliases_from_params<'a>(
    params: &'a FormalParameters<'a>,
    map: &mut HashMap<String, AliasValue>,
) {
    for param in &params.items {
        collect_aliases_from_binding(&param.pattern, map);
    }
}

/// Extract alias mappings from destructuring patterns.
///
/// Handles:
///   `{ component = 'td' }`           → component → Literal("td")
///   `{ component: Alias = 'td' }`    → Alias → Literal("td")
///   `{ component: Alias = component }` → Alias → Ident("component")
fn collect_aliases_from_binding<'a>(
    pattern: &'a BindingPattern<'a>,
    map: &mut HashMap<String, AliasValue>,
) {
    if let BindingPattern::ObjectPattern(obj) = pattern {
        for prop in &obj.properties {
            // `{ component = 'td' }` — simple destructuring with default
            if let BindingPattern::AssignmentPattern(assign) = &prop.value {
                let binding_name = binding_pattern_name(&assign.left);
                if let Some(name) = binding_name {
                    match &assign.right {
                        Expression::StringLiteral(s) => {
                            map.insert(name, AliasValue::Literal(s.value.to_string()));
                        }
                        Expression::Identifier(ident) => {
                            map.insert(name, AliasValue::Ident(ident.name.to_string()));
                        }
                        _ => {}
                    }
                }
            }
        }
    } else if let BindingPattern::AssignmentPattern(assign) = pattern {
        let binding_name = binding_pattern_name(&assign.left);
        if let Some(name) = binding_name {
            match &assign.right {
                Expression::StringLiteral(s) => {
                    map.insert(name, AliasValue::Literal(s.value.to_string()));
                }
                Expression::Identifier(ident) => {
                    map.insert(name, AliasValue::Ident(ident.name.to_string()));
                }
                _ => {}
            }
        }
    }
}

fn binding_pattern_name(pattern: &BindingPattern) -> Option<String> {
    match pattern {
        BindingPattern::BindingIdentifier(id) => Some(id.name.to_string()),
        _ => None,
    }
}

/// Check if the source file accepts `children` as a prop at all.
///
/// Looks for `children` in destructuring patterns of function parameters.
pub fn has_children_prop(source: &str) -> bool {
    // Check destructured `children` in function/arrow params (functional components)
    let allocator = Allocator::default();
    let source_type = SourceType::tsx();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    for stmt in &parsed.program.body {
        if check_children_in_statement(stmt) {
            return true;
        }
    }

    // Fallback: check for `children` in the Props interface definition
    // or `this.props.children` / `props.children` usage in class components.
    // This covers class components (e.g., Menu) that access children via
    // this.props.children in their render() method.
    source.contains("children?: React.ReactNode")
        || source.contains("children: React.ReactNode")
        || source.contains("this.props.children")
        || source.contains("props.children")
}

// ── Statement walking ───────────────────────────────────────────────────

#[allow(dead_code)]
fn find_children_in_statement<'a>(
    stmt: &'a Statement<'a>,
    source: &str,
    path: &mut Vec<String>,
    aliases: &HashMap<String, String>,
) -> bool {
    match stmt {
        Statement::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                for inner in &body.statements {
                    if find_children_in_statement(inner, source, path, aliases) {
                        return true;
                    }
                }
            }
        }
        Statement::ReturnStatement(ret) => {
            if let Some(expr) = &ret.argument {
                return find_children_in_expression(expr, source, path, aliases);
            }
        }
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                if let Some(init) = &declarator.init {
                    if find_children_in_expression(init, source, path, aliases) {
                        return true;
                    }
                }
            }
        }
        Statement::ExpressionStatement(expr_stmt) => {
            return find_children_in_expression(&expr_stmt.expression, source, path, aliases);
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                return find_children_in_declaration(decl, source, path, aliases);
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let Some(expr) = export.declaration.as_expression() {
                return find_children_in_expression(expr, source, path, aliases);
            }
        }
        Statement::BlockStatement(block) => {
            for inner in &block.body {
                if find_children_in_statement(inner, source, path, aliases) {
                    return true;
                }
            }
        }
        Statement::IfStatement(if_stmt) => {
            if find_children_in_statement(&if_stmt.consequent, source, path, aliases) {
                return true;
            }
            if let Some(alt) = &if_stmt.alternate {
                if find_children_in_statement(alt, source, path, aliases) {
                    return true;
                }
            }
        }
        Statement::ClassDeclaration(class) => {
            if find_children_in_class_body(&class.body, source, path, aliases) {
                return true;
            }
        }
        _ => {}
    }
    false
}

#[allow(dead_code)]
fn find_children_in_declaration<'a>(
    decl: &'a Declaration<'a>,
    source: &str,
    path: &mut Vec<String>,
    aliases: &HashMap<String, String>,
) -> bool {
    match decl {
        Declaration::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                for stmt in &body.statements {
                    if find_children_in_statement(stmt, source, path, aliases) {
                        return true;
                    }
                }
            }
        }
        Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    if find_children_in_expression(init, source, path, aliases) {
                        return true;
                    }
                }
            }
        }
        Declaration::ClassDeclaration(class) => {
            if find_children_in_class_body(&class.body, source, path, aliases) {
                return true;
            }
        }
        _ => {}
    }
    false
}

/// Walk a class body looking for a `render()` method and trace its return
/// for `{children}` or `{this.props.children}`.
#[allow(dead_code)]
fn find_children_in_class_body<'a>(
    body: &'a ClassBody<'a>,
    source: &str,
    path: &mut Vec<String>,
    aliases: &HashMap<String, String>,
) -> bool {
    for element in &body.body {
        let is_render_named = |key: &PropertyKey| -> bool {
            matches!(key, PropertyKey::StaticIdentifier(id) if id.name == "render")
        };
        match element {
            ClassElement::MethodDefinition(method) if is_render_named(&method.key) => {
                if let Some(body) = &method.value.body {
                    for stmt in &body.statements {
                        if find_children_in_statement(stmt, source, path, aliases) {
                            return true;
                        }
                    }
                }
            }
            ClassElement::PropertyDefinition(prop) if is_render_named(&prop.key) => {
                if let Some(init) = &prop.value {
                    if let Expression::ArrowFunctionExpression(arrow) = init {
                        for stmt in &arrow.body.statements {
                            if find_children_in_statement(stmt, source, path, aliases) {
                                return true;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    false
}

#[allow(dead_code)]
fn find_children_in_expression<'a>(
    expr: &'a Expression<'a>,
    source: &str,
    path: &mut Vec<String>,
    aliases: &HashMap<String, String>,
) -> bool {
    match expr {
        Expression::JSXElement(el) => find_children_in_jsx_element(el, source, path, aliases),
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                if find_children_in_jsx_child(child, source, path, aliases) {
                    return true;
                }
            }
            false
        }
        Expression::ParenthesizedExpression(paren) => {
            find_children_in_expression(&paren.expression, source, path, aliases)
        }
        Expression::ConditionalExpression(cond) => {
            // Check both branches
            find_children_in_expression(&cond.consequent, source, path, aliases)
                || find_children_in_expression(&cond.alternate, source, path, aliases)
        }
        Expression::LogicalExpression(logical) => {
            find_children_in_expression(&logical.right, source, path, aliases)
        }
        Expression::CallExpression(call) => {
            // Handle forwardRef((...) => ...) etc.
            for arg in &call.arguments {
                if let Some(expr) = arg.as_expression() {
                    if find_children_in_expression(expr, source, path, aliases) {
                        return true;
                    }
                }
            }
            false
        }
        Expression::ArrowFunctionExpression(arrow) => {
            for stmt in &arrow.body.statements {
                if find_children_in_statement(stmt, source, path, aliases) {
                    return true;
                }
            }
            false
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                for stmt in &body.statements {
                    if find_children_in_statement(stmt, source, path, aliases) {
                        return true;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

// ── JSX-specific walking ────────────────────────────────────────────────

#[allow(dead_code)]
fn find_children_in_jsx_element<'a>(
    el: &'a JSXElement<'a>,
    source: &str,
    path: &mut Vec<String>,
    aliases: &HashMap<String, String>,
) -> bool {
    let raw_tag = jsx_element_name(&el.opening_element.name);

    // Resolve dynamic component variables to their default HTML element.
    // e.g., <MergedComponent> where MergedComponent defaults to 'td'
    let tag_name = if raw_tag.starts_with(|c: char| c.is_uppercase()) {
        aliases.get(&raw_tag).cloned().unwrap_or(raw_tag)
    } else {
        raw_tag
    };

    // Check JSX props/attributes for children passed as props
    // e.g., <Popper popper={<Menu>{children}</Menu>} />
    for attr_item in &el.opening_element.attributes {
        if let JSXAttributeItem::Attribute(attr) = attr_item {
            if let Some(JSXAttributeValue::ExpressionContainer(container)) = &attr.value {
                if let Some(inner_expr) = container.expression.as_expression() {
                    path.push(tag_name.clone());
                    if find_children_in_expression(inner_expr, source, path, aliases) {
                        return true;
                    }
                    path.pop();
                }
            }
        }
    }

    // Check direct JSX children
    // We push our tag name first, then recurse. If recursion fails, we pop.
    path.push(tag_name.clone());

    for child in &el.children {
        match child {
            JSXChild::ExpressionContainer(container) => {
                if is_children_expression(&container.expression, source) {
                    return true;
                }
                // Could be a more complex expression containing children
                if let Some(expr) = container.expression.as_expression() {
                    if find_children_in_expression(expr, source, path, aliases) {
                        return true;
                    }
                }
            }
            JSXChild::Element(child_el) => {
                if find_children_in_jsx_element(child_el, source, path, aliases) {
                    return true;
                }
            }
            JSXChild::Fragment(frag) => {
                for frag_child in &frag.children {
                    if find_children_in_jsx_child(frag_child, source, path, aliases) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }

    // Children not found in this subtree, remove our tag
    path.pop();
    false
}

#[allow(dead_code)]
fn find_children_in_jsx_child<'a>(
    child: &'a JSXChild<'a>,
    source: &str,
    path: &mut Vec<String>,
    aliases: &HashMap<String, String>,
) -> bool {
    match child {
        JSXChild::Element(el) => find_children_in_jsx_element(el, source, path, aliases),
        JSXChild::ExpressionContainer(container) => {
            if is_children_expression(&container.expression, source) {
                return true;
            }
            if let Some(expr) = container.expression.as_expression() {
                return find_children_in_expression(expr, source, path, aliases);
            }
            false
        }
        JSXChild::Fragment(frag) => {
            for c in &frag.children {
                if find_children_in_jsx_child(c, source, path, aliases) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// Check if a JSX expression container holds `{children}` or `{props.children}`.
///
/// Also handles common patterns like `{mergedChildren || children}` and
/// `{someVar ?? children}` where children appears in a logical expression.
fn is_children_expression(expr: &JSXExpression, _source: &str) -> bool {
    match expr {
        JSXExpression::Identifier(id) if id.name == "children" => true,
        _ => {
            if let Some(inner) = expr.as_expression() {
                is_children_expr_inner(inner)
            } else {
                false
            }
        }
    }
}

fn is_children_expr_inner(expr: &Expression) -> bool {
    match expr {
        Expression::Identifier(id) => id.name == "children",
        Expression::StaticMemberExpression(member) => {
            if member.property.name != "children" {
                return false;
            }
            // Match props.children
            if matches!(&member.object, Expression::Identifier(id) if id.name == "props") {
                return true;
            }
            // Match this.props.children (class components)
            if let Expression::StaticMemberExpression(inner) = &member.object {
                return inner.property.name == "props"
                    && matches!(&inner.object, Expression::ThisExpression(_));
            }
            false
        }
        // Handle {mergedChildren || children}, {x ?? children}, {x && children}
        Expression::LogicalExpression(logical) => {
            is_children_expr_inner(&logical.left) || is_children_expr_inner(&logical.right)
        }
        // Handle {condition ? children : null} or {condition ? null : children}
        Expression::ConditionalExpression(cond) => {
            is_children_expr_inner(&cond.consequent) || is_children_expr_inner(&cond.alternate)
        }
        // Handle {(children)}
        Expression::ParenthesizedExpression(paren) => is_children_expr_inner(&paren.expression),
        _ => false,
    }
}

/// Get the element name from a JSX element name node.
fn jsx_element_name(name: &JSXElementName) -> String {
    match name {
        JSXElementName::Identifier(id) => id.name.to_string(),
        JSXElementName::IdentifierReference(id) => id.name.to_string(),
        JSXElementName::NamespacedName(ns) => {
            format!("{}:{}", ns.namespace.name, ns.name.name)
        }
        JSXElementName::MemberExpression(member) => {
            format!(
                "{}.{}",
                jsx_member_object(&member.object),
                member.property.name
            )
        }
        JSXElementName::ThisExpression(_) => "this".to_string(),
    }
}

fn jsx_member_object(obj: &JSXMemberExpressionObject) -> String {
    match obj {
        JSXMemberExpressionObject::IdentifierReference(id) => id.name.to_string(),
        JSXMemberExpressionObject::MemberExpression(member) => {
            format!(
                "{}.{}",
                jsx_member_object(&member.object),
                member.property.name
            )
        }
        JSXMemberExpressionObject::ThisExpression(_) => "this".to_string(),
    }
}

// ── Detail variants (track CSS tokens alongside tag names) ──────────────

fn find_children_in_statement_detail<'a>(
    stmt: &'a Statement<'a>,
    source: &str,
    path: &mut Vec<(String, Option<String>)>,
    aliases: &HashMap<String, String>,
) -> bool {
    match stmt {
        Statement::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                for inner in &body.statements {
                    if find_children_in_statement_detail(inner, source, path, aliases) {
                        return true;
                    }
                }
            }
        }
        Statement::ReturnStatement(ret) => {
            if let Some(expr) = &ret.argument {
                return find_children_in_expression_detail(expr, source, path, aliases);
            }
        }
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                if let Some(init) = &declarator.init {
                    if find_children_in_expression_detail(init, source, path, aliases) {
                        return true;
                    }
                }
            }
        }
        Statement::ExpressionStatement(expr_stmt) => {
            return find_children_in_expression_detail(
                &expr_stmt.expression,
                source,
                path,
                aliases,
            );
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                return find_children_in_declaration_detail(decl, source, path, aliases);
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let Some(expr) = export.declaration.as_expression() {
                return find_children_in_expression_detail(expr, source, path, aliases);
            }
        }
        Statement::BlockStatement(block) => {
            for inner in &block.body {
                if find_children_in_statement_detail(inner, source, path, aliases) {
                    return true;
                }
            }
        }
        Statement::IfStatement(if_stmt) => {
            if find_children_in_statement_detail(&if_stmt.consequent, source, path, aliases) {
                return true;
            }
            if let Some(alt) = &if_stmt.alternate {
                if find_children_in_statement_detail(alt, source, path, aliases) {
                    return true;
                }
            }
        }
        Statement::ClassDeclaration(class) => {
            if find_children_in_class_body_detail(&class.body, source, path, aliases) {
                return true;
            }
        }
        _ => {}
    }
    false
}

fn find_children_in_declaration_detail<'a>(
    decl: &'a Declaration<'a>,
    source: &str,
    path: &mut Vec<(String, Option<String>)>,
    aliases: &HashMap<String, String>,
) -> bool {
    match decl {
        Declaration::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                for stmt in &body.statements {
                    if find_children_in_statement_detail(stmt, source, path, aliases) {
                        return true;
                    }
                }
            }
        }
        Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    if find_children_in_expression_detail(init, source, path, aliases) {
                        return true;
                    }
                }
            }
        }
        Declaration::ClassDeclaration(class) => {
            if find_children_in_class_body_detail(&class.body, source, path, aliases) {
                return true;
            }
        }
        _ => {}
    }
    false
}

fn find_children_in_class_body_detail<'a>(
    body: &'a ClassBody<'a>,
    source: &str,
    path: &mut Vec<(String, Option<String>)>,
    aliases: &HashMap<String, String>,
) -> bool {
    for element in &body.body {
        let is_render_named = |key: &PropertyKey| -> bool {
            matches!(key, PropertyKey::StaticIdentifier(id) if id.name == "render")
        };
        match element {
            ClassElement::MethodDefinition(method) if is_render_named(&method.key) => {
                if let Some(body) = &method.value.body {
                    for stmt in &body.statements {
                        if find_children_in_statement_detail(stmt, source, path, aliases) {
                            return true;
                        }
                    }
                }
            }
            ClassElement::PropertyDefinition(prop) if is_render_named(&prop.key) => {
                if let Some(init) = &prop.value {
                    if let Expression::ArrowFunctionExpression(arrow) = init {
                        for stmt in &arrow.body.statements {
                            if find_children_in_statement_detail(stmt, source, path, aliases) {
                                return true;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    false
}

fn find_children_in_expression_detail<'a>(
    expr: &'a Expression<'a>,
    source: &str,
    path: &mut Vec<(String, Option<String>)>,
    aliases: &HashMap<String, String>,
) -> bool {
    match expr {
        Expression::JSXElement(el) => {
            find_children_in_jsx_element_detail(el, source, path, aliases)
        }
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                if find_children_in_jsx_child_detail(child, source, path, aliases) {
                    return true;
                }
            }
            false
        }
        Expression::ParenthesizedExpression(paren) => {
            find_children_in_expression_detail(&paren.expression, source, path, aliases)
        }
        Expression::ConditionalExpression(cond) => {
            find_children_in_expression_detail(&cond.consequent, source, path, aliases)
                || find_children_in_expression_detail(&cond.alternate, source, path, aliases)
        }
        Expression::LogicalExpression(logical) => {
            find_children_in_expression_detail(&logical.right, source, path, aliases)
        }
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Some(expr) = arg.as_expression() {
                    if find_children_in_expression_detail(expr, source, path, aliases) {
                        return true;
                    }
                }
            }
            false
        }
        Expression::ArrowFunctionExpression(arrow) => {
            for stmt in &arrow.body.statements {
                if find_children_in_statement_detail(stmt, source, path, aliases) {
                    return true;
                }
            }
            false
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                for stmt in &body.statements {
                    if find_children_in_statement_detail(stmt, source, path, aliases) {
                        return true;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

fn find_children_in_jsx_element_detail<'a>(
    el: &'a JSXElement<'a>,
    source: &str,
    path: &mut Vec<(String, Option<String>)>,
    aliases: &HashMap<String, String>,
) -> bool {
    let raw_tag = jsx_element_name(&el.opening_element.name);
    let tag_name = if raw_tag.starts_with(|c: char| c.is_uppercase()) {
        aliases.get(&raw_tag).cloned().unwrap_or(raw_tag)
    } else {
        raw_tag
    };

    // Extract CSS token from className={styles.xxx}
    let css_token = extract_styles_classname(&el.opening_element.attributes);

    // Check JSX props/attributes for children passed as props
    for attr_item in &el.opening_element.attributes {
        if let JSXAttributeItem::Attribute(attr) = attr_item {
            if let Some(JSXAttributeValue::ExpressionContainer(container)) = &attr.value {
                if let Some(inner_expr) = container.expression.as_expression() {
                    path.push((tag_name.clone(), css_token.clone()));
                    if find_children_in_expression_detail(inner_expr, source, path, aliases) {
                        return true;
                    }
                    path.pop();
                }
            }
        }
    }

    // Check direct JSX children
    path.push((tag_name.clone(), css_token));

    for child in &el.children {
        match child {
            JSXChild::ExpressionContainer(container) => {
                if is_children_expression(&container.expression, source) {
                    return true;
                }
                if let Some(expr) = container.expression.as_expression() {
                    if find_children_in_expression_detail(expr, source, path, aliases) {
                        return true;
                    }
                }
            }
            JSXChild::Element(child_el) => {
                if find_children_in_jsx_element_detail(child_el, source, path, aliases) {
                    return true;
                }
            }
            JSXChild::Fragment(frag) => {
                for frag_child in &frag.children {
                    if find_children_in_jsx_child_detail(frag_child, source, path, aliases) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }

    path.pop();
    false
}

fn find_children_in_jsx_child_detail<'a>(
    child: &'a JSXChild<'a>,
    source: &str,
    path: &mut Vec<(String, Option<String>)>,
    aliases: &HashMap<String, String>,
) -> bool {
    match child {
        JSXChild::Element(el) => find_children_in_jsx_element_detail(el, source, path, aliases),
        JSXChild::ExpressionContainer(container) => {
            if is_children_expression(&container.expression, source) {
                return true;
            }
            if let Some(expr) = container.expression.as_expression() {
                return find_children_in_expression_detail(expr, source, path, aliases);
            }
            false
        }
        JSXChild::Fragment(frag) => {
            for c in &frag.children {
                if find_children_in_jsx_child_detail(c, source, path, aliases) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// Extract `styles.xxx` token from a JSX element's className attribute.
///
/// Handles patterns:
/// - `className={styles.toolbarContent}` → Some("toolbarContent")
/// - `className={css(styles.toolbarContent, ...)}` → Some("toolbarContent")
/// - `className="static-class"` → None
/// - No className → None
fn extract_styles_classname(attrs: &[JSXAttributeItem]) -> Option<String> {
    for attr_item in attrs {
        if let JSXAttributeItem::Attribute(attr) = attr_item {
            let is_classname = match &attr.name {
                JSXAttributeName::Identifier(id) => id.name == "className",
                _ => false,
            };
            if !is_classname {
                continue;
            }
            if let Some(JSXAttributeValue::ExpressionContainer(container)) = &attr.value {
                if let Some(expr) = container.expression.as_expression() {
                    return extract_styles_token_from_expr(expr);
                }
            }
        }
    }
    None
}

/// Recursively extract the first `styles.xxx` token from an expression.
fn extract_styles_token_from_expr(expr: &Expression) -> Option<String> {
    match expr {
        // Direct: styles.toolbarContent
        Expression::StaticMemberExpression(member) => {
            if let Expression::Identifier(obj) = &member.object {
                if obj.name == "styles" {
                    return Some(member.property.name.to_string());
                }
            }
            None
        }
        // Handle css(styles.foo, ...) or cx(styles.foo, ...)
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Some(expr) = arg.as_expression() {
                    if let Some(token) = extract_styles_token_from_expr(expr) {
                        return Some(token);
                    }
                }
            }
            None
        }
        // Handle (styles.foo)
        Expression::ParenthesizedExpression(p) => extract_styles_token_from_expr(&p.expression),
        // Handle condition && styles.foo or styles.foo && condition
        Expression::LogicalExpression(logical) => extract_styles_token_from_expr(&logical.left)
            .or_else(|| extract_styles_token_from_expr(&logical.right)),
        // Handle condition ? styles.foo : styles.bar — take the first
        Expression::ConditionalExpression(cond) => extract_styles_token_from_expr(&cond.consequent)
            .or_else(|| extract_styles_token_from_expr(&cond.alternate)),
        _ => None,
    }
}

/// Check if any function parameter destructures `children`.
fn check_children_in_statement<'a>(stmt: &'a Statement<'a>) -> bool {
    match stmt {
        Statement::FunctionDeclaration(f) => check_children_in_params(&f.params),
        Statement::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    if check_children_in_expr(init) {
                        return true;
                    }
                }
            }
            false
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                return check_children_in_decl(decl);
            }
            false
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let Some(expr) = export.declaration.as_expression() {
                return check_children_in_expr(expr);
            }
            false
        }
        _ => false,
    }
}

fn check_children_in_decl<'a>(decl: &'a Declaration<'a>) -> bool {
    match decl {
        Declaration::FunctionDeclaration(f) => check_children_in_params(&f.params),
        Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    if check_children_in_expr(init) {
                        return true;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

fn check_children_in_expr<'a>(expr: &'a Expression<'a>) -> bool {
    match expr {
        Expression::ArrowFunctionExpression(arrow) => check_children_in_params(&arrow.params),
        Expression::FunctionExpression(func) => check_children_in_params(&func.params),
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Some(expr) = arg.as_expression() {
                    if check_children_in_expr(expr) {
                        return true;
                    }
                }
            }
            false
        }
        Expression::ParenthesizedExpression(paren) => check_children_in_expr(&paren.expression),
        _ => false,
    }
}

fn check_children_in_params(params: &FormalParameters) -> bool {
    for param in &params.items {
        if has_children_binding(&param.pattern) {
            return true;
        }
    }
    false
}

fn has_children_binding(pattern: &BindingPattern) -> bool {
    match pattern {
        BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                match &prop.key {
                    PropertyKey::StaticIdentifier(id) if id.name == "children" => {
                        return true;
                    }
                    _ => {}
                }
            }
            false
        }
        BindingPattern::AssignmentPattern(assign) => has_children_binding(&assign.left),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trace_simple_children() {
        let source = r#"
            const MyComponent = ({ children }: Props) => (
                <div className="wrapper">
                    {children}
                </div>
            );
        "#;

        let path = trace_children_slot(source);
        assert_eq!(path, vec!["div"]);
    }

    #[test]
    fn test_trace_nested_children() {
        let source = r#"
            const Modal = ({ children }: Props) => {
                return (
                    <Backdrop>
                        <FocusTrap>
                            <ModalBox>
                                {children}
                            </ModalBox>
                        </FocusTrap>
                    </Backdrop>
                );
            };
        "#;

        let path = trace_children_slot(source);
        assert_eq!(path, vec!["Backdrop", "FocusTrap", "ModalBox"]);
    }

    #[test]
    fn test_trace_children_in_prop() {
        let source = r#"
            const Dropdown = ({ children }: Props) => {
                const menu = (
                    <Menu>
                        <MenuContent>
                            {children}
                        </MenuContent>
                    </Menu>
                );
                return (
                    <Popper popper={menu} />
                );
            };
        "#;

        let path = trace_children_slot(source);
        assert_eq!(path, vec!["Menu", "MenuContent"]);
    }

    #[test]
    fn test_has_children_prop_true() {
        let source = r#"
            export const Button = ({ children, variant }: ButtonProps) => (
                <button>{children}</button>
            );
        "#;
        assert!(has_children_prop(source));
    }

    #[test]
    fn test_has_children_prop_false() {
        let source = r#"
            export const Icon = ({ name, size }: IconProps) => (
                <svg><use href={name} /></svg>
            );
        "#;
        assert!(!has_children_prop(source));
    }

    #[test]
    fn test_trace_dynamic_component_simple() {
        // Simple case: component prop with string default used as JSX tag
        let source = r#"
            const MyCell = ({ children, component = 'td' }: Props) => {
                const Component = component;
                return <Component>{children}</Component>;
            };
        "#;

        let path = trace_children_slot(source);
        assert_eq!(path, vec!["td"]);
    }

    #[test]
    fn test_trace_dynamic_component_merged() {
        // PatternFly pattern: prop default + destructuring with rename
        let source = r#"
            const TdBase = ({ children, component = 'td' }: Props) => {
                const merged = mergeProps();
                const { component: MergedComponent = component } = merged;
                return <MergedComponent>{children}</MergedComponent>;
            };
        "#;

        let path = trace_children_slot(source);
        assert_eq!(path, vec!["td"]);
    }

    #[test]
    fn test_trace_dynamic_component_th() {
        let source = r#"
            const ThBase = ({
                children,
                component = 'th',
            }: ThProps) => {
                const { component: MergedComponent = component, ...rest } = merged;
                return (
                    <MergedComponent className="header">
                        {children}
                    </MergedComponent>
                );
            };
        "#;

        let path = trace_children_slot(source);
        assert_eq!(path, vec!["th"]);
    }

    #[test]
    fn test_trace_dynamic_component_no_default() {
        // When the component prop has no string default, keep the original name
        let source = r#"
            const Box = ({ children, component: Component }: Props) => (
                <Component>{children}</Component>
            );
        "#;

        let path = trace_children_slot(source);
        // Component is PascalCase but has no resolvable default
        assert_eq!(path, vec!["Component"]);
    }

    #[test]
    fn test_collect_aliases_simple() {
        let allocator = Allocator::default();
        let source = r#"
            const Foo = ({ component = 'div' }: Props) => {
                return <component>{children}</component>;
            };
        "#;
        let parsed = Parser::new(&allocator, source, SourceType::tsx()).parse();
        let aliases = collect_component_aliases(&parsed.program.body);
        assert_eq!(aliases.get("component"), Some(&"div".to_string()));
    }

    #[test]
    fn test_trace_forwardref_with_internal_component() {
        // Pattern: forwardRef wrapper that delegates to an internal component
        // The tracer must find {children} inside TrBase even though Tr
        // doesn't directly use it
        let source = r#"
            const TrBase = ({
                children,
                className,
            }: TrProps) => {
                return (
                    <>
                        <tr className={className}>
                            {children}
                        </tr>
                    </>
                );
            };

            export const Tr = forwardRef((props: TrProps, ref) => (
                <TrBase {...props} innerRef={ref} />
            ));
        "#;

        let path = trace_children_slot(source);
        // Should find children inside TrBase's <tr>
        eprintln!("Tr forwardRef path: {:?}", path);
        assert_eq!(path, vec!["tr"]);
    }

    #[test]
    fn test_collect_aliases_two_hop() {
        let allocator = Allocator::default();
        let source = r#"
            const Foo = ({ component = 'td' }: Props) => {
                const { component: MergedComponent = component } = merged;
                return null;
            };
        "#;
        let parsed = Parser::new(&allocator, source, SourceType::tsx()).parse();
        let aliases = collect_component_aliases(&parsed.program.body);
        assert_eq!(aliases.get("component"), Some(&"td".to_string()));
        assert_eq!(aliases.get("MergedComponent"), Some(&"td".to_string()));
    }

    // ── Detail trace tests ──────────────────────────────────────────────

    #[test]
    fn test_detail_simple_styles() {
        let source = r#"
            const MyComponent = ({ children }: Props) => (
                <div className={styles.wrapper}>
                    {children}
                </div>
            );
        "#;
        let detail = trace_children_slot_detail(source);
        assert_eq!(
            detail,
            vec![("div".to_string(), Some("wrapper".to_string()))]
        );
    }

    #[test]
    fn test_detail_nested_styles() {
        let source = r#"
            const ToolbarContent = ({ children }: Props) => (
                <div className={styles.toolbarContent}>
                    <div className={styles.toolbarContentSection}>
                        {children}
                    </div>
                </div>
            );
        "#;
        let detail = trace_children_slot_detail(source);
        assert_eq!(
            detail,
            vec![
                ("div".to_string(), Some("toolbarContent".to_string())),
                ("div".to_string(), Some("toolbarContentSection".to_string())),
            ]
        );
    }

    #[test]
    fn test_detail_css_function() {
        // PF uses css() helper: className={css(styles.toolbar, className)}
        let source = r#"
            const Toolbar = ({ children }: Props) => (
                <div className={css(styles.toolbar, className)}>
                    {children}
                </div>
            );
        "#;
        let detail = trace_children_slot_detail(source);
        assert_eq!(
            detail,
            vec![("div".to_string(), Some("toolbar".to_string()))]
        );
    }

    #[test]
    fn test_detail_no_classname() {
        let source = r#"
            const Plain = ({ children }: Props) => (
                <div>
                    <span>{children}</span>
                </div>
            );
        "#;
        let detail = trace_children_slot_detail(source);
        assert_eq!(
            detail,
            vec![("div".to_string(), None), ("span".to_string(), None),]
        );
    }

    #[test]
    fn test_detail_mixed_styles_and_plain() {
        let source = r#"
            const Content = ({ children }: Props) => (
                <div className={styles.drawerContent}>
                    <div>
                        {children}
                    </div>
                </div>
            );
        "#;
        let detail = trace_children_slot_detail(source);
        assert_eq!(
            detail,
            vec![
                ("div".to_string(), Some("drawerContent".to_string())),
                ("div".to_string(), None),
            ]
        );
    }
}
