//! Core types, traits, and diff engine for the semver-analyzer.
//!
//! This crate contains language-agnostic components:
//! - API surface types (`ApiSurface`, `Symbol`, etc.)
//! - Report types (`AnalysisReport`, `StructuralChange`, etc.)
//! - Traits for language-pluggable analysis (`Language`, `ApiExtractor`, etc.)
//! - The structural diff engine (`diff_surfaces`)

pub mod diff;
pub mod shared;
pub mod traits;
pub mod types;

pub use shared::*;
pub use traits::*;
pub use types::*;
