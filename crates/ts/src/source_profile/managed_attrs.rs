//! Managed attribute detection via AST dataflow tracing.
//!
//! Detects the pattern where a React component:
//! 1. Destructures a prop out of the rest parameter (`const { ouiaId, ...rest } = props`)
//! 2. Passes it to a helper function (`const ouiaProps = getOUIAProps(name, ouiaId)`)
//! 3. Spreads the result onto a JSX element AFTER rest (`<button {...rest} {...ouiaProps}>`)
//!
//! This means any consumer-provided HTML attribute that the helper generates
//! (e.g., `data-ouia-component-id`) will be silently overridden.
//!
//! The detection is generic — it works for any prop/helper/attribute pattern,
//! not just OUIA.

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;
use semver_analyzer_core::types::sd::ManagedAttributeBinding;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Result of analyzing destructuring patterns in a function's parameters
/// or body (for class components using `this.props`).
#[derive(Debug, Default)]
struct PropsDestructuring {
    /// Props explicitly destructured by name (e.g., `ouiaId`, `ouiaSafe`).
    named_props: HashSet<String>,
    /// The variable name for the rest element, if any (e.g., `otherProps`).
    rest_name: Option<String>,
}

/// A flow from a destructured prop through a function call to a result variable.
#[derive(Debug)]
struct PropFunctionFlow {
    /// The destructured prop names used as arguments to the function.
    prop_args: Vec<String>,
    /// The name of the helper function called.
    function_name: String,
    /// The variable name where the result is stored.
    result_variable: String,
}

/// A spread attribute on a JSX element with its position.
#[derive(Debug)]
struct JsxSpreadEntry {
    /// Order in the attribute list (0-based).
    position: usize,
    /// The variable/expression name being spread.
    variable_name: String,
}

/// Spread information for a single JSX element.
#[derive(Debug)]
struct JsxElementSpreads {
    tag_name: String,
    spreads: Vec<JsxSpreadEntry>,
}

/// Variables that transitively contain the rest props (e.g., `componentProps`
/// when `const componentProps = { children: x, ...otherProps }`).
#[derive(Debug, Default)]
struct RestPropagation {
    /// Set of variable names that contain the rest props (including the
    /// rest variable itself and any intermediate objects that spread it).
    rest_carriers: HashSet<String>,
}

/// Extract managed attribute bindings from a component's source file.
///
/// This is the main entry point. It performs full dataflow tracing:
/// 1. Finds props destructuring with rest parameter
/// 2. Traces destructured props through function calls
/// 3. Finds JSX elements with spread attributes
/// 4. Detects the override pattern (managed spread after rest spread)
/// 5. Correlates with `data_attributes` to identify overridden HTML attrs
pub fn extract_managed_attributes(
    source: &str,
    _component_name: &str,
    known_props: &BTreeSet<String>,
    data_attributes: &BTreeMap<(String, String), String>,
) -> Vec<ManagedAttributeBinding> {
    let allocator = Allocator::default();
    let source_type = SourceType::tsx();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    let mut destructurings = Vec::new();
    let mut function_flows = Vec::new();
    let mut jsx_spreads = Vec::new();
    let mut rest_propagation = RestPropagation::default();

    // Walk all statements to collect data
    for stmt in &parsed.program.body {
        collect_from_stmt(
            stmt,
            source,
            known_props,
            &mut destructurings,
            &mut function_flows,
            &mut jsx_spreads,
            &mut rest_propagation,
        );
    }

    // If no destructuring with rest was found, no override pattern is possible
    let primary = match destructurings.into_iter().find(|d| d.rest_name.is_some()) {
        Some(d) => d,
        None => return Vec::new(),
    };

    let rest_name = primary.rest_name.as_deref().unwrap();

    // Add the rest variable itself to the carriers
    rest_propagation.rest_carriers.insert(rest_name.to_string());

    // Build the override analysis
    build_bindings(
        &primary,
        &function_flows,
        &jsx_spreads,
        &rest_propagation,
        data_attributes,
    )
}

