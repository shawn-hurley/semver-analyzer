//! Java test file discovery and assertion diff detection.
//!
//! Discovers JUnit/TestNG test files by convention (Maven/Gradle standard
//! layout) and detects assertion changes using text-based pattern matching.

use anyhow::{Context, Result};
use semver_analyzer_core::{TestConvention, TestDiff, TestFile};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Java test analyzer.
pub struct JavaTestAnalyzer;

impl JavaTestAnalyzer {
    pub fn new() -> Self {
        Self
    }

    /// Find test files associated with a Java source file.
    ///
    /// Uses Maven/Gradle standard layout conventions:
    /// - `src/main/java/com/example/Foo.java` → `src/test/java/com/example/FooTest.java`
    /// - Also checks for `FooTests.java`, `FooIT.java`, `FooITCase.java`
    pub fn find_tests(&self, repo: &Path, source_file: &Path) -> Result<Vec<TestFile>> {
        let mut results = Vec::new();
        let mut seen = std::collections::HashSet::new();

        let source_str = source_file.to_string_lossy();

        // Extract the stem (filename without .java)
        let stem = source_file
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();

        if stem.is_empty() {
            return Ok(results);
        }

        // Strategy 1: Maven/Gradle standard layout
        // src/main/java/pkg/Foo.java → src/test/java/pkg/FooTest.java
        if source_str.contains("/src/main/java/") {
            let test_base = source_str.replace("/src/main/java/", "/src/test/java/");
            let test_dir = Path::new(&test_base).parent().unwrap_or(Path::new(""));

            for suffix in &["Test", "Tests", "IT", "ITCase"] {
                let test_path = test_dir.join(format!("{}{}.java", stem, suffix));
                let abs_path = repo.join(&test_path);
                if abs_path.exists() && seen.insert(abs_path.clone()) {
                    results.push(TestFile {
                        path: test_path,
                        convention: if *suffix == "IT" || *suffix == "ITCase" {
                            TestConvention::Suffix(suffix.to_string())
                        } else {
                            TestConvention::MirrorTree("src/test/java".to_string())
                        },
                    });
                }
            }
        }

        // Strategy 2: Sibling test file (same directory)
        if let Some(parent) = source_file.parent() {
            for suffix in &["Test", "Tests", "IT", "ITCase"] {
                let test_path = parent.join(format!("{}{}.java", stem, suffix));
                let abs_path = repo.join(&test_path);
                if abs_path.exists() && seen.insert(abs_path.clone()) {
                    results.push(TestFile {
                        path: test_path,
                        convention: if *suffix == "Test" || *suffix == "Tests" {
                            TestConvention::Suffix(suffix.to_string())
                        } else {
                            TestConvention::Suffix(suffix.to_string())
                        },
                    });
                }
            }
        }

        // Strategy 3: Search test directories for files matching the class name
        let test_dirs = ["src/test/java", "src/test", "test"];
        for test_dir in &test_dirs {
            let abs_test_dir = repo.join(test_dir);
            if abs_test_dir.is_dir() {
                find_tests_recursive(repo, &abs_test_dir, &stem, &mut results, &mut seen)?;
            }
        }

        Ok(results)
    }

