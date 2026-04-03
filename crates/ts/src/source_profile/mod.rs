//! Source-level profile extraction for the v2 SD pipeline.
//!
//! Extracts a `ComponentSourceProfile` from a React component's `.tsx` source
//! file by combining:
//! - JSX render output analysis (elements, attributes, components)
//! - BEM token structure (from `styles.*` references)
//! - React API usage (createPortal, useContext, forwardRef, memo)
//! - Prop default values (from destructuring patterns)
//! - Children slot position (where `{children}` lands in the JSX tree)
//!
//! All extractions are deterministic — no LLM, no confidence scores.

pub mod bem;
pub mod children_slot;
pub mod diff;
pub mod prop_defaults;
pub mod react_api;

use crate::sd_types::ComponentSourceProfile;
use bem::{extract_style_tokens, parse_bem_structure, StyleToken};
use children_slot::{has_children_prop, trace_children_slot};
use prop_defaults::extract_prop_defaults;
use react_api::detect_react_api_usage;
use std::collections::{BTreeMap, BTreeSet};

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;

/// Extract a `ComponentSourceProfile` from a component's source file.
///
/// `name` is the component name (e.g., "Dropdown").
/// `file` is the relative path (e.g., "packages/react-core/src/components/Dropdown/Dropdown.tsx").
/// `source` is the full source text of the `.tsx` file.
pub fn extract_profile(name: &str, file: &str, source: &str) -> ComponentSourceProfile {
    let mut profile = ComponentSourceProfile {
        name: name.to_string(),
        file: file.to_string(),
        ..Default::default()
    };

    // ── Single-parse AST extraction ─────────────────────────────────
    // Extracts JSX info, styles import path (BEM block), and interface
    // extends clauses from one OXC parse pass.
    let ast_info = extract_source_info(source, name);

    // 1. JSX render output
    profile.rendered_elements = ast_info
        .element_tags
        .iter()
        .filter(|(tag, _)| tag.starts_with(|c: char| c.is_lowercase()))
        .map(|(k, v)| (k.clone(), *v as u32))
        .collect();
    profile.rendered_components = ast_info
        .element_tags
        .keys()
        .filter(|tag| tag.starts_with(|c: char| c.is_uppercase()))
        .cloned()
        .collect();
    profile.aria_attributes = ast_info
        .aria_attrs
        .iter()
        .map(|((elem, attr), val)| ((elem.clone(), attr.clone()), val.clone()))
        .collect();
    profile.role_attributes = ast_info.role_attrs.clone();
    profile.data_attributes = ast_info
        .data_attrs
        .iter()
        .map(|((elem, attr), val)| ((elem.clone(), attr.clone()), val.clone()))
        .collect();

    // 2. BEM token analysis — use import-derived block as ground truth
    let style_tokens = extract_style_tokens(source);
    for token in &style_tokens {
        match token {
            StyleToken::ClassToken(name) => {
                profile.css_tokens_used.insert(format!("styles.{name}"));
            }
            StyleToken::Modifier(name) => {
                profile
                    .css_tokens_used
                    .insert(format!("styles.modifiers.{name}"));
            }
        }
    }
    let bem = parse_bem_structure(&style_tokens, ast_info.styles_bem_block.as_deref());
    profile.bem_block = bem.block;
    profile.bem_elements = bem.elements;
    profile.bem_modifiers = bem.modifiers;

    // 3. React API usage
    let react_usage = detect_react_api_usage(source);
    profile.uses_portal = react_usage.uses_portal;
    profile.portal_target = react_usage.portal_target;
    profile.consumed_contexts = react_usage.consumed_contexts;
    profile.is_forward_ref = react_usage.is_forward_ref;
    profile.is_memo = react_usage.is_memo;

    // 4. Prop defaults
    profile.prop_defaults = extract_prop_defaults(source);

    // 5. Children slot
    profile.has_children_prop = has_children_prop(source);
    profile.children_slot_path = trace_children_slot(source);

    // 6. Props extends — from AST interface declarations
    profile.extends_props = ast_info.extends_props;

    // 7a. All props — from interface body
    profile.all_props = ast_info.all_props;
    profile.prop_types = ast_info.prop_types;

    // 7. Provided contexts — derived from rendered_components
    profile.provided_contexts = profile
        .rendered_components
        .iter()
        .filter_map(|rc| rc.strip_suffix(".Provider").map(|s| s.to_string()))
        .collect();

    profile
}

