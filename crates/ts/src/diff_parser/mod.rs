//! TypeScript DiffParser implementation.
//!
//! Parses git diffs between two refs and extracts all functions whose bodies
//! changed. This processes SOURCE files (.ts/.tsx), not .d.ts declaration files.
//!
//! ## Flow
//!
//! 1. `git diff --name-status from_ref..to_ref` → list of changed files
//! 2. Filter to `.ts`/`.tsx`/`.js`/`.jsx` source files (skip tests, configs, .d.ts)
//! 3. For each file, `git show from_ref:path` and `git show to_ref:path`
//! 4. Parse both versions with OXC
//! 5. Extract all function-like declarations from both ASTs
//! 6. Match by qualified name, compare bodies
//! 7. Return `Vec<ChangedFunction>` for all functions with differing bodies

use anyhow::{Context, Result};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;
use semver_analyzer_core::{ChangedFunction, DiffParser, SymbolKind, Visibility};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// TypeScript/JavaScript DiffParser implementation.
///
/// Uses git commands to retrieve file versions and OXC to parse source ASTs.
pub struct TsDiffParser;

impl TsDiffParser {
    pub fn new() -> Self {
        Self
    }
}

impl DiffParser for TsDiffParser {
    fn parse_changed_functions(
        &self,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<Vec<ChangedFunction>> {
        // Step 1: Get list of changed files
        let changed_files = git_diff_name_status(repo, from_ref, to_ref)?;

        let mut all_changed = Vec::new();

        for (status, file_path, renamed_from) in &changed_files {
            // Skip non-source files
            if !is_source_file(file_path) {
                continue;
            }

            match status {
                FileChange::Added => {
                    // New file: all functions are "added" (no old body)
                    let new_source = git_show(repo, to_ref, file_path)?;
                    let new_fns = extract_functions_from_source(&new_source, file_path)?;

                    for func in new_fns {
                        all_changed.push(ChangedFunction {
                            qualified_name: func.qualified_name,
                            name: func.name,
                            file: file_path.clone(),
                            line: func.line,
                            kind: func.kind,
                            visibility: func.visibility,
                            old_body: String::new(),
                            new_body: func.body,
                            old_signature: String::new(),
                            new_signature: func.signature,
                        });
                    }
                }

                FileChange::Deleted => {
                    // Deleted file: all functions are "removed" (no new body)
                    let old_source = git_show(repo, from_ref, file_path)?;
                    let old_fns = extract_functions_from_source(&old_source, file_path)?;

                    for func in old_fns {
                        all_changed.push(ChangedFunction {
                            qualified_name: func.qualified_name,
                            name: func.name,
                            file: file_path.clone(),
                            line: func.line,
                            kind: func.kind,
                            visibility: func.visibility,
                            old_body: func.body,
                            new_body: String::new(),
                            old_signature: func.signature,
                            new_signature: String::new(),
                        });
                    }
                }

                FileChange::Modified => {
                    let old_source = git_show(repo, from_ref, file_path)?;
                    let new_source = git_show(repo, to_ref, file_path)?;

                    let changes = diff_functions_in_file(&old_source, &new_source, file_path)?;
                    all_changed.extend(changes);
                }

                FileChange::Renamed => {
                    // Renamed file: compare old path content with new path content
                    let old_path = renamed_from.as_ref().unwrap_or(file_path);
                    let old_source = git_show(repo, from_ref, old_path)?;
                    let new_source = git_show(repo, to_ref, file_path)?;

                    let changes = diff_functions_in_file(&old_source, &new_source, file_path)?;
                    all_changed.extend(changes);
                }
            }
        }

        Ok(all_changed)
    }
}

// ── Git Operations ──────────────────────────────────────────────────────

/// File change status from `git diff --name-status`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileChange {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// Parse `git diff --name-status` output to get changed files.
///
/// Returns (status, path, optional_renamed_from).
fn git_diff_name_status(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
) -> Result<Vec<(FileChange, PathBuf, Option<PathBuf>)>> {
    let output = Command::new("git")
        .args([
            "diff",
            "--name-status",
            "-M30", // Detect renames with 30% similarity
            &format!("{}..{}", from_ref, to_ref),
        ])
        .current_dir(repo)
        .output()
        .context("Failed to run git diff")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.is_empty() {
            continue;
        }

        let status_char = parts[0].chars().next().unwrap_or('?');
        match status_char {
            'A' if parts.len() >= 2 => {
                results.push((FileChange::Added, PathBuf::from(parts[1]), None));
            }
            'D' if parts.len() >= 2 => {
                results.push((FileChange::Deleted, PathBuf::from(parts[1]), None));
            }
            'M' if parts.len() >= 2 => {
                results.push((FileChange::Modified, PathBuf::from(parts[1]), None));
            }
            'R' if parts.len() >= 3 => {
                // R100\told_path\tnew_path
                results.push((
                    FileChange::Renamed,
                    PathBuf::from(parts[2]),
                    Some(PathBuf::from(parts[1])),
                ));
            }
            _ => {
                // Skip unknown statuses (Copy, etc.)
            }
        }
    }

