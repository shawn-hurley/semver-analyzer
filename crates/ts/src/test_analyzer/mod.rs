//! TypeScript TestAnalyzer implementation.
//!
//! Finds test files associated with source files and analyzes their diffs
//! for assertion changes. This is "Option B" from the PLAN.md: text-based
//! assertion detection using regex patterns, no framework-specific AST parsing.
//!
//! ## Test Discovery
//!
//! Given `src/api/users.ts`, looks for:
//! - `src/api/users.test.ts` / `.test.tsx`
//! - `src/api/users.spec.ts` / `.spec.tsx`
//! - `src/api/__tests__/users.ts` / `.test.ts` / `.spec.ts`
//! - `src/__tests__/api/users.ts`
//!
//! ## Assertion Detection
//!
//! Matches common assertion patterns across testing frameworks:
//! - Jest/Vitest: `expect(...)`, `.toBe(...)`, `.toEqual(...)`, `.toThrow(...)`
//! - Mocha/Chai: `assert.equal(...)`, `should.equal(...)`, `expect(...).to.`
//! - Node assert: `assert(...)`, `assert.strictEqual(...)`
//! - Testing Library: `screen.getByText(...)`, `waitFor(...)`, `fireEvent`

use anyhow::{Context, Result};
use regex::Regex;
use semver_analyzer_core::{TestConvention, TestDiff, TestFile};
use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;

/// TypeScript/JavaScript test analyzer.
///
/// Discovers test files by naming convention and analyzes git diffs
/// for assertion pattern changes.
pub struct TsTestAnalyzer;

impl TsTestAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl TsTestAnalyzer {
    pub fn find_tests(&self, repo: &Path, source_file: &Path) -> Result<Vec<TestFile>> {
        find_test_files(repo, source_file)
    }

