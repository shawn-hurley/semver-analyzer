//! RAII guard for git worktree lifecycle management.
//!
//! Creates a temporary worktree, installs dependencies, runs tsc,
//! and cleans up on drop (even on panic or early return).

use super::error::WorktreeError;
use super::package_manager::PackageManager;
use super::tsc;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The directory name under the repo root where worktrees are created.
const WORKTREE_DIR_NAME: &str = ".semver-worktrees";

/// RAII guard that manages a git worktree's lifecycle.
///
/// On construction: creates a worktree, installs dependencies, runs tsc.
/// On drop: removes the worktree (even on panic or early return).
pub struct WorktreeGuard {
    /// Path to the repository root.
    repo_root: PathBuf,

    /// Path to the created worktree directory.
    worktree_path: PathBuf,

    /// The git ref this worktree was created for.
    git_ref: String,

    /// Whether the worktree was successfully created (controls cleanup).
    created: bool,
}

impl WorktreeGuard {
    /// Create a new worktree for the given git ref, install dependencies,
    /// and run `tsc --declaration`.
    ///
    /// This is the primary entry point. It performs the full worktree lifecycle:
    /// 1. Validate the repo and ref
    /// 2. Create the worktree via `git worktree add`
    /// 3. Detect and run the package manager install
    /// 4. Run `tsc --declaration --emitDeclarationOnly`
    /// 5. If tsc fails partially, try the project build as a fallback
    ///
    /// An optional `build_command` can be provided to customize the build step.
    /// If not provided and tsc fails, the project's `build` script is tried.
    ///
    /// On any failure, the worktree is cleaned up before the error propagates.
    pub fn new(
        repo: &Path,
        git_ref: &str,
        build_command: Option<&str>,
    ) -> Result<Self, WorktreeError> {
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
        };

        // Ensure parent directory exists
        let parent = worktree_path
            .parent()
            .expect("worktree path should have a parent");
        std::fs::create_dir_all(parent)?;

        // Create the worktree
        create_worktree(repo, git_ref, &worktree_path)?;
        guard.created = true;

        // Detect and install dependencies
        let pm = PackageManager::detect(&worktree_path).ok_or_else(|| {
            WorktreeError::NoLockfileFound {
                git_ref: git_ref.to_string(),
            }
        })?;

        run_package_install(&worktree_path, pm)?;

        // If user provided a build command, run it instead of tsc
        if let Some(cmd) = build_command {
            tracing::info!("Running user-provided build command");
            tsc::run_project_build(&worktree_path, Some(cmd))?;
            return Ok(guard);
        }

        // Run tsc --declaration (tries solution tsconfig, then per-package)
        match tsc::run_tsc_declaration(&worktree_path, git_ref) {
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
                match tsc::run_project_build(&worktree_path, None) {
                    Ok(()) => {
                        // Project build succeeded — should have better coverage now
                    }
                    Err(e) => {
                        // Project build also failed — proceed with partial tsc output
                        tracing::warn!(error = %e, succeeded = succeeded, "Project build fallback failed, proceeding with partial tsc output");
                    }
                }
            }
            Err(e) => {
                // Total tsc failure — try project build as last resort
                tracing::warn!(error = %e, "tsc failed completely, trying project build as fallback");
                match tsc::run_project_build(&worktree_path, None) {
                    Ok(()) => {
                        // Project build succeeded
                    }
                    Err(build_err) => {
                        // Both tsc and project build failed
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
        validate_git_repo(repo)?;
        validate_git_ref(repo, git_ref)?;

        let worktree_path = worktree_path_for(repo, git_ref);

        let mut guard = Self {
            repo_root: repo.to_path_buf(),
            worktree_path: worktree_path.clone(),
            git_ref: git_ref.to_string(),
            created: false,
        };

        let parent = worktree_path
            .parent()
            .expect("worktree path should have a parent");
        std::fs::create_dir_all(parent)?;

        create_worktree(repo, git_ref, &worktree_path)?;
        guard.created = true;

        Ok(guard)
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
    /// Looks in `<repo>/.semver-worktrees/` for any existing directories
    /// and attempts to clean them up via `git worktree remove`.
    pub fn cleanup_stale(repo: &Path) -> Result<usize, WorktreeError> {
        let worktree_dir = repo.join(WORKTREE_DIR_NAME);
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

/// Generate a deterministic worktree path for a given ref.
///
/// Path format: `<repo>/.semver-worktrees/<sanitized-ref>`
///
/// The ref is sanitized by replacing `/` with `_` and removing
/// characters that are invalid in directory names.
pub fn worktree_path_for(repo: &Path, git_ref: &str) -> PathBuf {
    let sanitized = sanitize_ref_name(git_ref);
    repo.join(WORKTREE_DIR_NAME).join(sanitized)
}

/// Sanitize a git ref name for use as a directory name.
///
/// Replaces `/` with `_`, removes characters that could cause issues
/// in file paths, and truncates to a reasonable length.
pub fn sanitize_ref_name(git_ref: &str) -> String {
    let sanitized: String = git_ref
        .chars()
        .map(|c| match c {
            '/' | '\\' => '_',
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_ascii_control() => '_',
            c => c,
        })
        .collect();

    // Truncate to 100 chars to avoid path length issues
    if sanitized.len() > 100 {
        sanitized[..100].to_string()
    } else {
        sanitized
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
fn run_package_install(worktree_dir: &Path, pm: PackageManager) -> Result<(), WorktreeError> {
    let (cmd, args) = pm.install_command();

    let output = Command::new(cmd)
        .args(args)
        .current_dir(worktree_dir)
        .output()
        .map_err(|e| WorktreeError::PackageInstallFailed {
            command: format!("{cmd} {}", args.join(" ")),
            reason: format!("Failed to execute: {e}"),
        })?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(WorktreeError::PackageInstallFailed {
            command: format!("{cmd} {}", args.join(" ")),
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
    fn worktree_path_structure() {
        let repo = Path::new("/repos/my-project");
        let path = worktree_path_for(repo, "v1.0.0");
        assert_eq!(
            path,
            PathBuf::from("/repos/my-project/.semver-worktrees/v1.0.0")
        );
    }

    #[test]
    fn worktree_path_sanitizes_ref() {
        let repo = Path::new("/repos/my-project");
        let path = worktree_path_for(repo, "feature/branch");
        assert_eq!(
            path,
            PathBuf::from("/repos/my-project/.semver-worktrees/feature_branch")
        );
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
}
