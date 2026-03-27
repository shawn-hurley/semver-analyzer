//! Output data structures for the analysis report.
//!
//! These define the JSON schema of the tool's output, matching the v2 harness
//! format. The format is designed to be consumed by both humans (via agents/CI)
//! and machines (via MCP).
//!
//! Key design: changes are grouped **per-file**, with only files containing
//! breaking changes included in the output. Each file entry has separate
//! arrays for API and behavioral breaking changes.

use super::bu::EvidenceType;
use super::change_subject::ChangeSubject;
use crate::traits::Language;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Top-level analysis report (v2 harness format).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct AnalysisReport<L: Language> {
    /// Path to the analyzed repository.
    pub repository: PathBuf,

    /// Git comparison metadata.
    pub comparison: Comparison,

    /// Summary counts.
    pub summary: Summary,

    /// Per-file changes, sorted alphabetically by file path.
    /// Only files with at least one breaking change are included.
    pub changes: Vec<FileChanges<L>>,

    /// Package manifest changes (e.g., package.json, go.mod, pom.xml).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub manifest_changes: Vec<ManifestChange<L>>,

    /// Files added between from_ref and to_ref (new exports, new types).
    /// Used to detect new sibling types that may be needed alongside
    /// modified types.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_files: Vec<PathBuf>,

    /// Per-package hierarchical view of changes.
    ///
    /// Contains pre-aggregated type summaries, constant groups, and
    /// added exports. This is the primary data source for rule generation
    /// — all downstream processing reads from this field instead of
    /// reconstructing structure from the flat `changes` list.
    ///
    /// Populated by `build_report()` using the API surfaces (old + new)
    /// and the structural/behavioral change lists. Not serialized into
    /// the report when empty (backward compat with older reports).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<PackageChanges<L>>,

    /// Member-level rename mappings discovered during analysis.
    ///
    /// Maps old member names to new member names (e.g., CSS token renames).
    /// Surfaced as a top-level field so the rule generator doesn't need
    /// to re-scan the diff for rename patterns.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub member_renames: HashMap<String, String>,

    /// LLM-inferred rename patterns for constants and interfaces.
    /// Populated by the rename inference phase between TD and BU.
    /// None when --no-llm is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inferred_rename_patterns: Option<InferredRenamePatterns>,

    /// Hierarchy changes between versions, computed by diffing LLM-inferred
    /// component hierarchies from both refs. Each entry describes how a
    /// component's expected children changed (added/removed children,
    /// migrated props). None when --no-llm is set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hierarchy_deltas: Vec<HierarchyDelta>,

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
#[serde(bound = "")]
pub struct FileChanges<L: Language> {
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
    pub breaking_behavioral_changes: Vec<BehavioralChange<L>>,

    /// Container/nesting changes detected from test/example diffs.
    /// These describe how symbol containment structure changed between versions
    /// (e.g., MastheadToggle moved inside MastheadMain).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub container_changes: Vec<ContainerChange>,
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

    /// Why a removed prop was removed and where its functionality went.
    /// Populated from LLM behavioral analysis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removal_disposition: Option<RemovalDisposition>,

    /// The HTML element this component renders (e.g., "ol", "div", "footer").
    /// Used when a component is replaced by a generic component that needs
    /// an explicit element type prop (e.g., TextList→Content component="ol").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renders_element: Option<String>,
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

impl From<super::surface::SymbolKind> for ApiChangeKind {
    fn from(kind: super::surface::SymbolKind) -> Self {
        use super::surface::SymbolKind;
        match kind {
            SymbolKind::Function => ApiChangeKind::Function,
            SymbolKind::Method => ApiChangeKind::Method,
            SymbolKind::Class => ApiChangeKind::Class,
            SymbolKind::Struct => ApiChangeKind::Struct,
            SymbolKind::Interface => ApiChangeKind::Interface,
            SymbolKind::TypeAlias => ApiChangeKind::TypeAlias,
            SymbolKind::Constant | SymbolKind::Variable => ApiChangeKind::Constant,
            SymbolKind::Property => ApiChangeKind::Property,
            SymbolKind::Enum | SymbolKind::EnumMember => ApiChangeKind::Constant,
            SymbolKind::Constructor => ApiChangeKind::Method,
            SymbolKind::GetAccessor | SymbolKind::SetAccessor => ApiChangeKind::Property,
            SymbolKind::Namespace => ApiChangeKind::ModuleExport,
        }
    }
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
#[serde(bound = "")]
pub struct BehavioralChange<L: Language> {
    /// The function/method/class where the behavioral change occurs.
    pub symbol: String,

    /// The kind of symbol.
    pub kind: BehavioralChangeKind,

    /// Sub-category of the behavioral change (DOM, CSS, a11y, etc.).
    /// When present, enables more precise Konveyor rule generation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<L::Category>,

    /// What was happening before and what happens now.
    pub description: String,

    /// Source file path (used internally for grouping, not in v2 output).
    #[serde(skip)]
    pub source_file: Option<String>,

    /// Confidence score (0.0 to 1.0) from the BU pipeline.
    /// Higher values indicate more reliable detection (e.g., TestDelta = 0.95,
    /// JsxDiff = 0.90, LlmWithTestContext = 0.70, LlmOnly = 0.55).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,

    /// How the behavioral change was detected.
    /// One of: "TestDelta", "JsxDiff", "LlmOnly", "LlmWithTestContext".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_type: Option<EvidenceType>,

    /// Symbol names referenced in this behavioral change description.
    /// Pre-extracted so downstream code doesn't need regex parsing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub referenced_symbols: Vec<String>,

    /// Whether this change only affects internal rendering and does NOT
    /// require consumer code changes. Set by LLM analysis.
    /// When true, the fix engine should skip this change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_internal_only: Option<bool>,
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

/// A change in the containment/nesting structure of a symbol between versions.
/// For React: JSX nesting changed (e.g., MastheadToggle moved from MastheadBrand to MastheadMain)
/// For Python: class moved from one module/package to another
/// For Go: function moved between packages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerChange {
    /// The symbol that changed containers.
    pub symbol: String,
    /// The old container/parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_container: Option<String>,
    /// The new container/parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_container: Option<String>,
    /// Description of the change from the LLM.
    pub description: String,
}

// ── Package-level hierarchical report types ─────────────────────────────
// These types provide the pre-aggregated, per-package, per-component view
// that rule generators consume directly. They are populated during
// `build_report()` using the full API surfaces.

/// All changes within a single package.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct PackageChanges<L: Language> {
    /// Package name (e.g., "@patternfly/react-core").
    pub name: String,

    /// Package version at the old ref.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_version: Option<String>,

    /// Package version at the new ref.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_version: Option<String>,

    /// Per-type summaries with pre-aggregated change data.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_summaries: Vec<ComponentSummary<L>>,

    /// Pre-grouped bulk constant/token changes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constants: Vec<ConstantGroup>,

    /// Structurally-detected added exports (new symbols).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_exports: Vec<AddedExport>,
}

/// Pre-aggregated summary of all changes to a single type.
///
/// Built from the API surface symbol tree and the flat structural changes.
/// Contains everything the rule generator needs for a type without
/// rescanning the full report.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct ComponentSummary<L: Language> {
    /// Type name (e.g., "Modal").
    pub name: String,

    /// Definition name (e.g., "ModalProps").
    pub definition_name: String,

    /// Overall status of this type.
    pub status: ComponentStatus,

    /// Aggregated member change counts.
    pub member_summary: MemberSummary,

    /// Details of each removed member.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_members: Vec<RemovedMember>,

    /// Details of each type change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_changes: Vec<TypeChange>,

    /// Migration target if the component/interface was removed and a
    /// replacement was detected via member overlap analysis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration_target: Option<MigrationTarget>,

    /// Behavioral changes pre-grouped for this component.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub behavioral_changes: Vec<BehavioralChange<L>>,

    /// Discovered child/sibling components (e.g., ModalHeader added
    /// alongside Modal being modified).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_components: Vec<ChildComponent>,

    /// Expected direct children of this component, derived from LLM
    /// hierarchy inference on the component family's source code.
    /// Each entry is a component name that can be looked up in another
    /// `ComponentSummary` within the same package.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_children: Vec<ExpectedChild>,

    /// Source files containing this component's definitions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_files: Vec<PathBuf>,
}

/// Overall status of a component across the version change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentStatus {
    /// Component exists in both versions but has changes.
    Modified,
    /// Component was removed (interface gone or mostly removed).
    Removed,
    /// Component was added in the new version.
    Added,
}

