//! Language-agnostic git utilities.
//!
//! Provides shared git operations used across all language crates:
//! - File reading from git refs (`read_git_file`)
//! - File diffing between refs (`git_diff_file`)
//! - Ref name sanitization (`sanitize_ref_name`)
//! - Worktree path computation (`worktree_path_for`)
//! - RAII worktree management (`WorktreeGuard`)
//!
//! These utilities were consolidated from duplicate implementations in
//! `crates/ts/` and `crates/java/`. Language crates should use these
//! directly (or compose with `WorktreeGuard`) rather than reimplementing
//! git plumbing.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The directory name under the repo root where worktrees are created.
const WORKTREE_DIR_NAME: &str = ".semver-worktrees";

// ── Git file operations ──────────────────────────────────────────────

/// Read a file from a git ref via `git show <ref>:<path>`.
///
/// Returns `None` if the file doesn't exist at the given ref,
/// the git command fails, or the output is not valid UTF-8.
/// All failures are logged at `trace` level for debugging with
/// `--log-level trace --log-file debug.log`.
pub fn read_git_file(repo: &Path, git_ref: &str, file_path: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["show", &format!("{git_ref}:{file_path}")])
        .current_dir(repo)
        .output()
        .map_err(|e| {
            tracing::trace!(
                %e,
                repo = %repo.display(),
                %git_ref,
                %file_path,
                "git show failed to execute"
            );
            e
        })
        .ok()?;

    if !output.status.success() {
        tracing::trace!(
            repo = %repo.display(),
            %git_ref,
            %file_path,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "git show returned non-zero"
        );
        return None;
    }

    String::from_utf8(output.stdout)
        .map_err(|e| {
            tracing::trace!(
                %e,
                %file_path,
                "git show output was not valid UTF-8"
            );
            e
        })
        .ok()
}

/// Get the diff of a single file between two refs via `git diff <from>..<to> -- <path>`.
///
/// Returns `None` if the file has no changes between the refs,
/// the git command fails, or the output is empty.
/// All failures are logged at `trace` level.
pub fn git_diff_file(repo: &Path, from_ref: &str, to_ref: &str, file_path: &str) -> Option<String> {
    let output = Command::new("git")
        .args([
            "-C",
            &repo.to_string_lossy(),
            "diff",
            &format!("{from_ref}..{to_ref}"),
            "--",
            file_path,
        ])
        .output()
        .map_err(|e| {
            tracing::trace!(
                %e,
                repo = %repo.display(),
                %from_ref,
                %to_ref,
                %file_path,
                "git diff failed to execute"
            );
            e
        })
        .ok()?;

    if !output.status.success() {
        tracing::trace!(
            repo = %repo.display(),
            %from_ref,
            %to_ref,
            %file_path,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "git diff returned non-zero"
        );
        return None;
    }

    let content = String::from_utf8_lossy(&output.stdout).to_string();
    if content.is_empty() {
        None
    } else {
        Some(content)
    }
}

// ── Ref name utilities ───────────────────────────────────────────────

/// Sanitize a git ref name for use as a directory name.
///
/// Replaces characters that are invalid in file paths (`/`, `\`, `:`,
/// `*`, `?`, `"`, `<`, `>`, `|`) and ASCII control characters with `_`.
/// Truncates to 100 characters to avoid path length issues.
pub fn sanitize_ref_name(git_ref: &str) -> String {
    let sanitized: String = git_ref
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
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

/// Generate a deterministic worktree path for a given ref.
///
/// Path format: `<repo>/.semver-worktrees/<sanitized-ref>`
pub fn worktree_path_for(repo: &Path, git_ref: &str) -> PathBuf {
    let sanitized = sanitize_ref_name(git_ref);
    repo.join(WORKTREE_DIR_NAME).join(sanitized)
}

// ── WorktreeGuard ────────────────────────────────────────────────────

/// RAII guard for a temporary git worktree.
///
/// Creates a detached worktree on construction, removes it on drop.
/// This provides the language-agnostic foundation — just git checkout,
/// no build steps. Language crates that need build steps (npm install,
/// tsc, mvn compile) should compose with this guard:
///
/// ```ignore
/// // In a language crate:
/// pub struct TsWorktreeGuard {
///     inner: semver_analyzer_core::git::WorktreeGuard,
///     warnings: Vec<ExtractionWarning>,
/// }
/// ```
pub struct WorktreeGuard {
    repo_root: PathBuf,
    worktree_path: PathBuf,
    git_ref: String,
    created: bool,
}

impl WorktreeGuard {
    /// Create a new worktree for the given git ref.
    ///
    /// Validates the repository and ref, then creates a detached worktree
    /// at `<repo>/.semver-worktrees/<sanitized-ref>`. If a stale worktree
    /// exists at the same path, it is removed first.
    ///
    /// On drop, the worktree is automatically removed.
    pub fn new(repo: &Path, git_ref: &str) -> Result<Self> {
        let repo = repo
            .canonicalize()
            .with_context(|| format!("Failed to canonicalize repo path: {}", repo.display()))?;
        let repo = repo.as_path();

        validate_git_repo(repo)?;
        validate_git_ref(repo, git_ref)?;

        let worktree_path = worktree_path_for(repo, git_ref);

        let mut guard = Self {
            repo_root: repo.to_path_buf(),
            worktree_path,
            git_ref: git_ref.to_string(),
            created: false,
        };

        // Ensure parent directory exists
        if let Some(parent) = guard.worktree_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create worktree parent directory")?;
        }

        // Remove stale worktree at same path if it exists
        if guard.worktree_path.exists() {
            let _ = remove_worktree(repo, &guard.worktree_path);
            let _ = std::fs::remove_dir_all(&guard.worktree_path);
        }

        // Create worktree
        create_worktree(repo, git_ref, &guard.worktree_path)?;
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
    pub fn cleanup_stale(repo: &Path) -> Result<usize> {
        let repo = repo
            .canonicalize()
            .with_context(|| format!("Failed to canonicalize repo path: {}", repo.display()))?;
        let repo = repo.as_path();
        let worktree_dir = repo.join(WORKTREE_DIR_NAME);
        if !worktree_dir.exists() {
            return Ok(0);
        }

        let mut cleaned = 0;
        let entries =
            std::fs::read_dir(&worktree_dir).context("Failed to read worktree directory")?;

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

// ── Internal helpers ─────────────────────────────────────────────────

/// Validate that the given path is a git repository.
fn validate_git_repo(repo: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(repo)
        .output()
        .context("Failed to run git")?;

    if output.status.success() {
        Ok(())
    } else {
        anyhow::bail!("Not a git repository: {}", repo.display())
    }
}

/// Validate that a git ref exists in the repository.
fn validate_git_ref(repo: &Path, git_ref: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", git_ref])
        .current_dir(repo)
        .output()
        .context("Failed to validate git ref")?;

    if output.status.success() {
        Ok(())
    } else {
        anyhow::bail!("Git ref '{}' not found", git_ref)
    }
}

/// Create a git worktree at the given path for the given ref.
fn create_worktree(repo: &Path, git_ref: &str, worktree_path: &Path) -> Result<()> {
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
        .context("Failed to run git worktree add")?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git worktree add failed at {}: {}",
            worktree_path.display(),
            stderr.trim()
        )
    }
}

/// Remove a git worktree.
fn remove_worktree(repo: &Path, worktree_path: &Path) -> Result<()> {
    let output = Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            &worktree_path.to_string_lossy(),
        ])
        .current_dir(repo)
        .output()
        .context("Failed to run git worktree remove")?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git worktree remove failed at {}: {}",
            worktree_path.display(),
            stderr.trim()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