/// Convert kebab-case to camelCase.
///
/// The CSS file path uses kebab-case (`modal-box`) but the JS token names
/// use camelCase (`modalBox`). This conversion aligns them.
fn kebab_to_camel_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut capitalize_next = false;
    for ch in s.chars() {
        if ch == '-' {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(ch.to_ascii_uppercase());
            capitalize_next = false;
        } else {
            result.push(ch);
        }
    }
    result
}

// (Props extends extraction is now done via the AST in extract_source_info)

// ── JSX info extraction (reused from jsx_diff, adapted for full source) ─

/// Aggregated information extracted from a full source file's AST.
#[derive(Debug, Default)]
struct FullSourceInfo {
    // ── JSX ─────────────────────────────────────────────────────────
    element_tags: BTreeMap<String, usize>,
    aria_attrs: BTreeMap<(String, String), String>,
    role_attrs: BTreeMap<String, String>,
    data_attrs: BTreeMap<(String, String), String>,

    // ── Imports ─────────────────────────────────────────────────────
    /// BEM block name derived from the primary `styles` import path.
    /// e.g., `import styles from '@patternfly/react-styles/css/components/Menu/menu'`
    /// → `Some("menu")`
    styles_bem_block: Option<String>,

    // ── Interface extends ───────────────────────────────────────────
    /// Props interfaces that the component's Props type extends.
    /// Extracted from TSInterfaceDeclaration extends clauses.
    extends_props: Vec<String>,

    // ── Interface props ─────────────────────────────────────────────
    /// All prop names from the component's Props interface body.
    all_props: BTreeSet<String>,
    /// Prop name → type annotation string.
    prop_types: BTreeMap<String, String>,
}

/// Extract all AST-level info from a full source file in a single parse.
///
/// Collects JSX elements/attributes, the styles import path (for BEM
/// block resolution), and interface extends clauses.
fn extract_source_info(source: &str, component_name: &str) -> FullSourceInfo {
    let allocator = Allocator::default();
    let source_type = SourceType::tsx();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    let mut info = FullSourceInfo::default();

    for stmt in &parsed.program.body {
        // Extract imports, interfaces, and JSX from top-level statements
        extract_from_module_stmt(stmt, source, component_name, &mut info);
    }

    info
}

/// Extract info from a single top-level statement (module-level).
fn extract_from_module_stmt<'a>(
    stmt: &'a Statement<'a>,
    source: &str,
    component_name: &str,
    info: &mut FullSourceInfo,
) {
    match stmt {
        // ── Import declarations ─────────────────────────────────────
        Statement::ImportDeclaration(import) => {
            let src = import.source.value.as_str();

            // Check for styles import from @patternfly/react-styles/css/...
            if src.contains("@patternfly/react-styles/css/") {
                // Check if the default binding is `styles` (not e.g., `breadcrumbStyles`)
                if let Some(specifiers) = &import.specifiers {
                    for spec in specifiers {
                        if let oxc_ast::ast::ImportDeclarationSpecifier::ImportDefaultSpecifier(
                            default_spec,
                        ) = spec
                        {
                            if default_spec.local.name == "styles" {
                                // Extract block from last path segment, converting
                                // kebab-case to camelCase to match JS token names.
                                // e.g., "modal-box" → "modalBox"
                                if let Some(block) = src.rsplit('/').next() {
                                    info.styles_bem_block = Some(kebab_to_camel_case(block));
                                }
                            }
                        }
                    }
                }
            }
        }

        // ── Export named (may contain interface or class declarations) ──
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                extract_from_decl(decl, source, component_name, info);
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let Some(expr) = export.declaration.as_expression() {
                walk_expr_for_jsx(expr, source, info);
            }
        }

        // ── Bare declarations (interface, class, function, variable) ──
        Statement::TSInterfaceDeclaration(iface) => {
            extract_extends_from_interface(iface, component_name, source, info);
        }

        // Walk all other statements for JSX
        _ => walk_stmt_for_jsx(stmt, source, info),
    }
}

