//! TypeScript CallGraphBuilder implementation.
//!
//! Finds same-file callers of a given function by walking the OXC AST.
//! Used by BU to walk UP from changed private functions to find affected
//! public APIs.
//!
//! ## Approach
//!
//! For each function-like declaration in the file, walk its body AST and
//! look for references to the target symbol. References are classified as:
//!
//! 1. **Direct call**: `target()` — the target is the callee of a CallExpression
//! 2. **Method call**: `this.target()` — the target is a method on `this`
//! 3. **HOF argument**: `arr.map(target)` — the target is passed as an argument
//!    to another function (higher-order function pattern)
//! 4. **Assignment/reference**: `const fn = target` — the target is referenced
//!    but not directly called (still counts as a usage for propagation)
//!
//! ## Limitations
//!
//! - No cross-file analysis (private functions are same-file only)
//! - No dynamic dispatch (`obj[methodName]()`)
//! - No alias tracking (`const alias = target; alias()` — won't detect `alias` as a call)
//! - String-based matching may have false positives from name shadowing
//!   (mitigated by only searching function bodies, not import declarations)

use anyhow::Result;
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;
use semver_analyzer_core::{CallGraphBuilder, Caller, Reference, Visibility};
use std::path::Path;

/// TypeScript same-file call graph builder.
///
/// Parses a source file with OXC and finds all functions that reference
/// a given symbol within their bodies.
pub struct TsCallGraphBuilder;

impl TsCallGraphBuilder {
    pub fn new() -> Self {
        Self
    }
}

impl CallGraphBuilder for TsCallGraphBuilder {
    fn find_callers(&self, file: &Path, symbol_name: &str) -> Result<Vec<Caller>> {
        let source = std::fs::read_to_string(file)
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", file.display(), e))?;

        find_callers_in_source(&source, file, symbol_name)
    }

    fn find_references(&self, file: &Path, symbol_name: &str) -> Result<Vec<Reference>> {
        let source = std::fs::read_to_string(file)
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", file.display(), e))?;

        find_references_in_source(&source, file, symbol_name)
    }
}

// ── Core Implementation ─────────────────────────────────────────────────

/// Find all functions in a source file that call/reference the target symbol.
///
/// Returns a `Caller` for each function-like declaration whose body
/// contains a reference to `symbol_name`.
pub(crate) fn find_callers_in_source(
    source: &str,
    file_path: &Path,
    symbol_name: &str,
) -> Result<Vec<Caller>> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(file_path).unwrap_or_else(|_| SourceType::tsx());

    let parsed = Parser::new(&allocator, source, source_type).parse();

    let mut callers = Vec::new();
    let file_prefix = file_path.to_string_lossy().to_string();

    collect_callers_from_statements(
        &parsed.program.body,
        source,
        &file_prefix,
        symbol_name,
        None, // no class context
        false,
        &mut callers,
    );

    Ok(callers)
}

/// Find all references to a symbol in a source file (for impact analysis).
pub(crate) fn find_references_in_source(
    source: &str,
    file_path: &Path,
    symbol_name: &str,
) -> Result<Vec<Reference>> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(file_path).unwrap_or_else(|_| SourceType::tsx());

    let parsed = Parser::new(&allocator, source, source_type).parse();

    let mut refs = Vec::new();

    collect_references_from_statements(
        &parsed.program.body,
        source,
        file_path,
        symbol_name,
        None,
        &mut refs,
    );

    Ok(refs)
}

// ── Caller Collection (walk function bodies for references) ─────────────