/// Aggregated member-level change counts for a type.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemberSummary {
    /// Total number of members in the old version.
    pub total: usize,
    /// Number of members removed.
    pub removed: usize,
    /// Number of members renamed.
    pub renamed: usize,
    /// Number of members whose type changed.
    pub type_changed: usize,
    /// Number of members added in the new version.
    pub added: usize,
    /// Ratio of removed members to total (0.0 to 1.0).
    /// A high ratio (> 0.5) indicates the type was "mostly removed"
    /// and may warrant a type-level migration rule.
    pub removal_ratio: f64,
}

/// A member that was removed from a type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovedMember {
    /// Member name.
    pub name: String,
    /// The type annotation before removal (if known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_type: Option<String>,
    /// Why the member was removed and where its functionality went.
    /// Populated from LLM behavioral analysis when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removal_disposition: Option<RemovalDisposition>,
}

/// Why a member was removed and where its functionality went.
/// Determined by LLM analysis of the source diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemovalDisposition {
    /// Member moved to a related type (e.g., Modal.title → ModalHeader.title).
    MovedToRelatedType {
        /// The target type name (e.g., "ModalHeader").
        target_type: String,
        /// How to pass the value: "prop" (named prop) or "children".
        mechanism: String,
    },
    /// Replaced by a different member on the same type.
    ReplacedByMember {
        /// The new member name.
        new_member: String,
    },
    /// Functionality is now automatic / inferred.
    MadeAutomatic,
    /// Truly removed with no replacement.
    TrulyRemoved,
}