/// Extract info from a declaration (inside export or bare).
fn extract_from_decl<'a>(
    decl: &'a Declaration<'a>,
    source: &str,
    component_name: &str,
    info: &mut FullSourceInfo,
) {
    match decl {
        Declaration::TSInterfaceDeclaration(iface) => {
            extract_extends_from_interface(iface, component_name, source, info);
        }
        Declaration::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                walk_stmts_for_jsx(&body.statements, source, info);
            }
        }
        Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    walk_expr_for_jsx(init, source, info);
                }
            }
        }
        Declaration::ClassDeclaration(cls) => {
            for item in &cls.body.body {
                if let ClassElement::MethodDefinition(method) = item {
                    if let Some(body) = &method.value.body {
                        walk_stmts_for_jsx(&body.statements, source, info);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Extract extends clause from a TS interface declaration.
///
/// Matches interfaces named `{Component}Props` or `{Component}BaseProps`.
/// Unwraps utility types like `Omit<MenuItemProps, 'ref'>` to extract
/// the underlying props type.
fn extract_extends_from_interface(
    iface: &oxc_ast::ast::TSInterfaceDeclaration,
    component_name: &str,
    source: &str,
    info: &mut FullSourceInfo,
) {
    let iface_name = iface.id.name.as_str();

    // Only extract from the component's own Props interface
    let props_name = format!("{}Props", component_name);
    let base_props_name = format!("{}BaseProps", component_name);
    if iface_name != props_name && iface_name != base_props_name {
        return;
    }

    for heritage in &iface.extends {
        let type_name = resolve_heritage_props_type(heritage);
        if let Some(name) = type_name {
            if name.ends_with("Props") && name != iface_name {
                info.extends_props.push(name);
            }
        }
    }

    // Extract prop names and types from the interface body
    for sig in &iface.body.body {
        if let oxc_ast::ast::TSSignature::TSPropertySignature(prop) = sig {
            if let oxc_ast::ast::PropertyKey::StaticIdentifier(id) = &prop.key {
                let prop_name = id.name.to_string();
                info.all_props.insert(prop_name.clone());

                // Extract the type annotation if present
                if let Some(type_ann) = &prop.type_annotation {
                    let type_str =
                        &source[type_ann.span.start as usize..type_ann.span.end as usize];
                    // Strip the leading `: ` from the type annotation span
                    let type_str = type_str.trim_start_matches(':').trim();
                    if !type_str.is_empty() {
                        info.prop_types.insert(prop_name, type_str.to_string());
                    }
                }
            }
        }
    }
}

/// Resolve the actual Props type name from a heritage clause.
///
/// Handles:
/// - `MenuProps` → "MenuProps" (direct reference)
/// - `Omit<MenuItemProps, 'ref'>` → "MenuItemProps" (unwrap utility type)
/// - `Partial<MenuProps>` → "MenuProps"
/// - `Pick<MenuProps, 'x' | 'y'>` → "MenuProps"
fn resolve_heritage_props_type(heritage: &oxc_ast::ast::TSInterfaceHeritage) -> Option<String> {
    let expr_name = match &heritage.expression {
        Expression::Identifier(id) => id.name.as_str(),
        _ => return None,
    };

    // Direct Props reference (e.g., `extends MenuProps`)
    if expr_name.ends_with("Props") {
        return Some(expr_name.to_string());
    }

    // Utility type wrapper (e.g., `extends Omit<MenuItemProps, 'ref'>`)
    if matches!(
        expr_name,
        "Omit" | "Partial" | "Pick" | "Required" | "Readonly"
    ) {
        // Extract the first type argument — that's the actual Props type
        if let Some(type_args) = &heritage.type_arguments {
            if let Some(first_param) = type_args.params.first() {
                if let oxc_ast::ast::TSType::TSTypeReference(type_ref) = first_param {
                    if let oxc_ast::ast::TSTypeName::IdentifierReference(id) = &type_ref.type_name {
                        return Some(id.name.to_string());
                    }
                }
            }
        }
    }

    None
}

// ── AST walking for JSX extraction ──────────────────────────────────────
// These mirror the jsx_diff walkers but populate FullSourceInfo.

fn walk_stmts_for_jsx<'a>(stmts: &'a [Statement<'a>], source: &str, info: &mut FullSourceInfo) {
    for stmt in stmts {
        walk_stmt_for_jsx(stmt, source, info);
    }
}

fn walk_stmt_for_jsx<'a>(stmt: &'a Statement<'a>, source: &str, info: &mut FullSourceInfo) {
    match stmt {
        Statement::ClassDeclaration(cls) => {
            for item in &cls.body.body {
                if let ClassElement::MethodDefinition(method) = item {
                    if let Some(body) = &method.value.body {
                        walk_stmts_for_jsx(&body.statements, source, info);
                    }
                }
            }
        }
        Statement::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                walk_stmts_for_jsx(&body.statements, source, info);
            }
        }
        Statement::ReturnStatement(ret) => {
            if let Some(expr) = &ret.argument {
                walk_expr_for_jsx(expr, source, info);
            }
        }
        Statement::ExpressionStatement(expr_stmt) => {
            walk_expr_for_jsx(&expr_stmt.expression, source, info);
        }
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                if let Some(init) = &declarator.init {
                    walk_expr_for_jsx(init, source, info);
                }
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                walk_decl_for_jsx(decl, source, info);
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let Some(expr) = export.declaration.as_expression() {
                walk_expr_for_jsx(expr, source, info);
            }
        }
        Statement::IfStatement(if_stmt) => {
            walk_stmt_for_jsx(&if_stmt.consequent, source, info);
            if let Some(alt) = &if_stmt.alternate {
                walk_stmt_for_jsx(alt, source, info);
            }
        }
        Statement::BlockStatement(block) => {
            walk_stmts_for_jsx(&block.body, source, info);
        }
        _ => {}
    }
}