/// Walk top-level statements to find function-like declarations,
/// then check each one's body for references to the target symbol.
fn collect_callers_from_statements<'a>(
    stmts: &[Statement<'a>],
    source: &str,
    file_prefix: &str,
    target: &str,
    class_name: Option<&str>,
    parent_exported: bool,
    out: &mut Vec<Caller>,
) {
    for stmt in stmts {
        match stmt {
            Statement::FunctionDeclaration(func) => {
                if let Some(id) = &func.id {
                    let name = id.name.as_str();
                    // Don't check if a function references itself
                    if name == target {
                        continue;
                    }
                    if let Some(body) = &func.body {
                        if body_references_symbol(body, source, target) {
                            let qualified = match class_name {
                                Some(cls) => format!("{}::{}::{}", file_prefix, cls, name),
                                None => format!("{}::{}", file_prefix, name),
                            };
                            let (sig, body_str) = split_fn_sig_body(func, source);
                            out.push(Caller {
                                qualified_name: qualified,
                                file: file_prefix.into(),
                                line: line_number(source, func.span.start as usize),
                                visibility: if parent_exported {
                                    Visibility::Exported
                                } else {
                                    Visibility::Internal
                                },
                                body: body_str,
                                signature: sig,
                            });
                        }
                    }
                }
            }

            Statement::VariableDeclaration(var_decl) => {
                for declarator in &var_decl.declarations {
                    if let Some(init) = &declarator.init {
                        if let BindingPattern::BindingIdentifier(id) = &declarator.id {
                            let name = id.name.as_str();
                            if name == target {
                                continue;
                            }
                            if let Some(arrow_body) = get_function_body_from_expr(init) {
                                if source_range_references_symbol(
                                    source,
                                    arrow_body.0,
                                    arrow_body.1,
                                    target,
                                ) {
                                    let qualified = match class_name {
                                        Some(cls) => {
                                            format!("{}::{}::{}", file_prefix, cls, name)
                                        }
                                        None => format!("{}::{}", file_prefix, name),
                                    };
                                    let body_str = source[arrow_body.0..arrow_body.1].to_string();
                                    let sig_end = arrow_body.0;
                                    let sig_start = declarator.span.start as usize;
                                    let sig = source[sig_start..sig_end].trim_end().to_string();

                                    out.push(Caller {
                                        qualified_name: qualified,
                                        file: file_prefix.into(),
                                        line: line_number(source, declarator.span.start as usize),
                                        visibility: if parent_exported {
                                            Visibility::Exported
                                        } else {
                                            Visibility::Internal
                                        },
                                        body: body_str,
                                        signature: sig,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            Statement::ClassDeclaration(class) => {
                if let Some(id) = &class.id {
                    let cls_name = id.name.as_str();
                    collect_callers_from_class_body(
                        &class.body,
                        source,
                        file_prefix,
                        cls_name,
                        target,
                        parent_exported,
                        out,
                    );
                }
            }

            Statement::ExportNamedDeclaration(export) => {
                if let Some(decl) = &export.declaration {
                    collect_callers_from_declaration(
                        decl,
                        source,
                        file_prefix,
                        target,
                        class_name,
                        out,
                    );
                }
            }

            Statement::ExportDefaultDeclaration(export) => match &export.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                    let name = func
                        .id
                        .as_ref()
                        .map(|id| id.name.as_str())
                        .unwrap_or("default");
                    if name != target {
                        if let Some(body) = &func.body {
                            if body_references_symbol(body, source, target) {
                                let qualified = format!("{}::{}", file_prefix, name);
                                let (sig, body_str) = split_fn_sig_body(func, source);
                                out.push(Caller {
                                    qualified_name: qualified,
                                    file: file_prefix.into(),
                                    line: line_number(source, func.span.start as usize),
                                    visibility: Visibility::Exported,
                                    body: body_str,
                                    signature: sig,
                                });
                            }
                        }
                    }
                }
                ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                    let cls_name = class
                        .id
                        .as_ref()
                        .map(|id| id.name.as_str())
                        .unwrap_or("default");
                    collect_callers_from_class_body(
                        &class.body,
                        source,
                        file_prefix,
                        cls_name,
                        target,
                        true,
                        out,
                    );
                }
                _ => {}
            },

            _ => {}
        }
    }
}

/// Check class methods for references to the target symbol.
fn collect_callers_from_class_body<'a>(
    body: &ClassBody<'a>,
    source: &str,
    file_prefix: &str,
    class_name: &str,
    target: &str,
    is_exported: bool,
    out: &mut Vec<Caller>,
) {
    for element in &body.body {
        match element {
            ClassElement::MethodDefinition(method) => {
                let name = property_key_name(&method.key);
                if name == target {
                    continue;
                }

                if let Some(fn_body) = &method.value.body {
                    if body_references_symbol(fn_body, source, target) {
                        let qualified = format!("{}::{}::{}", file_prefix, class_name, name);

                        let visibility = if method.accessibility == Some(TSAccessibility::Private) {
                            Visibility::Private
                        } else if is_exported {
                            Visibility::Exported
                        } else {
                            Visibility::Public
                        };

                        let (sig, body_str) = split_fn_sig_body(&method.value, source);

                        out.push(Caller {
                            qualified_name: qualified,
                            file: file_prefix.into(),
                            line: line_number(source, method.span.start as usize),
                            visibility,
                            body: body_str,
                            signature: sig,
                        });
                    }
                }
            }

            ClassElement::PropertyDefinition(prop) => {
                if let Some(value) = &prop.value {
                    let name = property_key_name(&prop.key);
                    if name == target {
                        continue;
                    }
                    if let Some(arrow_body) = get_function_body_from_expr(value) {
                        if source_range_references_symbol(
                            source,
                            arrow_body.0,
                            arrow_body.1,
                            target,
                        ) {
                            let qualified = format!("{}::{}::{}", file_prefix, class_name, name);

                            let visibility = if prop.accessibility == Some(TSAccessibility::Private)
                            {
                                Visibility::Private
                            } else if is_exported {
                                Visibility::Exported
                            } else {
                                Visibility::Public
                            };

                            let body_str = source[arrow_body.0..arrow_body.1].to_string();
                            let sig_start = prop.span.start as usize;
                            let sig = source[sig_start..arrow_body.0].trim_end().to_string();

                            out.push(Caller {
                                qualified_name: qualified,
                                file: file_prefix.into(),
                                line: line_number(source, prop.span.start as usize),
                                visibility,
                                body: body_str,
                                signature: sig,
                            });
                        }
                    }
                }
            }

            _ => {}
        }
    }
}

/// Handle exported declarations for caller collection.
fn collect_callers_from_declaration<'a>(
    decl: &Declaration<'a>,
    source: &str,
    file_prefix: &str,
    target: &str,
    class_name: Option<&str>,
    out: &mut Vec<Caller>,
) {
    match decl {
        Declaration::FunctionDeclaration(func) => {
            if let Some(id) = &func.id {
                let name = id.name.as_str();
                if name != target {
                    if let Some(body) = &func.body {
                        if body_references_symbol(body, source, target) {
                            let qualified = match class_name {
                                Some(cls) => {
                                    format!("{}::{}::{}", file_prefix, cls, name)
                                }
                                None => format!("{}::{}", file_prefix, name),
                            };
                            let (sig, body_str) = split_fn_sig_body(func, source);
                            out.push(Caller {
                                qualified_name: qualified,
                                file: file_prefix.into(),
                                line: line_number(source, func.span.start as usize),
                                visibility: Visibility::Exported,
                                body: body_str,
                                signature: sig,
                            });
                        }
                    }
                }
            }
        }

        Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    if let BindingPattern::BindingIdentifier(id) = &declarator.id {
                        let name = id.name.as_str();
                        if name == target {
                            continue;
                        }
                        if let Some(arrow_body) = get_function_body_from_expr(init) {
                            if source_range_references_symbol(
                                source,
                                arrow_body.0,
                                arrow_body.1,
                                target,
                            ) {
                                let qualified = match class_name {
                                    Some(cls) => {
                                        format!("{}::{}::{}", file_prefix, cls, name)
                                    }
                                    None => format!("{}::{}", file_prefix, name),
                                };
                                let body_str = source[arrow_body.0..arrow_body.1].to_string();
                                let sig_start = declarator.span.start as usize;
                                let sig = source[sig_start..arrow_body.0].trim_end().to_string();

                                out.push(Caller {
                                    qualified_name: qualified,
                                    file: file_prefix.into(),
                                    line: line_number(source, declarator.span.start as usize),
                                    visibility: Visibility::Exported,
                                    body: body_str,
                                    signature: sig,
                                });
                            }
                        }
                    }
                }
            }
        }

        Declaration::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                let cls_name = id.name.as_str();
                collect_callers_from_class_body(
                    &class.body,
                    source,
                    file_prefix,
                    cls_name,
                    target,
                    true,
                    out,
                );
            }
        }

        _ => {}
    }
}

