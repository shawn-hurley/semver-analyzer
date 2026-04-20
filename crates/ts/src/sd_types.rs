//! TypeScript/React-specific types for the SD (Source-Level Diff) pipeline (v2).
//!
//! Moved from `semver-analyzer-core::types::sd` during genericization (Phase 2).
//! These types are 100% React/JSX/BEM/PatternFly concepts and belong in the
//! TypeScript crate, not in the language-agnostic core.
//!
//! Key types:
//! 1. `ComponentSourceProfile` — extracted from a single component's .tsx source
//! 2. `SourceLevelChange` — a deterministic diff between old and new profiles
//! 3. `CompositionTree` — the expected JSX composition structure for a component family
//! 4. `CompositionChange` — a diff between old and new composition trees
//! 5. `ConformanceCheck` — a structural validity rule derived from the composition tree

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

// ── TrackedAttributes ───────────────────────────────────────────────────

/// JSX attribute observations with conditionality tracking.
///
/// Tracks both attribute values and whether each attribute appears in
/// an unconditional rendering context. Uses "unconditional wins"
/// semantics: if an attribute appears both conditionally and
/// unconditionally across JSX branches, it is classified as
/// unconditional.
///
/// Generic over the key type `K`:
/// - `TrackedAttributes<(String, String)>` for `(element_tag, attr_name)` keyed maps
///   (aria-* and data-* attributes).
/// - `TrackedAttributes<String>` for element-keyed maps (role attributes).
///
/// Custom `Serialize`/`Deserialize` implementations are provided for
/// both key types to ensure YAML-compatible map keys. Tuple keys use
/// `"key1::key2"` encoding.
#[derive(Debug, Clone, Default)]
pub struct TrackedAttributes<K: Ord + Clone> {
    /// Key → attribute value (last-seen wins for duplicates).
    pub entries: BTreeMap<K, String>,
    /// Keys that appeared in at least one unconditional context.
    /// "Unconditional wins": if an attribute appears both inside and
    /// outside conditional branches, it is classified as unconditional.
    pub unconditional: BTreeSet<K>,
}

// ── Serde for TrackedAttributes<String> (trivial) ───────────────────────

/// Intermediate form for serializing `TrackedAttributes<String>`.
#[derive(Serialize, Deserialize)]
struct TrackedStringRepr {
    entries: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    unconditional: BTreeSet<String>,
}

impl Serialize for TrackedAttributes<String> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let repr = TrackedStringRepr {
            entries: self.entries.clone(),
            unconditional: self.unconditional.clone(),
        };
        repr.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TrackedAttributes<String> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let repr = TrackedStringRepr::deserialize(deserializer)?;
        Ok(TrackedAttributes {
            entries: repr.entries,
            unconditional: repr.unconditional,
        })
    }
}

// ── Serde for TrackedAttributes<(String, String)> (tuple → "k1::k2") ────

/// Intermediate form for serializing `TrackedAttributes<(String, String)>`.
/// Uses `"key1::key2"` string encoding for tuple keys, ensuring YAML
/// compatibility (YAML requires string map keys).
#[derive(Serialize, Deserialize)]
struct TrackedTupleRepr {
    entries: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unconditional: Vec<String>,
}

impl Serialize for TrackedAttributes<(String, String)> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let entries: BTreeMap<String, String> = self
            .entries
            .iter()
            .map(|((k1, k2), v)| (format!("{k1}::{k2}"), v.clone()))
            .collect();
        let unconditional: Vec<String> = self
            .unconditional
            .iter()
            .map(|(k1, k2)| format!("{k1}::{k2}"))
            .collect();
        let repr = TrackedTupleRepr {
            entries,
            unconditional,
        };
        repr.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TrackedAttributes<(String, String)> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let repr = TrackedTupleRepr::deserialize(deserializer)?;
        let entries: BTreeMap<(String, String), String> = repr
            .entries
            .into_iter()
            .filter_map(|(key, value)| {
                key.split_once("::")
                    .map(|(k1, k2)| ((k1.to_string(), k2.to_string()), value))
            })
            .collect();
        let unconditional: BTreeSet<(String, String)> = repr
            .unconditional
            .into_iter()
            .filter_map(|key| {
                key.split_once("::")
                    .map(|(k1, k2)| (k1.to_string(), k2.to_string()))
            })
            .collect();
        Ok(TrackedAttributes {
            entries,
            unconditional,
        })
    }
}

