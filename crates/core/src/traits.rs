//! Trait definitions for language-pluggable analysis.
//!
//! Adding a new language means implementing these traits. The orchestrator,
//! diff engine, and output format are language-agnostic and reused unchanged.
//!
//! ## Trait ownership
//!
//! | Trait | Used by | Per-language? |
//! |---|---|---|
//! | `ApiExtractor` | TD | Yes |
//! | `DiffParser` | BU | Yes |
//! | `CallGraphBuilder` | BU + impact | Yes |
//! | `TestAnalyzer` | BU | Yes (test conventions differ) |
//! | `BehaviorAnalyzer` | BU | No (language-agnostic, LLM-based) |

use crate::types::{
    ApiSurface, BreakingVerdict, Caller, ChangedFunction, EvidenceSource, FunctionSpec, Reference,
    StructuralChange, Symbol, TestDiff, TestFile, Visibility,
};
use anyhow::Result;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::BTreeSet;
use std::fmt::Debug;
use std::path::Path;

// ── TD Traits ───────────────────────────────────────────────────────────

/// Extract the public API surface from source code at a git ref.
/// Used by TD (Top-Down) pipeline.
///
/// Each language provides its own implementation:
/// - TypeScript/JS: `tsc --declaration` + OXC parsing of `.d.ts` files
/// - Python (future): tree-sitter-python + `__all__` exports
/// - Go (future): tree-sitter-go + capitalized identifiers
/// - Rust (future): rustdoc JSON
pub trait ApiExtractor {
    /// Extract the public API surface from source code at a specific git ref.
    ///
    /// The implementation is responsible for:
    /// 1. Checking out the ref (via git worktree or similar)
    /// 2. Running any required build steps (e.g., `tsc --declaration`)
    /// 3. Parsing the output to populate an `ApiSurface`
    /// 4. Canonicalizing types so string comparison works
    /// 5. Cleaning up temporary files
    fn extract(&self, repo: &Path, git_ref: &str) -> Result<ApiSurface>;
}

// ── BU Traits (language-specific) ───────────────────────────────────────

/// Parse a git diff and extract all changed functions (public AND private).
/// Used by BU (Bottom-Up) pipeline.
///
/// Each language provides its own implementation because function
/// extraction requires language-specific AST parsing.
pub trait DiffParser {
    /// Parse the diff between two git refs and identify all functions
    /// whose bodies changed.
    ///
    /// The implementation:
    /// 1. Runs `git diff --name-status from_ref..to_ref` for changed files
    /// 2. For each changed source file, gets both versions via `git show`
    /// 3. Parses both versions with the language's parser (OXC for TS)
    /// 4. Walks both ASTs to extract function declarations with bodies
    /// 5. Matches functions by qualified name and compares bodies
    /// 6. Returns ALL changed functions (public AND private)
    fn parse_changed_functions(
        &self,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<Vec<ChangedFunction>>;
}

/// Build call graphs and find references.
///
/// BU uses `find_callers()` to walk UP from changed private functions.
/// Impact analysis uses `find_references()` to find dependents of broken
/// public symbols across the entire project.
pub trait CallGraphBuilder {
    /// Given a function, find what calls it (callers, not callees).
    ///
    /// For private (non-exported) functions, callers are always in the
    /// same file — per-file scope analysis handles this directly.
    /// No cross-file search is needed for the walk-UP path.
    ///
    /// Includes HOF heuristic detection: if the symbol is passed as an
    /// argument to a higher-order function (e.g., `arr.map(symbol)`,
    /// `emitter.on('event', symbol)`, `setTimeout(symbol)`), the
    /// enclosing function is treated as a caller.
    fn find_callers(&self, file: &Path, symbol_name: &str) -> Result<Vec<Caller>>;

    /// Given a public symbol, find all references to it across the
    /// project. Used for impact analysis after TD+BU merge.
    ///
    /// Uses the reverse import index:
    /// 1. Look up (source_file, symbol_name) in the import index
    /// 2. For each import site, report the importing file, local binding,
    ///    and call sites
    /// 3. Follow re-export chains (A re-exports from B re-exports from C)
    fn find_references(&self, file: &Path, symbol_name: &str) -> Result<Vec<Reference>>;
}

/// Find and analyze tests associated with a changed function.
/// Used by BU before LLM, to detect behavioral changes from test diffs.
pub trait TestAnalyzer {
    /// Given a source file, find its associated test file(s) by convention.
    /// e.g., `foo.ts` → `foo.test.ts`, `foo.spec.ts`, `__tests__/foo.ts`
    fn find_tests(&self, repo: &Path, source_file: &Path) -> Result<Vec<TestFile>>;

    /// Diff the test file between two refs. Returns changed assertion lines
    /// as raw text diffs (Option B approach — no framework-specific parsing).
    fn diff_test_assertions(
        &self,
        repo: &Path,
        test_file: &TestFile,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<TestDiff>;
}

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
        evidence: &EvidenceSource,
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

    /// Post-process the change list before returning from diff_surfaces.
    ///
    /// TypeScript: dedup default export changes.
    /// Most languages: no-op.
    fn post_process(&self, _changes: &mut Vec<StructuralChange>) {}
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
/// Composes `LanguageSemantics + MessageFormatter` and adds four associated
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

    /// Language identifier for serialization dispatch.
    fn name() -> &'static str;
}

// ── Convenience function (TD) ───────────────────────────────────────────

/// Compare two API surfaces and produce structural changes.
///
/// This is language-agnostic and written once. It operates on the
/// `ApiSurface` type produced by `ApiExtractor` implementations.
///
/// Type comparison is done via canonicalized string equality — the
/// `ApiExtractor` is responsible for normalizing types before they
/// reach this function.
pub fn diff_surfaces(old: &ApiSurface, new: &ApiSurface) -> Vec<StructuralChange> {
    crate::diff::diff_surfaces(old, new)
}
