//! Migration example mining from commented-out code in library test files.
//!
//! When library authors migrate their own test suites from an old API to
//! a new one, they frequently leave the old code as comments adjacent to
//! the replacement code. This module mines those pairs to build method-level
//! old→new API mappings automatically.
//!
//! ## Algorithm
//!
//! 1. Enumerate test files at the to-ref via `git ls-tree`
//! 2. For each file, parse with tree-sitter and collect comment nodes
//! 3. Identify consecutive commented-out lines that look like Java code
//! 4. Extract the adjacent active (non-comment) code in the same scope
//! 5. Parse method-call invocations from both old and new code blocks
//! 6. Resolve class names to FQNs via the file's import declarations
//! 7. Aggregate pairs across all files into statistical mappings

use crate::sd_types::*;
use anyhow::{Context, Result};
use semver_analyzer_core::git::read_git_file;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use tree_sitter::Parser;

/// Maximum number of representative examples to keep per mapping.
const MAX_PATTERN_EXAMPLES: usize = 3;

/// Minimum number of examples required to emit a mapping.
const MIN_EXAMPLES_FOR_MAPPING: usize = 1;

// ── Public entry point ──────────────────────────────────────────────────

/// Mine migration examples from test files at the to-ref.
///
/// `removed_symbols` is an optional set of class names known to be removed
/// (from the TD pipeline). When provided, only comments referencing these
/// symbols are considered. When `None`, all commented-out code blocks are
/// analyzed (self-discovery mode).
pub fn mine_migration_examples(
    repo: &Path,
    to_ref: &str,
    removed_symbols: Option<&HashSet<String>>,
) -> Result<(Vec<MigrationExample>, Vec<MigrationMapping>)> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .context("Failed to set tree-sitter Java language")?;

    let test_files = list_test_files(repo, to_ref)?;

    if test_files.is_empty() {
        tracing::debug!("No test files found at {}", to_ref);
        return Ok((Vec::new(), Vec::new()));
    }

    tracing::info!(
        files = test_files.len(),
        "SD Phase A.5: mining migration examples from test files"
    );

    let mut all_examples: Vec<MigrationExample> = Vec::new();

    for file_path in &test_files {
        let source = match read_git_file(repo, to_ref, file_path) {
            Some(s) => s,
            None => continue,
        };

        // Quick check: does this file contain any line comments?
        // Skip files with no comments to avoid unnecessary parsing.
        if !source.contains("//") {
            continue;
        }

        // If we have a removed-symbol list, skip files that don't reference any.
        if let Some(symbols) = removed_symbols {
            if !symbols.iter().any(|s| source.contains(s.as_str())) {
                continue;
            }
        }

        let examples = extract_examples_from_file(&mut parser, &source, file_path);
        all_examples.extend(examples);
    }

    tracing::info!(
        examples = all_examples.len(),
        "Migration examples extracted from test files"
    );

    let mappings = aggregate_mappings(&all_examples);

    tracing::info!(
        mappings = mappings.len(),
        methods = mappings.iter().map(|m| m.method_mappings.len()).sum::<usize>(),
        "Migration mappings aggregated"
    );

    Ok((all_examples, mappings))
}

// ── Test file discovery ─────────────────────────────────────────────────

fn list_test_files(repo: &Path, git_ref: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", git_ref])
        .current_dir(repo)
        .output()
        .context("Failed to run git ls-tree for test files")?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter(|l| !l.is_empty())
        .filter(|l| l.ends_with(".java"))
        .filter(|l| is_test_file(l))
        .map(|l| l.to_string())
        .collect())
}

fn is_test_file(path: &str) -> bool {
    path.contains("/src/test/")
        || path.ends_with("Test.java")
        || path.ends_with("Tests.java")
        || path.ends_with("IT.java")
        || path.ends_with("ITCase.java")
}

// ── Per-file example extraction ─────────────────────────────────────────

