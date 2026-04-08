# Semver Analyzer — Agent Guide

## Project Overview

A multi-language semver analysis tool built in Rust. Compares two versions of a
library (via git refs), detects breaking changes, and generates Konveyor
migration rules with fix strategies.

### Architecture

- `crates/core/` — Language-agnostic diff engine, types, traits
- `crates/ts/` — TypeScript/React-specific: source profiles, JSX analysis, CSS
  profiles, konveyor rule generation
- `crates/konveyor-core/` — Konveyor rule types and fix strategy framework
- `crates/llm/` — LLM integration for behavioral analysis
- `src/orchestrator.rs` — Pipeline orchestrator (TD+BU or TD+SD)
- `src/main.rs` — CLI entry point

### Three Pipelines

The analyzer has three pipelines. The `--pipeline-v2` flag controls which
combination runs.

#### TD (Top-Down) — Structural API Diff

**Always runs.** Extracts `.d.ts` API surfaces at both git refs, then diffs
them to detect:

- Renamed, removed, added symbols (constants, interfaces, type-aliases)
- Type changes on properties
- Signature changes (base class, return type)
- Relocations (moved to deprecated/, next/ promoted)
- Member-level renames within interfaces

Key files: `crates/core/src/diff/` (mod.rs, rename.rs, compare.rs, relocate.rs,
migration.rs)

#### BU (Bottom-Up) — Behavioral Analysis (v1 only)

**Runs when `--pipeline-v2` is NOT set.** Walks the git diff bottom-up:

1. Parse changed functions from git diff
2. For each changed function, find associated test files
3. Check test assertion changes → behavioral break
4. If private function has behavioral break → walk UP call graph to public API
5. Optionally runs LLM analysis on changed files for deeper behavioral insights

Key files: `src/orchestrator.rs` (run() method, lines 56–305)

#### SD (Source-Level Diff) — Deterministic Source Analysis (v2 only)

**Runs when `--pipeline-v2` IS set.** Replaces BU with deterministic AST-based
analysis:

1. Extract `ComponentSourceProfile` for each component at both refs
2. Diff profiles to produce `SourceLevelChange` entries:
   - DOM structure, ARIA, role, data attribute changes
   - CSS token usage, prop-style bindings
   - Portal usage, context dependencies
   - Forward ref, memo, composition
   - Prop defaults, children slot path
   - Managed attribute overrides (prop-overrides-attribute)
3. Build composition trees and conformance checks
4. Extract CSS profiles for class/variable removal detection

Key files: `crates/ts/src/source_profile/`, `crates/ts/src/sd_pipeline.rs`,
`crates/ts/src/composition/`

### Pipeline Selection

```sh
# v1: TD + BU (structural + behavioral)
semver-analyzer analyze typescript --repo ... --from v5 --to v6

# v2: TD + SD (structural + source-level) — default for pipeline runs
semver-analyzer analyze typescript --repo ... --from v5 --to v6 --pipeline-v2
```

Both produce an `AnalysisReport` with the same top-level structure. v1 populates
`breaking_behavioral_changes`, v2 populates `sd_result` (source_level_changes,
composition_trees, conformance_checks, etc.).

Rule generation (`konveyor` subcommand) also accepts `--pipeline-v2` to enable
v2-specific rules (composition, conformance, prop-to-child migration, test
impact, CSS removal, prop-attribute-override).

## Key Rules for Agents

### Rename Detection (CRITICAL)

**Before modifying `crates/core/src/diff/rename.rs`**, read:

- `design/rename-detector-verification.md` — Contains the verification dataset
  (15 known-true renames, 28 known-false renames with similarity scores and root
  causes), the verification procedure, and threshold boundaries.
- Run the verification procedure after any change to confirm no regressions.

#### Generic Type Parameter Normalization

The `normalize_type_structure()` function strips generic type parameters from
normalized `_T_` placeholders. This ensures that types like `ReactElement` and
`ReactElement<any>` produce identical normalized fingerprints (`_T_`), enabling
rename detection when the only type difference is a default generic parameter.

Without this, renamed props with trivially-different generic parameters (e.g.,
`labelIcon: ReactElement` → `labelHelp: ReactElement<any>`) fail all four
rename passes and are emitted as separate Removed + Added entries instead of
a single Renamed.

### Source Profile Extraction

Source profiles are extracted in `crates/ts/src/source_profile/`. Submodules:

- `mod.rs` — Main extraction, JSX walking (also detects cloneElement inline
  and JSX elements in parameter/variable destructuring defaults)