    Ok(results)
}

/// Get a file's content at a specific git ref.
fn git_show(repo: &Path, git_ref: &str, file_path: &Path) -> Result<String> {
    let spec = format!("{}:{}", git_ref, file_path.display());
    let output = Command::new("git")
        .args(["show", &spec])
        .current_dir(repo)
        .output()
        .with_context(|| format!("Failed to run git show {}", spec))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git show {} failed: {}", spec, stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ── File Filtering ──────────────────────────────────────────────────────

/// Check if a file is a TypeScript/JavaScript source file worth analyzing.
///
/// Excludes:
/// - `.d.ts` declaration files (TD handles those)
/// - Test files (TestAnalyzer handles those separately)
/// - Config files, stories, CSS, docs
fn is_source_file(path: &Path) -> bool {
    let path_str = path.to_string_lossy();

    // Must be TS/JS
    let is_ts_js = path_str.ends_with(".ts")
        || path_str.ends_with(".tsx")
        || path_str.ends_with(".js")
        || path_str.ends_with(".jsx")
        || path_str.ends_with(".mts")
        || path_str.ends_with(".mjs");

    if !is_ts_js {
        return false;
    }

    // Skip .d.ts files (TD handles them)
    if path_str.ends_with(".d.ts") || path_str.ends_with(".d.mts") {
        return false;
    }

    // Skip test files (TestAnalyzer handles them separately)
    if is_test_file(path) {
        return false;
    }

    // Skip non-source files
    let skip_patterns = [
        ".stories.",
        ".story.",
        ".config.",
        ".conf.",
        "__mocks__/",
        "__fixtures__/",
        ".eslintrc",
        "jest.config",
        "vitest.config",
        "webpack.config",
        "rollup.config",
        "vite.config",
        "tsconfig",
        "package.json",
    ];

    for pattern in &skip_patterns {
        if path_str.contains(pattern) {
            return false;
        }
    }

    true
}

/// Check if a file is a test file.
pub(crate) fn is_test_file(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    path_str.contains(".test.")
        || path_str.contains(".spec.")
        || path_str.contains("__tests__/")
        || path_str.contains("__test__/")
        || path_str.ends_with(".test.ts")
        || path_str.ends_with(".test.tsx")
        || path_str.ends_with(".spec.ts")
        || path_str.ends_with(".spec.tsx")
}

// ── Function Extraction from Source AST ─────────────────────────────────

/// A function-like construct extracted from a source file.
#[derive(Debug, Clone)]
struct ExtractedFunction {
    /// Qualified name: `file_path::ClassName.methodName` or `file_path::functionName`
    qualified_name: String,

    /// Simple name.
    name: String,

    /// Line number (1-indexed).
    line: usize,

    /// Symbol kind.
    kind: SymbolKind,

    /// Whether this function is exported.
    visibility: Visibility,

    /// Full function body source text (everything between and including `{ ... }`).
    body: String,

    /// Function signature (everything before the body).
    signature: String,
}

/// Extract all function-like declarations from a TypeScript/JavaScript source file.
///
/// Handles:
/// - `function foo() { ... }` — top-level function declaration
/// - `export function foo() { ... }` — exported function
/// - `const foo = () => { ... }` — arrow function assigned to const/let/var
/// - `const foo = function() { ... }` — function expression assigned to variable
/// - `class Foo { bar() { ... } }` — class method
/// - `class Foo { get bar() { ... } }` — getter
/// - `class Foo { set bar(v) { ... } }` — setter
/// - `class Foo { constructor() { ... } }` — constructor
fn extract_functions_from_source(source: &str, file_path: &Path) -> Result<Vec<ExtractedFunction>> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(file_path).unwrap_or_else(|_| SourceType::tsx());

    let parsed = Parser::new(&allocator, source, source_type).parse();
    // Don't bail on parse errors — extract what we can from partial ASTs.

    let mut functions = Vec::new();
    let file_prefix = file_path.to_string_lossy().to_string();

    extract_from_statements(
        &parsed.program.body,
        source,
        &file_prefix,
        None,  // no class context
        false, // not exported by default
        &mut functions,
    );

    Ok(functions)
}