fn extract_examples_from_file(
    parser: &mut Parser,
    source: &str,
    file_path: &str,
) -> Vec<MigrationExample> {
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let root = tree.root_node();

    // Collect imports for FQN resolution
    let imports = extract_import_map(root, source);

    // Collect all line comments with their line numbers and text
    let mut comments: Vec<CommentLine> = Vec::new();
    collect_line_comments(root, source, &mut comments);

    if comments.is_empty() {
        return Vec::new();
    }

    // Sort by line number
    comments.sort_by_key(|c| c.line);

    // Group consecutive commented-out lines into blocks
    let blocks = group_comment_blocks(&comments);

    // For each block, find adjacent active code and extract pairs
    let lines: Vec<&str> = source.lines().collect();
    let mut examples = Vec::new();

    for block in &blocks {
        // Strip comment markers to get the old code
        let old_code = block
            .lines
            .iter()
            .map(|c| c.stripped_text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        // Quick validation: does the old code look like Java statements?
        if !looks_like_java_code(&old_code) {
            continue;
        }

        // Find adjacent active (non-comment) code
        let new_code = extract_adjacent_active_code(&lines, block);
        if new_code.is_empty() {
            continue;
        }

        // Extract method-call pairs
        let old_calls = extract_method_calls_from_text(&old_code);
        let new_calls = extract_method_calls_from_text(&new_code);

        if old_calls.is_empty() || new_calls.is_empty() {
            continue;
        }

        // Build pairs by matching old calls to new calls
        let pairs = build_pairs(&old_calls, &new_calls, &imports);

        if pairs.is_empty() {
            continue;
        }

        examples.push(MigrationExample {
            old_code: old_code.clone(),
            new_code: new_code.clone(),
            pairs,
            file: file_path.to_string(),
        });
    }

    examples
}

// ── Comment collection ──────────────────────────────────────────────────

#[derive(Debug)]
struct CommentLine {
    line: usize,
    /// Comment text with `//` prefix stripped.
    stripped_text: String,
}

fn collect_line_comments(node: tree_sitter::Node, source: &str, out: &mut Vec<CommentLine>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "line_comment" {
            let text = &source[child.byte_range()];
            // Strip the `//` prefix and optional leading space
            let stripped = text
                .strip_prefix("//")
                .unwrap_or(text)
                .strip_prefix(' ')
                .unwrap_or(text.strip_prefix("//").unwrap_or(text));
            let line = child.start_position().row;
            out.push(CommentLine {
                line,
                stripped_text: stripped.to_string(),
            });
        } else {
            // Recurse into non-comment children (comments are extras
            // and appear at every level of the tree)
            collect_line_comments(child, source, out);
        }
    }
}

// ── Comment block grouping ──────────────────────────────────────────────

struct CommentBlock {
    lines: Vec<CommentLine>,
    start_line: usize,
    end_line: usize,
}

fn group_comment_blocks(comments: &[CommentLine]) -> Vec<CommentBlock> {
    let mut blocks = Vec::new();
    let mut i = 0;

    while i < comments.len() {
        let start = i;
        let start_line = comments[i].line;

        // Collect consecutive comments (allowing a gap of at most 1 blank line)
        while i + 1 < comments.len() && comments[i + 1].line <= comments[i].line + 2 {
            i += 1;
        }

        let end_line = comments[i].line;
        let block_comments: Vec<CommentLine> = comments[start..=i]
            .iter()
            .map(|c| CommentLine {
                line: c.line,
                stripped_text: c.stripped_text.clone(),
            })
            .collect();

        // Only consider blocks with at least 1 line that looks like code
        if block_comments
            .iter()
            .any(|c| looks_like_java_code(&c.stripped_text))
        {
            blocks.push(CommentBlock {
                lines: block_comments,
                start_line,
                end_line,
            });
        }

        i += 1;
    }

    blocks
}

// ── Code detection heuristics ───────────────────────────────────────────

/// Check if text looks like Java code rather than a prose comment.
fn looks_like_java_code(text: &str) -> bool {
    let trimmed = text.trim();

    // Skip empty or very short text
    if trimmed.len() < 3 {
        return false;
    }

    // Strong indicators of Java code
    let code_patterns = [
        // Method calls
        ".add(",
        ".get(",
        ".set(",
        ".list()",
        ".list();",
        ".uniqueResult()",
        "createCriteria(",
        "getCriteriaBuilder(",
        "createQuery(",
        // Assignments
        " = new ",
        " = session.",
        " = (", // cast
        // Common API patterns
        "Restrictions.",
        "Projections.",
        "DetachedCriteria.",
        "criteriaBuilder.",
        "CriteriaBuilder ",
        "CriteriaQuery<",
        "Root<",
        // General Java patterns
        "return ",
        ".forClass(",
        "session.",
        // Statement terminators
    ];

    for pat in &code_patterns {
        if trimmed.contains(pat) {
            return true;
        }
    }

    // Generic code indicators: ends with ; or ) or has method chaining
    if trimmed.ends_with(';')
        || trimmed.ends_with(')')
        || trimmed.ends_with("*/")
        || trimmed.contains(".(")
        || (trimmed.starts_with('.') && trimmed.contains('('))
    {
        return true;
    }

    false
}

