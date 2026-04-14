//! Lightweight git worktree management for Java extraction.
//!
//! Java source files are parsed directly — no build step, no package
//! manager, no declaration emit. We just need a checkout of the source
//! tree at a given git ref.
//!
//! Delegates to `semver_analyzer_core::git::WorktreeGuard` for all git
//! plumbing. The core guard handles worktree creation, RAII cleanup,
//! stale worktree removal, and ref sanitization.

pub use semver_analyzer_core::git::WorktreeGuard;