/// Build `ManagedAttributeBinding` entries from the collected dataflow facts.
fn build_bindings(
    destructuring: &PropsDestructuring,
    flows: &[PropFunctionFlow],
    jsx_spreads: &[JsxElementSpreads],
    rest_prop: &RestPropagation,
    data_attributes: &BTreeMap<(String, String), String>,
) -> Vec<ManagedAttributeBinding> {
    let mut bindings = Vec::new();

    // For each function flow, check if its result is spread on a JSX element
    // AFTER a rest-carrier spread
    for flow in flows {
        // Only consider flows that use destructured props
        if flow
            .prop_args
            .iter()
            .all(|p| !destructuring.named_props.contains(p))
        {
            continue;
        }

        for element in jsx_spreads {
            // Find spreads of the flow's result variable on this element
            let result_spread_pos = element.spreads.iter().find_map(|s| {
                if s.variable_name == flow.result_variable {
                    Some(s.position)
                } else {
                    None
                }
            });

            let result_pos = match result_spread_pos {
                Some(pos) => pos,
                None => continue,
            };

            // Find any rest-carrier spread on the same element that comes BEFORE
            let has_rest_before = element.spreads.iter().any(|s| {
                s.position < result_pos && rest_prop.rest_carriers.contains(&s.variable_name)
            });

            if !has_rest_before {
                continue;
            }

            // Override pattern detected! Now correlate with data_attributes
            // to find which HTML attributes are likely generated by the helper.
            let overridden: Vec<String> = data_attributes
                .keys()
                .filter(|(elem, _attr)| elem == &element.tag_name)
                .map(|(_elem, attr)| attr.clone())
                .collect();

            for prop_name in &flow.prop_args {
                if destructuring.named_props.contains(prop_name) {
                    bindings.push(ManagedAttributeBinding {
                        prop_name: prop_name.clone(),
                        generator_function: flow.function_name.clone(),
                        target_element: element.tag_name.clone(),
                        overridden_attributes: overridden.clone(),
                    });
                }
            }
        }
    }

    // Deduplicate by (prop_name, generator_function, target_element)
    let mut seen = HashSet::new();
    bindings.retain(|b| {
        seen.insert((
            b.prop_name.clone(),
            b.generator_function.clone(),
            b.target_element.clone(),
        ))
    });

    bindings
}

// ── AST walking ─────────────────────────────────────────────────────────

