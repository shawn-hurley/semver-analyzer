//! TypeScript compiler invocation for .d.ts generation.
//!
//! Runs `tsc --declaration --emitDeclarationOnly` in a worktree and
//! classifies any errors into actionable failure modes.
//!
//! For monorepos, supports multiple strategies:
//! 1. Root tsconfig.json (standard single-project)
//! 2. Solution tsconfig with `references` (e.g., `packages/tsconfig.json`)
//! 3. Per-package tsconfigs (fallback)

use super::error::WorktreeError;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Outcome of a tsc invocation that may partially succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TscOutcome {
    /// All tsc invocations succeeded.
    Success,
    /// Some packages succeeded, some failed. Extraction should continue
    /// and pick up whatever .d.ts files were generated.
    Partial { succeeded: usize, failed: usize },
}

/// Run `tsc --declaration --emitDeclarationOnly` in the given directory.
///
/// Tries strategies in order of preference:
/// 1. Root `tsconfig.json` — standard single-project case
/// 2. Solution tsconfig with `references` (e.g., `packages/tsconfig.json`) — uses `tsc --build`
/// 3. Per-package tsconfigs — runs tsc individually for each package
///
/// Returns `Ok(TscOutcome)` if at least some .d.ts files were generated,
/// or `Err(WorktreeError)` if nothing could be built at all.
pub fn run_tsc_declaration(
    worktree_dir: &Path,
    git_ref: &str,
) -> Result<TscOutcome, WorktreeError> {
    let tsconfig_path = worktree_dir.join("tsconfig.json");

    if tsconfig_path.exists() {
        if is_solution_tsconfig(&tsconfig_path) {
            // Strategy 1a: Root tsconfig has "references" — use tsc --build
            // which handles topological ordering of project references.
            // This is critical for monorepos where packages depend on sibling
            // packages (e.g., react-charts → react-core).
            tracing::info!("Root tsconfig.json has references, using tsc --build");
            match run_tsc_build(worktree_dir, &tsconfig_path, git_ref) {
                Ok(()) => {
                    tracing::info!("tsc --build succeeded");
                    return Ok(TscOutcome::Success);
                }
                Err(e) => {
                    // Solution build failed — some packages may have generated
                    // .d.ts files before the failure. Fall through to other
                    // strategies rather than hard-failing.
                    tracing::warn!(
                        error = %e,
                        "root tsc --build failed, falling through to other strategies"
                    );
                }
            }
        } else {
            // Strategy 1b: Standard single-project tsconfig (no references)
            run_tsc_single(worktree_dir, &tsconfig_path, git_ref)?;
            return Ok(TscOutcome::Success);
        }
    }

    // Strategy 2: Look for solution tsconfigs in subdirectories
    // (packages/tsconfig.json, libs/tsconfig.json, tsconfig.build.json)
    if let Some(solution) = find_solution_tsconfig(worktree_dir) {
        let display_path = solution
            .strip_prefix(worktree_dir)
            .unwrap_or(&solution)
            .display();
        tracing::info!(path = %display_path, "Found solution tsconfig");

        match run_tsc_build(worktree_dir, &solution, git_ref) {
            Ok(()) => {
                tracing::info!("tsc --build succeeded");
                return Ok(TscOutcome::Success);
            }
            Err(e) => {
                // Solution build failed — some packages may have generated .d.ts
                // files before the failure. Log and fall through to per-package.
                tracing::warn!(error = %e, "tsc --build partially failed, falling back to per-package tsc");
            }
        }
    }

    // Strategy 3: Per-package tsconfigs
    let package_tsconfigs = find_package_tsconfigs(worktree_dir);
    if package_tsconfigs.is_empty() {
        return Err(WorktreeError::NoTsconfigFound {
            git_ref: git_ref.to_string(),
        });
    }

    run_tsc_per_package(worktree_dir, &package_tsconfigs, git_ref)
}

