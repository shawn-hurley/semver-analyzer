//! RAII guard for git worktree lifecycle management.
//!
//! Creates a temporary worktree, installs dependencies, runs tsc,
//! and cleans up on drop (even on panic or early return).

use super::error::WorktreeError;
use super::package_manager::PackageManager;
use super::tsc;
use super::ExtractionWarning;
#[cfg(test)]
use semver_analyzer_core::git::sanitize_ref_name;
use semver_analyzer_core::git::worktree_path_for;
use semver_analyzer_core::traits::WorktreeAccess;
use std::path::{Path, PathBuf};
use std::process::Command;

/// RAII guard that manages a git worktree's lifecycle.
///
/// On construction: creates a worktree, installs dependencies, runs tsc.
/// On drop: removes the worktree (even on panic or early return).
///
/// Implements `WorktreeAccess` so it can be wrapped in `Arc` and shared
/// between TD and SD pipelines via `std::sync::mpsc::channel`.
pub struct WorktreeGuard {
    /// Path to the repository root.
    repo_root: PathBuf,

    /// Path to the created worktree directory.
    worktree_path: PathBuf,

    /// The git ref this worktree was created for.
    git_ref: String,

    /// Whether the worktree was successfully created (controls cleanup).
    created: bool,

    /// Non-fatal issues encountered during setup (partial tsc, fallbacks).
    /// Inspected by the caller to record degradation.
    warnings: Vec<ExtractionWarning>,
}

impl WorktreeGuard {
    /// Create a new worktree for the given git ref, install dependencies,
    /// and run `tsc --declaration`.
    ///
    /// This is the primary entry point. It performs the full worktree lifecycle:
    /// 1. Validate the repo and ref
    /// 2. Create the worktree via `git worktree add`
    /// 3. Resolve Node.js version (if configured via `config.node_version`)
    /// 4. Detect and run the package manager install (or use `config.install_command`)
    /// 5. Run `tsc --declaration --emitDeclarationOnly`
    /// 6. If tsc fails partially, try the project build as a fallback
    ///
    /// The `config` parameter allows per-ref overrides for Node.js version,
    /// install command, and build command.
    ///
    /// On any failure, the worktree is cleaned up before the error propagates.
    pub fn new(
        repo: &Path,
        git_ref: &str,
        config: &super::RefBuildConfig,
    ) -> Result<Self, WorktreeError> {
        // Canonicalize repo path to avoid relative path mismatches between
        // git (which resolves paths relative to its CWD) and Rust filesystem
        // calls (which resolve relative to the process CWD).
        let repo = repo.canonicalize().map_err(|e| {
            WorktreeError::CommandFailed(format!(
                "Failed to canonicalize repo path {}: {}",
                repo.display(),
                e
            ))
        })?;
        let repo = repo.as_path();

        // Validate repo is a git repository
        validate_git_repo(repo)?;

        // Validate the ref exists
        validate_git_ref(repo, git_ref)?;

        // Determine worktree path
        let worktree_path = worktree_path_for(repo, git_ref);

        // Create the guard (Drop will handle cleanup even if later steps fail)
        let mut guard = Self {
            repo_root: repo.to_path_buf(),
            worktree_path: worktree_path.clone(),
            git_ref: git_ref.to_string(),
            created: false,
            warnings: Vec::new(),
        };

        // Ensure parent directory exists
        let parent = worktree_path
            .parent()
            .expect("worktree path should have a parent");
        std::fs::create_dir_all(parent)?;

        // Create the worktree
        create_worktree(repo, git_ref, &worktree_path)?;
        guard.created = true;

        // Resolve Node.js environment (prepends nvm bin dir to PATH if configured)
        let node_env = super::nvm::build_node_env(config.node_version.as_deref())?;

        // Install dependencies
        if let Some(ref install_cmd) = config.install_command {
            run_custom_install(&worktree_path, install_cmd, &node_env)?;
        } else {
            let pm = PackageManager::detect(&worktree_path).ok_or_else(|| {
                WorktreeError::NoLockfileFound {
                    git_ref: git_ref.to_string(),
                }
            })?;
            run_package_install(&worktree_path, pm, &node_env)?;
        }

        // If user provided a build command, run it instead of tsc
        if let Some(ref cmd) = config.build_command {
            tracing::info!("Running user-provided build command");
            tsc::run_project_build(&worktree_path, Some(cmd), &node_env)?;
            return Ok(guard);
        }

        // Run tsc --declaration (tries solution tsconfig, then per-package)
        match tsc::run_tsc_declaration(&worktree_path, git_ref, &node_env) {
            Ok(tsc::TscOutcome::Success) => {
                // Full success — all packages compiled
            }
            Ok(tsc::TscOutcome::Partial { succeeded, failed }) => {
                // Partial success — try project build for better coverage
                tracing::warn!(
                    succeeded = succeeded,
                    failed = failed,
                    "tsc partial success, trying project build"
                );
                match tsc::run_project_build(&worktree_path, None, &node_env) {
                    Ok(()) => {
                        // Project build succeeded — should have better coverage now
                    }
                    Err(e) => {
                        // Project build also failed — proceed with partial tsc output
                        tracing::warn!(error = %e, succeeded = succeeded, "Project build fallback failed, proceeding with partial tsc output");
                        guard
                            .warnings
                            .push(ExtractionWarning::PartialTscBuildFailed {
                                succeeded,
                                failed,
                                build_error: e.to_string(),
                            });
                    }
                }
            }
            Err(e) => {
                // Total tsc failure — try project build as last resort
                tracing::warn!(error = %e, "tsc failed completely, trying project build as fallback");
                match tsc::run_project_build(&worktree_path, None, &node_env) {
                    Ok(()) => {
                        // Project build succeeded as fallback
                        guard
                            .warnings
                            .push(ExtractionWarning::TscFailedBuildSucceeded {
                                tsc_error: e.to_string(),
                            });
                    }
                    Err(build_err) => {
                        // Both tsc and project build failed — fatal
                        tracing::warn!(error = %build_err, "Project build also failed");
                        return Err(e);
                    }
                }
            }
        }

        Ok(guard)
    }

