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
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Top-level directory name for worktrees in the system temp dir.
const WORKTREE_DIR_NAME: &str = "semver-worktrees";

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

// ── Deprecation commit co-change analysis ────────────────────────────

/// A commit that added files to a deprecated component directory.
#[derive(Debug, Clone)]
pub struct DeprecationCommit {
    /// Short commit SHA.
    pub sha: String,
    /// The deprecated component name extracted from the path
    /// (e.g., "Tile" from `deprecated/components/Tile/Tile.tsx`).
    pub component: String,
}

/// Find commits between `from_ref` and `to_ref` that added files to
/// `deprecated/components/` directories (i.e., commits that deprecated
/// a component).
///
/// Runs `git log --diff-filter=A` to find commits that added `.tsx`/`.ts`
/// source files to deprecated component directories. Returns a list of
/// `(sha, component_name)` pairs.
///
/// Returns an empty vec on any git failure (shallow clone, invalid refs, etc.).
pub fn find_deprecation_commits(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
) -> Vec<DeprecationCommit> {
    // Use git log with --diff-filter=A to find commits that ADDED files
    // to deprecated component directories. The --name-only flag gives us
    // the file paths so we can extract the component name.
    let output = Command::new("git")
        .args([
            "log",
            "--diff-filter=A",
            "--name-only",
            "--pretty=format:%h",
            &format!("{}..{}", from_ref, to_ref),
            "--",
            "*/deprecated/components/*/[A-Z]*.tsx",
            "*/deprecated/components/*/[A-Z]*.ts",
        ])
        .current_dir(repo)
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::debug!(
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "git log for deprecation commits returned non-zero"
            );
            return vec![];
        }
        Err(e) => {
            tracing::debug!(%e, "Failed to run git log for deprecation commits");
            return vec![];
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut result = Vec::new();
    let mut current_sha = String::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Lines that don't contain '/' are commit SHAs from --pretty=format:%h
        if !line.contains('/') {
            current_sha = line.to_string();
            continue;
        }

        // File path line — extract the component name from
        // "*/deprecated/components/<ComponentName>/<file>"
        if current_sha.is_empty() {
            continue;
        }

        if let Some(component) = extract_component_from_deprecated_path(line) {
            // Avoid duplicates: same commit may add multiple files for one component
            if !result
                .iter()
                .any(|dc: &DeprecationCommit| dc.sha == current_sha && dc.component == component)
            {
                result.push(DeprecationCommit {
                    sha: current_sha.clone(),
                    component,
                });
            }
        }
    }

    result
}

/// Extract a component name from a deprecated component file path.
///
/// Looks for the pattern `deprecated/components/<Name>/` and returns `<Name>`.
/// Returns `None` if the path doesn't match the expected pattern.
fn extract_component_from_deprecated_path(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == "deprecated" && i + 2 < parts.len() && parts[i + 1] == "components" {
            return Some(parts[i + 2].to_string());
        }
    }
    None
}

