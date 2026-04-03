//! TypeScript/JavaScript support for the semver-analyzer.
//!
//! This crate provides the TypeScript-specific implementation of API surface
//! extraction, type canonicalization, worktree management, manifest diffing,
//! and BU pipeline components (diff parsing, test analysis).
//!
//! It depends on `semver-analyzer-core` for the shared types and traits.

pub mod call_graph;
pub mod canon;
pub mod cli;
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

// ── v2 SD (Source-Level Diff) pipeline modules ──────────────────────────
pub mod composition;
pub mod css_profile;
pub mod konveyor_v2;
pub mod sd_pipeline;
pub mod source_profile;

// ── Language-specific types (moved from core) ───────────────────────────
pub mod extensions;
pub mod hierarchy_types;
pub mod sd_types;
pub mod symbol_data;

// Re-export key types for convenience
pub use extensions::TsAnalysisExtensions;
pub use extract::OxcExtractor;
pub use language::{TsCategory, TsEvidence, TsManifestChangeType, TsReportData, TypeScript};
pub use worktree::WorktreeGuard;
