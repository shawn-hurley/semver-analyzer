# Migration Plan: TypeScript to Language Trait Architecture

## Goal

Refactor the current TypeScript-specific code into the `Language` trait
architecture described in `design/`, while ensuring that for **all existing
scenarios**, the same input produces semantically identical output.

## Testing Strategy

### Baseline integration tests (Phase 0)

Before any refactoring, create snapshot-based integration tests using the
`insta` crate. These tests capture current behavior at a **semantic level**
that survives the internal type changes.

For each test, we capture a normalized representation:

```rust
#[derive(Debug, Serialize)]
struct NormalizedChange {
    symbol: String,
    qualified_name: String,
    kind: String,
    is_breaking: bool,
    description: String,
    before: Option<String>,
    after: Option<String>,
    has_migration_target: bool,
}
```

This is serialized to YAML by insta. The description string is the most
important field -- if the `MessageFormatter` produces the same description,
the behavior is semantically identical.

**Where the baseline tests live:** `crates/ts/tests/` (integration test
directory for the ts crate). This location can depend on both `core` and
`ts`, and after refactoring validates the full TypeScript-specific pipeline.

**What the baseline tests cover:**

| Area | Source of test cases | Baseline captures |
|------|---------------------|-------------------|
| Diff engine | All 62 cases from `core/diff/tests.rs` | `Vec<NormalizedChange>` snapshots |
| Migration detection | All 9 cases from `core/diff/migration.rs` tests | Migration target snapshots |
| Relocation detection | All 8 cases from `core/diff/relocate.rs` tests | Relocation classification snapshots |
| Manifest diffing | All 23 cases from `ts/manifest/mod.rs` tests | `(field, is_breaking, description)` snapshots |
| JSX diffing | All 16 cases from `ts/jsx_diff/mod.rs` tests | `(category, description)` snapshots |
| CSS scanning | All 6 cases from `ts/css_scan/mod.rs` tests | `(category, description)` snapshots |

Total: ~124 baseline snapshot tests.

### Belt-and-suspenders verification

At each phase boundary:

1. `cargo test` -- all existing 584 unit tests must pass (until intentionally updated)
2. `cargo test -p semver-analyzer-ts` -- baseline integration tests must produce identical snapshots
3. `cargo insta review` -- any snapshot changes require explicit approval

---

## Phase 0: Baseline Integration Tests

**Goal:** Capture current behavior before any changes.

**Steps:**

1. Add `insta` as a dev-dependency to the workspace and `ts` crate
2. Create `crates/ts/tests/baseline_diff.rs` -- port all 62 diff test cases
3. Create `crates/ts/tests/baseline_migration.rs` -- port 9 migration cases
4. Create `crates/ts/tests/baseline_manifest.rs` -- port 23 manifest cases
5. Create `crates/ts/tests/baseline_behavioral.rs` -- port 16 JSX + 6 CSS cases
6. Run `cargo insta review` and accept all snapshots
7. Commit `.snap` files as the golden baseline

**Verification:** `cargo test` passes. All snapshots accepted.
**Output:** ~124 `.snap` files in `crates/ts/tests/snapshots/`

---

## Phase 1: New Types in Core (Additive)

**Goal:** Add new trait definitions and types alongside old ones. No existing code changes.

**Steps:**

1. Add `LanguageSemantics`, `MessageFormatter`, `Language` traits to `core/src/traits.rs`
2. Add `ChangeSubject` enum to new file `core/src/types/change_subject.rs`
3. Add `ReportEnvelope`, `AnalysisSummary`, `ChangeTypeCounts`, `LanguageReport<L>` to new file `core/src/types/envelope.rs`
4. Add `Visibility::Protected` to existing enum
5. Add `SymbolKind::Struct` to existing enum
6. Update `visibility_rank()` and `kind_label()` to handle new variants
7. Add serialization round-trip tests for new types

**Verification:** `cargo test` passes. Baseline snapshots unchanged. Purely additive.

---

## Phase 2: Implement `Language` for TypeScript

**Goal:** Create TypeScript implementation of all traits. Runs in parallel, does not replace existing code.

**Steps:**

1. Create `crates/ts/src/language.rs` with:
   - `pub struct TypeScript;`
   - `TsCategory` enum
   - `TsManifestChangeType` enum
   - `TsEvidence` enum
   - `TsReportData` struct

2. Implement `LanguageSemantics for TypeScript`:
   - `is_member_addition_breaking` -- extract from `compare.rs:672-705`
   - `same_family` -- extract `canonical_component_dir` from orchestrator
   - `same_identity` -- extract `strip_props_suffix` from `migration.rs`
   - `visibility_rank` -- extract from `helpers.rs:107-110`
   - `parse_union_values` -- extract `parse_union_literals` from `compare.rs`
   - `post_process` -- extract `dedup_default_exports` from `diff/mod.rs`

3. Implement `MessageFormatter for TypeScript`:
   - Extract all description-building from `compare.rs` and `helpers.rs`
   - Must produce identical description strings to current code

4. Implement `Language for TypeScript`:
   - Wire up associated types
   - `fn name() -> &'static str { "typescript" }`

5. Unit tests for each trait method

**Verification:** `cargo test` passes. Baseline snapshots unchanged.

---

## Phase 3: Wire Diff Engine to Use Traits

**Goal:** Change `diff_surfaces` to accept `&dyn LanguageSemantics` + `&dyn MessageFormatter`.

**Steps:**

1. Change `diff_surfaces` signature to take trait objects
2. Add backward-compatible `diff_surfaces_ts()` wrapper
3. Replace hardcoded rules in `compare.rs`:
   - Member addition breaking → `semantics.is_member_addition_breaking()`
   - `unwrap_or("void")` → formatter handles
   - `starts_with("Promise<")` → formatter handles
   - `parse_union_literals` → `semantics.parse_union_values()`
   - `unwrap_or("undefined")` → formatter handles