    /// Diff test assertions between two git refs.
    ///
    /// Uses text-based assertion detection — matches JUnit, AssertJ,
    /// Hamcrest, and TestNG assertion patterns.
    pub fn diff_test_assertions(
        &self,
        repo: &Path,
        test_file: &TestFile,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<TestDiff> {
        let output = Command::new("git")
            .args([
                "diff",
                &format!("{}..{}", from_ref, to_ref),
                "--",
                &test_file.path.to_string_lossy(),
            ])
            .current_dir(repo)
            .output()
            .context("Failed to run git diff for test file")?;

        let diff_text = String::from_utf8_lossy(&output.stdout).to_string();

        let mut removed_assertions = Vec::new();
        let mut added_assertions = Vec::new();

        for line in diff_text.lines() {
            if line.starts_with('-') && !line.starts_with("---") {
                let content = &line[1..];
                if is_assertion_line(content) {
                    removed_assertions.push(content.trim().to_string());
                }
            } else if line.starts_with('+') && !line.starts_with("+++") {
                let content = &line[1..];
                if is_assertion_line(content) {
                    added_assertions.push(content.trim().to_string());
                }
            }
        }

        let has_changes = !removed_assertions.is_empty() || !added_assertions.is_empty();

        Ok(TestDiff {
            test_file: test_file.path.clone(),
            removed_assertions,
            added_assertions,
            has_assertion_changes: has_changes,
            full_diff: diff_text,
        })
    }
}

/// Recursively find test files matching a class name in a directory.
fn find_tests_recursive(
    repo: &Path,
    dir: &Path,
    stem: &str,
    results: &mut Vec<TestFile>,
    seen: &mut std::collections::HashSet<PathBuf>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            find_tests_recursive(repo, &path, stem, results, seen)?;
        } else {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if name_str.ends_with(".java") {
                for suffix in &["Test", "Tests", "IT", "ITCase"] {
                    let expected = format!("{}{}.java", stem, suffix);
                    if name_str.as_ref() == expected {
                        let rel_path = path.strip_prefix(repo).unwrap_or(&path).to_path_buf();
                        if seen.insert(path.clone()) {
                            results.push(TestFile {
                                path: rel_path,
                                convention: TestConvention::TestsDir,
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Check if a line contains a Java assertion pattern.
///
/// Matches JUnit 4/5, AssertJ, Hamcrest, and TestNG assertion patterns.
fn is_assertion_line(line: &str) -> bool {
    let trimmed = line.trim();

    // Skip empty lines and comments
    if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('*') {
        return false;
    }

    // JUnit 4/5 assertions
    if trimmed.contains("assertEquals(")
        || trimmed.contains("assertNotEquals(")
        || trimmed.contains("assertTrue(")
        || trimmed.contains("assertFalse(")
        || trimmed.contains("assertNull(")
        || trimmed.contains("assertNotNull(")
        || trimmed.contains("assertSame(")
        || trimmed.contains("assertNotSame(")
        || trimmed.contains("assertArrayEquals(")
        || trimmed.contains("assertThrows(")
        || trimmed.contains("assertDoesNotThrow(")
        || trimmed.contains("assertTimeout(")
        || trimmed.contains("assertTimeoutPreemptively(")
        || trimmed.contains("assertIterableEquals(")
        || trimmed.contains("assertLinesMatch(")
        || trimmed.contains("assertAll(")
        || trimmed.contains("assertInstanceOf(")
    {
        return true;
    }

    // AssertJ (fluent assertions)
    if trimmed.contains("assertThat(")
        || trimmed.contains(".isEqualTo(")
        || trimmed.contains(".isNotEqualTo(")
        || trimmed.contains(".isNull()")
        || trimmed.contains(".isNotNull()")
        || trimmed.contains(".isTrue()")
        || trimmed.contains(".isFalse()")
        || trimmed.contains(".isEmpty()")
        || trimmed.contains(".isNotEmpty()")
        || trimmed.contains(".contains(")
        || trimmed.contains(".containsExactly(")
        || trimmed.contains(".hasSize(")
        || trimmed.contains(".isInstanceOf(")
        || trimmed.contains(".isExactlyInstanceOf(")
        || trimmed.contains(".startsWith(")
        || trimmed.contains(".endsWith(")
        || trimmed.contains(".matches(")
        || trimmed.contains(".satisfies(")
        || trimmed.contains(".extracting(")
        || trimmed.contains(".filteredOn(")
        || trimmed.contains(".isPresent()")
        || trimmed.contains(".isAbsent()")
        || trimmed.contains(".hasMessage(")
    {
        return true;
    }

    // Hamcrest matchers
    if trimmed.contains("assertThat(") && trimmed.contains("is(")
        || trimmed.contains("assertThat(") && trimmed.contains("equalTo(")
        || trimmed.contains("assertThat(") && trimmed.contains("hasItem(")
        || trimmed.contains("assertThat(") && trimmed.contains("hasItems(")
        || trimmed.contains("assertThat(") && trimmed.contains("containsString(")
        || trimmed.contains("assertThat(") && trimmed.contains("not(")
    {
        return true;
    }

    // TestNG assertions
    if trimmed.contains("Assert.assertEquals(")
        || trimmed.contains("Assert.assertNotEquals(")
        || trimmed.contains("Assert.assertTrue(")
        || trimmed.contains("Assert.assertFalse(")
        || trimmed.contains("Assert.assertNull(")
        || trimmed.contains("Assert.assertNotNull(")
        || trimmed.contains("Assert.assertSame(")
        || trimmed.contains("Assert.expectThrows(")
    {
        return true;
    }

    // Mockito verification
    if trimmed.contains("verify(") || trimmed.contains("verifyNoInteractions(") {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_assertion_line_junit() {
        assert!(is_assertion_line("  assertEquals(expected, actual);"));
        assert!(is_assertion_line(
            "  assertThrows(Exception.class, () -> foo());"
        ));
        assert!(is_assertion_line("  assertTrue(result.isPresent());"));
        assert!(is_assertion_line("  assertNull(obj);"));
    }

    #[test]
    fn test_is_assertion_line_assertj() {
        assert!(is_assertion_line(
            "  assertThat(result).isEqualTo(expected);"
        ));
        assert!(is_assertion_line("  assertThat(list).hasSize(3);"));
        assert!(is_assertion_line("  assertThat(opt).isPresent();"));
        assert!(is_assertion_line(
            "  assertThat(str).startsWith(\"Hello\");"
        ));
    }

    #[test]
    fn test_is_assertion_line_mockito() {
        assert!(is_assertion_line("  verify(mock).doThing();"));
        assert!(is_assertion_line("  verifyNoInteractions(mock);"));
    }

    #[test]
    fn test_is_assertion_line_negative() {
        assert!(!is_assertion_line("  int x = 1;"));
        assert!(!is_assertion_line("  // assertEquals(a, b);"));
        assert!(!is_assertion_line(""));
        assert!(!is_assertion_line("  * assert something"));
        assert!(!is_assertion_line("  String msg = \"assertEquals\";"));
    }

    #[test]
    fn test_is_assertion_line_testng() {
        assert!(is_assertion_line("  Assert.assertEquals(a, b);"));
        assert!(is_assertion_line("  Assert.assertTrue(flag);"));
    }
}
