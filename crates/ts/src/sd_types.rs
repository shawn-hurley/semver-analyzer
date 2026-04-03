//! Types for the SD (Source-Level Diff) pipeline (v2).
//!
//! These are TypeScript/React-specific types that were moved out of core
//! because they are inherently framework-specific. The SD pipeline extracts
//! structured profiles from React component source code and diffs them
//! as deterministic facts.
//!
//! Key types:
//! 1. `ComponentSourceProfile` — extracted from a single component's .tsx source
//! 2. `SourceLevelChange` — a deterministic diff between old and new profiles
//! 3. `CompositionTree` — the expected JSX composition structure for a component family
//! 4. `CompositionChange` — a diff between old and new composition trees
//! 5. `ConformanceCheck` — a structural validity rule derived from the composition tree

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

// ── Source Profile (extracted from a single component) ──────────────────

/// A structured profile of a React component's source-level characteristics.
///
/// Extracted by parsing the `.tsx` source file with OXC. Each field is
/// deterministic — derived from the AST, not inferred or guessed.
///
/// Two profiles (old version, new version) are diffed to produce
/// `SourceLevelChange` entries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComponentSourceProfile {
    /// Component name (e.g., "Dropdown", "Modal").
    pub name: String,

    /// Source file path relative to the repo root.
    pub file: String,

    // ── JSX render output ───────────────────────────────────────────
    /// HTML elements rendered in the JSX return tree, with counts.
    pub rendered_elements: BTreeMap<String, u32>,

    /// React components (PascalCase tags) rendered internally.
    pub rendered_components: Vec<String>,

    /// ARIA attributes on rendered elements.
    pub aria_attributes: BTreeMap<(String, String), String>,

    /// `role` attributes on rendered elements.
    pub role_attributes: BTreeMap<String, String>,

    /// `data-*` attributes on rendered elements.
    pub data_attributes: BTreeMap<(String, String), String>,

    // ── Prop defaults ───────────────────────────────────────────────
    /// Default values for props, extracted from destructuring patterns.
    pub prop_defaults: BTreeMap<String, String>,

    // ── React API usage ─────────────────────────────────────────────
    /// Whether the component uses `ReactDOM.createPortal()`.
    pub uses_portal: bool,

    /// If portal is used, the target expression (if statically determinable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portal_target: Option<String>,

    /// Contexts consumed via `useContext(X)`.
    pub consumed_contexts: Vec<String>,

    /// Contexts provided via `<XContext.Provider>`.
    pub provided_contexts: Vec<String>,

    /// Whether the component is wrapped in `React.forwardRef()`.
    pub is_forward_ref: bool,

    /// Whether the component is wrapped in `React.memo()`.
    pub is_memo: bool,

    // ── CSS / BEM structure ─────────────────────────────────────────
    /// `styles.*` token references found in the source.
    pub css_tokens_used: BTreeSet<String>,

    /// BEM block name derived from the primary `styles.*` token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bem_block: Option<String>,

    /// BEM elements derived from `styles.*` tokens.
    pub bem_elements: BTreeSet<String>,

    /// BEM modifiers derived from `styles.modifiers.*` tokens.
    pub bem_modifiers: BTreeSet<String>,

    // ── Type delegation ───────────────────────────────────────────────
    /// Props interfaces that this component's props extend.
    pub extends_props: Vec<String>,

    // ── Children slot ───────────────────────────────────────────────
    /// Where `{children}` appears in the JSX tree.
    pub children_slot_path: Vec<String>,

    /// Whether the component accepts `children` at all.
    pub has_children_prop: bool,

    /// All prop names on the component's Props interface.
    pub all_props: BTreeSet<String>,

    /// Prop name → type string mapping for props with known types.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prop_types: BTreeMap<String, String>,
}

// ── Source Level Change (diff between two profiles) ─────────────────────

/// A deterministic change detected by diffing two `ComponentSourceProfile`s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceLevelChange {
    /// Component this change applies to.
    pub component: String,

    /// What category of change this is.
    pub category: SourceLevelCategory,

    /// Human-readable description of the change.
    pub description: String,

    /// The old value (for context in migration messages).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_value: Option<String>,

    /// The new value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_value: Option<String>,

    /// Whether this change has distinct implications for test code.
    pub has_test_implications: bool,

    /// Test-specific description, if different from the main description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_description: Option<String>,
}

