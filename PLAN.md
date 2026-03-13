# Semantic Breaking Change Analyzer -- Design Plan

## Problem Statement

Given a repository and two git refs (tags, commits, branches), produce a
deterministic, structured report of **breaking changes** -- including both
structural API breaks and behavioral/semantic breaks -- along with their
**impact radius** (what code is affected).

Current approaches (the goose-harness and opencode-harness in this repo) send
raw diffs to an LLM and ask it to identify breaking changes. This works for
small changesets but has fundamental limitations:

- **Non-deterministic** -- same diff can produce different results across runs
- **No dependency graph** -- can't tell you *what code is affected* by a break
- **No type awareness** -- can't distinguish widening (safe) from narrowing (breaking) type changes
- **Context-limited** -- large repos exceed LLM context windows
- **No transitive analysis** -- if A breaks and B uses A, the tool doesn't flag B

## Goals

1. **Agent-agnostic** -- standalone CLI, can also be invoked by Goose, OpenCode, or any other agent
2. **Language-agnostic architecture** -- pluggable per-language analyzers via Rust traits, starting with TypeScript/JavaScript
3. **Deterministic structural analysis** -- static tools for API extraction and diffing (no LLM)
4. **LLM-assisted behavioral analysis** -- LLM only for the genuinely hard part: "did the behavior change in a breaking way?"
5. **Impact analysis** -- for each breaking change, report what code depends on it

## Prior Art

### Static Analysis Tools

| Tool | Language | Approach | Limitation |
|---|---|---|---|
| **cargo-semver-checks** | Rust | Declarative queries (Trustfall) over rustdoc JSON | Rust-only, no behavioral analysis |
| **api-extractor** | TypeScript | TS compiler API to extract `.d.ts` surface | TS-only, no behavioral analysis, no impact graph |
| **go-apidiff** | Go | `go/types` package comparison | Go-only |
| **GumTree** | Any (Java impl) | AST-to-AST tree matching algorithm | Structural diff only, no semantic understanding |
| **Difftastic** | Any (Rust impl) | tree-sitter + Dijkstra shortest edit path | Display tool, no structured output |
| **ast-grep** | Any (Rust impl) | tree-sitter pattern matching | Search tool, not a diff tool |
| **tree-sitter-graph** | Any (Rust impl) | DSL for building graphs from syntax trees | Low-level building block |
| **GitHub Semantic** | Multiple (Haskell) | Per-language Haskell AST types from tree-sitter | Archived April 2025 |
| **OXC** | JS/TS (Rust impl) | Parser, semantic analysis, module resolution | No type checking, no diffing |

### LLM-Based Semantic Analysis