/// A property whose type changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeChange {
    /// Property name.
    pub property: String,
    /// Type before the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// Type after the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

/// A child or sibling component discovered during analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildComponent {
    /// Component name (e.g., "ModalHeader").
    pub name: String,
    /// Whether this component was added or modified.
    pub status: ChildComponentStatus,
    /// Known members on this child component (from the new surface AST).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_members: Vec<String>,
    /// Members that were removed from the parent and match members on this
    /// child (by name). Populated from AST member comparison.
    /// E.g., parent `Modal` had `title` removed, child `ModalHeader`
    /// has `title` → `absorbed_members: ["title"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub absorbed_members: Vec<String>,
}

/// Status of a child/sibling component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildComponentStatus {
    /// Newly added in the new version.
    Added,
    /// Existed before but was modified.
    Modified,
}

/// An expected direct child component, derived from LLM hierarchy inference.
///
/// Each entry names a component that should be used as a direct child of the
/// parent component. The `name` resolves to another `ComponentSummary` in the
/// same package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedChild {
    /// Component name (e.g., "DropdownList").
    pub name: String,
    /// Whether this child is required or optional.
    #[serde(default)]
    pub required: bool,
}

/// A change in the component hierarchy between versions, computed by diffing
/// the old and new hierarchy inference results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HierarchyDelta {
    /// The parent component whose children changed.
    pub component: String,
    /// Children added in the new version.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_children: Vec<ExpectedChild>,
    /// Children removed in the new version (no longer direct children).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_children: Vec<String>,
    /// Members removed from this type that now exist on a child type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub migrated_members: Vec<MigratedMember>,
}

/// A member that migrated from a parent type to a child type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigratedMember {
    /// The member name on the old parent type.
    pub member_name: String,
    /// The child type the member moved to.
    pub target_child: String,
    /// The member name on the child, if different from the parent member name
    /// (e.g., parent `bodyAriaRole` → child ModalBody `role`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_member_name: Option<String>,
}

/// The hierarchy of a single component family, as inferred by the LLM.
///
/// Maps component names to their expected children. Used for both old and new
/// versions; the delta is computed by diffing two `FamilyHierarchy` values.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FamilyHierarchy {
    /// Component name → expected children.
    pub components: HashMap<String, Vec<ExpectedChild>>,
}

/// Pre-grouped bulk constant/token changes within a package.
///
/// When a package has many constants with the same change type (e.g.,
/// 2000 CSS variables removed), they are grouped into a single entry
/// instead of generating individual rules for each.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstantGroup {
    /// What happened to these constants (Removed, TypeChanged, etc.).
    pub change_type: ApiChangeType,
    /// Number of constants in this group.
    pub count: usize,
    /// The individual symbol names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<String>,
    /// Regex pattern matching the common prefix (e.g., `"^(c_|global_|chart_)\\w+$"`).
    #[serde(default)]
    pub common_prefix_pattern: String,
    /// Suggested rule generation strategy (e.g., "CssVariablePrefix").
    #[serde(default)]
    pub strategy_hint: String,
    /// Pre-extracted suffix renames (e.g., logical CSS property renames).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suffix_renames: Vec<SuffixRename>,
}

/// A suffix rename pattern found in constant/token names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuffixRename {
    /// Old suffix (e.g., "PaddingLeft").
    pub from: String,
    /// New suffix (e.g., "PaddingInlineStart").
    pub to: String,
}

/// A symbol that was added (newly exported) in the new version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddedExport {
    /// Symbol name (e.g., "ModalHeader").
    pub name: String,
    /// Fully qualified name from the API surface.
    pub qualified_name: String,
    /// Package this symbol belongs to.
    pub package: String,
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
    pub kind: super::surface::SymbolKind,

    /// Package this symbol belongs to (propagated from Symbol).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,

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