- `prop_defaults.rs` — Default value extraction from destructuring
- `prop_style.rs` — Prop-to-CSS-class binding detection
- `managed_attrs.rs` — Prop-overrides-attribute dataflow tracing
- `diff.rs` — Profile diffing to produce SourceLevelChange entries
- `bem.rs` — BEM CSS structure parsing
- `children_slot.rs` — Children wrapper path tracing + CSS token detail
- `clone_element.rs` — cloneElement prop injection detection
- `react_api.rs` — React API usage detection (portal, memo, forwardRef)

### Composition Tree V2 Architecture (CRITICAL)

The v2 composition tree builder (`build_composition_tree_v2` in
`composition/mod.rs`) replaces BEM-based edge creation with evidence-based
signals. **BEM determines family membership only. All parent-child edges come
from structural evidence.**

#### Signal Steps (in order)

| Step | Signal | Strength | Rationale |
|------|--------|----------|-----------|
| 1 | Internal rendering | Required | Component renders the child (JSX body + prop-default JSX) |
| 2 | CSS direct-child `>` | Required* | Styles require exact parent-child DOM |
| 3 | CSS grid parent-child | Required* | Layout breaks without grid container |
| 3b | CSS implicit grid child | Required* | Same — grid layout dependency |
| 4 | CSS flex context | Allowed | Layout preference, not strict |
| 5 | CSS descendant ` ` | Allowed | Works at any depth |
| 5.5 | CSS layout children | Allowed | Shared CSS rule with flex-wrap/gap implies containment |
| 6 | React context | Required | Null context = crash/broken behavior |
| 7 | DOM nesting | Required | Invalid HTML without correct parent |
| 8 | cloneElement | Required | Missing injected props breaks functionality |
| 8.5 | BEM element orphan fallback | Allowed | Orphan BEM elements connected to root as last resort |

*Steps 2, 3, 3b use `Allowed` instead of `Required` when the child component
equals the family root — this indicates recursive/self-nesting (e.g., DataList
inside DataListContent, Menu inside MenuItem) which is optional, not required.

Step 1 detects JSX elements in **parameter destructuring defaults**
(`({ bar = <Bar /> }) => ...`) and **variable destructuring defaults**
(`const { icon = <Icon /> } = this.props`), not just the function body.
This is critical for components like ChartBullet that receive sub-components
as props with JSX defaults.

Step 5.5 consumes `layout_children` from `CssBlockProfile` — pairs of BEM
elements where one is a layout container (has flex-wrap/gap/grid) and the
other is a co-rule sibling. This data was previously computed but never
consumed. It catches intermediate nesting within families (e.g.,
EmptyStateFooter → EmptyStateActions from a shared CSS rule with flex-wrap).

Step 8 has two filters to prevent false edges from shared prop vocabularies:
(1) skip creating A→B if B→A already exists from a prior step (prevents
reverse-of-existing cycles), and (2) remove bidirectional cloneElement pairs
(A→B + B→A both from cloneElement = peers, not hierarchy).

