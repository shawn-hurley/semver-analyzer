//! Java diff parser — detect changed method bodies between git refs.
//!
//! Uses `git diff --name-status` to find changed `.java` files, then
//! `git show ref:path` to get file content at each ref, and tree-sitter
//! to parse method/constructor declarations from both versions.

use anyhow::{Context, Result};
use semver_analyzer_core::git::read_git_file;
use semver_analyzer_core::{ChangedFunction, SymbolKind, Visibility};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use tree_sitter::{Node, Parser};

/// Java diff parser for the BU pipeline.
#[derive(Default)]
pub struct JavaDiffParser;

impl JavaDiffParser {
    pub fn new() -> Self {
        Self
    }

    /// Parse all changed functions between two git refs.
    pub fn parse_changed_functions(
        &self,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<Vec<ChangedFunction>> {
        let changed_files = get_changed_java_files(repo, from_ref, to_ref)?;
        let mut changed_functions = Vec::new();

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .context("Failed to set tree-sitter Java language")?;

        for (status, old_path, new_path) in &changed_files {
            match status.as_str() {
                "M" => {
                    let old_content = read_git_file(repo, from_ref, old_path).unwrap_or_default();
                    let new_content = read_git_file(repo, to_ref, new_path).unwrap_or_default();
                    let mut file_changes =
                        diff_functions_in_file(&mut parser, &old_content, &new_content, new_path)?;
                    changed_functions.append(&mut file_changes);
                }
                "A" => {
                    let new_content = read_git_file(repo, to_ref, new_path).unwrap_or_default();
                    let funcs = extract_functions(&mut parser, &new_content, new_path)?;
                    for func in funcs {
                        changed_functions.push(ChangedFunction {
                            qualified_name: func.qualified_name,
                            name: func.name,
                            file: PathBuf::from(new_path),
                            line: func.line,
                            kind: func.kind,
                            visibility: func.visibility,
                            old_body: None,
                            new_body: Some(func.body),
                            old_signature: None,
                            new_signature: Some(func.signature),
                        });
                    }
                }
                "D" => {
                    let old_content = read_git_file(repo, from_ref, old_path).unwrap_or_default();
                    let funcs = extract_functions(&mut parser, &old_content, old_path)?;
                    for func in funcs {
                        changed_functions.push(ChangedFunction {
                            qualified_name: func.qualified_name,
                            name: func.name,
                            file: PathBuf::from(old_path),
                            line: func.line,
                            kind: func.kind,
                            visibility: func.visibility,
                            old_body: Some(func.body),
                            new_body: None,
                            old_signature: Some(func.signature),
                            new_signature: None,
                        });
                    }
                }
                _ if status.starts_with('R') => {
                    let old_content = read_git_file(repo, from_ref, old_path).unwrap_or_default();
                    let new_content = read_git_file(repo, to_ref, new_path).unwrap_or_default();
                    let mut file_changes =
                        diff_functions_in_file(&mut parser, &old_content, &new_content, new_path)?;
                    changed_functions.append(&mut file_changes);
                }
                _ => {}
            }
        }

        Ok(changed_functions)
    }
}

// ── Git helpers ─────────────────────────────────────────────────────────

fn get_changed_java_files(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
) -> Result<Vec<(String, String, String)>> {
    let output = Command::new("git")
        .args([
            "diff",
            "--name-status",
            "-M30",
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
    let mut files = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            continue;
        }

        let status = parts[0].to_string();
        let path = parts[1].to_string();

        if !is_java_source(&path) {
            continue;
        }

        if status.starts_with('R') && parts.len() >= 3 {
            let new_path = parts[2].to_string();
            if is_java_source(&new_path) {
                files.push((status, path, new_path));
            }
        } else {
            files.push((status.clone(), path.clone(), path));
        }
    }

    Ok(files)
}

fn is_java_source(path: &str) -> bool {
    path.ends_with(".java")
        && !path.contains("/test/")
        && !path.ends_with("Test.java")
        && !path.ends_with("Tests.java")
        && !path.ends_with("IT.java")
        && !path.contains("module-info.java")
        && !path.contains("package-info.java")
}

// ── Function extraction ─────────────────────────────────────────────────

struct ExtractedFunction {
    qualified_name: String,
    name: String,
    body: String,
    signature: String,
    visibility: Visibility,
    kind: SymbolKind,
    line: usize,
}

fn extract_functions(
    parser: &mut Parser,
    source: &str,
    file_path: &str,
) -> Result<Vec<ExtractedFunction>> {
    if source.is_empty() {
        return Ok(Vec::new());
    }

    let tree = parser
        .parse(source, None)
        .context("tree-sitter failed to parse")?;

    let root = tree.root_node();
    let mut functions = Vec::new();
    let package = extract_package_name(root, source);

    walk_for_functions(root, source, file_path, &package, "", &mut functions);

    Ok(functions)
}

fn walk_for_functions(
    node: Node,
    source: &str,
    file_path: &str,
    package: &Option<String>,
    parent_class: &str,
    functions: &mut Vec<ExtractedFunction>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration" => {
                let class_name = child
                    .child_by_field_name("name")
                    .map(|n| node_text(n, source))
                    .unwrap_or("");

                let qualified_class = if parent_class.is_empty() {
                    match package {
                        Some(pkg) => format!("{}.{}", pkg, class_name),
                        None => class_name.to_string(),
                    }
                } else {
                    format!("{}.{}", parent_class, class_name)
                };

                walk_for_functions(
                    child,
                    source,
                    file_path,
                    package,
                    &qualified_class,
                    functions,
                );
            }
            "method_declaration" | "constructor_declaration" => {
                let name = child
                    .child_by_field_name("name")
                    .map(|n| node_text(n, source))
                    .unwrap_or("");

                let qualified_name = if parent_class.is_empty() {
                    format!("{}::{}", file_path, name)
                } else {
                    format!("{}::{}", parent_class, name)
                };

                let body = find_child_by_kind(child, "block")
                    .or_else(|| find_child_by_kind(child, "constructor_body"))
                    .map(|n| node_text(n, source))
                    .unwrap_or("")
                    .to_string();

                let body_start = find_child_by_kind(child, "block")
                    .or_else(|| find_child_by_kind(child, "constructor_body"))
                    .map(|n| n.start_byte())
                    .unwrap_or(child.end_byte());
                let signature = source[child.start_byte()..body_start].trim().to_string();

                let visibility = extract_visibility_enum(child);
                let kind = if child.kind() == "constructor_declaration" {
                    SymbolKind::Constructor
                } else {
                    SymbolKind::Method
                };

                functions.push(ExtractedFunction {
                    qualified_name,
                    name: name.to_string(),
                    body,
                    signature,
                    visibility,
                    kind,
                    line: child.start_position().row + 1,
                });
            }
            _ => {
                walk_for_functions(child, source, file_path, package, parent_class, functions);
            }
        }
    }
}

