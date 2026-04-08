//! cloneElement prop injection detection.
//!
//! Detects the `Children.map(children, child => cloneElement(child, { props }))` pattern
//! in React component source code. This pattern is used to inject props into children,
//! creating a structural coupling between parent and child components.
//!
//! The detection extracts the property names from cloneElement's second argument,
//! which can then be matched against family members' declared props to infer
//! parent-child relationships.

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;
use semver_analyzer_core::types::sd::CloneElementInjection;

/// Detect `cloneElement` prop injections in a component's source code.
///
/// Finds patterns like:
/// - `Children.map(children, child => cloneElement(child, { rowid }))`
/// - `React.Children.map(children, (child) => cloneElement(child, { rowid: value }))`
/// - `Children.forEach(children, child => { ... cloneElement(child, { prop }) })`
///
/// Returns a list of injections, one per cloneElement call found.
pub fn detect_clone_element_injections(source: &str) -> Vec<CloneElementInjection> {
    let allocator = Allocator::default();
    let source_type = SourceType::tsx();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    let mut injections = Vec::new();
    for stmt in &parsed.program.body {
        find_clone_element_in_statement(stmt, &mut injections);
    }

    injections
}

// ── Statement walking ──────────────────────────────────────────────────

fn find_clone_element_in_statement<'a>(
    stmt: &'a Statement<'a>,
    injections: &mut Vec<CloneElementInjection>,
) {
    match stmt {
        Statement::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                for inner in &body.statements {
                    find_clone_element_in_statement(inner, injections);
                }
            }
        }
        Statement::ReturnStatement(ret) => {
            if let Some(expr) = &ret.argument {
                find_clone_element_in_expression(expr, injections);
            }
        }
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                if let Some(init) = &declarator.init {
                    find_clone_element_in_expression(init, injections);
                }
            }
        }
        Statement::ExpressionStatement(expr_stmt) => {
            find_clone_element_in_expression(&expr_stmt.expression, injections);
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                find_clone_element_in_declaration(decl, injections);
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let Some(expr) = export.declaration.as_expression() {
                find_clone_element_in_expression(expr, injections);
            }
        }
        Statement::BlockStatement(block) => {
            for inner in &block.body {
                find_clone_element_in_statement(inner, injections);
            }
        }
        Statement::IfStatement(if_stmt) => {
            find_clone_element_in_statement(&if_stmt.consequent, injections);
            if let Some(alt) = &if_stmt.alternate {
                find_clone_element_in_statement(alt, injections);
            }
        }
        Statement::ClassDeclaration(class) => {
            for element in &class.body.body {
                if let ClassElement::MethodDefinition(method) = element {
                    if let Some(body) = &method.value.body {
                        for stmt in &body.statements {
                            find_clone_element_in_statement(stmt, injections);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn find_clone_element_in_declaration<'a>(
    decl: &'a Declaration<'a>,
    injections: &mut Vec<CloneElementInjection>,
) {
    match decl {
        Declaration::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                for stmt in &body.statements {
                    find_clone_element_in_statement(stmt, injections);
                }
            }
        }
        Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    find_clone_element_in_expression(init, injections);
                }
            }
        }
        Declaration::ClassDeclaration(class) => {
            for element in &class.body.body {
                if let ClassElement::MethodDefinition(method) = element {
                    if let Some(body) = &method.value.body {
                        for stmt in &body.statements {
                            find_clone_element_in_statement(stmt, injections);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

// ── Expression walking ─────────────────────────────────────────────────

fn find_clone_element_in_expression<'a>(
    expr: &'a Expression<'a>,
    injections: &mut Vec<CloneElementInjection>,
) {
    match expr {
        Expression::CallExpression(call) => {
            // Check if this is a cloneElement call
            if let Some(injection) = try_extract_clone_element(call) {
                injections.push(injection);
            }
            // Also recurse into arguments (Children.map callback contains cloneElement)
            for arg in &call.arguments {
                if let Some(expr) = arg.as_expression() {
                    find_clone_element_in_expression(expr, injections);
                }
            }
        }
        Expression::ArrowFunctionExpression(arrow) => {
            for stmt in &arrow.body.statements {
                find_clone_element_in_statement(stmt, injections);
            }
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                for stmt in &body.statements {
                    find_clone_element_in_statement(stmt, injections);
                }
            }
        }
        Expression::ParenthesizedExpression(paren) => {
            find_clone_element_in_expression(&paren.expression, injections);
        }
        Expression::ConditionalExpression(cond) => {
            find_clone_element_in_expression(&cond.consequent, injections);
            find_clone_element_in_expression(&cond.alternate, injections);
        }
        Expression::LogicalExpression(logical) => {
            find_clone_element_in_expression(&logical.left, injections);
            find_clone_element_in_expression(&logical.right, injections);
        }
        Expression::JSXElement(el) => {
            // Recurse into JSX children and attributes
            for child in &el.children {
                find_clone_element_in_jsx_child(child, injections);
            }
            for attr in &el.opening_element.attributes {
                if let JSXAttributeItem::Attribute(attr) = attr {
                    if let Some(JSXAttributeValue::ExpressionContainer(container)) = &attr.value {
                        if let Some(expr) = container.expression.as_expression() {
                            find_clone_element_in_expression(expr, injections);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn find_clone_element_in_jsx_child<'a>(
    child: &'a JSXChild<'a>,
    injections: &mut Vec<CloneElementInjection>,
) {
    match child {
        JSXChild::ExpressionContainer(container) => {
            if let Some(expr) = container.expression.as_expression() {
                find_clone_element_in_expression(expr, injections);
            }
        }
        JSXChild::Element(el) => {
            for child in &el.children {
                find_clone_element_in_jsx_child(child, injections);
            }
            for attr in &el.opening_element.attributes {
                if let JSXAttributeItem::Attribute(attr) = attr {
                    if let Some(JSXAttributeValue::ExpressionContainer(container)) = &attr.value {
                        if let Some(expr) = container.expression.as_expression() {
                            find_clone_element_in_expression(expr, injections);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

// ── cloneElement extraction ────────────────────────────────────────────

/// Check if a call expression is `cloneElement(child, { ... })` and extract
/// the injected prop names from the second argument.
///
/// Public so it can be called from the main AST walk in `extract_source_info`
/// without requiring a separate full parse.
pub fn try_extract_clone_element_from_call(call: &CallExpression) -> Option<CloneElementInjection> {
    try_extract_clone_element(call)
}

fn try_extract_clone_element(call: &CallExpression) -> Option<CloneElementInjection> {
    // Match `cloneElement(...)` or `React.cloneElement(...)`
    let is_clone_element = match &call.callee {
        Expression::Identifier(id) => id.name == "cloneElement",
        Expression::StaticMemberExpression(member) => {
            member.property.name == "cloneElement"
                && matches!(&member.object, Expression::Identifier(id) if id.name == "React")
        }
        _ => false,
    };

    if !is_clone_element {
        return None;
    }

    // Need at least 2 arguments: cloneElement(element, props)
    if call.arguments.len() < 2 {
        return None;
    }

    // Extract prop names from the second argument (object expression)
    let props_arg = call.arguments[1].as_expression()?;
    let prop_names = extract_object_prop_names(props_arg);

    if prop_names.is_empty() {
        return None;
    }

    Some(CloneElementInjection {
        injected_props: prop_names,
    })
}

/// Extract property names from an object expression.
///
/// Handles:
/// - `{ rowid }` → ["rowid"] (shorthand)
/// - `{ rowid: ariaLabelledBy }` → ["rowid"]
/// - `{ isDisabled: true }` → ["isDisabled"]
/// - `{ ...spread }` → skipped (not a specific prop injection)
/// - `condition ? { isDisabled: true } : {}` → ["isDisabled"] (conditional)
fn extract_object_prop_names(expr: &Expression) -> Vec<String> {
    match expr {
        Expression::ObjectExpression(obj) => {
            let mut names = Vec::new();
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyKind::ObjectProperty(p) => {
                        if let Some(name) = property_key_name(&p.key) {
                            // Skip standard HTML/ARIA attributes — these are
                            // generic attribute injections, not structural coupling
                            if !name.starts_with("aria-")
                                && !name.starts_with("data-")
                                && name != "className"
                                && name != "style"
                                && name != "ref"
                                && name != "key"
                            {
                                names.push(name);
                            }
                        }
                    }
                    ObjectPropertyKind::SpreadProperty(_) => {
                        // Spread props don't tell us specific prop names
                    }
                }
            }
            names
        }
        // Handle conditional: areAllGroupsDisabled ? { isDisabled: true } : {}
        Expression::ConditionalExpression(cond) => {
            let mut names = extract_object_prop_names(&cond.consequent);
            let alt_names = extract_object_prop_names(&cond.alternate);
            for name in alt_names {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            names
        }
        Expression::ParenthesizedExpression(paren) => extract_object_prop_names(&paren.expression),
        _ => Vec::new(),
    }
}

/// Get the string name from a property key.
fn property_key_name(key: &PropertyKey) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some(id.name.to_string()),
        PropertyKey::StringLiteral(s) => Some(s.value.to_string()),
        _ => None,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_clone_element() {
        let source = r#"
            const DataListItem = ({ children, rowid }: Props) => {
                return (
                    <li>
                        {Children.map(children, (child) =>
                            isValidElement(child) &&
                            cloneElement(child, { rowid: ariaLabelledBy })
                        )}
                    </li>
                );
            };
        "#;
        let injections = detect_clone_element_injections(source);
        assert_eq!(injections.len(), 1);
        assert_eq!(injections[0].injected_props, vec!["rowid"]);
    }

    #[test]
    fn test_clone_element_shorthand() {
        // DataListItemRow pattern: cloneElement(child, { rowid })
        let source = r#"
            const DataListItemRow = ({ children, rowid }: Props) => {
                return (
                    <div>
                        {Children.map(children, (child) =>
                            isValidElement(child) &&
                            cloneElement(child, { rowid })
                        )}
                    </div>
                );
            };
        "#;
        let injections = detect_clone_element_injections(source);
        assert_eq!(injections.len(), 1);
        assert_eq!(injections[0].injected_props, vec!["rowid"]);
    }

    #[test]
    fn test_clone_element_conditional() {
        // ToggleGroup pattern: conditional injection
        let source = r#"
            const ToggleGroup = ({ children, areAllGroupsDisabled }: Props) => {
                return (
                    <div>
                        {Children.map(children, (child) =>
                            child.type === ToggleGroupItem
                                ? cloneElement(child, areAllGroupsDisabled ? { isDisabled: true } : {})
                                : child
                        )}
                    </div>
                );
            };
        "#;
        let injections = detect_clone_element_injections(source);
        assert_eq!(injections.len(), 1);
        assert_eq!(injections[0].injected_props, vec!["isDisabled"]);
    }

    #[test]
    fn test_clone_element_multiple_props() {
        // JumpLinks pattern: multiple props injected
        let source = r#"
            const JumpLinks = ({ children }: Props) => {
                return (
                    <nav>
                        {Children.map(children, (child, i) =>
                            cloneElement(child, {
                                onClick(ev) { handleClick(ev, i); },
                                isActive: activeIndex === i,
                            })
                        )}
                    </nav>
                );
            };
        "#;
        let injections = detect_clone_element_injections(source);
        assert_eq!(injections.len(), 1);
        assert!(injections[0]
            .injected_props
            .contains(&"onClick".to_string()));
        assert!(injections[0]
            .injected_props
            .contains(&"isActive".to_string()));
    }

    #[test]
    fn test_clone_element_breadcrumb() {
        // Breadcrumb: injects showDivider
        let source = r#"
            const Breadcrumb = ({ children }: Props) => {
                return (
                    <nav>
                        <ol>
                            {Children.map(children, (child, i) =>
                                cloneElement(child, { showDivider: i > 0 })
                            )}
                        </ol>
                    </nav>
                );
            };
        "#;
        let injections = detect_clone_element_injections(source);
        assert_eq!(injections.len(), 1);
        assert_eq!(injections[0].injected_props, vec!["showDivider"]);
    }

    #[test]
    fn test_no_clone_element() {
        let source = r#"
            const Simple = ({ children }: Props) => {
                return <div>{children}</div>;
            };
        "#;
        let injections = detect_clone_element_injections(source);
        assert!(injections.is_empty());
    }

    #[test]
    fn test_clone_element_skips_aria_attrs() {
        // Tooltip pattern: injects only aria attributes — should be filtered
        let source = r#"
            const Tooltip = ({ children }: Props) => {
                return cloneElement(children, { 'aria-describedby': id });
            };
        "#;
        let injections = detect_clone_element_injections(source);
        // aria-describedby is filtered out, so no injection recorded
        assert!(injections.is_empty());
    }

    #[test]
    fn test_clone_element_react_dot_clone() {
        // React.cloneElement form
        let source = r#"
            const Parent = ({ children }: Props) => {
                return (
                    <div>
                        {Children.map(children, child =>
                            React.cloneElement(child, { isActive: true })
                        )}
                    </div>
                );
            };
        "#;
        let injections = detect_clone_element_injections(source);
        assert_eq!(injections.len(), 1);
        assert_eq!(injections[0].injected_props, vec!["isActive"]);
    }

    #[test]
    fn test_clone_element_in_class_component() {
        let source = r#"
            class DataListItem extends Component {
                render() {
                    const { children } = this.props;
                    return (
                        <li>
                            {Children.map(children, (child) =>
                                cloneElement(child, { rowid: this.props.id })
                            )}
                        </li>
                    );
                }
            }
        "#;
        let injections = detect_clone_element_injections(source);
        assert_eq!(injections.len(), 1);
        assert_eq!(injections[0].injected_props, vec!["rowid"]);
    }
}