    pub fn diff_test_assertions(
        &self,
        repo: &Path,
        test_file: &TestFile,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<TestDiff> {
        diff_test_file(repo, test_file, from_ref, to_ref)
    }
}

// ── Test File Discovery ─────────────────────────────────────────────────

/// Source file extensions we strip when searching for test files.
const SOURCE_EXTENSIONS: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mts", ".mjs"];

/// Test file extensions to try (ordered by priority).
const TEST_EXTENSIONS: &[&str] = &[
    ".test.ts",
    ".test.tsx",
    ".spec.ts",
    ".spec.tsx",
    ".test.js",
    ".test.jsx",
    ".spec.js",
    ".spec.jsx",
];

/// Find test files associated with a source file.
///
/// Checks multiple naming conventions:
/// 1. Sibling: `foo.test.ts` next to `foo.ts`
/// 2. Sibling spec: `foo.spec.ts` next to `foo.ts`
/// 3. `__tests__` directory (same level): `__tests__/foo.test.ts`
/// 4. `__tests__` subdirectories: `__tests__/Generated/foo.test.ts`
/// 5. `__tests__` directory (parent level): `../__tests__/dir/foo.test.ts`
/// 6. Component-level tests: if source is `FooBar.tsx`, also finds `Foo.test.tsx`
///    (parent component test that likely covers sub-components)
/// 7. Directory-level tests: all test files in the same `__tests__/` directory
///    (e.g., `Slider.test.tsx` likely covers both `Slider.tsx` and `SliderStep.tsx`)
fn find_test_files(repo: &Path, source_file: &Path) -> Result<Vec<TestFile>> {
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Get the stem (filename without extension)
    let file_name = source_file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    let stem = strip_source_extension(file_name);
    let parent = source_file.parent().unwrap_or(Path::new(""));

    let mut add = |path: std::path::PathBuf, convention: TestConvention| {
        if seen.insert(path.clone()) {
            found.push(TestFile { path, convention });
        }
    };

    // Strategy 1 & 2: Sibling .test.* and .spec.* files
    for ext in TEST_EXTENSIONS {
        let test_path = parent.join(format!("{}{}", stem, ext));
        let full_path = repo.join(&test_path);

        if full_path.exists() {
            let convention = if ext.contains(".test.") {
                TestConvention::DotTest
            } else {
                TestConvention::DotSpec
            };
            add(test_path, convention);
        }
    }

    // Strategy 3: __tests__ directory at the same level (exact name match)
    let tests_dir = parent.join("__tests__");
    if repo.join(&tests_dir).is_dir() {
        // Try: __tests__/foo.ts, __tests__/foo.test.ts, etc.
        for base_ext in &[".ts", ".tsx"] {
            let plain = tests_dir.join(format!("{}{}", stem, base_ext));
            if repo.join(&plain).exists() {
                add(plain, TestConvention::TestsDir);
            }
        }
        for ext in TEST_EXTENSIONS {
            let test_path = tests_dir.join(format!("{}{}", stem, ext));
            if repo.join(&test_path).exists() {
                add(test_path, TestConvention::TestsDir);
            }
        }

        // Strategy 4: __tests__ subdirectories (e.g., __tests__/Generated/Foo.test.tsx)
        if let Ok(entries) = std::fs::read_dir(repo.join(&tests_dir)) {
            for entry in entries.flatten() {
                let entry_path = entry.path();
                if entry_path.is_dir() {
                    let subdir_name = entry.file_name().to_string_lossy().to_string();
                    // Skip snapshot directories
                    if subdir_name == "__snapshots__" {
                        continue;
                    }
                    for ext in TEST_EXTENSIONS {
                        let test_path = tests_dir
                            .join(&subdir_name)
                            .join(format!("{}{}", stem, ext));
                        if repo.join(&test_path).exists() {
                            add(test_path, TestConvention::TestsDir);
                        }
                    }
                }
            }
        }

        // Strategy 7: Directory-level — all test files in the same __tests__/
        // If `SliderStep.tsx` is in the Slider directory, `__tests__/Slider.test.tsx`
        // likely covers it too.
        collect_test_files_in_dir(repo, &tests_dir, &mut |path| {
            add(path, TestConvention::TestsDir);
        });
    }

    // Strategy 5: __tests__ directory at parent level
    // e.g., src/api/users.ts -> src/__tests__/api/users.test.ts
    if let Some(grandparent) = parent.parent() {
        let dir_name = parent.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let parent_tests_dir = grandparent.join("__tests__").join(dir_name);

        if repo.join(&parent_tests_dir).is_dir() {
            for ext in TEST_EXTENSIONS {
                let test_path = parent_tests_dir.join(format!("{}{}", stem, ext));
                if repo.join(&test_path).exists() {
                    add(test_path, TestConvention::TestsDir);
                }
            }
        }
    }

    // Strategy 6: Component-level test (parent component name).
    // For `FooBar.tsx`, check if there's a `Foo.test.tsx` sibling or in __tests__.
    // This covers sub-components tested via the parent's test file.
    if let Some(parent_name) = infer_parent_component_name(stem) {
        for ext in TEST_EXTENSIONS {
            // Sibling
            let test_path = parent.join(format!("{}{}", parent_name, ext));
            if repo.join(&test_path).exists() {
                let convention = if ext.contains(".test.") {
                    TestConvention::DotTest
                } else {
                    TestConvention::DotSpec
                };
                add(test_path, convention);
            }

            // __tests__ dir
            let tests_dir = parent.join("__tests__");
            let test_path = tests_dir.join(format!("{}{}", parent_name, ext));
            if repo.join(&test_path).exists() {
                add(test_path, TestConvention::TestsDir);
            }
        }
    }

    Ok(found)
}

/// Collect all test files (recursively) in a `__tests__/` directory,
/// skipping `__snapshots__/` subdirectories.
fn collect_test_files_in_dir(repo: &Path, dir: &Path, add: &mut dyn FnMut(std::path::PathBuf)) {
    let full_dir = repo.join(dir);
    let entries = match std::fs::read_dir(&full_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let entry_path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if entry_path.is_dir() {
            if name == "__snapshots__" {
                continue;
            }
            collect_test_files_in_dir(repo, &dir.join(&name), add);
        } else if is_test_file(&name) {
            add(dir.join(&name));
        }
    }
}

/// Check if a filename is a test file.
fn is_test_file(filename: &str) -> bool {
    TEST_EXTENSIONS.iter().any(|ext| filename.ends_with(ext))
        || (filename.ends_with(".ts") || filename.ends_with(".tsx")) && !filename.ends_with(".d.ts")
}

/// Infer a parent component name from a sub-component name.
///
/// `SliderStep` → `Slider`  (strip common suffixes)
/// `CardHeader` → `Card`
/// `ModalBoxTitle` → `ModalBox`, `Modal`
/// `PopoverHeaderIcon` → `PopoverHeader`, `Popover`
///
/// Returns the longest prefix that could be a parent component, or None.
fn infer_parent_component_name(stem: &str) -> Option<String> {
    // Common sub-component suffixes to try stripping
    const SUFFIXES: &[&str] = &[
        "Step",
        "Item",
        "Header",
        "Footer",
        "Body",
        "Content",
        "Title",
        "Icon",
        "Action",
        "Toggle",
        "Button",
        "Close",
        "Group",
        "List",
        "Container",
        "Section",
        "Panel",
        "Brand",
        "Nav",
        "Box",
        "CloseButton",
        "BoxTitle",
        "BoxCloseButton",
        "HeaderIcon",
        "ExpandableContent",
        "ToggleGroup",
    ];

    // Sort by length descending so we try longer suffixes first
    let mut suffixes: Vec<&&str> = SUFFIXES.iter().collect();
    suffixes.sort_by(|a, b| b.len().cmp(&a.len()));

    for suffix in suffixes {
        if let Some(prefix) = stem.strip_suffix(suffix) {
            // Must leave at least 2 chars and start with uppercase
            if prefix.len() >= 2 && prefix.chars().next().map_or(false, |c| c.is_uppercase()) {
                return Some(prefix.to_string());
            }
        }
    }
    None
}

/// Strip the source file extension from a filename.
/// e.g., "users.ts" -> "users", "Button.tsx" -> "Button"
fn strip_source_extension(filename: &str) -> &str {
    for ext in SOURCE_EXTENSIONS {
        if let Some(stem) = filename.strip_suffix(ext) {
            return stem;
        }
    }
    // Fallback: strip last extension
    filename
        .rfind('.')
        .map(|i| &filename[..i])
        .unwrap_or(filename)
}

// ── Assertion Diff Analysis ─────────────────────────────────────────────

/// Assertion patterns for common JS/TS testing frameworks.
///
/// These regex patterns match lines that likely contain test assertions.
/// Ordered roughly by prevalence in modern JS/TS projects.
static ASSERTION_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        // Jest / Vitest / Testing Library
        Regex::new(r"expect\s*\(").unwrap(),
        Regex::new(r"\.toBe\s*\(").unwrap(),
        Regex::new(r"\.toEqual\s*\(").unwrap(),
        Regex::new(r"\.toStrictEqual\s*\(").unwrap(),
        Regex::new(r"\.toContain\s*\(").unwrap(),
        Regex::new(r"\.toMatch\s*\(").unwrap(),
        Regex::new(r"\.toThrow\s*\(").unwrap(),
        Regex::new(r"\.toThrowError\s*\(").unwrap(),
        Regex::new(r"\.toHaveBeenCalled").unwrap(),
        Regex::new(r"\.toHaveBeenCalledWith\s*\(").unwrap(),
        Regex::new(r"\.toHaveBeenCalledTimes\s*\(").unwrap(),
        Regex::new(r"\.toHaveLength\s*\(").unwrap(),
        Regex::new(r"\.toHaveProperty\s*\(").unwrap(),
        Regex::new(r"\.toBeNull\b").unwrap(),
        Regex::new(r"\.toBeUndefined\b").unwrap(),
        Regex::new(r"\.toBeDefined\b").unwrap(),
        Regex::new(r"\.toBeTruthy\b").unwrap(),
        Regex::new(r"\.toBeFalsy\b").unwrap(),
        Regex::new(r"\.toBeGreaterThan\s*\(").unwrap(),
        Regex::new(r"\.toBeLessThan\s*\(").unwrap(),
        Regex::new(r"\.toBeInstanceOf\s*\(").unwrap(),
        Regex::new(r"\.toHaveClass\s*\(").unwrap(),
        Regex::new(r"\.toHaveTextContent\s*\(").unwrap(),
        Regex::new(r"\.toHaveAttribute\s*\(").unwrap(),
        Regex::new(r"\.toBeInTheDocument\b").unwrap(),
        Regex::new(r"\.toBeVisible\b").unwrap(),
        Regex::new(r"\.resolves\.").unwrap(),
        Regex::new(r"\.rejects\.").unwrap(),
        Regex::new(r"\.not\.").unwrap(),
        // Node assert module
        Regex::new(r"assert\s*\(").unwrap(),
        Regex::new(r"assert\.\w+\s*\(").unwrap(),
        // Chai
        Regex::new(r"\.should\.").unwrap(),
        Regex::new(r"\.to\.be\.").unwrap(),
        Regex::new(r"\.to\.equal\s*\(").unwrap(),
        Regex::new(r"\.to\.have\.").unwrap(),
        Regex::new(r"\.to\.include\s*\(").unwrap(),
        Regex::new(r"\.to\.throw\s*\(").unwrap(),
        Regex::new(r"\.to\.deep\.equal\s*\(").unwrap(),
    ]
});