4. Replace in `diff/mod.rs`:
   - `dedup_default_exports` → `semantics.post_process()`
5. Replace in `helpers.rs`:
   - `visibility_rank` → `semantics.visibility_rank()`
   - `kind_label` → remove (formatter handles)
6. Replace in `migration.rs`:
   - `strip_props_suffix` → `semantics.same_identity()`
   - directory-based family → `semantics.same_family()`
7. Add description formatting pass after change collection
8. Update 62 diff tests to use `diff_surfaces_ts` wrapper or pass `&TypeScript`

**Verification:** `cargo test` passes. Baseline snapshot tests identical.

---

## Phase 4: Collapse StructuralChangeType

**Goal:** Replace 37-variant enum with 5 variants + `ChangeSubject`.

**Steps:**

1. Replace `StructuralChangeType` enum (37 → 5 variants)
2. Update `StructuralChange` struct (`kind` from `String` to `SymbolKind`)
3. Update `compare.rs`, `migration.rs`, `relocate.rs`, `rename.rs` to emit new variants
4. Rewrite all 62 tests in `core/diff/tests.rs` for new enum
5. Remove old enum and `to_api_change_type()`

**Verification:** `cargo test` passes (rewritten tests). Baseline snapshot tests produce identical semantic output.

---

## Phase 5: Move TS-Specific Types Out of Core

**Goal:** Core becomes truly language-agnostic.

**Steps:**

1. Move `BehavioralCategory` → `TsCategory` in ts crate (remove from core)
2. Move `ManifestChangeType` → `TsManifestChangeType` (remove from core)
3. Remove `EvidenceSource` from core (replaced by `TsEvidence`)
4. Update `BehaviorAnalyzer::check_propagation` to take `&str` not `&EvidenceSource`
5. Make report types generic: `BehavioralChange<L>`, `ManifestChange<L>`, `FileChanges<L>`, `PackageChanges<L>`
6. Move React-specific report types to ts crate as part of `TsReportData`:
   - `ComponentSummary`, `RemovalDisposition`, `CompositionPatternChange`
   - `ChildComponent`, `ExpectedChild`, `HierarchyDelta`, `FamilyHierarchy`
   - `MigratedProp`, `ConstantGroup`, `SuffixRename`, `AddedComponent`
7. Remove `JsxChange` from `core/types/bu.rs`
8. Implement `ReportEnvelope::from_report::<TypeScript>()`
9. Add envelope serialization tests

**Verification:** `cargo test` passes. Baseline snapshots identical. Core compiles with no TS imports.

---

## Phase 6: Refactor Orchestrator

**Goal:** Generic orchestrator parameterized by `Language`.

**Sub-phases:**

### 6A: Create baseline orchestrator tests
- Test BU pipeline flow with mock data
- Test call graph propagation
- Test report assembly

### 6B: Extract TS-specific functions to ts crate
- `detect_cross_family_context`, `extract_family_from_path`, `read_family_signatures`
- `find_qualifying_families`, `read_family_files`, `infer_and_diff_hierarchies`
- `diff_package_json`, `extract_component_refs`, `parse_behavioral_category`

### 6C: Define `LanguageOrchestrator<L>` trait
```rust
pub trait LanguageOrchestrator<L: Language> {
    fn run_bu_pipeline(...) -> Result<Vec<BehavioralChange<L>>>;
    fn build_report_data(...) -> Result<L::ReportData>;
    fn diff_manifest(...) -> Result<Vec<ManifestChange<L>>>;
}
```

### 6D: Implement `LanguageOrchestrator` for TypeScript

### 6E: Make `run_analysis` generic over `L: Language`

### 6F: Add CLI language dispatch (`--language=typescript`)

**Verification:** `cargo test` passes. Baseline snapshots identical. End-to-end PatternFly test.

---

## Phase 7: Refactor Konveyor Rule Generator

**Goal:** TS-specific rules in ts crate, shared utilities in `konveyor-core`.

**Steps:**

1. Create `crates/konveyor-core/` with shared types and utilities
2. Move full Konveyor generator to ts crate
3. Move 101 Konveyor tests to `crates/ts/tests/`
4. Optionally define generic `RuleGenerator<L>` trait

**Verification:** All 101 tests pass. Generated YAML identical to pre-refactoring.

---

## Phase Summary

| Phase | What changes | Risk | Tests added |
|-------|-------------|------|-------------|
| 0 | Nothing (baseline only) | None | ~124 snapshot tests |
| 1 | New types/traits added | Low | Type serialization tests |
| 2 | TS Language impl | Low | Trait method unit tests |
| 3 | Diff engine uses traits | Medium | Updated diff tests |
| 4 | StructuralChangeType collapsed | Medium | Rewritten diff tests |
| 5 | TS types move out of core | Medium | Envelope tests |
| 6 | Orchestrator refactored | High | Orchestrator integration tests |
| 7 | Konveyor refactored | Medium | Moved Konveyor tests |

## Dependency Order

```
Phase 0 (baseline tests)
    │
    ▼
Phase 1 (new types, additive)
    │
    ▼
Phase 2 (TS Language impl)
    │
    ▼
Phase 3 (diff engine uses traits)
    │
    ▼
Phase 4 (collapse StructuralChangeType)
    │
    ▼
Phase 5 (move TS types out of core)
    │
    ├──────────────────┐
    ▼                  ▼
Phase 6              Phase 7
(orchestrator)       (konveyor)
```

Phases 6 and 7 can proceed in parallel after Phase 5.

Each phase is independently committable and verifiable. If any phase
produces snapshot changes, the refactoring has a behavioral regression
that must be investigated before proceeding.