fn collect_from_stmt<'a>(
    stmt: &'a Statement<'a>,
    source: &str,
    known_props: &BTreeSet<String>,
    destructurings: &mut Vec<PropsDestructuring>,
    flows: &mut Vec<PropFunctionFlow>,
    jsx_spreads: &mut Vec<JsxElementSpreads>,
    rest_prop: &mut RestPropagation,
) {
    match stmt {
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                collect_from_decl(
                    decl,
                    source,
                    known_props,
                    destructurings,
                    flows,
                    jsx_spreads,
                    rest_prop,
                );
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let Some(expr) = export.declaration.as_expression() {
                collect_from_expr(
                    expr,
                    source,
                    known_props,
                    destructurings,
                    flows,
                    jsx_spreads,
                    rest_prop,
                );
            }
        }
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                // Check for destructuring pattern: const { a, b, ...rest } = this.props
                if let Some(init) = &declarator.init {
                    check_destructuring_declarator(
                        declarator,
                        init,
                        source,
                        known_props,
                        destructurings,
                        rest_prop,
                    );
                    // Check for function call: const ouiaProps = getOUIAProps(...)
                    check_function_flow(declarator, init, source, known_props, flows);
                    // Check for rest propagation: const componentProps = { ...rest, x }
                    check_rest_propagation(declarator, init, rest_prop);
                    // Walk expression for JSX
                    collect_from_expr(
                        init,
                        source,
                        known_props,
                        destructurings,
                        flows,
                        jsx_spreads,
                        rest_prop,
                    );
                }
            }
        }
        Statement::ReturnStatement(ret) => {
            if let Some(expr) = &ret.argument {
                collect_from_expr(
                    expr,
                    source,
                    known_props,
                    destructurings,
                    flows,
                    jsx_spreads,
                    rest_prop,
                );
            }
        }
        Statement::ExpressionStatement(expr_stmt) => {
            collect_from_expr(
                &expr_stmt.expression,
                source,
                known_props,
                destructurings,
                flows,
                jsx_spreads,
                rest_prop,
            );
        }
        Statement::IfStatement(if_stmt) => {
            collect_from_stmt(
                &if_stmt.consequent,
                source,
                known_props,
                destructurings,
                flows,
                jsx_spreads,
                rest_prop,
            );
            if let Some(alt) = &if_stmt.alternate {
                collect_from_stmt(
                    alt,
                    source,
                    known_props,
                    destructurings,
                    flows,
                    jsx_spreads,
                    rest_prop,
                );
            }
        }
        Statement::BlockStatement(block) => {
            for s in &block.body {
                collect_from_stmt(
                    s,
                    source,
                    known_props,
                    destructurings,
                    flows,
                    jsx_spreads,
                    rest_prop,
                );
            }
        }
        Statement::FunctionDeclaration(f) => {
            // Check function parameters for destructuring
            check_function_params(&f.params, source, known_props, destructurings);
            if let Some(body) = &f.body {
                for s in &body.statements {
                    collect_from_stmt(
                        s,
                        source,
                        known_props,
                        destructurings,
                        flows,
                        jsx_spreads,
                        rest_prop,
                    );
                }
            }
        }
        Statement::ClassDeclaration(cls) => {
            for item in &cls.body.body {
                match item {
                    ClassElement::MethodDefinition(method) => {
                        if let Some(body) = &method.value.body {
                            for s in &body.statements {
                                collect_from_stmt(
                                    s,
                                    source,
                                    known_props,
                                    destructurings,
                                    flows,
                                    jsx_spreads,
                                    rest_prop,
                                );
                            }
                        }
                    }
                    ClassElement::PropertyDefinition(prop) => {
                        if let Some(init) = &prop.value {
                            collect_from_expr(
                                init,
                                source,
                                known_props,
                                destructurings,
                                flows,
                                jsx_spreads,
                                rest_prop,
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn collect_from_decl<'a>(
    decl: &'a Declaration<'a>,
    source: &str,
    known_props: &BTreeSet<String>,
    destructurings: &mut Vec<PropsDestructuring>,
    flows: &mut Vec<PropFunctionFlow>,
    jsx_spreads: &mut Vec<JsxElementSpreads>,
    rest_prop: &mut RestPropagation,
) {
    match decl {
        Declaration::FunctionDeclaration(f) => {
            check_function_params(&f.params, source, known_props, destructurings);
            if let Some(body) = &f.body {
                for s in &body.statements {
                    collect_from_stmt(
                        s,
                        source,
                        known_props,
                        destructurings,
                        flows,
                        jsx_spreads,
                        rest_prop,
                    );
                }
            }
        }
        Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    check_destructuring_declarator(
                        declarator,
                        init,
                        source,
                        known_props,
                        destructurings,
                        rest_prop,
                    );
                    check_function_flow(declarator, init, source, known_props, flows);
                    check_rest_propagation(declarator, init, rest_prop);
                    collect_from_expr(
                        init,
                        source,
                        known_props,
                        destructurings,
                        flows,
                        jsx_spreads,
                        rest_prop,
                    );
                }
            }
        }
        Declaration::ClassDeclaration(cls) => {
            for item in &cls.body.body {
                match item {
                    ClassElement::MethodDefinition(method) => {
                        check_function_params(
                            &method.value.params,
                            source,
                            known_props,
                            destructurings,
                        );
                        if let Some(body) = &method.value.body {
                            for s in &body.statements {
                                collect_from_stmt(
                                    s,
                                    source,
                                    known_props,
                                    destructurings,
                                    flows,
                                    jsx_spreads,
                                    rest_prop,
                                );
                            }
                        }
                    }
                    ClassElement::PropertyDefinition(prop) => {
                        if let Some(init) = &prop.value {
                            collect_from_expr(
                                init,
                                source,
                                known_props,
                                destructurings,
                                flows,
                                jsx_spreads,
                                rest_prop,
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn collect_from_expr<'a>(
    expr: &'a Expression<'a>,
    source: &str,
    known_props: &BTreeSet<String>,
    destructurings: &mut Vec<PropsDestructuring>,
    flows: &mut Vec<PropFunctionFlow>,
    jsx_spreads: &mut Vec<JsxElementSpreads>,
    rest_prop: &mut RestPropagation,
) {
    match expr {
        Expression::JSXElement(el) => {
            collect_jsx_spreads(el, source, jsx_spreads);
            // Recurse into children
            for child in &el.children {
                collect_from_jsx_child(
                    child,
                    source,
                    known_props,
                    destructurings,
                    flows,
                    jsx_spreads,
                    rest_prop,
                );
            }
        }
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                collect_from_jsx_child(
                    child,
                    source,
                    known_props,
                    destructurings,
                    flows,
                    jsx_spreads,
                    rest_prop,
                );
            }
        }
        Expression::ParenthesizedExpression(paren) => {
            collect_from_expr(
                &paren.expression,
                source,
                known_props,
                destructurings,
                flows,
                jsx_spreads,
                rest_prop,
            );
        }
        Expression::ConditionalExpression(cond) => {
            collect_from_expr(
                &cond.consequent,
                source,
                known_props,
                destructurings,
                flows,
                jsx_spreads,
                rest_prop,
            );
            collect_from_expr(
                &cond.alternate,
                source,
                known_props,
                destructurings,
                flows,
                jsx_spreads,
                rest_prop,
            );
        }
        Expression::LogicalExpression(logical) => {
            collect_from_expr(
                &logical.right,
                source,
                known_props,
                destructurings,
                flows,
                jsx_spreads,
                rest_prop,
            );
        }
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    collect_from_expr(
                        e,
                        source,
                        known_props,
                        destructurings,
                        flows,
                        jsx_spreads,
                        rest_prop,
                    );
                }
            }
        }
        Expression::ArrowFunctionExpression(arrow) => {
            check_function_params(&arrow.params, source, known_props, destructurings);
            for s in &arrow.body.statements {
                collect_from_stmt(
                    s,
                    source,
                    known_props,
                    destructurings,
                    flows,
                    jsx_spreads,
                    rest_prop,
                );
            }
        }
        Expression::FunctionExpression(func) => {
            check_function_params(&func.params, source, known_props, destructurings);
            if let Some(body) = &func.body {
                for s in &body.statements {
                    collect_from_stmt(
                        s,
                        source,
                        known_props,
                        destructurings,
                        flows,
                        jsx_spreads,
                        rest_prop,
                    );
                }
            }
        }
        _ => {}
    }
}

fn collect_from_jsx_child<'a>(
    child: &'a JSXChild<'a>,
    source: &str,
    known_props: &BTreeSet<String>,
    destructurings: &mut Vec<PropsDestructuring>,
    flows: &mut Vec<PropFunctionFlow>,
    jsx_spreads: &mut Vec<JsxElementSpreads>,
    rest_prop: &mut RestPropagation,
) {
    match child {
        JSXChild::Element(el) => {
            collect_jsx_spreads(el, source, jsx_spreads);
            for c in &el.children {
                collect_from_jsx_child(
                    c,
                    source,
                    known_props,
                    destructurings,
                    flows,
                    jsx_spreads,
                    rest_prop,
                );
            }
        }
        JSXChild::Fragment(frag) => {
            for c in &frag.children {
                collect_from_jsx_child(
                    c,
                    source,
                    known_props,
                    destructurings,
                    flows,
                    jsx_spreads,
                    rest_prop,
                );
            }
        }
        JSXChild::ExpressionContainer(container) => {
            if let Some(expr) = container.expression.as_expression() {
                collect_from_expr(
                    expr,
                    source,
                    known_props,
                    destructurings,
                    flows,
                    jsx_spreads,
                    rest_prop,
                );
            }
        }
        _ => {}
    }
}

// ── Destructuring detection ─────────────────────────────────────────────

/// Check function parameters for object destructuring with rest.
fn check_function_params(
    params: &FormalParameters,
    _source: &str,
    known_props: &BTreeSet<String>,
    destructurings: &mut Vec<PropsDestructuring>,
) {
    for param in &params.items {
        check_binding_pattern(&param.pattern, known_props, destructurings);
    }
}

/// Check a binding pattern for object destructuring with rest.
fn check_binding_pattern(
    pattern: &BindingPattern,
    known_props: &BTreeSet<String>,
    destructurings: &mut Vec<PropsDestructuring>,
) {
    match pattern {
        BindingPattern::ObjectPattern(obj) => {
            extract_destructuring_from_object_pattern(obj, known_props, destructurings);
        }
        BindingPattern::AssignmentPattern(assign) => {
            // Handle ({ prop = default }: Props = {}) pattern
            check_binding_pattern(&assign.left, known_props, destructurings);
        }
        _ => {}
    }
}

/// Extract destructuring info from an ObjectPattern.
fn extract_destructuring_from_object_pattern(
    obj: &ObjectPattern,
    known_props: &BTreeSet<String>,
    destructurings: &mut Vec<PropsDestructuring>,
) {
    let mut destr = PropsDestructuring::default();

    for prop in &obj.properties {
        if let PropertyKey::StaticIdentifier(id) = &prop.key {
            let name = id.name.to_string();
            if known_props.contains(&name) || known_props.is_empty() {
                destr.named_props.insert(name);
            }
        }
    }

    if let Some(rest) = &obj.rest {
        if let BindingPattern::BindingIdentifier(id) = &rest.argument {
            destr.rest_name = Some(id.name.to_string());
        }
    }

    if !destr.named_props.is_empty() || destr.rest_name.is_some() {
        destructurings.push(destr);
    }
}

/// Check a variable declarator for `const { a, b, ...rest } = this.props` pattern.
fn check_destructuring_declarator<'a>(
    declarator: &'a VariableDeclarator<'a>,
    init: &'a Expression<'a>,
    _source: &str,
    known_props: &BTreeSet<String>,
    destructurings: &mut Vec<PropsDestructuring>,
    rest_prop: &mut RestPropagation,
) {
    // Check if the init is `this.props` (class component pattern)
    let is_this_props = matches!(init, Expression::StaticMemberExpression(member)
        if matches!(&member.object, Expression::ThisExpression(_))
        && member.property.name == "props"
    );

    if !is_this_props {
        return;
    }

    if let BindingPattern::ObjectPattern(obj) = &declarator.id {
        extract_destructuring_from_object_pattern(obj, known_props, destructurings);

        // Also register the rest name as a carrier
        if let Some(rest) = &obj.rest {
            if let BindingPattern::BindingIdentifier(id) = &rest.argument {
                rest_prop.rest_carriers.insert(id.name.to_string());
            }
        }
    }
}

// ── Function flow detection ─────────────────────────────────────────────

/// Check if a variable declaration is `const x = someFunction(prop1, prop2)`.
fn check_function_flow<'a>(
    declarator: &'a VariableDeclarator<'a>,
    init: &'a Expression<'a>,
    source: &str,
    known_props: &BTreeSet<String>,
    flows: &mut Vec<PropFunctionFlow>,
) {
    let var_name = match &declarator.id {
        BindingPattern::BindingIdentifier(id) => id.name.to_string(),
        _ => return,
    };

    let call = match init {
        Expression::CallExpression(call) => call,
        _ => return,
    };

    let func_name = match &call.callee {
        Expression::Identifier(id) => id.name.to_string(),
        Expression::StaticMemberExpression(member) => {
            format!(
                "{}.{}",
                expr_name(&member.object, source),
                member.property.name
            )
        }
        _ => return,
    };

    // Collect all known prop names used as arguments (including through ??, ||)
    let mut prop_args = Vec::new();
    for arg in &call.arguments {
        if let Some(expr) = arg.as_expression() {
            collect_prop_refs_from_expr(expr, source, known_props, &mut prop_args);
        }
    }

    if !prop_args.is_empty() {
        flows.push(PropFunctionFlow {
            prop_args,
            function_name: func_name,
            result_variable: var_name,
        });
    }
}

/// Recursively collect known prop name references from an expression.
/// Handles identifiers, `??`, `||`, `&&`, ternary, etc.
fn collect_prop_refs_from_expr(
    expr: &Expression,
    _source: &str,
    known_props: &BTreeSet<String>,
    out: &mut Vec<String>,
) {
    match expr {
        Expression::Identifier(id) => {
            let name = id.name.to_string();
            if known_props.contains(&name) && !out.contains(&name) {
                out.push(name);
            }
        }
        Expression::LogicalExpression(logical) => {
            collect_prop_refs_from_expr(&logical.left, _source, known_props, out);
            collect_prop_refs_from_expr(&logical.right, _source, known_props, out);
        }
        Expression::ConditionalExpression(cond) => {
            collect_prop_refs_from_expr(&cond.test, _source, known_props, out);
            collect_prop_refs_from_expr(&cond.consequent, _source, known_props, out);
            collect_prop_refs_from_expr(&cond.alternate, _source, known_props, out);
        }
        Expression::ParenthesizedExpression(paren) => {
            collect_prop_refs_from_expr(&paren.expression, _source, known_props, out);
        }
        _ => {}
    }
}

// ── Rest propagation detection ──────────────────────────────────────────

/// Check if a variable declaration spreads the rest variable into a new object:
/// `const componentProps = { children: x, ...otherProps }`
fn check_rest_propagation<'a>(
    declarator: &'a VariableDeclarator<'a>,
    init: &'a Expression<'a>,
    rest_prop: &mut RestPropagation,
) {
    let var_name = match &declarator.id {
        BindingPattern::BindingIdentifier(id) => id.name.to_string(),
        _ => return,
    };

    let obj = match init {
        Expression::ObjectExpression(obj) => obj,
        _ => return,
    };

    // Check if any property is a spread of a known rest carrier
    for prop in &obj.properties {
        if let ObjectPropertyKind::SpreadProperty(spread) = prop {
            if let Expression::Identifier(id) = &spread.argument {
                if rest_prop.rest_carriers.contains(id.name.as_str()) {
                    rest_prop.rest_carriers.insert(var_name);
                    return;
                }
            }
        }
    }
}

