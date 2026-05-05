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

use crate::sd_types::{ManagedAttributeBinding, TrackedAttributes};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;
use std::collections::{BTreeSet, HashMap, HashSet};

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
    data_attributes: &TrackedAttributes<(String, String)>,
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
///
/// Detects ALL helper-function-to-JSX-spread flows, regardless of spread
/// order. The `component_overrides` field records whether the managed spread
/// comes after the rest spread (component wins) or before (consumer wins).
fn build_bindings(
    destructuring: &PropsDestructuring,
    flows: &[PropFunctionFlow],
    jsx_spreads: &[JsxElementSpreads],
    rest_prop: &RestPropagation,
    data_attributes: &TrackedAttributes<(String, String)>,
) -> Vec<ManagedAttributeBinding> {
    let mut bindings = Vec::new();

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

            // Determine spread order: does a rest-carrier spread come BEFORE
            // the managed spread? If yes, component_overrides = true (the
            // managed spread silently overrides consumer attributes).
            let has_rest_before = element.spreads.iter().any(|s| {
                s.position < result_pos && rest_prop.rest_carriers.contains(&s.variable_name)
            });

            // Correlate with data_attributes to find which HTML attributes
            // are likely generated by the helper.
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
                        component_overrides: has_rest_before,
                        arg_position: None,
                    });
                }
            }
        }
    }

    // Deduplicate by (prop_name, generator_function, target_element).
    //
    // When a component has multiple render paths (e.g., typeahead vs standard
    // mode) that produce different spread orders for the same target element,
    // prefer `component_overrides: true`. If ANY render path has the managed
    // spread after the rest spread, consumer HTML attributes can be silently
    // overridden — that's the case we need to warn about.
    //
    // Also merge overridden_attributes from all duplicates (union).
    let mut best: HashMap<(String, String, String), usize> = HashMap::new();
    for (i, b) in bindings.iter().enumerate() {
        let key = (
            b.prop_name.clone(),
            b.generator_function.clone(),
            b.target_element.clone(),
        );
        match best.entry(key) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(i);
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let existing_idx = *e.get();
                // Prefer component_overrides: true (more conservative)
                if b.component_overrides && !bindings[existing_idx].component_overrides {
                    e.insert(i);
                }
            }
        }
    }
    let keep_indices: HashSet<usize> = best.values().cloned().collect();
    let mut idx = 0usize;
    bindings.retain(|_| {
        let k = keep_indices.contains(&idx);
        idx += 1;
        k
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
            collect_jsx_spreads(el, source, known_props, jsx_spreads, flows);
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
            collect_jsx_spreads(el, source, known_props, jsx_spreads, flows);
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
///
/// Handles two patterns:
/// 1. Variable spreads: `{...ouiaProps}` — captured by `expr_name`
/// 2. Inline function calls: `{...getOUIAProps(name, ouiaId)}` — unwraps
///    the expression to find `CallExpression` nodes and creates synthetic
///    `PropFunctionFlow` entries so `build_bindings` treats them identically
///    to variable-assigned calls.
fn collect_jsx_spreads<'a>(
    el: &'a JSXElement<'a>,
    source: &str,
    known_props: &BTreeSet<String>,
    jsx_spreads: &mut Vec<JsxElementSpreads>,
    function_flows: &mut Vec<PropFunctionFlow>,
) {
    let tag_name = jsx_element_tag_name(&el.opening_element.name);
    let mut spreads = Vec::new();

    for (position, attr_item) in el.opening_element.attributes.iter().enumerate() {
        if let JSXAttributeItem::SpreadAttribute(spread) = attr_item {
            let variable_name = expr_name(&spread.argument, source);
            if !variable_name.is_empty() {
                // Pattern 1: Variable reference — `{...ouiaProps}`
                spreads.push(JsxSpreadEntry {
                    position,
                    variable_name,
                });
            } else {
                // Pattern 2+: Inline expression — unwrap to find CallExpression
                if let Some((func_name, prop_args)) =
                    extract_inline_call(&spread.argument, source, known_props)
                {
                    let synthetic_name = format!("__inline_{func_name}_{position}");
                    spreads.push(JsxSpreadEntry {
                        position,
                        variable_name: synthetic_name.clone(),
                    });
                    function_flows.push(PropFunctionFlow {
                        prop_args,
                        function_name: func_name,
                        result_variable: synthetic_name,
                    });
                }
            }
        }
    }

    if !spreads.is_empty() {
        jsx_spreads.push(JsxElementSpreads { tag_name, spreads });
    }

    // Recurse into children
    for child in &el.children {
        collect_jsx_spreads_from_child(child, source, known_props, jsx_spreads, function_flows);
    }
}

fn collect_jsx_spreads_from_child<'a>(
    child: &'a JSXChild<'a>,
    source: &str,
    known_props: &BTreeSet<String>,
    jsx_spreads: &mut Vec<JsxElementSpreads>,
    function_flows: &mut Vec<PropFunctionFlow>,
) {
    match child {
        JSXChild::Element(el) => {
            collect_jsx_spreads(el, source, known_props, jsx_spreads, function_flows);
        }
        JSXChild::Fragment(frag) => {
            for c in &frag.children {
                collect_jsx_spreads_from_child(c, source, known_props, jsx_spreads, function_flows);
            }
        }
        _ => {}
    }
}