/// Recursively extract functions from a list of statements.
///
/// `class_name` is set when processing class body methods.
/// `parent_exported` tracks whether the parent context is exported.
fn extract_from_statements(
    stmts: &[Statement<'_>],
    source: &str,
    file_prefix: &str,
    class_name: Option<&str>,
    parent_exported: bool,
    out: &mut Vec<ExtractedFunction>,
) {
    for stmt in stmts {
        match stmt {
            // ── Function declarations ───────────────────────────────
            Statement::FunctionDeclaration(func) => {
                if let Some(id) = &func.id {
                    let name = id.name.to_string();
                    let qualified = match class_name {
                        Some(cls) => format!("{}::{}::{}", file_prefix, cls, name),
                        None => format!("{}::{}", file_prefix, name),
                    };
                    let (sig, body) = split_function_sig_body(func, source);
                    out.push(ExtractedFunction {
                        qualified_name: qualified,
                        name,
                        line: line_number(source, func.span.start as usize),
                        kind: SymbolKind::Function,
                        visibility: if parent_exported {
                            Visibility::Exported
                        } else {
                            Visibility::Internal
                        },
                        body,
                        signature: sig,
                    });
                }
            }

            // ── Variable declarations (arrow fns, fn expressions) ───
            Statement::VariableDeclaration(var_decl) => {
                for declarator in &var_decl.declarations {
                    if let Some(init) = &declarator.init {
                        if let BindingPattern::BindingIdentifier(id) = &declarator.id {
                            let name = id.name.to_string();
                            if let Some(func_info) = extract_from_expression(init, source) {
                                let qualified = match class_name {
                                    Some(cls) => {
                                        format!("{}::{}::{}", file_prefix, cls, name)
                                    }
                                    None => format!("{}::{}", file_prefix, name),
                                };
                                out.push(ExtractedFunction {
                                    qualified_name: qualified,
                                    name,
                                    line: line_number(source, declarator.span.start as usize),
                                    kind: func_info.kind,
                                    visibility: if parent_exported {
                                        Visibility::Exported
                                    } else {
                                        Visibility::Internal
                                    },
                                    body: func_info.body,
                                    signature: func_info.sig,
                                });
                            }
                        }
                    }
                }
            }

            // ── Class declarations ──────────────────────────────────
            Statement::ClassDeclaration(class) => {
                if let Some(id) = &class.id {
                    let cls_name = id.name.to_string();
                    extract_from_class_body(
                        &class.body,
                        source,
                        file_prefix,
                        &cls_name,
                        parent_exported,
                        out,
                    );
                }
            }

            // ── Export named declaration ─────────────────────────────
            Statement::ExportNamedDeclaration(export) => {
                if let Some(decl) = &export.declaration {
                    extract_from_exported_declaration(decl, source, file_prefix, class_name, out);
                }
            }

            // ── Export default declaration ───────────────────────────
            Statement::ExportDefaultDeclaration(export) => match &export.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                    let name = func
                        .id
                        .as_ref()
                        .map(|id| id.name.to_string())
                        .unwrap_or_else(|| "default".to_string());
                    let qualified = format!("{}::{}", file_prefix, name);
                    let (sig, body) = split_function_sig_body(func, source);
                    out.push(ExtractedFunction {
                        qualified_name: qualified,
                        name,
                        line: line_number(source, func.span.start as usize),
                        kind: SymbolKind::Function,
                        visibility: Visibility::Exported,
                        body,
                        signature: sig,
                    });
                }
                ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                    let cls_name = class
                        .id
                        .as_ref()
                        .map(|id| id.name.to_string())
                        .unwrap_or_else(|| "default".to_string());
                    extract_from_class_body(&class.body, source, file_prefix, &cls_name, true, out);
                }
                _ => {}
            },

            _ => {}
        }
    }
}

/// Extract function info from a class body (methods, getters, setters, constructor).
fn extract_from_class_body(
    body: &ClassBody<'_>,
    source: &str,
    file_prefix: &str,
    class_name: &str,
    is_exported: bool,
    out: &mut Vec<ExtractedFunction>,
) {
    for element in &body.body {
        match element {
            ClassElement::MethodDefinition(method) => {
                if method.value.body.is_none() {
                    continue; // Abstract method or declaration — no body to compare
                }

                let name = property_key_name(&method.key);
                let qualified = format!("{}::{}::{}", file_prefix, class_name, name);

                let kind = match method.kind {
                    MethodDefinitionKind::Constructor => SymbolKind::Constructor,
                    MethodDefinitionKind::Get => SymbolKind::GetAccessor,
                    MethodDefinitionKind::Set => SymbolKind::SetAccessor,
                    MethodDefinitionKind::Method => SymbolKind::Method,
                };

                let visibility = if method.accessibility == Some(TSAccessibility::Private) {
                    Visibility::Private
                } else if is_exported {
                    Visibility::Exported
                } else {
                    Visibility::Public
                };

                let (sig, body) = split_function_sig_body(&method.value, source);

                out.push(ExtractedFunction {
                    qualified_name: qualified,
                    name,
                    line: line_number(source, method.span.start as usize),
                    kind,
                    visibility,
                    body,
                    signature: sig,
                });
            }

            ClassElement::PropertyDefinition(prop) => {
                // Check for arrow functions assigned to class properties
                // e.g., `handleClick = () => { ... }`
                if let Some(value) = &prop.value {
                    if let Some(func_info) = extract_from_expression(value, source) {
                        let name = property_key_name(&prop.key);
                        let qualified = format!("{}::{}::{}", file_prefix, class_name, name);

                        let visibility = if prop.accessibility == Some(TSAccessibility::Private) {
                            Visibility::Private
                        } else if is_exported {
                            Visibility::Exported
                        } else {
                            Visibility::Public
                        };

                        out.push(ExtractedFunction {
                            qualified_name: qualified,
                            name,
                            line: line_number(source, prop.span.start as usize),
                            kind: func_info.kind,
                            visibility,
                            body: func_info.body,
                            signature: func_info.sig,
                        });
                    }
                }
            }

            _ => {}
        }
    }
}

