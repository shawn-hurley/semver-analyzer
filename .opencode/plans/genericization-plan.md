# Core Genericization Plan

Make the core crate truly language-agnostic by moving all
TypeScript/React/PatternFly-specific code into the TS crate.

## Current State

The core crate is architecturally designed to be language-agnostic with its
trait system, but significant React/TS/PatternFly domain knowledge has leaked
into it:

| File | Severity | What's leaking |
|------|----------|----------------|
| `types/sd.rs` | **Entire file** | `ComponentSourceProfile`, JSX, BEM, React APIs, DOM attributes, `DeprecatedReplacement` |
| `traits.rs` | **Heavy** | `compute_deterministic_hierarchy` (360 lines) — React `XProps` convention, `rendered_components`, `Omit<>` stripping. 500 lines of PatternFly test fixtures |
| `types/surface.rs` | **Moderate** | `Symbol.rendered_components` (JSX render tree), `Symbol.css` (PatternFly style tokens) |
| `types/report.rs` | **Moderate** | `ExpectedChild` (JSX child/prop mechanism), `ContainerChange` (JSX nesting), `ChildComponent`, `renders_element`, `HierarchyDelta` |
| `diff/rename.rs` | **Moderate** | `extract_token_value()` — parses TS `.d.ts` CSS variable annotations |
| `diff/relocate.rs` | **Moderate** | `canonical_path()` hardcodes `/deprecated/` and `/next/` path stripping |
| `diff/helpers.rs` | **Light** | `is_star_reexport()` — JS/TS barrel-file `export *` pattern |
| `diff/mod.rs` | **Moderate** | `derive_import_subpath()` hardcodes `/deprecated/` and `/next/` |

## Reference: `feature/genericize-language-specifics` Branch

The branch established the approach with 4 commits:

1. **`AnalysisExtensions` pattern** — opaque associated type for pipeline data
2. **`Symbol<M>` + static dispatch** — per-symbol metadata via generics
3. **Trait method extraction** — 3 new `LanguageSemantics` methods
4. **Java scaffold** — validated abstractions with a second language

Main has diverged with ~34 commits since the branch point, adding the SD
pipeline, composition trees, CSS profiles, managed attributes, deprecated
replacement detection, type-incompatible member renames, and more.

---

## Phase 0: Verification Safety Net

**Commit: "test: add Konveyor YAML snapshots and report digest for refactoring safety"**

Add pre-refactoring verification artifacts:

1. Add 3–5 insta snapshots in `ts/konveyor.rs` for representative Konveyor
   rule YAML output (rename, removal, type-change, CSS token consolidation).
2. Add snapshots for `konveyor_v2.rs` (composition, conformance, CSS removal).
3. Run the full pipeline against PatternFly v5→v6, capture a report digest
   (change counts by category, package counts, component summary counts)
   as a checked-in golden file or test assertion.

**Rationale**: The existing 104 insta diff snapshots cover the diff engine but
NOT the Konveyor rule YAML output format or v2 rules. No test currently catches
a serde field rename in the rules.

---

## Phase 1: Symbol\<M\> + Static Dispatch + Hierarchy Move

**Commit: "refactor: genericize Symbol\<M\> and ApiSurface\<M\>, switch diff engine to static dispatch, move hierarchy to TS"**

This is the largest commit. It must be atomic because `Symbol.rendered_components`
is read by the `compute_deterministic_hierarchy` default impl — removing the
field and moving the algorithm must happen simultaneously.

### 1.1 Genericize `Symbol<M>` and `ApiSurface<M>`

In `core/types/surface.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound(serialize = "M: Serialize", deserialize = "M: Deserialize<'de>"))]
pub struct Symbol<M: Default + Clone = ()> {
    // ... all existing fields unchanged ...
    pub members: Vec<Symbol<M>>,   // recursive, now generic
    pub language_data: M,          // NEW — replaces rendered_components + css
}
```

- Default type parameter `M = ()` preserves backward compat for existing code
  using plain `Symbol`.
