---
marp: true
theme: default
paginate: true
backgroundColor: #fff
style: |
  section {
    font-family: 'Segoe UI', system-ui, -apple-system, sans-serif;
    font-size: 28px;
  }
  h1 {
    color: #151515;
    font-weight: 700;
  }
  h2 {
    color: #151515;
    border-bottom: 3px solid #0066cc;
    padding-bottom: 0.2em;
    display: inline-block;
  }
  h3 {
    color: #0066cc;
  }
  table {
    font-size: 0.78em;
  }
  code {
    background: #f0f0f0;
    padding: 0.1em 0.3em;
    border-radius: 3px;
    font-size: 0.9em;
  }
  .columns {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 1.5em;
  }
  .big-number {
    font-size: 3.5em;
    font-weight: 800;
    line-height: 1.1;
  }
  .blue { color: #0066cc; }
  .red { color: #c9190b; }
  .green { color: #3e8635; }
  .orange { color: #f0ab00; }
  .muted { color: #6a6e73; font-size: 0.85em; }
  .stat-row {
    display: flex;
    justify-content: space-around;
    margin-top: 1em;
    text-align: center;
  }
  .stat-box {
    background: #f0f0f0;
    border-radius: 12px;
    padding: 1em 1.5em;
  }
  .callout {
    background: linear-gradient(135deg, #e0f0ff, #ece7f7);
    border-left: 4px solid #0066cc;
    padding: 0.5em 1em;
    border-radius: 0 8px 8px 0;
    font-size: 0.8em;
    margin-top: 0.4em;
  }
  section.compact {
    font-size: 24px;
  }
  section.compact table {
    font-size: 0.82em;
  }
  section.compact h1 {
    margin-bottom: 0.2em;
  }
  section.compact .columns {
    gap: 1em;
  }
  ul { margin: 0.3em 0; }
  li { margin-bottom: 0.15em; }
---

<!-- _class: lead -->
<!-- _paginate: false -->
<!-- _backgroundColor: #151515 -->
<!-- _color: #fff -->

# semver-analyzer

### Deterministic Breaking Change Analysis for TypeScript

<br>

PatternFly React v5 &rarr; v6 | Validation Results | Roadmap

<span class="muted" style="color:#aaa;">March 2026</span>

---

# The Problem

Existing approaches send raw diffs to an LLM and ask "what breaks?"

This fails at scale:

| Limitation | Impact |
|---|---|
| **Non-deterministic** | Same diff produces different results across runs |
| **No type awareness** | Can't distinguish widening (safe) from narrowing (breaking) |
| **No dependency graph** | Can't tell you *what code* is affected by a break |
| **Context-limited** | Large repos exceed LLM context windows |
| **No transitive analysis** | If A breaks and B uses A, B isn't flagged |

<div class="callout">

We need static analysis for the deterministic parts, LLM only for the genuinely hard part: "did the *behavior* change in a breaking way?"

</div>

---

# Project Goals

1. **Agent-agnostic** -- standalone CLI; can be invoked by Goose, OpenCode, or any agent
2. **Language-agnostic architecture** -- pluggable per-language analyzers via Rust traits (TypeScript first)
3. **Deterministic structural analysis** -- static API extraction and diffing, no LLM
4. **LLM-assisted behavioral analysis** -- LLM only for body-changed-but-signature-same cases
5. **Impact analysis** -- for each breaking change, report what code depends on it

### The gap this fills

No existing open-source tool combines static API surface extraction, structural diff with type compatibility, dependency graph, and LLM behavioral analysis. The closest tools are Rust-only (`cargo-semver-checks`) or proprietary (Qodo).

---

# Architecture: Full Pipeline

```
              semver-analyzer                  Upstream analysis
         (git-based source analysis)           Rust CLI, deterministic
                    |
                    v
          Structured JSON Report               172 changes detected
       (143 API + 29 behavioral for PF)        for PatternFly v5→v6
                    |
         ┌──────────┴──────────────┐
         v                         v
  semver-analyzer            frontend-analyzer-provider
  konveyor command            (hand-crafted rules)
  (auto-generated)            (curated fix engine)
         |                         |
         v                         v
  ┌────────────────┐       ┌────────────────┐
  │ konveyor-rules/ │       │ patternfly-    │
  │ + fix-guidance/ │       │ v5-to-v6/      │
  └────────────────┘       └────────────────┘
         |                         |
         └────────────┬────────────┘
                      v
           Scan & Fix User Projects
```

---

<!-- _class: compact -->

# Architecture: TD Pipeline (Structural)

<div class="columns">
<div>

**Extraction (per ref)**
1. Create git worktree
2. Detect package manager, install deps
3. `tsc --declaration` with fallbacks:
   - Solution tsconfig (`tsc --build`)
   - Project build (`yarn build`)
   - Custom `--build-command`
4. Parse `.d.ts` files with OXC
5. Canonicalize types (6 rules)

</div>
<div>

**Diffing (4 phases)**
1. Exact qualified name match
2. Relocation / deprecated detection
3. Fingerprint + LCS rename detection
4. Unmatched &rarr; removed / added

**30+ change categories:** removed exports, signature changes, type narrows, visibility, generics, class hierarchy, enum members, manifest breaks

</div>
</div>

<div class="callout">

PatternFly: extracted **56,539 symbols** across 8 monorepo packages (24.6x improvement over naive tsc).

</div>

---

<!-- _class: compact -->

# Architecture: BU Pipeline (Behavioral)

<div class="columns">
<div>

**Function-level analysis**
1. Parse `git diff` for changed source files
2. Extract function bodies at both refs (OXC)
3. Compare normalized bodies
4. Cross-ref with TD via `DashMap` + broadcast (skip duplicates)

**Test discovery (7 strategies)**
- Sibling `.test.*` / `.spec.*` / `__tests__/`
- Parent, component, and directory level

</div>
<div>

**Behavioral break detection**
- Test assertions changed &rarr; **HIGH** confidence
- No test changes + LLM &rarr; send diff to LLM
- Private function breaks &rarr; walk up call graph

**LLM integration**
- Any CLI tool (Goose, OpenCode, etc.)
- Cost circuit breaker (`--max-llm-cost`)
- 5 concurrent analyses, template-constrained prompts

</div>
</div>

<div class="callout">

TD and BU run concurrently via `tokio::join!`. TD broadcasts structural breaks so BU skips redundant analysis.

</div>

---

<!-- _class: compact -->

# Architecture: Crate Structure

<div class="columns">
<div>

```
semver-analyzer (binary)
  src/main.rs           CLI, report building
  src/orchestrator.rs   Concurrent TD/BU
  src/cli/mod.rs        Clap definitions
  src/konveyor/mod.rs   Konveyor rule + fix gen
```

```
crates/core/            Language-agnostic
  traits.rs             LanguageSupport trait
  shared.rs             DashMap + broadcast
  diff/                 4-phase differ
  types/                ApiSurface, Report
```

</div>
<div>

```
crates/ts/              TypeScript support
  extract/              OXC .d.ts extraction
  canon/                Type canonicalization
  diff_parser/          Git diff parsing
  test_analyzer/        Test discovery
  call_graph/           Caller detection
  manifest/             package.json diff
  worktree/             Git, tsc, pkg mgr
```

```
crates/llm/             Behavioral analysis
  invoke.rs             LLM command exec
  prompts.rs            Prompt templates
  spec_compare.rs       Spec comparison
```

</div>
</div>

---

<!-- _class: compact -->

# Konveyor Rule Generation

The `konveyor` command auto-generates Konveyor rules and fix guidance from any `AnalysisReport`.

```bash
# From a pre-existing report
semver-analyzer konveyor --from-report report.json --output-dir ./rules

# Or run analysis + generate in one shot
semver-analyzer konveyor --repo ./my-lib --from v1.0.0 --to v2.0.0 \
  --output-dir ./rules --no-llm
```

Output (two sibling directories):

```
./rules/                        ./fix-guidance/
  ruleset.yaml                    fix-guidance.yaml
  breaking-changes.yaml
```

Each breaking change maps to a Konveyor rule (`builtin.filecontent` or `builtin.json`) and a fix guidance entry with strategy, confidence, before/after, and suggested replacement.

---

<!-- _class: compact -->

# Fix Guidance: Strategy Mapping

Each breaking change deterministically maps to a fix strategy and confidence level:

| Change Type | Strategy | Confidence | Auto-fixable? |
|---|---|---|:---:|
| Renamed | `rename` | Exact | Yes |
| Signature changed | `update_signature` | High | Yes |
| Type changed | `update_type` | High | Yes |
| Removed | `find_alternative` | Low | No (manual) |
| Visibility reduced | `find_alternative` | Medium | No |
| Behavioral | `manual_review` | Medium | No (LLM) |
| CJS&rarr;ESM | `update_import` | High | Yes |
| Peer dep changed | `update_dependency` | High | Yes |

<div class="callout">

Rename changes are **Exact confidence** -- safe for mechanical find-and-replace. Behavioral changes are flagged as `ai-generated` and require manual review.

</div>

---

<!-- _class: compact -->

# Comparison: Auto-Generated vs Hand-Crafted Rules

<div class="columns">
<div>

### semver-analyzer `konveyor`
(auto-generated from report)

- Rules generated **automatically** from any analysis
- Uses `builtin.filecontent` (regex) -- no custom provider needed
- Fix guidance in separate YAML with strategy + confidence
- Works for **any** library version diff, not just PatternFly
- Trade-off: regex patterns may have false positives

</div>
<div>

### frontend-analyzer-provider
(hand-crafted for PF v5&rarr;v6)

- Rules **manually written** per migration
- Custom `frontend.referenced` provider -- AST-level matching
- Fix engine with **pattern + LLM** two-phase fixing
- 60+ rule-to-strategy mappings in Rust code
- Applies fixes directly to files
- Trade-off: only works for PatternFly, requires maintenance

</div>
</div>

<div class="callout">

The auto-generated rules are a **fast path** -- generate rules for any library in seconds. The hand-crafted provider is the **deep path** -- precise AST analysis with direct fix application. Both feed into the Konveyor ecosystem.

</div>

---

<!-- _backgroundColor: #0066cc -->
<!-- _color: #fff -->

# Output Comparison
### semver-analyzer vs pf-codemods

PatternFly React v5.4.0 &rarr; v6.0.0

---

# Coverage at a Glance

<div class="stat-row">
<div class="stat-box">
  <div class="big-number blue">172</div>
  <div class="muted">Reference breaking changes</div>
</div>
<div class="stat-box">
  <div class="big-number green">6</div>
  <div class="muted">Accepted misses</div>
</div>
<div class="stat-box">
  <div class="big-number orange">83</div>
  <div class="muted">AI-only extra findings</div>
</div>
</div>

<br>

<div class="callout">

**Zero false negatives in scope.** All 6 misses are intentional -- internal components, private properties, attribution differences. The pipeline finds **83 additional breaking changes** that have no codemod rule.

</div>

<div class="muted" style="margin-top:0.5em;">

Reference: 143 API changes + 29 behavioral changes across 88 files

</div>

---

<!-- _class: compact -->

# Accepted Misses (6)

All intentional divergences -- the pipeline is correct in each case.

| Component | Disposition | Reason |
|---|---|---|
| `ToolbarChipGroupContent` | **Internal** | Not exported from barrel. Public changes under Toolbar/ToolbarUtils |
| `Toolbar.chipContainerRef` | **Private** | Private class property, not a public prop |
| `PageHeaderToolsItem.isSelected` | **Subsumed** | Entire file deleted in v6; prop removal invisible |
| `EmptyState` | **Attribution** | Reports sub-component removals instead of parent signature change |
| `SliderStep` | **Attribution** | CSS var change attributed to parent `Slider` after rollup |
| `Banner.color` | **Match order** | Change exists but consumed by competing match |

<div class="callout">

In every case, the underlying change **is** captured -- just attributed differently or correctly excluded as non-public API.

</div>

---

<!-- _class: compact -->

# Extra Findings (83) -- Category Breakdown

Changes found by the pipeline with **no corresponding codemod rule**:

| Category | Count | Compiler? | Risk |
|---|:---:|:---:|---|
| **CSS Class Changes** | 14 | No | Silent visual regressions |
| **DOM Structure** | 12 | No | Breaks snapshots, selectors, E2E |
| **Accessibility** | 12 | No | ARIA/role -- highest severity |
| **Interface / Type Removals** | 10 | Yes | Build errors |
| **Module Exports** | 5 | Yes | Import errors |
| **Inherited Prop Renames** | 5 | Yes | Build errors |
| **Prop Type Narrows** | 5 | Yes | Build errors |
| **Other** | 5 | Mixed | Varies |
| **Icon Migration** | 4 | No | Mostly transparent |
| **Behavioral** | 3 | No | Focus/hover changes |
| **Prop Removals** | 3 | Yes | Build errors |

---

<!-- _class: compact -->

# The Silent Regression Gap

Our pipeline found **41 breaking changes** that pf-codemods does not cover and `tsc` will not catch.

<div class="columns">
<div>

| Category | Found |
|---|:---:|
| CSS Classes | 14 |
| DOM Structure | 12 |
| Accessibility | 12 |
| Icon Migration | 4 |
| Behavioral | 3 |
| **Total** | **41** |

</div>
<div>

### Without the pipeline, these ship silently

`tsc` compiles &rarr; no lint warnings &rarr; CI green &rarr; deploy

**Users** discover them as:
- Visual regressions from CSS/DOM changes
- Accessibility audit failures
- Broken E2E / snapshot tests

</div>
</div>

<div class="callout">

**Our pipeline is the only tool that surfaces these.** No codemod, no compiler error -- highest-risk migration gap.

</div>

---

<!-- _class: compact -->

# What We Found: Accessibility & Behavioral

Changes caught by the pipeline -- **no codemod, no build error**.

<div class="columns">
<div>

### Accessibility (12)

| Component | Change |
|---|---|
| `ClipboardCopyButton` | `aria-labelledby` removed |
| `NavItemSeparator` | role: `separator` &rarr; `presentation` |
| `FileUploadField` | `aria-describedby` removed |
| `ExpandableSectionToggle` | `aria-hidden` removed from icon |
| `NumberInput` | screen reader traversal changed |
| `ProgressContainer` | `role="progressbar"` moved |

</div>
<div>

### Behavioral (3)

| Component | Change |
|---|---|
| `Menu` | hover persists during keyboard nav |
| `MenuContainer` | no auto-focus on toggle |
| `ClipboardCopy` | array joined with space, not `""` |

No type checking or linting catches a removed `aria-labelledby` or a changed focus pattern. These affect **compliance**.

</div>
</div>

---

<!-- _class: compact -->

# What We Found: CSS & DOM Structure

Changes caught by the pipeline -- **no codemod, no build error**.

<div class="columns">
<div>

### CSS Classes (14)

| Component | Change |
|---|---|
| `NavExpandable` | modifier dropped from `<li>` |
| `NumberInput` | validated state modifier removed |
| `Title` | default size &rarr; `undefined` |
| `DataListCheck` | `<Checkbox>` replaces `<input>` |
| `CalendarMonth` | hardcoded class names |

</div>
<div>

### DOM Structure (12)

| Component | Change |
|---|---|
| `ExpandableSectionToggle` | new wrapper `<div>` |
| `Truncate` / `MenuItem` | tooltip: wrapper &rarr; sibling |
| `NavList` | scroll buttons in new `<div>` |
| `Hint` | actions div now conditional |
| `MenuGroup` | label div now conditional |

</div>
</div>

These break snapshot tests, custom CSS overrides, and E2E selectors -- discovered as visual regressions **after deployment**.

---

<!-- _backgroundColor: #151515 -->
<!-- _color: #fff -->

# Where To Go From Here

---

<!-- _class: compact -->

# Roadmap Skeleton

<div class="columns">
<div>

### Near-Term
- **ESM/CJS dedup** -- ~57k &rarr; ~28k symbols
- **Suppress tsc noise** -- collapse warnings on fallback success
- **MCP server** -- `serve` subcommand for agent integration
- **CI integration** -- auto-analyze on tag push

### Medium-Term
- **Incremental analysis** -- cache surfaces, re-extract changed packages only
- **Cross-package impact** -- `workspace:*` dependency tracking
- ~~**Rule generation** -- auto-generate Konveyor YAML from output~~ **Done**

</div>
<div>

### Long-Term
- **Additional languages** -- Rust, Python, Java
- **Patch oracles** -- executable specs to *prove* behavioral diffs
- **Config/schema** -- OpenAPI, protobuf, GraphQL
- **Visual regression bridge** -- DOM/CSS findings &rarr; snapshot tooling

### Open Questions
- Does `api-extractor` `.api.json` replace `.d.ts` parsing?
- What LLM confidence threshold for behavioral breaks?
- How practical are executable tests for behavioral proof?

</div>
</div>