/// Find component families whose source files were modified in the given commit.
///
/// Runs `git show --name-status --diff-filter=AM` to find Added or Modified files.
/// Filters to source files (`.tsx`/`.ts`) in non-deprecated `components/` directories,
/// excluding index files, tests, examples, docs, snapshots, and CSS.
///
/// Returns a deduplicated list of component family names (e.g., `["Card"]`).
/// The `deprecated_family` parameter is excluded from results (to avoid
/// self-matches), as are same-name families (already handled by Phase A.5).
pub fn commit_co_changed_families(
    repo: &Path,
    commit_sha: &str,
    deprecated_family: &str,
) -> Vec<String> {
    let output = Command::new("git")
        .args([
            "show",
            "--name-only",
            "--diff-filter=AM",
            "--pretty=format:",
            commit_sha,
        ])
        .current_dir(repo)
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::debug!(
                sha = commit_sha,
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "git show for commit co-change returned non-zero"
            );
            return vec![];
        }
        Err(e) => {
            tracing::debug!(%e, sha = commit_sha, "Failed to run git show for co-change");
            return vec![];
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut families = std::collections::HashSet::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Must be in a non-deprecated components/ directory
        if !line.contains("/components/") || line.contains("/deprecated/") {
            continue;
        }

        // Must be a source file (.tsx or .ts)
        if !line.ends_with(".tsx") && !line.ends_with(".ts") {
            continue;
        }

        // Exclude non-source files
        if line.contains("/examples/")
            || line.contains("/__tests__/")
            || line.contains("__snapshots__")
            || line.ends_with(".test.tsx")
            || line.ends_with(".test.ts")
            || line.ends_with(".spec.tsx")
            || line.ends_with(".spec.ts")
            || line.ends_with(".css")
            || line.ends_with(".md")
            || line.ends_with(".snap")
        {
            continue;
        }

        // Exclude index/barrel files
        let filename = line.rsplit('/').next().unwrap_or("");
        if filename == "index.ts" || filename == "index.tsx" {
            continue;
        }

        // Extract the component family name from the path:
        // "packages/.../components/<FamilyName>/FileName.tsx" → "FamilyName"
        if let Some(family) = extract_family_from_components_path(line) {
            // Exclude the deprecated family itself and same-name families
            if family != deprecated_family {
                families.insert(family);
            }
        }
    }

    families.into_iter().collect()
}

/// Extract a component family name from a non-deprecated components path.
///
/// Looks for the pattern `components/<Name>/` and returns `<Name>`.
fn extract_family_from_components_path(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == "components" && i + 1 < parts.len() {
            // Make sure this isn't under deprecated/
            if i > 0 && parts[i - 1] == "deprecated" {
                continue;
            }
            return Some(parts[i + 1].to_string());
        }
    }
    None
}

// ── Git-based component rename detection ─────────────────────────────

/// A component-level rename detected from git file renames.
#[derive(Debug, Clone)]
pub struct GitComponentRename {
    /// Component directory in the old version (e.g., "NotAuthorized").
    pub old_component: String,
    /// Component directory in the new version (e.g., "UnauthorizedAccess").
    pub new_component: String,
    /// The git rename similarity percentage (0-100).
    pub similarity: u32,
}