- Remove `rendered_components: Vec<String>` and `css: Vec<String>` from `Symbol`.
- Add `Symbol<()>::with_metadata<M>(self) -> Symbol<M>` conversion method.
- `ApiSurface<M>` wraps `Vec<Symbol<M>>`.

### 1.2 Create `TsSymbolData`

New file `crates/ts/src/symbol_data.rs`:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TsSymbolData {
    pub rendered_components: Vec<String>,
    pub css: Vec<String>,
}
```

Update `ts/extract/mod.rs` to populate `language_data: TsSymbolData` during
extraction (the `populate_rendered_components` function and CSS token
extraction already produce this data).

### 1.3 Add `SymbolData` associated type

On `Language` trait:

```rust
type SymbolData: Debug + Clone + Default + PartialEq + Eq
    + Serialize + DeserializeOwned + Send + Sync;
```

TypeScript sets `type SymbolData = TsSymbolData`.

### 1.4 Make `LanguageSemantics<M>` and `HierarchySemantics<M>` generic

```rust
pub trait LanguageSemantics<M: Default + Clone = ()> {
    fn is_member_addition_breaking(&self, container: &Symbol<M>, member: &Symbol<M>) -> bool;
    fn same_family(&self, a: &Symbol<M>, b: &Symbol<M>) -> bool;
    // ... all methods that take Symbol now take Symbol<M> ...
    fn hierarchy(&self) -> Option<&dyn HierarchySemantics<M>> { None }
    // renames() and body_analyzer() stay non-generic (they don't take Symbol)
}

pub trait HierarchySemantics<M: Default + Clone = ()> {
    fn family_name_from_symbols(&self, symbols: &[&Symbol<M>]) -> Option<String>;
    fn is_hierarchy_candidate(&self, sym: &Symbol<M>) -> bool;
    fn compute_deterministic_hierarchy(
        &self,
        new_surface: &ApiSurface<M>,
        structural_changes: &[StructuralChange],
    ) -> HashMap<String, HashMap<String, Vec<ExpectedChild>>> {
        let _ = (new_surface, structural_changes);
        HashMap::new() // Gutted — language impls override
    }
    // ... other methods unchanged ...
}
```

`Language` supertrait becomes `LanguageSemantics<Self::SymbolData>`.

`RenameSemantics` and `BodyAnalysisSemantics` stay non-generic — they only
take string slices (`&str`, `&[&str]`), not `Symbol`.

### 1.5 Switch diff engine to static dispatch

All functions in `diff/mod.rs`, `diff/compare.rs`, `diff/rename.rs`,
`diff/relocate.rs`, `diff/migration.rs` gain `<M, S>` type parameters:

```rust
// Before:
pub fn diff_surfaces_with_semantics(
    old: &ApiSurface, new: &ApiSurface, semantics: &dyn LanguageSemantics
) -> Vec<StructuralChange>