/// Info extracted from a function-like expression (arrow function or function expression).
struct FuncExprInfo {
    kind: SymbolKind,
    sig: String,
    body: String,
}

/// Try to extract function info from an expression (arrow function, function expression).
fn extract_from_expression<'a>(expr: &'a Expression<'a>, source: &str) -> Option<FuncExprInfo> {
    match expr {
        Expression::ArrowFunctionExpression(arrow) => {
            let body_span = arrow.body.span;
            let body_str = source[body_span.start as usize..body_span.end as usize].to_string();

            // Signature: everything from arrow start to body start
            let sig_end = body_span.start as usize;
            let sig_start = arrow.span.start as usize;
            let sig = source[sig_start..sig_end].trim_end().to_string();

            Some(FuncExprInfo {
                kind: SymbolKind::Function,
                sig,
                body: body_str,
            })
        }

        Expression::FunctionExpression(func) => {
            let (sig, body) = split_function_sig_body(func, source);
            Some(FuncExprInfo {
                kind: SymbolKind::Function,
                sig,
                body,
            })
        }

        // Handle `as` type assertions wrapping arrows: `(() => {}) as Handler`
        Expression::TSAsExpression(ts_as) => extract_from_expression(&ts_as.expression, source),

        // Handle satisfies: `(() => {}) satisfies Handler`
        Expression::TSSatisfiesExpression(ts_sat) => {
            extract_from_expression(&ts_sat.expression, source)
        }

        // Handle parenthesized: `((() => {}))`
        Expression::ParenthesizedExpression(paren) => {
            extract_from_expression(&paren.expression, source)
        }

        // Handle HOC wrappers: `React.forwardRef((...) => ...)`, `memo((...) => ...)`,
        // `observer(...)`, `styled(...)`, `connect(...)(...) => ...`, etc.
        // Extract the first argument if it's a function expression.
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Argument::ArrowFunctionExpression(arrow) = arg {
                    let body_span = arrow.body.span;
                    let body_str =
                        source[body_span.start as usize..body_span.end as usize].to_string();
                    let sig_end = body_span.start as usize;
                    let sig_start = arrow.span.start as usize;
                    let sig = source[sig_start..sig_end].trim_end().to_string();
                    return Some(FuncExprInfo {
                        kind: SymbolKind::Function,
                        sig,
                        body: body_str,
                    });
                }
                if let Argument::FunctionExpression(func) = arg {
                    let (sig, body) = split_function_sig_body(func, source);
                    return Some(FuncExprInfo {
                        kind: SymbolKind::Function,
                        sig,
                        body,
                    });
                }
            }
            None
        }

        _ => None,
    }
}

// ── Function Body/Signature Splitting ───────────────────────────────────

/// Split a function into its signature (everything before `{`) and body (`{ ... }`).
fn split_function_sig_body(func: &Function<'_>, source: &str) -> (String, String) {
    match &func.body {
        Some(body) => {
            let body_span = body.span;
            let body_str = source[body_span.start as usize..body_span.end as usize].to_string();

            // Signature: from function start to body start
            let sig_start = func.span.start as usize;
            let sig_end = body_span.start as usize;
            let sig = source[sig_start..sig_end].trim_end().to_string();

            (sig, body_str)
        }
        None => {
            // No body (declaration only)
            let full = source[func.span.start as usize..func.span.end as usize].to_string();
            (full, String::new())
        }
    }
}

// ── Cross-Version Comparison ────────────────────────────────────────────

