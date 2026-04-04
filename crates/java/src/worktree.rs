//! Lightweight git worktree management for Java extraction.
//!
//! Java source files are parsed directly — no build step, no package
//! manager, no declaration emit. We just need a checkout of the source
//! tree at a given git ref.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// RAII guard for a temporary git worktree.
///
/// Creates a detached worktree on construction, removes it on drop.
/// Much simpler than the TS version — no npm install, no tsc, no
/// package manager detection.
pub struct WorktreeGuard {
    repo_root: PathBuf,
    worktree_path: PathBuf,
    created: bool,
}

impl WorktreeGuard {
    /// Create a new worktree for the given git ref.
    pub fn new(repo: &Path, git_ref: &str) -> Result<Self> {
        // Validate repo
        let status = Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(repo)
            .output()
            .context("Failed to run git")?;
        if !status.status.success() {
            anyhow::bail!("{} is not a git repository", repo.display());
        }

        // Validate ref
        let status = Command::new("git")
            .args(["rev-parse", "--verify", git_ref])
            .current_dir(repo)
            .output()
            .context("Failed to validate git ref")?;
        if !status.status.success() {
            anyhow::bail!("Git ref '{}' not found", git_ref);
        }

        // Compute worktree path
        let sanitized = sanitize_ref(git_ref);
        let worktree_path = repo.join(".semver-worktrees").join(&sanitized);

        let mut guard = Self {
            repo_root: repo.to_path_buf(),
            worktree_path,
            created: false,
        };

        // Create parent directory
        if let Some(parent) = guard.worktree_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create worktree parent directory")?;
        }

        // Remove stale worktree at same path if it exists
        if guard.worktree_path.exists() {
            let _ = Command::new("git")
                .args([
                    "worktree",
                    "remove",
                    "--force",
                    &guard.worktree_path.to_string_lossy(),
                ])
                .current_dir(&guard.repo_root)
                .output();
            let _ = std::fs::remove_dir_all(&guard.worktree_path);
        }

        // Create worktree
        let output = Command::new("git")
            .args([
                "worktree",
                "add",
                "--detach",
                &guard.worktree_path.to_string_lossy(),
                git_ref,
            ])
            .current_dir(&guard.repo_root)
            .output()
            .context("Failed to create git worktree")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git worktree add failed: {}", stderr);
        }

        guard.created = true;
        Ok(guard)
    }

    /// Path to the worktree root.
    pub fn path(&self) -> &Path {
        &self.worktree_path
    }
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        if self.created {
            let result = Command::new("git")
                .args([
                    "worktree",
                    "remove",
                    "--force",
                    &self.worktree_path.to_string_lossy(),
                ])
                .current_dir(&self.repo_root)
                .output();

            if result.is_err() || result.is_ok_and(|o| !o.status.success()) {
                let _ = std::fs::remove_dir_all(&self.worktree_path);
            }
        }
    }
}

/// Sanitize a git ref for use as a directory name.
fn sanitize_ref(git_ref: &str) -> String {
    git_ref
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_ascii_control() => '_',
            c => c,
        })
        .take(100)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_ref() {
        assert_eq!(sanitize_ref("v3.2.0"), "v3.2.0");
        assert_eq!(sanitize_ref("feature/foo"), "feature_foo");
        assert_eq!(sanitize_ref("HEAD~1"), "HEAD~1");
    }
}