    /// Create a worktree without installing dependencies or running tsc.
    ///
    /// This is useful for testing the RAII cleanup behavior, and as a
    /// building block for `new()`.
    pub fn create_only(repo: &Path, git_ref: &str) -> Result<Self, WorktreeError> {
        let repo = repo.canonicalize().map_err(|e| {
            WorktreeError::CommandFailed(format!(
                "Failed to canonicalize repo path {}: {}",
                repo.display(),
                e
            ))
        })?;
        let repo = repo.as_path();

        validate_git_repo(repo)?;
        validate_git_ref(repo, git_ref)?;

        let worktree_path = worktree_path_for(repo, git_ref);

        let mut guard = Self {
            repo_root: repo.to_path_buf(),
            worktree_path: worktree_path.clone(),
            git_ref: git_ref.to_string(),
            created: false,
            warnings: Vec::new(),
        };

        let parent = worktree_path
            .parent()
            .expect("worktree path should have a parent");
        std::fs::create_dir_all(parent)?;

        create_worktree(repo, git_ref, &worktree_path)?;
        guard.created = true;

        Ok(guard)
    }

    /// Non-fatal issues encountered during worktree setup.
    ///
    /// The caller should inspect these after a successful `new()` and
    /// record them on the `DegradationTracker` for the end-of-run summary.
    pub fn warnings(&self) -> &[ExtractionWarning] {
        &self.warnings
    }

    /// Path to the worktree directory.
    pub fn path(&self) -> &Path {
        &self.worktree_path
    }

    /// The git ref this worktree was created for.
    pub fn git_ref(&self) -> &str {
        &self.git_ref
    }

    /// Scan for and remove stale worktrees from previous crashed runs.
    ///
    /// Looks in `<tmp>/semver-worktrees/<repo-hash>/` for any existing
    /// directories and attempts to clean them up via `git worktree remove`.
    pub fn cleanup_stale(repo: &Path) -> Result<usize, WorktreeError> {
        let repo = repo.canonicalize().map_err(|e| {
            WorktreeError::CommandFailed(format!(
                "Failed to canonicalize repo path {}: {}",
                repo.display(),
                e
            ))
        })?;
        let repo = repo.as_path();
        let worktree_dir = semver_analyzer_core::git::worktree_dir_for(repo);
        if !worktree_dir.exists() {
            return Ok(0);
        }

        let mut cleaned = 0;
        let entries = std::fs::read_dir(&worktree_dir)?;

        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let path = entry.path();
                tracing::info!(path = %path.display(), "Cleaning up stale worktree");
                if remove_worktree(repo, &path).is_ok() {
                    cleaned += 1;
                } else {
                    // If git worktree remove fails, try force-removing the directory
                    let _ = std::fs::remove_dir_all(&path);
                    cleaned += 1;
                }
            }
        }

        // Remove the parent directory if it's now empty
        if std::fs::read_dir(&worktree_dir)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true)
        {
            let _ = std::fs::remove_dir(&worktree_dir);
        }

        Ok(cleaned)
    }
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        if self.created {
            if let Err(e) = remove_worktree(&self.repo_root, &self.worktree_path) {
                tracing::warn!(
                    path = %self.worktree_path.display(),
                    error = %e,
                    "Failed to remove worktree"
                );
                // Last resort: force remove the directory
                let _ = std::fs::remove_dir_all(&self.worktree_path);
            }
        }
    }
}

