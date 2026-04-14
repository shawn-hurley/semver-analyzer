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
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
pub trait LanguageSemantics<M: Default + Clone + PartialEq = ()> {
    /// Is adding this member to this container a breaking change?
    ///
    /// This is the single rule that differs most fundamentally by language:
    /// - TypeScript: breaking only if the member is required (non-optional).
    /// - Go: ALWAYS breaking for interfaces (all implementors must add it).
    /// - Java: breaking for abstract methods, not for default methods.
    /// - C#: breaking for abstract members on interfaces.
    /// - Python: breaking for abstract methods on Protocol/ABC.
    fn is_member_addition_breaking(&self, container: &Symbol<M>, member: &Symbol<M>) -> bool;

    /// Are these two symbols part of the same logical family/group?
    ///
    /// Used to scope migration detection. When a symbol is removed, only
    /// symbols in the same family are considered as potential absorption targets.
    ///
    /// - TypeScript/React: same component directory
    /// - Go: same package
    /// - Java: same package
    /// - Python: same module
    fn same_family(&self, a: &Symbol<M>, b: &Symbol<M>) -> bool;

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

    /// Should this symbol be excluded from diff analysis?
    ///
    /// Called by the diff engine to filter out symbols that should not be
    /// compared. The most common case is TypeScript's `export * from '...'`
    /// star re-export directives.
    ///
    /// TypeScript: `sym.name == "*"` (star re-exports)
    /// Default: `false` (all symbols are analyzed)
    fn should_skip_symbol(&self, _sym: &Symbol<M>) -> bool {
        false
    }