/// Run tsc individually for each package tsconfig.
///
/// Continues past failures and reports partial results.
fn run_tsc_per_package(
    worktree_dir: &Path,
    tsconfigs: &[PathBuf],
    git_ref: &str,
) -> Result<TscOutcome, WorktreeError> {
    tracing::info!(package_count = tsconfigs.len(), "Running tsc for packages");

    let mut successes = 0;
    let mut failures = 0;
    for tsconfig in tsconfigs {
        let pkg_dir = tsconfig.parent().unwrap_or(worktree_dir);
        let pkg_name = pkg_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        match run_tsc_single(worktree_dir, tsconfig, git_ref) {
            Ok(()) => {
                successes += 1;
            }
            Err(e) => {
                // Log but continue — some packages may have type errors
                // that don't prevent declaration generation for others.
                tracing::warn!(package = %pkg_name, error = %e, "tsc failed for package");
                failures += 1;
            }
        }
    }

    tracing::info!(succeeded = successes, failed = failures, "tsc complete");

    if successes == 0 {
        return Err(WorktreeError::TscFailed {
            git_ref: git_ref.to_string(),
            error_count: failures,
            reason: "All package tsc invocations failed".to_string(),
        });
    }

    Ok(TscOutcome::Partial {
        succeeded: successes,
        failed: failures,
    })
}

/// Find a solution tsconfig — a tsconfig that only has `references` to other projects.
///
/// Solution tsconfigs are used in monorepos to orchestrate multi-project builds.
/// They typically have `"files": []` (or no `include`) and `"references": [...]`.
///
/// Checks common locations in priority order:
/// 1. `packages/tsconfig.json`
/// 2. `libs/tsconfig.json`
/// 3. `tsconfig.build.json` (at root)
pub fn find_solution_tsconfig(worktree_dir: &Path) -> Option<PathBuf> {
    let candidates = [
        "packages/tsconfig.json",
        "libs/tsconfig.json",
        "tsconfig.build.json",
    ];

    for candidate in &candidates {
        let path = worktree_dir.join(candidate);
        if path.exists() && is_solution_tsconfig(&path) {
            return Some(path);
        }
    }
    None
}

/// Check if a tsconfig.json file is a solution-style config.
///
/// A solution tsconfig has `"references"` and typically `"files": []`.
/// Uses simple string matching since tsconfig can have comments and trailing commas.
pub fn is_solution_tsconfig(path: &Path) -> bool {
    if let Ok(contents) = std::fs::read_to_string(path) {
        contents.contains("\"references\"")
    } else {
        false
    }
}

/// Run `tsc --build` on a solution tsconfig.
///
/// This handles topological ordering of project references automatically.
/// Used for monorepos where packages reference each other.
fn run_tsc_build(
    worktree_dir: &Path,
    tsconfig_path: &Path,
    git_ref: &str,
) -> Result<(), WorktreeError> {
    let tsc_bin = find_tsc_binary(worktree_dir);
    let tsconfig_str = tsconfig_path.to_string_lossy().to_string();

    let output = Command::new(&tsc_bin)
        .args(["--build", &tsconfig_str, "--force"])
        .current_dir(worktree_dir)
        .output()
        .map_err(|e| WorktreeError::CommandFailed(format!("Failed to run tsc --build: {e}")))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let combined = format!("{stdout}\n{stderr}");

    classify_tsc_error(&combined, git_ref)
}