/// Compare functions from two versions of the same file.
///
/// Matches functions by qualified name. Returns `ChangedFunction` entries
/// for functions that:
/// - Exist in both versions with different bodies (modified)
/// - Exist only in old (removed)
/// - Exist only in new (added)
fn diff_functions_in_file(
    old_source: &str,
    new_source: &str,
    file_path: &Path,
) -> Result<Vec<ChangedFunction>> {
    let old_fns = extract_functions_from_source(old_source, file_path)?;
    let new_fns = extract_functions_from_source(new_source, file_path)?;

    let old_map: HashMap<&str, &ExtractedFunction> = old_fns
        .iter()
        .map(|f| (f.qualified_name.as_str(), f))
        .collect();
    let new_map: HashMap<&str, &ExtractedFunction> = new_fns
        .iter()
        .map(|f| (f.qualified_name.as_str(), f))
        .collect();

    let mut changes = Vec::new();

    // Check for modified and removed functions
    for (qname, old_fn) in &old_map {
        if let Some(new_fn) = new_map.get(qname) {
            // Both versions exist — compare bodies
            let old_body_normalized = normalize_body(&old_fn.body);
            let new_body_normalized = normalize_body(&new_fn.body);

            if old_body_normalized != new_body_normalized {
                changes.push(ChangedFunction {
                    qualified_name: qname.to_string(),
                    name: new_fn.name.clone(),
                    file: file_path.to_path_buf(),
                    line: new_fn.line,
                    kind: new_fn.kind,
                    visibility: new_fn.visibility,
                    old_body: old_fn.body.clone(),
                    new_body: new_fn.body.clone(),
                    old_signature: old_fn.signature.clone(),
                    new_signature: new_fn.signature.clone(),
                });
            }
        } else {
            // Function removed
            changes.push(ChangedFunction {
                qualified_name: qname.to_string(),
                name: old_fn.name.clone(),
                file: file_path.to_path_buf(),
                line: old_fn.line,
                kind: old_fn.kind,
                visibility: old_fn.visibility,
                old_body: old_fn.body.clone(),
                new_body: String::new(),
                old_signature: old_fn.signature.clone(),
                new_signature: String::new(),
            });
        }
    }

    // Check for added functions (in new but not in old)
    for (qname, new_fn) in &new_map {
        if !old_map.contains_key(qname) {
            changes.push(ChangedFunction {
                qualified_name: qname.to_string(),
                name: new_fn.name.clone(),
                file: file_path.to_path_buf(),
                line: new_fn.line,
                kind: new_fn.kind,
                visibility: new_fn.visibility,
                old_body: String::new(),
                new_body: new_fn.body.clone(),
                old_signature: String::new(),
                new_signature: new_fn.signature.clone(),
            });
        }
    }

    Ok(changes)
}

