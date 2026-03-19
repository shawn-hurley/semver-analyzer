//! Output data structures for the analysis report.
//!
//! These define the JSON schema of the tool's output, matching the v2 harness
//! format. The format is designed to be consumed by both humans (via agents/CI)
//! and machines (via MCP).
//!
//! Key design: changes are grouped **per-file**, with only files containing
//! breaking changes included in the output. Each file entry has separate
//! arrays for API and behavioral breaking changes.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Top-level analysis report (v2 harness format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisReport {
    /// Path to the analyzed repository.
    pub repository: PathBuf,

    /// Git comparison metadata.
    pub comparison: Comparison,

    /// Summary counts.
    pub summary: Summary,

    /// Per-file changes, sorted alphabetically by file path.
    /// Only files with at least one breaking change are included.
    pub changes: Vec<FileChanges>,

    /// Package manifest changes (package.json).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub manifest_changes: Vec<ManifestChange>,

    /// Files added between from_ref and to_ref (new exports, new components).
    /// Used to detect new sibling components that may be needed alongside
    /// modified components.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_files: Vec<PathBuf>,

    /// Metadata about the analysis run.
    pub metadata: AnalysisMetadata,
}

/// Git comparison metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comparison {
    pub from_ref: String,
    pub to_ref: String,
    pub from_sha: String,
    pub to_sha: String,
    pub commit_count: usize,
    pub analysis_timestamp: String,
}

/// Summary counts for the report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub total_breaking_changes: usize,
    pub breaking_api_changes: usize,
    pub breaking_behavioral_changes: usize,
    pub files_with_breaking_changes: usize,
}

/// All breaking changes within a single file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChanges {
    /// Path to the file relative to repository root.
    pub file: PathBuf,

    /// Git status of the file.
    pub status: FileStatus,

    /// Original file path if status is renamed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<PathBuf>,

    /// Breaking changes to public/exported symbols.
    pub breaking_api_changes: Vec<ApiChange>,

    /// Breaking behavioral changes (DOM structure, CSS, defaults, rendering).
    pub breaking_behavioral_changes: Vec<BehavioralChange>,
}

/// Git file status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// A breaking API change detected by structural analysis (TD pipeline).
///
/// Follows the v2 harness schema: symbol uses `Component.propName` format,
/// kind maps to a fixed set of categories, and change classifies the type
/// of breaking change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiChange {
    /// Symbol name: `ComponentName` for component-level, `ComponentName.propName`
    /// for prop-level changes.
    pub symbol: String,

    /// The kind of symbol.
    pub kind: ApiChangeKind,

    /// What kind of breaking change occurred.
    pub change: ApiChangeType,

    /// The symbol's signature/definition before the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,

    /// The symbol's signature/definition after the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,

    /// Human-readable description of what broke and how it affects consumers.
    pub description: String,

    /// Migration target metadata when a replacement has been detected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration_target: Option<MigrationTarget>,
}

/// Kind of symbol affected by an API change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiChangeKind {
    Function,
    Method,
    Class,
    #[serde(rename = "struct")]
    Struct,
    Interface,
    #[serde(rename = "trait")]
    Trait,
    TypeAlias,
    Constant,
    Field,
    Property,
    ModuleExport,
}

/// Type of breaking API change.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiChangeType {
    Removed,
    SignatureChanged,
    TypeChanged,
    VisibilityChanged,
    Renamed,
}

/// A behavioral change detected by BU analysis (possibly LLM-assisted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehavioralChange {
    /// The function/method/class where the behavioral change occurs.
    pub symbol: String,

    /// The kind of symbol.
    pub kind: BehavioralChangeKind,

    /// Sub-category of the behavioral change (DOM, CSS, a11y, etc.).
    /// When present, enables more precise Konveyor rule generation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<BehavioralCategory>,

    /// What was happening before and what happens now.
    pub description: String,

    /// Source file path (used internally for grouping, not in v2 output).
    #[serde(skip)]
    pub source_file: Option<String>,
}

/// Kind of symbol for behavioral changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BehavioralChangeKind {
    Function,
    Method,
    Class,
    Module,
}

