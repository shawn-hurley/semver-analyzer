//! Java support for the semver-analyzer.
//!
//! This crate provides the Java-specific implementation of API surface
//! extraction, manifest diffing, and BU pipeline components.
//!
//! It depends on `semver-analyzer-core` for the shared types and traits.
//!
//! ## Parser
//!
//! Uses `tree-sitter` + `tree-sitter-java` for source parsing. Unlike the
//! TypeScript crate (which requires `tsc --declaration` to produce `.d.ts`
//! files), Java source files are parsed directly — no build step needed.
//!
//! ## Manifest Support
//!
//! - `pom.xml` — parsed with `quick-xml`
//! - `build.gradle` / `build.gradle.kts` — regex-based extraction

pub mod cli;
pub mod language;
pub mod types;

// Extraction and analysis modules
pub mod diff_parser;
pub mod extract;
pub mod konveyor;
pub mod manifest;
pub mod report;
pub mod test_analyzer;
pub mod worktree;

// Re-export key types for convenience
pub use language::Java;
pub use types::{
    JavaAnnotation, JavaCategory, JavaEvidence, JavaManifestChangeType, JavaReportData,
    JavaSymbolData,
};