/// Check if a line contains an assertion pattern.
fn is_assertion_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('*') {
        return false;
    }
    ASSERTION_PATTERNS.iter().any(|pat| pat.is_match(trimmed))
}

/// Diff a test file between two git refs and analyze assertion changes.
fn diff_test_file(
    repo: &Path,
    test_file: &TestFile,
    from_ref: &str,
    to_ref: &str,
) -> Result<TestDiff> {
    // Get the unified diff
    let full_diff = git_diff_file(repo, from_ref, to_ref, &test_file.path)?;

    // Parse diff hunks for added/removed assertion lines
    let mut removed_assertions = Vec::new();
    let mut added_assertions = Vec::new();

    for line in full_diff.lines() {
        if line.starts_with('-') && !line.starts_with("---") {
            let content = &line[1..]; // Strip the '-' prefix
            if is_assertion_line(content) {
                removed_assertions.push(content.trim().to_string());
            }
        } else if line.starts_with('+') && !line.starts_with("+++") {
            let content = &line[1..]; // Strip the '+' prefix
            if is_assertion_line(content) {
                added_assertions.push(content.trim().to_string());
            }
        }
    }

    let has_assertion_changes = !removed_assertions.is_empty() || !added_assertions.is_empty();

    Ok(TestDiff {
        test_file: test_file.path.clone(),
        removed_assertions,
        added_assertions,
        has_assertion_changes,
        full_diff,
    })
}