/// Structural change type — what happened and to what.
///
/// 5 lifecycle variants, each carrying a `ChangeSubject` that describes
/// what aspect of the symbol was affected. The `before`/`after` fields on
/// the parent `StructuralChange` carry the values.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StructuralChangeType {
    Added(ChangeSubject),
    Removed(ChangeSubject),
    Changed(ChangeSubject),
    Renamed {
        from: ChangeSubject,
        to: ChangeSubject,
    },
    Relocated {
        from: ChangeSubject,
        to: ChangeSubject,
    },
}

impl StructuralChangeType {
    /// Map to the v2 harness API change type for report output.
    pub fn to_api_change_type(&self) -> ApiChangeType {
        match self {
            Self::Added(_) => ApiChangeType::SignatureChanged,
            Self::Removed(_) => ApiChangeType::Removed,
            Self::Changed(subject) => match subject {
                ChangeSubject::Visibility => ApiChangeType::VisibilityChanged,
                ChangeSubject::ReturnType
                | ChangeSubject::Parameter { .. }
                | ChangeSubject::UnionValue { .. } => ApiChangeType::TypeChanged,
                _ => ApiChangeType::SignatureChanged,
            },
            Self::Renamed { .. } => ApiChangeType::Renamed,
            Self::Relocated { .. } => ApiChangeType::Renamed,
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
    /// Package of the removed symbol (from Symbol::package).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_package: Option<String>,
    /// The suggested replacement symbol name (e.g., "EmptyStateProps").
    pub replacement_symbol: String,
    /// Qualified name of the replacement symbol.
    pub replacement_qualified_name: String,
    /// Package of the replacement symbol (from Symbol::package).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_package: Option<String>,
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

/// A breaking change in a package manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct ManifestChange<L: Language> {
    /// What field changed.
    pub field: String,

    /// Change type.
    pub change_type: L::ManifestChangeType,

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

// ── LLM-inferred rename patterns ──────────────────────────────────────

/// Rename patterns discovered by the LLM rename inference phase.
/// Stored in the report for transparency and reuse.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InferredRenamePatterns {
    /// Regex substitution patterns for bulk constant renames
    /// (e.g., PaddingTop → PaddingBlockStart).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constant_patterns: Vec<InferredConstantPattern>,

    /// Direct name mappings for interface/component renames
    /// (e.g., TextProps → ContentProps).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interface_mappings: Vec<InferredInterfaceMapping>,

    /// Statistics about the inference run.
    pub metadata: InferenceMetadata,
}

/// A regex-based rename pattern for constants, inferred by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferredConstantPattern {
    /// Regex pattern matching removed constant names.
    pub match_regex: String,
    /// Replacement string (may use capture group references like ${1}).
    pub replace: String,
    /// Number of removed constants this pattern successfully maps to an added constant.
    pub hit_count: usize,
    /// Total number of removed constants in the package.
    pub total_removed: usize,
}

/// A direct name mapping for an interface/component rename, inferred by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferredInterfaceMapping {
    /// The removed interface/component name.
    pub old_name: String,
    /// The added interface/component name (the replacement).
    pub new_name: String,
    /// LLM confidence: "high", "medium", or "low".
    pub confidence: String,
    /// Brief explanation from the LLM.
    pub reason: String,
    /// Member overlap ratio between old and new (computed during validation).
    pub member_overlap_ratio: f64,
}

/// An API change detected by LLM file-level analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmApiChange {
    pub file_path: String,
    pub symbol: String,
    pub change: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removal_disposition: Option<RemovalDisposition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renders_element: Option<String>,
}

/// Statistics about the LLM rename inference run.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InferenceMetadata {
    /// Number of LLM calls made (0, 1, or 2).
    pub llm_calls: usize,
    /// Fraction of removed constants mapped by inferred patterns.
    pub constant_hit_rate: f64,
    /// Number of interface rename mappings found.
    pub interface_mappings_found: usize,
}

// ── Pipeline result type ──────────────────────────────────────────────

use super::surface::ApiSurface;

/// Results from the full analysis pipeline.
///
/// Produced by the orchestrator, consumed by `Language::build_report()`
/// to construct the language-specific report.
#[derive(Debug)]
#[allow(dead_code)]
pub struct AnalysisResult<L: Language> {
    pub structural_changes: Arc<Vec<StructuralChange>>,
    pub behavioral_changes: Vec<BehavioralChange<L>>,
    pub manifest_changes: Vec<ManifestChange<L>>,
    pub llm_api_changes: Vec<LlmApiChange>,
    pub old_surface: Arc<ApiSurface>,
    pub new_surface: Arc<ApiSurface>,
    pub inferred_rename_patterns: Option<InferredRenamePatterns>,
    pub container_changes: Vec<(String, Vec<ContainerChange>)>,
    pub hierarchy_deltas: Vec<HierarchyDelta>,
    pub new_hierarchies: HashMap<String, HashMap<String, Vec<ExpectedChild>>>,
}