// ── Adjacent active code extraction ─────────────────────────────────────

fn extract_adjacent_active_code(lines: &[&str], block: &CommentBlock) -> String {
    let mut active_lines = Vec::new();

    // Look ABOVE the comment block for active code (the more common pattern:
    // new code appears first, then the commented-out old code below)
    let search_start = block.start_line.saturating_sub(1);
    let mut above_lines = Vec::new();

    for i in (0..=search_start).rev() {
        if i >= lines.len() {
            continue;
        }
        let line = lines[i].trim();
        if line.is_empty() {
            // Allow one blank line, then stop
            if !above_lines.is_empty() {
                break;
            }
            continue;
        }
        if line.starts_with("//") {
            // Hit another comment — stop
            break;
        }
        above_lines.push(lines[i]);
        // Collect up to 10 lines of context above
        if above_lines.len() >= 10 {
            break;
        }
    }
    above_lines.reverse();

    // Look BELOW the comment block for active code
    let search_end = block.end_line + 1;
    let mut below_lines = Vec::new();

    for line_ref in lines.iter().skip(search_end).take(10) {
        let line = line_ref.trim();
        if line.is_empty() {
            if !below_lines.is_empty() {
                break;
            }
            continue;
        }
        if line.starts_with("//") {
            break;
        }
        below_lines.push(*line_ref);
        if below_lines.len() >= 10 {
            break;
        }
    }

    // Use whichever side has more Java code content
    let above_score: usize = above_lines
        .iter()
        .filter(|l| looks_like_java_code(l))
        .count();
    let below_score: usize = below_lines
        .iter()
        .filter(|l| looks_like_java_code(l))
        .count();

    if above_score >= below_score && above_score > 0 {
        active_lines.extend(above_lines);
    } else if below_score > 0 {
        active_lines.extend(below_lines);
    }

    active_lines.join("\n")
}

// ── Method-call extraction ──────────────────────────────────────────────

#[derive(Debug, Clone)]
struct MethodCall {
    /// The receiver.method pattern (e.g., `"Restrictions.eq"`, `"session.createCriteria"`)
    receiver_method: String,
    /// Just the class/receiver name (e.g., `"Restrictions"`, `"session"`)
    receiver: String,
    /// Just the method name (e.g., `"eq"`, `"createCriteria"`)
    method: String,
}

/// Extract method calls from a code fragment using regex-like pattern matching.
///
/// We use simple text patterns rather than tree-sitter parsing of the fragment
/// because commented-out code may be partial and not parse as a complete
/// compilation unit.
fn extract_method_calls_from_text(code: &str) -> Vec<MethodCall> {
    let mut calls = Vec::new();
    let mut seen = HashSet::new();

    // Match patterns like `Foo.bar(` or `foo.bar(`
    // Also match chained calls like `.add(Restrictions.eq(`
    for line in code.lines() {
        let trimmed = line.trim();
        extract_calls_from_line(trimmed, &mut calls, &mut seen);
    }

    calls
}

fn extract_calls_from_line(line: &str, calls: &mut Vec<MethodCall>, seen: &mut HashSet<String>) {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Look for `identifier.identifier(` pattern
        if is_java_ident_char(bytes[i]) {
            let start = i;
            while i < len && is_java_ident_char(bytes[i]) {
                i += 1;
            }
            let receiver = &line[start..i];

            // Check for `.method(`
            if i < len && bytes[i] == b'.' {
                i += 1;
                let method_start = i;
                while i < len && is_java_ident_char(bytes[i]) {
                    i += 1;
                }
                if i > method_start && i < len && bytes[i] == b'(' {
                    let method = &line[method_start..i];
                    let key = format!("{}.{}", receiver, method);
                    if !seen.contains(&key) {
                        seen.insert(key.clone());
                        calls.push(MethodCall {
                            receiver_method: key,
                            receiver: receiver.to_string(),
                            method: method.to_string(),
                        });
                    }
                }
            }
        } else {
            i += 1;
        }
    }
}

fn is_java_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// ── Import resolution ───────────────────────────────────────────────────