/// Sub-category of a behavioral breaking change.
///
/// Enables downstream tools (Konveyor rule generation, fix guidance)
/// to produce targeted rules and labels like `change-type=dom-structure`
/// or `impact=frontend-testing`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BehavioralCategory {
    /// Changed element types, added/removed wrapper elements, altered
    /// component nesting structure.
    DomStructure,
    /// CSS class name renames, removals, or changed class application logic.
    CssClass,
    /// CSS custom property (variable) renames or removals.
    CssVariable,
    /// ARIA attribute changes, role changes, keyboard navigation, focus
    /// management, tab order changes.
    Accessibility,
    /// Changed default prop values, altered conditional logic, changed
    /// return values for same inputs.
    DefaultValue,
    /// Changed event handling, state management, side effects.
    LogicChange,
    /// Changed data-ouia-*, data-testid, or other data attributes.
    DataAttribute,
    /// General render output change not covered by other categories.
    RenderOutput,
}

// ── Internal types used during diff computation ─────────────────────────
// These are NOT part of the v2 output schema but are used internally
// by the diff engine to compute changes before they are mapped to
// ApiChange entries.

/// A structural change detected by the diff engine.
///
/// This is the internal representation. The `build_report` function in
/// the binary crate converts these to `ApiChange` entries for the output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuralChange {
    /// The affected symbol name.
    pub symbol: String,

    /// Fully qualified symbol name.
    pub qualified_name: String,

    /// Symbol kind (function, class, interface, etc.).
    pub kind: String,

    /// What type of structural change this is.
    pub change_type: StructuralChangeType,

    /// The symbol's signature/definition before the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,

    /// The symbol's signature/definition after the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,

    /// Human-readable description of the change.
    pub description: String,

    /// Whether this change is breaking.
    pub is_breaking: bool,

    /// Impact analysis: what code depends on this symbol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impact: Option<ImpactAnalysis>,

    /// Migration target: a suggested replacement for a removed symbol,
    /// detected via same-directory member overlap analysis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration_target: Option<MigrationTarget>,
}

/// Categories of structural changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuralChangeType {
    // Symbol-level
    SymbolRemoved,
    SymbolAdded,
    SymbolRenamed,
    SymbolMovedToDeprecated,

    // Parameter changes
    ParameterAdded,
    ParameterRemoved,
    ParameterTypeChanged,
    ParameterMadeRequired,
    ParameterMadeOptional,
    ParameterDefaultValueChanged,
    RestParameterAdded,
    RestParameterRemoved,

    // Return type changes
    ReturnTypeChanged,
    MadeAsync,
    MadeSync,

    // Visibility changes
    VisibilityReduced,
    VisibilityIncreased,

    // Generic type parameter changes
    TypeParameterAdded,
    TypeParameterRemoved,
    TypeParameterReordered,
    TypeParameterConstraintChanged,
    TypeParameterDefaultChanged,

    // Property modifier changes
    ReadonlyAdded,
    ReadonlyRemoved,
    AbstractAdded,
    AbstractRemoved,
    StaticInstanceChanged,
    AccessorKindChanged,

    // Class hierarchy changes
    BaseClassChanged,
    InterfaceImplementationAdded,
    InterfaceImplementationRemoved,

    // Enum changes
    EnumMemberAdded,
    EnumMemberRemoved,
    EnumMemberValueChanged,

    // Interface/type changes
    PropertyAdded,
    PropertyRemoved,
    PropertyRenamed,

    // Union literal value changes (e.g., 'primary' | 'secondary' → 'primary' | 'danger')
    UnionMemberRemoved,
    UnionMemberAdded,

    // Special
    ThisParameterTypeChanged,

    // Structural migration suggestions
    /// A removed symbol has a likely replacement in the same component directory,
    /// detected via member name overlap analysis.
    MigrationSuggested,
}

