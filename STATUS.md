# Semver Analyzer + Frontend Analyzer Provider: Current Status

**Date:** 2026-03-17
**Session:** kantra rule comparison, fix strategy pipeline, end-to-end verification

---

## What Was Done

### 1. Kantra Rule Comparison (HC vs Auto-Generated)

Ran kantra with hand-crafted rules (164 rules from `frontend-analyzer-provider/rules/patternfly-v5-to-v6/`) and auto-generated rules against the quipucords-ui repo (v2.1.0 baseline at commit `3b3ce52`).

**Final detection results:**
- Auto-generated: 2,551 rules, 77 violations, 294 incidents
- Hand-crafted: 164 rules, 30 violations, 127 incidents
- **Coverage of HC by auto: 126/127 (99.2%)**
- 1 gap: `pfv6-prop-rename-toolbar-align` at a TS object literal (provider limitation)

### 2. Rule Generator Improvements (src/konveyor/mod.rs)

All changes are generic, not PatternFly-specific:

| Fix | Description |
|---|---|
| P0-A | Class/Interface removals emit `Or(JSX_COMPONENT, IMPORT)` |
| P0-B | PascalCase constants excluded from token consolidation |
| P0-C | Synthesize IMPORT rules for interfaces with significant prop removals |
| P0-C' | `*Props` interface removal also matches component name (strip Props suffix) |
| P1-A | Suppressed — prop-level rules provide better specificity |
| P2-A | `composition_rules` config (parent constraint) |
| P3-A | `prop_renames` config |
| P4-A | Fixed `extract_value_filter` for union types |
| P4-B | `api_change_to_rules` returns `Vec<KonveyorRule>` for multi-value |
| P4-C | `value_reviews` config |
| P5 | `and`/`not` combinators + `missing_imports` config |
| Fix 1a | Catch-all arm: PascalCase constants get `Or(JSX_COMPONENT, IMPORT)` |
| Fix 1b | TypeAlias arm: `Or(TYPE_REFERENCE, IMPORT)` |
| Fix 2 | Behavioral rules use component name not leaf method for dotted symbols |
| Fix 3 | `component_warnings` config |
| FP fix 1 | Deprecated path: anchored `from` regex + IMPORT-only for subpath-scoped rules |
| FP fix 2 | Filter out behavioral changes from test/demo/integration source files |
| Approach B | Suppress `component-review` rules (noisy, prop-level rules are better) |

### 3. Fix Strategy Pipeline Redesign

**Problem:** The old `generate_fix_strategies` used index-based matching between report entries and the rules array. This was fundamentally broken because `api_change_to_rules` returns `Vec` (variable rules per change), behavioral changes are filtered, and synthetic rules are appended after iteration.

**Solution:** Each `KonveyorRule` now carries its own `fix_strategy: Option<FixStrategyEntry>` field, set at creation time. During consolidation, `merge_rule_group` merges strategies by collecting mappings of the highest-priority type. After consolidation, `extract_fix_strategies(&rules)` produces the final JSON.

**Deleted:**
- `generate_fix_strategies()` — entire index-based function (~90 lines)
- `merge_fix_strategies()` in main.rs
- `strategy_priority()` in main.rs (moved to konveyor/mod.rs)
- All re-keying logic in main.rs (~50 lines)
- 8 merge tests from main.rs

**Result:** 77/77 strategy match rate (was 26/77 with old approach)

### 4. Fix Engine Integration (frontend-analyzer-provider)

Added `--strategies <FILE>` flag to the `fix` subcommand:
- `crates/core/src/fix.rs`: Added `StrategyEntry`, `MappingEntry`, `load_strategies_from_json()`, `CssVariablePrefix` variant
- `src/fix_engine.rs`: `plan_fixes()` accepts `external_strategies` parameter, lookup order: external → hardcoded → label inference → LLM
- `src/cli/fix.rs`: `--strategies` flag, loads JSON and passes to `plan_fixes()`

**Fix results with strategies:**
```
Without --strategies:  0 pattern fixes, 212 LLM-assisted
With --strategies:    50 pattern fixes, 51 edits, 25 files, 0 skipped
```

### 5. End-to-End Pipeline Script

Created `/Users/shurley/repos/ai_harness/run-pipeline.sh` with 6 steps:
- `build` → `setup` → `analyze` → `rules` → `kantra` → `fix`
- Each step uses well-known paths under `$WORK_DIR`
- Per-step invocation: `./run-pipeline.sh rules kantra fix`
- `--strategies` flag auto-passed to fix engine

### 6. Comparison vs Hand-Done Migration (v2.2.0 release)

The PF6 migration was done in commit `6a8b6a7` touching 52 files.