/// Run the project's own build command as a fallback when tsc fails.
///
/// Many monorepos require pre-tsc generation steps (CSS modules, icon generation,
/// design tokens, etc.) that only run as part of the full build. If tsc alone
/// can't produce .d.ts files, this function tries the project's build script.
///
/// For custom build commands (user-provided), runs the command directly.
/// Otherwise, detects the package manager and runs `<pm> run build`.
pub fn run_project_build(
    worktree_dir: &Path,
    build_command: Option<&str>,
) -> Result<(), WorktreeError> {
    // Determine whether we need a shell to interpret the command.
    // Compound commands (using &&, ||, ;, |, or shell expansions) must
    // be run through `sh -c` so the shell handles chaining and quoting.
    let needs_shell = build_command
        .map(|c| c.contains("&&") || c.contains("||") || c.contains(';') || c.contains('|'))
        .unwrap_or(false);

    let (cmd, args) = if let Some(custom) = build_command {
        if custom.trim().is_empty() {
            return Err(WorktreeError::CommandFailed(
                "Empty build command".to_string(),
            ));
        }

        if needs_shell {
            // Run compound commands through the shell
            ("sh".to_string(), vec!["-c".to_string(), custom.to_string()])
        } else {
            // Simple command: split on whitespace
            let parts: Vec<&str> = custom.split_whitespace().collect();
            (
                parts[0].to_string(),
                parts[1..].iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
        }
    } else {
        // Auto-detect: use the package manager to run `build`
        detect_build_command(worktree_dir)?
    };

    tracing::info!(command = %cmd, args = %args.join(" "), "Running project build");

    let output = Command::new(&cmd)
        .args(&args)
        .current_dir(worktree_dir)
        .env("NODE_ENV", "production")
        .output()
        .map_err(|e| WorktreeError::CommandFailed(format!("Failed to run {cmd}: {e}")))?;

    if output.status.success() {
        tracing::info!("Project build succeeded");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Truncate long build output to just the last 20 lines
        let combined = format!("{stdout}\n{stderr}");
        let tail: String = combined
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        Err(WorktreeError::ProjectBuildFailed {
            command: format!("{cmd} {}", args.join(" ")),
            reason: tail,
        })
    }
}

/// Detect the right build command based on what package manager / scripts are available.
///
/// Checks for `build` script in `package.json`, then uses the appropriate
/// package manager to run it.
fn detect_build_command(worktree_dir: &Path) -> Result<(String, Vec<String>), WorktreeError> {
    // Verify there's a build script in package.json
    let pkg_json_path = worktree_dir.join("package.json");
    if pkg_json_path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&pkg_json_path) {
            if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&contents) {
                let has_build = pkg.get("scripts").and_then(|s| s.get("build")).is_some();
                if !has_build {
                    return Err(WorktreeError::CommandFailed(
                        "No 'build' script found in package.json".to_string(),
                    ));
                }
            }
        }
    }

    // Detect package manager and construct build command
    use super::package_manager::PackageManager;
    let pm = PackageManager::detect(worktree_dir);

    match pm {
        Some(PackageManager::Pnpm) => Ok(("pnpm".into(), vec!["run".into(), "build".into()])),
        Some(PackageManager::Yarn | PackageManager::YarnClassic) => {
            Ok(("yarn".into(), vec!["build".into()]))
        }
        Some(PackageManager::Npm) | None => Ok(("npm".into(), vec!["run".into(), "build".into()])),
    }
}

/// Run tsc for a single tsconfig.json file.
fn run_tsc_single(
    worktree_dir: &Path,
    tsconfig_path: &Path,
    git_ref: &str,
) -> Result<(), WorktreeError> {
    // Check for noEmit conflict
    if let Ok(contents) = std::fs::read_to_string(tsconfig_path) {
        if check_no_emit_conflict(&contents) {
            // Override noEmit via command line
        }
    }

    let tsc_bin = find_tsc_binary(worktree_dir);
    let tsconfig_str = tsconfig_path.to_string_lossy().to_string();

    // Check if this uses composite mode (project references)
    let uses_composite = if let Ok(contents) = std::fs::read_to_string(tsconfig_path) {
        contents.contains("\"composite\"") && contents.contains("true")
    } else {
        false
    };

    let output = if uses_composite {
        // Use --build mode for composite projects
        Command::new(&tsc_bin)
            .args(["--build", &tsconfig_str, "--force"])
            .current_dir(worktree_dir)
            .output()
            .map_err(|e| WorktreeError::CommandFailed(format!("Failed to run tsc: {e}")))?
    } else {
        // Standard declaration emit
        Command::new(&tsc_bin)
            .args([
                "--project",
                &tsconfig_str,
                "--declaration",
                "--emitDeclarationOnly",
                "--noEmit",
                "false",
            ])
            .current_dir(worktree_dir)
            .output()
            .map_err(|e| WorktreeError::CommandFailed(format!("Failed to run tsc: {e}")))?
    };

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let combined = format!("{stdout}\n{stderr}");

    classify_tsc_error(&combined, git_ref)
}

/// Find tsconfig.json files in package subdirectories (monorepo support).
///
/// Looks for `packages/*/tsconfig.json` patterns. Skips test/demo packages.
fn find_package_tsconfigs(worktree_dir: &Path) -> Vec<std::path::PathBuf> {
    let mut tsconfigs = Vec::new();

    // Common monorepo package dirs
    for packages_dir_name in &["packages", "libs", "apps"] {
        let packages_dir = worktree_dir.join(packages_dir_name);
        if !packages_dir.is_dir() {
            continue;
        }

        if let Ok(entries) = std::fs::read_dir(&packages_dir) {
            for entry in entries.flatten() {
                let pkg_dir = entry.path();
                if !pkg_dir.is_dir() {
                    continue;
                }

                let dir_name = entry.file_name().to_string_lossy().to_string();
                // Skip test/demo/integration packages
                if dir_name.contains("integration")
                    || dir_name.contains("demo")
                    || dir_name.contains("test")
                    || dir_name.starts_with('.')
                {
                    continue;
                }

                let tsconfig = pkg_dir.join("tsconfig.json");
                if tsconfig.exists() {
                    tsconfigs.push(tsconfig);
                }
            }
        }
    }

    tsconfigs.sort();
    tsconfigs
}

