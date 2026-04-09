# TypeScript/React Analysis Guide

This guide explains what the semver-analyzer detects when analyzing TypeScript, JavaScript, and React projects, and how to interpret the results.

## What Gets Analyzed

The analyzer examines three aspects of your library:

1. **API surface** (TD pipeline, always runs) -- Extracts `.d.ts` declaration files at both git refs using `tsc`, diffs the public API to find structural breaking changes (removed exports, renamed props, type changes, etc.)

2. **Component source code** (SD pipeline, default) -- Parses component source files at both refs to detect source-level changes that don't appear in the API surface: DOM structure, CSS tokens, React patterns, composition trees, and more.

3. **Package manifest** (always runs) -- Diffs `package.json` for entry point, exports map, peer dependency, and module system changes.

An optional fourth mode, **behavioral analysis** (BU pipeline, via `--behavioral`), uses test-delta heuristics and LLM inference to detect implementation-level behavioral changes.

## Structural Changes (TD Pipeline)

The TD pipeline extracts `.d.ts` declaration files by running `tsc --declaration --emitDeclarationOnly` (with monorepo-aware fallbacks) at both git refs, then diffs the resulting API surfaces.

### What counts as a symbol

| Symbol Kind | TS Constructs |
|-------------|---------------|
| Function | Exported functions |
| Class | React class components, utility classes |
| Interface | Props interfaces (`ButtonProps`, `ModalProps`) |
| Type Alias | Type aliases (`type Variant = 'primary' \| 'secondary'`) |
| Constant | Exported constants, variables, enum values |
| Property | Interface properties, get/set accessors |
| Method | Class methods, constructors |
| Module Export | Namespace/barrel exports |

### Change types

| Change | What it means | Example |
|--------|--------------|---------|
| Removed | Symbol or member deleted from the public API | `Button.isFlat` prop removed |
| Renamed | Symbol detected as renamed via similarity matching | `isOpen` -> `isExpanded` |
| Type Changed | Type annotation changed on a property or return type | `variant: string` -> `variant: 'primary' \| 'danger'` |
| Signature Changed | Interface structure changed (base class, added required member, modifier) | `ButtonProps extends OldBase` -> `ButtonProps extends NewBase` |
| Visibility Changed | Export visibility narrowed | Previously exported symbol no longer exported |
| Relocated | Symbol moved between paths | `Chip` moved to `deprecated/Chip` |

### Rename detection

The analyzer uses a 4-pass rename detection algorithm:

1. **Exact fingerprint match** -- Same type structure, different name
2. **Fingerprint + LCS similarity** -- Similar type structure and name similarity above threshold
3. **Member overlap** -- Interfaces with high member-name overlap
4. **Name similarity only** -- For type-incompatible renames (e.g., `splitButtonOptions: SplitButtonOptions` -> `splitButtonItems: ReactNode[]`)

Type-incompatible renames (where the type changed structurally) are reported as a single "Changed" entry, preserving the linkage between old and new names.

### Relocation and deprecated replacement

When a component is moved to a `/deprecated/` path, the analyzer detects this as a relocation. If the component was also *replaced* by a differently-named component (e.g., `Chip` -> `Label`), the analyzer detects the replacement via **rendering swap signals**: host components that stopped rendering the old component and started rendering the new one.

This works even when the old and new names have zero lexical similarity.

## Source-Level Changes (SD Pipeline)

The SD pipeline performs deterministic, AST-based analysis of component source code. It extracts a `ComponentSourceProfile` at each ref and diffs them to produce source-level change entries.

All SD analysis is fully deterministic -- no LLM or heuristics involved.

### Component Composition

The analyzer builds **composition trees** for multi-component families (e.g., `Table` -> `Thead`/`Tbody` -> `Tr` -> `Td`). These trees represent the expected parent-child nesting relationships.

**How edges are detected** (10 signal steps):

1. Internal rendering -- Component renders the child in its JSX body
2. CSS direct-child selectors (`>`) -- Styles require exact parent-child DOM
3. CSS grid layout -- Grid container/child dependency
4. CSS flex context -- Flex layout preference
5. CSS descendant selectors -- Works at any depth
6. React context -- Provider/consumer dependency
7. DOM nesting -- HTML validity requirements (e.g., `<li>` inside `<ul>`)
8. cloneElement -- Missing injected props breaks functionality

