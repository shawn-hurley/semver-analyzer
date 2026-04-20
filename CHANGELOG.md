# Changelog

## [0.0.4] — 2026-04-17

### Added

- **Java language support** — Validate the multi-language architecture with a
  Java `Language` impl alongside TypeScript (`27c4e5f`).
- **Deprecated replacement detection** — Two strategies to detect when a
  deprecated component is replaced by another (e.g., `Chip` → `Label`,
  `Tile` → `Card`):
  - Rendering swap: host components stop rendering the deprecated component
    and start rendering the new one (`f2cdaf7`).
  - Commit co-change: fallback that analyzes git commits to find co-changed
    families (`b081a99`).
- **Dead CSS class detection** — Flag CSS classes where a naive version prefix
  swap produces a non-existent class in the new version (`2c002ae`).
- **Peer dependency rules** — Detect and generate rules for new peer
  dependencies in monorepo packages (`2aae11c`).
- **Behavioral change detection** — Worktree sharing, transitive OUIA rules,
  and parallel TD extraction for the BU pipeline (`5207a03`).
- **Composition tree v2 signals** — CSS layout children, BEM orphan fallback,
  dependency-aware building with delegate projection, grid re-parenting
  through `display:contents` mode-switchers (`a32efaa`, `f70f57d`, `2c7b983`).
- **Family migration strategies** — Generate family-level strategies with
  deprecated context, family labels, and shared removed-prop classification
  (`fa771a3`, `6058c9e`).
- **Error reporting overhaul** — `ErrorTip` trait, `Diagnosed` wrapper,
  `DegradationTracker`, and colored CLI output with actionable tips
  (`32f3462`).
- **CLI improvements** — Pipeline default swap (SD is now default),
  comprehensive `--help` documentation (`8cc54b9`).
- **Cross-compilation** — Makefile and guide for cross-compiling to
  linux-amd64 from macOS (`02dc679`).
- **Konveyor YAML snapshots** — Snapshot tests for refactoring safety
  (`72bc2ad`).

### Changed

- **Multi-language genericization** — Major refactor across 6 commits to make
  the core crate fully language-agnostic:
  - Genericize `Symbol<M>` and `ApiSurface<M>`, switch diff engine to static
    dispatch (`9af8816`).
  - Extract language-specific diff logic into `LanguageSemantics` trait
    (`81d53ed`).
  - Rename `ComponentSummary` to `TypeSummary`, move React-specific types
    behind `L::ReportData` (`ed32963`).
  - Add `AnalysisExtensions`, move SD types and deprecated replacement logic
    to TS crate (`425058d`).
  - Wire `ExtendedAnalysisParams`, hierarchy to TS, LLM category
    parameterization (`2ce96b3`).
  - Remove all remaining language-specific code from core crates (`b984e63`).
- **Two-dimensional edge strength model** — `EdgeStrength` now encodes CHP
  (child-must-have-parent) and PMC (parent-must-have-child) independently,
  driving `notParent` and `requiresChild` conformance rules separately
  (`3d4f237`, `f431b66`, `20d9422`).
- **Conformance rule generation** — Eliminated ~45 false `invalidDirectChild`
  rules via three-layer filtering, CHP-only first hop, and CHP suppression
  (`8ac9d22`).

### Fixed

- **Rename detection** — Strip generic type parameters in fingerprint
  normalization so `ReactElement` and `ReactElement<any>` match (`5e392b4`).
- **Type-incompatible renames** — Emit as `Changed` instead of separate
  `Removed` + `Added` to preserve the old→new linkage (`23aec30`).
- **Conformance false positives** — 5 targeted fixes reducing false positives:
  downgrade context/bidirectional CSS edges to Allowed, correct edge strengths,
  merge multi-parent rules, skip internal/back-edges (`e1b3d9a`, `ca29a14`,
  `238ab5c`, `e0612d5`).
- **Composition tree accuracy** — Resolve 23 missing components, improve
  accuracy with 6 targeted fixes, BEM block independence checks (`352a6e8`,
  `46adbee`, `ff153cd`, `2c03723`).
- **Rule precision** — Suppress main `type-changed` rule when per-value
  sub-rules exist, use `PropValueChange` instead of `RemoveProp` for removed
  enum values, filter empty prop-override rules (`593c299`, `f6f27db`,
  `837d642`).
- **cloneElement edge strength** — Use `Wrapper` strength for ReactElement
  children type (ChartDonutThreshold) (`f2aa591`).
- **Wizard recursive nesting** — Downgrade bidirectional CHP cycles to
  `Allowed` (`3a49acd`).
- **Cross-family absorption** — Enrichment for new child components that
  absorb props from intermediate family members (`9fb82e0`).
- **New-sibling rules** — Use `requires_child` discriminator; `new_imports`
  includes all depths and excludes context providers (`9145a23`, `bba4534`).
- **Package version derivation** — Fall back to git tag when `package.json`
  has a placeholder version (`ca1ae4f`).
- **WorktreeGuard** — Canonicalize repo path to fix relative path resolution;
  use corepack for install when `packageManager` field is present (`4ddd438`,
  `125aeea`).
- **Conformance rule IDs** — Deduplicate by including family name and
  shortening segments (`62ae36c`).
- **CSS token handling** — Exclude token-mapped constants from collapsed
  `CssVariablePrefix` groups, add explicit CSS custom property rename
  mappings (`4da1295`, `86173f8`).
- **Deprecated replacement rules** — Mark as LLM-assisted, not codemod
  (`2ba33eb`).
- **Clippy 1.95 compatibility** — Resolve `collapsible_match`,
  `unnecessary_sort_by`, `unnecessary_unwrap` warnings (`31f0725`,
  `d52df3e`).

### CI

- Add `semver-analyzer-java` to release workflow publish and version-bump
  steps (`d3a31d8`).
- Handle already-published crates gracefully and add `workflow_dispatch`
  trigger with `skip_preflight` / `skip_version_bump` for retrying failed
  releases (`b13c12d`).
