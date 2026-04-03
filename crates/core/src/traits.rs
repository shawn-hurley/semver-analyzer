//! Trait definitions for language-pluggable analysis.
//!
//! Adding a new language means implementing these traits. The orchestrator,
//! diff engine, and output format are language-agnostic and reused unchanged.
//!
//! ## Trait ownership
//!
//! | Trait | Used by | Per-language? |
//! |---|---|---|
//! | `Language` | TD + BU | Yes (unified analysis pipeline) |
//! | `BehaviorAnalyzer` | BU | No (language-agnostic, LLM-based) |

use crate::types::{
    ApiSurface, BehavioralChangeKind, BodyAnalysisResult, BreakingVerdict, Caller, ChangeSubject,
    ChangedFunction, EvidenceType, ExpectedChild, FunctionSpec, Reference, StructuralChange,
    StructuralChangeType, Symbol, SymbolKind, TestDiff, TestFile, Visibility,
};
use anyhow::Result;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::fmt::Debug;
use std::path::Path;

// ── BU Traits (language-agnostic, LLM-based) ───────────────────────────

/// Analyze behavioral changes via LLM-based spec inference.
///
/// Language-agnostic: the function body and signature are passed as
/// strings. The LLM generates template-constrained `FunctionSpec`
/// objects, which are compared mechanically (Tier 1) or via LLM
/// fallback (Tier 2).
///
/// Implementations may use:
/// - Direct LLM API calls (OpenAI, Anthropic, etc.)
/// - `goose run --no-session -q -t "..."`
/// - `opencode run "..."`
/// - Any other agent CLI via `--llm-command`
pub trait BehaviorAnalyzer {
    /// Infer a function's behavioral spec from its body alone.
    ///
    /// Lower confidence than `infer_spec_with_test_context` because
    /// the LLM has no grounded examples of expected behavior.
    fn infer_spec(&self, function_body: &str, signature: &str) -> Result<FunctionSpec>;

    /// Infer a spec with additional context from the test file.
    ///
    /// The test assertions give the LLM concrete examples of expected
    /// behavior — reducing hallucination compared to body-only inference.
    fn infer_spec_with_test_context(
        &self,
        function_body: &str,
        signature: &str,
        test_context: &TestDiff,
    ) -> Result<FunctionSpec>;

    /// Compare two specs and determine if the change is breaking.
    ///
    /// Uses a two-tier approach:
    /// - Tier 1: Structural comparison on `FunctionSpec` fields
    /// - Tier 2: LLM fallback for `notes` diffs and ambiguous matches
    fn specs_are_breaking(&self, old: &FunctionSpec, new: &FunctionSpec)
        -> Result<BreakingVerdict>;

    /// Check whether a caller propagates a behavioral break from a callee.
    ///
    /// Given a caller's body/signature and evidence of a behavioral
    /// break in a callee it invokes, determine whether the caller's
    /// observable behavior actually changes. The caller might absorb
    /// the break by:
    ///   - Ignoring the callee's return value
    ///   - Catching and handling the callee's new error behavior
    ///   - Only invoking the callee on code paths that don't trigger
    ///     the behavioral change
    ///   - Applying its own validation that masks the change
    ///
    /// Returns true if the break propagates (caller IS affected),
    /// false if the caller absorbs it (NOT affected).
    fn check_propagation(
        &self,
        caller_body: &str,
        caller_signature: &str,
        callee_name: &str,
        evidence_description: &str,
    ) -> Result<bool>;
}

// ── Language abstraction traits (multi-language architecture) ────────────
//
// These traits define the integration point for multi-language support.
// See `design/01-traits.md` for detailed documentation.

/// Language-specific semantic rules consumed by the diff engine.
///
/// These encode the places where "is this breaking?" or "are these related?"
/// differ fundamentally by language. The diff engine calls these methods
/// instead of hardcoding language-specific rules.
pub trait LanguageSemantics {
    /// Is adding this member to this container a breaking change?
    ///
    /// This is the single rule that differs most fundamentally by language:
    /// - TypeScript: breaking only if the member is required (non-optional).
    /// - Go: ALWAYS breaking for interfaces (all implementors must add it).
    /// - Java: breaking for abstract methods, not for default methods.
    /// - C#: breaking for abstract members on interfaces.
    /// - Python: breaking for abstract methods on Protocol/ABC.
    fn is_member_addition_breaking(&self, container: &Symbol, member: &Symbol) -> bool;

    /// Are these two symbols part of the same logical family/group?
    ///
    /// Used to scope migration detection. When a symbol is removed, only
    /// symbols in the same family are considered as potential absorption targets.
    ///
    /// - TypeScript/React: same component directory
    /// - Go: same package
    /// - Java: same package
    /// - Python: same module
    fn same_family(&self, a: &Symbol, b: &Symbol) -> bool;

    /// Are these two symbols the same concept, possibly at different paths?
    ///
    /// When true, migration detection does a full member comparison (all members,
    /// not just newly-added ones) because the candidate is assumed to be a direct
    /// replacement for the removed symbol.
    ///
    /// Resolves companion types linked by naming convention:
    /// - TypeScript: `Button` and `ButtonProps` (component + its props interface)
    /// - Go: `Client` and `ClientOptions` (struct + its configuration)
    /// - Java: `UserService` and `UserServiceImpl` (interface + implementation)
    fn same_identity(&self, a: &Symbol, b: &Symbol) -> bool;

    /// Numeric rank for a visibility level (higher = more visible).
    ///
    /// Used to determine if visibility was reduced (breaking) or increased.
    /// The ordering differs by language:
    /// - TypeScript: Private(0) < Internal(1) < Protected(1) < Public(2) < Exported(3)
    /// - Java: Private(0) < PackagePrivate(1) < Protected(2) < Public(3)
    /// - Go: Internal(0) < Exported(1)
    fn visibility_rank(&self, v: Visibility) -> u8;

    /// Parse union/constrained type values for fine-grained diffing.
    ///
    /// TypeScript: parse `'primary' | 'secondary' | 'danger'`.
    /// Python: parse `Literal['a', 'b']`.
    /// Most other languages return `None`.
    fn parse_union_values(&self, _type_str: &str) -> Option<BTreeSet<String>> {
        None
    }

    /// Whether a return type string represents an async wrapper.
    ///
    /// Used by the diff engine to detect sync→async and async→sync changes,
    /// which are always breaking regardless of the inner type.
    ///
    /// TypeScript/JavaScript: `Promise<T>`
    /// Python: `Coroutine[...]`, `Awaitable[...]`
    /// Java: `CompletableFuture<T>`, `Future<T>`
    /// Go: returns `false` (async handled via goroutines, not return types)
    fn is_async_wrapper(&self, _type_str: &str) -> bool {
        false
    }

