//! Types for the SD (Source-Level Diff) pipeline (v2).
//!
//! SD replaces most of the BU behavioral analysis pipeline with deterministic,
//! AST-based source code analysis. Instead of diffing function bodies and
//! guessing at behavioral changes, SD extracts structured profiles from each
//! public component's source and diffs them as facts.
//!
//! Key types:
//! 1. `ComponentSourceProfile` — extracted from a single component's .tsx source
//! 2. `SourceLevelChange` — a deterministic diff between old and new profiles
//! 3. `CompositionTree` — the expected JSX composition structure for a component family
//! 4. `CompositionChange` — a diff between old and new composition trees
//! 5. `ConformanceCheck` — a structural validity rule derived from the composition tree

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Serialize a BTreeMap<(String, String), String> as a JSON object with
/// `"key1::key2"` string keys. This allows tuple-keyed maps to roundtrip
/// through JSON.
fn serialize_tuple_map<S>(
    map: &BTreeMap<(String, String), String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    use serde::ser::SerializeMap;
    let mut m = serializer.serialize_map(Some(map.len()))?;
    for ((k1, k2), v) in map {
        m.serialize_entry(&format!("{}::{}", k1, k2), v)?;
    }
    m.end()
}

/// Deserialize a JSON object with `"key1::key2"` string keys back into
/// a BTreeMap<(String, String), String>.
fn deserialize_tuple_map<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<(String, String), String>, D::Error>
where
    D: Deserializer<'de>,
{
    let string_map: BTreeMap<String, String> = BTreeMap::deserialize(deserializer)?;
    let mut result = BTreeMap::new();
    for (key, value) in string_map {
        if let Some((k1, k2)) = key.split_once("::") {
            result.insert((k1.to_string(), k2.to_string()), value);
        }
    }
    Ok(result)
}

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
    /// e.g., { "div": 2, "button": 1, "section": 1 }
    pub rendered_elements: BTreeMap<String, u32>,

    /// React components (PascalCase tags) rendered internally.
    /// e.g., ["Menu", "MenuContent", "Popper"]
    pub rendered_components: Vec<String>,

    /// ARIA attributes on rendered elements.
    /// Key: (element_tag, attribute_name), Value: attribute_value.
    /// e.g., { ("div", "aria-label"): "Navigation", ("button", "role"): "menuitem" }
    #[serde(
        serialize_with = "serialize_tuple_map",
        deserialize_with = "deserialize_tuple_map"
    )]
    pub aria_attributes: BTreeMap<(String, String), String>,

    /// `role` attributes on rendered elements.
    /// Key: element_tag, Value: role value.
    pub role_attributes: BTreeMap<String, String>,

    /// `data-*` attributes on rendered elements.
    /// Key: (element_tag, attribute_name), Value: attribute_value.
    #[serde(
        serialize_with = "serialize_tuple_map",
        deserialize_with = "deserialize_tuple_map"
    )]
    pub data_attributes: BTreeMap<(String, String), String>,

    // ── Prop defaults ───────────────────────────────────────────────
    /// Default values for props, extracted from destructuring patterns.
    /// e.g., `const { variant = 'primary' } = props` → { "variant": "'primary'" }
    pub prop_defaults: BTreeMap<String, String>,

    // ── React API usage ─────────────────────────────────────────────
    /// Whether the component uses `ReactDOM.createPortal()`.
    pub uses_portal: bool,

    /// If portal is used, the target expression (if statically determinable).
    /// e.g., "document.body", "appendTo", etc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portal_target: Option<String>,

    /// Contexts consumed via `useContext(X)`.
    /// Contains the context name (e.g., "AccordionItemContext").
    pub consumed_contexts: Vec<String>,

    /// Contexts provided via `<XContext.Provider>`.
    /// Extracted from rendered_components entries ending in ".Provider".
    /// e.g., ["MenuContext", "AccordionContext"]
    pub provided_contexts: Vec<String>,

    /// Whether the component is wrapped in `React.forwardRef()`.
    pub is_forward_ref: bool,

    /// Whether the component is wrapped in `React.memo()`.
    pub is_memo: bool,

    // ── CSS / BEM structure ─────────────────────────────────────────
    /// `styles.*` token references found in the source.
    /// e.g., { "styles.menu", "styles.menuList", "styles.modifiers" }
    pub css_tokens_used: BTreeSet<String>,

    /// BEM block name derived from the primary `styles.*` token.
    /// e.g., "menu" (from `styles.menu` → `pf-v6-c-menu`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bem_block: Option<String>,

    /// BEM elements derived from `styles.*` tokens.
    /// e.g., { "list" (from styles.menuList → menu__list),
    ///         "listItem" (from styles.menuListItem → menu__list-item) }
    pub bem_elements: BTreeSet<String>,

    /// BEM modifiers derived from `styles.modifiers.*` tokens.
    /// e.g., { "expanded", "disabled", "plain" }
    pub bem_modifiers: BTreeSet<String>,

    // ── Prop-to-style bindings ────────────────────────────────────────
    /// Props that control CSS class application via conditional expressions.
    /// Maps prop name → set of CSS tokens gated by that prop.
    /// e.g., { "isScrollable" → {"styles.modifiers.scrollable"} }
    ///
    /// Detected by tracing conditional patterns in `className` expressions:
    /// `prop && styles.xxx`, `prop ? styles.xxx : …`, `{ [styles.xxx]: prop }`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prop_style_bindings: BTreeMap<String, BTreeSet<String>>,

    // ── Type delegation ───────────────────────────────────────────────
    /// Props interfaces that this component's props extend.
    /// Extracted from `interface XProps extends Y, Z { ... }`.
    /// e.g., DropdownListProps extends MenuListProps → ["MenuListProps"]
    ///
    /// Used to detect wrapper/delegation patterns: if DropdownList extends
    /// MenuListProps, the Dropdown family delegates to the Menu family.
    pub extends_props: Vec<String>,

    // ── Children slot ───────────────────────────────────────────────
    /// Where `{children}` (or `{props.children}`) appears in the JSX tree.
    /// Represented as the chain of wrapper components/elements from the
    /// return root down to the children slot.
    /// e.g., ["Popper", "Menu", "MenuContent"] means children land inside
    /// Menu > MenuContent, rendered via Popper.
    pub children_slot_path: Vec<String>,

    /// Enhanced children slot path with CSS token information.
    /// Each entry is `(tag_name, css_token)` where css_token is the
    /// `styles.xxx` token from the `className` attribute, if present.
    /// e.g., [("div", Some("toolbarContent")), ("div", Some("toolbarContentSection"))]
    /// tells us the component wraps children in `styles.toolbarContentSection`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children_slot_detail: Vec<(String, Option<String>)>,

    /// Whether the component accepts `children` at all.
    pub has_children_prop: bool,

    /// All prop names on the component's Props interface.
    /// Extracted from `interface XProps { propA: string; propB: number; }`.
    pub all_props: BTreeSet<String>,

    /// Props that are required (not optional, no `?` marker).
    /// Subset of `all_props`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub required_props: BTreeSet<String>,

    /// Prop name → type string mapping for props with known types.
    /// e.g., { "icon": "ReactNode", "variant": "'primary' | 'secondary'" }
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prop_types: BTreeMap<String, String>,

    // ── cloneElement prop threading ──────────────────────────────────────
    /// Props injected into children via `React.Children.map` + `cloneElement`.
    /// Each entry lists the prop names passed in cloneElement's second argument.
    ///
    /// e.g., DataListItem does `cloneElement(child, { rowid: ariaLabelledBy })`
    /// → `[CloneElementInjection { injected_props: ["rowid"] }]`
    ///
    /// Used to infer parent-child relationships: if component A injects
    /// prop "rowid" and family member B declares "rowid" in its interface,
    /// then B is a child of A.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clone_element_injections: Vec<CloneElementInjection>,

    // ── Managed attribute bindings ─────────────────────────────────────
    /// Props that the component extracts from rest, transforms via a helper
    /// function, and spreads back onto a JSX element after rest — overriding
    /// any consumer-provided HTML attribute that the helper generates.
    ///
    /// e.g., `ouiaId` is destructured, passed to `getOUIAProps()`, and the
    /// result `{...ouiaProps}` is spread after `{...otherProps}` on `<button>`.
    /// Any consumer passing `data-ouia-component-id` directly will be overridden.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub managed_attributes: Vec<ManagedAttributeBinding>,
}