/// Unwrap an expression through TypeScript wrappers, parentheses,
/// conditionals, and logical operators to find a `CallExpression`.
///
/// Returns `Some((function_name, prop_args))` if a call is found with
/// known prop references in its arguments.
fn extract_inline_call(
    expr: &Expression,
    source: &str,
    known_props: &BTreeSet<String>,
) -> Option<(String, Vec<String>)> {
    match expr {
        // Direct call: `getOUIAProps(name, ouiaId)`
        Expression::CallExpression(call) => {
            let func_name = match &call.callee {
                Expression::Identifier(id) => id.name.to_string(),
                Expression::StaticMemberExpression(member) => {
                    format!(
                        "{}.{}",
                        expr_name(&member.object, source),
                        member.property.name
                    )
                }
                _ => return None,
            };
            let mut prop_args = Vec::new();
            for arg in &call.arguments {
                if let Some(arg_expr) = arg.as_expression() {
                    collect_prop_refs_from_expr(arg_expr, source, known_props, &mut prop_args);
                }
            }
            if prop_args.is_empty() {
                return None;
            }
            Some((func_name, prop_args))
        }
        // TS wrappers: `(expr as Type)`, `expr!`, `<Type>expr`, `expr satisfies T`
        Expression::TSAsExpression(ts) => extract_inline_call(&ts.expression, source, known_props),
        Expression::TSSatisfiesExpression(ts) => {
            extract_inline_call(&ts.expression, source, known_props)
        }
        Expression::TSNonNullExpression(ts) => {
            extract_inline_call(&ts.expression, source, known_props)
        }
        Expression::TSTypeAssertion(ts) => extract_inline_call(&ts.expression, source, known_props),
        // Parenthesized: `(getOUIAProps(...))`
        Expression::ParenthesizedExpression(paren) => {
            extract_inline_call(&paren.expression, source, known_props)
        }
        // Conditional: `(condition ? getOUIAProps(...) : {})`
        // Check both branches; prefer the one with a call.
        Expression::ConditionalExpression(cond) => {
            extract_inline_call(&cond.consequent, source, known_props)
                .or_else(|| extract_inline_call(&cond.alternate, source, known_props))
        }
        // Logical: `(enabled && getOUIAProps(...))`
        Expression::LogicalExpression(logical) => {
            extract_inline_call(&logical.right, source, known_props)
                .or_else(|| extract_inline_call(&logical.left, source, known_props))
        }
        _ => None,
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Extract a simple name from an expression (for variable identification).
fn expr_name(expr: &Expression, _source: &str) -> String {
    match expr {
        Expression::Identifier(id) => id.name.to_string(),
        Expression::ThisExpression(_) => "this".to_string(),
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

/// Extract the property keys from a named function's return object.
///
/// Given the source of a helper file and a function name, parses the file
/// to find the function, then extracts the object literal keys from its
/// return statement (or arrow expression body).
///
/// For `getOUIAProps` returning:
///   `return { 'data-ouia-component-type': ..., 'data-ouia-safe': ... }`
/// Returns: `["data-ouia-component-type", "data-ouia-safe", "data-ouia-component-id"]`
///
/// Handles:
/// - `function foo() { return { ... }; }`
/// - `const foo = () => ({ ... })`
/// - `export const foo = (...) => ({ ... })`
/// - `export function foo() { ... }`
/// - Multiple return statements (unions all keys)
pub fn extract_return_object_keys(source: &str, function_name: &str) -> Vec<String> {
    let allocator = Allocator::default();
    let source_type = SourceType::tsx();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    let mut keys = Vec::new();

    for stmt in &parsed.program.body {
        extract_keys_from_stmt(stmt, function_name, &mut keys);
    }

    // Deduplicate and sort for deterministic output
    let mut seen = HashSet::new();
    keys.retain(|k| seen.insert(k.clone()));
    keys.sort();
    keys
}

/// Walk statements to find a named function/const and extract its return object keys.
fn extract_keys_from_stmt<'a>(
    stmt: &'a Statement<'a>,
    function_name: &str,
    keys: &mut Vec<String>,
) {
    match stmt {
        Statement::FunctionDeclaration(f) => {
            if let Some(id) = &f.id {
                if id.name == function_name {
                    if let Some(body) = &f.body {
                        extract_keys_from_body_stmts(&body.statements, keys);
                    }
                }
            }
        }
        Statement::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                let name = match &declarator.id {
                    BindingPattern::BindingIdentifier(id) => id.name.as_str(),
                    _ => continue,
                };
                if name != function_name {
                    continue;
                }
                if let Some(init) = &declarator.init {
                    extract_keys_from_arrow_or_function(init, keys);
                }
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                match decl {
                    Declaration::FunctionDeclaration(f) => {
                        if let Some(id) = &f.id {
                            if id.name == function_name {
                                if let Some(body) = &f.body {
                                    extract_keys_from_body_stmts(&body.statements, keys);
                                }
                            }
                        }
                    }
                    Declaration::VariableDeclaration(var_decl) => {
                        for declarator in &var_decl.declarations {
                            let name = match &declarator.id {
                                BindingPattern::BindingIdentifier(id) => id.name.as_str(),
                                _ => continue,
                            };
                            if name != function_name {
                                continue;
                            }
                            if let Some(init) = &declarator.init {
                                extract_keys_from_arrow_or_function(init, keys);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            // Only match if function_name is "default"
            if function_name == "default" {
                if let Some(expr) = export.declaration.as_expression() {
                    extract_keys_from_arrow_or_function(expr, keys);
                }
            }
        }
        _ => {}
    }
}

/// Extract return object keys from an arrow function or function expression.
fn extract_keys_from_arrow_or_function<'a>(
    expr: &'a Expression<'a>,
    keys: &mut Vec<String>,
) {
    match expr {
        Expression::ArrowFunctionExpression(arrow) => {
            // Arrow with expression body: `=> ({ key1: ..., key2: ... })`
            if arrow.expression {
                for stmt in &arrow.body.statements {
                    if let Statement::ExpressionStatement(expr_stmt) = stmt {
                        extract_keys_from_object_expr(&expr_stmt.expression, keys);
                    }
                }
            } else {
                // Arrow with block body: `=> { return { ... }; }`
                extract_keys_from_body_stmts(&arrow.body.statements, keys);
            }
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                extract_keys_from_body_stmts(&body.statements, keys);
            }
        }
        // Unwrap call expressions like React.memo(() => ...)
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    extract_keys_from_arrow_or_function(e, keys);
                }
            }
        }
        _ => {}
    }
}

/// Extract return object keys from a list of function body statements.
fn extract_keys_from_body_stmts<'a>(
    stmts: &'a [Statement<'a>],
    keys: &mut Vec<String>,
) {
    for stmt in stmts {
        if let Statement::ReturnStatement(ret) = stmt {
            if let Some(arg) = &ret.argument {
                extract_keys_from_object_expr(arg, keys);
            }
        }
        // Also check if/else blocks for conditional returns
        if let Statement::IfStatement(if_stmt) = stmt {
            if let Statement::BlockStatement(block) = &if_stmt.consequent {
                extract_keys_from_body_stmts(&block.body, keys);
            }
            if let Some(Statement::BlockStatement(block)) = if_stmt.alternate.as_ref() {
                extract_keys_from_body_stmts(&block.body, keys);
            }
        }
    }
}