impl<K: Ord + Clone> TrackedAttributes<K> {
    /// Insert an attribute observation. If `conditional` is false,
    /// the key is also added to the unconditional set.
    pub fn insert(&mut self, key: K, value: String, conditional: bool) {
        self.entries.insert(key.clone(), value);
        if !conditional {
            self.unconditional.insert(key);
        }
    }

    /// Whether the key exists but was only observed in conditional contexts.
    pub fn is_conditional(&self, key: &K) -> bool {
        self.entries.contains_key(key) && !self.unconditional.contains(key)
    }

    /// Whether the key exists and was observed unconditionally at least once.
    pub fn is_unconditional(&self, key: &K) -> bool {
        self.unconditional.contains(key)
    }

    /// Whether the key exists in the entries map.
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Look up an attribute value by key.
    pub fn get(&self, key: &K) -> Option<&String> {
        self.entries.get(key)
    }

    /// Iterate over all `(key, value)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &String)> {
        self.entries.iter()
    }

    /// Iterate over all keys.
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.entries.keys()
    }

    /// Whether the entries map is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A React component rendered internally by another component.
///
/// Tracks whether the rendering is conditional (inside a ternary,
/// `&&`, `if` branch) or unconditional (always rendered).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenderedComponent {
    /// Component name (PascalCase tag, e.g., "ModalBox", "Menu").
    pub name: String,
    /// Whether this component is rendered conditionally.
    /// - `false` — always rendered (unconditional): `return <Child/>`
    /// - `true` — conditionally rendered: `condition ? <Child/> : null`
    #[serde(default)]
    pub conditional: bool,
}

impl RenderedComponent {
    pub fn unconditional(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            conditional: false,
        }
    }

    pub fn conditional(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            conditional: true,
        }
    }
}

impl From<&str> for RenderedComponent {
    /// Creates an unconditional RenderedComponent from a string.
    /// Convenience for test code and migration of existing `vec!["Name".into()]`.
    fn from(name: &str) -> Self {
        Self::unconditional(name)
    }
}

impl From<String> for RenderedComponent {
    fn from(name: String) -> Self {
        Self {
            name,
            conditional: false,
        }
    }
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

    /// React components (PascalCase tags) rendered internally,
    /// with a flag indicating whether rendering is conditional.
    /// e.g., [RenderedComponent { name: "Menu", conditional: false }]
    pub rendered_components: Vec<RenderedComponent>,

    /// ARIA attributes on rendered elements, with conditionality tracking.
    /// Key: (element_tag, attribute_name), Value: attribute_value.
    /// e.g., { ("div", "aria-label"): "Navigation" }
    pub aria_attributes: TrackedAttributes<(String, String)>,

    /// `role` attributes on rendered elements, with conditionality tracking.
    /// Key: element_tag, Value: role value.
    pub role_attributes: TrackedAttributes<String>,

    /// `data-*` attributes on rendered elements, with conditionality tracking.
    /// Key: (element_tag, attribute_name), Value: attribute_value.
    pub data_attributes: TrackedAttributes<(String, String)>,

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

    /// Whether the managed spread comes AFTER the rest spread on the
    /// same JSX element, meaning the component's generated attributes
    /// silently override any consumer-provided values.
    ///
    /// `true` = component wins (managed spread after rest spread).
    /// `false` = consumer wins (managed spread before rest, or no rest).
    ///
    /// Prop-attribute-override rules only fire when `true`. Transitive
    /// behavioral change detection (Phase A.7) uses both — the helper's
    /// output change affects the rendered attributes regardless of order.
    #[serde(default)]
    pub component_overrides: bool,
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

    /// For transitive behavioral changes: the chain from the affected
    /// component to the root cause dependency.
    ///
    /// Example: `["Alert", "getOUIAProps", "src/helpers/OUIA/ouia.ts"]`
    /// means Alert is affected because it imports `getOUIAProps` from
    /// `ouia.ts`, and that helper function changed its output between
    /// versions.
    ///
    /// When `None`, the change was detected by direct profile comparison
    /// (the normal case). When `Some`, it was detected by analyzing
    /// transitive dependency changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_chain: Option<Vec<String>>,
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
    /// Attribute rendering changed from unconditional to conditional (or vice versa).
    /// The attribute still exists in both versions, but its presence in the DOM
    /// is no longer guaranteed. Tests using `getAttribute()` expecting a specific
    /// value may now receive `null`.
    AttributeConditionality,
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