/// Props injected into children via `cloneElement`.
///
/// Detected by finding `cloneElement(child, { prop1, prop2, ... })` calls
/// inside `Children.map` or `Children.forEach` callbacks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloneElementInjection {
    /// The prop names passed in cloneElement's second argument.
    /// e.g., ["rowid"] for DataListItem's `cloneElement(child, { rowid })`
    pub injected_props: Vec<String>,
}

/// A prop-to-HTML-attribute override binding.
///
/// Represents a pattern where a component destructures a prop out of the
/// rest parameter, passes it through a helper function, and spreads the
/// result onto a JSX element after the rest props. This means any
/// consumer-provided HTML attribute with the same name as the generated
/// attributes will be silently overridden.
///
/// Detected via AST dataflow analysis of the component's function body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedAttributeBinding {
    /// The React prop name that the component extracts and manages.
    /// e.g., "ouiaId"
    pub prop_name: String,

    /// The helper function that transforms the prop into HTML attributes.
    /// e.g., "getOUIAProps"
    pub generator_function: String,

    /// The JSX element tag where the managed spread is applied.
    /// e.g., "button"
    pub target_element: String,

    /// HTML attributes on the target element that are likely overridden
    /// by the managed spread. Correlated from the data_attributes map.
    /// e.g., ["data-ouia-component-id", "data-ouia-component-type"]
    pub overridden_attributes: Vec<String>,
}

