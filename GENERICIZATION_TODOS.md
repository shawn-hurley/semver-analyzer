# Core Genericization TODOs

Issues found while implementing the Java `Language` crate — things in core
that are either TS-leaky, poorly abstracted, or need reconsideration for
multi-language support.

---

## G1: `ChangedFunction` uses sentinel strings instead of `Option`

**File**: `crates/core/src/types/bu.rs:26`

`old_body`, `new_body`, `old_signature`, `new_signature` are `String`, not
`Option<String>`. An empty string means "function didn't exist at this ref"
but is ambiguous — a function can legitimately have an empty body (`{}`).

**Recommendation**: Change to `Option<String>` so callers can distinguish
"not present" from "empty body". This would affect the TS diff parser, the
BU pipeline orchestration, and the LLM prompt builder.

---

## G2: `ExpectedChild` is React-specific in core

**File**: `crates/core/src/types/report.rs:510`

`ExpectedChild` has fields `mechanism` ("child" vs "prop") and `prop_name`
which are React JSX concepts. The `default_mechanism()` function hard-codes
`"child"`. These concepts don't map to Java, Go, or Python.

**Recommendation**: Move `ExpectedChild` out of core into the TS crate (as
part of `TsAnalysisExtensions`), or generalize it. The concept of "parent
expects these sub-components" could be generic with language-specific
mechanism details, but right now the structure is React-shaped.

**Phase**: Deferred Phase 4 (from original genericization plan).

---

## G3: `ComponentSummary` names suggest React components

**File**: `crates/core/src/types/report.rs:348`

`ComponentSummary` has `name`, `definition_name`, `child_components`,
`expected_children`. These fields assume a React-like component model.
For Java, the equivalent would be "TypeSummary" with "nested types" or
"related beans". The field names leak the TS/React mental model.

**Recommendation**: Either rename to `TypeSummary` (generic) or accept
that this is a "UI component summary" concept used by both React and
potentially Angular/Vue, but irrelevant for Java/Go. Could be moved
behind `L::ReportData` as a language-specific concept.

---

## G4: `ContainerChange` description says "from the LLM"

**File**: `crates/core/src/types/report.rs:295`

The `description` field comment says "Description of the change from the
LLM." This couples core to the LLM pipeline. Container changes could also
be detected deterministically (as the Java crate would do for Spring bean
hierarchy changes).

**Recommendation**: Change doc comment to "Description of the change" 
(remove LLM reference). The source of the description is an implementation
detail.

---

## G5: `RemovalDisposition` variants are React-shaped

**File**: `crates/core/src/types/report.rs:444`

`RemovalDisposition::MovedToRelatedType` uses `mechanism` with values like
"child composition" and "prop-passing" — React JSX patterns. The
`MadeAutomatic` variant describes "prop is now auto-derived" — also a
React pattern. `TrulyRemoved` and `ReplacedByMember` are generic.

**Recommendation**: Keep the generic variants in core
(`TrulyRemoved`, `ReplacedByMember`, `MovedToRelatedType` with a generic
mechanism string). Move React-specific interpretation to the TS crate's
report builder.

---

## G6: `ApiChangeKind` doesn't cover `Enum` or `Constructor`

**File**: `crates/core/src/types/report.rs:188`

`ApiChangeKind` maps `Enum` and `EnumMember` → `Constant`, and
`Constructor` → `Method`. These lose semantic precision. Java enums are
fundamentally different from constants, and constructors have different
semver rules from methods.

**Recommendation**: Add `ApiChangeKind::Enum`, `ApiChangeKind::Constructor`
variants. This affects report output and rule generators.

---

## G7: `Visibility` lacks a `Default` impl

**File**: `crates/core/src/types/surface.rs:214`

`Visibility` doesn't implement `Default`. The Java crate had to work
around this in `JavaModifiers` with a manual `Default` impl.

Every language has a "default visibility" concept:
- Java: package-private (`Internal`)
- Go: unexported (`Internal`)
- Python: public (no access modifiers)
- TypeScript: depends on context

Since the default differs per language, a blanket `Default` impl may not
make sense. But it's worth considering adding
`impl Default for Visibility { fn default() -> Self { Self::Internal } }`
or letting languages specify it.

---

## G8: `TestConvention` is somewhat language-specific

**File**: `crates/core/src/types/bu.rs:321`

`TestConvention::DotTest` and `DotSpec` are TS/JS patterns. Java uses
`SuffixTest` (FooTest.java) and `TestsDir` (src/test/java/). The
`OtherSuffix` variant catches language-specific conventions, but the
named variants are TS-centric.