| Paper/Tool | Year | What it does | Relevance |
|---|---|---|---|
| **PatchGuru** ([arXiv:2602.05270](https://arxiv.org/abs/2602.05270)) | 2026 | LLM infers *executable patch specifications* from PRs. Synthesizes "patch oracles" -- runtime tests comparing pre/post-patch behavior. Precision 0.62, $0.07/PR. | **Highest** -- closest to LLM-based behavioral break detection. Generates tests that *prove* a behavioral difference. |
| **Preguss** ([arXiv:2512.24594](https://arxiv.org/abs/2512.24594), OOPSLA 2026) | 2025 | LLM generates formal pre/postconditions from code. Static analysis narrows focus, LLM fills in specs. Reduces human verification by 80-88%. | **High** -- infer specs for v1 and v2, compare them to detect behavioral breaks without vague "is this breaking?" prompts. |
| **SESpec** ([arXiv:2506.09550](https://arxiv.org/abs/2506.09550)) | 2025 | Combines symbolic execution with LLM to generate verified specs. Template-guided generation reduces hallucinations. | **High** -- constraining LLM output with templates from static analysis improves reliability. |
| **SmartNote** (FSE 2025) | 2025 | LLM-powered release note generator. Aggregates code changes, produces structured summaries. Outperforms manual notes on completeness. | **Moderate** -- demonstrates LLMs can summarize *what changed and why*, but doesn't classify breaking vs non-breaking. |
| **BALI** ([arXiv:2601.00882](https://arxiv.org/abs/2601.00882), AAAI 2026) | 2025 | Branch-aware loop invariant inference combining LLMs with SMT solvers. | **Moderate** -- neuro-symbolic pattern: LLMs propose, formal tools verify. |
| **Qodo** (formerly CodiumAI) | Commercial | Claims "breaking change analysis across repos." 15+ specialized review agents. | **High** -- explicitly claims breaking change detection, but implementation details are proprietary. |
| **CodeRabbit** | Commercial | AI code review with "impact of changes" analysis. Code graph + 40+ linters + multi-model. | **Moderate** -- review tool, not a structured breaking change classifier. |

### LLM Reliability Warnings

| Paper | Year | Key Finding |
|---|---|---|
| **ReDef** ([arXiv:2509.09192](https://arxiv.org/abs/2509.09192)) | 2025 | Current code models (CodeBERT, CodeT5+, UniXcoder) do **not** truly understand code modifications. Under counterfactual tests (swapping added/deleted blocks), performance barely degrades -- models pattern-match on surface features, not semantics. |
| **"Are LLMs Reliable Code Reviewers?"** ([arXiv:2603.00539](https://arxiv.org/abs/2603.00539)) | 2026 | LLMs frequently misclassify *correct* code as defective. More detailed prompts *increase* misjudgment rates. Proposed fix: use the LLM's own suggested fix as counterfactual, then validate via tests. |
| **CHOKE** ([arXiv:2502.12964](https://arxiv.org/abs/2502.12964)) | 2025 | "Certain Hallucinations Overriding Known Evidence" -- models answer correctly, then trivial perturbations cause *high-confidence* wrong answers. Any LLM-based analyzer must account for confident-but-wrong outputs. |
| **Code understanding under obfuscation** ([arXiv:2505.10443](https://arxiv.org/abs/2505.10443)) | 2025 | LLMs produce correct predictions based on flawed reasoning 10-50% of the time. Semantics-preserving mutations cause prediction instability. |

### The Gap This Tool Fills

No existing open-source tool combines:
1. Static API surface extraction with full type resolution
2. Structural diff with type compatibility analysis
3. Dependency/usage graph for impact analysis
4. LLM-based behavioral analysis for body-changed-but-signature-same cases

The closest is PatchGuru (LLM-based patch oracles), but it focuses on bug
detection, not breaking change classification for API consumers. And
cargo-semver-checks is the gold standard for structural analysis, but is
Rust-only and doesn't do behavioral analysis.

### Key Insights from Prior Art

**cargo-semver-checks**: Lints are written as **declarative queries** over a
structured API representation using the Trustfall query engine, not as
imperative code. The data source is rustdoc JSON -- the compiler's own
structured output. This pattern (let the compiler do type resolution, query
the output) is directly applicable to TypeScript via `.d.ts` files.

**GumTree**: AST-level diffing produces far better results than line-level
diffing. The tree matching algorithm (top-down greedy + bottom-up recovery)
handles moves and renames that `git diff` misses entirely.

**api-extractor**: Generates `.api.md` and `.api.json` report files that can
be checked into source control. Changes to these files in PRs flag API surface
changes for human review. Also generates rolled-up `.d.ts` bundles. This
"API report as artifact" pattern is worth adopting.

**PatchGuru**: The most effective LLM-based approach generates *executable
tests* that demonstrate behavioral differences, rather than asking the LLM
to describe or classify changes. Precision 0.62 at $0.07/PR is a practical
cost/accuracy tradeoff.

**Preguss/SESpec**: Asking the LLM "what does this function guarantee?" (spec
inference) is more reliable than asking "is this change breaking?" (direct
classification). Infer specs for v1, infer specs for v2, diff the specs.
Template-guided generation with static analysis constraints reduces
hallucination rates from ~30% to ~11-19%.

## Architecture

### The Hard Problem: Type Resolution

The single biggest challenge is the gap between **syntax** (what a parser
sees) and **semantics** (what the type system knows). This gap is enormous
for TypeScript.

Example: Is changing `ReadonlyArray<Item>` to `Item[]` breaking?

- **Parser-only view**: Different strings, flag it as changed.
- **Type system view**: `Item[]` is assignable to `ReadonlyArray<Item>`, so
  this *widens* the accepted input. Not breaking.

Reversing the direction *is* breaking -- but you need the type system to know
which direction is which.

**The `.d.ts` shortcut**: Rather than reimplementing the TypeScript type
checker in Rust (which has been attempted and abandoned -- see `stc`), we use
the same pattern as cargo-semver-checks:

1. **cargo-semver-checks** runs `rustdoc` to produce a JSON API surface, then
   queries it with Trustfall.
2. **This tool** runs `tsc --declaration` to produce `.d.ts` files (the
   compiler's own API surface representation), then parses them with OXC.

The TypeScript compiler does the hard work (type resolution, alias expansion,
re-export resolution, generic instantiation). The Rust binary consumes the
already-resolved output.

### OXC: The Parser Foundation

OXC (Oxidation Compiler) is a high-performance JavaScript/TypeScript toolchain
written in Rust. It replaces tree-sitter for TypeScript/JavaScript analysis
while tree-sitter remains available for other languages.

**GitHub**: https://github.com/oxc-project/oxc (19.7k stars, 331 contributors)
**Part of**: VoidZero (the organization behind Rolldown/Vite)

| Crate | What it provides |
|---|---|
| `oxc_parser` | Full TS/JS parser, faster than SWC. Produces a typed Rust AST. |
| `oxc_semantic` | Scope analysis, symbol binding, reference resolution. |
| `oxc_resolver` | Node.js-compatible module resolution (`package.json` exports, barrel files, conditional exports). |

OXC sits between tree-sitter and the full TypeScript compiler:

```
tree-sitter          OXC                    tsc
──────────────────────────────────────────────────────
Syntax only          Syntax + scope         Syntax + full types
Generic CST          Typed Rust AST         Full type system
No scope analysis    Symbol binding         Full type checking
No module resolve    Node.js resolution     Full module resolve
Any language         JS/TS only             TS only
```

For this tool, OXC handles parsing and reference resolution. `tsc` handles
type resolution (via `.d.ts` generation). Tree-sitter handles non-JS/TS
languages.

### Concurrent TD/BU Architecture

The analysis runs two concurrent processes that share state for deduplication:

- **TD (Top-Down)**: Starts from the public API surface. Finds structural
  breaking changes (signature changes, removed symbols). Fast, deterministic,
  no LLM.
- **BU (Bottom-Up)**: Starts from the git diff. Identifies every changed
  function (public AND private), does behavioral analysis, and walks UP the
  call graph to find affected public APIs. May use LLM for spec inference.

This replaces the earlier deep_hash approach. The key advantages:

1. **No depth limit needed** -- BU starts from the actual changed functions
   (the diff tells you exactly which ones) and walks UP. No need to walk
   down through arbitrary call depth.
2. **Natural deduplication** -- BU checks shared state and skips anything
   TD already found, so no redundant work.
3. **Minimal LLM usage** -- spec inference only runs on functions that
   actually changed AND weren't already caught by TD structurally.
4. **Early termination** -- if a private function's spec didn't change
   meaningfully, BU stops. No upward propagation needed.

```
                     Shared State
                    ┌─────────────┐
                    │ DashMap of   │
                    │ found breaks │
              ┌────►│              │◄────┐
              │     └─────────────┘     │
              │       check / insert     │
              │       + broadcast ch     │
              │                          │
     ┌────────┴─────────┐    ┌───────────┴──────────────┐
     │   TD (Top-Down)   │    │   BU (Bottom-Up)          │
     │                    │    │                            │
     │ 1. tsc --decl at A │    │ 1. git diff A..B           │
     │ 2. tsc --decl at B │    │ 2. Parse changed            │
     │ 3. OXC parse .d.ts │    │    functions from diff      │
     │ 4. diff_surfaces()  │    │ 3. For each changed fn      │
     │ 5. For each break:  │    │    a. In shared state?      │
     │    insert to shared │    │       -> skip               │
     │    + broadcast      │    │    b. Find associated test   │
     │                    │    │    c. Test assertions chg'd? │
     │ Finds:             │    │       -> HIGH confidence     │
     │  - removed symbols │    │       -> walk UP (no LLM)   │
     │  - sig changes     │    │    d. Test exists, no chg?   │
     │  - type changes    │    │       -> LLM + test context  │
     │  - visibility      │    │       -> MEDIUM confidence   │
     │                    │    │    e. No test?               │
     │                    │    │       -> LLM spec inference  │
     │                    │    │       -> LOWER confidence    │
     │                    │    │    f. Spec same?             │
     │                    │    │       -> skip (no-op)        │
     └────────────────────┘    └──────────────────────────────┘
              │                          │
              └──────────┬───────────────┘
                         │
                    ┌────▼─────┐
                    │  Merge   │
                    │ + enrich │
                    │ + impact │
                    └────┬─────┘
                         │
                    report.json
```

#### TD/BU Coordination: Broadcast Channel

TD and BU run fully concurrently. Since BU's analysis (especially LLM spec
inference) is expensive, we use a `tokio::sync::broadcast` channel so BU
learns about TD's findings in near-real-time rather than only via DashMap
polling.

**How it works:**

1. `SharedFindings` owns a `broadcast::Sender<String>`. When TD inserts a
   structural break, it also sends the qualified name on the channel.
2. BU holds a `broadcast::Receiver<String>`. Before processing each changed
   function, BU drains any pending messages from the channel into a local
   `HashSet<String>` (the "skip set").
3. BU checks both the DashMap (for anything inserted before BU subscribed)
   AND the local skip set (for findings received via broadcast).
4. If the symbol is in either, BU skips it -- no LLM call wasted.

**Why not just DashMap?** DashMap checks work, but TD's `tsc` invocations
are I/O-bound and may take seconds. BU's diff parsing is faster and may
begin its analysis loop before TD has inserted anything. Without the
broadcast channel, BU would redundantly analyze functions that TD identifies
moments later. The LLM calls BU makes are the most expensive operation in
the pipeline (both in latency and cost), so avoiding even a few redundant
calls justifies the coordination complexity.

**Edge case:** If BU processes a function and finds a behavioral break,
then TD later inserts a structural break for the same symbol, the merge
step resolves this: structural breaks take precedence. The behavioral
finding is discarded (the structural finding is more precise and
deterministic). This is correct but means BU occasionally does work that
gets thrown away. The broadcast channel minimizes but does not eliminate
this -- which is acceptable.

**Backpressure:** The broadcast channel is bounded (capacity = number of
symbols in the API surface, or a reasonable cap like 4096). If TD produces
findings faster than BU drains them, `broadcast::send` succeeds as long as
the buffer isn't full. In practice TD produces findings in a batch (after
`diff_surfaces()` completes) rather than one-at-a-time, so a single drain
by BU picks up all of them.

#### After Both Complete

There may be a residual set of changes BU found where a private function's
behavioral change propagated UP to a function whose public/private status
isn't yet determined (e.g., it's not directly exported but might be
re-exported elsewhere). A short reconciliation pass resolves these against
the API surface TD already extracted.

### Trait-Based Language Abstraction

The core design uses Rust traits to separate language-specific work from
language-agnostic orchestration. Adding a new language means implementing
the traits -- the orchestrator, diff engine, and output format are reused.

```rust
/// Extract the public API surface from source code at a git ref.
/// Used by TD.
trait ApiExtractor {
    fn extract(&self, repo: &Path, git_ref: &str) -> Result<ApiSurface>;
}

/// Parse a git diff and extract all changed functions (public AND private).
/// Used by BU.
trait DiffParser {
    fn parse_changed_functions(
        &self,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<Vec<ChangedFunction>>;
}

/// Build call graphs and find references.
/// BU uses find_callers() to walk UP from changed private functions.
/// Impact analysis uses find_references() to find dependents of broken
/// public symbols across the entire project.
///
/// Implementations read from the shared ProjectSemanticModel rather than
/// re-parsing source files. TD builds the model during API extraction;
/// BU and impact analysis consume it.
trait CallGraphBuilder {
    /// Given a function, find what calls it (callers, not callees).
    ///
    /// For private (non-exported) functions, callers are always in the
    /// same file -- oxc_semantic's per-file scope analysis handles this
    /// directly. No cross-file search is needed for the walk-UP path.
    ///
    /// Includes HOF heuristic detection: if the symbol is passed as an
    /// argument to a higher-order function (e.g., `arr.map(symbol)`,
    /// `emitter.on('event', symbol)`, `setTimeout(symbol)`), the
    /// enclosing function is treated as a caller.
    fn find_callers(
        &self,
        model: &ProjectSemanticModel,
        symbol: &QualifiedName,
    ) -> Result<Vec<Caller>>;

    /// Given a public symbol, find all references to it across the
    /// project. Used for impact analysis after TD+BU merge.
    ///
    /// Uses the reverse import index in ProjectSemanticModel:
    /// 1. Look up (source_file, symbol_name) in the import_index
    /// 2. For each ImportSite, report the importing file, local binding,
    ///    and call sites
    /// 3. Follow re-export chains (A re-exports from B re-exports from C)
    fn find_references(
        &self,
        model: &ProjectSemanticModel,
        symbol: &QualifiedName,
    ) -> Result<Vec<Reference>>;
}

/// Find and analyze tests associated with a changed function.
/// Used by BU before LLM, to detect behavioral changes from test diffs.
trait TestAnalyzer {
    /// Given a function, find its associated test file(s) by convention.
    /// e.g., foo.ts -> foo.test.ts, foo.spec.ts, __tests__/foo.ts
    fn find_tests(&self, repo: &Path, symbol: &QualifiedName) -> Result<Vec<TestFile>>;

    /// Diff the test file between two refs. Returns changed assertion lines
    /// as raw text diffs (Option B approach -- no framework-specific parsing).
    fn diff_test_assertions(
        &self,
        repo: &Path,
        test_file: &TestFile,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<TestDiff>;
}

/// Analyze behavioral changes. Used by BU. Language-agnostic (LLM-based).
trait BehaviorAnalyzer {
    /// Infer a function's behavioral spec from its body.
    fn infer_spec(&self, function_body: &str, signature: &str) -> Result<FunctionSpec>;

    /// Infer a spec with additional context from the test file.
    /// The test assertions give the LLM concrete examples of expected behavior.
    fn infer_spec_with_test_context(
        &self,
        function_body: &str,
        signature: &str,
        test_context: &TestDiff,
    ) -> Result<FunctionSpec>;

    /// Compare two specs and determine if the change is breaking.
    fn specs_are_breaking(&self, old: &FunctionSpec, new: &FunctionSpec) -> Result<BreakingVerdict>;

    /// Check whether a caller propagates a behavioral break from a callee.
    ///
    /// Given a caller's body/signature and the evidence of a behavioral
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
        callee_evidence: &EvidenceSource,
    ) -> Result<bool>;
}
```

The structural diff engine (`diff_surfaces()`) has no trait -- it operates
on the language-agnostic `ApiSurface` type and is written once.

#### Shared Data Structures

```rust
/// Language-agnostic public API surface (used by TD).
struct ApiSurface {
    symbols: Vec<Symbol>,
}

struct Symbol {
    name: String,
    qualified_name: String,          // e.g. "src/api/users.createUser"
    kind: SymbolKind,                // Function, Method, Class, Interface, etc.
    visibility: Visibility,          // Exported, Public, Internal
    file: PathBuf,
    line: usize,
    signature: Option<Signature>,

    // -- Class hierarchy (populated for Class and Interface kinds) --

    /// Parent class (extends). e.g., "BaseValidator" for
    /// `class EmailValidator extends BaseValidator`.
    extends: Option<String>,

    /// Implemented interfaces. e.g., ["Serializable", "Comparable"]
    /// for `class User implements Serializable, Comparable`.
    implements: Vec<String>,

    /// Whether this symbol is abstract (class or method).
    is_abstract: bool,

    // -- Type dependencies --

    /// Types referenced in this symbol's signature (parameter types,
    /// return types, generic constraints, property types). Used for
    /// transitive impact analysis: if a referenced type changes,
    /// this symbol is potentially affected.
    ///
    /// Example: `fn createUser(opts: UserOptions): Promise<User>`
    ///   -> type_dependencies: ["UserOptions", "User"]
    ///
    /// Includes type-only imports (`import type { X }`) which don't
    /// create runtime references but DO create API surface dependencies.
    type_dependencies: Vec<String>,

    // -- Member modifiers (populated for class/interface members) --

    /// Whether this member is readonly.
    is_readonly: bool,

    /// Whether this member is static.
    is_static: bool,

    /// Whether this member is an accessor (get/set) vs plain property.
    accessor_kind: Option<AccessorKind>,  // None, Get, Set, GetSet
}

enum AccessorKind { Get, Set, GetSet }

struct Signature {
    parameters: Vec<Parameter>,
    return_type: Option<String>,     // Stored as string -- type compatibility
    type_parameters: Vec<TypeParameter>,  // Full type param with default info
    is_async: bool,
}

struct TypeParameter {
    name: String,                    // e.g. "T"
    constraint: Option<String>,      // e.g. "extends Serializable"
    default: Option<String>,         // e.g. "= unknown"
}

struct Parameter {
    name: String,
    type_annotation: Option<String>,
    optional: bool,
    has_default: bool,
    default_value: Option<String>,   // Actual default value for static comparison
    is_rest: bool,                   // ...args rest parameter
}

/// A function whose body changed between refs (used by BU).
struct ChangedFunction {
    qualified_name: String,
    file: PathBuf,
    line: usize,
    kind: SymbolKind,
    visibility: Visibility,          // May be Private -- BU processes all
    old_body: String,
    new_body: String,
    old_signature: String,
    new_signature: String,
}

/// Diff of a test file between two refs (Option B: raw text diffs).
/// No framework-specific parsing -- just captures changed lines that
/// look like assertions (contain "expect", "assert", "should", etc.)
struct TestDiff {
    test_file: PathBuf,
    removed_lines: Vec<String>,      // Lines removed (v1 assertions)
    added_lines: Vec<String>,        // Lines added (v2 assertions)
    has_assertion_changes: bool,     // True if any assertion-like lines changed
    full_diff: String,               // Raw unified diff for LLM context
}

/// Inferred behavioral specification for a function.
///
/// Template-constrained: the LLM emits this as a JSON object matching
/// this schema. Each field uses structured sub-types rather than free-text
/// strings, enabling mechanical comparison without a second LLM call.
///
/// The LLM prompt includes the JSON schema as a template, and the response
/// is validated against it. This reduces hallucination (Preguss/SESpec
/// finding: template-guided generation drops hallucination from ~30% to
/// ~11-19%).
struct FunctionSpec {
    /// Input validation rules. Each constraint specifies which parameter,
    /// what condition is checked, and what happens on violation.
    preconditions: Vec<Precondition>,

    /// Guaranteed outputs. Each specifies a condition under which a
    /// specific return value/shape is produced.
    postconditions: Vec<Postcondition>,

    /// Error/exception behavior. Each specifies a trigger condition,
    /// the error type thrown/rejected, and the error message pattern.
    error_behavior: Vec<ErrorBehavior>,

    /// External state changes (DB writes, file I/O, network calls,
    /// event emissions, logging). Each specifies what is mutated and how.
    side_effects: Vec<SideEffect>,

    /// Free-text notes for behavioral nuances that don't fit the
    /// structured fields above. Compared via LLM fallback only.
    notes: Vec<String>,
}

struct Precondition {
    parameter: String,               // Which parameter this constrains
    condition: String,               // e.g., "must be non-empty string"
    on_violation: String,            // e.g., "throws TypeError"
}

struct Postcondition {
    condition: String,               // When this output is produced
    returns: String,                 // What is returned/resolved
}

struct ErrorBehavior {
    trigger: String,                 // What input/state causes the error
    error_type: String,              // e.g., "TypeError", "ValidationError"
    message_pattern: Option<String>, // Optional: regex or substring of message
}

struct SideEffect {
    target: String,                  // What external state is changed
    action: String,                  // e.g., "inserts row", "emits event"
    condition: Option<String>,       // When this side effect occurs
}

/// How the behavioral change was detected.
enum EvidenceSource {
    /// Test assertions changed -- developer explicitly declared new behavior.
    /// Highest confidence, no LLM needed.
    TestDelta(TestDiff),

    /// Test exists but didn't change. LLM analyzed with test as context.
    /// Medium confidence.
    LlmWithTestContext { spec_old: FunctionSpec, spec_new: FunctionSpec },

    /// No test found. LLM analyzed body diff alone.
    /// Lower confidence.
    LlmOnly { spec_old: FunctionSpec, spec_new: FunctionSpec },
}

/// Shared state between TD and BU (concurrent, thread-safe).
struct SharedFindings {
    /// Keyed by qualified_name. TD inserts structural breaks.
    /// BU checks before doing work.
    structural_breaks: DashMap<String, StructuralBreak>,

    /// BU inserts behavioral breaks here.
    behavioral_breaks: DashMap<String, BehavioralBreak>,

    /// Broadcast channel for TD -> BU coordination.
    /// TD sends qualified names as it inserts structural breaks.
    /// BU drains pending messages into a local skip set before each
    /// function analysis, avoiding redundant LLM calls on symbols
    /// TD has already identified structurally.
    td_broadcast: broadcast::Sender<String>,

    /// Shared semantic model built by TD during API extraction.
    /// TD parses all source files with oxc_parser + oxc_semantic and
    /// stores the result here. BU reads from it for find_callers()
    /// (same-file scope analysis) and the post-merge impact analysis
    /// step uses it for cross-file find_references() via the reverse
    /// import index. One parse of the full codebase, two consumers.
    semantic_model: OnceCell<ProjectSemanticModel>,
}

/// Project-wide semantic model built once by TD, shared with BU
/// and impact analysis.
struct ProjectSemanticModel {
    /// Per-file semantic analysis from oxc_semantic. Provides scope
    /// trees, symbol tables, and reference resolution within each file.
    file_semantics: HashMap<PathBuf, oxc_semantic::SemanticModel>,

    /// Reverse import index for cross-file reference lookup.
    /// Maps (source_file, exported_symbol) to all importing locations.
    /// Built by resolving every import declaration with oxc_resolver.
    import_index: HashMap<(PathBuf, String), Vec<ImportSite>>,
}

struct ImportSite {
    importing_file: PathBuf,
    local_binding: String,      // The name used in the importing file
    line: usize,
    call_sites: Vec<usize>,     // Lines where the binding is invoked
}
```

Type annotations are stored as **strings**. Type *compatibility* checking
(is `string | number` assignable to `string`?) is done inside each
`ApiExtractor` implementation, not in the diff engine. The extractor
normalizes types to a comparable canonical form.

#### Per-Language Implementations

```rust
/// TypeScript/JavaScript -- uses OXC + tsc .d.ts output
struct OxcLanguageSupport { /* config */ }

impl ApiExtractor for OxcLanguageSupport {
    fn extract(&self, repo: &Path, git_ref: &str) -> Result<ApiSurface> {
        // 1. git worktree add for the ref
        // 2. Run tsc --declaration --emitDeclarationOnly
        // 3. Parse .d.ts files with oxc_parser
        // 4. Use oxc_resolver to follow re-exports
        // 5. Build ApiSurface with fully resolved types
    }
}

impl DiffParser for OxcLanguageSupport {
    fn parse_changed_functions(&self, repo: &Path, from: &str, to: &str) -> Result<Vec<ChangedFunction>> {
        // 1. git diff from..to --name-only to get changed files
        // 2. For each changed file, parse both versions with oxc_parser
        // 3. Walk ASTs to find functions whose bodies differ
        // 4. Return all changed functions (public AND private)
    }
}

impl CallGraphBuilder for OxcLanguageSupport {
    fn find_callers(&self, model: &ProjectSemanticModel, symbol: &QualifiedName) -> Result<Vec<Caller>> {
        // 1. Look up the symbol's file in model.file_semantics
        // 2. Use the file's SemanticModel to find all references to the symbol
        // 3. For direct call expressions: return the enclosing function as caller
        // 4. HOF heuristic: if the symbol appears as an argument in a call
        //    expression (e.g., arr.map(symbol), emitter.on('event', symbol),
        //    setTimeout(symbol)), treat the enclosing function as a caller
        // 5. Return deduplicated list of Callers
    }

    fn find_references(&self, model: &ProjectSemanticModel, symbol: &QualifiedName) -> Result<Vec<Reference>> {
        // 1. Look up (symbol.file, symbol.name) in model.import_index
        // 2. For each ImportSite, build a Reference with file, line, and context
        // 3. Follow re-export chains via oxc_resolver
        // 4. Return all cross-file references for impact reporting
    }
}

/// Python -- uses tree-sitter (added later)
struct TreeSitterPythonSupport;

impl ApiExtractor for TreeSitterPythonSupport { /* ... */ }
impl DiffParser for TreeSitterPythonSupport { /* ... */ }
impl CallGraphBuilder for TreeSitterPythonSupport { /* ... */ }

/// Go -- uses tree-sitter (added later)
struct TreeSitterGoSupport;

impl ApiExtractor for TreeSitterGoSupport { /* ... */ }
impl DiffParser for TreeSitterGoSupport { /* ... */ }
impl CallGraphBuilder for TreeSitterGoSupport { /* ... */ }
```

#### The Orchestrator (Written Once)

```rust
/// The main analysis pipeline. Language-agnostic. Written once.
async fn analyze(
    lang: &dyn LanguageSupport,      // implements ApiExtractor + DiffParser + CallGraphBuilder
    tests: &dyn TestAnalyzer,         // test file discovery and assertion diff
    behavior: &dyn BehaviorAnalyzer,  // LLM-based, language-agnostic
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    use_llm: bool,
) -> Result<AnalysisReport> {
    let shared = Arc::new(SharedFindings::new());

    // Run TD and BU concurrently
    let (td_result, bu_result) = tokio::join!(
        top_down(lang, repo, from_ref, to_ref, shared.clone()),
        bottom_up(lang, tests, behavior, repo, from_ref, to_ref, shared.clone(), use_llm),
    );

    td_result?;
    bu_result?;

    // Reconciliation pass: resolve any BU findings whose
    // public/private status is ambiguous against TD's API surface
    reconcile(&shared)?;

    // Enrich all findings with impact data using the shared semantic model
    let model = shared.semantic_model.get()
        .expect("TD must populate semantic model before enrichment");
    let mut report_items = Vec::new();
    for (_, brk) in shared.structural_breaks.iter() {
        let refs = lang.find_references(model, &brk.symbol)?;
        report_items.push(enrich(brk, refs));
    }
    for (_, brk) in shared.behavioral_breaks.iter() {
        let refs = lang.find_references(model, &brk.symbol)?;
        report_items.push(enrich(brk, refs));
    }

    Ok(build_report(report_items))
}

/// TD: Extract public API at both refs, diff structurally.
async fn top_down(
    lang: &dyn ApiExtractor,
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    shared: Arc<SharedFindings>,
) -> Result<()> {
    let surface_old = lang.extract(repo, from_ref)?;
    let surface_new = lang.extract(repo, to_ref)?;

    for change in diff_surfaces(&surface_old, &surface_new) {
        let name = change.symbol.clone();
        shared.structural_breaks.insert(name.clone(), change);
        // Broadcast to BU so it can skip this symbol immediately,
        // even if it hasn't polled the DashMap yet.
        let _ = shared.td_broadcast.send(name);
    }
    Ok(())
}

/// BU: Parse diff, identify changed functions, check tests, analyze behavior,
///     walk UP call graph to find affected public APIs.
async fn bottom_up(
    lang: &dyn LanguageSupport,
    tests: &dyn TestAnalyzer,
    behavior: &dyn BehaviorAnalyzer,
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    shared: Arc<SharedFindings>,
    use_llm: bool,
) -> Result<()> {
    let mut td_rx = shared.td_broadcast.subscribe();
    let mut td_skip_set: HashSet<String> = HashSet::new();

    let changed_fns = lang.parse_changed_functions(repo, from_ref, to_ref)?;

    for func in changed_fns {
        // Drain any TD findings that arrived since last iteration.
        while let Ok(name) = td_rx.try_recv() {
            td_skip_set.insert(name);
        }

        // 1. Already found by TD? Skip (check both broadcast skip set
        //    and DashMap for findings that arrived before we subscribed).
        if td_skip_set.contains(&func.qualified_name)
            || shared.structural_breaks.contains_key(&func.qualified_name)
        {
            continue;
        }

        // 2. Check for associated test changes (no LLM needed).
        let test_files = tests.find_tests(repo, &func.qualified_name)?;
        let test_diff = test_files.iter()
            .filter_map(|tf| tests.diff_test_assertions(repo, tf, from_ref, to_ref).ok())
            .find(|td| td.has_assertion_changes);

        let (is_breaking, evidence) = match test_diff {
            // Case A: Test assertions changed.
            // Developer explicitly declared new behavior. High confidence.
            // No LLM needed -- the test diff IS the behavioral delta.
            Some(td) => {
                (true, EvidenceSource::TestDelta(td))
            }

            // Case B: Test exists but assertions didn't change.
            // Could be a refactor (no behavior change) or a missed test update.
            // Use LLM with the test file as additional context.
            None if !test_files.is_empty() && use_llm => {
                let test_ctx = tests.diff_test_assertions(
                    repo, &test_files[0], from_ref, to_ref
                )?;
                let old_spec = behavior.infer_spec_with_test_context(
                    &func.old_body, &func.old_signature, &test_ctx
                )?;
                let new_spec = behavior.infer_spec_with_test_context(
                    &func.new_body, &func.new_signature, &test_ctx
                )?;
                let verdict = behavior.specs_are_breaking(&old_spec, &new_spec)?;
                (verdict.is_breaking, EvidenceSource::LlmWithTestContext {
                    spec_old: old_spec, spec_new: new_spec
                })
            }

            // Case C: No test found. LLM spec inference from body only.
            None if test_files.is_empty() && use_llm => {
                let old_spec = behavior.infer_spec(&func.old_body, &func.old_signature)?;
                let new_spec = behavior.infer_spec(&func.new_body, &func.new_signature)?;
                let verdict = behavior.specs_are_breaking(&old_spec, &new_spec)?;
                (verdict.is_breaking, EvidenceSource::LlmOnly {
                    spec_old: old_spec, spec_new: new_spec
                })
            }

            // No LLM and no test change -- can't determine behavioral break.
            _ => continue,
        };

        if !is_breaking {
            continue; // Spec didn't change meaningfully -- stop here
        }

        // 3. Is this function public/exported?
        if func.visibility == Visibility::Exported {
            shared.behavioral_breaks.insert(func.qualified_name.clone(), /* ... */);
            continue;
        }

        // 4. Private function with breaking change -- walk UP call graph
        //    Uses the shared semantic model (built by TD) for reference
        //    resolution. Private function callers are always same-file,
        //    so oxc_semantic handles this without cross-file search.
        let model = shared.semantic_model.get()
            .expect("TD must populate semantic model before BU walks call graph");
        let mut to_check = vec![func.qualified_name.clone()];
        let mut visited = HashSet::new();

        while let Some(current) = to_check.pop() {
            if !visited.insert(current.clone()) {
                continue; // Cycle detection
            }

            let callers = lang.find_callers(model, &current)?;
            for caller in callers {
                if shared.structural_breaks.contains_key(&caller.qualified_name) {
                    continue; // TD already found it
                }

                // Propagation check: does this caller actually propagate
                // the behavioral break, or does it absorb/mask it?
                //
                // Without LLM: assume propagation (safe over-report).
                // With LLM: ask whether the caller's observable behavior
                // changes given the callee's behavioral delta. This
                // reduces false positives at the cost of an extra LLM call.
                let propagates = if use_llm {
                    behavior.check_propagation(
                        &caller.body,
                        &caller.signature,
                        &func.qualified_name,
                        &evidence,
                    )?
                } else {
                    // Conservative: assume the break propagates.
                    // This is the safe default -- over-report rather
                    // than miss a real break.
                    true
                };

                if !propagates {
                    continue; // Caller absorbs the break -- stop this path
                }

                if caller.visibility == Visibility::Exported {
                    shared.behavioral_breaks.insert(
                        caller.qualified_name.clone(),
                        BehavioralBreak {
                            symbol: caller.qualified_name,
                            caused_by: func.qualified_name.clone(),
                            call_path: build_call_path(&visited, &caller),
                            evidence: evidence.clone(),
                        },
                    );
                } else {
                    to_check.push(caller.qualified_name);
                }
            }
        }
    }

    Ok(())
}
```

#### What's Written Once vs Per-Language

| Component | Written once | Per-language |
|---|---|---|
| Orchestrator (TD/BU + merge) | Yes | -- |
| Structural diff engine (`diff_surfaces()`) | Yes | -- |
| Shared state / coordination | Yes | -- |
| `ApiSurface` / `Symbol` data structures | Yes | -- |
| Report generation / JSON output | Yes | -- |
| `BehaviorAnalyzer` (LLM-based) | Yes (language-agnostic) | -- |
| `ApiExtractor` implementation | -- | Yes |
| `DiffParser` implementation | -- | Yes |
| `CallGraphBuilder` implementation | -- | Yes |
| `TestAnalyzer` implementation | -- | Yes (test conventions differ per language/framework) |
| Type normalization rules | -- | Yes (inside each extractor) |

### TD: Structural Analysis (Deterministic)

TD extracts the public API surface at both refs using `tsc --declaration`
(for TypeScript) and OXC parsing, then diffs them structurally.

**For TypeScript/JavaScript** (OXC + tsc):

The public API surface includes:
- `export`ed functions, classes, interfaces, type aliases, enums, constants
- `export default` declarations
- Re-exports (`export { foo } from './bar'`, `export * from './baz'`)
- Public class members (methods, properties, getters/setters)
- Interface members and enum members
- Function/method parameter types, return types, generic constraints
- JSDoc `@public` / `@internal` / `@deprecated` annotations

The extraction workflow:
1. Check out the git ref into a temporary worktree
2. Detect package manager and install dependencies (see Worktree Lifecycle below)
3. Run `tsc --declaration --emitDeclarationOnly` to produce `.d.ts` files
4. Parse `.d.ts` files with `oxc_parser` (types are already resolved by tsc)
5. Use `oxc_resolver` to follow re-export chains and resolve barrel files
6. Assemble the `ApiSurface`
7. Clean up worktree (RAII guard ensures cleanup even on failure)

#### `tsc` Failure Handling

`tsc --declaration` is a hard requirement for TypeScript analysis. If it
fails at either ref, the tool aborts with a clear error message identifying
the failure mode. The tool detects and reports these failures distinctly:

| Failure Mode | Detection | Error Message Guidance |
|---|---|---|
| No `tsconfig.json` found | File not found at expected paths | "No tsconfig.json found at ref X. Ensure the project has a TypeScript configuration." |
| `noEmit: true` conflict | Parse tsconfig, check `noEmit` | "tsconfig.json has noEmit: true, which conflicts with --declaration. Consider adding a separate tsconfig.build.json." |
| Missing `node_modules` | Import resolution errors in tsc stderr | "Dependencies not installed at ref X. The tool runs package install automatically; check that the lockfile is valid." |
| TypeScript compilation errors | Non-zero exit code with diagnostics | "tsc --declaration failed with N errors at ref X. The project must compile cleanly for API extraction." |
| Monorepo `composite`/`references` not built | Project reference errors in stderr | "Project references not built. Run tsc --build in the monorepo root first." |
| Unsupported TypeScript syntax | Parser errors for newer syntax | "Code at ref X uses TypeScript features not supported by the installed tsc version." |

**Future: Graceful Degradation Options** (not implemented initially):

These fallbacks may be added in later phases to handle projects where
`tsc --declaration` is unreliable:

- **OXC-only parsing**: Fall back to parsing `.ts` source directly with OXC,
  without type resolution. Loses the ability to distinguish widening from
  narrowing type changes, but retains symbol-level structural diffs (removed/
  added exports, parameter count changes). Report with degraded confidence.
- **Best-effort `tsc`**: Try `tsc` with progressively relaxed options
  (`--skipLibCheck`, custom tsconfig overrides). Accept partial `.d.ts` output
  and fill gaps with OXC.
- **Hybrid per-ref**: If `tsc` succeeds at one ref but fails at the other,
  use `.d.ts` for the successful ref and OXC-only for the failed ref. Flag
  comparisons involving the OXC-only side with lower confidence.
- **Pure JS support**: Auto-generate a minimal tsconfig with `allowJs: true`
  and `declaration: true` for JavaScript-only projects.
- **`--from-path` / `--to-path` flags**: Accept pre-prepared directories
  instead of git refs, letting users handle dependency installation and
  build steps themselves.

#### Worktree Lifecycle

Each git ref is checked out into a temporary worktree for extraction.
The full lifecycle is managed by an RAII guard (`WorktreeGuard`) that
ensures cleanup even on panic, early return, or SIGINT.

**Steps:**

1. `git worktree add <tmpdir> <ref>` -- check out the ref
2. Detect package manager from lockfile in the worktree:
   - `pnpm-lock.yaml` -> `pnpm install --frozen-lockfile`
   - `yarn.lock` -> `yarn install --frozen-lockfile`
   - `package-lock.json` -> `npm ci`
   - None found -> hard fail ("No lockfile found at ref X")
3. Run the detected install command. Hard fail if it exits non-zero.
4. Run `tsc --declaration --emitDeclarationOnly`. Hard fail if it exits
   non-zero (see failure table above).
5. Proceed with OXC parsing of the generated `.d.ts` files.
6. On drop (success or failure): `git worktree remove --force <tmpdir>`

**Cleanup guarantees:**

- `WorktreeGuard` implements Rust's `Drop` trait to remove the worktree
- A `ctrlc` handler is registered at startup to trigger cleanup on SIGINT
- Worktrees use a predictable naming convention
  (`<repo>/.semver-worktrees/<ref-hash>`) so stale worktrees from prior
  crashed runs can be detected and cleaned up on next invocation

**Alternative for TypeScript**: Microsoft's `api-extractor` generates an
`.api.json` file that is essentially "rustdoc JSON for TypeScript" -- a
structured JSON representation of the entire public API surface with release
tags (`@public`, `@beta`, `@internal`). This could replace `.d.ts` parsing
if its schema provides sufficient detail.

**For other languages** (tree-sitter, added later):

- **Python**: tree-sitter-python, `__all__` exports, no-underscore-prefix convention
- **Go**: tree-sitter-go, capitalized identifiers
- **Rust**: rustdoc JSON (the cargo-semver-checks approach)
- **Java/Kotlin**: tree-sitter with `public` visibility modifier detection

**Structural diff change types** (deterministic, no LLM needed):

| Change | Breaking? | Detection |
|---|---|---|
| Symbol removed | Yes | Present in A, absent in B |
| Symbol added | No | Absent in A, present in B |
| Parameter added (required) | Yes | New param without default/optional |
| Parameter added (optional) | No | New param with default or `?` |
| Parameter removed | Yes | Param in A, absent in B |
| Parameter type narrowed | Yes | Type in B is a strict subset of A |
| Parameter type widened | No | Type in B is a strict superset of A |
| Return type widened | Yes | Callers may get unexpected types |
| Return type narrowed | No | Callers get a more specific type |
| Visibility reduced | Yes | `export` -> unexported |
| Visibility increased | No | unexported -> `export` |
| Made async | Yes | Return type changes from `T` to `Promise<T>` |
| Made sync | Yes | Return type changes from `Promise<T>` to `T` |
| Generic constraint tightened | Yes | Fewer types satisfy the constraint |
| Generic constraint loosened | No | More types satisfy the constraint |
| Generic type param added (required) | Yes | `Foo<T>` -> `Foo<T, U>` (no default for U) |
| Generic type param added (with default) | No | `Foo<T>` -> `Foo<T, U = unknown>` |
| Generic type param removed | Yes | `Foo<T, U>` -> `Foo<T>` |
| Generic type param reordered | Yes | `Foo<T, U>` -> `Foo<U, T>` |
| Default value changed | Yes | Same signature, different runtime behavior |
| Rest param added | No | `fn(a)` -> `fn(a, ...rest)` (existing calls work) |
| Rest param removed | Yes | Callers passing extra args will error |
| `readonly` added to property | Yes | Consumers that write to it will error |
| `readonly` removed from property | No | Consumers gain write access |
| `abstract` added to class/method | Yes | All subclasses must implement |
| `abstract` removed | No | Subclasses no longer forced to implement |
| `static` <-> instance change | Yes | Access pattern changes entirely |
| Accessor to property (or vice versa) | Yes | `get x()` -> `x: T` changes semantics |
| Base class changed (`extends`) | Yes | Inherited members may differ |
| Interface added to `implements` | No | Class now satisfies more contracts |
| Interface removed from `implements` | Yes | Consumers relying on the interface |
| Enum member added | Maybe | Breaks exhaustive `switch` in consumers |
| Enum member removed | Yes | Consumers referencing it will error |
| Enum member value changed | Yes | Runtime value differs silently |
| `this` parameter type changed | Yes | Call-site `this` context requirements change |
| Exported type/interface property added (required) | Yes | All consumers must provide it |
| Exported type/interface property added (optional) | No | Existing consumers unaffected |
| Exported type/interface property removed | Yes | Consumers referencing it will error |

Type compatibility is handled by the extractor, not the diff engine. The
extractor normalizes types to canonical strings; the diff engine does string
comparison.

#### Type Canonicalization Rules

After `tsc --declaration` resolves type aliases, `typeof` expressions, and
conditional types, the extractor applies these canonicalization rules so
that string comparison works correctly. Rules are applied by parsing the
type string with OXC's type annotation parser, transforming the AST, and
re-serializing to a string. This avoids fragile regex-based normalization.

**Rule 1: Union/Intersection Ordering**

Sort members alphabetically after recursively canonicalizing each member.
Flatten nested unions/intersections (they are associative).

- `string | number` -> `number | string`
- `B & A` -> `A & B`
- `(C | A) | B` -> `A | B | C`

**Rule 2: Array Syntax**

Normalize generic array forms to shorthand syntax.

- `Array<string>` -> `string[]`
- `ReadonlyArray<Item>` -> `readonly Item[]`
- `Array<string | number>` -> `(number | string)[]` (parenthesization
  added after Rule 1 reorders the union)

**Rule 3: Parenthesization**

Remove unnecessary parentheses. Add necessary parentheses for union/
intersection types inside array brackets.

- `(string)` -> `string`
- `((string))` -> `string`
- `(string | number)[]` -> kept (needed for array of union)

**Rule 4: Whitespace Normalization**

Collapse all whitespace to single spaces. Trim leading/trailing whitespace.
Remove trailing semicolons in object type literals.

- `{  a :  string ;  }` -> `{ a: string }`

**Rule 5: `never`/`unknown` Absorption**

Apply TypeScript's type algebra identities:

- `string | never` -> `string` (`never` is the identity for `|`)
- `T | unknown` -> `unknown` (`unknown` absorbs in `|`)
- `T & unknown` -> `T` (`unknown` is the identity for `&`)
- `T & never` -> `never` (`never` absorbs in `&`)

**Not handled (accepted false positives):**

- Unresolved utility types (`Omit<X, K>`, `Pick<X, K>`, `Partial<X>`)
  left in string form. If the type alias changed but resolves to the same
  structure, it is flagged as "changed." This is a safe over-report.
- Deeply nested conditional types that `tsc` didn't fully evaluate.
- Branded types / opaque type patterns.
- `Readonly<{ a: string }>` vs `{ readonly a: string }` -- `tsc` usually
  resolves `Readonly<T>` in `.d.ts` output, but if it doesn't, these are
  flagged as different. Safe over-report.

#### Class Hierarchy Impact Analysis

When TD detects a structural change to a class (base class method changed,
`abstract` added, constructor parameters changed), the impact analysis
must trace the class hierarchy downward to find affected subclasses:

1. Build a class hierarchy index from the `ApiSurface`:
   - Map each class's `extends` field to its parent
   - Build the reverse: `parent -> [child1, child2, ...]`
2. For each structural break on a class symbol:
   - Walk the hierarchy downward to find all subclasses
   - For each subclass, check if it overrides the changed member:
     - **Overrides**: subclass is unaffected (it has its own implementation)
     - **Inherits without overriding**: subclass IS affected -- report as
       transitive break with the inheritance chain as the call path
3. Special cases:
   - `abstract` added to a method -> ALL subclasses must implement it
   - Constructor parameter change -> ALL subclasses with `super()` calls
     are affected
   - Base class changed (`extends A` -> `extends B`) -> all inherited
     members may differ

The class hierarchy index is built from `.d.ts` output and stored in the
`ProjectSemanticModel`. OXC's parser provides `extends` and `implements`
clauses directly from the AST.

#### Type Dependency Impact Analysis

When TD detects a structural change to an exported type, interface, or
type alias, the impact analysis must trace *type-level references* to
find all symbols whose signatures depend on the changed type:

1. Build a type dependency index from the `ApiSurface`:
   - For each symbol, `type_dependencies` lists the types it references
   - Build the reverse: `type_name -> [symbols that reference it]`
2. For each structural break on a type/interface:
   - Look up all symbols that have it in their `type_dependencies`
   - Report them as transitively affected

This captures cases like:

```typescript
// v1
interface UserOptions { name: string; }

// v2 -- added required property
interface UserOptions { name: string; role: UserRole; }

// These functions are transitively affected even though their
// signatures didn't change -- their parameter TYPE changed:
function createUser(opts: UserOptions): User { ... }
function updateUser(id: string, opts: UserOptions): User { ... }
```

Type-only imports (`import type { X }`) are tracked in the import index
specifically for this purpose -- they don't create runtime references
but DO create API surface dependencies.

#### Package Manifest Analysis (`package.json`)

TD also analyzes `package.json` changes between refs. These are structural,
deterministic checks that don't require code parsing:

| Change | Breaking? | Detection |
|---|---|---|
| `main` / `module` / `types` entry point changed | Yes | Different file path |
| `exports` map entry removed | Yes | Consumers importing that subpath will error |
| `exports` map entry added | No | New import paths available |
| `exports` condition removed (e.g., `import`, `require`) | Yes | Consumers using that condition lose access |
| CJS -> ESM switch (`"type": "module"` added) | Yes | `require()` consumers break |
| ESM -> CJS switch (`"type": "module"` removed) | Yes | `import` consumers break |
| Peer dependency added | Yes | Consumers must install it |
| Peer dependency removed | No | Consumers no longer need it |
| Peer dependency version range narrowed | Yes | Consumers on old versions break |
| Peer dependency version range widened | No | More consumer versions accepted |
| `engines` constraint tightened | Yes | Consumers on older runtimes excluded |
| `engines` constraint loosened | No | More runtimes accepted |
| `bin` entry removed | Yes | CLI consumers lose the command |

Implementation: parse `package.json` at both refs as JSON, diff the
relevant fields structurally. No AST parsing needed -- just JSON comparison
with semantic awareness of semver ranges (for peer deps / engines).

### BU: Behavioral Analysis (LLM-Assisted)

BU starts from `git diff` -- the actual changed code -- and works upward.

**Step 1: Identify all changed functions**

Parse the diff to find every function (public AND private) whose body changed.
This uses `DiffParser`: parse both versions of each changed file with OXC,
walk the ASTs, and identify functions with different bodies.

**Step 2: Check shared state, skip if TD handled it**

For each changed function, check `SharedFindings`. If TD already flagged a
structural break on this symbol, skip it -- no additional analysis needed.

**Step 3: Spec inference on changed functions**

For functions TD didn't catch, use the LLM to infer behavioral specifications:

Rather than asking "is this change breaking?" (vague, unreliable), the tool
asks two focused questions:

1. "What does this function guarantee?" (infer pre/postconditions for v1)
2. "What does this function guarantee?" (infer pre/postconditions for v2)

Then compare the specs. If postconditions weakened or error behavior changed,
it's potentially breaking. If the specs are functionally identical (e.g., a
refactor that doesn't change behavior), BU stops here -- no upward propagation.

This is more reliable because:
- LLMs are better at *describing* what code does than *predicting impact*
- The inferred specs are auditable -- you can verify them
- Specs are cacheable -- if you've inferred specs for v1, reuse them when
  checking v1 vs v3
- Template-guided generation (Preguss) reduces hallucination from ~30% to
  ~11-19%

**Step 3b: Spec comparison strategy**

Once specs are inferred for v1 and v2, `specs_are_breaking()` determines
whether the behavioral change is breaking. This uses a two-tier approach:

**Tier 1: Structural comparison (no LLM)**

Compare the template-constrained `FunctionSpec` fields mechanically:

| Field | Breaking if... | Detection |
|---|---|---|
| `preconditions` | New precondition added (function accepts less) | Precondition in v2, absent in v1 |
| `preconditions` | Existing precondition tightened | `condition` string differs for same `parameter` |
| `postconditions` | Postcondition removed (function guarantees less) | Postcondition in v1, absent in v2 |
| `postconditions` | Return value changed for same condition | `returns` string differs for same `condition` |
| `error_behavior` | Error type changed | `error_type` differs for same `trigger` |
| `error_behavior` | New error case added (function throws where it didn't) | ErrorBehavior in v2, absent in v1 |
| `error_behavior` | Error case removed | Not breaking (function is more permissive) |
| `side_effects` | Side effect removed or changed | SideEffect in v1, absent/different in v2 |
| `side_effects` | New side effect added | Potentially breaking (consumers may not expect it) |

Matching is done by comparing the structured sub-fields:
- `Precondition` entries match on `parameter` (same param = same rule)
- `Postcondition` entries match on `condition` (same condition = same guarantee)
- `ErrorBehavior` entries match on `trigger` (same trigger = same error case)
- `SideEffect` entries match on `target` + `action` pair

String comparisons within matched entries use normalized lowercase
trimmed form. Exact match = no change. Any difference = potential break.

**Tier 2: LLM fallback (for `notes` and ambiguous matches)**

If Tier 1 finds no structural differences but the `notes` fields differ,
or if string matching is ambiguous (e.g., "must be non-empty" vs "must
have length > 0"), fall back to a single LLM call:

- Send both specs (full JSON) to the LLM
- Prompt: "These are behavioral specs for v1 and v2 of the same function.
  Are there any breaking changes? Specifically: are preconditions
  tightened, postconditions weakened, error types changed, or side effects
  altered? Respond with a structured verdict."
- This is a focused, bounded question (not "is this change breaking?" on
  raw code) so LLM reliability is higher

**Confidence scoring:**

The comparison method affects the confidence score:

| Scenario | Confidence |
|---|---|
| Test assertions changed (Option B) | 0.95 |
| Structural spec diff found a clear delta (Tier 1) | 0.80 |
| LLM spec comparison with test context (Tier 2 + tests) | 0.70 |
| LLM spec comparison without test context (Tier 2 alone) | 0.55 |
| Only `notes` field differed | 0.40 |

**Step 4: Walk UP the call graph**

If the changed function is private and its spec changed, BU walks UP:
- Use `CallGraphBuilder.find_callers()` to find what calls this function
- For each caller:
  - Already in shared state (TD found it)? Skip this path.
  - Is the caller public/exported? Record a transitive behavioral break.
  - Is the caller also private? Push it onto the stack, keep walking up.
- Cycle detection via visited set (handles recursive/mutually recursive functions).

No depth limit is needed because:
- BU starts from the actual change (bottom), not from the public API (top)
- Walking up terminates when it reaches a public function or runs out of callers
- The visited set prevents infinite loops from cycles

#### Call Graph Scope & Limitations

**Private functions: same-file only (fully handled)**

A non-exported function can only be called from within its own module.
`oxc_semantic`'s per-file scope analysis handles this completely -- it
tracks every symbol declaration and reference within a file's scope tree.
BU's "walk UP" from a changed private function uses same-file reference
resolution, which is reliable and has no blind spots for direct calls.

**Higher-order function (HOF) heuristics**

When a function is passed as a value rather than called directly,
`oxc_semantic` sees it as a reference but doesn't model the indirect
invocation. BU applies heuristic pattern matching to recognize common
HOF patterns as "call-like":

| Pattern | Example | Treatment |
|---|---|---|
| Array HOFs | `arr.map(fn)`, `arr.filter(fn)`, `arr.forEach(fn)` | Enclosing function is a caller of `fn` |
| Event emitters | `emitter.on('event', fn)`, `emitter.addListener(...)` | `fn` is reachable (caller context unknown) |
| Timers | `setTimeout(fn)`, `setInterval(fn)` | Enclosing function is a caller of `fn` |
| Promise chains | `promise.then(fn)`, `promise.catch(fn)` | Enclosing function is a caller of `fn` |
| Generic callbacks | `someFn(arg1, fn)` where `fn` is the changed symbol | Enclosing function is a caller of `fn` |

Detection: when processing references to a changed symbol, check if the
reference appears as an argument in a `CallExpression` AST node (rather
than being the callee). If so, treat the enclosing function as a caller.

**Cross-file references: import-chain index (for impact analysis)**

For public symbols (after TD+BU merge), cross-file impact analysis uses
the `ProjectSemanticModel.import_index`:

1. TD builds the reverse import index during API extraction by scanning
   all `import` declarations and resolving them with `oxc_resolver`
2. Impact analysis queries: "which files import symbol X from file Y?"
3. Re-export chains are followed: if `index.ts` re-exports from
   `utils.ts`, and `app.ts` imports from `index.ts`, the chain is traced

**Known limitations (documented in output)**

These patterns are not statically resolvable and are acknowledged as
blind spots in the analysis report:

- **Dynamic dispatch**: `obj[methodName]()`, `Reflect.apply()`,
  `.call()/.apply()` -- the callee is determined at runtime
- **Framework indirection**: React hooks (`useEffect`, `useMemo`),
  Express middleware (`app.use`), dependency injection containers,
  decorator patterns -- the framework mediates the call
- **External consumers**: code in other repositories that imports from
  this project is invisible to the analysis

The report includes a `call_graph_analysis` field indicating the
analysis method: `"static_with_hof_heuristics"`. Consumers can use this
to understand the completeness of impact analysis.

**Step 5: Record findings**

Each behavioral break records:
- The affected public symbol
- The private function that actually changed (the root cause)
- The call path between them (e.g., `createUser -> _processInput -> _normalizeEmail`)
- The old and new specs
- A confidence score

#### Test-Driven Behavioral Detection (Options B & C)

Before invoking the LLM for spec inference, BU checks for associated test
file changes. Test changes are a high-confidence signal for behavioral
changes because developers explicitly encode expected behavior in tests.
When a developer changes an assertion, they are declaring that the function's
contract changed -- that's stronger evidence than any LLM inference.

The `TestAnalyzer` trait uses two complementary approaches:

**Option B: Text-Based Test Diff Analysis (No LLM)**

Treat test diffs as plain text, using regex patterns to identify changed
assertion lines. No framework-specific AST parsing -- just heuristic
matching on common assertion patterns (`expect`, `assert`, `should`,
`toBe`, `toEqual`, `toThrow`, `toHaveBeenCalledWith`, etc.).

How it works:

1. Find test files by naming convention (`foo.ts` -> `foo.test.ts`,
   `foo.spec.ts`, `__tests__/foo.ts`)
2. Run `git diff from_ref..to_ref` on the test file
3. Filter diff hunks for lines matching assertion patterns
4. If assertions changed -> behavioral change, HIGH confidence, no LLM

What it catches:

- Changed expected values (`expect(result).toBe(5)` -> `expect(result).toBe(10)`)
- New or removed error expectations (`expect(() => fn()).toThrow()`)
- Changed assertion count or structure
- Renamed/added/removed test cases with assertion-bearing lines

What it misses:

- Assertions using custom matchers or uncommon framework DSLs
- Test setup/fixture changes that alter the meaning of unchanged assertions
- Tests in unrelated files that don't follow naming conventions
- Behavioral changes the developer didn't bother to test

This is the `TestDiff` struct in the shared data structures and the
`diff_test_assertions()` method on the `TestAnalyzer` trait.

**Option C: LLM-Augmented Test Context**

When a test file exists but Option B found no assertion changes, feed the
full test diff to the LLM as additional context alongside the function body.
The test file gives the LLM grounded, concrete examples of how the function
is expected to behave -- reducing hallucination compared to body-only
inference.

How it works:

1. Option B found no assertion changes, but a test file exists for this
   function
2. Include the full test diff (setup, teardown, assertion context) in the
   LLM prompt alongside the function body
3. The LLM sees concrete examples of expected behavior, anchoring its
   spec inference to real usage

The LLM prompt effectively becomes:

- "Here is the function body (old version and new version)"
- "Here are the tests for this function (old version and new version)"
- "What does this function guarantee? List preconditions, postconditions,
  error behavior, and side effects."

This is the `infer_spec_with_test_context()` method on the
`BehaviorAnalyzer` trait. When tests exist but didn't change, the fact
that the developer *didn't* update the tests is itself a signal -- it may
mean the behavior didn't actually change (refactor), or it may mean the
developer missed updating the test.

**B + C Decision Matrix**

| Scenario | Action | Confidence | LLM? |
|---|---|---|---|
| Test assertions changed (Option B match) | Report break directly | HIGH | No |
| Test exists, no assertion change | Option C: LLM + test context | MEDIUM | Yes |
| No test file found | LLM spec inference (body only) | LOWER | Yes |
| `--no-llm` and no test assertion change | Skip (indeterminate) | N/A | No |

**Why not Option A (structural assertion parsing)?**

Option A would parse test ASTs with framework-specific knowledge (Jest,
Vitest, Mocha, Chai, node:test, etc.) to extract structured assertion
data -- e.g., "this test asserts that `createUser('a@b.com')` returns
`{ id: 1, email: 'a@b.com' }`". This gives the most precise results but
requires:

- Per-framework parser implementations (Jest vs Vitest vs Mocha vs ...)
- Tracking framework API changes across versions
- Handling custom matchers, plugins, and assertion libraries
- Resolving test helper abstractions (`createTestUser(overrides)`)

The implementation effort is disproportionate to the gain. Option B catches
the obvious cases (changed expected values, new/removed assertions) with
simple regex, and Option C handles the ambiguous cases by delegating to the
LLM with rich context. Together, B + C cover the practical use cases with
far less implementation cost and no framework coupling. If a future language
plugin needs more precise test analysis, Option A can be added as an
enhanced `TestAnalyzer` implementation for that language without changing
the orchestrator.

**Optional: Patch oracle generation** (based on PatchGuru):

For highest confidence, generate an executable test that demonstrates the
behavioral difference. If the test passes on v1 but fails on v2 (or vice
versa), that's proof the behavior changed.

**Agent-agnostic invocation**: BU's LLM calls can be invoked via:
- Direct LLM API call (OpenAI, Anthropic, etc.)
- `goose run --no-session -q -t "..."`
- `opencode run "..."`
- Any other agent CLI via `--llm-command`
- Or skipped entirely (`--no-llm` flag) for purely static analysis

**Confidence reporting**: Behavioral changes include a confidence score and
the analysis method used, so consumers can filter by reliability:

```json
{
  "symbol": "createUser",
  "caused_by": "_normalizeEmail",
  "call_path": ["createUser", "_processInput", "_normalizeEmail"],
  "confidence": 0.92,
  "analysis_method": "llm_spec_inference",
  "old_spec": {
    "postconditions": ["Inserts user with lowercased, trimmed email"],
    "error_behavior": ["Throws if email is empty"]
  },
  "new_spec": {
    "postconditions": ["Inserts user with lowercased, trimmed, plus-stripped email"],
    "error_behavior": ["Throws if email is empty"]
  },
  "description": "Email normalization now strips + aliases, affecting all users created via createUser"
}
```

## Output Format

Same JSON schema as the current harnesses, extended with impact data:

```json
{
  "repository": "/path/to/repo",
  "comparison": {
    "from_ref": "v1.0.0",
    "to_ref": "v1.1.0",
    "from_sha": "abc123",
    "to_sha": "def456",
    "commit_count": 42,
    "analysis_timestamp": "2026-03-06T00:00:00Z"
  },
  "summary": {
    "total_breaking_changes": 3,
    "breaking_api_changes": 2,
    "breaking_behavioral_changes": 1,
    "files_with_breaking_changes": 2
  },
  "changes": [
    {
      "file": "src/api/users.ts",
      "status": "modified",
      "breaking_api_changes": [
        {
          "symbol": "createUser",
          "kind": "function",
          "change": "signature_changed",
          "before": "createUser(email: string, options?: CreateUserOptions): Promise<User>",
          "after": "createUser(email: string, role: UserRole, options?: CreateUserOptions): Promise<User>",
          "description": "Added required parameter 'role' before the optional 'options' parameter",
          "impact": {
            "internal_dependents": [
              {"file": "src/routes/signup.ts", "line": 23, "symbol": "handleSignup"},
              {"file": "src/routes/admin.ts", "line": 67, "symbol": "bulkCreateUsers"}
            ],
            "transitive_dependents": [
              {"file": "src/controllers/onboarding.ts", "line": 12, "symbol": "onboardNewUser"}
            ]
          }
        }
      ],
      "breaking_behavioral_changes": [
        {
          "symbol": "validateEmail",
          "kind": "function",
          "description": "Now rejects emails with '+' aliases (e.g. user+tag@example.com) that were previously accepted",
          "confidence": 0.92,
          "analysis_method": "llm_spec_inference",
          "impact": {
            "internal_dependents": [
              {"file": "src/api/users.ts", "line": 15, "symbol": "createUser"},
              {"file": "src/api/invites.ts", "line": 8, "symbol": "sendInvite"}
            ]
          }
        }
      ]
    }
  ]
}
```

## Implementation Language

**Recommendation: Rust**

| Factor | Rust | TypeScript | Python |
|---|---|---|---|
| OXC support | Native crates (`oxc_parser`, `oxc_semantic`, `oxc_resolver`) | npm bindings | N/A |
| tree-sitter support | Native (written in Rust) | Bindings (node-tree-sitter) | Bindings (py-tree-sitter) |
| tree-sitter-graph | Native crate | N/A | N/A |
| stack-graphs | Native crate (by GitHub) | N/A | N/A |
| Performance | Fastest | Moderate | Slowest |
| Single binary distribution | Yes | Requires Node.js | Requires Python |
| Prior art reference | cargo-semver-checks | api-extractor | pyright |

Rust gives native access to OXC, tree-sitter, tree-sitter-graph, and
stack-graphs. It produces a single static binary. cargo-semver-checks
demonstrates the pattern works for Rust; this tool applies the same pattern
to TypeScript (via `.d.ts`) and other languages (via tree-sitter).

**External dependency**: `tsc` (TypeScript compiler) is required for
TypeScript analysis. Every TypeScript project already has this installed.
For non-TypeScript languages, no external dependencies are needed.

## CLI Design

```
semver-analyzer <command> [options]

Commands:
  extract   Extract API surface from source code at a specific ref
  diff      Compare two API surfaces and identify structural changes
  impact    Analyze impact of changes on dependent code
  analyze   Full pipeline: extract -> diff -> impact -> behavioral
  serve     Start as an MCP server (stdio transport)

Examples:
  # Extract API surface at a tag
  semver-analyzer extract --repo /path/to/repo --ref v1.0.0 -o surface-v1.json

  # Compare two surfaces
  semver-analyzer diff --from surface-v1.json --to surface-v2.json -o changes.json

  # Full analysis between two tags
  semver-analyzer analyze --repo /path/to/repo --from v1.0.0 --to v1.1.0 -o report.json

  # Full analysis with LLM behavioral analysis via goose
  semver-analyzer analyze --repo /path/to/repo --from v1.0.0 --to v1.1.0 \
    --llm-command "goose run --no-session -q -t" -o report.json

  # Full analysis with LLM behavioral analysis via opencode
  semver-analyzer analyze --repo /path/to/repo --from v1.0.0 --to v1.1.0 \
    --llm-command "opencode run" -o report.json

  # Static analysis only (no LLM)
  semver-analyzer analyze --repo /path/to/repo --from v1.0.0 --to v1.1.0 \
    --no-llm -o report.json

  # Start as MCP server for agent integration
  semver-analyzer serve
```

## MCP Server Design

When running as an MCP server (`semver-analyzer serve`), the following tools
are exposed:

| Tool | Description |
|---|---|
| `extract_api_surface` | Extract API surface at a git ref |
| `diff_api_surfaces` | Compare two surfaces for structural changes |
| `analyze_impact` | Find dependents of changed symbols |
| `analyze_breaking_changes` | Full pipeline analysis |

Both Goose and OpenCode can connect to this MCP server as an extension,
replacing the current bash-based harnesses with a proper static analysis tool.

## Implementation Roadmap

**Revised estimate: 14 weeks** (was 10 weeks). The expanded scope --
type canonicalization, class hierarchy tracking, type dependency analysis,
package.json diffing, HOF heuristics, propagation checks, and the
testing strategy -- adds ~4 weeks of implementation and testing effort.

### Phase 1: Foundation + TD Pipeline (Weeks 1-4)

- Set up Rust workspace with OXC dependencies (`oxc_parser`, `oxc_semantic`,
  `oxc_resolver`) and `tokio` for async
- Define shared data structures: `ApiSurface`, `Symbol` (with `extends`,
  `implements`, `type_dependencies`, modifier fields), `Signature` (with
  `TypeParameter`), `Parameter` (with `default_value`, `is_rest`),
  `ChangedFunction`, `SharedFindings`
- Define traits: `ApiExtractor`, `DiffParser`, `CallGraphBuilder`,
  `TestAnalyzer`, `BehaviorAnalyzer`
- Implement worktree lifecycle: `WorktreeGuard` RAII, package manager
  detection, `npm ci`/`yarn install`/`pnpm install`, stale worktree cleanup
- Implement `OxcLanguageSupport::extract()`: run `tsc --declaration`, parse
  `.d.ts` output with OXC, build `ProjectSemanticModel`
- Implement type canonicalization (5 rules: union ordering, array syntax,
  parenthesization, whitespace, never/unknown absorption)
- Implement `diff_surfaces()` -- language-agnostic structural diff covering
  all change types in the structural diff table (30+ categories)
- Implement class hierarchy index and type dependency index
- Implement package.json manifest diffing
- Startup validation (git repo check, tsc availability, ref existence)
- Unit tests: type canonicalization golden files, diff_surfaces on synthetic
  ApiSurface pairs, package.json diff, hierarchy/type dependency index
- CLI: `semver-analyzer analyze --repo /path --from v1.0 --to v1.1 --no-llm`
  (TD-only mode, no BU, no LLM)
- Monorepo build support: solution tsconfig detection (`tsc --build`),
  project build fallback (`yarn build` / `npm run build`), `--build-command`
  CLI option for custom build pipelines
- **Milestone**: structural breaking changes reported, fully deterministic,
  with type canonicalization, class hierarchy, type deps, package.json,
  and monorepo support (validated against patternfly-react: 56k+ symbols)

### Phase 2: BU Pipeline + Call Graph + Test Detection (Weeks 5-7)

- Implement `OxcLanguageSupport::parse_changed_functions()`: parse git diff,
  identify all changed functions via OXC AST comparison
- Populate `ProjectSemanticModel` during TD extraction: per-file semantic
  analysis + reverse import index via `oxc_resolver`
- Implement `OxcLanguageSupport::find_callers()`: use shared
  `ProjectSemanticModel` for same-file reference resolution + HOF heuristic
  detection (map/filter/on/setTimeout/Promise.then patterns)
- Implement `OxcLanguageSupport::find_references()`: use `import_index` for
  cross-file impact analysis of broken public symbols, plus type-level
  reference tracing for interface/type alias changes
- Implement `SharedFindings` with `DashMap` + broadcast channel +
  `OnceCell<ProjectSemanticModel>` for concurrent TD/BU coordination
- Wire up the concurrent orchestrator (`tokio::join!` of TD and BU)
- Implement BU's upward walk with cycle detection
- Implement `TestAnalyzer` trait with Option B (text-based test diff analysis):
  - Test file discovery by naming convention (`.test.ts`, `.spec.ts`,
    `__tests__/`)
  - Regex-based assertion change detection (`expect`, `assert`, `toBe`, etc.)
  - `TestDiff` struct population with assertion-bearing diff lines
  - When test assertions changed, flag as behavioral break (no LLM needed)
- Integration test fixtures: `added-required-param`, `removed-export`,
  `private-fn-behavior-change`, `class-hierarchy-break`, `no-breaking-changes`
- CLI: `semver-analyzer analyze --repo /path --from v1.0 --to v1.1 --no-llm`
  now runs TD+BU concurrently, BU catches test-evidenced behavioral breaks
  without LLM and walks the call graph upward from them
- **Milestone**: concurrent TD/BU working, call graph walking verified,
  test-driven behavioral detection (Option B) working without LLM

### Phase 3: LLM Behavioral Analysis + Test Context (Weeks 8-10)

- Implement `BehaviorAnalyzer` trait with template-constrained spec inference:
  - `FunctionSpec` JSON schema template for LLM prompts
  - `infer_spec()`: body-only spec inference (no test)
  - `infer_spec_with_test_context()`: include test diffs in LLM prompt when
    test exists but Option B found no assertion changes
  - Response validation against `FunctionSpec` schema (reject malformed, retry once)
- Implement spec comparison (Step 3b):
  - Tier 1: structural comparison on `FunctionSpec` fields (matching by
    parameter/condition/trigger, detecting additions/removals/changes)
  - Tier 2: LLM fallback for `notes` field diffs and ambiguous string matches
- Implement `check_propagation()`: LLM-assisted caller impact check with
  conservative fallback (assume propagation) in `--no-llm` mode
- Agent-agnostic invocation: `--llm-command` flag for goose/opencode/direct API
- Confidence scoring across all tiers (0.95 test delta -> 0.40 notes-only)
- LLM error handling: malformed response retry, timeout, rate limit backoff,
  cost circuit breaker (`--max-llm-cost`)
- Early termination: if spec didn't change, stop upward propagation
- Cost tracking and reporting in output JSON (`llm_usage` section)
- Integration test fixtures with LLM: `private-fn-behavior-change` with
  LLM enabled, snapshot-based regression tests
- **Milestone**: end-to-end analysis with tiered behavioral change detection,
  cost controls, and error resilience

### Phase 4: Impact Enrichment + Reporting (Weeks 11-12)

- After TD+BU merge, enrich all findings with impact data using shared
  `ProjectSemanticModel`:
  - Value-level references via `find_references()`
  - Type-level references via type dependency index
  - Class hierarchy impact via hierarchy index
- Internal dependents (what code in the repo uses the broken symbol)
- Transitive dependents (through re-export chains)
- JSON output with full call paths, specs, confidence scores, cost report
- Reconciliation pass for ambiguous public/private status
- `call_graph_analysis` field in output indicating analysis completeness
- Integration test: full pipeline validation against all fixtures
- Real-world validation: run against PatternFly testdata, compare to
  migration guide
- **Milestone**: production-quality output format with impact analysis

### Phase 5: MCP Server + Agent Integration (Weeks 13-14)

- Wrap the CLI as an MCP server (stdio transport)
- Tools: `extract_api_surface`, `diff_api_surfaces`, `analyze_impact`,
  `analyze_breaking_changes`
- Both Goose and OpenCode can connect to it
- Replace the current bash-based harnesses
- End-to-end test: invoke via MCP, verify output matches CLI
- **Milestone**: agent-integrated, ready for production use

### Phase 6: Additional Languages (Ongoing)

- Implement `TreeSitterPythonSupport` (tree-sitter-python)
- Implement `TreeSitterGoSupport` (tree-sitter-go)
- Each implementation needs `ApiExtractor` + `DiffParser` + `CallGraphBuilder`
  + `TestAnalyzer`
- Orchestrator, diff engine, LLM behavioral analysis, and output format are
  all reused unchanged

## What the Current Harnesses Miss

These are gaps in the goose-harness and opencode-harness that this tool
addresses:

1. **Type compatibility analysis** -- Distinguishing widening (safe) from
   narrowing (breaking) type changes. The LLM-only approach frequently gets
   this wrong (ReDef 2025 confirms models don't truly understand type changes).

2. **Transitive impact** -- A->B->C dependency chains mean a single break can
   cascade. The current harnesses report that function A changed, but not that
   functions B and C are also broken as a consequence.

3. **Function body change detection** -- BU identifies changed functions via
   AST comparison (parsing both versions with OXC and diffing). Only functions
   whose bodies actually differ undergo behavioral analysis. This dramatically
   reduces LLM usage compared to sending all functions to the LLM.

4. **Re-export resolution** -- Removing a re-export is only breaking if no
   other export path exists for that symbol. `oxc_resolver` handles this.

5. **Schema/config changes** -- Breaking changes often happen in API schemas
   (OpenAPI, protobuf, GraphQL), database migrations, or configuration files.
   These are outside the scope of code-level analysis but should be flagged.

6. **Default value changes** -- Changing a default parameter value doesn't
   change the signature but can silently change behavior. Detectable statically
   by comparing default value AST nodes, without needing an LLM.

7. **Overload resolution changes** -- In TypeScript, adding or removing
   function overloads can change which overload a specific call site resolves
   to, potentially breaking callers.

## The Biggest Risk

**LLM reliability for behavioral analysis.** The research shows:

- Current code models don't truly understand modifications (ReDef 2025)
- LLMs misclassify correct code as defective (ICSE 2026)
- Models can be confidently wrong (CHOKE 2025)
- Best published precision for LLM-based patch analysis: 0.62 (PatchGuru)

Mitigations built into the design:
- LLM is only used for behavioral analysis, not structural (Layers 1-3 are
  deterministic)
- Spec-inference approach is more reliable than direct classification
- Confidence scores let consumers filter low-confidence findings
- `--no-llm` flag provides a fully static fallback
- Template-guided prompts with static analysis constraints reduce hallucination

The tool is designed so that **Layers 1-3 alone are useful** without any LLM.
Layer 4 adds value but is explicitly optional and comes with reliability
caveats.

## Testing Strategy

Testing a breaking change analyzer requires verifying correctness at
multiple levels. The strategy uses three tiers:

### Tier 1: Unit Tests (per component)

| Component | Test approach | What's verified |
|---|---|---|
| Type canonicalization | Golden file tests: input type string -> canonical output | All 5 normalization rules, edge cases (nested unions, generic arrays, never/unknown absorption) |
| `diff_surfaces()` | Synthetic `ApiSurface` pairs with known changes | Every row in the structural diff table produces the correct change type |
| `FunctionSpec` comparison | Synthetic spec pairs | Tier 1 structural comparison correctly classifies breaking/non-breaking |
| Package manifest diffing | Synthetic `package.json` pairs | All 14 manifest change types detected correctly |
| Class hierarchy index | Synthetic class hierarchies | Downward walk finds all affected subclasses |
| Type dependency index | Synthetic type references | Reverse index correctly maps types to dependent symbols |
| HOF heuristic detection | Source files with map/filter/on/setTimeout patterns | Enclosing functions correctly identified as callers |
| Test file discovery | Directory layouts with various naming conventions | `.test.ts`, `.spec.ts`, `__tests__/` patterns found |
| Assertion regex matching | Test diff snippets | `expect`, `assert`, `toBe`, `toThrow` etc. correctly identified |

### Tier 2: Integration Tests (pipeline correctness)

Curated test fixtures: small TypeScript projects with known breaking changes
between two versions. Each fixture is a git repo with two tagged commits.

| Fixture | What it tests |
|---|---|
| `added-required-param` | Function gains a required parameter |
| `type-narrowing` | Return type widens (breaking), parameter type widens (not breaking) |
| `removed-export` | Exported symbol removed |
| `private-fn-behavior-change` | Private function changes behavior, caller is public |
| `class-hierarchy-break` | Base class method changes, subclass inherits |
| `interface-property-added` | Required property added to exported interface |
| `barrel-reexport-removed` | Re-export removed from barrel file |
| `enum-member-added` | Enum gains a member (exhaustiveness concern) |
| `package-json-esm-switch` | `"type": "module"` added |
| `default-value-changed` | Default parameter value changes |
| `no-breaking-changes` | Refactor with no breaking changes (tests for false positives) |

Each fixture has an expected output JSON. The integration test runs the
full pipeline and diffs against the expected output.

### Tier 3: Real-World Validation

Run the tool against known semver-breaking releases in open-source projects
and compare findings against the project's changelog and migration guides.

| Project | Known break | Validation |
|---|---|---|
| PatternFly (existing testdata) | Various API changes between minor versions | Cross-reference with PatternFly migration guide |
| TypeScript (compiler) | Breaking changes documented per release | Cross-reference with TS release notes |
| React | Major version API removals | Cross-reference with React upgrade guides |

Real-world validation measures both **recall** (did we find the documented
breaks?) and **precision** (did we report false breaks?).

### LLM-Specific Testing

For LLM-based behavioral analysis, tests must account for non-determinism:

- **Determinism boundary**: verify that `--no-llm` mode produces identical
  output across runs (fully deterministic)
- **Spec schema validation**: verify LLM responses conform to the
  `FunctionSpec` JSON schema (reject malformed responses)
- **Regression snapshots**: for each fixture, store a "reference" LLM
  analysis. Flag regressions when the tool's LLM output diverges
  significantly from the reference (but allow minor rephrasing)
- **Cost tracking**: each test run logs token usage and cost per fixture

## Error Handling Strategy

The tool must handle failures gracefully across external processes (`git`,
`tsc`, package managers), file system operations, and LLM interactions.

### Error Classification

| Category | Examples | Handling |
|---|---|---|
| **Fatal** | No git repo, invalid refs, `tsc` fails (hard fail policy) | Abort with clear error message and remediation guidance |
| **Degraded** | LLM returns malformed spec, LLM timeout | Skip this function's behavioral analysis, log warning, continue with reduced coverage |
| **Recoverable** | Single file parse failure, one test file not found | Log warning, skip the affected symbol, continue analysis |
| **Environmental** | Shallow clone (no worktree support), missing `tsc` binary | Detect at startup, abort with installation instructions |

### Startup Validation

Before beginning analysis, validate the environment:

1. Verify `repo` is a git repository (not shallow, has the requested refs)
2. Verify `tsc` is available (check `node_modules/.bin/tsc` or global)
3. Verify the requested refs exist (`git rev-parse`)
4. Check available disk space (worktrees + node_modules need ~2x project size)
5. If `--llm-command` is specified, verify the command exists

### LLM Error Handling

LLM calls are the most failure-prone component:

- **Malformed response**: If the LLM returns JSON that doesn't match the
  `FunctionSpec` schema, retry once with a more explicit prompt. If retry
  fails, skip this function and log it as `analysis_skipped`.
- **Timeout**: LLM calls have a configurable timeout (default 30s per
  function). On timeout, skip and log.
- **Rate limiting**: If the LLM provider returns 429, implement exponential
  backoff with a maximum of 3 retries.
- **Cost circuit breaker**: If cumulative LLM cost exceeds a configurable
  threshold (`--max-llm-cost`, default $5.00), stop LLM analysis for
  remaining functions and complete with static-only results. Log a warning.

### Worktree Cleanup on Error

Handled by the RAII `WorktreeGuard` (see Worktree Lifecycle section).
Additionally:

- On startup, scan for stale worktrees from prior crashed runs
  (matching the `.semver-worktrees/` naming convention) and remove them
- Log all cleanup actions for debugging

## LLM Cost Estimation

LLM usage occurs in three places within BU:

1. **Spec inference** (`infer_spec` / `infer_spec_with_test_context`):
   2 calls per changed function (v1 spec + v2 spec)
2. **Spec comparison** (`specs_are_breaking`, Tier 2 fallback):
   1 call per function where Tier 1 structural comparison is ambiguous
3. **Propagation check** (`check_propagation`):
   1 call per caller in the walk-UP chain

### Cost Model

Assumptions: GPT-4o-class model at ~$2.50/M input tokens, ~$10/M output
tokens. Average function body: ~50 lines (~500 tokens). Spec output: ~200
tokens. Propagation check: ~300 tokens input, ~100 tokens output.

| Changeset size | Changed fns | Spec inference calls | Comparison calls | Propagation calls | Est. total cost |
|---|---|---|---|---|---|
| Small PR (5 files) | ~10 | 20 | ~5 | ~8 | ~$0.05 |
| Medium PR (20 files) | ~40 | 80 | ~20 | ~30 | ~$0.20 |
| Large PR (50 files) | ~100 | 200 | ~50 | ~80 | ~$0.50 |
| Major version bump | ~500 | 1000 | ~250 | ~400 | ~$2.50 |

### Cost Controls

- `--no-llm`: skip all LLM analysis ($0.00, static analysis only)
- `--max-llm-cost <amount>`: circuit breaker, stop LLM calls when
  cumulative cost exceeds threshold
- TD skip via broadcast channel: avoids redundant spec inference on
  symbols TD already identified structurally
- Early termination: if spec didn't change (Tier 1 comparison), no
  propagation check needed
- Test assertion detection (Option B): high-confidence behavioral break
  detection without any LLM call

### Cost Reporting

The output JSON includes a `cost` section:

```json
{
  "llm_usage": {
    "total_calls": 42,
    "spec_inference_calls": 30,
    "comparison_calls": 8,
    "propagation_calls": 4,
    "total_input_tokens": 21500,
    "total_output_tokens": 8400,
    "estimated_cost_usd": 0.14,
    "circuit_breaker_triggered": false
  }
}
```

## Known Issues & Future Work

### Monorepo Build & Extraction (Discovered via PatternFly)

Running against `patternfly-react` v5.4.0 vs v6.4.0 revealed several issues
that were partially addressed and one that remains open:

**Fixed**: Monorepo packages that require pre-tsc code generation steps
(CSS module declarations, icon component generation, design token files)
caused `tsc --declaration` to fail with "Cannot find module" for 6 of 8
packages. This was fixed by adding a cascading build strategy:

1. Detect **solution tsconfigs** (e.g., `packages/tsconfig.json` with
   `"references"`) and use `tsc --build` for proper topological ordering
2. If tsc fails partially, fall back to the project's own **`build` script**
   (e.g., `yarn build`) which handles code generation + compilation
3. A `--build-command` CLI option lets users specify a custom build command
   for projects with non-standard build pipelines

This increased symbol extraction from 2,297 to **56,539** (24.6x improvement).

**Open: ESM/CJS declaration deduplication**. After a full project build,
the extractor picks up `.d.ts` files from **both** `dist/esm/` and `dist/js/`
(or `dist/cjs/`) output directories. This roughly doubles the symbol count
and change count since the same API surface is declared in both build outputs.

Approach to fix:
- During `.d.ts` file discovery, detect when a package has multiple build
  output directories (e.g., `dist/esm/`, `dist/cjs/`, `dist/js/`)
- Prefer ESM declarations over CJS (ESM is the canonical modern format)
- Deduplicate symbols by qualified name, keeping only one copy
- Alternatively, respect the package's `"exports"` map or `"types"` field
  in `package.json` to determine which declaration files are the canonical
  entry points and only extract from those
- This should be implemented in `extract/mod.rs` in the `find_dts_files()`
  function or as a post-extraction deduplication pass

Impact on PatternFly analysis: would reduce symbols from ~57k to ~28k and
breaking changes from ~35k to ~17k (rough estimate), giving a more accurate
picture of actual API surface changes.

**Open: Suppress redundant tsc warnings when build fallback succeeds**.
Currently the output shows per-package tsc failure warnings even though the
subsequent `yarn build` fallback succeeds. These warnings are confusing to
users. When the build fallback succeeds, the per-package tsc warnings should
be suppressed or collapsed into a single summary line.

## Open Questions

1. **api-extractor vs raw `.d.ts`**: Does `api-extractor`'s `.api.json` schema
   contain enough information to replace `.d.ts` parsing? If so, it would be
   simpler and more structured. Need to evaluate its schema against the
   required breaking change categories.

2. **Monorepo package boundaries**: How do we handle `workspace:*`
   dependencies? Do we analyze cross-package impact automatically?

3. **Test-based validation**: Should we optionally run the project's test
   suite against the new code to validate whether a change actually breaks?
   This gives definitive answers but is slow and requires a working build.

4. **Config/schema changes**: Should we detect breaking changes in non-code
   artifacts (OpenAPI specs, protobuf definitions, GraphQL schemas)?

5. **Confidence thresholds**: For LLM-detected behavioral changes, what
   confidence threshold should trigger a "breaking" classification? Should
   this be configurable?

6. **Incremental analysis**: For large repos, can we cache API surfaces and
   only re-extract changed files?

7. **Patch oracle feasibility**: How practical is generating executable tests
   (PatchGuru-style) to prove behavioral differences? This would give the
   highest confidence but requires a working test/build environment.

---

## Known Gaps — Behavioral (4 remaining from v2 harness comparison)

These 4 symbols are detected by the v2 harness as behavioral breaking changes
but our LLM file-level analysis does not flag them. The LLM analyzes the file
and sees the diff but does not report the specific behavioral break.

1. **PopoverHeaderIcon** (`packages/react-core/src/components/Popover/PopoverHeaderIcon.tsx`)
   - v2: CSS class prefix changed from `pf-v5-c-popover__title-icon` to `pf-v6-c-popover__title-icon`
   - Root cause: Change is only in the imported CSS module (`styles.popoverTitleIcon`), not in the component source. The component code is unchanged — the behavioral difference comes from a transitive CSS token rename.

2. **Slider** (`packages/react-core/src/components/Slider/SliderStep.tsx`)
   - v2: CSS custom property renamed from `--pf-v5-c-slider__step--Left` to `--pf-v6-c-slider__step--InsetInlineStart`
   - Root cause: The diff is a CSS token import rename (`sliderStepLeft` → `sliderStepInsetInlineStart`). The LLM sees the variable rename but doesn't flag it as breaking since the component still sets a CSS custom property — just with a different name.

3. **TabButton** (`packages/react-core/src/components/Tabs/TabButton.tsx`)
   - v2: `data-ouia-component-type` attribute value changed from `PF5/TabButton` to `PF6/TabButton`
   - Root cause: The OUIA version change comes from `getOUIAProps()` helper, not from the component itself. The component code is unchanged — the behavioral difference is transitive via the helper function.

4. **WizardNav** (`packages/react-core/src/components/Wizard/WizardNav.tsx`)
   - v2: `data-ouia-component-type` changed from `PF5/WizardNav` to `PF6/WizardNav`
   - Root cause: Same as TabButton — transitive OUIA version change via `getOUIAProps()`.

### Common theme
All 4 are **transitive behavioral changes** — the component source is unchanged or trivially changed, but an imported dependency (CSS module or OUIA helper) changed its output. Fixing these requires either:
- Cross-file dependency tracking (if `getOUIAProps` changed, propagate to all callers)
- Prompt engineering to explicitly flag OUIA version and CSS module token renames as breaking
- A dedicated pass for known framework-specific patterns (PF CSS prefix migration, OUIA versioning)
