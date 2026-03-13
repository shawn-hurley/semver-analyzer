//! Error types for worktree operations.

use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur during worktree management.
#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("Not a git repository: {path}")]
    NotAGitRepo { path: PathBuf },

    #[error("Git ref does not exist: {git_ref}")]
    RefNotFound { git_ref: String },

    #[error("Failed to create git worktree at {path}: {reason}")]
    WorktreeCreationFailed { path: PathBuf, reason: String },

    #[error("Failed to remove git worktree at {path}: {reason}")]
    WorktreeRemovalFailed { path: PathBuf, reason: String },

    #[error("No lockfile found at ref {git_ref}. Expected one of: package-lock.json, yarn.lock, pnpm-lock.yaml")]
    NoLockfileFound { git_ref: String },

    #[error("Package install failed ({command}): {reason}")]
    PackageInstallFailed { command: String, reason: String },

    #[error("No tsconfig.json found at ref {git_ref}")]
    NoTsconfigFound { git_ref: String },

    #[error("tsconfig.json has noEmit: true, which conflicts with --declaration. Consider adding a separate tsconfig.build.json")]
    NoEmitConflict,

    #[error("tsc --declaration failed with {error_count} errors at ref {git_ref}: {reason}")]
    TscFailed {
        git_ref: String,
        error_count: usize,
        reason: String,
    },

    #[error("Dependencies not installed at ref {git_ref}. Import resolution errors in tsc output")]
    MissingDependencies { git_ref: String },

    #[error("Project references not built. Run tsc --build in the monorepo root first")]
    ProjectReferencesNotBuilt,

    #[error("Unsupported TypeScript syntax at ref {git_ref}: {reason}")]
    UnsupportedSyntax { git_ref: String, reason: String },

    #[error("Project build failed ({command}): {reason}")]
    ProjectBuildFailed { command: String, reason: String },

    #[error("Insufficient disk space: need approximately {needed_mb}MB, have {available_mb}MB")]
    InsufficientDiskSpace { needed_mb: u64, available_mb: u64 },

    #[error("Command execution failed: {0}")]
    CommandFailed(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