fn extract_package_name(root: Node, source: &str) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "package_declaration" {
            let mut inner = child.walk();
            for pkg_child in child.children(&mut inner) {
                if pkg_child.kind() == "scoped_identifier" || pkg_child.kind() == "identifier" {
                    return Some(node_text(pkg_child, source).to_string());
                }
            }
        }
    }
    None
}

fn extract_visibility_enum(node: Node) -> Visibility {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mut mod_cursor = child.walk();
            for mod_child in child.children(&mut mod_cursor) {
                match mod_child.kind() {
                    "public" => return Visibility::Public,
                    "protected" => return Visibility::Protected,
                    "private" => return Visibility::Private,
                    _ => {}
                }
            }
        }
    }
    Visibility::Internal
}

// ── Diff logic ──────────────────────────────────────────────────────────

fn diff_functions_in_file(
    parser: &mut Parser,
    old_source: &str,
    new_source: &str,
    file_path: &str,
) -> Result<Vec<ChangedFunction>> {
    let old_funcs = extract_functions(parser, old_source, file_path)?;
    let new_funcs = extract_functions(parser, new_source, file_path)?;

    let old_map: HashMap<&str, &ExtractedFunction> = old_funcs
        .iter()
        .map(|f| (f.qualified_name.as_str(), f))
        .collect();
    let new_map: HashMap<&str, &ExtractedFunction> = new_funcs
        .iter()
        .map(|f| (f.qualified_name.as_str(), f))
        .collect();

    let mut changes = Vec::new();

    for (qname, old_func) in &old_map {
        if let Some(new_func) = new_map.get(qname) {
            let old_norm = normalize_body(&old_func.body);
            let new_norm = normalize_body(&new_func.body);

            if old_norm != new_norm {
                changes.push(ChangedFunction {
                    qualified_name: qname.to_string(),
                    name: new_func.name.clone(),
                    file: PathBuf::from(file_path),
                    line: new_func.line,
                    kind: new_func.kind,
                    visibility: new_func.visibility,
                    old_body: Some(old_func.body.clone()),
                    new_body: Some(new_func.body.clone()),
                    old_signature: Some(old_func.signature.clone()),
                    new_signature: Some(new_func.signature.clone()),
                });
            }
        } else {
            changes.push(ChangedFunction {
                qualified_name: qname.to_string(),
                name: old_func.name.clone(),
                file: PathBuf::from(file_path),
                line: old_func.line,
                kind: old_func.kind,
                visibility: old_func.visibility,
                old_body: Some(old_func.body.clone()),
                new_body: None,
                old_signature: Some(old_func.signature.clone()),
                new_signature: None,
            });
        }
    }

    for (qname, new_func) in &new_map {
        if !old_map.contains_key(qname) {
            changes.push(ChangedFunction {
                qualified_name: qname.to_string(),
                name: new_func.name.clone(),
                file: PathBuf::from(file_path),
                line: new_func.line,
                kind: new_func.kind,
                visibility: new_func.visibility,
                old_body: None,
                new_body: Some(new_func.body.clone()),
                old_signature: None,
                new_signature: Some(new_func.signature.clone()),
            });
        }
    }

    Ok(changes)
}