    /// Format an import/use statement change hint for migration descriptions.
    ///
    /// When a symbol is renamed across packages, the diff engine includes
    /// import guidance so consumers know to update their import paths.
    ///
    /// TypeScript: `"replace \`import { X } from 'old-pkg'\` with \`import { X } from 'new-pkg'\`"`
    /// Go: `"replace \`\"old/pkg\"\` with \`\"new/pkg\"\`"`
    /// Default: generic format without language-specific syntax.
    fn format_import_change(&self, symbol: &str, old_path: &str, new_path: &str) -> String {
        format!(
            "replace import of `{}` from `{}` with `{}`",
            symbol, old_path, new_path,
        )
    }

    /// Post-process the change list before returning from diff_surfaces.
    ///
    /// TypeScript: dedup default export changes.
    /// Most languages: no-op.
    fn post_process(&self, _changes: &mut Vec<StructuralChange>) {}

    /// If this language supports component hierarchy inference (e.g., React,
    /// Vue, Django templates), return the hierarchy semantics implementation.
    ///
    /// The orchestrator uses this to prepare data for LLM hierarchy inference.
    /// The trait is NOT responsible for LLM calls or prompt construction.
    fn hierarchy(&self) -> Option<&dyn HierarchySemantics> {
        None
    }

    /// If this language supports LLM-based rename inference (e.g., CSS
    /// physical→logical property renames, interface rename mappings),
    /// return the rename semantics implementation.
    ///
    /// The orchestrator uses this to prepare data for LLM rename inference.
    /// The trait is NOT responsible for LLM calls or prompt construction.
    fn renames(&self) -> Option<&dyn RenameSemantics> {
        None
    }

    /// If this language has deterministic body-level analysis (e.g., JSX diff,
    /// CSS variable scanning for TypeScript), return the body analysis
    /// implementation.
    ///
    /// The orchestrator calls this during BU Phase 1 to detect behavioral
    /// breaks from function body changes without LLM assistance.
    fn body_analyzer(&self) -> Option<&dyn BodyAnalysisSemantics> {
        None
    }
}

// ── Optional capability traits ──────────────────────────────────────────
//
// These traits represent optional analysis capabilities that some languages
// support. They are accessed via optional accessors on `LanguageSemantics`.
// The orchestrator checks for their presence and conditionally runs the
// corresponding analysis steps.

/// Deterministic data preparation for component hierarchy inference.
///
/// Languages with component composition models (React, Vue, Django, etc.)
/// implement this to tell the orchestrator what files belong to a component
/// family and how families relate to each other.
///
/// The orchestrator uses `same_family` for symbol grouping, then these
/// methods for data preparation. The LLM call itself stays in the orchestrator.
///
/// TODO: Reconsider — the methods that take repo/git_ref currently require
/// language impls to know about git. A future refactor should have the
/// orchestrator own all git plumbing and pass content to pure-logic methods.
pub trait HierarchySemantics {
    /// Get file paths belonging to a component family directory.
    ///
    /// Given a family name (e.g., "Dropdown"), returns relative paths to
    /// all source files in that family. Used to read content for the LLM prompt.
    fn family_source_paths(&self, repo: &Path, git_ref: &str, family_name: &str) -> Vec<String>;

    /// Get a human-readable family name from a group of symbols.
    ///
    /// TypeScript/React: extracts the component directory name
    /// (e.g., "Dropdown" from "packages/react-core/src/components/Dropdown/...")
    fn family_name_from_symbols(&self, symbols: &[&Symbol]) -> Option<String>;

    /// Detect cross-family relationships (e.g., React context imports).
    ///
    /// Returns pairs of (consumer_family, provider_family, relationship_name).
    /// Used to include related component signatures in the LLM prompt.
    fn cross_family_relationships(
        &self,
        repo: &Path,
        git_ref: &str,
    ) -> Vec<(String, String, String)>;

    /// Read related component signatures for cross-family context.
    ///
    /// Given a provider family and the context/relationship names that
    /// link it to a consumer, returns relevant source content to include
    /// in the LLM prompt.
    fn related_family_content(
        &self,
        repo: &Path,
        git_ref: &str,
        family_name: &str,
        relationship_names: &[String],
    ) -> Option<String>;

    /// Whether a symbol is a candidate for hierarchy inference.
    ///
    /// The orchestrator calls this to filter symbols when grouping into
    /// families. Only candidates are counted toward the minimum threshold.
    ///
    /// TypeScript/React: PascalCase Variable/Class/Function/Constant
    /// (React components are PascalCase functions or classes).
    fn is_hierarchy_candidate(&self, sym: &Symbol) -> bool;

    /// Minimum number of exported types for a family to qualify
    /// for hierarchy inference. Default: 2.
    fn min_components_for_hierarchy(&self) -> usize {
        2
    }