// ── Reference Detection ─────────────────────────────────────────────────

/// Check if a function body contains a reference to the target symbol.
///
/// Uses the source text within the body's span to find identifier occurrences.
/// Matches whole words only to avoid substring false positives
/// (e.g., "validate" shouldn't match inside "validateEmail").
fn body_references_symbol(body: &FunctionBody<'_>, source: &str, target: &str) -> bool {
    let start = body.span.start as usize;
    let end = body.span.end as usize;
    source_range_references_symbol(source, start, end, target)
}

/// Check if a source range contains a whole-word reference to the target symbol.
///
/// A "whole word" match requires the character before and after the match
/// to be non-identifier characters (not alphanumeric or underscore).
/// This prevents false positives like matching "validate" inside "validateEmail".
fn source_range_references_symbol(source: &str, start: usize, end: usize, target: &str) -> bool {
    let body_text = &source[start..end.min(source.len())];

    let mut search_from = 0;
    while let Some(pos) = body_text[search_from..].find(target) {
        let abs_pos = search_from + pos;
        let after_pos = abs_pos + target.len();

        // Check word boundary before
        let before_ok = abs_pos == 0 || {
            let c = body_text.as_bytes()[abs_pos - 1] as char;
            !c.is_alphanumeric() && c != '_' && c != '$'
        };

        // Check word boundary after
        let after_ok = after_pos >= body_text.len() || {
            let c = body_text.as_bytes()[after_pos] as char;
            !c.is_alphanumeric() && c != '_' && c != '$'
        };

        if before_ok && after_ok {
            return true;
        }

        search_from = abs_pos + 1;
    }

    false
}