```
                        Hand-Done    Auto Pipeline
Files changed:          52           30
Perfect matches:        -            5 files (identical)
Near-perfect:           -            3 files (2-5 lines diff)
Partially correct:      -            16 files
Over-changed (goose):   -            5 files
Build-breaking misses:  0            4 files
Test snapshots:         16           0 (run jest --updateSnapshot)
```

---

## What's Left (Follow-On Tasks)

### Build-Breaking Misses (4 files)

| File | Issue | Root Cause | Fix |
|---|---|---|---|
| `aboutModal.tsx` | `TextContent`/`TextList`/`TextListItem` → `Content` | **Detected** (12 incidents). Multi-symbol rename — goose was given the file but didn't rename all symbols | Improve goose prompt to include rename mappings from strategy |
| `contextIcon.tsx` | react-tokens color imports removed, Icon wrapper needed | **Detected** (3 incidents). Complex structural change | Goose needs better context about the structural migration pattern |
| `usePaginationPropHelpers.ts` | `alignRight` → `alignEnd` in TS object literal | **Detected**. Provider can't match `JSX_PROP` in plain TS objects | Provider enhancement: match typed object literals |
| `app.css` (via `app.tsx`) | Missing `utilities/_index.css` import | **Detected** (missing-import rule). Strategy is `Manual` | Add `AddImport` fix strategy type to the fix engine |

### Fix Engine Improvements

1. **AddImport strategy** — for missing-import rules, the fix is adding a new import line, not renaming. Needs a new `FixStrategy::AddImport { import_line: String }` variant.

2. **CSS prefix fix for `.css` files** — the `CssVariablePrefix` strategy works via `plan_rename` fallback (substring match on incident line), but the fix engine needs to scan the full file for all occurrences of the old prefix, not just the incident line.

3. **Multi-mapping consolidation for `Rename` rules** — the `mappings` field carries all rename pairs through consolidation, but the fix engine's `to_fix_strategy()` for `Rename` already handles this. The remaining gap is that some consolidated rules have a mix of Rename + RemoveProp, and only the highest-priority type survives. Consider splitting consolidated rules by strategy type.

4. **Goose prompt improvement** — pass the fix strategy's `from`/`to` mappings into the goose prompt so it knows exactly what to rename, not just "this file has violations."

### Provider Improvements

1. **TS object literal matching** — the `frontend-analyzer-provider` matches `JSX_PROP` only in JSX elements. Props set on plain TypeScript objects typed with a `*Props` interface are invisible. This causes the `usePaginationPropHelpers.ts` miss.

2. **`from` field matching** — the provider does unanchored regex matching on `from`. For deprecated-path rules, we use `^@pkg/deprecated$` anchoring. However, JSX_COMPONENT/JSX_PROP/TYPE_REFERENCE incidents don't carry a `module` variable, so `from` filtering is bypassed for non-IMPORT locations. We handle this by restricting deprecated-path rules to IMPORT-only.

### Pipeline Improvements

1. **Post-fix test run** — add `jest --updateSnapshot` step after LLM fixes to auto-regenerate 16 snapshot files.
2. **Re-analysis loop** — the pipeline does pattern fix → re-analyze → LLM fix. Could add another re-analyze after LLM fix to measure remaining incidents.
3. **Diff report** — generate a summary comparing auto-fix output against a reference branch (like the v2.2.0 comparison above).

---

## Key File Locations

### semver-analyzer
- `src/konveyor/mod.rs` — rule generation, consolidation, fix strategies
- `src/main.rs` — CLI, strategy extraction
- `hack/integration/patternfly-rename-patterns.yaml` — config (composition, prop renames, value reviews, component warnings, missing imports)

### frontend-analyzer-provider
- `crates/core/src/fix.rs` — FixStrategy types, StrategyEntry deserialization, load_strategies_from_json
- `src/fix_engine.rs` — plan_fixes (external strategies), apply_fixes
- `src/cli/fix.rs` — --strategies flag

### Pipeline
- `/Users/shurley/repos/ai_harness/run-pipeline.sh` — end-to-end script
- Work dir: `/tmp/semver-pipeline/`
  - `analysis/patternfly-report.json` — analysis output
  - `rules/breaking-changes.yaml` — konveyor rules
  - `fix-guidance/fix-strategies.json` — fix strategies (2,551 entries)
  - `kantra/output/output.yaml` — kantra violations
  - `kantra/quipucords-ui-fixed/` — auto-fixed codebase

### Test Data
- Quipucords v2.1.0 baseline: commit `3b3ce52`
- Quipucords v2.2.0 (hand-done migration): commit `5ea3785`
- PF6 migration commit: `6a8b6a7`
- PatternFly range: v5.4.0 → v6.1.0