    /// Compute component hierarchy deterministically from three signals:
    ///
    /// 1. **Prop absorption** (`structural_changes`): If a parent component
    ///    had props removed, and those props now exist on a new family member,
    ///    that member is a consumer child of the parent.
    ///
    /// 2. **Cross-family extends mapping** (`rendered_components` + `extends`):
    ///    If component A renders component X from a different family, and
    ///    component B's props interface extends X's props interface, then B
    ///    is a consumer child of A (B wraps X, which is rendered by A).
    ///
    /// 3. **Internal rendering** (`rendered_components`): If a parent renders
    ///    a child internally, that child is prop-passed, not a direct JSX child.
    ///
    /// The method works on the NEW surface and structural changes. It returns
    /// the expected hierarchy for the new version.
    fn compute_deterministic_hierarchy(
        &self,
        new_surface: &ApiSurface,
        structural_changes: &[StructuralChange],
    ) -> HashMap<String, HashMap<String, Vec<ExpectedChild>>> {
        use std::collections::{BTreeMap, HashSet};

        // ── Index: group hierarchy candidates by family ──────────────

        let mut families: HashMap<String, Vec<&Symbol>> = HashMap::new();
        for sym in &new_surface.symbols {
            if !self.is_hierarchy_candidate(sym) {
                continue;
            }
            if let Some(family) = self.family_name_from_symbols(&[sym]) {
                families.entry(family).or_default().push(sym);
            }
        }

        // ── Index: interface extends map ─────────────────────────────
        //
        // Maps interface name → what it extends.
        // e.g., "DropdownProps" → "MenuProps"
        // Also maps "DropdownListProps" → "MenuListProps".
        let mut iface_extends: HashMap<&str, &str> = HashMap::new();
        for sym in &new_surface.symbols {
            if sym.kind == SymbolKind::Interface {
                if let Some(ext) = &sym.extends {
                    iface_extends.insert(&sym.name, ext.as_str());
                }
            }
        }

        // ── Index: component → props interface name ──────────────────
        //
        // Convention: component "Dropdown" → props interface "DropdownProps".
        // We verify the interface actually exists in the surface.
        let iface_names: HashSet<&str> = new_surface
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .map(|s| s.name.as_str())
            .collect();

        // ── Index: props interface → component name ──────────────────
        //
        // Reverse mapping: "MenuProps" → "Menu", "MenuListProps" → "MenuList"
        // Used for cross-family extends resolution.
        let mut props_to_component: HashMap<String, &str> = HashMap::new();
        for sym in &new_surface.symbols {
            if !self.is_hierarchy_candidate(sym) {
                continue;
            }
            let props_name = format!("{}Props", sym.name);
            if iface_names.contains(props_name.as_str()) {
                props_to_component.insert(props_name, &sym.name);
            }
        }

        // ── Signal 1: Prop absorption ────────────────────────────────
        //
        // For each parent interface with removed members, find new family
        // members whose props interface has matching member names.
        // parent_component → set of child component names that absorbed props.

        // First, collect removed members per parent symbol.
        // StructuralChange for "Modal.title" means member "title" removed
        // from the symbol named "Modal" (or its Props interface).
        let mut removed_props_by_parent: HashMap<String, HashSet<String>> = HashMap::new();
        for change in structural_changes {
            if let StructuralChangeType::Removed(ChangeSubject::Member { name, .. }) =
                &change.change_type
            {
                // The parent is the symbol that lost the member.
                // change.symbol is "Modal.title" → parent is "Modal"
                // OR change.symbol is "ModalProps" and member is "title"
                let parent = if let Some((p, _)) = change.symbol.rsplit_once('.') {
                    // "ModalProps.title" → "ModalProps" → strip "Props" → "Modal"
                    p.strip_suffix("Props").unwrap_or(p).to_string()
                } else {
                    // Just "ModalProps" → strip "Props" → "Modal"
                    change
                        .symbol
                        .strip_suffix("Props")
                        .unwrap_or(&change.symbol)
                        .to_string()
                };
                removed_props_by_parent
                    .entry(parent)
                    .or_default()
                    .insert(name.clone());
            }
        }

        // For each family, check which new members absorbed removed props.
        let mut absorption_children: HashMap<String, BTreeMap<String, Vec<String>>> =
            HashMap::new();

        for (_family_name, members) in &families {
            for parent in members.iter() {
                let removed = match removed_props_by_parent.get(&parent.name) {
                    Some(r) if !r.is_empty() => r,
                    _ => continue,
                };

                // Find family members that have matching prop names
                for candidate in members.iter() {
                    if candidate.name == parent.name {
                        continue;
                    }

                    // Check candidate's own members
                    let candidate_props: HashSet<&str> =
                        candidate.members.iter().map(|m| m.name.as_str()).collect();

                    // Also check the candidate's Props interface members
                    let props_iface_name = format!("{}Props", candidate.name);
                    let iface_props: HashSet<&str> = new_surface
                        .symbols
                        .iter()
                        .find(|s| s.name == props_iface_name && s.kind == SymbolKind::Interface)
                        .map(|s| s.members.iter().map(|m| m.name.as_str()).collect())
                        .unwrap_or_default();

                    let all_candidate_props: HashSet<&str> =
                        candidate_props.union(&iface_props).copied().collect();

                    let absorbed: Vec<String> = removed
                        .iter()
                        .filter(|prop| all_candidate_props.contains(prop.as_str()))
                        .cloned()
                        .collect();

                    if !absorbed.is_empty() {
                        absorption_children
                            .entry(parent.name.clone())
                            .or_default()
                            .insert(candidate.name.clone(), absorbed);
                    }
                }
            }
        }

        // ── Signal 2: Cross-family extends mapping ───────────────────
        //
        // If Dropdown renders Menu (from Menu family), and DropdownList's
        // props extend MenuListProps → DropdownList maps to MenuList.
        // If Menu's hierarchy says "Menu renders MenuList internally" (or
        // MenuList is a known child of Menu), then DropdownList is a child
        // of Dropdown by the same relationship.
        //
        // Build: for each family, a map of
        //   component → {rendered_external_component → external_component's family component}
        // Then: for each family member whose props extend an external component's
        // props, it maps to that external component.

        // extends_map: family member → external component it wraps
        // e.g., "Dropdown" → "Menu", "DropdownList" → "MenuList"
        let mut extends_map: HashMap<&str, &str> = HashMap::new();
        for (_, members) in &families {
            for sym in members {
                let props_name = format!("{}Props", sym.name);
                if let Some(ext_iface) = iface_extends.get(props_name.as_str()) {
                    // Strip Omit<...> wrapper if present — just get the base name
                    let ext_clean = ext_iface
                        .strip_prefix("Omit<")
                        .and_then(|s| s.split(',').next())
                        .unwrap_or(ext_iface);
                    if let Some(ext_component) = props_to_component.get(ext_clean) {
                        // Only cross-family: the extended component should NOT be
                        // in the same family.
                        let ext_family = self.family_name_from_symbols(&[
                            // Find the actual Symbol for the extended component
                            new_surface
                                .symbols
                                .iter()
                                .find(|s| s.name.as_str() == *ext_component)
                                .unwrap_or(sym),
                        ]);
                        let own_family = self.family_name_from_symbols(&[sym]);
                        if ext_family != own_family {
                            extends_map.insert(&sym.name, ext_component);
                        }
                    }
                }
            }
        }

        // ── Combine signals into hierarchy ───────────────────────────

        let mut result: HashMap<String, HashMap<String, Vec<ExpectedChild>>> = HashMap::new();

        for (family_name, members) in &families {
            let member_names: HashSet<&str> = members.iter().map(|s| s.name.as_str()).collect();
            let mut family_hierarchy: HashMap<String, Vec<ExpectedChild>> = HashMap::new();

            // What each component renders from the family (Signal 3: internal rendering)
            let mut renders_family: HashMap<&str, HashSet<&str>> = HashMap::new();
            for sym in members {
                let family_renders: HashSet<&str> = sym
                    .rendered_components
                    .iter()
                    .filter(|r| {
                        member_names.contains(r.as_str()) && r.as_str() != sym.name.as_str()
                    })
                    .map(|r| r.as_str())
                    .collect();
                if !family_renders.is_empty() {
                    renders_family.insert(&sym.name, family_renders);
                }
            }

            for parent in members.iter() {
                let mut children: BTreeMap<&str, ExpectedChild> = BTreeMap::new();

                // ── Signal 1: absorption ─────────────────────────────
                // Children that absorbed removed props from this parent.
                if let Some(absorbed) = absorption_children.get(&parent.name) {
                    for (child_name, _absorbed_props) in absorbed {
                        if !member_names.contains(child_name.as_str()) {
                            continue;
                        }
                        // Is this child rendered internally by parent? → prop-passed
                        let parent_renders = renders_family.get(parent.name.as_str());
                        let is_rendered = parent_renders
                            .map(|r| r.contains(child_name.as_str()))
                            .unwrap_or(false);

                        let child = if is_rendered {
                            // Parent renders it internally → prop-passed
                            ExpectedChild {
                                name: child_name.clone(),
                                required: false,
                                mechanism: "prop".to_string(),
                                prop_name: None,
                            }
                        } else {
                            // Parent does NOT render it → direct JSX child
                            ExpectedChild::new(child_name, false)
                        };
                        children.insert(child_name.as_str(), child);
                    }
                }

                // ── Signal 2: cross-family extends mapping ───────────
                // If this parent extends an external component (e.g.,
                // Dropdown extends Menu) AND renders it internally, find
                // which family members extend the external component's
                // children.
                //
                // The "renders it internally" check prevents inverse
                // mappings: DropdownList extends MenuList but does NOT
                // render Menu, so it should not map Menu's children as
                // its own children.
                if let Some(ext_parent) = extends_map.get(parent.name.as_str()) {
                    // Gate: parent must render the external parent in its JSX,
                    // AND the external parent must be a container (renders
                    // family members). Without the container check, leaf
                    // wrappers like DropdownList (renders MenuList) would
                    // incorrectly claim siblings as children.
                    let renders_ext_parent = parent
                        .rendered_components
                        .iter()
                        .any(|r| r.as_str() == *ext_parent);

                    // Check that ext_parent is a container in its family:
                    // it must render at least one component from the same
                    // external family.
                    let ext_parent_sym = new_surface
                        .symbols
                        .iter()
                        .find(|s| s.name.as_str() == *ext_parent);
                    let ext_parent_is_container = ext_parent_sym
                        .map(|ep| {
                            let ep_family = self.family_name_from_symbols(&[ep]);
                            ep.rendered_components.iter().any(|rc| {
                                new_surface
                                    .symbols
                                    .iter()
                                    .filter(|s| self.is_hierarchy_candidate(s))
                                    .any(|s| {
                                        s.name.as_str() == rc.as_str()
                                            && self.family_name_from_symbols(&[s]) == ep_family
                                    })
                            })
                        })
                        .unwrap_or(false);

                    if !renders_ext_parent || !ext_parent_is_container {
                        // Skip: either this component doesn't render the
                        // external parent, or the external parent isn't a
                        // container in its family.
                    } else {
                        if let Some(ext_sym) = ext_parent_sym {
                            // For each family member that extends an external component,
                            // check if that external component is rendered by the
                            // external parent OR is a known child of it.
                            for candidate in members.iter() {
                                if candidate.name == parent.name {
                                    continue;
                                }
                                if children.contains_key(candidate.name.as_str()) {
                                    continue; // Already found via absorption
                                }

                                // Does this candidate extend something from the external family?
                                if let Some(ext_child) = extends_map.get(candidate.name.as_str()) {
                                    // ext_child is the external component this candidate wraps.
                                    // Is it rendered by the external parent?
                                    let ext_renders_child = ext_sym
                                        .rendered_components
                                        .contains(&ext_child.to_string());

                                    // Only add the child if:
                                    // 1. The external parent does NOT render it
                                    //    (consumers must provide it as JSX child)
                                    // 2. The candidate's external component is
                                    //    NOT itself a container (to prevent
                                    //    mapping parents as children — e.g.,
                                    //    Menu should not be a child of MenuGroup)
                                    if !ext_renders_child {
                                        // Check: is ext_child a container?
                                        let ext_child_sym = new_surface
                                            .symbols
                                            .iter()
                                            .find(|s| s.name.as_str() == *ext_child);
                                        let ext_child_is_container = ext_child_sym
                                            .map(|ec| {
                                                let ec_family =
                                                    self.family_name_from_symbols(&[ec]);
                                                ec.rendered_components.iter().any(|rc| {
                                                    new_surface
                                                        .symbols
                                                        .iter()
                                                        .filter(|s| self.is_hierarchy_candidate(s))
                                                        .any(|s| {
                                                            s.name.as_str() == rc.as_str()
                                                                && self
                                                                    .family_name_from_symbols(&[s])
                                                                    == ec_family
                                                        })
                                                })
                                            })
                                            .unwrap_or(false);

                                        if !ext_child_is_container {
                                            children.insert(
                                                &candidate.name,
                                                ExpectedChild::new(&candidate.name, false),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    } // else renders_ext_parent
                }

                if !children.is_empty() {
                    family_hierarchy.insert(parent.name.clone(), children.into_values().collect());
                }
            }

            if !family_hierarchy.is_empty() {
                result.insert(family_name.clone(), family_hierarchy);
            }
        }

        result
    }
}

/// Deterministic data preparation for LLM-based rename inference.
///
/// Languages that benefit from LLM-detected rename patterns (e.g., CSS
/// physical→logical property renames, interface rename mappings) implement
/// this to prepare the data for the LLM call.
///
/// The orchestrator calls these methods to build LLM inputs. The LLM call
/// itself and prompt construction stay in the orchestrator/LLM crate.
pub trait RenameSemantics {
    /// Sample removed constants for rename pattern inference.
    ///
    /// Default implementation returns the first 30. Language impls can
    /// prioritize certain suffixes/patterns for better LLM pattern discovery.
    fn sample_removed_constants<'a>(
        &self,
        removed: &[&'a str],
        _added: &[&'a str],
    ) -> Vec<&'a str> {
        removed.iter().take(30).copied().collect()
    }

    /// Sample added constants for rename pattern inference.
    ///
    /// Default implementation returns the first 30.
    fn sample_added_constants<'a>(&self, _removed: &[&'a str], added: &[&'a str]) -> Vec<&'a str> {
        added.iter().take(30).copied().collect()
    }

    /// Minimum count of removed constants to trigger rename inference.
    /// Default: 50.
    fn min_removed_for_constant_inference(&self) -> usize {
        50
    }

    /// Minimum count of removed interfaces to trigger interface rename
    /// inference. Default: 2.
    fn min_removed_for_interface_inference(&self) -> usize {
        2
    }
}

/// Deterministic body-level analysis for behavioral change detection.
///
/// Languages with framework-specific body patterns (e.g., JSX diff and CSS
/// variable scanning for TypeScript/React) implement this to detect
/// behavioral breaks from function body changes without LLM assistance.
///
/// The orchestrator calls `analyze_changed_body` during BU Phase 1 for each
/// changed function that passes visibility filtering.
///
/// The `category_label` field on results uses the serde serialization format
/// of the language's `Category` type. At the call site, the orchestrator
/// deserializes this into `L::Category` via serde.
pub trait BodyAnalysisSemantics {
    /// Run deterministic analysis on a changed function's body.
    ///
    /// Returns a list of (description, category_label) pairs representing
    /// behavioral breaks detected. The category_label is the string form
    /// of the language's Category enum (e.g., "dom_structure" for
    /// `TsCategory::DomStructure`).
    ///
    /// TypeScript: runs JSX diff + CSS variable scanning.
    /// Other languages: may check annotation changes, decorator changes, etc.
    fn analyze_changed_body(
        &self,
        old_body: &str,
        new_body: &str,
        func_name: &str,
        file_path: &str,
    ) -> Vec<BodyAnalysisResult>;
}

/// Language-specific human-readable descriptions for changes.
///
/// Each language owns its messaging entirely -- there is no generic
/// template in core. These descriptions are consumed by LLMs downstream,
/// so language-appropriate terminology matters.
pub trait MessageFormatter {
    /// Produce a human-readable description for a structural change.
    fn describe(&self, change: &StructuralChange) -> String;
}

/// The core language abstraction.
///
/// Composes `LanguageSemantics + MessageFormatter` and adds five associated
/// types representing language-specific data flowing through the pipeline.
///
/// Code that only needs semantic rules can take `&dyn LanguageSemantics`
/// (no generic parameter). Code that needs the associated types takes
/// `L: Language`.
pub trait Language: LanguageSemantics + MessageFormatter + Send + Sync + 'static {
    /// Behavioral change categories for this language.
    type Category: Debug + Clone + Serialize + DeserializeOwned + Eq + std::hash::Hash + Send + Sync;

    /// Manifest change types for this language's package system.
    type ManifestChangeType: Debug
        + Clone
        + Serialize
        + DeserializeOwned
        + Eq
        + PartialEq
        + Send
        + Sync;

    /// Evidence data carried on behavioral changes.
    type Evidence: Debug + Clone + Serialize + DeserializeOwned + Send + Sync;

    /// Language-specific report data.
    type ReportData: Debug + Clone + Serialize + DeserializeOwned + Send + Sync;

    /// Language-specific analysis extensions.
    ///
    /// Opaque data produced during analysis that core passes through
    /// without inspecting. The language implementation populates this
    /// during `run_extended_analysis()` and consumes it in `build_report()`.
    ///
    /// For TypeScript: SD pipeline results + hierarchy inference results.
    /// For languages without extended analysis: `()`.
    type AnalysisExtensions: Debug + Clone + Default + Serialize + DeserializeOwned + Send + Sync;

    // ── Constants ────────────────────────────────────────────────────

    /// Symbol kinds that represent type definitions eligible for rename inference.
    /// TypeScript: `&[SymbolKind::Interface, SymbolKind::Class]`
    /// Go: `&[SymbolKind::Struct, SymbolKind::Interface]`
    const RENAMEABLE_SYMBOL_KINDS: &'static [SymbolKind];

    /// Language identifier for serialization dispatch.
    const NAME: &'static str;

    /// Manifest file path(s) for this language's package system.
    ///
    /// TypeScript: `&["package.json"]`
    /// Go: `&["go.mod"]`
    /// Java: `&["pom.xml"]` or `&["build.gradle"]`
    ///
    /// TODO: Reconsider — the orchestrator currently reads these files via git
    /// and passes content to `diff_manifest_content`. A future refactor should
    /// unify all git plumbing in the orchestrator so language impls are pure
    /// content processors.
    const MANIFEST_FILES: &'static [&'static str];

    /// Source file glob patterns for `git diff --name-only` filtering.
    ///
    /// TypeScript: `&["*.ts", "*.tsx"]`
    /// Go: `&["*.go"]`
    /// Java: `&["*.java"]`
    ///
    /// TODO: Same reconsideration as MANIFEST_FILES.
    const SOURCE_FILE_PATTERNS: &'static [&'static str];

    // ── Analysis pipeline methods ───────────────────────────────────

    /// Extract the public API surface from source code at a git ref.
    ///
    /// The implementation is responsible for checking out the ref,
    /// running any required build steps, parsing the output, and
    /// cleaning up temporary files.
    fn extract(&self, repo: &Path, git_ref: &str) -> Result<ApiSurface>;

    /// Parse the diff between two git refs and identify all functions
    /// whose bodies changed (public AND private).
    fn parse_changed_functions(
        &self,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<Vec<ChangedFunction>>;

    /// Given a function, find what calls it (callers, not callees).
    fn find_callers(&self, file: &Path, symbol_name: &str) -> Result<Vec<Caller>>;

    /// Given a public symbol, find all references to it across the project.
    fn find_references(&self, file: &Path, symbol_name: &str) -> Result<Vec<Reference>>;

    /// Given a source file, find its associated test file(s) by convention.
    fn find_tests(&self, repo: &Path, source_file: &Path) -> Result<Vec<TestFile>>;

    /// Diff the test file between two refs. Returns changed assertion lines.
    fn diff_test_assertions(
        &self,
        repo: &Path,
        test_file: &TestFile,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<TestDiff>;

    // ── Methods ─────────────────────────────────────────────────────

    /// Diff manifest content between two versions.
    ///
    /// The orchestrator reads the manifest file(s) at both refs and passes
    /// the raw content here. The language interprets the format and determines
    /// what changed and whether it's breaking.
    ///
    /// TODO: Reconsider — same as above re: git plumbing ownership.
    fn diff_manifest_content(old: &str, new: &str) -> Vec<crate::types::ManifestChange<Self>>
    where
        Self: Sized;

    /// Whether a file path should be excluded from BU analysis.
    ///
    /// Filters out test files, build artifacts, index/barrel files, etc.
    /// TypeScript: excludes `index.ts`, `.d.ts`, `.test.`, `.spec.`,
    /// `__tests__/`, `dist/`
    ///
    /// TODO: Same reconsideration as above.
    fn should_exclude_from_analysis(path: &Path) -> bool;

    /// Build the language-specific report from analysis results.
    ///
    /// This is the primary report-building entry point. The Language owns
    /// the entire report construction — language-agnostic structure (grouping
    /// changes by file, counting breaks) AND language-specific enrichment
    /// (component detection, hierarchy, child components, etc.).
    ///
    /// The result is dropped into a `ReportEnvelope` by the caller.
    fn build_report(
        results: &crate::types::AnalysisResult<Self>,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
    ) -> crate::types::AnalysisReport<Self>
    where
        Self: Sized;

    // ── Behavioral change methods ───────────────────────────────

    /// Determine the behavioral change kind from the evidence type.
    /// TypeScript: LLM/body analysis → Class (component-level), test delta → Function
    /// Default: always Function
    fn behavioral_change_kind(&self, _evidence_type: &EvidenceType) -> BehavioralChangeKind {
        BehavioralChangeKind::Function
    }

    /// Extract symbol references from a behavioral change description.
    /// TypeScript: extracts PascalCase component names (e.g., `<Modal>`, `` `Button` ``)
    /// Default: empty vec
    fn extract_referenced_symbols(&self, _description: &str) -> Vec<String> {
        vec![]
    }

    /// Format a qualified name for display in reports.
    /// TypeScript: `src/Modal.tsx::Modal` → `Modal`
    /// Default: return the qualified name as-is
    fn display_name(&self, qualified_name: &str) -> String {
        qualified_name.to_string()
    }

    // ── Extended analysis (language-specific pipelines) ────────────

    /// Run all language-specific analysis beyond TD/BU.
    ///
    /// This is the single entry point for language-specific pipeline
    /// steps that core doesn't understand. The orchestrator calls this
    /// after TD completes and passes the results through to `build_report()`.
    ///
    /// For TypeScript: runs the SD pipeline + hierarchy inference.
    /// Default: returns empty extensions (no language-specific analysis).
    #[allow(unused_variables)]
    fn run_extended_analysis(
        &self,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
        structural_changes: &[crate::types::StructuralChange],
        old_surface: &ApiSurface,
        new_surface: &ApiSurface,
        llm_command: Option<&str>,
        dep_css_dir: Option<&Path>,
        no_llm: bool,
    ) -> Result<Self::AnalysisExtensions> {
        Ok(Self::AnalysisExtensions::default())
    }
}

// ── Convenience functions (TD) ──────────────────────────────────────────

/// Compare two API surfaces using language-specific semantic rules.
///
/// This is the primary entry point for the TD (Top-Down) pipeline.
/// The `semantics` parameter provides language-specific rules.
pub fn diff_surfaces_with_semantics(
    old: &ApiSurface,
    new: &ApiSurface,
    semantics: &dyn LanguageSemantics,
) -> Vec<StructuralChange> {
    crate::diff::diff_surfaces_with_semantics(old, new, semantics)
}

/// Compare two API surfaces using minimal semantics (no language-specific rules).
///
/// This uses `MinimalSemantics` which is language-agnostic: no member additions
/// are breaking, no union parsing, no post-processing. For language-aware
/// diffing, use `diff_surfaces_with_semantics` with a `LanguageSemantics` impl.
pub fn diff_surfaces(old: &ApiSurface, new: &ApiSurface) -> Vec<StructuralChange> {
    crate::diff::diff_surfaces(old, new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SymbolKind, Visibility};
    use std::path::PathBuf;

    /// Minimal HierarchySemantics impl for testing.
    struct TestHierarchy;

    impl HierarchySemantics for TestHierarchy {
        fn family_source_paths(
            &self,
            _repo: &Path,
            _git_ref: &str,
            _family_name: &str,
        ) -> Vec<String> {
            Vec::new()
        }

        fn family_name_from_symbols(&self, symbols: &[&Symbol]) -> Option<String> {
            symbols.first().and_then(|s| {
                s.file
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().to_string())
            })
        }

        fn cross_family_relationships(
            &self,
            _repo: &Path,
            _git_ref: &str,
        ) -> Vec<(String, String, String)> {
            Vec::new()
        }

        fn related_family_content(
            &self,
            _repo: &Path,
            _git_ref: &str,
            _family_name: &str,
            _relationship_names: &[String],
        ) -> Option<String> {
            None
        }

        fn is_hierarchy_candidate(&self, sym: &Symbol) -> bool {
            matches!(
                sym.kind,
                SymbolKind::Variable | SymbolKind::Function | SymbolKind::Constant
            ) && sym.name.starts_with(|c: char| c.is_ascii_uppercase())
        }
    }

    // ─── Test helpers ────────────────────────────────────────────────

    fn make_component(name: &str, family: &str, rendered: Vec<&str>) -> Symbol {
        let mut sym = Symbol::new(
            name,
            format!("src/components/{}/{}.{}", family, name, name),
            SymbolKind::Variable,
            Visibility::Exported,
            PathBuf::from(format!("src/components/{}/{}.d.ts", family, name)),
            1,
        );
        sym.rendered_components = rendered.into_iter().map(|s| s.to_string()).collect();
        sym
    }

    fn make_interface(
        name: &str,
        family: &str,
        extends: Option<&str>,
        members: Vec<&str>,
    ) -> Symbol {
        let mut sym = Symbol::new(
            name,
            format!("src/components/{}/{}.{}", family, name, name),
            SymbolKind::Interface,
            Visibility::Exported,
            PathBuf::from(format!("src/components/{}/{}.d.ts", family, name)),
            1,
        );
        sym.extends = extends.map(|e| e.to_string());
        sym.members = members
            .into_iter()
            .map(|m| {
                Symbol::new(
                    m,
                    format!("{}.{}", name, m),
                    SymbolKind::Variable,
                    Visibility::Exported,
                    PathBuf::from(format!("src/components/{}/{}.d.ts", family, name)),
                    1,
                )
            })
            .collect();
        sym
    }

    /// Create a structural change for a removed member.
    fn removed_member(parent: &str, member: &str) -> StructuralChange {
        StructuralChange {
            symbol: format!("{}.{}", parent, member),
            qualified_name: format!("src/components/X/{}.{}", parent, member),
            kind: SymbolKind::Interface,
            package: None,
            change_type: StructuralChangeType::Removed(ChangeSubject::Member {
                name: member.to_string(),
                kind: SymbolKind::Variable,
            }),
            before: None,
            after: None,
            description: format!("property `{}` was removed", member),
            is_breaking: true,
            impact: None,
            migration_target: None,
        }
    }

    fn child_names(
        result: &HashMap<String, HashMap<String, Vec<ExpectedChild>>>,
        family: &str,
        component: &str,
    ) -> Vec<String> {
        result
            .get(family)
            .and_then(|f| f.get(component))
            .map(|children| children.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default()
    }

    fn child_mechanism(
        result: &HashMap<String, HashMap<String, Vec<ExpectedChild>>>,
        family: &str,
        parent: &str,
        child: &str,
    ) -> Option<String> {
        result
            .get(family)
            .and_then(|f| f.get(parent))
            .and_then(|children| children.iter().find(|c| c.name == child))
            .map(|c| c.mechanism.clone())
    }

    fn has_entry(
        result: &HashMap<String, HashMap<String, Vec<ExpectedChild>>>,
        family: &str,
        component: &str,
    ) -> bool {
        result
            .get(family)
            .map(|f| f.contains_key(component))
            .unwrap_or(false)
    }

    // ═══════════════════════════════════════════════════════════════════
    // Signal 1: Prop absorption (Modal v5→v6 pattern)
    // ═══════════════════════════════════════════════════════════════════
    //
    // Modal had props (title, description, actions, footer, bodyAriaRole, etc.)
    // that were removed. These props now exist on new child components:
    //   - ModalHeader absorbed: title, description, help, titleIconVariant, titleLabel
    //   - ModalBody absorbed: bodyAriaRole (as "role")
    //   - ModalFooter absorbed: actions, footer
    //
    // Since Modal does NOT render ModalHeader/ModalBody/ModalFooter
    // internally (they're not in rendered_components), they are direct
    // JSX children (mechanism = "child").

    #[test]
    fn signal1_modal_v6_absorption() {
        let h = TestHierarchy;

        // New surface: v6 Modal family
        let new_surface = ApiSurface {
            symbols: vec![
                make_component("Modal", "Modal", vec!["ModalContent"]),
                make_component(
                    "ModalHeader",
                    "Modal",
                    vec!["ModalBoxDescription", "ModalBoxTitle"],
                ),
                make_component("ModalBody", "Modal", vec![]),
                make_component("ModalFooter", "Modal", vec![]),
                // ModalProps interface with no extends (standalone)
                make_interface(
                    "ModalProps",
                    "Modal",
                    None,
                    vec!["children", "className", "isOpen", "onClose", "variant"],
                ),
                // ModalHeaderProps has the absorbed props
                make_interface(
                    "ModalHeaderProps",
                    "Modal",
                    None,
                    vec![
                        "children",
                        "className",
                        "title",
                        "description",
                        "help",
                        "titleIconVariant",
                        "titleScreenReaderText",
                    ],
                ),
                // ModalBodyProps has bodyAriaRole as "role"
                make_interface(
                    "ModalBodyProps",
                    "Modal",
                    None,
                    vec!["children", "className", "role"],
                ),
                // ModalFooterProps has the absorbed actions
                make_interface(
                    "ModalFooterProps",
                    "Modal",
                    None,
                    vec!["children", "className"],
                ),
            ],
        };

        // Structural changes: Modal had these props removed
        let changes = vec![
            removed_member("ModalProps", "title"),
            removed_member("ModalProps", "description"),
            removed_member("ModalProps", "help"),
            removed_member("ModalProps", "titleIconVariant"),
            removed_member("ModalProps", "titleLabel"),
            removed_member("ModalProps", "bodyAriaRole"),
            removed_member("ModalProps", "actions"),
            removed_member("ModalProps", "footer"),
            removed_member("ModalProps", "header"),
            removed_member("ModalProps", "showClose"),
            removed_member("ModalProps", "hasNoBodyWrapper"),
        ];

        let result = h.compute_deterministic_hierarchy(&new_surface, &changes);

        // Modal should have consumer children from absorption
        assert!(
            has_entry(&result, "Modal", "Modal"),
            "Modal should be a parent"
        );
        let modal_children = child_names(&result, "Modal", "Modal");

        // ModalHeader absorbed title, description, help, titleIconVariant
        assert!(
            modal_children.contains(&"ModalHeader".to_string()),
            "ModalHeader absorbed props from Modal"
        );

        // ModalBody absorbed bodyAriaRole (as "role" — but "bodyAriaRole" is in removed set
        // and doesn't match "role" on ModalBody. However "actions" maps to ModalFooter
        // since ModalFooterProps doesn't have "actions" either... Let's check what DOES match)

        // ModalFooter: its props are [children, className] — "actions" and "footer"
        // are NOT in ModalFooterProps in our test. So ModalFooter won't be detected
        // via absorption unless "children" matches a removed prop name.
        // In the real pipeline, "children" is a removed prop name? No. "footer" is.
        // "footer" is not in ModalFooterProps members. So this test correctly shows
        // that absorption only works when the prop names actually match.

        // ModalHeader has "title", "description", "help", "titleIconVariant" which
        // match removed props from ModalProps → detected as child
        assert!(
            modal_children.contains(&"ModalHeader".to_string()),
            "ModalHeader absorbed title/description/help/titleIconVariant from Modal"
        );

        // Since Modal doesn't render ModalHeader internally → mechanism = "child"
        assert_eq!(
            child_mechanism(&result, "Modal", "Modal", "ModalHeader"),
            Some("child".to_string()),
            "ModalHeader is a direct JSX child (not rendered internally)"
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    // Signal 2: Cross-family extends (Dropdown wraps Menu)
    // ═══════════════════════════════════════════════════════════════════
    //
    // Dropdown.tsx renders Menu and MenuContent (from Menu family).
    // DropdownProps extends MenuProps.
    // DropdownListProps extends MenuListProps.
    // DropdownItemProps extends MenuItemProps.
    // DropdownGroupProps extends MenuGroupProps.
    //
    // Menu renders MenuContent internally. MenuContent is NOT rendered
    // by Dropdown's family members as a consumer child, but Menu's
    // relationship to MenuList/MenuGroup tells us:
    //   - Menu does NOT render MenuList/MenuGroup internally → they are
    //     consumer children of Menu
    //   - By extends mapping: DropdownList (extends MenuList) is a consumer
    //     child of Dropdown (extends Menu)

    #[test]
    fn signal2_dropdown_cross_family_extends() {
        let h = TestHierarchy;

        let new_surface = ApiSurface {
            symbols: vec![
                // Dropdown family components
                make_component(
                    "Dropdown",
                    "Dropdown",
                    vec!["Menu", "MenuContent", "Popper"],
                ),
                make_component("DropdownGroup", "Dropdown", vec!["MenuGroup"]),
                make_component("DropdownItem", "Dropdown", vec!["MenuItem"]),
                make_component("DropdownList", "Dropdown", vec!["MenuList"]),
                // Dropdown family interfaces with extends
                make_interface(
                    "DropdownProps",
                    "Dropdown",
                    Some("MenuProps"),
                    vec!["children", "className", "toggle", "isOpen", "onSelect"],
                ),
                make_interface(
                    "DropdownGroupProps",
                    "Dropdown",
                    Some("MenuGroupProps"),
                    vec!["children", "label"],
                ),
                make_interface(
                    "DropdownItemProps",
                    "Dropdown",
                    Some("MenuItemProps"),
                    vec!["children", "value", "isDisabled"],
                ),
                make_interface(
                    "DropdownListProps",
                    "Dropdown",
                    Some("MenuListProps"),
                    vec!["children"],
                ),
                // Menu family components (different directory = different family)
                // Menu renders MenuContext (a React context provider) internally.
                // This makes Menu a "container" in its family — it renders at
                // least one family member.
                make_component("Menu", "Menu", vec!["MenuContext"]),
                make_component("MenuContext", "Menu", vec![]),
                make_component("MenuContent", "Menu", vec![]),
                make_component("MenuList", "Menu", vec![]),
                make_component("MenuItem", "Menu", vec![]),
                make_component("MenuGroup", "Menu", vec!["MenuList"]),
                // Menu family interfaces
                make_interface(
                    "MenuProps",
                    "Menu",
                    None,
                    vec!["children", "className", "onSelect"],
                ),
                make_interface("MenuListProps", "Menu", None, vec!["children"]),
                make_interface("MenuItemProps", "Menu", None, vec!["children", "value"]),
                make_interface("MenuGroupProps", "Menu", None, vec!["children", "label"]),
            ],
        };

        let result = h.compute_deterministic_hierarchy(&new_surface, &[]);

        // Dropdown extends Menu. Menu does NOT render MenuList, MenuItem, MenuGroup
        // internally → they are consumer children of Menu.
        // By extends mapping:
        //   DropdownList (extends MenuList) → consumer child of Dropdown
        //   DropdownItem (extends MenuItem) → consumer child of Dropdown
        //   DropdownGroup (extends MenuGroup) → consumer child of Dropdown
        assert!(
            has_entry(&result, "Dropdown", "Dropdown"),
            "Dropdown should be a parent via extends mapping"
        );

        let dropdown_children = child_names(&result, "Dropdown", "Dropdown");

        // Dropdown should contain children whose external components are
        // NOT containers. MenuList and MenuItem are leaves (render nothing
        // from Menu family), so DropdownList and DropdownItem map as children.
        // MenuGroup IS a container (renders MenuList), so DropdownGroup is
        // NOT a direct child of Dropdown — it has its own hierarchy.
        assert!(
            dropdown_children.contains(&"DropdownItem".to_string()),
            "DropdownItem wraps MenuItem (leaf) → consumer child of Dropdown"
        );
        assert!(
            dropdown_children.contains(&"DropdownList".to_string()),
            "DropdownList wraps MenuList (leaf) → consumer child of Dropdown"
        );
        assert!(
            !dropdown_children.contains(&"DropdownGroup".to_string()),
            "DropdownGroup wraps MenuGroup (container) → not a direct child of Dropdown"
        );

        // DropdownGroup IS a parent (MenuGroup renders MenuList, is a container).
        // DropdownGroup should find DropdownItem as a child (MenuItem is a leaf,
        // not rendered by MenuGroup).
        assert!(
            has_entry(&result, "Dropdown", "DropdownGroup"),
            "DropdownGroup should be a parent"
        );
        let group_children = child_names(&result, "Dropdown", "DropdownGroup");
        assert!(
            group_children.contains(&"DropdownItem".to_string()),
            "DropdownItem is a child of DropdownGroup"
        );
        // DropdownGroup should NOT list Dropdown or DropdownList as children
        assert!(
            !group_children.contains(&"Dropdown".to_string()),
            "Dropdown is a container, not a child of DropdownGroup"
        );

        // DropdownList and DropdownItem should NOT be parents
        assert!(
            !has_entry(&result, "Dropdown", "DropdownList"),
            "DropdownList should NOT be a parent (MenuList is not a container)"
        );
        assert!(
            !has_entry(&result, "Dropdown", "DropdownItem"),
            "DropdownItem should NOT be a parent"
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    // Signal 3: Internal rendering determines prop-passed mechanism
    // ═══════════════════════════════════════════════════════════════════
    //
    // When a parent renders a child internally AND that child absorbed
    // a removed prop, the mechanism should be "prop" (not "child").

    #[test]
    fn signal3_rendered_internally_means_prop_passed() {
        let h = TestHierarchy;

        // FormFieldGroup renders FormFieldGroupHeader internally (via header prop)
        let new_surface = ApiSurface {
            symbols: vec![
                make_component(
                    "FormFieldGroup",
                    "Form",
                    vec!["FormFieldGroupHeader"], // renders it internally!
                ),
                make_component("FormFieldGroupHeader", "Form", vec![]),
                make_component("FormGroup", "Form", vec![]),
                // Interfaces
                make_interface(
                    "FormFieldGroupProps",
                    "Form",
                    None,
                    vec!["children", "header"],
                ),
                make_interface(
                    "FormFieldGroupHeaderProps",
                    "Form",
                    None,
                    vec!["titleText", "titleDescription"],
                ),
            ],
        };

        // FormFieldGroup had titleText and titleDescription removed
        let changes = vec![
            removed_member("FormFieldGroupProps", "titleText"),
            removed_member("FormFieldGroupProps", "titleDescription"),
        ];

        let result = h.compute_deterministic_hierarchy(&new_surface, &changes);

        // FormFieldGroupHeader absorbed titleText/titleDescription from FormFieldGroup
        assert!(has_entry(&result, "Form", "FormFieldGroup"));
        let children = child_names(&result, "Form", "FormFieldGroup");
        assert!(
            children.contains(&"FormFieldGroupHeader".to_string()),
            "FormFieldGroupHeader absorbed props from FormFieldGroup"
        );

        // Since FormFieldGroup RENDERS FormFieldGroupHeader internally → prop-passed
        assert_eq!(
            child_mechanism(&result, "Form", "FormFieldGroup", "FormFieldGroupHeader"),
            Some("prop".to_string()),
            "FormFieldGroupHeader is rendered internally → prop-passed"
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    // Masthead: all leaves, no hierarchy
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn masthead_all_leaves() {
        let h = TestHierarchy;

        let surface = ApiSurface {
            symbols: vec![
                make_component("Masthead", "Masthead", vec![]),
                make_component("MastheadBrand", "Masthead", vec![]),
                make_component("MastheadContent", "Masthead", vec![]),
                make_component("MastheadLogo", "Masthead", vec![]),
                make_component("MastheadMain", "Masthead", vec![]),
                make_component("MastheadToggle", "Masthead", vec![]),
            ],
        };

        let result = h.compute_deterministic_hierarchy(&surface, &[]);

        assert!(
            !result.contains_key("Masthead"),
            "Masthead: all components are leaves (div wrappers)"
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    // No data → empty hierarchy
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn no_signals_empty_hierarchy() {
        let h = TestHierarchy;

        let surface = ApiSurface {
            symbols: vec![
                make_component("Modal", "Modal", vec![]),
                make_component("ModalHeader", "Modal", vec![]),
            ],
        };

        let result = h.compute_deterministic_hierarchy(&surface, &[]);
        assert!(result.is_empty(), "no signals → no hierarchy");
    }

    // ═══════════════════════════════════════════════════════════════════
    // Interfaces/types excluded from hierarchy candidates
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn interfaces_not_hierarchy_candidates() {
        let h = TestHierarchy;

        let surface = ApiSurface {
            symbols: vec![
                make_component("Modal", "Modal", vec![]),
                make_component("ModalBody", "Modal", vec![]),
                make_interface("ModalProps", "Modal", None, vec!["children"]),
            ],
        };

        let changes = vec![removed_member("ModalProps", "title")];
        let result = h.compute_deterministic_hierarchy(&surface, &changes);

        // ModalProps should not appear as a child anywhere
        for family in result.values() {
            for children in family.values() {
                for child in children {
                    assert_ne!(
                        child.name, "ModalProps",
                        "Interfaces should not be hierarchy candidates"
                    );
                }
            }
        }
    }
}
