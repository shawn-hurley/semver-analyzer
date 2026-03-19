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
    AddedComponent,
    AnalysisMetadata,
    AnalysisReport,
    ApiChange,
    ApiChangeKind,
    ApiChangeType,
    ApiSurface,
    // BU pipeline types
    BehavioralBreak,
    BehavioralCategory,
    BehavioralChange,
    BehavioralChangeKind,
    BreakingVerdict,
    Caller,
    ChangedFunction,
    ChildComponent,
    ChildComponentStatus,
    Comparison,
    ComponentStatus,
    CompositionPatternChange,
    ComponentSummary,
    ConstantGroup,
    Dependent,
    ErrorBehavior,
    EvidenceSource,
    FileChanges,
    FileStatus,
    FunctionSpec,
    ImpactAnalysis,
    InferenceMetadata,
    InferredConstantPattern,
    InferredInterfaceMapping,
    InferredRenamePatterns,
    JsxChange,
    LlmUsage,
    ManifestChange,
    ManifestChangeType,
    MemberMapping,
    MigrationTarget,
    PackageChanges,
    Parameter,
    Postcondition,
    Precondition,
    PropertySummary,
    Reference,
    RemovalDisposition,
    RemovedProperty,
    SideEffect,
    Signature,
    StructuralChange,
    StructuralChangeType,
    SuffixRename,
    Summary,
    Symbol,
    SymbolKind,
    TestConvention,
    TestDiff,
    TestFile,
    TypeChange,
    TypeParameter,
    Visibility,
};