fn walk_decl_for_jsx<'a>(decl: &'a Declaration<'a>, source: &str, info: &mut FullSourceInfo) {
    match decl {
        Declaration::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                walk_stmts_for_jsx(&body.statements, source, info);
            }
        }
        Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    walk_expr_for_jsx(init, source, info);
                }
            }
        }
        Declaration::ClassDeclaration(cls) => {
            for item in &cls.body.body {
                if let ClassElement::MethodDefinition(method) = item {
                    if let Some(body) = &method.value.body {
                        walk_stmts_for_jsx(&body.statements, source, info);
                    }
                }
            }
        }
        _ => {}
    }
}

fn walk_expr_for_jsx<'a>(expr: &'a Expression<'a>, source: &str, info: &mut FullSourceInfo) {
    match expr {
        Expression::JSXElement(el) => visit_jsx_element_info(el, source, info),
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                walk_jsx_child_info(child, source, info);
            }
        }
        Expression::ParenthesizedExpression(paren) => {
            walk_expr_for_jsx(&paren.expression, source, info);
        }
        Expression::ConditionalExpression(cond) => {
            walk_expr_for_jsx(&cond.consequent, source, info);
            walk_expr_for_jsx(&cond.alternate, source, info);
        }
        Expression::LogicalExpression(logical) => {
            walk_expr_for_jsx(&logical.right, source, info);
        }
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Some(expr) = arg.as_expression() {
                    walk_expr_for_jsx(expr, source, info);
                }
            }
        }
        Expression::ArrowFunctionExpression(arrow) => {
            walk_stmts_for_jsx(&arrow.body.statements, source, info);
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                walk_stmts_for_jsx(&body.statements, source, info);
            }
        }
        _ => {}
    }
}