/// Get unified diff for a single file between two refs.
fn git_diff_file(repo: &Path, from_ref: &str, to_ref: &str, file_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args([
            "diff",
            &format!("{}..{}", from_ref, to_ref),
            "--",
            &file_path.to_string_lossy(),
        ])
        .current_dir(repo)
        .output()
        .with_context(|| format!("Failed to run git diff for {}", file_path.display()))?;

    if !output.status.success() {
        // git diff returns 0 even if no changes, non-zero only on error
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed for {}: {}", file_path.display(), stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── strip_source_extension tests ────────────────────────────────

    #[test]
    fn strip_ts_extension() {
        assert_eq!(strip_source_extension("users.ts"), "users");
        assert_eq!(strip_source_extension("Button.tsx"), "Button");
        assert_eq!(strip_source_extension("utils.js"), "utils");
        assert_eq!(strip_source_extension("app.jsx"), "app");
        assert_eq!(strip_source_extension("lib.mts"), "lib");
    }

    #[test]
    fn strip_unknown_extension() {
        assert_eq!(strip_source_extension("data.json"), "data");
        assert_eq!(strip_source_extension("noext"), "noext");
    }

    // ── is_assertion_line tests ─────────────────────────────────────

    #[test]
    fn detects_jest_expect() {
        assert!(is_assertion_line("  expect(result).toBe(5);"));
        assert!(is_assertion_line("expect(fn).toThrow();"));
        assert!(is_assertion_line("  expect(list).toHaveLength(3);"));
        assert!(is_assertion_line("expect(obj).toEqual({ a: 1 });"));
        assert!(is_assertion_line("expect(cb).toHaveBeenCalledWith('foo');"));
        assert!(is_assertion_line("expect(cb).toHaveBeenCalledTimes(2);"));
    }

    #[test]
    fn detects_jest_negated() {
        assert!(is_assertion_line("expect(result).not.toBe(null);"));
        assert!(is_assertion_line("expect(el).not.toBeInTheDocument();"));
    }

    #[test]
    fn detects_testing_library() {
        assert!(is_assertion_line("expect(el).toBeInTheDocument();"));
        assert!(is_assertion_line("expect(el).toHaveTextContent('hello');"));
        assert!(is_assertion_line("expect(el).toHaveClass('active');"));
        assert!(is_assertion_line("expect(el).toBeVisible();"));
        assert!(is_assertion_line(
            "expect(el).toHaveAttribute('role', 'button');"
        ));
    }

    #[test]
    fn detects_chai_assertions() {
        assert!(is_assertion_line("result.should.equal(5);"));
        assert!(is_assertion_line("expect(x).to.be.true;"));
        assert!(is_assertion_line("expect(x).to.equal(5);"));
        assert!(is_assertion_line("expect(x).to.have.property('name');"));
        assert!(is_assertion_line("expect(x).to.include('foo');"));
        assert!(is_assertion_line("expect(fn).to.throw(Error);"));
        assert!(is_assertion_line("expect(x).to.deep.equal({ a: 1 });"));
    }

    #[test]
    fn detects_node_assert() {
        assert!(is_assertion_line("assert(result === true);"));
        assert!(is_assertion_line("assert.equal(a, b);"));
        assert!(is_assertion_line("assert.strictEqual(a, b);"));
        assert!(is_assertion_line("assert.deepEqual(a, b);"));
    }

    #[test]
    fn detects_async_assertions() {
        assert!(is_assertion_line("await expect(promise).resolves.toBe(5);"));
        assert!(is_assertion_line(
            "await expect(promise).rejects.toThrow();"
        ));
    }

    #[test]
    fn rejects_non_assertions() {
        assert!(!is_assertion_line("const result = calculate();"));
        assert!(!is_assertion_line("// expect(result).toBe(5);"));
        assert!(!is_assertion_line("  * expect(result).toBe(5);"));
        assert!(!is_assertion_line(""));
        assert!(!is_assertion_line("  "));
        assert!(!is_assertion_line("import { expect } from 'vitest';"));
        assert!(!is_assertion_line("describe('test', () => {"));
        assert!(!is_assertion_line("it('should work', () => {"));
    }

    // ── Diff parsing for assertions ─────────────────────────────────

    #[test]
    fn parse_diff_finds_changed_assertions() {
        let diff = r#"diff --git a/test.ts b/test.ts
index abc..def 100644
--- a/test.ts
+++ b/test.ts
@@ -10,3 +10,3 @@
-  expect(result).toBe(5);
+  expect(result).toBe(10);
   const x = 1;
-  expect(list).toHaveLength(3);
+  expect(list).toHaveLength(5);
"#;

        let mut removed = Vec::new();
        let mut added = Vec::new();

        for line in diff.lines() {
            if line.starts_with('-') && !line.starts_with("---") {
                let content = &line[1..];
                if is_assertion_line(content) {
                    removed.push(content.trim().to_string());
                }
            } else if line.starts_with('+') && !line.starts_with("+++") {
                let content = &line[1..];
                if is_assertion_line(content) {
                    added.push(content.trim().to_string());
                }
            }
        }

        assert_eq!(removed.len(), 2);
        assert_eq!(added.len(), 2);
        assert!(removed[0].contains("toBe(5)"));
        assert!(added[0].contains("toBe(10)"));
        assert!(removed[1].contains("toHaveLength(3)"));
        assert!(added[1].contains("toHaveLength(5)"));
    }

    #[test]
    fn parse_diff_ignores_non_assertion_changes() {
        let diff = r#"diff --git a/test.ts b/test.ts
--- a/test.ts
+++ b/test.ts
@@ -5,3 +5,3 @@
-  const name = 'Alice';
+  const name = 'Bob';
-  // This is a comment
+  // Updated comment
"#;

        let mut removed = Vec::new();
        let mut added = Vec::new();

        for line in diff.lines() {
            if line.starts_with('-') && !line.starts_with("---") {
                let content = &line[1..];
                if is_assertion_line(content) {
                    removed.push(content.trim().to_string());
                }
            } else if line.starts_with('+') && !line.starts_with("+++") {
                let content = &line[1..];
                if is_assertion_line(content) {
                    added.push(content.trim().to_string());
                }
            }
        }

        assert!(removed.is_empty());
        assert!(added.is_empty());
    }

    #[test]
    fn parse_diff_new_assertion_added() {
        let diff = r#"diff --git a/test.ts b/test.ts
--- a/test.ts
+++ b/test.ts
@@ -10,2 +10,4 @@
   expect(result).toBe(5);
+  expect(result).toBeGreaterThan(0);
+  expect(result).toBeLessThan(100);
"#;

        let mut added = Vec::new();
        for line in diff.lines() {
            if line.starts_with('+') && !line.starts_with("+++") {
                let content = &line[1..];
                if is_assertion_line(content) {
                    added.push(content.trim().to_string());
                }
            }
        }

        assert_eq!(added.len(), 2);
        assert!(added[0].contains("toBeGreaterThan"));
        assert!(added[1].contains("toBeLessThan"));
    }

    #[test]
    fn parse_diff_assertion_removed() {
        let diff = r#"diff --git a/test.ts b/test.ts
--- a/test.ts
+++ b/test.ts
@@ -10,3 +10,1 @@
-  expect(result).toThrow();
-  expect(result).toThrowError('invalid');
   const cleanup = true;
"#;

        let mut removed = Vec::new();
        for line in diff.lines() {
            if line.starts_with('-') && !line.starts_with("---") {
                let content = &line[1..];
                if is_assertion_line(content) {
                    removed.push(content.trim().to_string());
                }
            }
        }

        assert_eq!(removed.len(), 2);
        assert!(removed[0].contains("toThrow()"));
        assert!(removed[1].contains("toThrowError"));
    }

    // ── Test file discovery (filesystem-dependent, use tempdir) ──────

    #[test]
    fn find_tests_sibling_test() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("users.ts"), "export function foo() {}").unwrap();
        std::fs::write(src.join("users.test.ts"), "test('foo', () => {});").unwrap();

        let found = find_test_files(dir.path(), Path::new("src/users.ts")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, PathBuf::from("src/users.test.ts"));
        assert_eq!(found[0].convention, TestConvention::DotTest);
    }

    #[test]
    fn find_tests_sibling_spec() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("users.ts"), "").unwrap();
        std::fs::write(src.join("users.spec.ts"), "").unwrap();

        let found = find_test_files(dir.path(), Path::new("src/users.ts")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].convention, TestConvention::DotSpec);
    }

    #[test]
    fn find_tests_tsx() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("Button.tsx"), "").unwrap();
        std::fs::write(src.join("Button.test.tsx"), "").unwrap();

        let found = find_test_files(dir.path(), Path::new("src/Button.tsx")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, PathBuf::from("src/Button.test.tsx"));
    }

    #[test]
    fn find_tests_in_tests_dir() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let tests = src.join("__tests__");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(src.join("users.ts"), "").unwrap();
        std::fs::write(tests.join("users.ts"), "").unwrap();

        let found = find_test_files(dir.path(), Path::new("src/users.ts")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, PathBuf::from("src/__tests__/users.ts"));
        assert_eq!(found[0].convention, TestConvention::TestsDir);
    }

    #[test]
    fn find_tests_in_tests_dir_with_test_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let tests = src.join("__tests__");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(src.join("users.ts"), "").unwrap();
        std::fs::write(tests.join("users.test.ts"), "").unwrap();

        let found = find_test_files(dir.path(), Path::new("src/users.ts")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, PathBuf::from("src/__tests__/users.test.ts"));
    }

    #[test]
    fn find_tests_multiple_matches() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let tests = src.join("__tests__");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(src.join("users.ts"), "").unwrap();
        std::fs::write(src.join("users.test.ts"), "").unwrap();
        std::fs::write(src.join("users.spec.ts"), "").unwrap();
        std::fs::write(tests.join("users.test.ts"), "").unwrap();

        let found = find_test_files(dir.path(), Path::new("src/users.ts")).unwrap();
        assert_eq!(found.len(), 3);
    }

    #[test]
    fn find_tests_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("users.ts"), "").unwrap();

        let found = find_test_files(dir.path(), Path::new("src/users.ts")).unwrap();
        assert!(found.is_empty());
    }

    // ── Strategy 4: __tests__ subdirectories ──────────────────────

    #[test]
    fn find_tests_in_nested_tests_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src").join("Popover");
        let tests = src.join("__tests__").join("Generated");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(src.join("PopoverHeader.tsx"), "").unwrap();
        std::fs::write(tests.join("PopoverHeader.test.tsx"), "").unwrap();

        let found =
            find_test_files(dir.path(), Path::new("src/Popover/PopoverHeader.tsx")).unwrap();
        assert!(
            found.iter().any(|f| f.path
                == PathBuf::from("src/Popover/__tests__/Generated/PopoverHeader.test.tsx")),
            "Should find test in __tests__/Generated/ subdir, got: {:?}",
            found.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
    }

    // ── Strategy 6: Component-level tests ───────────────────────────

    #[test]
    fn find_tests_parent_component_name() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src").join("Slider");
        let tests = src.join("__tests__");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(src.join("SliderStep.tsx"), "").unwrap();
        std::fs::write(tests.join("Slider.test.tsx"), "").unwrap();

        let found = find_test_files(dir.path(), Path::new("src/Slider/SliderStep.tsx")).unwrap();
        assert!(
            found
                .iter()
                .any(|f| f.path == PathBuf::from("src/Slider/__tests__/Slider.test.tsx")),
            "Should find parent component test, got: {:?}",
            found.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn find_tests_parent_component_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src").join("Card");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("CardHeader.tsx"), "").unwrap();
        std::fs::write(src.join("Card.test.tsx"), "").unwrap();

        let found = find_test_files(dir.path(), Path::new("src/Card/CardHeader.tsx")).unwrap();
        assert!(
            found
                .iter()
                .any(|f| f.path == PathBuf::from("src/Card/Card.test.tsx")),
            "Should find parent component sibling test, got: {:?}",
            found.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
    }

    // ── Strategy 7: Directory-level tests ───────────────────────────

    #[test]
    fn find_tests_directory_level_all_tests() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src").join("Nav");
        let tests = src.join("__tests__");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(src.join("NavItem.tsx"), "").unwrap();
        std::fs::write(tests.join("Nav.test.tsx"), "").unwrap();
        std::fs::write(tests.join("NavExpandable.test.tsx"), "").unwrap();

        let found = find_test_files(dir.path(), Path::new("src/Nav/NavItem.tsx")).unwrap();
        // Should find both tests since they're in the same __tests__ dir
        let paths: Vec<_> = found
            .iter()
            .map(|f| f.path.to_string_lossy().to_string())
            .collect();
        assert!(
            paths.iter().any(|p| p.contains("Nav.test.tsx")),
            "Should find Nav.test.tsx via directory-level, got: {:?}",
            paths
        );
        assert!(
            paths.iter().any(|p| p.contains("NavExpandable.test.tsx")),
            "Should find NavExpandable.test.tsx via directory-level, got: {:?}",
            paths
        );
    }

    // ── infer_parent_component_name tests ────────────────────────────

    #[test]
    fn infer_parent_slider_step() {
        assert_eq!(
            infer_parent_component_name("SliderStep"),
            Some("Slider".into())
        );
    }

    #[test]
    fn infer_parent_card_header() {
        assert_eq!(
            infer_parent_component_name("CardHeader"),
            Some("Card".into())
        );
    }

    #[test]
    fn infer_parent_modal_box_title() {
        // BoxTitle suffix is longer than Title, so strips to Modal (grandparent)
        // This is fine — Modal.test.tsx covers ModalBoxTitle.tsx
        assert_eq!(
            infer_parent_component_name("ModalBoxTitle"),
            Some("Modal".into())
        );
    }

    #[test]
    fn infer_parent_popover_header_icon() {
        // HeaderIcon suffix is longer than Icon, so strips to Popover (grandparent)
        // This is fine — Popover.test.tsx covers PopoverHeaderIcon.tsx
        assert_eq!(
            infer_parent_component_name("PopoverHeaderIcon"),
            Some("Popover".into())
        );
    }

    #[test]
    fn infer_parent_none_for_short() {
        assert_eq!(infer_parent_component_name("Ab"), None); // prefix too short
        assert_eq!(infer_parent_component_name("Icon"), None); // prefix "I" too short
    }

    // ── Existing test ───────────────────────────────────────────────

    #[test]
    fn find_tests_parent_tests_dir() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let api = src.join("api");
        let tests = src.join("__tests__").join("api");
        std::fs::create_dir_all(&api).unwrap();
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(api.join("users.ts"), "").unwrap();
        std::fs::write(tests.join("users.test.ts"), "").unwrap();

        let found = find_test_files(dir.path(), Path::new("src/api/users.ts")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].path,
            PathBuf::from("src/__tests__/api/users.test.ts")
        );
    }
}