    /// Human-readable label for members when building migration descriptions.
    ///
    /// TypeScript: `"props"` (component properties)
    /// Go: `"fields"` (struct fields)
    /// Default: `"members"`
    fn member_label(&self) -> &'static str {
        "members"
    }

    /// Extract a fallback key for rename matching from a symbol's metadata.
    ///
    /// When fingerprint-based rename detection fails, the diff engine uses
    /// this method to extract an alternative matching key. For TypeScript
    /// CSS tokens, this parses the resolved CSS value from the `.d.ts`
    /// type annotation (e.g., the string `"#151515"` from a CSS variable).
    ///
    /// TypeScript: parses `["value"]: "..."` from the return type annotation
    /// Default: `None` (no fallback key)
    fn extract_rename_fallback_key(&self, _sym: &Symbol<M>) -> Option<String> {
        None
    }

    /// Normalize a qualified name for relocation detection.
    ///
    /// Strips language-specific path segments that represent lifecycle
    /// modifiers (e.g., TypeScript's `/deprecated/` and `/next/` directories).
    /// Symbols with matching canonical names are detected as relocations
    /// rather than separate removals and additions.
    ///
    /// TypeScript: strips `/deprecated/` and `/next/` segments
    /// Default: returns the name unchanged
    fn canonical_name_for_relocation(&self, qualified_name: &str) -> String {
        qualified_name.to_string()
    }

    /// Classify a relocation based on old and new qualified names.
    ///
    /// Returns a human-readable label describing the relocation direction
    /// (e.g., "moved to deprecated exports", "promoted from next to stable").
    /// Returns `None` for generic relocations with no special classification.
    ///
    /// TypeScript: detects `/deprecated/` and `/next/` transitions
    /// Default: `None` (no classification)
    fn classify_relocation(&self, _old_qname: &str, _new_qname: &str) -> Option<&'static str> {
        None
    }

    /// Derive the import subpath for a symbol, used in migration descriptions.
    ///
    /// When a symbol moves between submodules (e.g., from main exports to
    /// `/deprecated/` exports), the import path changes. This method derives
    /// the effective import path from the package name and qualified name.
    ///
    /// TypeScript: appends `/deprecated` or `/next` based on qualified name
    /// Default: returns the package name unchanged
    fn derive_import_subpath(&self, package: Option<&str>, _qualified_name: &str) -> String {
        package.unwrap_or("unknown").to_string()
    }

    /// Produce additional structural changes by diffing language-specific
    /// metadata on two matched symbols.
    ///
    /// Called by the diff engine for each pair of symbols that matched by
    /// qualified name. The default implementation returns no changes.
    ///
    /// TypeScript: could diff `rendered_components` or `css` metadata.
    /// Default: empty (no language-specific metadata diffing)
    fn diff_language_data(&self, _old: &Symbol<M>, _new: &Symbol<M>) -> Vec<StructuralChange> {
        vec![]
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
    fn hierarchy(&self) -> Option<&dyn HierarchySemantics<M>> {
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
pub trait HierarchySemantics<M: Default + Clone + PartialEq = ()> {
    /// Get file paths belonging to a component family directory.
    ///
    /// Given a family name (e.g., "Dropdown"), returns relative paths to
    /// all source files in that family. Used to read content for the LLM prompt.
    fn family_source_paths(&self, repo: &Path, git_ref: &str, family_name: &str) -> Vec<String>;

    /// Get a human-readable family name from a group of symbols.
    ///
    /// TypeScript/React: extracts the component directory name
    /// (e.g., "Dropdown" from "packages/react-core/src/components/Dropdown/...")
    fn family_name_from_symbols(&self, symbols: &[&Symbol<M>]) -> Option<String>;

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
    fn is_hierarchy_candidate(&self, sym: &Symbol<M>) -> bool;

    /// Minimum number of exported types for a family to qualify
    /// for hierarchy inference. Default: 2.
    fn min_components_for_hierarchy(&self) -> usize {
        2
    }

    /// Compute component hierarchy deterministically.
    ///
    /// The default implementation returns an empty map. Language implementations
    /// that support component hierarchy (e.g., TypeScript/React) override this
    /// with the full algorithm using language-specific metadata.
    ///
    /// The method works on the NEW surface and structural changes. It returns
    /// the expected hierarchy for the new version.
    fn compute_deterministic_hierarchy(
        &self,
        new_surface: &ApiSurface<M>,
        structural_changes: &[StructuralChange],
    ) -> HashMap<String, HashMap<String, Vec<ExpectedChild>>> {
        let _ = (new_surface, structural_changes);
        HashMap::new()
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

// ── Extended analysis parameters ─────────────────────────────────────────

/// Parameters for `Language::run_extended_analysis`.
///
/// Bundles the repo/ref context with data computed by the orchestrator
/// (removed CSS blocks, dep-repo packages) so that language implementations
/// can attach them to their extensions without the orchestrator needing to
/// know the concrete extension type.
#[derive(Debug, Clone)]
pub struct ExtendedAnalysisParams {
    /// Path to the primary repository being analyzed.
    pub repo: PathBuf,
    /// Git ref for the old (from) version.
    pub from_ref: String,
    /// Git ref for the new (to) version.
    pub to_ref: String,
    /// Optional path to the dependency CSS repo (already checked out/built).
    pub dep_css_dir: Option<PathBuf>,
    /// CSS component blocks removed between old and new dep-repo versions
    /// (e.g., `["select", "chip"]`). Computed by the orchestrator from
    /// `detect_removed_css_blocks()`.
    pub removed_css_blocks: Vec<String>,
    /// Dependency repo packages (name → version at new ref).
    /// Used to generate dep-update rules for packages outside the main
    /// analyzed monorepo (e.g., `@patternfly/patternfly` CSS package).
    pub dep_repo_packages: HashMap<String, String>,
}

// ── LLM category definitions ────────────────────────────────────────────

/// A behavioral change category definition for LLM prompts.
///
/// Each language provides a list of these to guide the LLM's output.
/// The `id` must match the serde name of the corresponding `Language::Category`
/// enum variant (e.g., `"dom_structure"` for `TsCategory::DomStructure`).
#[derive(Debug, Clone)]
pub struct LlmCategoryDefinition {
    /// Machine-readable identifier (e.g., `"dom_structure"`, `"annotation_change"`).
    /// Must match the serde serialization of `Language::Category` variants.
    pub id: String,
    /// Short human label (e.g., `"DOM/render changes"`, `"Annotation changes"`).
    pub label: String,
    /// Detailed description for the LLM prompt explaining what this category covers.
    pub description: String,
}

/// The core language abstraction.
///
/// Composes `LanguageSemantics + MessageFormatter` and adds four associated
/// types representing language-specific data flowing through the pipeline.
///
/// Code that only needs semantic rules can take `&dyn LanguageSemantics`
/// (no generic parameter). Code that needs the associated types takes
/// `L: Language`.
pub trait Language:
    LanguageSemantics<Self::SymbolData> + MessageFormatter + Send + Sync + 'static
{
    /// Per-symbol metadata type carried in `Symbol<M>.language_data`.
    ///
    /// TypeScript: `TsSymbolData` (rendered components, CSS tokens).
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
    /// Carries pipeline results that are specific to this language
    /// (e.g., SD pipeline results, hierarchy deltas for TypeScript).
    /// Replaces the concrete `sd_result` and `hierarchy_deltas` fields
    /// that were previously on `AnalysisReport`/`AnalysisResult`.
    ///
    /// TypeScript: `TsAnalysisExtensions` (SD result, hierarchy deltas).
    /// Languages without extended analysis: `EmptyExtensions`.
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
    ///
    /// An optional `DegradationTracker` can be provided to record non-fatal
    /// extraction issues (e.g., partial tsc success). These appear in the
    /// end-of-run summary.
    fn extract(
        &self,
        repo: &Path,
        git_ref: &str,
        degradation: Option<&crate::diagnostics::DegradationTracker>,
    ) -> Result<ApiSurface<Self::SymbolData>>;

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

    /// Return the behavioral change categories for LLM prompts.
    ///
    /// Each category has an `id` that must match the serde serialization of
    /// the corresponding `Language::Category` variant. The LLM prompt is
    /// built dynamically from these definitions, so adding a new language
    /// automatically gets language-appropriate behavioral categories.
    ///
    /// Default: empty (no behavioral categories — LLM skips category assignment).
    fn llm_categories(&self) -> Vec<LlmCategoryDefinition> {
        vec![]
    }

    // ── v2 Extended Analysis pipeline ───────────────────────────────

    /// Run language-specific extended analysis.
    ///
    /// For TypeScript, this runs the SD (Source-Level Diff) pipeline:
    /// reads component source files at both refs, extracts structured
    /// profiles, diffs them, and builds composition trees.
    ///
    /// The `params` struct carries both common fields (repo, refs, CSS dir)
    /// and data computed by the orchestrator (removed CSS blocks, dep-repo
    /// packages) that the language impl can attach to its extensions.
    ///
    /// Default implementation returns empty extensions (no extended analysis).
    fn run_extended_analysis(
        &self,
        _params: &ExtendedAnalysisParams,
    ) -> Result<Self::AnalysisExtensions> {
        Ok(Self::AnalysisExtensions::default())
    }

    /// Post-process extensions after both TD and extended analysis complete.
    ///
    /// This is where language-specific cross-pipeline processing happens.
    /// For TypeScript, this runs deprecated replacement detection (requires
    /// both TD structural changes and SD source-level changes) and transforms
    /// structural changes accordingly.
    ///
    /// Returns the (potentially modified) structural changes.
    /// Default implementation is a no-op.
    fn finalize_extensions(
        &self,
        _extensions: &mut Self::AnalysisExtensions,
        structural_changes: Arc<Vec<StructuralChange>>,
    ) -> Arc<Vec<StructuralChange>> {
        structural_changes
    }

    /// Return log-friendly summary lines for the extended analysis results.
    ///
    /// The orchestrator calls this for progress/logging output.
    /// Default implementation returns empty (no summary).
    fn extensions_log_summary(&self, _extensions: &Self::AnalysisExtensions) -> Vec<String> {
        vec![]
    }
}

// ── Convenience functions (TD) ──────────────────────────────────────────

/// Compare two API surfaces using language-specific semantic rules.
///
/// This is the primary entry point for the TD (Top-Down) pipeline.
/// The `semantics` parameter provides language-specific rules.
pub fn diff_surfaces_with_semantics<M, S>(
    old: &ApiSurface<M>,
    new: &ApiSurface<M>,
    semantics: &S,
) -> Vec<StructuralChange>
where
    M: Default + Clone + PartialEq,
    S: LanguageSemantics<M>,
{
    crate::diff::diff_surfaces_with_semantics(old, new, semantics)
}

/// Compare two API surfaces using minimal semantics (no language-specific rules).
///
/// This uses `MinimalSemantics` which is language-agnostic: no member additions
/// are breaking, no union parsing, no post-processing. For language-aware
/// diffing, use `diff_surfaces_with_semantics` with a `LanguageSemantics` impl.
pub fn diff_surfaces<M: Default + Clone + PartialEq>(
    old: &ApiSurface<M>,
    new: &ApiSurface<M>,
) -> Vec<StructuralChange> {
    crate::diff::diff_surfaces(old, new)
}

// Hierarchy algorithm tests live in crates/ts/src/language.rs where
// they can use TsSymbolData.rendered_components. The core default
// implementation returns an empty map.