Step 8.5 connects orphan BEM elements to the family root as a last resort.
It fires for family members with zero incoming edges after all structural
signals (Steps 1-8 + 5.5) if the member appears in `css_element_to_component_map`
(has BEM element CSS tokens of the root's block). Three guards prevent false
edges:
1. **Orphan gate**: Only fires for members with zero incoming edges, preventing
   wrong edges for already-connected components in Category 3 families.
2. **CSS element map membership**: Member must have CSS tokens that are BEM
   elements of the root's block (filters out context objects, type exports).
3. **BEM independence**: Member must NOT have its own distinct BEM block
   (prevents false edges for collision families like Label/LabelGroup,
   Menu/MenuToggle, Alert/AlertGroup where camelCase naming creates false
   prefix matches in the CSS element map).

After all steps, members with zero edges (no incoming AND no outgoing) are
dropped from the tree (no "default to root" guessing). Members with outgoing
edges but no incoming edges are retained as **secondary roots** — top-level
containers within the family (e.g., JumpLinksList wraps `<ul>` containing
JumpLinksItem `<li>` children). Non-exported secondary roots are then properly
collapsed by `collapse_internal_nodes`.

#### EdgeStrength: Required vs Allowed

Every edge has a `strength: EdgeStrength` field:

- **Required** — Rendering breaks without this nesting. Generates conformance
  rules (`notParent` checks in the scanner).
- **Allowed** — Valid placement documented in CSS but not the only option. Stays
  in the tree for migration guidance but produces zero conformance rules.

Conformance rule generation (`konveyor_v2.rs` and `sd_pipeline.rs`) filters
edges by `strength == Required` before generating `notParent` rules.

Collapsed edges (from `collapse_internal_nodes`) inherit the **stronger** of
the two edges in the chain.

#### CSS Element → Component Mapping (CRITICAL)

`build_css_element_to_component_map` maps CSS BEM element names to React
components via `css_tokens_used`. **Tokens are stored with the `"styles."`
prefix** (e.g., `"styles.drawerBody"`). The mapping function must strip
`"styles."` before matching against the BEM block name. Skip `"styles.modifiers."`
tokens — they don't map to BEM elements.

#### CSS Profile Loading

CSS profiles are loaded from a dependency repo (e.g., `@patternfly/patternfly`).
The orchestrator uses `WorktreeGuard::create_only` for the dep repo (not
`WorktreeGuard::new`, which requires tsconfig.json). The caller-provided build
command (e.g., `yarn install && npx gulp buildPatternfly`) runs directly in
the worktree.

Multiple CSS files per component directory are all read and merged via
`merge_css_profile`. The old `enrich_trees_with_css` function is no longer
called — CSS enrichment is integrated into the v2 builder.

#### children_slot_detail

A parallel field to `children_slot_path` that captures CSS tokens alongside
tag names. Each entry is `(tag_name, Option<css_token>)`. Used by the flex
context step (step 4) to determine what CSS element wraps `{children}`:

```
children_slot_path:   ["div", "div"]
children_slot_detail: [("div", Some("toolbarContent")), ("div", Some("toolbarContentSection"))]
```

Extraction uses a single AST parse via `trace_children_slot_both` (not separate
parses for path and detail).

#### Known Issues

- **Shared CSS token**: Multiple components render the same CSS class (e.g.,
  `DrawerContentBody` and `DrawerPanelBody` both render `__body`). First
  component registered wins in the mapping. Needs multi-component token map
  with parent-context disambiguation.
- **Non-component exports**: Context objects, type exports may appear as family
  members if they have any structural signal. Need filtering.
- **Prop-based composition**: Components passed via props (e.g., `panelContent`
  on DrawerContent) create collapsed edges that look like children composition.
  The TD pipeline handles these separately.

#### Family Grouping and Deprecated Separation

`extract_family_from_path` in `sd_pipeline.rs` determines which component
family a file belongs to. It looks for the `"components"` path segment and
takes the next segment as the family name. When a modifier directory
(`deprecated/` or `next/`) precedes `"components"`, it is included as a
prefix:

- `src/components/DualListSelector/` → `"DualListSelector"`
- `src/deprecated/components/DualListSelector/` → `"deprecated/DualListSelector"`
- `src/next/components/Foo/` → `"next/Foo"`

The tree's `root` field is set to the family name (not the component name)
after tree construction and collapse, so deprecated families are
distinguishable from main families in the output.

Files from the `code-connect` package (Figma integration) are excluded from
SD file discovery via `should_exclude_from_sd`.

### BEM Block Independence (CRITICAL)

When classifying parent-child relationships via BEM analysis, components with
their own distinct BEM block must be classified as `Independent`, not as
elements of another component's block. There are two code paths that enforce
this:

1. **`classify_bem_relationship()` in `bem.rs`** — Checks `child_block !=
   parent_block` BEFORE token prefix matching. This prevents camelCase naming
   collisions (e.g., `labelGroup` from `label-group` appearing to be element
   `Group` of block `label`).

2. **`infer_ownership_by_name_prefix()` in `composition/mod.rs`** — Uses strict
   block equality (`child_block_lower == root_name_lower`). Only proceeds when
   the child's dominant BEM block is the SAME as the root's name. Rejects all
   cases where they differ, because BEM blocks are stored in camelCase
   (`kebab_to_camel_case` at extraction time), making it impossible to
   distinguish separate blocks from sub-elements by name alone.

**Known collision families** (upstream-verified as independent):
- `label` vs `label-group` (Label / LabelGroup — LabelGroup CONTAINS Labels)
- `alert` vs `alert-group` (Alert / AlertGroup — AlertGroup CONTAINS Alerts)
- `menu` vs `menu-toggle` (Menu / MenuToggle — MenuToggle is a trigger, not a child)
- `form` vs `form-control` (Form / FormControl — separate component)
- `progress` vs `progress-stepper` (Progress / ProgressStepper — unrelated components)

**Never** add a composition edge between components that have different BEM
blocks. If a future PF component genuinely needs cross-block ownership, add
an explicit override rather than weakening the BEM independence checks.

### Composition Rule Generation (CRITICAL)

The rule generator in `konveyor_v2.rs` creates three types of composition rules:

- **`removed-member`** — Fires when a removed component is still used as JSX.
  These are kept and are correct.
- **`requires`** — Removed. Redundant with conformance rules which check the
  same parent-child relationship from the child's perspective (via `notParent`).
  Conformance is more precise because it only fires when the child is misplaced.
- **`new-member`** — Removed. The migration rule (`component-import-deprecated`)
  already lists new child components in its message. New-member rules fired on
  every parent usage regardless of whether the new child was already present.

**Conformance rules are filtered by `EdgeStrength::Required`.** Only edges
where rendering actually breaks (CSS `>` selectors, grid layout, context,
DOM nesting, cloneElement) generate `notParent` conformance rules. Edges from
CSS descendant selectors and flex context (tagged `Allowed`) stay in the tree
for documentation but don't generate scanner rules. This prevents false
positives from CSS descendant selectors that match at any depth.

**Migration rule `when` clauses** (`component-import-deprecated`) use:
- `JSX_PROP` conditions (one per removed prop) for Modified components — only
  fires when a deprecated prop is actually used
- `IMPORT` trigger for fully Removed components — importing a removed component
  is itself the issue
- `child` filter for structural detection — fires when old internal components
  are still used as children

### Type-Incompatible Member Renames (CRITICAL)

When a property is renamed AND its type changes to a structurally different
category (e.g., `splitButtonOptions: SplitButtonOptions` →
`splitButtonItems: ReactNode[]`), the diff engine must emit a **single
`StructuralChangeType::Changed`** entry — NOT separate Removed + Added entries.

- **rename.rs Pass 4** detects these via name similarity (≥0.6 threshold)
- **compare.rs `diff_members()`** separates compatible vs incompatible renames
  using `types_structurally_similar()` (compares `TypeCategory`: Reference vs
  Array vs Object vs Function vs Primitive vs Tuple)
- Type-compatible renames → `StructuralChangeType::Renamed` (mechanical codemod)
- Type-incompatible renames → `StructuralChangeType::Changed` with `before` =
  old signature, `after` = new signature (routes to LLM-assisted fixing via
  `ApiChangeType::SignatureChanged`)

**Never** drop type-incompatible renames back to separate Removed + Added. This
loses the linkage between old and new prop, producing a useless "remove prop,
find an alternative" fix strategy instead of "prop X was replaced by prop Y
with a different type."

Regression test:
`crates/core/src/diff/compare.rs::test_type_incompatible_member_rename_produces_changed_not_removed_plus_added`
— uses real PatternFly `MenuToggleProps` data
(`splitButtonOptions: SplitButtonOptions` → `splitButtonItems: ReactNode[]`).

### Deprecated Replacement Detection

When a component is relocated to `/deprecated/` AND a differently-named
component replaces it (e.g., `Chip` → `Label`), the standard rename detector
cannot find the relationship because:

1. `Label` already exists in both v5 and v6 (never enters the "added" pool)
2. Relocation detection claims `Chip` before rename detection runs
3. "Chip" and "Label" have zero lexical similarity (LCS = 0)

The **deprecated replacement detection** step in `src/orchestrator.rs` solves
this by using **rendering swap** signals from the SD pipeline. After both TD and
SD pipelines complete but before the report is assembled:

1. `detect_deprecated_replacements()` finds relocated components where host
   components stopped rendering the old component and started rendering a new
   one (e.g., ToolbarFilter stopped rendering `Chip`, started rendering `Label`)
2. `apply_deprecated_replacements()` transforms the structural changes:
   - Relocation entries → `Changed` with `before="Chip"`, `after="Label"`
   - Props relocations → `Changed` with `before="ChipProps"`, `after="LabelProps"`
   - Suppresses redundant signature-changed entries (base class change)
   - Preserves non-replaced relocations (Modal, Tile, etc.) unchanged

The detection filters out Fragment, React.Fragment, other relocated components,
and uses a Group-suffix tiebreaker when candidates have equal host evidence.

Key type: `DeprecatedReplacement` in `crates/core/src/types/sd.rs`
Key functions: `detect_deprecated_replacements()`, `apply_deprecated_replacements()`
  in `src/orchestrator.rs`
Tests: `deprecated_replacement_tests` module in `src/orchestrator.rs` (15 tests)

### Konveyor Rules

- `crates/ts/src/konveyor.rs` — v1 rule generation (TD pipeline)
- `crates/ts/src/konveyor_v2.rs` — v2 rule generation (SD pipeline: composition,
  conformance, context, prop-to-child migration, test impact, CSS removal,
  prop-attribute-override)
- `crates/konveyor-core/src/lib.rs` — Shared rule types, fix strategies

### Testing

```sh
cargo test -p semver-analyzer-ts --lib    # ~650 unit tests
cargo test -p semver-analyzer-ts          # + integration tests
cargo test                                # full suite
```

## PatternFly v5 → v6 Reference

The primary test case is PatternFly React v5.4.0 → v6.4.1. Key stats:

- 15,525 total breaking changes
- 340 non-token removals, 4,094 renames (3,995 CSS tokens), 3,866 type changes
- 28 known false-positive renames (see design doc for full details)
- Full change landscape and verification data in
  `design/rename-detector-verification.md`
