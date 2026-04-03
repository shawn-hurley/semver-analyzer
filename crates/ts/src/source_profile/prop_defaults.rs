//! Prop default value extraction from destructuring patterns.
//!
//! React components receive props as a single argument and typically
//! destructure them with default values:
//!
//! ```tsx
//! const MyComponent = ({
//!     variant = 'primary',
//!     isOpen = false,
//!     zIndex = 9999,
//!     onOpenChangeKeys = ['Escape', 'Tab'],
//! }: MyComponentProps) => { ... };
//! ```
//!
//! These defaults are NOT present in `.d.ts` files (TypeScript strips them).
//! This module extracts them from the `.tsx` source so SD can diff them.

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};
use std::collections::BTreeMap;

/// Extract prop default values from a component's source file.
///
/// Looks for function/arrow function declarations with an object destructuring
/// pattern as the first parameter, and extracts any default values.
///
/// Returns a map of prop_name → default_value_expression (as source text).
pub fn extract_prop_defaults(source: &str) -> BTreeMap<String, String> {
    let allocator = Allocator::default();
    let source_type = SourceType::tsx();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    let mut defaults = BTreeMap::new();

    for stmt in &parsed.program.body {
        collect_defaults_from_statement(stmt, source, &mut defaults);
    }

    defaults
}

fn collect_defaults_from_statement<'a>(
    stmt: &'a Statement<'a>,
    source: &str,
    defaults: &mut BTreeMap<String, String>,
) {
    match stmt {
        Statement::FunctionDeclaration(f) => {
            collect_defaults_from_params(&f.params, source, defaults);
            // Also check the body for inner component functions
            if let Some(body) = &f.body {
                for inner_stmt in &body.statements {
                    collect_defaults_from_statement(inner_stmt, source, defaults);
                }
            }
        }
        Statement::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    collect_defaults_from_expression(init, source, defaults);
                }
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                collect_defaults_from_declaration(decl, source, defaults);
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let Some(expr) = export.declaration.as_expression() {
                collect_defaults_from_expression(expr, source, defaults);
            }
        }
        _ => {}
    }
}

fn collect_defaults_from_declaration<'a>(
    decl: &'a Declaration<'a>,
    source: &str,
    defaults: &mut BTreeMap<String, String>,
) {
    match decl {
        Declaration::FunctionDeclaration(f) => {
            collect_defaults_from_params(&f.params, source, defaults);
        }
        Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    collect_defaults_from_expression(init, source, defaults);
                }
            }
        }
        _ => {}
    }
}

fn collect_defaults_from_expression<'a>(
    expr: &'a Expression<'a>,
    source: &str,
    defaults: &mut BTreeMap<String, String>,
) {
    match expr {
        Expression::ArrowFunctionExpression(arrow) => {
            collect_defaults_from_params(&arrow.params, source, defaults);
        }
        Expression::FunctionExpression(func) => {
            collect_defaults_from_params(&func.params, source, defaults);
        }
        Expression::CallExpression(call) => {
            // Handle forwardRef((...) => ...) and memo((...) => ...)
            for arg in &call.arguments {
                if let Some(expr) = arg.as_expression() {
                    collect_defaults_from_expression(expr, source, defaults);
                }
            }
        }
        Expression::ParenthesizedExpression(paren) => {
            collect_defaults_from_expression(&paren.expression, source, defaults);
        }
        _ => {}
    }
}

fn collect_defaults_from_params(
    params: &FormalParameters,
    source: &str,
    defaults: &mut BTreeMap<String, String>,
) {
    // React components typically have a single object destructuring parameter
    for param in &params.items {
        if let BindingPattern::ObjectPattern(obj) = &param.pattern {
            for prop in &obj.properties {
                extract_default_from_binding_property(prop, source, defaults);
            }
        }

        // Also handle AssignmentPattern wrapping ObjectPattern:
        // ({ variant = 'primary' }: Props = {})
        if let BindingPattern::AssignmentPattern(assign) = &param.pattern {
            if let BindingPattern::ObjectPattern(obj) = &assign.left {
                for prop in &obj.properties {
                    extract_default_from_binding_property(prop, source, defaults);
                }
            }
        }
    }
}

fn extract_default_from_binding_property(
    prop: &BindingProperty,
    source: &str,
    defaults: &mut BTreeMap<String, String>,
) {
    // Get the property name
    let name = match &prop.key {
        PropertyKey::StaticIdentifier(id) => id.name.to_string(),
        _ => return,
    };

    // Check if the value has a default (AssignmentPattern)
    if let BindingPattern::AssignmentPattern(assign) = &prop.value {
        let default_text = span_text(source, assign.right.span()).trim().to_string();
        if !default_text.is_empty() {
            defaults.insert(name, default_text);
        }
    }
}

fn span_text(source: &str, span: Span) -> &str {
    &source[span.start as usize..span.end as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_arrow_function_defaults() {
        let source = r#"
            const MyComponent = ({
                variant = 'primary',
                isOpen = false,
                zIndex = 9999,
                onOpenChangeKeys = ['Escape', 'Tab'],
            }: MyComponentProps) => {
                return <div />;
            };
        "#;

        let defaults = extract_prop_defaults(source);
        assert_eq!(defaults.get("variant"), Some(&"'primary'".to_string()));
        assert_eq!(defaults.get("isOpen"), Some(&"false".to_string()));
        assert_eq!(defaults.get("zIndex"), Some(&"9999".to_string()));
        assert_eq!(
            defaults.get("onOpenChangeKeys"),
            Some(&"['Escape', 'Tab']".to_string())
        );
    }

    #[test]
    fn test_extract_function_declaration_defaults() {
        let source = r#"
            function MyComponent({
                size = 'md',
                isDisabled = false,
            }: Props) {
                return <div />;
            }
        "#;

        let defaults = extract_prop_defaults(source);
        assert_eq!(defaults.get("size"), Some(&"'md'".to_string()));
        assert_eq!(defaults.get("isDisabled"), Some(&"false".to_string()));
    }

    #[test]
    fn test_extract_exported_component_defaults() {
        let source = r#"
            export const Button = ({
                variant = 'primary',
                children,
            }: ButtonProps) => (
                <button>{children}</button>
            );
        "#;

        let defaults = extract_prop_defaults(source);
        assert_eq!(defaults.get("variant"), Some(&"'primary'".to_string()));
        assert!(!defaults.contains_key("children"));
    }

    #[test]
    fn test_extract_forward_ref_defaults() {
        let source = r#"
            export const DropdownBase: React.FunctionComponent<DropdownProps> = ({
                children,
                className,
                isOpen,
                shouldFocusToggleOnSelect = false,
                shouldPreventScrollOnItemFocus = true,
                focusTimeoutDelay = 0,
            }: DropdownProps) => {
                return <div />;
            };

            export const Dropdown = forwardRef((props: DropdownProps, ref: React.Ref<any>) => (
                <DropdownBase innerRef={ref} {...props} />
            ));
        "#;

        let defaults = extract_prop_defaults(source);
        assert_eq!(
            defaults.get("shouldFocusToggleOnSelect"),
            Some(&"false".to_string())
        );
        assert_eq!(
            defaults.get("shouldPreventScrollOnItemFocus"),
            Some(&"true".to_string())
        );
        assert_eq!(defaults.get("focusTimeoutDelay"), Some(&"0".to_string()));
    }

    #[test]
    fn test_no_defaults() {
        let source = r#"
            export const Plain = ({ children, className }: Props) => (
                <div className={className}>{children}</div>
            );
        "#;

        let defaults = extract_prop_defaults(source);
        assert!(defaults.is_empty());
    }
}
