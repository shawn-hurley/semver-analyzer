//! Git worktree management with RAII cleanup.
//!
//! Manages temporary worktrees for checking out git refs, installing
//! dependencies, and running tsc. Ensures cleanup on drop, panic, or SIGINT.

mod error;
mod guard;
mod package_manager;
mod tsc;

pub use error::WorktreeError;
pub use guard::WorktreeGuard;
pub use package_manager::PackageManager;