impl WorktreeAccess for WorktreeGuard {
    fn path(&self) -> &Path {
        &self.worktree_path
    }
}

/// Validate that the given path is a git repository.
fn validate_git_repo(repo: &Path) -> Result<(), WorktreeError> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(repo)
        .output()
        .map_err(|e| WorktreeError::CommandFailed(format!("Failed to run git: {e}")))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(WorktreeError::NotAGitRepo {
            path: repo.to_path_buf(),
        })
    }
}

/// Validate that a git ref exists in the repository.
fn validate_git_ref(repo: &Path, git_ref: &str) -> Result<(), WorktreeError> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", git_ref])
        .current_dir(repo)
        .output()
        .map_err(|e| WorktreeError::CommandFailed(format!("Failed to run git: {e}")))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(WorktreeError::RefNotFound {
            git_ref: git_ref.to_string(),
        })
    }
}

/// Create a git worktree at the given path for the given ref.
fn create_worktree(repo: &Path, git_ref: &str, worktree_path: &Path) -> Result<(), WorktreeError> {
    // Remove any existing directory at this path (stale from a previous run)
    if worktree_path.exists() {
        let _ = remove_worktree(repo, worktree_path);
        let _ = std::fs::remove_dir_all(worktree_path);
    }

    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            &worktree_path.to_string_lossy(),
            git_ref,
        ])
        .current_dir(repo)
        .output()
        .map_err(|e| {
            WorktreeError::CommandFailed(format!("Failed to run git worktree add: {e}"))
        })?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(WorktreeError::WorktreeCreationFailed {
            path: worktree_path.to_path_buf(),
            reason: stderr.trim().to_string(),
        })
    }
}

/// Remove a git worktree.
fn remove_worktree(repo: &Path, worktree_path: &Path) -> Result<(), WorktreeError> {
    let output = Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            &worktree_path.to_string_lossy(),
        ])
        .current_dir(repo)
        .output()
        .map_err(|e| {
            WorktreeError::CommandFailed(format!("Failed to run git worktree remove: {e}"))
        })?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(WorktreeError::WorktreeRemovalFailed {
            path: worktree_path.to_path_buf(),
            reason: stderr.trim().to_string(),
        })
    }
}

/// Run the package manager install command in the worktree directory.
fn run_package_install(
    worktree_dir: &Path,
    pm: PackageManager,
    node_env: &[(String, String)],
) -> Result<(), WorktreeError> {
    let (cmd, args) = pm.install_command(worktree_dir);
    let display_cmd = format!("{cmd} {}", args.join(" "));

    let output = Command::new(cmd)
        .args(args)
        .current_dir(worktree_dir)
        .envs(node_env.iter().map(|(k, v)| (k, v)))
        .output()
        .map_err(|e| WorktreeError::PackageInstallFailed {
            command: display_cmd.clone(),
            reason: format!("Failed to execute: {e}"),
        })?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(WorktreeError::PackageInstallFailed {
            command: display_cmd,
            reason: stderr.trim().to_string(),
        })
    }
}