    /// For `PropPassed` edges, the name of the prop on the parent that
    /// accepts this child (e.g., "actionLinks", "labelHelp", "sidebar").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prop_name: Option<String>,
}

/// How strongly a composition edge is enforced.
///
/// Four strengths encode two independent constraint dimensions:
/// - **child-must-have-parent (CHP)**: Does the child break outside the parent?
/// - **parent-must-have-child (PMC)**: Does the parent need this child?
///
/// | Strength   | CHP | PMC | Rule types generated        |
/// |------------|-----|-----|-----------------------------|
/// | Required   | YES | YES | `notParent` + `requiresChild` |
/// | Structural | YES | NO  | `notParent` only             |
/// | Wrapper    | NO  | YES | `requiresChild` only         |
/// | Allowed    | NO  | NO  | Neither (regex inclusion only)|
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeStrength {
    /// Valid nesting shown in CSS descendant selectors or examples,
    /// but not the only valid placement. No conformance rule generated.
    #[default]
    Allowed = 0,
    /// Child must be inside this parent when used (CHP=YES, PMC=NO).
    /// CSS `>`, CSS grid, React context, DOM nesting (non-container).
    /// Generates `notParent` rules only.
    Structural = 1,
    /// Parent must contain this child (PMC=YES, CHP=NO).
    /// cloneElement, internal rendering (unconditional).
    /// Generates `requiresChild` rules only.
    Wrapper = 2,
    /// Both directions required (CHP=YES, PMC=YES).
    /// DOM nesting for pure containers, or Structural + Wrapper combined.
    /// Generates both `notParent` and `requiresChild` rules.
    Required = 3,
}

impl EdgeStrength {
    /// Whether this strength implies child-must-have-parent (CHP).
    /// Used by conformance rule generation for `notParent` rules.
    pub fn child_requires_parent(&self) -> bool {
        matches!(self, EdgeStrength::Required | EdgeStrength::Structural)
    }

    /// Whether this strength implies parent-must-have-child (PMC).
    /// Used by conformance rule generation for `requiresChild` rules.
    pub fn parent_requires_child(&self) -> bool {
        matches!(self, EdgeStrength::Required | EdgeStrength::Wrapper)
    }

    /// Combine two strengths when multiple signals create the same edge.
    /// Each dimension is ORed independently:
    /// - If ANY signal says CHP=YES, the result has CHP=YES.
    /// - If ANY signal says PMC=YES, the result has PMC=YES.
    pub fn combine(&self, other: &EdgeStrength) -> EdgeStrength {
        let chp = self.child_requires_parent() || other.child_requires_parent();
        let pmc = self.parent_requires_child() || other.parent_requires_child();
        match (chp, pmc) {
            (true, true) => EdgeStrength::Required,
            (true, false) => EdgeStrength::Structural,
            (false, true) => EdgeStrength::Wrapper,
            (false, false) => EdgeStrength::Allowed,
        }
    }

    /// Compute the collapsed strength through a chain of two edges
    /// (A →outer→ B →inner→ C), producing the transitive edge A → C.
    ///
    /// `self` is the outer edge (A→B), `child_edge` is the inner edge (B→C).
    ///
    /// CHP (C must be inside A): The inner child (C) must be inside the
    /// intermediate (B) — inner.CHP — AND the intermediate is guaranteed
    /// to be inside the transitive parent (A) — outer.PMC (A always
    /// renders B). If both hold, C transitively must be inside A.
    ///
    /// PMC (A must contain C): Only if ALL links say "parent requires
    /// child" — outer.PMC AND inner.PMC.
    pub fn collapse_chain(&self, child_edge: &EdgeStrength) -> EdgeStrength {
        let chp = child_edge.child_requires_parent() && self.parent_requires_child();
        let pmc = self.parent_requires_child() && child_edge.parent_requires_child();
        match (chp, pmc) {
            (true, true) => EdgeStrength::Required,
            (true, false) => EdgeStrength::Structural,
            (false, true) => EdgeStrength::Wrapper,
            (false, false) => EdgeStrength::Allowed,
        }
    }
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
    /// Child is passed to the parent via a named `ReactNode`/`ReactElement`
    /// prop rather than placed as a JSX child. The prop name is stored in
    /// the edge's `prop_name` field.
    PropPassed,
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

    /// CSS classes where a version prefix swap produces a non-existent class.
    ///
    /// Each entry is `(old_class, dead_swapped_class)`. For example,
    /// `("pf-v5-c-form__actions--right", "pf-v6-c-form__actions--right")`
    /// when the `__actions--right` BEM modifier was removed in v6.
    /// Used to generate rules that flag these dead classes and prevent
    /// the `CssVariablePrefix` fix from creating broken CSS.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dead_css_classes_after_swap: Vec<(String, String)>,

