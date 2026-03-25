# Core Traits

## The `Language` Trait

The `Language` trait is the central integration point for multi-language support.
It is a supertrait that composes `LanguageSemantics` (semantic rules for the diff
engine) and `MessageFormatter` (human-readable descriptions). It also carries
four associated types that represent language-specific data flowing through
the analysis pipeline.

```rust
pub trait Language: LanguageSemantics + MessageFormatter + Send + Sync + 'static {
    /// Behavioral change categories for this language.
    ///
    /// Each language defines what kinds of behavioral changes it can detect.
    /// TypeScript/React: DomStructure, CssClass, CssVariable, Accessibility, etc.
    /// Go: ErrorHandling, Concurrency, IoBehavior, etc.
    type Category: Debug + Clone + Serialize + DeserializeOwned + Eq + Hash + Send + Sync;

    /// Manifest change types for this language's package system.
    ///
    /// TypeScript/npm: PeerDependencyAdded, ExportsEntryRemoved, ModuleSystemChanged, etc.
    /// Go: GoVersionChanged, RequireAdded, RequireVersionChanged, etc.
    type ManifestChangeType: Debug + Clone + Serialize + DeserializeOwned + Eq + PartialEq + Send + Sync;

    /// Evidence data carried on behavioral changes.
    ///
    /// Describes how a behavioral change was detected, with language-specific
    /// detail. The MessageFormatter uses this to produce appropriate descriptions.
    /// TypeScript: JsxDiff data (element before/after), CSS scan data, LLM spec summary.
    /// Go: interface satisfaction data, test assertion data.
    type Evidence: Debug + Clone + Serialize + DeserializeOwned + Send + Sync;

    /// Language-specific report data.
    ///
    /// Framework-specific groupings and analysis that don't fit the universal
    /// structural/behavioral model. Stored in the ReportEnvelope's language-specific
    /// section and only deserializable by consumers that know the language.
    /// TypeScript: ComponentSummary, HierarchyDelta, CompositionPatternChange, etc.
    /// Go: PackageSummary, InterfaceSatisfactionReport, etc.
    type ReportData: Debug + Clone + Serialize + DeserializeOwned + Send + Sync;

    /// Language identifier for serialization dispatch.
    ///
    /// Used in the ReportEnvelope to tag which language produced the report,
    /// so consumers can deserialize the language-specific section correctly.
    fn name() -> &'static str;
}
```

### Why a supertrait?

Composing `LanguageSemantics + MessageFormatter` rather than putting all methods on
one trait serves a practical purpose: the diff engine only needs `LanguageSemantics`
and can work with `&dyn LanguageSemantics` (no generic parameter). The message
formatting pass only needs `&dyn MessageFormatter`. Only the report types and
orchestrator need the full `Language` trait with its associated types, which
requires the generic parameter `<L: Language>`.

This means the diff engine stays non-generic:

```rust
pub fn diff_surfaces(
    old: &ApiSurface,
    new: &ApiSurface,
    semantics: &dyn LanguageSemantics,
    formatter: &dyn MessageFormatter,
) -> Vec<StructuralChange>
```

While report types carry the language identity:

```rust
pub struct AnalysisReport<L: Language> {
    pub packages: Vec<PackageChanges<L>>,
}
```

### Why four associated types?

Each represents a genuinely distinct concept:

| Type | What it represents | Who produces it | Who consumes it |
|------|-------------------|-----------------|-----------------|
| `Category` | What kind of behavioral change | BU pipeline, LLM prompts | Report, rule generator |
| `ManifestChangeType` | What kind of manifest change | Manifest differ | Report, rule generator |
| `Evidence` | How a behavioral change was detected | BU pipeline | MessageFormatter, report |
| `ReportData` | Framework-specific analysis groupings | Orchestrator | Rule generator, deep consumers |

Three or four associated types is within normal range for Rust traits. For
reference, `tower::Service` has 3, and serde's `Serializer` has 19.

---

## `LanguageSemantics` Trait

Semantic rules consumed by the diff engine during structural analysis. These
are the places where "is this breaking?" or "are these related?" differ
fundamentally by language.