// ── JSX spread collection ───────────────────────────────────────────────

/// Collect spread attributes from a JSX element.
fn collect_jsx_spreads(el: &JSXElement, source: &str, jsx_spreads: &mut Vec<JsxElementSpreads>) {
    let tag_name = jsx_element_tag_name(&el.opening_element.name);
    let mut spreads = Vec::new();

    for (position, attr_item) in el.opening_element.attributes.iter().enumerate() {
        if let JSXAttributeItem::SpreadAttribute(spread) = attr_item {
            let variable_name = expr_name(&spread.argument, source);
            if !variable_name.is_empty() {
                spreads.push(JsxSpreadEntry {
                    position,
                    variable_name,
                });
            }
        }
    }

    if !spreads.is_empty() {
        jsx_spreads.push(JsxElementSpreads { tag_name, spreads });
    }

    // Recurse into children
    for child in &el.children {
        collect_jsx_spreads_from_child(child, source, jsx_spreads);
    }
}

fn collect_jsx_spreads_from_child(
    child: &JSXChild,
    source: &str,
    jsx_spreads: &mut Vec<JsxElementSpreads>,
) {
    match child {
        JSXChild::Element(el) => collect_jsx_spreads(el, source, jsx_spreads),
        JSXChild::Fragment(frag) => {
            for c in &frag.children {
                collect_jsx_spreads_from_child(c, source, jsx_spreads);
            }
        }
        _ => {}
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Extract a simple name from an expression (for variable identification).
fn expr_name(expr: &Expression, _source: &str) -> String {
    match expr {
        Expression::Identifier(id) => id.name.to_string(),
        Expression::StaticMemberExpression(member) => {
            let obj = expr_name(&member.object, _source);
            format!("{}.{}", obj, member.property.name)
        }
        _ => String::new(),
    }
}

/// Extract the tag name from a JSX element name.
fn jsx_element_tag_name(name: &JSXElementName) -> String {
    match name {
        JSXElementName::Identifier(id) => id.name.to_string(),
        JSXElementName::IdentifierReference(id) => id.name.to_string(),
        JSXElementName::NamespacedName(ns) => {
            format!("{}:{}", ns.namespace.name, ns.name.name)
        }
        JSXElementName::MemberExpression(member) => {
            format!(
                "{}.{}",
                jsx_member_obj_name(&member.object),
                member.property.name
            )
        }
        JSXElementName::ThisExpression(_) => "this".to_string(),
    }
}

fn jsx_member_obj_name(obj: &JSXMemberExpressionObject) -> String {
    match obj {
        JSXMemberExpressionObject::IdentifierReference(id) => id.name.to_string(),
        JSXMemberExpressionObject::MemberExpression(member) => {
            format!(
                "{}.{}",
                jsx_member_obj_name(&member.object),
                member.property.name
            )
        }
        JSXMemberExpressionObject::ThisExpression(_) => "this".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn data_attrs(entries: &[(&str, &str)]) -> BTreeMap<(String, String), String> {
        entries
            .iter()
            .map(|(elem, attr)| ((elem.to_string(), attr.to_string()), String::new()))
            .collect()
    }

    /// Test: MenuToggle class component pattern.
    /// ouiaId is destructured from this.props, passed to getOUIAProps(),
    /// result is spread after componentProps (which contains ...otherProps).
    #[test]
    fn test_class_component_ouia_pattern() {
        let source = r#"
            import { getOUIAProps } from '../../helpers';

            export interface MenuToggleProps {
                ouiaId?: number | string;
                ouiaSafe?: boolean;
                children?: React.ReactNode;
            }

            class MenuToggleBase extends React.Component {
                render() {
                    const {
                        children, ouiaId, ouiaSafe, ...otherProps
                    } = this.props;

                    const ouiaProps = getOUIAProps('MenuToggle', ouiaId, ouiaSafe);

                    const componentProps = {
                        children: children,
                        ...otherProps,
                    };

                    return (
                        <button {...componentProps} {...ouiaProps} />
                    );
                }
            }
        "#;

        let known = props(&["ouiaId", "ouiaSafe", "children"]);
        let data = data_attrs(&[
            ("button", "data-ouia-component-id"),
            ("button", "data-ouia-component-type"),
            ("button", "data-ouia-safe"),
        ]);

        let bindings = extract_managed_attributes(source, "MenuToggle", &known, &data);

        assert!(
            !bindings.is_empty(),
            "Expected managed attribute bindings, got none"
        );

        let ouia_binding = bindings.iter().find(|b| b.prop_name == "ouiaId");
        assert!(
            ouia_binding.is_some(),
            "Expected ouiaId binding, found: {:?}",
            bindings
        );

        let binding = ouia_binding.unwrap();
        assert_eq!(binding.generator_function, "getOUIAProps");
        assert_eq!(binding.target_element, "button");
        assert!(binding
            .overridden_attributes
            .contains(&"data-ouia-component-id".to_string()));
    }

    /// Test: Function component pattern with arrow function.
    #[test]
    fn test_function_component_pattern() {
        let source = r#"
            export const MyComponent = ({ testId, ...rest }: Props) => {
                const testProps = generateTestIds('MyComponent', testId);
                return <div {...rest} {...testProps} />;
            };
        "#;

        let known = props(&["testId"]);
        let data = data_attrs(&[("div", "data-testid")]);

        let bindings = extract_managed_attributes(source, "MyComponent", &known, &data);

        assert!(
            !bindings.is_empty(),
            "Expected managed attribute binding for testId"
        );
        assert_eq!(bindings[0].prop_name, "testId");
        assert_eq!(bindings[0].generator_function, "generateTestIds");
    }

    /// Test: No override when managed spread comes BEFORE rest.
    #[test]
    fn test_no_override_when_managed_before_rest() {
        let source = r#"
            export const MyComponent = ({ testId, ...rest }: Props) => {
                const testProps = generateTestIds('MyComponent', testId);
                return <div {...testProps} {...rest} />;
            };
        "#;

        let known = props(&["testId"]);
        let data = data_attrs(&[("div", "data-testid")]);

        let bindings = extract_managed_attributes(source, "MyComponent", &known, &data);

        assert!(
            bindings.is_empty(),
            "Expected no bindings when managed spread comes before rest, got: {:?}",
            bindings
        );
    }

    /// Test: No override when there's no rest parameter.
    #[test]
    fn test_no_rest_no_override() {
        let source = r#"
            export const MyComponent = ({ testId }: Props) => {
                const testProps = generateTestIds('MyComponent', testId);
                return <div {...testProps} />;
            };
        "#;

        let known = props(&["testId"]);
        let data = data_attrs(&[("div", "data-testid")]);

        let bindings = extract_managed_attributes(source, "MyComponent", &known, &data);

        assert!(
            bindings.is_empty(),
            "Expected no bindings without rest parameter"
        );
    }

    /// Test: Prop used with ?? fallback (ouiaId ?? this.state.ouiaStateId).
    #[test]
    fn test_prop_with_nullish_coalescing() {
        let source = r#"
            class Base extends React.Component {
                render() {
                    const { ouiaId, ouiaSafe, ...otherProps } = this.props;
                    const ouiaProps = getOUIAProps('X', ouiaId ?? this.state.id, ouiaSafe);
                    return <button {...otherProps} {...ouiaProps} />;
                }
            }
        "#;

        let known = props(&["ouiaId", "ouiaSafe"]);
        let data = data_attrs(&[("button", "data-ouia-component-id")]);

        let bindings = extract_managed_attributes(source, "X", &known, &data);
        assert!(
            !bindings.is_empty(),
            "Should detect prop through ?? expression"
        );
    }
}