Each edge has a **strength**: `Required` (rendering breaks without this nesting) or `Allowed` (valid placement but not the only option).

**Composition changes detected:**

| Change | Description |
|--------|-------------|
| Prop to child | Props moved from parent to a new child component (e.g., `Modal.title` -> `<ModalHeader>`) |
| Child to prop | Child component absorbed back into parent as a prop |
| Member removed | Component removed from the family |
| Member added | New component added to the family |
| New required child | New intermediate wrapper required between parent and children |

**Conformance rules** are generated from `Required` edges. These detect incorrect nesting in consumer code:

- `notParent` -- Child must be placed inside a specific parent
- `requiresChild` -- Parent must contain specific children
- `invalidDirectChild` -- Child needs an intermediate wrapper

### CSS Token Analysis

The analyzer extracts BEM-structured CSS class and variable usage per component:

- **BEM block/element/modifier** parsing from `styles.*` token references
- **Prop-to-style binding** detection -- when a prop controls which CSS class is applied
- **CSS block removal** -- entire component CSS blocks removed between versions
- **Token change detection** -- CSS tokens added, removed, or reassigned

When a prop still exists but its CSS token was removed, the prop effectively becomes a no-op. The analyzer detects this and flags it.

### React API Patterns

| Pattern | What's detected |
|---------|----------------|
| Portal usage | `createPortal` added or removed. Affects where content renders in the DOM |
| Context dependency | `useContext` dependency added/removed. Components may crash without the correct provider |
| forwardRef | `React.forwardRef()` wrapper added/removed. Ref forwarding starts/stops working |
| memo | `React.memo()` wrapper added/removed. Re-render behavior changes |
| cloneElement | Prop injection via `React.cloneElement` added/removed |
| Rendered components | Internal component rendering changes (starts/stops rendering another component) |

### DOM and Accessibility

| Category | What's detected | Test impact |
|----------|----------------|-------------|
| DOM structure | Root/wrapper elements changed (e.g., `<div>` to `<section>`) | Snapshot tests, DOM selectors break |
| ARIA attributes | `aria-label`, `aria-describedby`, etc. added/removed/changed | `getByLabelText` queries affected |
| Role attributes | `role` attribute changed (e.g., `role="menu"` to `role="listbox"`) | `getByRole` queries must update |
| Data attributes | `data-testid`, `data-ouia-*` attributes changed | `getByTestId` selectors break |

### Prop Analysis

| Category | What's detected |
|----------|----------------|
| Default values | Default value for a prop changed in destructuring pattern. Components behave differently without explicit prop |
| Prop-attribute override | Component internally derives an HTML attribute from a prop via a helper function, potentially overriding consumer-provided values |
| Required props | New required props added to a component |

## Manifest Changes

The analyzer diffs `package.json` to detect package-level breaking changes:

| Change Type | Description |
|-------------|-------------|
| Entry point changed | `main`, `module`, `types`, or `typings` field changed or removed |
| Exports entry removed | Entry removed from the `"exports"` map |
| Exports entry added | New entry in the `"exports"` map |
| Exports condition removed | Condition dropped from an exports entry (e.g., `"require"` removed) |
| Module system changed | `"type"` field changed (CJS to ESM or vice versa) |
| Peer dependency added/removed/changed | Peer dependency version range changed |
| Engine constraint changed | Node.js or npm version requirement changed |
| Bin entry removed | CLI binary entry removed |

## Behavioral Changes (BU Pipeline, opt-in)

When `--behavioral` is set, the analyzer uses a bottom-up approach to detect implementation-level changes:

1. Parses `git diff` to find changed source files
2. Extracts function bodies at both refs
3. Identifies functions whose implementations changed
4. Finds associated test files (7 discovery strategies)
5. If test assertions changed: HIGH confidence behavioral break
6. If LLM enabled: sends diffs for semantic analysis
7. Walks up the call graph for private functions with behavioral breaks

**Behavioral change categories:**

| Category | Description |
|----------|-------------|
| DOM Structure | Changed element types, wrapper elements |
| CSS Class | CSS class name renames or removals |
| CSS Variable | CSS custom property changes |
| Accessibility | ARIA/role/keyboard navigation changes |
| Default Value | Changed default prop/parameter values |
| Logic Change | Changed conditional logic, return values |
| Data Attribute | Changed `data-*` attributes |
| Render Output | General render output change |