/// Detect component-level renames by scanning per-commit git rename
/// detection from `to_ref` back to `from_ref`.
///
/// Per-commit scanning produces much higher similarity scores than
/// cumulative `git diff` because each commit changes fewer files.
/// For example, `NotAuthorized → UnauthorizedAccess` shows R061 in
/// cumulative diff but R090 in the specific rename commit.
///
/// The algorithm:
/// 1. List commits from `from_ref` to `to_ref`
/// 2. For each commit (newest first), run `git diff --name-status --find-renames`
///    against its parent
/// 3. Parse `R` entries to extract directory-level component renames
/// 4. Deduplicate: keep the highest-similarity entry per (old_dir, new_dir) pair
///
/// Returns an empty vec on any git failure.
pub fn detect_git_component_renames(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
) -> Vec<GitComponentRename> {
    // Get commit list from from_ref to to_ref (oldest to newest)
    let rev_output = Command::new("git")
        .args([
            "rev-list",
            "--reverse",
            &format!("{}..{}", from_ref, to_ref),
        ])
        .current_dir(repo)
        .output();

    let rev_output = match rev_output {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::debug!(
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "git rev-list for component renames returned non-zero"
            );
            return vec![];
        }
        Err(e) => {
            tracing::debug!(%e, "Failed to run git rev-list for component renames");
            return vec![];
        }
    };

    let commits_str = String::from_utf8_lossy(&rev_output.stdout);
    let commits: Vec<&str> = commits_str.lines().filter(|l| !l.trim().is_empty()).collect();

    if commits.is_empty() {
        return vec![];
    }

    tracing::debug!(
        count = commits.len(),
        "Scanning commits for git component renames"
    );

    // Track best rename per (old_dir, new_dir) pair
    let mut best: std::collections::HashMap<(String, String), GitComponentRename> =
        std::collections::HashMap::new();

    for commit_sha in &commits {
        // Diff this commit against its parent
        let diff_output = Command::new("git")
            .args([
                "diff",
                "--name-status",
                "--find-renames",
                &format!("{}~1..{}", commit_sha, commit_sha),
            ])
            .current_dir(repo)
            .output();

        let diff_output = match diff_output {
            Ok(o) if o.status.success() => o,
            // Skip commits that fail (e.g., initial commit has no parent)
            _ => continue,
        };

        let stdout = String::from_utf8_lossy(&diff_output.stdout);

        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 3 {
                continue;
            }

            // Only process R (rename) entries: "R090\told_path\tnew_path"
            let status = parts[0];
            if !status.starts_with('R') {
                continue;
            }

            // Parse similarity from status (e.g., "R090" → 90)
            let similarity: u32 = status[1..].parse().unwrap_or(0);

            let old_path = parts[1];
            let new_path = parts[2];

            // Only consider source files (component files start with uppercase)
            let old_filename = old_path.rsplit('/').next().unwrap_or("");
            let new_filename = new_path.rsplit('/').next().unwrap_or("");
            if !is_component_source_file(old_filename)
                || !is_component_source_file(new_filename)
            {
                continue;
            }

            // Extract parent directories
            let old_dir = match old_path.rsplit_once('/') {
                Some((dir, _)) => dir,
                None => continue,
            };
            let new_dir = match new_path.rsplit_once('/') {
                Some((dir, _)) => dir,
                None => continue,
            };

            // Only interesting if directories are different (actual component move)
            if old_dir == new_dir {
                continue;
            }

            // Extract component names from directory paths
            // e.g., "packages/module/src/NotAuthorized" → "NotAuthorized"
            let old_component = old_dir.rsplit('/').next().unwrap_or(old_dir);
            let new_component = new_dir.rsplit('/').next().unwrap_or(new_dir);

            // Component names should start with uppercase
            if !old_component.starts_with(|c: char| c.is_ascii_uppercase())
                || !new_component.starts_with(|c: char| c.is_ascii_uppercase())
            {
                continue;
            }

            let key = (old_dir.to_string(), new_dir.to_string());
            let entry = best.entry(key).or_insert_with(|| GitComponentRename {
                old_component: old_component.to_string(),
                new_component: new_component.to_string(),
                similarity: 0,
            });

            // Keep highest similarity
            if similarity > entry.similarity {
                entry.similarity = similarity;
            }
        }
    }

    let result: Vec<GitComponentRename> = best.into_values().collect();

    if !result.is_empty() {
        for rename in &result {
            tracing::info!(
                old = %rename.old_component,
                new = %rename.new_component,
                similarity = rename.similarity,
                "Git component rename detected"
            );
        }
    }

    result
}