// ── Source Level Change (diff between two profiles) ─────────────────────

/// A deterministic change detected by diffing two `ComponentSourceProfile`s.
///
/// Unlike `BehavioralChange` (BU), these have no confidence scores — they
/// are facts derived from AST comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceLevelChange {
    /// Component this change applies to.
    pub component: String,

    /// What category of change this is.
    pub category: SourceLevelCategory,

    /// Human-readable description of the change.
    /// Generated from templates, not LLM.
    pub description: String,

    /// The old value (for context in migration messages).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_value: Option<String>,

    /// The new value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_value: Option<String>,

    /// Whether this change has distinct implications for test code
    /// vs production code. If true, rule generation should produce
    /// two rules (production + test with `filePattern`).
    pub has_test_implications: bool,

    /// Test-specific description, if different from the main description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_description: Option<String>,

    /// The DOM element this change pertains to (e.g., "button", "div").
    /// Set for element-specific changes (role, aria, dom structure, data attributes).
    /// Used in rule ID generation to disambiguate changes on different elements
    /// within the same component.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element: Option<String>,

    /// When set, this change is from a cross-component migration diff
    /// (e.g., deprecated Select → new Select). The value is the source
    /// file path of the removed component that this change migrates away
    /// from. Downstream rule generation can use this to separate
    /// deprecated-migration rules from same-component evolution rules.
    ///
    /// Example: `"packages/react-core/src/deprecated/components/Select/Select.tsx"`
    /// means this change describes a behavioral difference between the
    /// deprecated Select and its non-deprecated replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_from: Option<String>,
}

/// Categories of source-level changes.
///
/// Each category maps to a rule template for message generation.
/// Categories marked with `has_test_implications` get a second rule
/// targeting test files with test-specific guidance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceLevelCategory {
    /// Root element or wrapper element changed (e.g., div → section).
    DomStructure,
    /// ARIA attribute added, removed, or changed.
    AriaChange,
    /// `role` attribute added, removed, or changed.
    RoleChange,
    /// `data-*` attribute added, removed, or changed.
    DataAttribute,
    /// CSS/BEM token added, removed, or changed.
    CssToken,
    /// Prop default value changed.
    PropDefault,
    /// `createPortal` usage added or removed.
    PortalUsage,
    /// `useContext` dependency added, removed, or changed.
    ContextDependency,
    /// Component composition structure changed (new required children, etc.).
    Composition,
    /// `forwardRef` wrapper added or removed.
    ForwardRef,
    /// `memo` wrapper added or removed.
    Memo,
    /// Rendered child component added or removed.
    RenderedComponent,
    /// Component prop overrides a consumer-provided HTML attribute.
    /// Detected when a destructured prop is transformed via a helper function
    /// and the result is spread onto a JSX element after the rest props,
    /// silently overriding any matching HTML attribute the consumer passes.
    PropAttributeOverride,
}

// ── Composition Tree ────────────────────────────────────────────────────

/// The expected JSX composition tree for a component family.
///
/// Derived from: family exports (index file) + children slot tracing +
/// BEM token analysis + rendered_components.
///
/// Used to generate both migration rules (v5 vs v6 tree diff) and
/// conformance rules (is the consumer's JSX structure valid?).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompositionTree {
    /// The root component of this family (e.g., "Dropdown", "Modal").
    pub root: String,

    /// All family members (exported from the family's index file).
    pub family_members: Vec<String>,

    /// Parent → children edges in the expected composition tree.
    pub edges: Vec<CompositionEdge>,
}