fn walk_jsx_child_info<'a>(child: &'a JSXChild<'a>, source: &str, info: &mut FullSourceInfo) {
    match child {
        JSXChild::Element(el) => visit_jsx_element_info(el, source, info),
        JSXChild::Fragment(frag) => {
            for c in &frag.children {
                walk_jsx_child_info(c, source, info);
            }
        }
        JSXChild::ExpressionContainer(container) => {
            if let Some(expr) = container.expression.as_expression() {
                walk_expr_for_jsx(expr, source, info);
            }
        }
        _ => {}
    }
}

fn visit_jsx_element_info<'a>(el: &'a JSXElement<'a>, source: &str, info: &mut FullSourceInfo) {
    let tag_name = jsx_element_name_str(&el.opening_element.name);

    *info.element_tags.entry(tag_name.clone()).or_insert(0) += 1;

    // Extract attributes
    for attr_item in &el.opening_element.attributes {
        if let JSXAttributeItem::Attribute(attr) = attr_item {
            let attr_name = jsx_attr_name_str(&attr.name);
            let attr_value = attr
                .value
                .as_ref()
                .map(|v| jsx_attr_value_str(v, source))
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
    }

    // Recurse into children
    for child in &el.children {
        walk_jsx_child_info(child, source, info);
    }

    // Also recurse into attribute values (for JSX in props)
    for attr_item in &el.opening_element.attributes {
        if let JSXAttributeItem::Attribute(attr) = attr_item {
            if let Some(JSXAttributeValue::ExpressionContainer(container)) = &attr.value {
                if let Some(expr) = container.expression.as_expression() {
                    walk_expr_for_jsx(expr, source, info);
                }
            }
        }
    }
}

// ── JSX name/value helpers ──────────────────────────────────────────────

fn jsx_element_name_str(name: &JSXElementName) -> String {
    match name {
        JSXElementName::Identifier(id) => id.name.to_string(),
        JSXElementName::IdentifierReference(id) => id.name.to_string(),
        JSXElementName::NamespacedName(ns) => {
            format!("{}:{}", ns.namespace.name, ns.name.name)
        }
        JSXElementName::MemberExpression(member) => {
            format!(
                "{}.{}",
                jsx_member_obj_str(&member.object),
                member.property.name
            )
        }
        JSXElementName::ThisExpression(_) => "this".to_string(),
    }
}

fn jsx_member_obj_str(obj: &JSXMemberExpressionObject) -> String {
    match obj {
        JSXMemberExpressionObject::IdentifierReference(id) => id.name.to_string(),
        JSXMemberExpressionObject::MemberExpression(member) => {
            format!(
                "{}.{}",
                jsx_member_obj_str(&member.object),
                member.property.name
            )
        }
        JSXMemberExpressionObject::ThisExpression(_) => "this".to_string(),
    }
}

fn jsx_attr_name_str(name: &JSXAttributeName) -> String {
    match name {
        JSXAttributeName::Identifier(id) => id.name.to_string(),
        JSXAttributeName::NamespacedName(ns) => {
            format!("{}:{}", ns.namespace.name, ns.name.name)
        }
    }
}