/// Find the tsc binary, preferring local node_modules/.bin/tsc.
fn find_tsc_binary(worktree_dir: &Path) -> String {
    let local_tsc = worktree_dir.join("node_modules/.bin/tsc");
    if local_tsc.exists() {
        local_tsc.to_string_lossy().to_string()
    } else {
        // Fall back to global tsc
        "tsc".to_string()
    }
}

/// Check if tsconfig.json has "noEmit": true which conflicts with --declaration.
///
/// This is a simple string check, not a full JSON parse, because tsconfig
/// can contain comments and trailing commas (JSON5-like).
pub fn check_no_emit_conflict(tsconfig_contents: &str) -> bool {
    // Look for "noEmit": true or "noEmit":true (with varying whitespace)
    // This is intentionally simple -- false positives are acceptable
    // (we'll catch the real error from tsc output anyway).
    tsconfig_contents.contains("\"noEmit\"")
        && tsconfig_contents.contains("true")
        && !tsconfig_contents.contains("\"noEmit\": false")
        && !tsconfig_contents.contains("\"noEmit\":false")
}

/// Classify a tsc error message into a specific WorktreeError variant.
fn classify_tsc_error(output: &str, git_ref: &str) -> Result<(), WorktreeError> {
    // Count errors
    let error_count = count_tsc_errors(output);

    // Check for project reference issues
    if output.contains("Referenced project") || output.contains("--build") {
        return Err(WorktreeError::ProjectReferencesNotBuilt);
    }

    // Check for import resolution failures.
    // Distinguish between true missing external dependencies and workspace
    // sibling packages that haven't been built yet. Scoped package names
    // like @org/pkg-name that appear in "Cannot find module" errors are
    // typically workspace siblings — they're installed (symlinked) but
    // their .d.ts output doesn't exist until they're compiled.
    if output.contains("Cannot find module")
        || output.contains("Could not find a declaration file for module")
    {
        // Heuristic: if the missing module is a scoped package (@org/...),
        // it's likely a workspace sibling that needs to be built first.
        let has_workspace_module = output.lines().any(|line| {
            (line.contains("Cannot find module")
                || line.contains("Could not find a declaration file"))
                && line.contains("'@")
        });

        if has_workspace_module {
            tracing::warn!(
                git_ref = %git_ref,
                "tsc failed: workspace sibling packages not yet built (not a missing install)"
            );
            return Err(WorktreeError::ProjectReferencesNotBuilt);
        }

        return Err(WorktreeError::MissingDependencies {
            git_ref: git_ref.to_string(),
        });
    }

    // Check for syntax errors that suggest version incompatibility
    if output.contains("Unexpected token") || output.contains("Expression expected") {
        return Err(WorktreeError::UnsupportedSyntax {
            git_ref: git_ref.to_string(),
            reason: first_error_line(output).to_string(),
        });
    }

    // Generic tsc failure
    Err(WorktreeError::TscFailed {
        git_ref: git_ref.to_string(),
        error_count,
        reason: first_error_line(output).to_string(),
    })
}

/// Count the number of errors in tsc output.
///
/// tsc outputs errors like: `src/file.ts(1,2): error TS1234: ...`
/// and a summary line like: `Found 3 errors.`
pub fn count_tsc_errors(output: &str) -> usize {
    // Try the summary line first: "Found N error(s)."
    for line in output.lines().rev() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Found ") {
            if let Some(count_str) = rest.split_whitespace().next() {
                if let Ok(count) = count_str.parse::<usize>() {
                    return count;
                }
            }
        }
    }

    // Fall back to counting "error TS" occurrences
    output.matches("error TS").count()
}

