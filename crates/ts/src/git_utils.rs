//! Shared git utility functions for the TypeScript crate.
//!
//! Re-exports from `semver_analyzer_core::git` for backwards compatibility.
//! All git operations are now centralized in the core crate.

pub use semver_analyzer_core::git::{git_diff_file, read_git_file};