```rust
pub trait LanguageSemantics {
    /// Is adding this member to this container a breaking change?
    ///
    /// This is the single rule that differs most fundamentally by language:
    /// - TypeScript: breaking only if the member is required (non-optional).
    ///   Structural typing means consumers passing the interface must now
    ///   provide the new member.
    /// - Go: ALWAYS breaking for interfaces. All implementors must add the
    ///   new method. Go has no optional interface members.
    /// - Java: breaking for abstract methods on interfaces (pre-default methods).
    ///   Not breaking for default methods (Java 8+).
    /// - C#: breaking for abstract members on interfaces.
    /// - Python: breaking for abstract methods on Protocol/ABC.
    fn is_member_addition_breaking(&self, container: &Symbol, member: &Symbol) -> bool;

    /// Are these two symbols part of the same logical family/group?
    ///
    /// Used to scope migration detection. When a symbol is removed, only
    /// symbols in the same family are considered as potential absorption
    /// targets for its members.
    ///
    /// - TypeScript/React: symbols in the same component directory
    ///   (e.g., `components/Modal/Modal.tsx` and `components/Modal/ModalHeader.tsx`)
    /// - Go: symbols in the same package
    /// - Java: classes in the same package
    /// - Python: symbols in the same module
    fn same_family(&self, a: &Symbol, b: &Symbol) -> bool;

    /// Are these two symbols the same concept, possibly at different paths?
    ///
    /// When true, migration detection does a full member comparison (all members,
    /// not just newly-added ones) because the candidate is assumed to be a direct
    /// replacement for the removed symbol.
    ///
    /// This resolves companion types -- types structurally paired by naming convention:
    /// - TypeScript: `Button` and `ButtonProps` (component + its props interface)
    /// - Go: `Client` and `ClientOptions` (struct + its configuration)
    /// - Java: `UserService` and `UserServiceImpl` (interface + its implementation)
    /// - Python: `UserView` and `UserViewMixin` (class + its mixin)
    ///
    /// Receives full `Symbol` references so implementations can use `kind`, `file`,
    /// `members`, `implements`, etc. -- not just names.
    fn same_identity(&self, a: &Symbol, b: &Symbol) -> bool;

    /// Numeric rank for a visibility level (higher = more visible).
    ///
    /// Used by the diff engine to determine if visibility was reduced (breaking)
    /// or increased (non-breaking). The ranking differs by language:
    ///
    /// TypeScript: Private(0) < Internal(1) < Public(2) < Exported(3)
    /// Java:       Private(0) < PackagePrivate(1) < Protected(2) < Public(3)
    /// C#:         Private(0) < PrivateProtected(1) < Internal(2) < Protected(3)
    ///             < ProtectedInternal(4) < Public(5)
    /// Go:         Internal(0) < Exported(1)  (only two levels)
    fn visibility_rank(&self, v: Visibility) -> u8;

    /// Parse union/constrained type values for fine-grained diffing.
    ///
    /// TypeScript has string literal union types (`'primary' | 'secondary' | 'danger'`).
    /// Python has `Literal['a', 'b']`. Most other languages return `None`.
    ///
    /// When this returns `Some`, the diff engine can detect individual union member
    /// additions and removals rather than reporting a single "type changed" event.
    fn parse_union_values(&self, type_str: &str) -> Option<BTreeSet<String>> {
        None
    }

    /// Post-process the change list before returning from diff_surfaces.
    ///
    /// TypeScript uses this to deduplicate default export changes (a symbol
    /// exported both by name and as `export default` produces duplicate changes).
    /// Most languages return unchanged.
    fn post_process(&self, _changes: &mut Vec<StructuralChange>) {}
}
```

### Design decisions

**`is_member_addition_breaking` receives full `Symbol` references**, not just
kinds or names. This lets Go check `container.kind == SymbolKind::Interface`
while TypeScript checks the member's optionality via its signature. Java could
check whether the method has a `default` keyword (modeled via a modifier or flag).

**`same_family` and `same_identity` replace `strip_props_suffix`** from the
current code. The old function was a narrow string manipulation that only worked
for React's `Props` suffix convention. The new trait methods:
- Separate two distinct questions: "are these related?" vs "are these the same thing?"
- Accept full `Symbol` references so implementations can use type system relationships
  (e.g., Java's `implements`), not just name patterns
- Have descriptive names that tell implementors what the method is used for

**`visibility_rank` moves to the trait** because visibility ordering differs by
language. The current hardcoded ranking maps TypeScript `protected` to `Internal`,
losing the distinction. Java's `protected` is more visible than package-private.
C# has six visibility levels.

**`parse_union_values` and `post_process` have defaults** because most languages
don't need them. Only TypeScript uses both; Python might use `parse_union_values`
for `Literal` types.

---

## `MessageFormatter` Trait

Produces all human-readable descriptions for structural changes. Each language
owns its messaging entirely -- there is no generic template in core.

```rust
pub trait MessageFormatter {
    /// Produce a human-readable description for a structural change.
    ///
    /// The `StructuralChange` carries all structured data: the change type
    /// (Added/Removed/Changed/Renamed/Relocated), the subject (what was
    /// affected), before/after values, and the symbol's kind.
    ///
    /// The formatter uses language-specific terminology:
    /// - TypeScript: "Required prop `onClick` was added to interface `ButtonProps`"
    /// - Go: "Method `Close` was added to interface `Reader` -- all implementors must add it"
    /// - Java: "Abstract method `validate()` was added to interface `Validator`"
    ///
    /// These descriptions are consumed by LLMs downstream, so language-appropriate
    /// terminology matters for generating accurate migration guidance.
    fn describe(&self, change: &StructuralChange) -> String;
}
```

### Why `describe` is the only method

Earlier iterations of this design had formatting primitives (`format_return_type`,
`format_param`, `member_term`) with a default `describe` implementation that
composed them. This was rejected because:

1. Each language genuinely produces different descriptions, not just different
   fill-in-the-blank values
2. Template-based descriptions produce awkward output for edge cases
3. The descriptions are consumed by LLMs, so quality matters more than DRY
4. A single `match` over `StructuralChangeType` in each language crate is
   straightforward to write and test

Each language crate implements one `match` with ~25 arms. The arms genuinely
differ -- Go would say "method added to interface (all implementors must update)"
while TypeScript says "required prop added to interface (all consumers must
pass it)". Trying to generalize that into a template produces worse output.