/// Extract the first error line from tsc output for use in error messages.
fn first_error_line(output: &str) -> &str {
    output
        .lines()
        .find(|line| line.contains("error TS") || line.contains("Error:"))
        .unwrap_or("Unknown error")
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_emit_conflict_detected() {
        let tsconfig = r#"{
            "compilerOptions": {
                "noEmit": true,
                "strict": true
            }
        }"#;
        assert!(check_no_emit_conflict(tsconfig));
    }

    #[test]
    fn no_emit_false_not_a_conflict() {
        let tsconfig = r#"{
            "compilerOptions": {
                "noEmit": false,
                "strict": true
            }
        }"#;
        assert!(!check_no_emit_conflict(tsconfig));
    }

    #[test]
    fn no_no_emit_field_not_a_conflict() {
        let tsconfig = r#"{
            "compilerOptions": {
                "strict": true,
                "declaration": true
            }
        }"#;
        assert!(!check_no_emit_conflict(tsconfig));
    }

    #[test]
    fn count_errors_from_summary_line() {
        let output = r#"
src/index.ts(1,1): error TS2304: Cannot find name 'foo'.
src/index.ts(2,1): error TS2304: Cannot find name 'bar'.
src/index.ts(3,1): error TS2304: Cannot find name 'baz'.

Found 3 errors.
"#;
        assert_eq!(count_tsc_errors(output), 3);
    }

    #[test]
    fn count_errors_from_summary_plural() {
        let output = "Found 15 errors in 3 files.\n";
        assert_eq!(count_tsc_errors(output), 15);
    }

    #[test]
    fn count_errors_fallback_to_occurrence_count() {
        let output = r#"
src/a.ts(1,1): error TS2304: Cannot find name 'x'.
src/b.ts(1,1): error TS2304: Cannot find name 'y'.
"#;
        assert_eq!(count_tsc_errors(output), 2);
    }

    #[test]
    fn count_errors_zero_when_no_errors() {
        let output = "Compilation complete.\n";
        assert_eq!(count_tsc_errors(output), 0);
    }

    #[test]
    fn classify_missing_dependencies() {
        let output = "src/index.ts(1,1): error TS2307: Cannot find module 'express'.\n";
        let result = classify_tsc_error(output, "v1.0.0");
        match result {
            Err(WorktreeError::MissingDependencies { git_ref }) => {
                assert_eq!(git_ref, "v1.0.0");
            }
            other => panic!("Expected MissingDependencies, got {:?}", other),
        }
    }

    #[test]
    fn classify_project_references() {
        let output = "error TS6305: Referenced project '/foo/tsconfig.json' must have setting \"composite\": true.\n";
        let result = classify_tsc_error(output, "v1.0.0");
        assert!(matches!(
            result,
            Err(WorktreeError::ProjectReferencesNotBuilt)
        ));
    }

    #[test]
    fn classify_workspace_sibling_as_project_references() {
        // Scoped packages like @patternfly/react-core are workspace siblings,
        // not truly missing external deps. Should be ProjectReferencesNotBuilt.
        let output =
            "src/index.ts(1,1): error TS2307: Cannot find module '@patternfly/react-core'.\n";
        let result = classify_tsc_error(output, "v6.4.1");
        assert!(
            matches!(result, Err(WorktreeError::ProjectReferencesNotBuilt)),
            "Expected ProjectReferencesNotBuilt for workspace sibling, got {:?}",
            result
        );
    }

    #[test]
    fn classify_unsupported_syntax() {
        let output = "src/index.ts(1,1): error TS1109: Expression expected.\n";
        let result = classify_tsc_error(output, "v1.0.0");
        assert!(matches!(
            result,
            Err(WorktreeError::UnsupportedSyntax { .. })
        ));
    }

    #[test]
    fn classify_generic_failure() {
        let output = "src/index.ts(1,1): error TS2322: Type 'string' is not assignable to type 'number'.\nFound 1 error.\n";
        let result = classify_tsc_error(output, "v1.0.0");
        match result {
            Err(WorktreeError::TscFailed {
                git_ref,
                error_count,
                ..
            }) => {
                assert_eq!(git_ref, "v1.0.0");
                assert_eq!(error_count, 1);
            }
            other => panic!("Expected TscFailed, got {:?}", other),
        }
    }

    #[test]
    fn first_error_line_finds_ts_error() {
        let output =
            "some preamble\nsrc/x.ts(1,1): error TS2304: Cannot find name 'x'.\nmore stuff\n";
        assert_eq!(
            first_error_line(output),
            "src/x.ts(1,1): error TS2304: Cannot find name 'x'."
        );
    }

    #[test]
    fn first_error_line_returns_unknown_when_no_error() {
        assert_eq!(first_error_line("no errors here\n"), "Unknown error");
    }

    // ── Solution tsconfig detection tests ──

    #[test]
    fn solution_tsconfig_detected() {
        let dir = tempfile::TempDir::new().unwrap();
        let packages_dir = dir.path().join("packages");
        std::fs::create_dir_all(&packages_dir).unwrap();

        // Write a solution tsconfig with references
        std::fs::write(
            packages_dir.join("tsconfig.json"),
            r#"{
                "files": [],
                "references": [
                    { "path": "./core" },
                    { "path": "./icons" }
                ]
            }"#,
        )
        .unwrap();

        let result = find_solution_tsconfig(dir.path());
        assert!(result.is_some());
        assert_eq!(result.unwrap(), dir.path().join("packages/tsconfig.json"));
    }

    #[test]
    fn solution_tsconfig_not_found_without_references() {
        let dir = tempfile::TempDir::new().unwrap();
        let packages_dir = dir.path().join("packages");
        std::fs::create_dir_all(&packages_dir).unwrap();

        // Write a non-solution tsconfig (no references)
        std::fs::write(
            packages_dir.join("tsconfig.json"),
            r#"{ "compilerOptions": { "strict": true } }"#,
        )
        .unwrap();

        let result = find_solution_tsconfig(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn solution_tsconfig_root_build_variant() {
        let dir = tempfile::TempDir::new().unwrap();

        // tsconfig.build.json at root with references
        std::fs::write(
            dir.path().join("tsconfig.build.json"),
            r#"{
                "files": [],
                "references": [
                    { "path": "./packages/core" }
                ]
            }"#,
        )
        .unwrap();

        let result = find_solution_tsconfig(dir.path());
        assert!(result.is_some());
        assert_eq!(result.unwrap(), dir.path().join("tsconfig.build.json"));
    }

    #[test]
    fn solution_tsconfig_packages_preferred_over_root() {
        let dir = tempfile::TempDir::new().unwrap();
        let packages_dir = dir.path().join("packages");
        std::fs::create_dir_all(&packages_dir).unwrap();

        // Both exist — packages/tsconfig.json should be preferred
        std::fs::write(
            packages_dir.join("tsconfig.json"),
            r#"{ "files": [], "references": [{ "path": "./a" }] }"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("tsconfig.build.json"),
            r#"{ "files": [], "references": [{ "path": "./b" }] }"#,
        )
        .unwrap();

        let result = find_solution_tsconfig(dir.path());
        assert_eq!(result.unwrap(), dir.path().join("packages/tsconfig.json"));
    }

    #[test]
    fn is_solution_tsconfig_true_for_references() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("tsconfig.json");
        std::fs::write(
            &path,
            r#"{ "files": [], "references": [{ "path": "./a" }] }"#,
        )
        .unwrap();
        assert!(is_solution_tsconfig(&path));
    }

    #[test]
    fn is_solution_tsconfig_false_for_plain() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("tsconfig.json");
        std::fs::write(&path, r#"{ "compilerOptions": { "strict": true } }"#).unwrap();
        assert!(!is_solution_tsconfig(&path));
    }

    #[test]
    fn is_solution_tsconfig_false_for_missing_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.json");
        assert!(!is_solution_tsconfig(&path));
    }

    // ── Build command detection tests ──

    #[test]
    fn detect_build_command_with_build_script() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "scripts": { "build": "tsc --build" } }"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("package-lock.json"), "{}").unwrap();

        let result = detect_build_command(dir.path());
        assert!(result.is_ok());
        let (cmd, args) = result.unwrap();
        assert_eq!(cmd, "npm");
        assert!(args.contains(&"build".to_string()));
    }

    #[test]
    fn detect_build_command_no_build_script() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "scripts": { "test": "jest" } }"#,
        )
        .unwrap();

        let result = detect_build_command(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn detect_build_command_yarn() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "scripts": { "build": "tsc" } }"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("yarn.lock"), "").unwrap();

        let (cmd, args) = detect_build_command(dir.path()).unwrap();
        assert_eq!(cmd, "yarn");
        assert_eq!(args, vec!["build"]);
    }

    #[test]
    fn detect_build_command_pnpm() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "scripts": { "build": "tsc" } }"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();

        let (cmd, args) = detect_build_command(dir.path()).unwrap();
        assert_eq!(cmd, "pnpm");
        assert_eq!(args, vec!["run", "build"]);
    }

    // ── TscOutcome tests ──

    #[test]
    fn tsc_outcome_partial_equality() {
        assert_eq!(
            TscOutcome::Partial {
                succeeded: 3,
                failed: 2
            },
            TscOutcome::Partial {
                succeeded: 3,
                failed: 2
            }
        );
        assert_ne!(
            TscOutcome::Success,
            TscOutcome::Partial {
                succeeded: 1,
                failed: 0
            }
        );
    }
}