// After:
pub fn diff_surfaces_with_semantics<M: Default + Clone, S: LanguageSemantics<M>>(
    old: &ApiSurface<M>, new: &ApiSurface<M>, semantics: &S
) -> Vec<StructuralChange>
```

`MinimalSemantics` gets a blanket impl:

```rust
impl<M: Default + Clone> LanguageSemantics<M> for MinimalSemantics { ... }
```

**`&dyn LanguageSemantics` sites to update** (11 total, all in `core/diff/`):
- `mod.rs:63, 790`
- `compare.rs:28, 43, 228, 264, 448, 636, 884`

The orchestrator already uses `L: Language` generics (not `dyn`), so it
needs no dispatch changes — `lang: &L` already satisfies `&S` where
`S: LanguageSemantics<L::SymbolData>`.

### 1.6 Move hierarchy algorithm to TS

- Gut the 360-line `compute_deterministic_hierarchy` default impl in
  `core/traits.rs` (replace with `HashMap::new()`).
- Move the full algorithm to `TypeScript`'s `HierarchySemantics<TsSymbolData>`
  impl in `ts/language.rs`. It accesses `sym.language_data.rendered_components`
  instead of `sym.rendered_components`.
- Move ~500 lines of PatternFly test fixtures from `core/traits.rs` tests
  to `ts/` test modules.

**Why merged with Symbol\<M\>**: The default impl reads
`sym.rendered_components` directly. When that field moves to `TsSymbolData`,
the default impl won't compile. TS currently does NOT override this method —
it relies entirely on the default. These changes are structurally coupled.

### Files touched

`core/types/surface.rs`, `core/traits.rs` (remove ~860 lines impl+tests),
`core/diff/mod.rs`, `core/diff/compare.rs`, `core/diff/rename.rs`,
`core/diff/relocate.rs`, `core/diff/migration.rs`, `ts/symbol_data.rs` (new),
`ts/language.rs`, `ts/extract/mod.rs`, `ts/report.rs`, `ts/konveyor.rs`,
`ts/composition/mod.rs`, `ts/source_profile/diff.rs`

### Test impact

- All core diff tests need mechanical `rendered_components: vec![], css: vec![]`
  → removal (these become part of `language_data: ()` for `Symbol<()>`).
- 104 insta snapshots should be unaffected (they capture normalized output,
  not internal types).
- ~500 lines of hierarchy tests move from core to TS.

---

## Phase 2: AnalysisExtensions + SD/Hierarchy Type Migration

**Commit: "refactor: add AnalysisExtensions, move SD pipeline and hierarchy types to TS crate"**

### 2.1 Add `AnalysisExtensions` associated type

On `Language` trait:

```rust
type AnalysisExtensions: Debug + Clone + Default + Serialize + DeserializeOwned + Send + Sync;
```

### 2.2 Move `types/sd.rs` to TS crate

Delete `core/types/sd.rs` entirely. All types move to `ts/sd_types.rs`:

- `ComponentSourceProfile`
- `SourceLevelChange`, `SourceLevelCategory`, `SourceLevelSeverity`
- `CompositionTree`, `CompositionEdge`, `ChildRelationship`
- `CompositionChange`, `CompositionChangeType`
- `ConformanceCheck`, `ConformanceCheckType`
- `SdPipelineResult`
- `ManagedAttributeBinding`
- `DeprecatedReplacement`

These are 100% React/JSX/BEM/PatternFly concepts.

### 2.3 Move hierarchy types to TS crate

From `core/types/report.rs` to `ts/hierarchy_types.rs`:

- `ExpectedChild` (JSX child/prop mechanism)
- `HierarchyDelta`, `MigratedMember`
- `FamilyHierarchy`
- `default_mechanism()` helper

### 2.4 Create `TsAnalysisExtensions`

New file `ts/extensions.rs`:

```rust
pub struct TsAnalysisExtensions {
    pub sd_result: Option<SdPipelineResult>,
    pub hierarchy_deltas: Vec<HierarchyDelta>,
    pub new_hierarchies: HashMap<String, HashMap<String, Vec<ExpectedChild>>>,
}
```

### 2.5 Update `AnalysisReport<L>` and `AnalysisResult<L>`

Replace concrete fields with `extensions: L::AnalysisExtensions`:

```rust
// Remove these from AnalysisReport:
// pub hierarchy_deltas: Vec<HierarchyDelta>,
// pub sd_result: Option<SdPipelineResult>,