    /// Full CSS class inventory from the old (from) version of the dep repo.
    /// Used to generate enumerated per-class rules.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub old_css_class_inventory: HashSet<String>,

    /// Full CSS class inventory from the new (to) version of the dep repo.
    /// Used together with `old_css_class_inventory` for class rename vs removal.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub new_css_class_inventory: HashSet<String>,

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

/// How a deprecated replacement was detected.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReplacementEvidence {
    /// Detected via host components swapping rendered components
    /// (e.g., ToolbarFilter stopped rendering `Chip`, started rendering `Label`).
    #[default]
    RenderingSwap,
    /// Detected via git commit co-change analysis: the commit that deprecated
    /// the component also modified source files in the replacement component's
    /// directory (e.g., the Tile deprecation commit also modified `Card/CardHeader.tsx`).
    CommitCoChange,
}

/// A deprecated component that has a differently-named replacement,
/// detected via rendering swap analysis or git commit co-change analysis.
///
/// When host components (e.g., ToolbarFilter) stopped rendering `Chip`
/// and started rendering `Label` between versions, this establishes
/// the `Chip → Label` replacement relationship.
///
/// When no rendering swap is available (e.g., Tile is a standalone leaf
/// component), the commit that deprecated Tile may also modify Card's
/// source files — establishing the `Tile → Card` relationship via
/// co-change analysis.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeprecatedReplacement {
    /// The deprecated component name (e.g., "Chip").
    pub old_component: String,
    /// The replacement component name (e.g., "Label").
    pub new_component: String,
    /// Evidence details: host component names for `RenderingSwap`,
    /// commit SHAs for `CommitCoChange`.
    pub evidence_hosts: Vec<String>,
    /// How this replacement was detected.
    #[serde(default)]
    pub evidence_source: ReplacementEvidence,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── EdgeStrength::combine tests ─────────────────────────────────

    #[test]
    fn test_combine_structural_plus_wrapper_equals_required() {
        // CSS > (Structural) + cloneElement (Wrapper) = Required
        assert_eq!(
            EdgeStrength::Structural.combine(&EdgeStrength::Wrapper),
            EdgeStrength::Required,
        );
        // Order shouldn't matter
        assert_eq!(
            EdgeStrength::Wrapper.combine(&EdgeStrength::Structural),
            EdgeStrength::Required,
        );
    }

    #[test]
    fn test_combine_allowed_with_structural() {
        assert_eq!(
            EdgeStrength::Allowed.combine(&EdgeStrength::Structural),
            EdgeStrength::Structural,
        );
    }

    #[test]
    fn test_combine_allowed_with_wrapper() {
        assert_eq!(
            EdgeStrength::Allowed.combine(&EdgeStrength::Wrapper),
            EdgeStrength::Wrapper,
        );
    }

    #[test]
    fn test_combine_required_dominates() {
        assert_eq!(
            EdgeStrength::Required.combine(&EdgeStrength::Allowed),
            EdgeStrength::Required,
        );
        assert_eq!(
            EdgeStrength::Required.combine(&EdgeStrength::Structural),
            EdgeStrength::Required,
        );
        assert_eq!(
            EdgeStrength::Required.combine(&EdgeStrength::Wrapper),
            EdgeStrength::Required,
        );
    }

    #[test]
    fn test_combine_allowed_stays_allowed() {
        assert_eq!(
            EdgeStrength::Allowed.combine(&EdgeStrength::Allowed),
            EdgeStrength::Allowed,
        );
    }

    // ── EdgeStrength::collapse_chain tests ───────────────────────────

    #[test]
    fn test_collapse_modal_chain_wrapper_then_structural() {
        // Modal(Wrapper) → ModalContent → ModalBody(Structural)
        // Wrapper: Modal always renders ModalContent (PMC=YES)
        // Structural: ModalBody must be inside ModalContent (CHP=YES)
        // Collapsed: ModalBody must be inside Modal (CHP=YES from
        // inner, because outer PMC guarantees intermediate is present)
        // PMC=NO (Structural inner doesn't have PMC)
        let outer = EdgeStrength::Wrapper;
        let inner = EdgeStrength::Structural;
        assert_eq!(
            outer.collapse_chain(&inner),
            EdgeStrength::Structural,
            "Modal→ModalBody should be Structural after collapse"
        );
    }

    #[test]
    fn test_collapse_required_chain() {
        // Table(Required) → Tbody → Tr(Required)
        // Both links have CHP+PMC → collapsed is Required
        let outer = EdgeStrength::Required;
        let inner = EdgeStrength::Required;
        assert_eq!(
            outer.collapse_chain(&inner),
            EdgeStrength::Required,
            "Required + Required = Required"
        );
    }

    #[test]
    fn test_collapse_wrapper_chain() {
        // A(Wrapper) → B → C(Wrapper)
        // A always renders B, B always renders C
        // C doesn't need to be inside A (CHP=NO for both)
        // But A transitively needs C (PMC=YES for both)
        let outer = EdgeStrength::Wrapper;
        let inner = EdgeStrength::Wrapper;
        assert_eq!(
            outer.collapse_chain(&inner),
            EdgeStrength::Wrapper,
            "Wrapper + Wrapper = Wrapper (parent still needs child)"
        );
    }

    #[test]
    fn test_collapse_structural_then_wrapper() {
        // A(Structural) → B → C(Wrapper)
        // Structural outer: A doesn't render B (PMC=NO)
        // So even though B→C is Wrapper (B needs C),
        // A→C has no constraint (A doesn't guarantee B is present)
        let outer = EdgeStrength::Structural;
        let inner = EdgeStrength::Wrapper;
        assert_eq!(
            outer.collapse_chain(&inner),
            EdgeStrength::Allowed,
            "Structural + Wrapper = Allowed (outer doesn't guarantee intermediate)"
        );
    }

    #[test]
    fn test_collapse_wrapper_then_allowed() {
        // A(Wrapper) → B → C(Allowed)
        // Inner is Allowed: C has no constraints relative to B
        // Collapsed: C has no constraints relative to A
        let outer = EdgeStrength::Wrapper;
        let inner = EdgeStrength::Allowed;
        assert_eq!(
            outer.collapse_chain(&inner),
            EdgeStrength::Allowed,
            "Wrapper + Allowed = Allowed (inner has no constraints)"
        );
    }

    #[test]
    fn test_collapse_allowed_kills_everything() {
        // If outer is Allowed, nothing propagates
        let outer = EdgeStrength::Allowed;
        assert_eq!(
            outer.collapse_chain(&EdgeStrength::Required),
            EdgeStrength::Allowed
        );
        assert_eq!(
            outer.collapse_chain(&EdgeStrength::Structural),
            EdgeStrength::Allowed
        );
        assert_eq!(
            outer.collapse_chain(&EdgeStrength::Wrapper),
            EdgeStrength::Allowed
        );
    }

    #[test]
    fn test_collapse_required_then_structural() {
        // A(Required) → B → C(Structural)
        // CHP: inner.CHP(true) AND outer.PMC(true) = true → C must be inside A
        // PMC: outer.PMC(true) AND inner.PMC(false) = false → A doesn't need C
        let outer = EdgeStrength::Required;
        let inner = EdgeStrength::Structural;
        assert_eq!(
            outer.collapse_chain(&inner),
            EdgeStrength::Structural,
            "Required + Structural = Structural (child constraint propagates, parent constraint doesn't)"
        );
    }

    #[test]
    fn test_collapse_required_then_wrapper() {
        // A(Required) → B → C(Wrapper)
        // CHP: inner.CHP(false) AND outer.PMC(true) = false
        // PMC: outer.PMC(true) AND inner.PMC(true) = true
        let outer = EdgeStrength::Required;
        let inner = EdgeStrength::Wrapper;
        assert_eq!(
            outer.collapse_chain(&inner),
            EdgeStrength::Wrapper,
            "Required + Wrapper = Wrapper (parent needs child transitively)"
        );
    }

    // ── EdgeStrength dimension accessor tests ────────────────────────

    #[test]
    fn test_strength_dimensions() {
        assert!(!EdgeStrength::Allowed.child_requires_parent());
        assert!(!EdgeStrength::Allowed.parent_requires_child());

        assert!(EdgeStrength::Structural.child_requires_parent());
        assert!(!EdgeStrength::Structural.parent_requires_child());

        assert!(!EdgeStrength::Wrapper.child_requires_parent());
        assert!(EdgeStrength::Wrapper.parent_requires_child());

        assert!(EdgeStrength::Required.child_requires_parent());
        assert!(EdgeStrength::Required.parent_requires_child());
    }
}