/// Extract property keys from an object expression (potentially wrapped).
fn extract_keys_from_object_expr<'a>(
    expr: &'a Expression<'a>,
    keys: &mut Vec<String>,
) {
    match expr {
        Expression::ObjectExpression(obj) => {
            for prop in &obj.properties {
                if let ObjectPropertyKind::ObjectProperty(p) = prop {
                    match &p.key {
                        PropertyKey::StringLiteral(s) => {
                            keys.push(s.value.to_string());
                        }
                        PropertyKey::StaticIdentifier(id) => {
                            keys.push(id.name.to_string());
                        }
                        _ => {} // Computed keys — skip
                    }
                }
            }
        }
        // Unwrap parentheses: `({ ... })`
        Expression::ParenthesizedExpression(paren) => {
            extract_keys_from_object_expr(&paren.expression, keys);
        }
        // Unwrap TS casts: `{ ... } as ReturnType`
        Expression::TSAsExpression(ts) => {
            extract_keys_from_object_expr(&ts.expression, keys);
        }
        Expression::TSSatisfiesExpression(ts) => {
            extract_keys_from_object_expr(&ts.expression, keys);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn data_attrs(entries: &[(&str, &str)]) -> TrackedAttributes<(String, String)> {
        let mut tracked = TrackedAttributes::default();
        for (elem, attr) in entries {
            tracked.insert((elem.to_string(), attr.to_string()), String::new(), false);
        }
        tracked
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

    /// Test: Managed spread before rest — binding detected with
    /// `component_overrides: false` (consumer wins).
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
            !bindings.is_empty(),
            "Expected binding with consumer-wins order, got none"
        );
        assert!(
            !bindings[0].component_overrides,
            "Expected component_overrides=false when managed spread comes before rest"
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

    /// Test: Inline getOUIAProps call in JSX spread — class component.
    /// `{...getOUIAProps('Checkbox', ouiaId, ouiaSafe)}` directly in JSX
    /// without assigning to a variable first.
    #[test]
    fn test_inline_call_in_jsx_spread() {
        let source = r#"
            class Checkbox extends React.Component {
                render() {
                    const { ouiaId, ouiaSafe, ...props } = this.props;
                    return (
                        <input
                            {...props}
                            {...getOUIAProps('Checkbox', ouiaId, ouiaSafe)}
                        />
                    );
                }
            }
        "#;

        let known = props(&["ouiaId", "ouiaSafe"]);
        let data = data_attrs(&[("input", "data-ouia-component-type")]);

        let bindings = extract_managed_attributes(source, "Checkbox", &known, &data);

        assert!(
            !bindings.is_empty(),
            "Expected managed attribute bindings for inline call, got none"
        );

        let binding = bindings.iter().find(|b| b.prop_name == "ouiaId");
        assert!(
            binding.is_some(),
            "Expected ouiaId binding from inline call, found: {:?}",
            bindings
        );
        assert_eq!(binding.unwrap().generator_function, "getOUIAProps");
        assert_eq!(binding.unwrap().target_element, "input");
    }

    /// Test: Inline call with REVERSE spread order — binding detected
    /// with `component_overrides: false` (consumer wins).
    #[test]
    fn test_inline_call_reverse_order_consumer_wins() {
        let source = r#"
            class Nav extends React.Component {
                render() {
                    const { ouiaId, ouiaSafe, ...props } = this.props;
                    return (
                        <nav
                            {...getOUIAProps('Nav', ouiaId, ouiaSafe)}
                            {...props}
                        />
                    );
                }
            }
        "#;

        let known = props(&["ouiaId", "ouiaSafe"]);
        let data = data_attrs(&[("nav", "data-ouia-component-type")]);

        let bindings = extract_managed_attributes(source, "Nav", &known, &data);

        assert!(
            !bindings.is_empty(),
            "Expected binding with consumer-wins order, got none"
        );
        assert!(
            !bindings[0].component_overrides,
            "Expected component_overrides=false when inline call comes before rest"
        );
        assert_eq!(bindings[0].generator_function, "getOUIAProps");
    }

    /// Test: Inline function call with TS `as` cast wrapper.
    /// `{...(getOUIAProps(...) as any)}` — should unwrap TSAsExpression.
    #[test]
    fn test_inline_call_with_ts_as_expression() {
        let source = r#"
            class Switch extends React.Component {
                render() {
                    const { ouiaId, ouiaSafe, ...props } = this.props;
                    return (
                        <input
                            {...props}
                            {...(getOUIAProps('Switch', ouiaId, ouiaSafe) as any)}
                        />
                    );
                }
            }
        "#;

        let known = props(&["ouiaId", "ouiaSafe"]);
        let data = data_attrs(&[("input", "data-ouia-component-type")]);

        let bindings = extract_managed_attributes(source, "Switch", &known, &data);

        assert!(
            !bindings.is_empty(),
            "Expected bindings through TS `as` expression"
        );
        assert_eq!(bindings[0].generator_function, "getOUIAProps");
    }

    /// Test: Inline call with conditional expression wrapper.
    /// `{...(enabled ? getOUIAProps(...) : {})}` — should unwrap conditional.
    #[test]
    fn test_inline_call_with_conditional() {
        let source = r#"
            export const MyComponent = ({ ouiaId, enabled, ...rest }: Props) => {
                return (
                    <div
                        {...rest}
                        {...(enabled ? getOUIAProps('MyComponent', ouiaId) : {})}
                    />
                );
            };
        "#;

        let known = props(&["ouiaId", "enabled"]);
        let data = data_attrs(&[("div", "data-ouia-component-type")]);

        let bindings = extract_managed_attributes(source, "MyComponent", &known, &data);

        assert!(
            !bindings.is_empty(),
            "Expected bindings through conditional expression"
        );
        assert_eq!(bindings[0].generator_function, "getOUIAProps");
    }

    /// Test: Inline hook call (useOUIAProps) — function component.
    #[test]
    fn test_inline_hook_call() {
        let source = r#"
            export const Alert = ({ ouiaId, ouiaSafe, ...rest }: AlertProps) => {
                return (
                    <div
                        {...rest}
                        {...useOUIAProps('Alert', ouiaId, ouiaSafe)}
                    />
                );
            };
        "#;

        let known = props(&["ouiaId", "ouiaSafe"]);
        let data = data_attrs(&[("div", "data-ouia-component-type")]);

        let bindings = extract_managed_attributes(source, "Alert", &known, &data);

        assert!(
            !bindings.is_empty(),
            "Expected bindings from inline hook call"
        );
        assert_eq!(bindings[0].generator_function, "useOUIAProps");
    }

    /// Test: Method call pattern — `this.getOUIAProps()` in spread.
    #[test]
    fn test_inline_method_call() {
        let source = r#"
            class Widget extends React.Component {
                render() {
                    const { ouiaId, ...rest } = this.props;
                    return (
                        <div
                            {...rest}
                            {...this.getOUIAProps('Widget', ouiaId)}
                        />
                    );
                }
            }
        "#;

        let known = props(&["ouiaId"]);
        let data = data_attrs(&[("div", "data-ouia-component-type")]);

        let bindings = extract_managed_attributes(source, "Widget", &known, &data);

        assert!(
            !bindings.is_empty(),
            "Expected bindings from this.method() call"
        );
        assert_eq!(bindings[0].generator_function, "this.getOUIAProps");
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

    // ── extract_return_object_keys tests ─────────────────────────────

    /// Test: Extract keys from getOUIAProps (function declaration with return).
    #[test]
    fn test_extract_keys_function_declaration() {
        let source = r#"
            export function getOUIAProps(componentType: string, id: OuiaId, ouiaSafe: boolean = true) {
                return {
                    'data-ouia-component-type': `PF6/${componentType}`,
                    'data-ouia-safe': ouiaSafe,
                    'data-ouia-component-id': id
                };
            }
        "#;

        let keys = extract_return_object_keys(source, "getOUIAProps");
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&"data-ouia-component-id".to_string()));
        assert!(keys.contains(&"data-ouia-component-type".to_string()));
        assert!(keys.contains(&"data-ouia-safe".to_string()));
    }

    /// Test: Extract keys from useOUIAProps (exported const arrow with implicit return).
    #[test]
    fn test_extract_keys_arrow_expression_body() {
        let source = r#"
            export const useOUIAProps = (componentType: string, id?: OuiaId, ouiaSafe: boolean = true, variant?: string) => ({
                'data-ouia-component-type': `PF6/${componentType}`,
                'data-ouia-safe': ouiaSafe,
                'data-ouia-component-id': useOUIAId(componentType, id, variant)
            });
        "#;

        let keys = extract_return_object_keys(source, "useOUIAProps");
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&"data-ouia-component-id".to_string()));
        assert!(keys.contains(&"data-ouia-component-type".to_string()));
        assert!(keys.contains(&"data-ouia-safe".to_string()));
    }

    /// Test: No keys from a function that doesn't return an object.
    #[test]
    fn test_extract_keys_no_object_return() {
        let source = r#"
            export function helper(x: number): number {
                return x + 1;
            }
        "#;

        let keys = extract_return_object_keys(source, "helper");
        assert!(keys.is_empty());
    }

    /// Test: Function not found returns empty.
    #[test]
    fn test_extract_keys_function_not_found() {
        let source = r#"
            export function foo() { return {}; }
        "#;

        let keys = extract_return_object_keys(source, "bar");
        assert!(keys.is_empty());
    }

    /// Test: Arrow with block body and return statement.
    #[test]
    fn test_extract_keys_arrow_block_body() {
        let source = r#"
            const getProps = (name: string) => {
                const prefix = 'my-';
                return { 'data-testid': prefix + name, role: 'button' };
            };
        "#;

        let keys = extract_return_object_keys(source, "getProps");
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"data-testid".to_string()));
        assert!(keys.contains(&"role".to_string()));
    }
}