/// Build a map from simple class name → fully qualified name using imports.
fn extract_import_map(root: tree_sitter::Node, source: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "import_declaration" {
            let text = &source[child.byte_range()];
            let fqn = text
                .trim_start_matches("import ")
                .trim_start_matches("static ")
                .trim_end_matches(';')
                .trim();

            // Skip wildcard imports
            if fqn.ends_with(".*") {
                // For wildcard imports, store the package prefix
                // so we can do partial resolution
                continue;
            }

            // Extract simple name from FQN
            if let Some(simple) = fqn.rsplit('.').next() {
                map.insert(simple.to_string(), fqn.to_string());
            }
        }
    }
    map
}

// ── Pair building ───────────────────────────────────────────────────────

fn build_pairs(
    old_calls: &[MethodCall],
    new_calls: &[MethodCall],
    imports: &HashMap<String, String>,
) -> Vec<MigrationExamplePair> {
    let mut pairs = Vec::new();

    // For each old call, try to find a semantically related new call.
    // Strategy: match by receiver class being different but method being
    // the same or similar, OR by the calls being in corresponding positions.
    for old in old_calls {
        // Skip trivial calls (getters, common utilities)
        if is_trivial_call(&old.method) {
            continue;
        }

        // Try to find a matching new call:
        // 1. Same method name but different receiver → likely a migration
        // 2. Different method name but same semantic role → fuzzy match
        let mut best_match: Option<&MethodCall> = None;

        for new in new_calls {
            if is_trivial_call(&new.method) {
                continue;
            }

            // Skip if both calls are on the same class (not a migration)
            if old.receiver == new.receiver {
                continue;
            }

            // Exact method name match (e.g., old: `Order.asc` → new: `cb.asc`)
            if old.method == new.method {
                best_match = Some(new);
                break;
            }

            // Known abbreviation expansions
            if is_abbreviation_match(&old.method, &new.method) {
                best_match = Some(new);
                break;
            }
        }

        if let Some(matched) = best_match {
            let old_fqn = imports
                .get(&old.receiver)
                .map(|fqn| format!("{}.{}", fqn, old.method));
            let new_fqn = imports
                .get(&matched.receiver)
                .map(|fqn| format!("{}.{}", fqn, matched.method));

            pairs.push(MigrationExamplePair {
                old_call: old.receiver_method.clone(),
                new_call: matched.receiver_method.clone(),
                old_fqn,
                new_fqn,
            });
        }
    }

    pairs
}

fn is_trivial_call(method: &str) -> bool {
    matches!(
        method,
        "get"
            | "set"
            | "put"
            | "add"
            | "remove"
            | "size"
            | "isEmpty"
            | "toString"
            | "equals"
            | "hashCode"
            | "valueOf"
            | "println"
            | "print"
            | "close"
            | "flush"
            | "getClass"
            | "iterator"
            | "next"
            | "hasNext"
    )
}

fn is_abbreviation_match(old: &str, new: &str) -> bool {
    // Common Java API abbreviation patterns
    let abbreviations: &[(&str, &str)] = &[
        ("eq", "equal"),
        ("ne", "notEqual"),
        ("gt", "greaterThan"),
        ("ge", "greaterThanOrEqualTo"),
        ("lt", "lessThan"),
        ("le", "lessThanOrEqualTo"),
        ("idEq", "equal"),
        ("uniqueResult", "getSingleResult"),
    ];

    for &(short, long) in abbreviations {
        if old == short && new == long {
            return true;
        }
    }

    false
}

// ── Aggregation ─────────────────────────────────────────────────────────