**Recommendation**: This is minor — the existing enum is extensible enough
with `OtherSuffix`. No action needed unless more languages expose issues.

---

## G9: `BehavioralChangeKind` has only two variants

**File**: `crates/core/src/types/bu.rs` (search for `BehavioralChangeKind`)

`BehavioralChangeKind::Function` and `BehavioralChangeKind::Class`. The
`Class` variant was added for React components (component-level behavioral
changes). Java doesn't really have a class-level behavioral concept that
differs from method-level.

**Recommendation**: The `Language::behavioral_change_kind` method already
lets each language map evidence types to these kinds. The two-variant enum
is sufficient for now — Java would use `Function` for everything, which
is the default.

---

## G10: LLM prompt hardcodes TS/React categories (from Phase 4 deferred)

**File**: `crates/llm/src/prompts.rs`

`build_file_behavioral_prompt` includes hardcoded categories:
"DOM_STRUCTURE", "CSS_CLASS", "CSS_VARIABLE", "ACCESSIBILITY",
"DEFAULT_VALUE", "LOGIC_CHANGE", "DATA_ATTRIBUTE", "RENDER_OUTPUT".

These are React behavioral categories. Java would need entirely different
ones (LOGIC_CHANGE, EXCEPTION_HANDLING, CONFIGURATION, CONCURRENCY, etc.).

**Recommendation**: The LLM crate should accept category labels from the
`Language` impl (via a new trait method or from `L::Category` enum
variants) rather than hardcoding them.

---

## G11: `default_kind()` hardcodes "class" in LLM crate (from Phase 4)

**File**: `crates/llm/src/invoke.rs`

The LLM invocation uses `default_kind() -> "class"` as a fallback for
behavioral change kind. This is TS/React-specific (treating components as
classes).

**Recommendation**: Remove the hardcoded default or make it configurable
per-language.

---

## G12: Orchestrator has TS-specific file path patterns

**File**: `src/orchestrator.rs`

The orchestrator may contain hardcoded patterns like `"*.ts"`, `"*.tsx"`,
`"package.json"`, `"node_modules"`, etc. These should be parameterized
through the `Language` trait constants (`SOURCE_FILE_PATTERNS`,
`MANIFEST_FILES`).

**Recommendation**: Audit the orchestrator for any string literals that
should come from `Language` constants. The current constants
(`MANIFEST_FILES`, `SOURCE_FILE_PATTERNS`) exist but may not be used
everywhere they should be.

---

## G13: `ApiChange.renders_element` is React-specific

**File**: `crates/core/src/types/report.rs:182`

`ApiChange` has a `renders_element: Option<String>` field documented as
"The HTML element this component renders (e.g., 'ol', 'div', 'footer')."
This is a React-specific concept for when a component is replaced by a
generic component that needs an explicit element type prop.

This field has no meaning for Java, Go, or Python. The Java report builder
sets it to `None` for every change.

**Recommendation**: Move to `L::ReportData` or to a language-specific
wrapper type. The core `ApiChange` struct should only contain fields
that are meaningful across all languages.

---

## G14: `TestConvention` variant names favor TS patterns

**File**: `crates/core/src/types/bu.rs:321`

`DotTest` and `DotSpec` are TS-specific naming patterns. Java uses
`Suffix("Test")` and `MirrorTree("src/test/java")` instead. The enum
itself is generic enough (the `Suffix` and `MirrorTree` variants handle
Java), but `DotTest`/`DotSpec` could be expressed as `Suffix(".test.")`
and `Suffix(".spec.")` for consistency.

**Recommendation**: Low priority. The current design works. Could
consolidate `DotTest`/`DotSpec` into `Suffix` if cleanliness is desired.

---

## Priority

| ID | Severity | Effort | Phase |
|----|----------|--------|-------|
| G1 | High | Medium | Next (affects correctness) |
| G2 | Medium | Low | Deferred Phase 4 |
| G3 | Low | Low | Deferred (rename only) |
| G4 | Low | Trivial | Now (doc change) |
| G5 | Medium | Medium | Deferred Phase 4 |
| G6 | Medium | Low | Next |
| G7 | Low | Trivial | Next |
| G10 | High | Medium | Phase 4 (LLM parameterization) |
| G11 | Medium | Low | Phase 4 |
| G12 | Medium | Medium | Phase 4 |
| G13 | Medium | Low | Deferred (move to L::ReportData) |
| G14 | Low | Trivial | Optional (naming consistency) |