/// Categories of source-level changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceLevelCategory {
    DomStructure,
    AriaChange,
    RoleChange,
    DataAttribute,
    CssToken,
    PropDefault,
    PortalUsage,
    ContextDependency,
    Composition,
    ForwardRef,
    Memo,
    RenderedComponent,
}

// ── Composition Tree ────────────────────────────────────────────────────

/// The expected JSX composition tree for a component family.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompositionTree {
    /// The root component of this family.
    pub root: String,

    /// All family members (exported from the family's index file).
    pub family_members: Vec<String>,

    /// Parent → children edges in the expected composition tree.
    pub edges: Vec<CompositionEdge>,
}

/// An edge in the composition tree: parent expects child.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositionEdge {
    pub parent: String,
    pub child: String,
    pub relationship: ChildRelationship,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bem_evidence: Option<String>,
}

/// How a child component relates to its parent in the composition tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildRelationship {
    BemElement,
    IndependentBlock,
    Internal,
    DirectChild,
    Unknown,
}

// ── Composition Change (diff between two trees) ─────────────────────────

/// A change in the composition structure between old and new versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositionChange {
    pub family: String,
    pub change_type: CompositionChangeType,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_pattern: Option<String>,
}

/// Types of composition changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompositionChangeType {
    NewRequiredChild {
        parent: String,
        new_child: String,
        wraps: Vec<String>,
    },
    PropToChild {
        parent: String,
        child: String,
        props: Vec<String>,
    },
    ChildToProp {
        parent: String,
        child: String,
        props: Vec<String>,
    },
    FamilyMemberRemoved {
        member: String,
    },
    FamilyMemberAdded {
        member: String,
    },
    PropDrivenToComposition {
        parent: String,
    },
    CompositionToPropDriven {
        parent: String,
    },
}

// ── Conformance Check ───────────────────────────────────────────────────

/// A structural conformance rule derived from the composition tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConformanceCheck {
    pub family: String,
    pub check_type: ConformanceCheckType,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correct_example: Option<String>,
}

/// Types of conformance checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConformanceCheckType {
    MissingIntermediate {
        parent: String,
        child: String,
        required_intermediate: String,
    },
    MissingChild {
        parent: String,
        expected_child: String,
    },
    InvalidDirectChild {
        parent: String,
        child: String,
        expected_parent: String,
    },
}

// ── SD Pipeline Result ──────────────────────────────────────────────────

/// Results from the SD (Source-Level Diff) pipeline.
///
/// Produced by `Language::run_source_diff()`, consumed by the v2
/// report builder and rule generator.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SdPipelineResult {
    /// Deterministic source-level changes between old and new profiles.
    pub source_level_changes: Vec<SourceLevelChange>,

    /// Composition trees for component families in the new version.
    pub composition_trees: Vec<CompositionTree>,

    /// Composition changes between old and new trees.
    pub composition_changes: Vec<CompositionChange>,

    /// Conformance checks derived from the new composition trees.
    pub conformance_checks: Vec<ConformanceCheck>,

    /// Component name → package name mapping for the NEW version.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub component_packages: HashMap<String, String>,

    /// Component name → package name mapping for the OLD version.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub old_component_packages: HashMap<String, String>,

    /// Component name → all prop names, for both old and new versions.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub old_component_props: HashMap<String, BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub new_component_props: HashMap<String, BTreeSet<String>>,

    /// Component name → prop type map for new version.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub new_component_prop_types: HashMap<String, BTreeMap<String, String>>,

    /// Dependency repo packages (name → version at new ref).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub dep_repo_packages: HashMap<String, String>,

    /// Extracted profiles keyed by component name, for both versions.
    #[serde(skip)]
    pub old_profiles: HashMap<String, ComponentSourceProfile>,
    #[serde(skip)]
    pub new_profiles: HashMap<String, ComponentSourceProfile>,
}

// ── Severity (used by source_profile diff) ──────────────────────────────

/// Severity level for source-level changes.
///
/// Used internally by the SD diff to rank changes; not serialized
/// into the final report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceLevelSeverity {
    Info,
    Warning,
    Breaking,
}
