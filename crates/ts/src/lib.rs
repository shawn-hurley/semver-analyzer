//! TypeScript/JavaScript support for the semver-analyzer.
//!
//! This crate provides the TypeScript-specific implementation of API surface
//! extraction, type canonicalization, worktree management, manifest diffing,
//! and BU pipeline components (diff parsing, test analysis).
//!
//! It depends on `semver-analyzer-core` for the shared types and traits.

pub mod call_graph;
pub mod canon;
pub mod diff_parser;
pub mod extract;
pub mod manifest;
pub mod test_analyzer;
pub mod worktree;

// Re-export key types for convenience
pub use call_graph::TsCallGraphBuilder;
pub use diff_parser::TsDiffParser;
pub use extract::OxcExtractor;
pub use test_analyzer::TsTestAnalyzer;
pub use worktree::WorktreeGuard;