### What `describe` replaces

In the current code, description strings are built inline in the diff engine
(`compare.rs`, `helpers.rs`) using hardcoded TypeScript terminology:
- `unwrap_or("void")` -- TypeScript's default return type
- `starts_with("Promise<")` -- TypeScript's async wrapper
- `kind_label()` mapping `SymbolKind` to display strings like `"getter"`, `"setter"`

All of this moves to the language crate's `MessageFormatter` implementation.
The diff engine produces `StructuralChange` with `description: String::new()`,
then a formatting pass calls `formatter.describe()` to fill it in.

---

## Existing Extraction & Analysis Traits

These traits already exist and remain unchanged. They are implemented by each
language crate.

```rust
/// Extract an API surface from source at a git ref.
pub trait ApiExtractor {
    fn extract(&self, repo_path: &Path, git_ref: &str) -> Result<ApiSurface>;
}

/// Parse changed functions from a git diff.
pub trait DiffParser {
    fn parse_diff(&self, repo_path: &Path, from: &str, to: &str) -> Result<Vec<ChangedFunction>>;
}

/// Build a call graph for propagation analysis.
pub trait CallGraphBuilder {
    fn build(&self, repo_path: &Path, git_ref: &str) -> Result<CallGraph>;
    fn find_callers(&self, file: &Path, symbol: &str) -> Result<Vec<Caller>>;
}

/// Find and diff tests between versions.
pub trait TestAnalyzer {
    fn find_test_diffs(&self, repo_path: &Path, from: &str, to: &str) -> Result<Vec<TestDiff>>;
}

/// LLM-based behavioral spec inference (already language-agnostic).
pub trait BehaviorAnalyzer {
    fn infer_spec(&self, function_body: &str, signature: &str) -> Result<FunctionSpec>;
    fn infer_spec_with_test_context(
        &self,
        function_body: &str,
        signature: &str,
        test_context: &TestDiff,
    ) -> Result<FunctionSpec>;
    fn specs_are_breaking(
        &self,
        old: &FunctionSpec,
        new: &FunctionSpec,
    ) -> Result<BreakingVerdict>;
    fn check_propagation(
        &self,
        caller_body: &str,
        caller_signature: &str,
        callee_name: &str,
        evidence: &str,
    ) -> Result<bool>;
}
```

### Note on `BehaviorAnalyzer::check_propagation`

The `evidence` parameter changes from `&EvidenceSource` (a core enum with a
TS-specific `JsxDiff` variant) to `&str` (a pre-formatted description). The
language crate's `MessageFormatter` or `Evidence` type formats the evidence
into text before passing it to the LLM. The `BehaviorAnalyzer` trait stays
language-agnostic -- it just sends text to an LLM.

---

## Trait Relationship Diagram

```
                    ┌─────────────────┐
                    │    Language      │
                    │                 │
                    │  type Category  │
                    │  type Manifest  │
                    │  type Evidence  │
                    │  type Report    │
                    │  fn name()      │
                    └────────┬────────┘
                             │ supertrait of
                    ┌────────┴────────┐
                    │                 │
           ┌────────┴───┐    ┌───────┴────────┐
           │ Language    │    │  Message       │
           │ Semantics   │    │  Formatter     │
           │             │    │                │
           │ 3 required  │    │ 1 method:      │
           │ 2 optional  │    │  describe()    │
           │ methods     │    │                │
           └─────────────┘    └────────────────┘

    Used by diff engine        Used by formatting pass
    (no generic param)         (no generic param)
```

The `Language` trait with its associated types is only needed by code that
produces or consumes the report's language-specific data. The diff engine
and message formatting work through the sub-traits without generics.