/// Check if a filename looks like a component source file.
/// Must end with .tsx/.ts and start with an uppercase letter.
fn is_component_source_file(filename: &str) -> bool {
    (filename.ends_with(".tsx") || filename.ends_with(".ts"))
        && !filename.ends_with(".test.tsx")
        && !filename.ends_with(".test.ts")
        && !filename.ends_with(".spec.tsx")
        && !filename.ends_with(".spec.ts")
        && !filename.ends_with(".d.ts")
        && filename != "index.ts"
        && filename != "index.tsx"
        && filename.starts_with(|c: char| c.is_ascii_uppercase())
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

/// Compute a deterministic hash for a repo path.
///
/// Used to create unique worktree directories per repo in the system
/// temp dir. Two runs against the same repo produce the same hash.
fn repo_hash(repo: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    repo.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Generate a deterministic worktree path for a given ref.
///
/// Path format: `<tmp>/semver-worktrees/<repo-hash>/<sanitized-ref>`
///
/// Worktrees are placed in the system temp dir rather than inside the
/// repo to avoid polluting the working tree. On crash, orphaned
/// worktrees sit in `/tmp/` where the OS cleans them up on reboot.
/// The repo hash ensures different repos don't collide.
pub fn worktree_path_for(repo: &Path, git_ref: &str) -> PathBuf {
    let sanitized = sanitize_ref_name(git_ref);
    std::env::temp_dir()
        .join(WORKTREE_DIR_NAME)
        .join(repo_hash(repo))
        .join(sanitized)
}

/// Return the parent directory for all worktrees of a given repo.
///
/// Path format: `<tmp>/semver-worktrees/<repo-hash>/`
pub fn worktree_dir_for(repo: &Path) -> PathBuf {
    std::env::temp_dir()
        .join(WORKTREE_DIR_NAME)
        .join(repo_hash(repo))
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
    /// Looks in `<tmp>/semver-worktrees/<repo-hash>/` for any existing
    /// directories and attempts to clean them up via `git worktree remove`.
    pub fn cleanup_stale(repo: &Path) -> Result<usize> {
        let repo = repo
            .canonicalize()
            .with_context(|| format!("Failed to canonicalize repo path: {}", repo.display()))?;
        let repo = repo.as_path();
        let worktree_dir = worktree_dir_for(repo);
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
    fn worktree_path_in_tmp_dir() {
        let repo = Path::new("/repos/my-project");
        let path = worktree_path_for(repo, "v1.0.0");
        let expected = std::env::temp_dir()
            .join("semver-worktrees")
            .join(repo_hash(repo))
            .join("v1.0.0");
        assert_eq!(path, expected);
    }

    #[test]
    fn worktree_path_sanitizes_ref() {
        let repo = Path::new("/repos/my-project");
        let path = worktree_path_for(repo, "feature/branch");
        assert!(path.ends_with("feature_branch"));
        // Verify it's in the tmp dir, not the repo
        assert!(!path.starts_with(repo));
    }

    #[test]
    fn worktree_path_deterministic_per_repo() {
        let repo = Path::new("/repos/my-project");
        let path1 = worktree_path_for(repo, "v1.0.0");
        let path2 = worktree_path_for(repo, "v1.0.0");
        assert_eq!(path1, path2);
    }

    #[test]
    fn worktree_path_different_repos_differ() {
        let repo_a = Path::new("/repos/project-a");
        let repo_b = Path::new("/repos/project-b");
        let path_a = worktree_path_for(repo_a, "v1.0.0");
        let path_b = worktree_path_for(repo_b, "v1.0.0");
        assert_ne!(path_a, path_b);
    }

    // ── Deprecation commit co-change analysis tests ─────────────────

    #[test]
    fn extract_component_from_deprecated_path_standard() {
        assert_eq!(
            extract_component_from_deprecated_path(
                "packages/react-core/src/deprecated/components/Tile/Tile.tsx"
            ),
            Some("Tile".to_string())
        );
    }

    #[test]
    fn extract_component_from_deprecated_path_nested() {
        assert_eq!(
            extract_component_from_deprecated_path(
                "packages/react-core/src/deprecated/components/Modal/ModalBox.tsx"
            ),
            Some("Modal".to_string())
        );
    }

    #[test]
    fn extract_component_from_deprecated_path_non_deprecated() {
        assert_eq!(
            extract_component_from_deprecated_path(
                "packages/react-core/src/components/Card/Card.tsx"
            ),
            None
        );
    }

    #[test]
    fn extract_family_from_components_path_standard() {
        assert_eq!(
            extract_family_from_components_path(
                "packages/react-core/src/components/Card/CardHeader.tsx"
            ),
            Some("Card".to_string())
        );
    }

    #[test]
    fn extract_family_from_components_path_excludes_deprecated() {
        // Should not match deprecated/components paths
        assert_eq!(
            extract_family_from_components_path(
                "packages/react-core/src/deprecated/components/Tile/Tile.tsx"
            ),
            None
        );
    }

    #[test]
    fn extract_family_from_components_path_no_match() {
        assert_eq!(
            extract_family_from_components_path("packages/react-core/src/helpers/util.ts"),
            None
        );
    }
}