fn aggregate_mappings(examples: &[MigrationExample]) -> Vec<MigrationMapping> {
    // Group by (old_class, new_class) pair
    // Key: (old_receiver, new_receiver)
    // Value: list of method pairs and example refs
    let mut class_pairs: HashMap<(String, String), Vec<(&MigrationExample, &MigrationExamplePair)>> =
        HashMap::new();

    for example in examples {
        for pair in &example.pairs {
            // Extract the class portion from the call
            let old_class = pair.old_call.split('.').next().unwrap_or("").to_string();
            let new_class = pair.new_call.split('.').next().unwrap_or("").to_string();

            if old_class.is_empty() || new_class.is_empty() {
                continue;
            }

            class_pairs
                .entry((old_class, new_class))
                .or_default()
                .push((example, pair));
        }
    }

    let mut mappings = Vec::new();

    for ((old_class, new_class), pair_list) in &class_pairs {
        if pair_list.len() < MIN_EXAMPLES_FOR_MAPPING {
            continue;
        }

        // Resolve FQNs: pick the most common FQN across examples
        let old_fqn = most_common_fqn(pair_list.iter().filter_map(|(_, p)| p.old_fqn.as_deref()));
        let new_fqn = most_common_fqn(pair_list.iter().filter_map(|(_, p)| p.new_fqn.as_deref()));

        // Strip the method part from the FQN to get class-level FQN
        let old_class_fqn = old_fqn
            .as_ref()
            .and_then(|fqn| fqn.rsplit_once('.').map(|(pkg, _)| pkg.to_string()))
            .unwrap_or_else(|| old_class.clone());
        let new_class_fqn = new_fqn
            .as_ref()
            .and_then(|fqn| fqn.rsplit_once('.').map(|(pkg, _)| pkg.to_string()))
            .unwrap_or_else(|| new_class.clone());

        // Count method-name mappings
        let mut method_counts: HashMap<(String, String), usize> = HashMap::new();
        for (_, pair) in pair_list {
            let old_method = pair
                .old_call
                .split('.')
                .nth(1)
                .unwrap_or("")
                .to_string();
            let new_method = pair
                .new_call
                .split('.')
                .nth(1)
                .unwrap_or("")
                .to_string();
            if !old_method.is_empty() && !new_method.is_empty() {
                *method_counts
                    .entry((old_method, new_method))
                    .or_default() += 1;
            }
        }

        let mut method_mappings: Vec<MethodMapping> = method_counts
            .into_iter()
            .map(|((old_m, new_m), count)| MethodMapping {
                old_method: old_m,
                new_method: new_m,
                confidence: count,
            })
            .collect();
        method_mappings.sort_by_key(|m| std::cmp::Reverse(m.confidence));

        // Collect unique representative examples
        let mut pattern_examples = Vec::new();
        let mut seen_files = HashSet::new();
        for (ex, _) in pair_list {
            if seen_files.contains(&ex.file) {
                continue;
            }
            seen_files.insert(ex.file.clone());
            pattern_examples.push((*ex).clone());
            if pattern_examples.len() >= MAX_PATTERN_EXAMPLES {
                break;
            }
        }

        mappings.push(MigrationMapping {
            old_class: old_class.clone(),
            old_fqn: old_class_fqn,
            new_class: new_class.clone(),
            new_fqn: new_class_fqn,
            method_mappings,
            example_count: pair_list.len(),
            pattern_examples,
        });
    }

    // Sort by example count (most evidence first)
    mappings.sort_by_key(|m| std::cmp::Reverse(m.example_count));

    mappings
}