// Add:
pub extensions: L::AnalysisExtensions,
```

Same for `AnalysisResult`.

### 2.6 Replace `run_source_diff` with `run_extended_analysis`

On `Language` trait:

```rust
fn run_extended_analysis(
    &self, _repo: &Path, _from_ref: &str, _to_ref: &str,
    _dep_css_dir: Option<&Path>,
) -> Result<Self::AnalysisExtensions> {
    Ok(Self::AnalysisExtensions::default())
}
```

TS overrides to run SD pipeline + hierarchy inference.

### 2.7 Update orchestrator

- `detect_deprecated_replacements` and `apply_deprecated_replacements`
  access `SdPipelineResult` through `extensions.sd_result`.
- Remove `compute_deterministic_hierarchy` from the `HierarchySemantics`
  trait (it's now behind `run_extended_analysis`).

### Files touched

`core/types/sd.rs` (deleted), `core/types/report.rs`, `core/traits.rs`,
`ts/sd_types.rs` (new), `ts/extensions.rs` (new), `ts/hierarchy_types.rs` (new),
`ts/sd_pipeline.rs`, `ts/konveyor_v2.rs`, `ts/report.rs`, `src/orchestrator.rs`,
`src/main.rs`

---

## Phase 3: Trait Method Extraction

**Commit: "refactor: extract language-specific diff logic into LanguageSemantics trait methods"**

### 5 new methods on `LanguageSemantics<M>`

| Method | Replaces | Default | TS override |
|--------|----------|---------|-------------|
| `should_skip_symbol(&Symbol<M>) -> bool` | `is_star_reexport()` in `helpers.rs` | `false` | `sym.name == "*"` |
| `member_label() -> &str` | Hardcoded `"props"` in `migration.rs` | `"members"` | `"props"` |
| `extract_rename_fallback_key(&Symbol<M>) -> Option<String>` | `extract_token_value()` in `rename.rs` (~38 lines) | `None` | Parses `.d.ts` CSS value annotations |
| `canonical_name_for_relocation(&str) -> String` | `canonical_path()` in `relocate.rs` | Identity (return unchanged) | Strips `/deprecated/` and `/next/` |
| `diff_language_data(&Symbol<M>, &Symbol<M>) -> Vec<StructuralChange>` | N/A (enables custom metadata diff) | `vec![]` | Could diff `rendered_components`/`css` |

### Also move or refactor

- `derive_import_subpath()` in `diff/mod.rs` — hardcodes `/deprecated/` and
  `/next/`. Make it call `canonical_name_for_relocation`, or move to TS crate.
- Remove `is_star_reexport()` from `helpers.rs`.
- Remove `extract_token_value()` from `rename.rs`.
- Remove `canonical_path()` from `relocate.rs`.

### NOT changing (already handled)

- `visibility_rank` — already a trait method on `LanguageSemantics`, already
  called via `semantics.visibility_rank()` in `compare.rs`. The
  `helpers::visibility_rank` is only used by `MinimalSemantics` as its default.

### Files touched

`core/traits.rs`, `core/diff/mod.rs`, `core/diff/rename.rs`,
`core/diff/relocate.rs`, `core/diff/helpers.rs`, `ts/language.rs`

---

## Phase 4: Aggressive Report Type Cleanup

**Commit: "refactor: move React-specific report types behind Language associated types, rename ComponentSummary → TypeSummary"**

### 4.1 Activate `L::ReportData`

The `Language` trait already has `type ReportData` (currently a placeholder
with `TsReportData` as an empty struct with comment: "will eventually absorb
ComponentSummary, HierarchyDelta, ContainerChange"). Activate it:

Add `pub language_data: L::ReportData` field to `TypeSummary<L>` (renamed
from `ComponentSummary`).

### 4.2 Move to TS crate (into `TsReportData`)

- `ChildComponent`, `ChildComponentStatus` → `TsReportData.child_components`
- `ExpectedChild` reference → `TsReportData.expected_children`
  (type already moved in Phase 2)

### 4.3 Move `ContainerChange` to TS crate

Accessed through `TsAnalysisExtensions` (it's pipeline data from LLM BU
analysis, not per-component).

### 4.4 Handle remaining fields

- **`renders_element`** on `ApiChange` — keep as `Option<String>` (generic
  enough for any UI framework: Vue, Angular, etc.). Update doc to remove
  React-specific examples.
- **`RemovalDisposition`** — keep in core (variants are abstract:
  `TrulyRemoved`, `ReplacedByMember`, `MovedToRelatedType`, `MadeAutomatic`).
  The `mechanism` field is already a free-form string. Fix React-specific
  comments.

### 4.5 Rename `ComponentSummary` → `TypeSummary`

Clean rename throughout codebase, including serialized JSON field names.
This is an intentional breaking change to JSON output format.

### 4.6 Clean up comments

Remove PatternFly component references and React vocabulary from
`core/types/report.rs` doc comments.

### Files touched

`core/types/report.rs`, `ts/language.rs` (`TsReportData` activated),
`ts/report.rs`, `ts/konveyor.rs`, `ts/konveyor_v2.rs`,
`konveyor-core/src/lib.rs`, `src/orchestrator.rs`, `src/main.rs`

---

## Phase 5: Remaining Genericization TODOs

**Commit: "refactor: address remaining language-specific leaks in core types"**

From `GENERICIZATION_TODOS.md` on the generalize branch:

| ID | Issue | Action |
|----|-------|--------|
| G1 | `ChangedFunction` sentinel strings | `old_body`/`new_body` etc. → `Option<String>` |
| G6 | `ApiChangeKind` missing variants | Add `Enum`, `Constructor` variants |
| G4 | `ContainerChange.description` doc | Remove "from the LLM" |
| G7 | `Visibility` `Default` impl | Consider adding |
| G8/G14 | `TestConvention` TS-centric naming | Consolidate `DotTest`/`DotSpec` → `Suffix` |
| G10/G11 | LLM hardcodes TS categories | Parameterize through `Language` trait |
| G12 | Orchestrator TS file patterns | Use `Language` constants everywhere |
| G13 | `ApiChange.renders_element` | Already handled in Phase 4 |

---

## Phase 6: Java Language Scaffold (Validation)

**Commit: "feat: add Java language support to validate multi-language architecture"**

Port from the generalize branch's `crates/java/` with updates for the
current APIs:

1. **`crates/java/`** — `Java` struct implementing all `Language` trait types
   with `SymbolData = JavaSymbolData`, `AnalysisExtensions = ()`,
   `ReportData = ()`.

2. **`JavaSymbolData`** — annotations, throws, sealed, final, permits.

3. **`JavaExtractor`** — tree-sitter-java based source parsing.

4. **Trait implementations** — `LanguageSemantics<JavaSymbolData>` with:
   - `should_skip_symbol`: skip `package-info`
   - `member_label`: `"methods"`
   - `canonical_name_for_relocation`: simple class name extraction
   - `diff_language_data`: annotation/throws/sealed/final changes

5. **CLI wiring** — `semver-analyzer analyze java` subcommand.

6. **Integration test** with Spring Boot 3.3→3.5.

---

## Execution Order and Dependencies

```
Phase 0  (Verification safety net)
   ↓
