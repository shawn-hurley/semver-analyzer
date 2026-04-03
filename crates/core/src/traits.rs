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
    ApiSurface, BehavioralChangeKind, BodyAnalysisResult, BreakingVerdict, Caller, ChangedFunction,
    EvidenceType, ExpectedChild, FunctionSpec, Reference, StructuralChange, Symbol, SymbolKind,
    TestDiff, TestFile, Visibility,
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
///
/// The type parameter `M` is the per-symbol metadata type, matching
/// `Symbol<M>`. The diff engine is generic over both `M` and the
/// `LanguageSemantics<M>` implementation.
pub trait LanguageSemantics<M: Default + Clone = ()> {
    /// Is adding this member to this container a breaking change?
    fn is_member_addition_breaking(&self, container: &Symbol<M>, member: &Symbol<M>) -> bool;

    /// Are these two symbols part of the same logical family/group?
    ///
    /// Used to scope migration detection. When a symbol is removed, only
    /// symbols in the same family are considered as potential absorption targets.
    fn same_family(&self, a: &Symbol<M>, b: &Symbol<M>) -> bool;

    /// Are these two symbols the same concept, possibly at different paths?
    ///
    /// When true, migration detection does a full member comparison (all members,
    /// not just newly-added ones) because the candidate is assumed to be a direct
    /// replacement for the removed symbol.
    fn same_identity(&self, a: &Symbol<M>, b: &Symbol<M>) -> bool;

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
        _new_surface: &ApiSurface,
        _structural_changes: &[StructuralChange],
    ) -> HashMap<String, HashMap<String, Vec<ExpectedChild>>> {
        // Default: no deterministic hierarchy. Language implementations
        // (e.g., TypeScript) override this with their framework-specific
        // logic using rendered_components, prop absorption, etc.
        //
        // The original ~370-line React-specific default implementation was
        // removed during genericization. See the TypeScript crate for the
        // language-specific implementation.
        HashMap::new()
    }
}

// The original compute_deterministic_hierarchy default implementation
// (~370 lines) was removed. It used React-specific patterns:
// - format!("{}Props", sym.name) naming convention
// - rendered_components field (now on TsSymbolData)
// - Omit<...> type stripping (TypeScript utility type)
// - "prop" mechanism (JSX prop-passing)
//
// The TypeScript crate now owns this logic.

// (deleted ~350 lines of React-specific compute_deterministic_hierarchy default impl)
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
/// Composes `LanguageSemantics<Self::SymbolData> + MessageFormatter` and adds
/// associated types representing language-specific data flowing through the
/// pipeline.
///
/// The diff engine uses `impl LanguageSemantics<M>` for static dispatch.
/// Code that needs the full set of associated types takes `L: Language`.
pub trait Language:
    LanguageSemantics<Self::SymbolData> + MessageFormatter + Send + Sync + 'static
{
    /// Per-symbol metadata type carried on `Symbol<M>`.
    ///
    /// TypeScript: `TsSymbolData` (rendered_components, css).
    /// Languages without per-symbol metadata: `()`.
    type SymbolData: Debug
        + Clone
        + Default
        + PartialEq
        + Eq
        + Serialize
        + DeserializeOwned
        + Send
        + Sync;

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
    fn extract(&self, repo: &Path, git_ref: &str) -> Result<ApiSurface<Self::SymbolData>>;

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
        old_surface: &ApiSurface<Self::SymbolData>,
        new_surface: &ApiSurface<Self::SymbolData>,
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
/// The `semantics` parameter provides language-specific rules via static dispatch.
pub fn diff_surfaces_with_semantics<M: Default + Clone, S: LanguageSemantics<M>>(
    old: &ApiSurface<M>,
    new: &ApiSurface<M>,
    semantics: &S,
) -> Vec<StructuralChange> {
    crate::diff::diff_surfaces_with_semantics(old, new, semantics)
}

/// Compare two API surfaces using minimal semantics (no language-specific rules).
///
/// This uses `MinimalSemantics` which is language-agnostic: no member additions
/// are breaking, no union parsing, no post-processing. For language-aware
/// diffing, use `diff_surfaces_with_semantics` with a `LanguageSemantics` impl.
pub fn diff_surfaces<M: Default + Clone>(
    old: &ApiSurface<M>,
    new: &ApiSurface<M>,
) -> Vec<StructuralChange> {
    crate::diff::diff_surfaces(old, new)
}

// Hierarchy tests were removed during genericization.
// The compute_deterministic_hierarchy tests should be re-created
// in the TypeScript crate when the implementation moves there.