// ── Reference Collection (for impact analysis) ─────────────────────────

/// Collect all references to a symbol across all functions in a file.
fn collect_references_from_statements<'a>(
    stmts: &[Statement<'a>],
    source: &str,
    file_path: &Path,
    target: &str,
    enclosing: Option<&str>,
    out: &mut Vec<Reference>,
) {
    for stmt in stmts {
        match stmt {
            Statement::FunctionDeclaration(func) => {
                if let Some(id) = &func.id {
                    let name = id.name.to_string();
                    if name == target {
                        continue;
                    }
                    if let Some(body) = &func.body {
                        if body_references_symbol(body, source, target) {
                            let start = body.span.start as usize;
                            let body_text = &source[start..body.span.end as usize];
                            collect_refs_in_text(
                                body_text,
                                start,
                                source,
                                file_path,
                                target,
                                Some(&name),
                                out,
                            );
                        }
                    }
                }
            }

            Statement::VariableDeclaration(var_decl) => {
                for declarator in &var_decl.declarations {
                    if let BindingPattern::BindingIdentifier(id) = &declarator.id {
                        let name = id.name.to_string();
                        if name == target {
                            continue;
                        }
                        if let Some(init) = &declarator.init {
                            if let Some(body_range) = get_function_body_from_expr(init) {
                                let body_text = &source[body_range.0..body_range.1];
                                if source_range_references_symbol(
                                    source,
                                    body_range.0,
                                    body_range.1,
                                    target,
                                ) {
                                    collect_refs_in_text(
                                        body_text,
                                        body_range.0,
                                        source,
                                        file_path,
                                        target,
                                        Some(&name),
                                        out,
                                    );
                                }
                            }
                        }
                    }
                }
            }

            // Module-level references (not inside a function)
            _ => {
                let span_start = stmt_span_start(stmt);
                let span_end = stmt_span_end(stmt);
                if let (Some(s), Some(e)) = (span_start, span_end) {
                    if source_range_references_symbol(source, s, e, target) {
                        collect_refs_in_text(
                            &source[s..e],
                            s,
                            source,
                            file_path,
                            target,
                            enclosing,
                            out,
                        );
                    }
                }
            }
        }
    }
}