/// An edge in the composition tree: parent expects child.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositionEdge {
    /// The parent component.
    pub parent: String,

    /// The child component.
    pub child: String,

    /// How the child relates to the parent.
    pub relationship: ChildRelationship,

    /// Whether this child is required (vs optional).
    pub required: bool,

    /// BEM evidence for this relationship, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bem_evidence: Option<String>,

    /// How strongly this nesting is enforced.
    /// `Required` = rendering breaks without it (conformance rules generated).
    /// `Allowed` = valid placement but not the only option (no conformance rules).
    #[serde(default)]
    pub strength: EdgeStrength,
}

/// How strongly a composition edge is enforced.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeStrength {
    /// Valid nesting shown in CSS descendant selectors or examples,
    /// but not the only valid placement. No conformance rule generated.
    #[default]
    Allowed = 0,
    /// Rendering breaks without this nesting — CSS layout, context,
    /// DOM semantics, or prop threading requires this parent.
    /// Conformance rules are generated for Required edges.
    Required = 1,
}

/// How a child component relates to its parent in the composition tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildRelationship {
    /// Child is a BEM element of the parent's block (`__element`).
    /// Structurally required — CSS assumes this nesting.
    BemElement,
    /// Child is an independent BEM block.
    /// Typically passed via prop, not nested in JSX.
    IndependentBlock,
    /// Child is rendered internally by the parent (not provided by consumer).
    Internal,
    /// Child is expected in the consumer's JSX children.
    DirectChild,
    /// Relationship could not be determined from BEM.
    Unknown,
}

// ── Composition Change (diff between two trees) ─────────────────────────

/// A change in the composition structure between old and new versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositionChange {
    /// The component family this change applies to.
    pub family: String,

    /// What kind of composition change.
    pub change_type: CompositionChangeType,

    /// Human-readable description.
    pub description: String,

    /// Before pattern (JSX snippet), if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_pattern: Option<String>,

    /// After pattern (JSX snippet), if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_pattern: Option<String>,
}

/// Types of composition changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompositionChangeType {
    /// New child component required between parent and existing children.
    /// e.g., DropdownList inserted between Dropdown and DropdownItem.
    NewRequiredChild {
        parent: String,
        new_child: String,
        wraps: Vec<String>,
    },
    /// Props moved from parent to a new child component.
    /// e.g., Modal's `title` prop moved to ModalHeader.
    PropToChild {
        parent: String,
        child: String,
        props: Vec<String>,
    },
    /// Child components absorbed back into parent as props.
    ChildToProp {
        parent: String,
        child: String,
        props: Vec<String>,
    },
    /// Family member removed (no replacement).
    FamilyMemberRemoved { member: String },
    /// Family member added.
    FamilyMemberAdded { member: String },
    /// Component changed from prop-driven to composition-based API.
    PropDrivenToComposition { parent: String },
    /// Component changed from composition-based to prop-driven API.
    CompositionToPropDriven { parent: String },
}

// ── Conformance Check ───────────────────────────────────────────────────

/// A structural conformance rule derived from the composition tree.
///
/// These rules validate that consumer code uses the correct JSX
/// composition structure — they work for both migrated and new code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConformanceCheck {
    /// Component family this check applies to.
    pub family: String,

    /// What kind of conformance check.
    pub check_type: ConformanceCheckType,

    /// Human-readable description of what correct usage looks like.
    pub description: String,

    /// Example of correct JSX structure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correct_example: Option<String>,
}

/// Types of conformance checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConformanceCheckType {
    /// Child X must have intermediate Y between it and parent Z.
    /// e.g., DropdownItem must be inside DropdownList inside Dropdown.
    MissingIntermediate {
        parent: String,
        child: String,
        required_intermediate: String,
    },
    /// Parent X should contain child Y.
    /// e.g., Modal should contain ModalBody.
    MissingChild {
        parent: String,
        expected_child: String,
    },
    /// Child X should not be a direct child of parent Z.
    /// e.g., DropdownItem should not be direct child of Dropdown.
    InvalidDirectChild {
        parent: String,
        child: String,
        expected_parent: String,
    },
    /// All children of parent X must be wrapped in component Y.
    /// e.g., all children of InputGroup must be InputGroupItem or InputGroupText.
    /// Detected from CSS: parent uses flex/grid layout, all child styling
    /// exclusively targets a single BEM element wrapper.
    ExclusiveWrapper {
        parent: String,
        /// Allowed direct children (regex pattern matching their names).
        allowed_children: Vec<String>,
    },
}

