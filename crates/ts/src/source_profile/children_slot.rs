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

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;

/// Trace the path from JSX root to `{children}` in a component's source.
///
/// Returns the chain of component/element names that wrap the children slot.
/// Returns an empty vec if `{children}` is not found in the JSX tree.
pub fn trace_children_slot(source: &str) -> Vec<String> {
    let allocator = Allocator::default();
    let source_type = SourceType::tsx();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    let mut path = Vec::new();
    for stmt in &parsed.program.body {
        if find_children_in_statement(stmt, source, &mut path) {
            return path;
        }
    }

    path
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

fn find_children_in_statement<'a>(
    stmt: &'a Statement<'a>,
    source: &str,
    path: &mut Vec<String>,
) -> bool {
    match stmt {
        Statement::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                for inner in &body.statements {
                    if find_children_in_statement(inner, source, path) {
                        return true;
                    }
                }
            }
        }
        Statement::ReturnStatement(ret) => {
            if let Some(expr) = &ret.argument {
                return find_children_in_expression(expr, source, path);
            }
        }
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                if let Some(init) = &declarator.init {
                    if find_children_in_expression(init, source, path) {
                        return true;
                    }
                }
            }
        }
        Statement::ExpressionStatement(expr_stmt) => {
            return find_children_in_expression(&expr_stmt.expression, source, path);
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                return find_children_in_declaration(decl, source, path);
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let Some(expr) = export.declaration.as_expression() {
                return find_children_in_expression(expr, source, path);
            }
        }
        Statement::BlockStatement(block) => {
            for inner in &block.body {
                if find_children_in_statement(inner, source, path) {
                    return true;
                }
            }
        }
        Statement::IfStatement(if_stmt) => {
            if find_children_in_statement(&if_stmt.consequent, source, path) {
                return true;
            }
            if let Some(alt) = &if_stmt.alternate {
                if find_children_in_statement(alt, source, path) {
                    return true;
                }
            }
        }
        _ => {}
    }
    false
}

fn find_children_in_declaration<'a>(
    decl: &'a Declaration<'a>,
    source: &str,
    path: &mut Vec<String>,
) -> bool {
    match decl {
        Declaration::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                for stmt in &body.statements {
                    if find_children_in_statement(stmt, source, path) {
                        return true;
                    }
                }
            }
        }
        Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    if find_children_in_expression(init, source, path) {
                        return true;
                    }
                }
            }
        }
        _ => {}
    }
    false
}

fn find_children_in_expression<'a>(
    expr: &'a Expression<'a>,
    source: &str,
    path: &mut Vec<String>,
) -> bool {
    match expr {
        Expression::JSXElement(el) => find_children_in_jsx_element(el, source, path),
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                if find_children_in_jsx_child(child, source, path) {
                    return true;
                }
            }
            false
        }
        Expression::ParenthesizedExpression(paren) => {
            find_children_in_expression(&paren.expression, source, path)
        }
        Expression::ConditionalExpression(cond) => {
            // Check both branches
            find_children_in_expression(&cond.consequent, source, path)
                || find_children_in_expression(&cond.alternate, source, path)
        }
        Expression::LogicalExpression(logical) => {
            find_children_in_expression(&logical.right, source, path)
        }
        Expression::CallExpression(call) => {
            // Handle forwardRef((...) => ...) etc.
            for arg in &call.arguments {
                if let Some(expr) = arg.as_expression() {
                    if find_children_in_expression(expr, source, path) {
                        return true;
                    }
                }
            }
            false
        }
        Expression::ArrowFunctionExpression(arrow) => {
            for stmt in &arrow.body.statements {
                if find_children_in_statement(stmt, source, path) {
                    return true;
                }
            }
            false
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                for stmt in &body.statements {
                    if find_children_in_statement(stmt, source, path) {
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

fn find_children_in_jsx_element<'a>(
    el: &'a JSXElement<'a>,
    source: &str,
    path: &mut Vec<String>,
) -> bool {
    let tag_name = jsx_element_name(&el.opening_element.name);

    // Check JSX props/attributes for children passed as props
    // e.g., <Popper popper={<Menu>{children}</Menu>} />
    for attr_item in &el.opening_element.attributes {
        if let JSXAttributeItem::Attribute(attr) = attr_item {
            if let Some(value) = &attr.value {
                if let JSXAttributeValue::ExpressionContainer(container) = value {
                    if let Some(inner_expr) = container.expression.as_expression() {
                        path.push(tag_name.clone());
                        if find_children_in_expression(inner_expr, source, path) {
                            return true;
                        }
                        path.pop();
                    }
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
                    if find_children_in_expression(expr, source, path) {
                        return true;
                    }
                }
            }
            JSXChild::Element(child_el) => {
                if find_children_in_jsx_element(child_el, source, path) {
                    return true;
                }
            }
            JSXChild::Fragment(frag) => {
                for frag_child in &frag.children {
                    if find_children_in_jsx_child(frag_child, source, path) {
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

fn find_children_in_jsx_child<'a>(
    child: &'a JSXChild<'a>,
    source: &str,
    path: &mut Vec<String>,
) -> bool {
    match child {
        JSXChild::Element(el) => find_children_in_jsx_element(el, source, path),
        JSXChild::ExpressionContainer(container) => {
            if is_children_expression(&container.expression, source) {
                return true;
            }
            if let Some(expr) = container.expression.as_expression() {
                return find_children_in_expression(expr, source, path);
            }
            false
        }
        JSXChild::Fragment(frag) => {
            for c in &frag.children {
                if find_children_in_jsx_child(c, source, path) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// Check if a JSX expression container holds `{children}` or `{props.children}`.
fn is_children_expression(expr: &JSXExpression, _source: &str) -> bool {
    match expr {
        JSXExpression::Identifier(id) if id.name == "children" => true,
        _ => {
            if let Some(inner) = expr.as_expression() {
                match inner {
                    Expression::Identifier(id) => id.name == "children",
                    Expression::StaticMemberExpression(member) => {
                        member.property.name == "children"
                            && matches!(&member.object, Expression::Identifier(id) if id.name == "props")
                    }
                    // Handle {renderedContent} or other variables that contain children
                    _ => false,
                }
            } else {
                false
            }
        }
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
}