/// Find individual reference positions within a text range.
fn collect_refs_in_text(
    text: &str,
    base_offset: usize,
    full_source: &str,
    file_path: &Path,
    target: &str,
    enclosing_symbol: Option<&str>,
    out: &mut Vec<Reference>,
) {
    let mut search_from = 0;
    while let Some(pos) = text[search_from..].find(target) {
        let abs_pos = search_from + pos;
        let after_pos = abs_pos + target.len();

        let before_ok = abs_pos == 0 || {
            let c = text.as_bytes()[abs_pos - 1] as char;
            !c.is_alphanumeric() && c != '_' && c != '$'
        };

        let after_ok = after_pos >= text.len() || {
            let c = text.as_bytes()[after_pos] as char;
            !c.is_alphanumeric() && c != '_' && c != '$'
        };

        if before_ok && after_ok {
            let global_offset = base_offset + abs_pos;
            out.push(Reference {
                file: file_path.to_path_buf(),
                line: line_number(full_source, global_offset),
                local_binding: target.to_string(),
                enclosing_symbol: enclosing_symbol.map(|s| s.to_string()),
            });
        }

        search_from = abs_pos + 1;
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Get the body span (start, end) from a function-like expression.
fn get_function_body_from_expr<'a>(expr: &'a Expression<'a>) -> Option<(usize, usize)> {
    match expr {
        Expression::ArrowFunctionExpression(arrow) => {
            let span = arrow.body.span;
            Some((span.start as usize, span.end as usize))
        }
        Expression::FunctionExpression(func) => func
            .body
            .as_ref()
            .map(|b| (b.span.start as usize, b.span.end as usize)),
        Expression::TSAsExpression(ts_as) => get_function_body_from_expr(&ts_as.expression),
        Expression::TSSatisfiesExpression(ts_sat) => {
            get_function_body_from_expr(&ts_sat.expression)
        }
        Expression::ParenthesizedExpression(paren) => {
            get_function_body_from_expr(&paren.expression)
        }
        // Handle HOC wrappers: `React.forwardRef((...) => ...)`, `memo((...) => ...)`, etc.
        // Extract the first argument if it's a function expression.
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Argument::ArrowFunctionExpression(arrow) = arg {
                    let span = arrow.body.span;
                    return Some((span.start as usize, span.end as usize));
                }
                if let Argument::FunctionExpression(func) = arg {
                    return func
                        .body
                        .as_ref()
                        .map(|b| (b.span.start as usize, b.span.end as usize));
                }
            }
            None
        }
        _ => None,
    }
}

/// Split function into signature and body strings.
fn split_fn_sig_body(func: &Function<'_>, source: &str) -> (String, String) {
    match &func.body {
        Some(body) => {
            let body_str = source[body.span.start as usize..body.span.end as usize].to_string();
            let sig = source[func.span.start as usize..body.span.start as usize]
                .trim_end()
                .to_string();
            (sig, body_str)
        }
        None => {
            let full = source[func.span.start as usize..func.span.end as usize].to_string();
            (full, String::new())
        }
    }
}

