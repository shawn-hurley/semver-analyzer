//! TypeScript/JavaScript support for the semver-analyzer.
//!
//! This crate provides the TypeScript-specific implementation of API surface
//! extraction, type canonicalization, worktree management, manifest diffing,
//! and BU pipeline components (diff parsing, test analysis).
//!
//! It depends on `semver-analyzer-core` for the shared types and traits.

pub mod call_graph;
pub mod canon;
pub mod css_scan;
pub mod diff_parser;
pub mod extract;
pub mod jsx_diff;
pub mod konveyor;
pub mod language;
pub mod manifest;
pub mod report;
pub mod test_analyzer;
pub mod worktree;

// Re-export key types for convenience
pub use extract::OxcExtractor;
pub use language::{TsCategory, TsEvidence, TsManifestChangeType, TsReportData, TypeScript};
pub use worktree::WorktreeGuard;
