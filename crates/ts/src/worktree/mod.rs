//! Git worktree management with RAII cleanup.
//!
//! Manages temporary worktrees for checking out git refs, installing
//! dependencies, and running tsc. Ensures cleanup on drop, panic, or SIGINT.

mod error;
mod guard;
pub mod nvm;
mod package_manager;
mod tsc;

pub use error::WorktreeError;
pub use guard::WorktreeGuard;
pub use package_manager::PackageManager;

/// Per-ref build configuration for worktree operations.
///
/// Carries Node.js version, install command overrides, and build command
/// overrides that may differ between the "from" and "to" refs.
#[derive(Debug, Clone, Default)]
pub struct RefBuildConfig {
    /// Node.js version to use (e.g., "18", "18.17.0", "lts/hydrogen").
    /// Resolved via nvm to a bin directory prepended to PATH.
    pub node_version: Option<String>,

    /// Override the install command (e.g., "npm ci", "yarn install --frozen-lockfile").
    /// Bypasses auto-detection from lockfiles.
    pub install_command: Option<String>,

    /// Override the build command (e.g., "yarn build").
    /// Replaces the default tsc invocation.
    pub build_command: Option<String>,
}

/// Non-fatal issues encountered during worktree setup.
///
/// These are captured on [`WorktreeGuard`] via `guard.warnings()` and
/// propagated to the `DegradationTracker` by the caller of `extract()`.
/// The per-package tsc failures stay as `tracing::warn!` for `--log-file`
/// visibility; only the aggregate outcome is captured here.
#[derive(Debug, Clone)]
pub enum ExtractionWarning {
    /// tsc partially succeeded — some packages compiled, others failed.
    /// The project build fallback also failed.
    PartialTscBuildFailed {
        succeeded: usize,
        failed: usize,
        build_error: String,
    },

    /// tsc completely failed but the project build succeeded as fallback.
    TscFailedBuildSucceeded { tsc_error: String },
}