/// Get property key name.
fn property_key_name(key: &PropertyKey<'_>) -> String {
    match key {
        PropertyKey::StaticIdentifier(id) => id.name.to_string(),
        PropertyKey::PrivateIdentifier(id) => format!("#{}", id.name),
        _ => "<computed>".to_string(),
    }
}

/// Convert byte offset to 1-indexed line number.
fn line_number(source: &str, byte_offset: usize) -> usize {
    source[..byte_offset.min(source.len())]
        .chars()
        .filter(|&c| c == '\n')
        .count()
        + 1
}

/// Get span start for a statement (best effort).
fn stmt_span_start(stmt: &Statement<'_>) -> Option<usize> {
    use oxc_span::GetSpan;
    Some(stmt.span().start as usize)
}

/// Get span end for a statement (best effort).
fn stmt_span_end(stmt: &Statement<'_>) -> Option<usize> {
    use oxc_span::GetSpan;
    Some(stmt.span().end as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Word boundary matching tests ────────────────────────────────

    #[test]
    fn word_boundary_exact_match() {
        assert!(source_range_references_symbol("foo()", 0, 5, "foo"));
        assert!(source_range_references_symbol("call(foo)", 0, 9, "foo"));
        assert!(source_range_references_symbol("x = foo;", 0, 8, "foo"));
    }

    #[test]
    fn word_boundary_rejects_substrings() {
        assert!(!source_range_references_symbol("fooBar()", 0, 8, "foo"));
        assert!(!source_range_references_symbol("barfoo()", 0, 8, "foo"));
        assert!(!source_range_references_symbol("_foo()", 0, 6, "foo"));
        assert!(!source_range_references_symbol("foo2()", 0, 6, "foo"));
        assert!(!source_range_references_symbol("$foo()", 0, 6, "foo"));
    }

    #[test]
    fn word_boundary_common_contexts() {
        assert!(source_range_references_symbol("this.foo()", 0, 10, "foo"));
        assert!(source_range_references_symbol("arr.map(foo)", 0, 12, "foo"));
        assert!(source_range_references_symbol("[foo, bar]", 0, 10, "foo"));
        assert!(source_range_references_symbol("{ foo: 1 }", 0, 10, "foo"));
        assert!(source_range_references_symbol("if (foo) {}", 0, 11, "foo"));
        assert!(source_range_references_symbol("return foo;", 0, 11, "foo"));
    }

    // ── Direct call detection ───────────────────────────────────────

    #[test]
    fn finds_direct_caller() {
        let source = r#"
function helper(): number {
  return 42;
}

function main(): number {
  return helper() + 1;
}
"#;
        let callers = find_callers_in_source(source, Path::new("test.ts"), "helper").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].qualified_name, "test.ts::main");
        assert!(callers[0].body.contains("helper()"));
    }

    #[test]
    fn doesnt_find_self_reference() {
        let source = r#"
function recursive(n: number): number {
  if (n <= 1) return 1;
  return n * recursive(n - 1);
}
"#;
        let callers = find_callers_in_source(source, Path::new("test.ts"), "recursive").unwrap();
        assert_eq!(callers.len(), 0, "Should not include self-reference");
    }

    #[test]
    fn finds_multiple_callers() {
        let source = r#"
function validate(x: string): boolean {
  return x.length > 0;
}

function processA(input: string): string {
  if (!validate(input)) throw new Error();
  return input.trim();
}

function processB(input: string): string {
  validate(input);
  return input.toUpperCase();
}
"#;
        let callers = find_callers_in_source(source, Path::new("test.ts"), "validate").unwrap();
        assert_eq!(callers.len(), 2);

        let names: Vec<&str> = callers.iter().map(|c| c.qualified_name.as_str()).collect();
        assert!(names.contains(&"test.ts::processA"));
        assert!(names.contains(&"test.ts::processB"));
    }

    // ── Arrow function callers ──────────────────────────────────────

    #[test]
    fn finds_arrow_function_caller() {
        let source = r#"
function helper(): number {
  return 42;
}

const main = (): number => {
  return helper() + 1;
};
"#;
        let callers = find_callers_in_source(source, Path::new("test.ts"), "helper").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].qualified_name, "test.ts::main");
    }

    // ── HOF pattern detection ───────────────────────────────────────

    #[test]
    fn finds_hof_map_caller() {
        let source = r#"
function transform(x: number): number {
  return x * 2;
}

function processAll(items: number[]): number[] {
  return items.map(transform);
}
"#;
        let callers = find_callers_in_source(source, Path::new("test.ts"), "transform").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].qualified_name, "test.ts::processAll");
    }

    #[test]
    fn finds_settimeout_hof() {
        let source = r#"
function cleanup(): void {
  console.log("done");
}

function scheduleCleanup(): void {
  setTimeout(cleanup, 1000);
}
"#;
        let callers = find_callers_in_source(source, Path::new("test.ts"), "cleanup").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].qualified_name, "test.ts::scheduleCleanup");
    }

    #[test]
    fn finds_event_handler_hof() {
        let source = r#"
function onMessage(msg: string): void {
  console.log(msg);
}

function setup(): void {
  emitter.on('message', onMessage);
}
"#;
        let callers = find_callers_in_source(source, Path::new("test.ts"), "onMessage").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].qualified_name, "test.ts::setup");
    }

    // ── Class method callers ────────────────────────────────────────

    #[test]
    fn finds_class_method_caller() {
        let source = r#"
class Service {
  private validate(input: string): boolean {
    return input.length > 0;
  }

  process(input: string): string {
    if (!this.validate(input)) throw new Error();
    return input.trim();
  }
}
"#;
        let callers = find_callers_in_source(source, Path::new("test.ts"), "validate").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].qualified_name, "test.ts::Service::process");
    }

    #[test]
    fn class_method_visibility() {
        let source = r#"
export class Service {
  private helper(): void {}

  public doWork(): void {
    this.helper();
  }

  private internal(): void {
    this.helper();
  }
}
"#;
        let callers = find_callers_in_source(source, Path::new("test.ts"), "helper").unwrap();
        assert_eq!(callers.len(), 2);

        let do_work = callers
            .iter()
            .find(|c| c.qualified_name.contains("doWork"))
            .unwrap();
        assert_eq!(do_work.visibility, Visibility::Exported);

        let internal = callers
            .iter()
            .find(|c| c.qualified_name.contains("internal"))
            .unwrap();
        assert_eq!(internal.visibility, Visibility::Private);
    }

    // ── Exported function detection ─────────────────────────────────

    #[test]
    fn exported_caller_has_exported_visibility() {
        let source = r#"
function _private(): number {
  return 42;
}

export function publicApi(): number {
  return _private() + 1;
}
"#;
        let callers = find_callers_in_source(source, Path::new("test.ts"), "_private").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].visibility, Visibility::Exported);
    }

    #[test]
    fn non_exported_caller_has_internal_visibility() {
        let source = r#"
function _private(): number {
  return 42;
}

function wrapper(): number {
  return _private() + 1;
}
"#;
        let callers = find_callers_in_source(source, Path::new("test.ts"), "_private").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].visibility, Visibility::Internal);
    }

    // ── Class property arrow function callers ───────────────────────

    #[test]
    fn finds_class_property_arrow_caller() {
        let source = r#"
class Component {
  private validate(): boolean {
    return true;
  }

  handleClick = () => {
    if (this.validate()) {
      this.setState({ valid: true });
    }
  };
}
"#;
        let callers = find_callers_in_source(source, Path::new("test.tsx"), "validate").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(
            callers[0].qualified_name,
            "test.tsx::Component::handleClick"
        );
    }

    // ── No false positives ──────────────────────────────────────────

    #[test]
    fn no_false_positive_substring() {
        let source = r#"
function validate(): boolean {
  return true;
}

function validateAll(): boolean {
  return true;
}

function runValidateAll(): boolean {
  return validateAll();
}
"#;
        let callers = find_callers_in_source(source, Path::new("test.ts"), "validate").unwrap();
        // runValidateAll calls validateAll, NOT validate
        assert_eq!(callers.len(), 0);
    }

    // ── Reference finding tests ─────────────────────────────────────

    #[test]
    fn finds_references_in_functions() {
        let source = r#"
function target(): number {
  return 42;
}

function caller1(): number {
  return target() + 1;
}

function caller2(): number {
  const val = target();
  return val * 2;
}
"#;
        let refs = find_references_in_source(source, Path::new("test.ts"), "target").unwrap();
        assert_eq!(refs.len(), 2);
        assert!(refs
            .iter()
            .any(|r| r.enclosing_symbol.as_deref() == Some("caller1")));
        assert!(refs
            .iter()
            .any(|r| r.enclosing_symbol.as_deref() == Some("caller2")));
    }

    // ── Export default function ──────────────────────────────────────

    #[test]
    fn finds_export_default_function_caller() {
        let source = r#"
function helper(): string {
  return "hello";
}

export default function main(): string {
  return helper() + " world";
}
"#;
        let callers = find_callers_in_source(source, Path::new("test.ts"), "helper").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].qualified_name, "test.ts::main");
        assert_eq!(callers[0].visibility, Visibility::Exported);
    }

    // ── Nested function calls ───────────────────────────────────────

    #[test]
    fn finds_nested_call_in_expression() {
        let source = r#"
function normalize(s: string): string {
  return s.toLowerCase();
}

function process(input: string): string {
  return normalize(input.trim()).replace(/\s+/g, '-');
}
"#;
        let callers = find_callers_in_source(source, Path::new("test.ts"), "normalize").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].qualified_name, "test.ts::process");
    }

    // ── forwardRef / memo HOC wrappers ──────────────────────────────

    #[test]
    fn finds_caller_in_forward_ref_wrapper() {
        let source = r#"
const ButtonBase = ({ children }: ButtonProps) => {
  return <button>{children}</button>;
};

export const Button = React.forwardRef((props: ButtonProps, ref: React.Ref<any>) => (
  <ButtonBase innerRef={ref} {...props} />
));
"#;
        let callers =
            find_callers_in_source(source, Path::new("Button.tsx"), "ButtonBase").unwrap();
        assert!(
            callers
                .iter()
                .any(|c| c.qualified_name == "Button.tsx::Button"
                    && c.visibility == Visibility::Exported),
            "forwardRef wrapper should be found as caller of ButtonBase, got: {:?}",
            callers
                .iter()
                .map(|c| (&c.qualified_name, &c.visibility))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn finds_caller_in_memo_wrapper() {
        let source = r#"
function renderLabel(text: string) {
  return <span>{text}</span>;
}

export const Label = React.memo((props: LabelProps) => {
  return <div>{renderLabel(props.text)}</div>;
});
"#;
        let callers =
            find_callers_in_source(source, Path::new("Label.tsx"), "renderLabel").unwrap();
        assert!(
            callers
                .iter()
                .any(|c| c.qualified_name == "Label.tsx::Label"
                    && c.visibility == Visibility::Exported),
            "memo wrapper should be found as caller, got: {:?}",
            callers
                .iter()
                .map(|c| &c.qualified_name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn forward_ref_function_expression_found() {
        let source = r#"
function formatValue(v: number) { return String(v); }

export const Counter = React.forwardRef(function Counter(props: CounterProps, ref) {
  return <span ref={ref}>{formatValue(props.count)}</span>;
});
"#;
        let callers =
            find_callers_in_source(source, Path::new("Counter.tsx"), "formatValue").unwrap();
        assert!(
            callers
                .iter()
                .any(|c| c.qualified_name == "Counter.tsx::Counter"),
            "forwardRef with function expression should be found, got: {:?}",
            callers
                .iter()
                .map(|c| &c.qualified_name)
                .collect::<Vec<_>>()
        );
    }
}