fn jsx_attr_value_str(value: &JSXAttributeValue, source: &str) -> String {
    match value {
        JSXAttributeValue::StringLiteral(s) => s.value.to_string(),
        JSXAttributeValue::ExpressionContainer(container) => {
            let span = container.span;
            source
                .get(span.start as usize..span.end as usize)
                .unwrap_or("")
                .to_string()
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_profile_simple() {
        let source = r#"
            import styles from '@patternfly/react-styles/css/components/Menu/menu';
            import { css } from '@patternfly/react-styles';

            export const MenuList = ({ children, className }: MenuListProps) => (
                <ul className={css(styles.menuList, className)}>
                    {children}
                </ul>
            );
        "#;

        let profile = extract_profile(
            "MenuList",
            "packages/react-core/src/components/Menu/MenuList.tsx",
            source,
        );
        assert_eq!(profile.name, "MenuList");
        assert!(profile.rendered_elements.contains_key("ul"));
        assert!(profile.has_children_prop);
        assert_eq!(profile.children_slot_path, vec!["ul"]);
        assert!(profile.css_tokens_used.contains("styles.menuList"));
    }

    #[test]
    fn test_extract_profile_with_portal() {
        let source = r#"
            import * as ReactDOM from 'react-dom';

            class Modal extends React.Component {
                render() {
                    return ReactDOM.createPortal(
                        <ModalContent>{this.props.children}</ModalContent>,
                        this.getElement(this.props.appendTo)
                    );
                }
            }
            export { Modal };
        "#;

        let profile = extract_profile("Modal", "Modal.tsx", source);
        assert!(profile.uses_portal);
        assert!(profile.portal_target.is_some());
    }

    #[test]
    fn test_extract_profile_with_context() {
        let source = r#"
            import { useContext } from 'react';
            import { AccordionItemContext } from './AccordionItemContext';

            export const AccordionContent = ({ children }: Props) => {
                const { isExpanded } = useContext(AccordionItemContext);
                return isExpanded ? <div>{children}</div> : null;
            };
        "#;

        let profile = extract_profile("AccordionContent", "AccordionContent.tsx", source);
        assert!(profile
            .consumed_contexts
            .contains(&"AccordionItemContext".to_string()));
    }

    #[test]
    fn test_extract_extends_props() {
        let source = r#"
            import { MenuListProps, MenuList } from '../Menu';

            export interface DropdownListProps extends MenuListProps {
                children: React.ReactNode;
                className?: string;
            }
        "#;

        let profile = extract_profile("DropdownList", "DropdownList.tsx", source);
        assert_eq!(profile.extends_props, vec!["MenuListProps"]);
    }

    #[test]
    fn test_extract_extends_props_multiple() {
        let source = r#"
            export interface DropdownProps extends MenuProps, OUIAProps {
                children?: React.ReactNode;
            }
        "#;

        let profile = extract_profile("Dropdown", "Dropdown.tsx", source);
        assert_eq!(profile.extends_props, vec!["MenuProps", "OUIAProps"]);
    }

    #[test]
    fn test_extract_extends_props_omit() {
        let source = r#"
            export interface DropdownItemProps extends Omit<MenuItemProps, 'ref'>, OUIAProps {
                children?: React.ReactNode;
            }
        "#;

        let profile = extract_profile("DropdownItem", "DropdownItem.tsx", source);
        assert_eq!(profile.extends_props, vec!["MenuItemProps", "OUIAProps"]);
    }

    #[test]
    fn test_extract_profile_class_component_context() {
        let source = r#"
            import { Component } from 'react';
            import { MenuContext } from './MenuContext';

            export interface MenuProps {
                children?: React.ReactNode;
            }

            class MenuBase extends Component<MenuProps> {
                render() {
                    return (
                        <MenuContext.Provider value={{ menuId: 'test' }}>
                            <div>{this.props.children}</div>
                        </MenuContext.Provider>
                    );
                }
            }

            export const Menu = MenuBase;
        "#;

        let profile = extract_profile("Menu", "Menu.tsx", source);
        assert!(
            profile
                .rendered_components
                .contains(&"MenuContext.Provider".to_string()),
            "Expected MenuContext.Provider in rendered_components, got: {:?}",
            profile.rendered_components
        );
        assert!(
            profile
                .provided_contexts
                .contains(&"MenuContext".to_string()),
            "Expected MenuContext in provided_contexts, got: {:?}",
            profile.provided_contexts
        );
        assert!(
            profile.has_children_prop,
            "Expected has_children_prop=true for class component with children?: React.ReactNode"
        );
    }

    #[test]
    fn test_extract_profile_with_defaults() {
        let source = r#"
            export const Button = ({
                variant = 'primary',
                isDisabled = false,
                children,
            }: ButtonProps) => (
                <button disabled={isDisabled}>{children}</button>
            );
        "#;

        let profile = extract_profile("Button", "Button.tsx", source);
        assert_eq!(
            profile.prop_defaults.get("variant"),
            Some(&"'primary'".to_string())
        );
        assert_eq!(
            profile.prop_defaults.get("isDisabled"),
            Some(&"false".to_string())
        );
    }
}