fn normalize_body(body: &str) -> String {
    let mut result = Vec::new();
    let mut in_block_comment = false;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if in_block_comment {
            if trimmed.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }
        if trimmed.starts_with("/*") {
            if !trimmed.contains("*/") {
                in_block_comment = true;
            }
            continue;
        }
        if trimmed.starts_with("//") {
            continue;
        }
        result.push(trimmed);
    }

    result.join("\n")
}

// ── Tree-sitter helpers ─────────────────────────────────────────────────

fn node_text<'a>(node: Node, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

#[allow(clippy::manual_find)]
fn find_child_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_body() {
        let body = r#"{
            // setup
            int x = 1;

            /* multi-line
             * comment
             */
            return x;
        }"#;
        let normalized = normalize_body(body);
        assert!(!normalized.contains("// setup"));
        assert!(!normalized.contains("multi-line"));
        assert!(normalized.contains("int x = 1;"));
        assert!(normalized.contains("return x;"));
    }

    #[test]
    fn test_extract_functions() {
        let source = r#"
            package com.example;
            public class Foo {
                public void doThing() {
                    System.out.println("hello");
                }
                private int calculate(int x) {
                    return x * 2;
                }
            }
        "#;

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();

        let funcs = extract_functions(&mut parser, source, "Foo.java").unwrap();
        assert_eq!(funcs.len(), 2);
        assert!(funcs.iter().any(|f| f.qualified_name.contains("doThing")));
    }

    #[test]
    fn test_diff_functions_formatting_only() {
        let old = r#"
            package com.example;
            public class Foo {
                public void doThing() {
                    int x = 1;
                    return;
                }
            }
        "#;
        let new = r#"
            package com.example;
            public class Foo {
                public void doThing() {
                    int x = 1;

                    return;
                }
            }
        "#;

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();

        let changes = diff_functions_in_file(&mut parser, old, new, "Foo.java").unwrap();
        assert_eq!(changes.len(), 0);
    }
}
