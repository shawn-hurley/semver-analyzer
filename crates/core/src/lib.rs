//! Core types, traits, and diff engine for the semver-analyzer.
//!
//! This crate contains language-agnostic components:
//! - API surface types (`ApiSurface`, `Symbol`, etc.)
//! - Report types (`AnalysisReport`, `StructuralChange`, etc.)
//! - The `ApiExtractor` trait (implemented by language-specific crates)
//! - The structural diff engine (`diff_surfaces`)

pub mod diff;
pub mod shared;
pub mod traits;
pub mod types;

// Re-export traits for convenience
pub use traits::{ApiExtractor, BehaviorAnalyzer, CallGraphBuilder, DiffParser, TestAnalyzer};

// Re-export shared state
pub use shared::{BuReceiver, SharedFindings};
pub use types::{
    // Surface types (TD pipeline)
    AccessorKind,
    // Report types (output)
    AnalysisMetadata,
    AnalysisReport,
    ApiChange,
    ApiChangeKind,
    ApiChangeType,
    ApiSurface,
    // BU pipeline types
    BehavioralBreak,
    BehavioralChange,
    BehavioralChangeKind,
    BreakingVerdict,
    Caller,
    ChangedFunction,
    Comparison,
    Dependent,
    ErrorBehavior,
    EvidenceSource,
    FileChanges,
    FileStatus,
    FunctionSpec,
    ImpactAnalysis,
    LlmUsage,
    ManifestChange,
    ManifestChangeType,
    Parameter,
    Postcondition,
    Precondition,
    Reference,
    SideEffect,
    Signature,
    StructuralChange,
    StructuralChangeType,
    Summary,
    Symbol,
    SymbolKind,
    TestConvention,
    TestDiff,
    TestFile,
    TypeParameter,
    Visibility,
};