Use `--behavioral` when you need LLM-powered analysis of implementation changes that don't show up in the API surface or source profiles. The default SD pipeline covers most use cases deterministically.

## Interpreting the Report

### Key fields to check first

1. **`summary`** -- Total breaking changes, API vs behavioral counts
2. **`changes`** -- Per-file breakdown. Each entry has `breaking_api_changes` (structural) and `breaking_behavioral_changes` (behavioral, if `--behavioral` was used)
3. **`packages`** -- Pre-aggregated per-package view with component summaries. This is what rule generation uses
4. **`sd_result`** -- Source-level changes, composition trees, conformance checks. Only present with the default SD pipeline

### Finding changes for a specific component

The `packages` array contains `type_summaries` with per-component summaries:

```json
{
  "packages": [{
    "name": "@patternfly/react-core",
    "type_summaries": [{
      "name": "Modal",
      "definition_name": "ModalProps",
      "status": "modified",
      "member_summary": {
        "total": 25, "removed": 3, "renamed": 1,
        "type_changed": 2, "added": 1, "removal_ratio": 0.12
      },
      "removed_members": [
        { "name": "title", "old_type": "ReactNode" }
      ]
    }]
  }]
}
```

### SD result structure

The `sd_result` object contains:

- **`source_level_changes`** -- Array of individual source-level changes per component
- **`composition_trees`** -- Parent-child relationship trees for component families
- **`composition_changes`** -- What changed in composition between versions
- **`conformance_checks`** -- Structural validity rules derived from composition trees
- **`removed_css_blocks`** -- CSS component blocks that were entirely removed
- **`deprecated_replacements`** -- Components moved to deprecated with a detected replacement

See [docs/report-format.md](report-format.md) for the complete report schema.

## Known Limitations

### General

- **ESM/CJS declaration deduplication**: Projects that emit both ESM and CJS builds will have roughly doubled symbol counts. The analyzer picks up `.d.ts` from both output directories.
- **Language support**: Only TypeScript/JavaScript is currently supported.

### Structural Analysis

- Rename detection uses lexical similarity thresholds. Renames with zero similarity (e.g., `Chip` -> `Label`) are only detected via the deprecated replacement mechanism.
- Migration target detection requires at least 3 overlapping members and 25% overlap ratio.
- Generic type parameter normalization strips parameters to placeholders, which could mask meaningful generic differences in rare cases.

### Source-Level Analysis

- **Missing composition members**: 23 exported consumer-facing components are missing across 18 families in composition tree detection.
- **Shared CSS tokens**: When multiple components render the same CSS class, only the first component is mapped. This affects ~3 components.
- **Context/type export false positives**: 3 known false positives where context objects appear as family members (`PageContext`, `WizardContext`, `DualListSelectorContext`).
- **Prop-based composition**: Components passed via props (e.g., `panelContent`) create edges that look like children composition.
- **Children slot tracing**: Only captures static JSX paths; dynamic/conditional rendering may be missed.

### What's excluded from analysis

The analyzer skips these files during source-level analysis:

- Test files (`.test.*`, `.spec.*`, `__tests__/`)
- Barrel/index files (`index.ts`, `index.tsx`)
- Build output (`dist/`)
- Demo/example files (`/examples/`, `/demos/`)
- Figma code connect files (`.figma.*`, `/code-connect/`)
- Mock files (`__mocks__/`)

## Tips

- **Build failures**: If `tsc` fails, use `--build-command` to specify a custom build command (e.g., `--build-command "yarn build:esm"`).
- **CSS framework analysis**: Use `--dep-repo` to analyze a CSS dependency repo alongside the main library. This enables CSS profile extraction and CSS removal detection.
- **Monorepos**: The analyzer detects monorepo structures and uses solution tsconfig or project build scripts automatically. Use `--build-command` if the automatic detection doesn't work for your layout.
- **Large reports**: For libraries with thousands of changes (e.g., PatternFly v5 -> v6 has 15,000+), focus on the `packages` array for a structured view rather than the flat `changes` array.
- **PatternFly analysis**: See [docs/patternfly-walkthrough.md](patternfly-walkthrough.md) for a step-by-step guide to analyzing PatternFly React, including Node version requirements, build commands, and CSS dependency setup.
- **LLM integration**: See [docs/llm-integration.md](llm-integration.md) for goose setup and using the behavioral pipeline with LLM analysis.