impl StructuralChangeType {
    /// Map internal change type to v2 harness API change type.
    pub fn to_api_change_type(&self) -> ApiChangeType {
        match self {
            Self::SymbolRemoved
            | Self::ParameterRemoved
            | Self::RestParameterRemoved
            | Self::PropertyRemoved
            | Self::EnumMemberRemoved
            | Self::InterfaceImplementationRemoved => ApiChangeType::Removed,

            Self::SymbolRenamed | Self::SymbolMovedToDeprecated | Self::PropertyRenamed => {
                ApiChangeType::Renamed
            }

            Self::ParameterAdded
            | Self::ParameterMadeRequired
            | Self::ParameterMadeOptional
            | Self::ParameterDefaultValueChanged
            | Self::RestParameterAdded
            | Self::MadeAsync
            | Self::MadeSync
            | Self::TypeParameterAdded
            | Self::TypeParameterRemoved
            | Self::TypeParameterReordered
            | Self::TypeParameterConstraintChanged
            | Self::TypeParameterDefaultChanged
            | Self::BaseClassChanged
            | Self::InterfaceImplementationAdded
            | Self::AbstractAdded
            | Self::AbstractRemoved
            | Self::StaticInstanceChanged
            | Self::AccessorKindChanged
            | Self::ReadonlyAdded
            | Self::ReadonlyRemoved
            | Self::ThisParameterTypeChanged
            | Self::PropertyAdded
            | Self::EnumMemberAdded
            | Self::EnumMemberValueChanged => ApiChangeType::SignatureChanged,

            Self::ParameterTypeChanged
            | Self::ReturnTypeChanged
            | Self::UnionMemberRemoved
            | Self::UnionMemberAdded => ApiChangeType::TypeChanged,

            Self::VisibilityReduced | Self::VisibilityIncreased => ApiChangeType::VisibilityChanged,

            Self::SymbolAdded => ApiChangeType::SignatureChanged,

            Self::MigrationSuggested => ApiChangeType::Removed,
        }
    }
}

/// A member-level mapping between a removed symbol and its suggested replacement.
///
/// When a removed interface/class has members that overlap with a surviving
/// interface in the same component directory, this records the mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberMapping {
    /// Name of the member in the removed interface.
    pub old_name: String,
    /// Name of the matching member in the replacement interface.
    pub new_name: String,
    /// Type annotation of the member in the removed interface (from `signature.return_type`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_type: Option<String>,
    /// Type annotation of the member in the replacement interface (from `signature.return_type`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_type: Option<String>,
}

/// A structural migration target detected by same-directory member overlap.
///
/// Produced by the migration detection phase in the diff engine. Records
/// that a removed symbol has a plausible replacement, including the specific
/// member-level overlap that signals the relationship.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationTarget {
    /// The removed symbol name (e.g., "EmptyStateHeaderProps").
    pub removed_symbol: String,
    /// Qualified name of the removed symbol.
    pub removed_qualified_name: String,
    /// The suggested replacement symbol name (e.g., "EmptyStateProps").
    pub replacement_symbol: String,
    /// Qualified name of the replacement symbol.
    pub replacement_qualified_name: String,
    /// Member names that overlap between removed and replacement.
    pub matching_members: Vec<MemberMapping>,
    /// Member names only in the removed symbol (no match in replacement).
    pub removed_only_members: Vec<String>,
    /// The ratio of overlap: |matching| / |removed.members|.
    pub overlap_ratio: f64,
}

/// Impact analysis: what code depends on a broken symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactAnalysis {
    /// Direct dependents within the repository.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub internal_dependents: Vec<Dependent>,

    /// Transitive dependents (reached through call chains).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub transitive_dependents: Vec<Dependent>,
}

/// A code location that depends on a broken symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependent {
    pub file: PathBuf,
    pub line: usize,
    pub symbol: String,
}

/// A breaking change in package.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestChange {
    /// What field changed.
    pub field: String,

    /// Change type.
    pub change_type: ManifestChangeType,

    /// Value before the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,

    /// Value after the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,

    /// Human-readable description.
    pub description: String,

    /// Whether this change is breaking.
    pub is_breaking: bool,
}

/// Categories of package.json changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestChangeType {
    EntryPointChanged,
    ExportsEntryRemoved,
    ExportsEntryAdded,
    ExportsConditionRemoved,
    ModuleSystemChanged,
    PeerDependencyAdded,
    PeerDependencyRemoved,
    PeerDependencyRangeChanged,
    EngineConstraintChanged,
    BinEntryRemoved,
}

/// Metadata about the analysis run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisMetadata {
    /// How the call graph was analyzed.
    pub call_graph_analysis: String,

    /// Tool version.
    pub tool_version: String,

    /// LLM usage statistics (None if --no-llm).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_usage: Option<LlmUsage>,
}

/// LLM usage statistics for cost tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmUsage {
    pub total_calls: usize,
    pub spec_inference_calls: usize,
    pub comparison_calls: usize,
    pub propagation_calls: usize,
    pub total_input_tokens: usize,
    pub total_output_tokens: usize,
    pub estimated_cost_usd: f64,
    pub circuit_breaker_triggered: bool,
}