// ── SD Pipeline Result ──────────────────────────────────────────────────

/// Results from the SD (Source-Level Diff) pipeline.
///
/// Produced by `Language::run_source_diff()`, consumed by the v2
/// orchestrator to merge with TD structural changes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SdPipelineResult {
    /// Deterministic source-level changes between old and new profiles.
    pub source_level_changes: Vec<SourceLevelChange>,

    /// Composition trees for component families in the new version.
    pub composition_trees: Vec<CompositionTree>,

    /// Composition trees for component families in the old version.
    /// Only populated for families that had changes between versions.
    /// Used for cross-family child→prop migration detection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub old_composition_trees: Vec<CompositionTree>,

    /// Composition changes between old and new trees.
    pub composition_changes: Vec<CompositionChange>,

    /// Conformance checks derived from the new composition trees.
    pub conformance_checks: Vec<ConformanceCheck>,

    /// Component name → npm package name mapping for the NEW version.
    /// Populated during SD pipeline from source file paths.
    /// Used by rule generation to set the correct `from:` package scope.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub component_packages: HashMap<String, String>,

    /// Component name → npm package name mapping for the OLD version.
    /// Used to detect deprecated↔main migrations.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub old_component_packages: HashMap<String, String>,

    /// Component name → all prop names, for both old and new versions.
    /// Used for child→prop migration detection (comparing which props
    /// existed in old vs new). Serialized so `--from-report` works.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub old_component_props: HashMap<String, BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub new_component_props: HashMap<String, BTreeSet<String>>,

    /// Component name → prop type map for the old version.
    /// Used for detecting value changes on renamed props.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub old_component_prop_types: HashMap<String, BTreeMap<String, String>>,

    /// Component name → prop type map for new version.
    /// Used for determining if a prop is ReactNode-ish in child→prop detection.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub new_component_prop_types: HashMap<String, BTreeMap<String, String>>,

    /// Component name → required prop names for the new version.
    /// Used for required-prop-added conformance rules.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub new_required_props: HashMap<String, BTreeSet<String>>,

    /// Dependency repo packages (name → version at new ref).
    /// Used to generate dep-update rules for packages outside the main
    /// analyzed monorepo (e.g., `@patternfly/patternfly` CSS package).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub dep_repo_packages: HashMap<String, String>,

    /// CSS component blocks that were removed between the old and new
    /// versions of the dependency CSS repo (e.g., "select", "chip").
    /// Used to generate rules that flag consumer CSS files referencing
    /// removed PF class prefixes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_css_blocks: Vec<String>,

    /// Deprecated component → replacement mappings detected via rendering swaps.
    ///
    /// When a component is relocated to `/deprecated/` and other components
    /// in the codebase switched from rendering the old component to rendering
    /// a new one (e.g., ToolbarFilter stopped rendering `Chip` and started
    /// rendering `Label`), this records the replacement relationship.
    ///
    /// Populated by the orchestrator after both TD and SD pipelines complete.
    /// Consumed by report building and rule generation to produce unified
    /// migration entries instead of separate relocation + signature-changed rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deprecated_replacements: Vec<DeprecatedReplacement>,

    /// Extracted profiles keyed by component name, for both versions.
    /// Retained for downstream use (rule generation, debugging).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub old_profiles: HashMap<String, ComponentSourceProfile>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub new_profiles: HashMap<String, ComponentSourceProfile>,
}

// ── Deprecated Replacement Detection ────────────────────────────────────

/// A deprecated component that has a differently-named replacement,
/// detected via rendering swap analysis.
///
/// When host components (e.g., ToolbarFilter) stopped rendering `Chip`
/// and started rendering `Label` between versions, this establishes
/// the `Chip → Label` replacement relationship.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeprecatedReplacement {
    /// The deprecated component name (e.g., "Chip").
    pub old_component: String,
    /// The replacement component name (e.g., "Label").
    pub new_component: String,
    /// Host components that confirmed the swap
    /// (e.g., \["ToolbarFilter", "MultiTypeaheadSelect"\]).
    pub evidence_hosts: Vec<String>,
}