/// Run a user-provided install command in the worktree directory.
fn run_custom_install(
    worktree_dir: &Path,
    install_cmd: &str,
    node_env: &[(String, String)],
) -> Result<(), WorktreeError> {
    tracing::info!(command = %install_cmd, "Running user-provided install command");

    let needs_shell =
        install_cmd.contains("&&") || install_cmd.contains("||") || install_cmd.contains(';');

    let output = if needs_shell {
        Command::new("sh")
            .args(["-c", install_cmd])
            .current_dir(worktree_dir)
            .envs(node_env.iter().map(|(k, v)| (k, v)))
            .output()
    } else {
        let parts: Vec<&str> = install_cmd.split_whitespace().collect();
        if parts.is_empty() {
            return Err(WorktreeError::PackageInstallFailed {
                command: install_cmd.to_string(),
                reason: "Empty install command".to_string(),
            });
        }
        Command::new(parts[0])
            .args(&parts[1..])
            .current_dir(worktree_dir)
            .envs(node_env.iter().map(|(k, v)| (k, v)))
            .output()
    }
    .map_err(|e| WorktreeError::PackageInstallFailed {
        command: install_cmd.to_string(),
        reason: format!("Failed to execute: {e}"),
    })?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(WorktreeError::PackageInstallFailed {
            command: install_cmd.to_string(),
            reason: stderr.trim().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    // -- Pure unit tests (no git needed) --

    #[test]
    fn sanitize_simple_ref() {
        assert_eq!(sanitize_ref_name("v1.0.0"), "v1.0.0");
    }

    #[test]
    fn sanitize_ref_with_slashes() {
        assert_eq!(sanitize_ref_name("feature/my-branch"), "feature_my-branch");
    }

    #[test]
    fn sanitize_ref_with_special_chars() {
        assert_eq!(
            sanitize_ref_name("ref:with*special?chars"),
            "ref_with_special_chars"
        );
    }

    #[test]
    fn sanitize_long_ref_truncated() {
        let long_ref = "a".repeat(150);
        let result = sanitize_ref_name(&long_ref);
        assert_eq!(result.len(), 100);
    }

    #[test]
    fn worktree_path_in_tmp_dir() {
        let repo = Path::new("/repos/my-project");
        let path = worktree_path_for(repo, "v1.0.0");
        // Should be in the system temp dir, not inside the repo
        assert!(!path.starts_with(repo));
        assert!(path.ends_with("v1.0.0"));
    }

    #[test]
    fn worktree_path_sanitizes_ref() {
        let repo = Path::new("/repos/my-project");
        let path = worktree_path_for(repo, "feature/branch");
        assert!(path.ends_with("feature_branch"));
        assert!(!path.starts_with(repo));
    }

    // -- Helper: create a temporary git repo with a commit and tag --

    /// Run a git command isolated from global/system config.
    ///
    /// Sets `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL=/dev/null` to
    /// prevent global settings (e.g. `commit.gpgsign=true`) from interfering
    /// with test repos, and disables GPG signing explicitly.
    fn run_git(repo: &Path, args: &[&str]) {
        let output = StdCommand::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed (exit {}):\nstdout: {}\nstderr: {}",
            args,
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn create_test_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let repo = dir.path();

        run_git(repo, &["init", "-b", "main"]);
        run_git(repo, &["config", "user.email", "test@test.com"]);
        run_git(repo, &["config", "user.name", "Test"]);
        run_git(repo, &["config", "commit.gpgsign", "false"]);

        std::fs::write(repo.join("file.txt"), "hello").unwrap();

        run_git(repo, &["add", "."]);
        run_git(repo, &["commit", "-m", "initial"]);
        run_git(repo, &["tag", "v1.0.0"]);

        dir
    }

    // -- Integration tests: worktree lifecycle --

    #[test]
    fn worktree_created_and_cleaned_up_on_drop() {
        let repo_dir = create_test_repo();
        let repo = repo_dir.path();

        let worktree_path;
        {
            let guard = WorktreeGuard::create_only(repo, "v1.0.0").unwrap();
            worktree_path = guard.path().to_path_buf();

            // Worktree should exist while guard is alive
            assert!(
                worktree_path.exists(),
                "worktree should exist after creation"
            );
            assert!(
                worktree_path.join("file.txt").exists(),
                "worktree should contain repo files"
            );
        }
        // Guard dropped here -- worktree should be removed

        assert!(
            !worktree_path.exists(),
            "worktree should be removed after guard is dropped"
        );
    }

    #[test]
    fn worktree_cleaned_up_on_early_drop() {
        let repo_dir = create_test_repo();
        let repo = repo_dir.path();

        let guard = WorktreeGuard::create_only(repo, "v1.0.0").unwrap();
        let worktree_path = guard.path().to_path_buf();
        assert!(worktree_path.exists());

        // Explicitly drop early (simulates error path / early return)
        drop(guard);

        assert!(
            !worktree_path.exists(),
            "worktree should be removed after explicit drop"
        );
    }

    #[test]
    fn cleanup_stale_removes_leftover_worktrees() {
        let repo_dir = create_test_repo();
        let repo = repo_dir.path();

        // Create a worktree, then "leak" it by forgetting the guard
        let guard = WorktreeGuard::create_only(repo, "v1.0.0").unwrap();
        let worktree_path = guard.path().to_path_buf();

        // Prevent Drop from running -- simulate a crash
        std::mem::forget(guard);
        assert!(worktree_path.exists(), "leaked worktree should still exist");

        // cleanup_stale should find and remove it
        let cleaned = WorktreeGuard::cleanup_stale(repo).unwrap();
        assert_eq!(cleaned, 1, "should have cleaned up 1 stale worktree");
        assert!(
            !worktree_path.exists(),
            "stale worktree should be removed after cleanup"
        );
    }

    #[test]
    fn cleanup_stale_returns_zero_when_nothing_to_clean() {
        let repo_dir = create_test_repo();
        let cleaned = WorktreeGuard::cleanup_stale(repo_dir.path()).unwrap();
        assert_eq!(cleaned, 0);
    }

    #[test]
    fn create_only_fails_for_nonexistent_ref() {
        let repo_dir = create_test_repo();
        let result = WorktreeGuard::create_only(repo_dir.path(), "nonexistent-ref");
        assert!(matches!(result, Err(WorktreeError::RefNotFound { .. })));
    }

    #[test]
    fn create_only_fails_for_non_git_dir() {
        let dir = TempDir::new().unwrap();
        let result = WorktreeGuard::create_only(dir.path(), "v1.0.0");
        assert!(matches!(result, Err(WorktreeError::NotAGitRepo { .. })));
    }

    #[test]
    fn git_ref_accessor_returns_correct_ref() {
        let repo_dir = create_test_repo();
        let guard = WorktreeGuard::create_only(repo_dir.path(), "v1.0.0").unwrap();
        assert_eq!(guard.git_ref(), "v1.0.0");
    }

    #[test]
    fn second_worktree_for_same_ref_replaces_stale() {
        let repo_dir = create_test_repo();
        let repo = repo_dir.path();

        // Create first worktree and leak it
        let guard1 = WorktreeGuard::create_only(repo, "v1.0.0").unwrap();
        let path1 = guard1.path().to_path_buf();
        std::mem::forget(guard1);
        assert!(path1.exists());

        // Creating a second worktree for the same ref should succeed
        // (it removes the stale one first)
        let guard2 = WorktreeGuard::create_only(repo, "v1.0.0").unwrap();
        assert!(guard2.path().exists());
        assert_eq!(guard2.path(), path1); // same path

        // Cleanup: let guard2 drop normally
    }

    /// Create a test repo under the current working directory with a lockfile.
    ///
    /// Uses `tempdir_in(".")` so we can derive a relative path via
    /// `strip_prefix` without changing the process-global CWD (which would
    /// be flaky under parallel test execution).
    fn create_test_repo_in_cwd() -> TempDir {
        let dir = tempfile::Builder::new()
            .prefix("test-repo-")
            .tempdir_in(".")
            .unwrap();
        let repo = dir.path();

        run_git(repo, &["init", "-b", "main"]);
        run_git(repo, &["config", "user.email", "test@test.com"]);
        run_git(repo, &["config", "user.name", "Test"]);
        run_git(repo, &["config", "commit.gpgsign", "false"]);

        std::fs::write(repo.join("file.txt"), "hello").unwrap();
        std::fs::write(repo.join("package-lock.json"), "{}").unwrap();
        std::fs::write(
            repo.join("package.json"),
            r#"{"name":"test","version":"1.0.0"}"#,
        )
        .unwrap();

        run_git(repo, &["add", "."]);
        run_git(repo, &["commit", "-m", "initial"]);
        run_git(repo, &["tag", "v1.0.0"]);

        dir
    }

    #[test]
    fn relative_repo_path_finds_lockfile_in_worktree() {
        // Regression test for #1: when repo is a relative path,
        // WorktreeGuard::new() must find the lockfile in the worktree it
        // created, not miss it because of a double-nested path.
        //
        // The repo is created under CWD so we can derive a relative path
        // via strip_prefix — no set_current_dir needed.
        let repo_dir = create_test_repo_in_cwd();
        let cwd = std::env::current_dir().unwrap();
        let relative_repo = repo_dir
            .path()
            .strip_prefix(&cwd)
            .expect("repo should be under CWD since we used tempdir_in(\".\")");

        // Call new() — the actual path from the bug report.
        // With the fix, PackageManager::detect() finds package-lock.json
        // and proceeds to `npm ci`, which fails in the test environment.
        // Without the fix, it fails with NoLockfileFound because git
        // created the worktree at a double-nested path.
        let result = WorktreeGuard::new(
            relative_repo,
            "v1.0.0",
            &crate::worktree::RefBuildConfig::default(),
        );

        if let Err(WorktreeError::NoLockfileFound { .. }) = result {
            panic!(
                "relative path caused double-nested worktree path; \
                 lockfile not found at expected location"
            );
        }
        // Any other outcome (PackageInstallFailed, TscFailed, NoTsconfigFound,
        // or even Ok) means the lockfile WAS found — the fix works.
    }
}