fn most_common_fqn<'a>(fqns: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for fqn in fqns {
        *counts.entry(fqn).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(fqn, _)| fqn.to_string())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_looks_like_java_code() {
        assert!(looks_like_java_code("session.createCriteria(Foo.class);"));
        assert!(looks_like_java_code(
            "Restrictions.eq(\"name\", \"value\")"
        ));
        assert!(looks_like_java_code(".add(Restrictions.eq(\"x\", 1))"));
        assert!(looks_like_java_code("List result = session.createCriteria(Foo.class)"));
        assert!(looks_like_java_code("CriteriaBuilder cb = session.getCriteriaBuilder();"));

        // Prose comments
        assert!(!looks_like_java_code(
            "This test verifies that criteria work"
        ));
        assert!(!looks_like_java_code("TODO: fix this later"));
        assert!(!looks_like_java_code("@author John"));
    }

    #[test]
    fn test_extract_method_calls_from_text() {
        let code = r#"
            session.createCriteria(Foo.class)
                .add(Restrictions.eq("name", "bar"))
                .setProjection(Projections.rowCount())
                .list();
        "#;

        let calls = extract_method_calls_from_text(code);
        let call_strs: Vec<&str> = calls.iter().map(|c| c.receiver_method.as_str()).collect();

        assert!(call_strs.contains(&"session.createCriteria"));
        assert!(call_strs.contains(&"Restrictions.eq"));
        assert!(call_strs.contains(&"Projections.rowCount"));
    }

    #[test]
    fn test_extract_method_calls_chained() {
        let code = "criteriaBuilder.equal(root.get(\"name\"), \"Fnac\")";
        let calls = extract_method_calls_from_text(code);
        let call_strs: Vec<&str> = calls.iter().map(|c| c.receiver_method.as_str()).collect();

        assert!(call_strs.contains(&"criteriaBuilder.equal"));
        assert!(call_strs.contains(&"root.get"));
    }

    #[test]
    fn test_abbreviation_match() {
        assert!(is_abbreviation_match("eq", "equal"));
        assert!(is_abbreviation_match("ne", "notEqual"));
        assert!(is_abbreviation_match("gt", "greaterThan"));
        assert!(!is_abbreviation_match("like", "like"));
        assert!(!is_abbreviation_match("foo", "bar"));
    }

    #[test]
    fn test_build_pairs() {
        let old_calls = vec![
            MethodCall {
                receiver_method: "Restrictions.eq".into(),
                receiver: "Restrictions".into(),
                method: "eq".into(),
            },
            MethodCall {
                receiver_method: "Order.asc".into(),
                receiver: "Order".into(),
                method: "asc".into(),
            },
        ];
        let new_calls = vec![
            MethodCall {
                receiver_method: "cb.equal".into(),
                receiver: "cb".into(),
                method: "equal".into(),
            },
            MethodCall {
                receiver_method: "cb.asc".into(),
                receiver: "cb".into(),
                method: "asc".into(),
            },
        ];

        let imports = HashMap::new();
        let pairs = build_pairs(&old_calls, &new_calls, &imports);

        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].old_call, "Restrictions.eq");
        assert_eq!(pairs[0].new_call, "cb.equal");
        assert_eq!(pairs[1].old_call, "Order.asc");
        assert_eq!(pairs[1].new_call, "cb.asc");
    }

    #[test]
    fn test_group_comment_blocks() {
        let comments = vec![
            CommentLine {
                line: 10,
                stripped_text: "session.createCriteria(Foo.class)".into(),
            },
            CommentLine {
                line: 11,
                stripped_text: "    .add(Restrictions.eq(\"x\", 1))".into(),
            },
            CommentLine {
                line: 12,
                stripped_text: "    .list();".into(),
            },
            // Gap
            CommentLine {
                line: 20,
                stripped_text: "This is a prose comment".into(),
            },
            // Another code block
            CommentLine {
                line: 30,
                stripped_text: "Projections.rowCount()".into(),
            },
        ];

        let blocks = group_comment_blocks(&comments);
        // Should get 2 blocks: lines 10-12 (code) and line 30 (code)
        // Line 20 is prose and shouldn't form a code block
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].lines.len(), 3);
        assert_eq!(blocks[1].lines.len(), 1);
    }

    #[test]
    fn test_aggregate_mappings_basic() {
        let examples = vec![
            MigrationExample {
                old_code: "Restrictions.eq(\"name\", val)".into(),
                new_code: "cb.equal(root.get(\"name\"), val)".into(),
                pairs: vec![MigrationExamplePair {
                    old_call: "Restrictions.eq".into(),
                    new_call: "cb.equal".into(),
                    old_fqn: Some("org.hibernate.criterion.Restrictions.eq".into()),
                    new_fqn: None,
                }],
                file: "Test1.java".into(),
            },
            MigrationExample {
                old_code: "Restrictions.eq(\"id\", id)".into(),
                new_code: "cb.equal(root.get(\"id\"), id)".into(),
                pairs: vec![MigrationExamplePair {
                    old_call: "Restrictions.eq".into(),
                    new_call: "cb.equal".into(),
                    old_fqn: Some("org.hibernate.criterion.Restrictions.eq".into()),
                    new_fqn: None,
                }],
                file: "Test2.java".into(),
            },
        ];

        let mappings = aggregate_mappings(&examples);
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].old_class, "Restrictions");
        assert_eq!(mappings[0].new_class, "cb");
        assert_eq!(mappings[0].example_count, 2);
        assert_eq!(mappings[0].method_mappings.len(), 1);
        assert_eq!(mappings[0].method_mappings[0].old_method, "eq");
        assert_eq!(mappings[0].method_mappings[0].new_method, "equal");
        assert_eq!(mappings[0].method_mappings[0].confidence, 2);
    }

    #[test]
    fn test_is_trivial_call() {
        assert!(is_trivial_call("get"));
        assert!(is_trivial_call("set"));
        assert!(is_trivial_call("add"));
        assert!(!is_trivial_call("createCriteria"));
        assert!(!is_trivial_call("equal"));
        assert!(!is_trivial_call("eq"));
    }
}