/// Normalize a function body for comparison.
///
/// Strips:
/// - Leading/trailing whitespace on each line
/// - Empty lines
/// - Single-line and multi-line comments
///
/// This reduces false positives from formatting-only changes.
fn normalize_body(body: &str) -> String {
    let mut result = String::new();
    let mut in_block_comment = false;

    for line in body.lines() {
        let trimmed = line.trim();

        if in_block_comment {
            if let Some(pos) = trimmed.find("*/") {
                // End of block comment — keep rest of line if any
                let after = trimmed[pos + 2..].trim();
                if !after.is_empty() {
                    result.push_str(after);
                    result.push('\n');
                }
                in_block_comment = false;
            }
            continue;
        }

        // Skip single-line comments
        if trimmed.starts_with("//") {
            continue;
        }

        // Handle block comment start
        if trimmed.contains("/*") {
            if let Some(start_pos) = trimmed.find("/*") {
                let before = trimmed[..start_pos].trim();
                if !before.is_empty() {
                    result.push_str(before);
                    result.push('\n');
                }
                if trimmed[start_pos..].contains("*/") {
                    // Block comment starts and ends on same line
                    if let Some(end_pos) = trimmed[start_pos..].find("*/") {
                        let after = trimmed[start_pos + end_pos + 2..].trim();
                        if !after.is_empty() {
                            result.push_str(after);
                            result.push('\n');
                        }
                    }
                } else {
                    in_block_comment = true;
                }
                continue;
            }
        }

        if !trimmed.is_empty() {
            result.push_str(trimmed);
            result.push('\n');
        }
    }

    result
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Get the name from a property key (used for class methods).
fn property_key_name(key: &PropertyKey<'_>) -> String {
    match key {
        PropertyKey::StaticIdentifier(id) => id.name.to_string(),
        PropertyKey::PrivateIdentifier(id) => format!("#{}", id.name),
        _ => "<computed>".to_string(),
    }
}

/// Convert a byte offset to a 1-indexed line number.
fn line_number(source: &str, byte_offset: usize) -> usize {
    source[..byte_offset.min(source.len())]
        .chars()
        .filter(|&c| c == '\n')
        .count()
        + 1
}

/// Process an exported declaration directly (since we can't easily convert
/// Declaration to Statement in OXC's type system).
fn extract_from_exported_declaration<'a>(
    decl: &'a Declaration<'a>,
    source: &str,
    file_prefix: &str,
    class_name: Option<&str>,
    out: &mut Vec<ExtractedFunction>,
) {
    match decl {
        Declaration::FunctionDeclaration(func) => {
            if let Some(id) = &func.id {
                let name = id.name.to_string();
                let qualified = match class_name {
                    Some(cls) => format!("{}::{}::{}", file_prefix, cls, name),
                    None => format!("{}::{}", file_prefix, name),
                };
                let (sig, body) = split_function_sig_body(func, source);
                out.push(ExtractedFunction {
                    qualified_name: qualified,
                    name,
                    line: line_number(source, func.span.start as usize),
                    kind: SymbolKind::Function,
                    visibility: Visibility::Exported,
                    body,
                    signature: sig,
                });
            }
        }

        Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    if let BindingPattern::BindingIdentifier(id) = &declarator.id {
                        let name = id.name.to_string();
                        if let Some(func_info) = extract_from_expression(init, source) {
                            let qualified = match class_name {
                                Some(cls) => format!("{}::{}::{}", file_prefix, cls, name),
                                None => format!("{}::{}", file_prefix, name),
                            };
                            out.push(ExtractedFunction {
                                qualified_name: qualified,
                                name,
                                line: line_number(source, declarator.span.start as usize),
                                kind: func_info.kind,
                                visibility: Visibility::Exported,
                                body: func_info.body,
                                signature: func_info.sig,
                            });
                        }
                    }
                }
            }
        }

        Declaration::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                let cls_name = id.name.to_string();
                extract_from_class_body(&class.body, source, file_prefix, &cls_name, true, out);
            }
        }

        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── normalize_body tests ────────────────────────────────────────

    #[test]
    fn normalize_strips_comments_and_whitespace() {
        let body = r#"{
  // This is a comment
  const x = 1;
  /* block comment */
  return x + 1;
}"#;
        let normalized = normalize_body(body);
        assert_eq!(normalized, "{\nconst x = 1;\nreturn x + 1;\n}\n");
    }

    #[test]
    fn normalize_strips_multiline_block_comments() {
        let body = r#"{
  const x = 1;
  /*
   * Multi-line
   * comment
   */
  return x;
}"#;
        let normalized = normalize_body(body);
        assert_eq!(normalized, "{\nconst x = 1;\nreturn x;\n}\n");
    }

    #[test]
    fn normalize_identical_bodies_match() {
        let body1 = r#"{
    const x = 1;
    return x;
  }"#;
        let body2 = r#"{
  const x = 1;
  return x;
}"#;
        assert_eq!(normalize_body(body1), normalize_body(body2));
    }

    #[test]
    fn normalize_different_bodies_differ() {
        let body1 = "{ return x + 1; }";
        let body2 = "{ return x + 2; }";
        assert_ne!(normalize_body(body1), normalize_body(body2));
    }

    // ── is_source_file tests ────────────────────────────────────────

    #[test]
    fn source_file_accepts_ts() {
        assert!(is_source_file(Path::new("src/api/users.ts")));
        assert!(is_source_file(Path::new("src/components/Button.tsx")));
        assert!(is_source_file(Path::new("src/utils.js")));
        assert!(is_source_file(Path::new("src/app.jsx")));
        assert!(is_source_file(Path::new("src/lib.mts")));
    }

    #[test]
    fn source_file_rejects_dts() {
        assert!(!is_source_file(Path::new("dist/api/users.d.ts")));
        assert!(!is_source_file(Path::new("types/index.d.mts")));
    }

    #[test]
    fn source_file_rejects_tests() {
        assert!(!is_source_file(Path::new("src/api/users.test.ts")));
        assert!(!is_source_file(Path::new("src/api/users.spec.tsx")));
        assert!(!is_source_file(Path::new("src/__tests__/users.ts")));
    }

    #[test]
    fn source_file_rejects_configs() {
        assert!(!is_source_file(Path::new("jest.config.ts")));
        assert!(!is_source_file(Path::new("vitest.config.ts")));
        assert!(!is_source_file(Path::new("webpack.config.js")));
        assert!(!is_source_file(Path::new("tsconfig.json")));
    }

    #[test]
    fn source_file_rejects_non_js() {
        assert!(!is_source_file(Path::new("src/styles.css")));
        assert!(!is_source_file(Path::new("README.md")));
        assert!(!is_source_file(Path::new("package.json")));
    }

    // ── is_test_file tests ──────────────────────────────────────────

    #[test]
    fn test_file_detection() {
        assert!(is_test_file(Path::new("src/api/users.test.ts")));
        assert!(is_test_file(Path::new("src/api/users.spec.tsx")));
        assert!(is_test_file(Path::new("src/__tests__/users.ts")));
        assert!(!is_test_file(Path::new("src/api/users.ts")));
    }

    // ── Function extraction tests ───────────────────────────────────

    #[test]
    fn extract_top_level_function() {
        let source = r#"
function createUser(email: string): User {
  return db.insert(email);
}
"#;
        let fns = extract_functions_from_source(source, Path::new("src/api.ts")).unwrap();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "createUser");
        assert_eq!(fns[0].qualified_name, "src/api.ts::createUser");
        assert_eq!(fns[0].kind, SymbolKind::Function);
        assert_eq!(fns[0].visibility, Visibility::Internal);
        assert!(fns[0].body.contains("db.insert(email)"));
        assert!(fns[0].signature.contains("createUser"));
    }

    #[test]
    fn extract_exported_function() {
        let source = r#"
export function validate(input: string): boolean {
  return input.length > 0;
}
"#;
        let fns = extract_functions_from_source(source, Path::new("src/utils.ts")).unwrap();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].visibility, Visibility::Exported);
    }

    #[test]
    fn extract_arrow_function_const() {
        let source = r#"
const handler = (req: Request): Response => {
  return new Response("ok");
};
"#;
        let fns = extract_functions_from_source(source, Path::new("src/handler.ts")).unwrap();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "handler");
        assert_eq!(fns[0].kind, SymbolKind::Function);
        assert!(fns[0].body.contains("new Response"));
    }

    #[test]
    fn extract_exported_arrow_function() {
        let source = r#"
export const greet = (name: string): string => {
  return `Hello, ${name}!`;
};
"#;
        let fns = extract_functions_from_source(source, Path::new("src/greet.ts")).unwrap();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "greet");
        assert_eq!(fns[0].visibility, Visibility::Exported);
    }

    #[test]
    fn extract_class_methods() {
        let source = r#"
class UserService {
  constructor(private db: Database) {}

  async createUser(email: string): Promise<User> {
    return this.db.insert(email);
  }

  private validate(email: string): boolean {
    return email.includes("@");
  }

  get count(): number {
    return this.db.count();
  }
}
"#;
        let fns = extract_functions_from_source(source, Path::new("src/service.ts")).unwrap();
        assert_eq!(fns.len(), 4); // constructor, createUser, validate, count

        let constructor = fns.iter().find(|f| f.name == "constructor").unwrap();
        assert_eq!(constructor.kind, SymbolKind::Constructor);

        let create = fns.iter().find(|f| f.name == "createUser").unwrap();
        assert_eq!(create.kind, SymbolKind::Method);
        assert!(create.body.contains("this.db.insert"));

        let validate = fns.iter().find(|f| f.name == "validate").unwrap();
        assert_eq!(validate.visibility, Visibility::Private);

        let count = fns.iter().find(|f| f.name == "count").unwrap();
        assert_eq!(count.kind, SymbolKind::GetAccessor);
    }

    #[test]
    fn extract_exported_class_methods() {
        let source = r#"
export class Validator {
  check(input: string): boolean {
    return input.length > 0;
  }
}
"#;
        let fns = extract_functions_from_source(source, Path::new("src/validator.ts")).unwrap();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "check");
        assert_eq!(fns[0].visibility, Visibility::Exported);
        assert_eq!(fns[0].qualified_name, "src/validator.ts::Validator::check");
    }

    #[test]
    fn extract_default_exported_function() {
        let source = r#"
export default function main(): void {
  console.log("hello");
}
"#;
        let fns = extract_functions_from_source(source, Path::new("src/main.ts")).unwrap();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "main");
        assert_eq!(fns[0].visibility, Visibility::Exported);
    }

    #[test]
    fn extract_class_property_arrow() {
        let source = r#"
class Component {
  handleClick = () => {
    this.setState({ clicked: true });
  };
}
"#;
        let fns = extract_functions_from_source(source, Path::new("src/component.tsx")).unwrap();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "handleClick");
        assert!(fns[0].body.contains("setState"));
    }

    #[test]
    fn extract_multiple_functions() {
        let source = r#"
export function foo(): void {
  console.log("foo");
}

function bar(): void {
  console.log("bar");
}

const baz = (): void => {
  console.log("baz");
};
"#;
        let fns = extract_functions_from_source(source, Path::new("src/multi.ts")).unwrap();
        assert_eq!(fns.len(), 3);

        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"foo"));
        assert!(names.contains(&"bar"));
        assert!(names.contains(&"baz"));
    }

    // ── Cross-version diffing tests ─────────────────────────────────

    #[test]
    fn diff_detects_body_change() {
        let old = r#"
function greet(name: string): string {
  return "Hello, " + name;
}
"#;
        let new = r#"
function greet(name: string): string {
  return `Hello, ${name}!`;
}
"#;
        let changes = diff_functions_in_file(old, new, Path::new("src/greet.ts")).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].name, "greet");
        assert!(changes[0].old_body.contains("\"Hello, \""));
        assert!(changes[0].new_body.contains("${name}"));
    }

    #[test]
    fn diff_ignores_comment_only_changes() {
        let old = r#"
function greet(name: string): string {
  // Original comment
  return "Hello, " + name;
}
"#;
        let new = r#"
function greet(name: string): string {
  // Updated comment
  return "Hello, " + name;
}
"#;
        let changes = diff_functions_in_file(old, new, Path::new("src/greet.ts")).unwrap();
        assert_eq!(changes.len(), 0, "Comment-only changes should be filtered");
    }

    #[test]
    fn diff_ignores_whitespace_only_changes() {
        let old = r#"
function greet(name: string): string {
    return "Hello, " + name;
}
"#;
        let new = r#"
function greet(name: string): string {
  return "Hello, " + name;
}
"#;
        let changes = diff_functions_in_file(old, new, Path::new("src/greet.ts")).unwrap();
        assert_eq!(
            changes.len(),
            0,
            "Whitespace-only changes should be filtered"
        );
    }

    #[test]
    fn diff_detects_added_function() {
        let old = r#"
function existing(): void {
  console.log("hello");
}
"#;
        let new = r#"
function existing(): void {
  console.log("hello");
}

function added(): void {
  console.log("new");
}
"#;
        let changes = diff_functions_in_file(old, new, Path::new("src/funcs.ts")).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].name, "added");
        assert!(changes[0].old_body.is_empty());
        assert!(!changes[0].new_body.is_empty());
    }

    #[test]
    fn diff_detects_removed_function() {
        let old = r#"
function removed(): void {
  console.log("gone");
}

function kept(): void {
  console.log("still here");
}
"#;
        let new = r#"
function kept(): void {
  console.log("still here");
}
"#;
        let changes = diff_functions_in_file(old, new, Path::new("src/funcs.ts")).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].name, "removed");
        assert!(!changes[0].old_body.is_empty());
        assert!(changes[0].new_body.is_empty());
    }

    #[test]
    fn diff_detects_signature_and_body_change() {
        let old = r#"
function process(input: string): string {
  return input.trim();
}
"#;
        let new = r#"
function process(input: string, options?: Options): string {
  if (options?.validate) input = validate(input);
  return input.trim();
}
"#;
        let changes = diff_functions_in_file(old, new, Path::new("src/process.ts")).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(changes[0].old_signature.contains("input: string)"));
        assert!(changes[0].new_signature.contains("options?: Options"));
    }

    // ── line_number tests ───────────────────────────────────────────

    #[test]
    fn line_number_calculation() {
        let source = "line1\nline2\nline3\n";
        assert_eq!(line_number(source, 0), 1);
        assert_eq!(line_number(source, 6), 2); // Start of "line2"
        assert_eq!(line_number(source, 12), 3); // Start of "line3"
    }

    // ── property_key_name tests ─────────────────────────────────────

    #[test]
    fn extract_react_component() {
        let source = r#"
export const Button: React.FC<ButtonProps> = ({ children, onClick }) => {
  return <button onClick={onClick}>{children}</button>;
};
"#;
        let fns = extract_functions_from_source(source, Path::new("src/Button.tsx")).unwrap();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "Button");
        assert_eq!(fns[0].visibility, Visibility::Exported);
    }

    // ── forwardRef / memo HOC wrapper extraction ────────────────────

    #[test]
    fn extract_forward_ref_arrow() {
        let source = r#"
export const Button = React.forwardRef((props: ButtonProps, ref: React.Ref<any>) => (
  <button ref={ref} {...props} />
));
"#;
        let fns = extract_functions_from_source(source, Path::new("src/Button.tsx")).unwrap();
        assert_eq!(
            fns.len(),
            1,
            "Should extract arrow inside forwardRef, got: {:?}",
            fns.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        assert_eq!(fns[0].name, "Button");
        assert_eq!(fns[0].visibility, Visibility::Exported);
        assert!(
            fns[0].body.contains("button"),
            "Body should contain the JSX"
        );
    }

    #[test]
    fn extract_forward_ref_function_expr() {
        let source = r#"
export const Input = React.forwardRef(function Input(props: InputProps, ref) {
  return <input ref={ref} {...props} />;
});
"#;
        let fns = extract_functions_from_source(source, Path::new("src/Input.tsx")).unwrap();
        assert!(
            fns.iter()
                .any(|f| f.name == "Input" && f.visibility == Visibility::Exported),
            "Should extract function inside forwardRef, got: {:?}",
            fns.iter()
                .map(|f| (&f.name, &f.visibility))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn extract_memo_arrow() {
        let source = r#"
export const Label = React.memo((props: LabelProps) => {
  return <span className="label">{props.text}</span>;
});
"#;
        let fns = extract_functions_from_source(source, Path::new("src/Label.tsx")).unwrap();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "Label");
        assert_eq!(fns[0].visibility, Visibility::Exported);
    }

    #[test]
    fn forward_ref_body_change_detected() {
        let old = r#"
export const Button = React.forwardRef((props: ButtonProps, ref: React.Ref<any>) => (
  <button ref={ref} className="old" {...props} />
));
"#;
        let new = r#"
export const Button = React.forwardRef((props: ButtonProps, ref: React.Ref<any>) => (
  <button ref={ref} className="new" {...props} />
));
"#;
        let changes = diff_functions_in_file(old, new, Path::new("src/Button.tsx")).unwrap();
        assert_eq!(
            changes.len(),
            1,
            "Should detect body change inside forwardRef"
        );
        assert_eq!(changes[0].name, "Button");
    }

    #[test]
    fn forward_ref_delegates_to_internal_both_extracted() {
        let source = r#"
const ButtonBase = ({ children, onClick }: ButtonProps) => {
  return <button onClick={onClick}>{children}</button>;
};

export const Button = React.forwardRef((props: ButtonProps, ref: React.Ref<any>) => (
  <ButtonBase innerRef={ref} {...props} />
));
"#;
        let fns = extract_functions_from_source(source, Path::new("src/Button.tsx")).unwrap();
        let names: Vec<_> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(
            names.contains(&"ButtonBase"),
            "Should find ButtonBase, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Button"),
            "Should find Button (forwardRef wrapper), got: {:?}",
            names
        );
    }
}