Phase 1  (Symbol<M> + Static Dispatch + Hierarchy Move)  ← LARGEST commit
   ↓
Phase 2  (AnalysisExtensions + SD types move)
   ↓
Phase 3  (Trait method extraction)
   ↓
Phase 4  (Report cleanup + TypeSummary rename)
   ↓
Phase 5  (Remaining TODOs)
   ↓
Phase 6  (Java validation)
```

Phases 3 and 4 can theoretically be done in parallel but depend on
Phase 2 for the extensions pattern.

## Per-Phase Verification

After each commit:

```sh
cargo test                              # Full suite (~700+ tests)
cargo clippy -- -D warnings             # No warnings
cargo test -p semver-analyzer-ts --lib  # TS-specific (~589 tests)
```

104 insta snapshots catch semantic drift in diff engine output.
Phase 0 snapshots catch Konveyor rule output drift.
Intentional serialization changes (Phase 4 renames) get documented
in commit messages and snapshots updated.

After Phase 6: run the Spring Boot integration test to validate Java
extraction + diffing works end-to-end.

## Risk Areas

1. **Phase 1 size** — ~2000+ lines changed. `Symbol<M>` touches every
   diff function signature. Must be done atomically due to
   `rendered_components` dependency in `compute_deterministic_hierarchy`.

2. **Serde compatibility** — `Symbol<M>` needs `#[serde(bound)]` for
   correct serialization. Pattern validated by the generalize branch.

3. **Phase 2 orchestrator changes** — `detect_deprecated_replacements`
   and `apply_deprecated_replacements` (888 lines, 15 tests) access
   `SdPipelineResult` directly. Need updates to go through extensions.

4. **Phase 4 serialization break** — `ComponentSummary` → `TypeSummary`
   changes JSON output field names. All downstream consumers must update.

5. **`konveyor-core` crate** — Has patterns assuming CSS/frontend contexts
   but is structurally generic. May need `FixStrategy` variants for other
   languages (Java adds `AnnotationChange` and `ExceptionHandling`).
