# Konveyor Migration Rules

## Overview

The `konveyor` command generates [Konveyor](https://www.konveyor.io/)-compatible YAML rules from a breaking change analysis. These rules detect migration issues in consumer codebases -- applications that depend on the library being analyzed.

**Typical workflow:**

1. Run `semver-analyzer analyze` to produce a report comparing two library versions
2. Run `semver-analyzer konveyor` to generate migration rules from the report
3. Run the rules against a consumer codebase using [kantra](https://github.com/konveyor/kantra) or the Konveyor frontend-analyzer-provider

Rules cover structural API changes (removed/renamed symbols, type changes), source-level changes (component nesting, CSS tokens, React API patterns), and optionally behavioral changes (via LLM analysis).

## Quick Start

```bash
# Step 1: Analyze the library for breaking changes
semver-analyzer analyze typescript \
  --repo /path/to/library \
  --from v5.0.0 --to v6.0.0 \
  -o report.json

# Step 2: Generate Konveyor rules
semver-analyzer konveyor typescript \
  --from-report report.json \
  --output-dir ./rules

# Step 3: Run rules against a consumer application
kantra analyze \
  --rules ./rules \
  --input /path/to/consumer-app
```

For AST-level conditions (`frontend.referenced`, `frontend.cssclass`, etc.), kantra requires a frontend-analyzer-provider gRPC server. See the [kantra documentation](https://github.com/konveyor/kantra) for setup details.

You can also combine steps 1 and 2 by using `--repo` mode:

```bash
semver-analyzer konveyor typescript \
  --repo /path/to/library \
  --from v5.0.0 --to v6.0.0 \
  --output-dir ./rules
```

## Command Reference

### From a report (recommended for iteration)

```bash
semver-analyzer konveyor typescript \
  --from-report report.json \
  --output-dir ./rules
```

This is the preferred mode when iterating on rule quality. You run analysis once and regenerate rules as needed.

### Inline analysis

```bash
semver-analyzer konveyor typescript \
  --repo /path/to/repo \
  --from v5.0.0 --to v6.0.0 \
  --output-dir ./rules
```

Runs the full analysis pipeline internally, then generates rules. Accepts the same pipeline, LLM, build, and dependency repo flags as `analyze typescript`. Run `semver-analyzer konveyor typescript --help` for the full list.

### Key flags

| Flag | Description |
|------|-------------|
| `--from-report <path>` | Load a pre-existing analysis report (mutually exclusive with `--repo`) |
| `--repo <path>` | Path to git repository (runs full analysis) |
| `--from <ref>` | Old git ref |
| `--to <ref>` | New git ref |
| `--output-dir <path>` | Output directory for the generated ruleset |
| **Rule Generation** | |
| `--rename-patterns <path>` | YAML file with custom rename patterns (see [Customization](#customization)) |
| `--no-consolidate` | Keep one rule per declaration change (see [Rule Consolidation](#rule-consolidation)) |
| `--file-pattern <glob>` | File glob for filecontent rules (default: `*.{ts,tsx,js,jsx,mjs,cjs}`) |
| `--ruleset-name <name>` | Name for the generated ruleset (default: `semver-breaking-changes`) |

## Output Structure

```
output-dir/
├── ruleset.yaml              # Ruleset metadata
└── breaking-changes.yaml     # All migration rules

output-dir/../fix-guidance/
├── fix-guidance.yaml         # Per-rule migration guidance
└── fix-strategies.json       # Machine-readable fix strategies (keyed by rule ID)
```

### `ruleset.yaml`

Metadata consumed by kantra to identify the ruleset:

```yaml
name: semver-breaking-changes
description: "Breaking changes detected between v5.0.0 and v6.0.0 by semver-analyzer v0.0.4"
labels:
  - source=semver-analyzer
```

### `breaking-changes.yaml`

Array of migration rules. Each rule has an ID, conditions, description, and migration guidance. See [Rule Anatomy](#rule-anatomy) for the full format.

### `fix-strategies.json`

Machine-readable fix strategies keyed by rule ID. Used by the frontend-analyzer-provider's fix engine to apply automated fixes. Not needed for detection-only workflows.

```json
{
  "semver-button-variant-removed": {
    "strategy": "RemoveProp",
    "component": "Button",
    "prop": "variant"
  }
}
```

### `fix-guidance.yaml`

Human-readable migration guidance with summary statistics:

```yaml
migration:
  from_ref: v5.0.0
  to_ref: v6.0.0
  generated_by: semver-analyzer
summary:
  total_fixes: 142
  auto_fixable: 87
  needs_review: 31
  manual_only: 24
fixes:
  - rule_id: semver-button-variant-removed
    confidence: high
    source: pattern
    kind: find_alternative
    description: "Remove the 'variant' prop from Button"
```

## Rule Categories

Rules are generated from three pipelines. The TD pipeline always runs. By default, the SD pipeline also runs; use `--behavioral` to substitute the BU pipeline.

### Structural Rules (TD Pipeline)

Generated from the API surface diff. Detect direct API-level breaking changes.

| `change-type` | What it detects | Example trigger |
|---|---|---|
| `removed` | Symbol removed (prop, constant, interface) | `Button.isFlat` removed |
| `renamed` | Symbol renamed | `isOpen` -> `isExpanded` |
| `type-changed` | Type annotation changed | `variant: 'primary' \| 'secondary'` -> `variant: 'primary' \| 'danger'` |
| `signature-changed` | Interface base class or return type changed | `ButtonProps extends OldBase` -> `ButtonProps extends NewBase` |
| `visibility-changed` | Export visibility narrowed | Public -> protected |
| `prop-value-change` | Specific union value removed from prop type | `variant="tertiary"` no longer valid |
| `component-removal` | Component fully removed or heavily restructured | `Chip` removed (replaced by `Label`) |
| `new-sibling-component` | New required child component added | `EmptyStateActions` added to `EmptyState` |
| `css-class` | CSS class prefix renamed | `pf-v5-c-button` -> `pf-v6-c-button` |
| `css-variable` | CSS variable prefix/suffix renamed | `--pf-v5-global--Color` -> `--pf-t--global--color` |
| `dependency-update` | npm package needs version bump | `@patternfly/react-core` needs `^6.0.0` |
| `manifest` | package.json structural changes | Scripts, configs, exports map changed |

### Source-Level Rules (SD Pipeline, default)

Generated from deterministic AST-based analysis. Detect component-level migration issues.

| `change-type` | What it detects | Example trigger |
|---|---|---|
| `conformance` | Incorrect component nesting | `<Table><Td>` without intermediate `<Tr>` |
| `composition` | Family member removed | `DataListControl` no longer exists |
| `deprecated-migration` | Component moved to/from `/deprecated` | `Chip` moved to `deprecated/Chip`, replaced by `Label` |
| `prop-to-child` | Prop moved from parent to new child component | `Modal.header` prop replaced by `<ModalHeader>` child |
| `child-to-prop` | Child component replaced by parent prop | `<EmptyStateIcon>` child replaced by `icon` prop on parent |
| `context-dependency` | React context provider/consumer changed | `AccordionContext` structure changed |
| `prop-value-removed` | Specific string value removed from prop type | `Alert severity="info"` no longer valid |
| `required-prop-added` | New required prop on a component | `Wizard` now requires `step` prop |
| `test-impact` | Testing Library queries affected | `role="button"` changed to `role="link"` |
| `css-removal` | Entire CSS component block removed | `.pf-c-chip` block removed |
| `prop-attribute-override` | Component internally manages HTML attribute from prop | `aria-label` derived from `label` prop |
| `composition-inversion` | Internal subcomponent replaced by render prop | `<PanelHeader>` replaced by `header` render prop |

### Behavioral Rules (BU Pipeline, opt-in)

Generated when `--behavioral` is set. Require LLM analysis.

| `change-type` | What it detects | Example trigger |
|---|---|---|
| `behavioral` | Implementation behavior changed | Function returns different values for same inputs |

## Rule Anatomy

Each rule in `breaking-changes.yaml` follows this structure:

```yaml
- ruleID: semver-button-variant-type-changed
  labels:
    - "source=semver-analyzer"
    - "change-type=type-changed"
    - "kind=property"
    - "has-codemod=false"
    - "package=@patternfly/react-core"
  effort: 3
  category: mandatory
  description: "Type of 'variant' changed on Button"
  message: |
    The type of `variant` on `Button` changed from
    `'primary' | 'secondary' | 'tertiary'` to
    `'primary' | 'secondary' | 'danger'`.

    The value `tertiary` is no longer valid.
    Review usages and update to a supported value.
  links:
    - url: https://github.com/patternfly/patternfly-react/releases/tag/v6.0.0
      title: Release notes
  when:
    frontend.referenced:
      pattern: "^variant$"
      location: JSX_PROP
      component: "^Button$"
```

| Field | Description |
|-------|-------------|
| `ruleID` | Unique identifier. Used as a key in fix-strategies.json |
| `labels` | Metadata for filtering. See [Labels Reference](#labels-reference) |
| `effort` | Estimated migration effort (1-10). Higher = more work |
| `category` | `mandatory` (must fix) or `potential` (may need review) |
| `description` | One-line summary |
| `message` | Detailed migration guidance. May include code examples |
| `links` | Related documentation URLs |
| `when` | Condition that triggers the rule. See [Condition Types](#condition-types) |

## Condition Types

Rules use different condition types depending on what they detect:

| YAML Key | What it matches | Used by |
|----------|----------------|---------|
| `frontend.referenced` | AST-level symbol matching (imports, JSX, types) | Most rules |
| `frontend.cssclass` | CSS class name patterns in stylesheets | CSS class prefix rules, CSS removal |
| `frontend.cssvar` | CSS custom property patterns | CSS variable prefix/suffix rules |
| `frontend.dependency` | package.json dependency name + version | Dependency update rules |
| `builtin.filecontent` | Regex match in file contents | Fallback for large constant groups |
| `or: [...]` | Any sub-condition matches | Multi-location rules |
| `and: [...]` | All sub-conditions match | Missing import detection |

### `frontend.referenced` locations

The `location` field controls where in the AST to match:

| Location | Matches | Example |
|----------|---------|---------|
| `IMPORT` | Import statement | `import { Button } from '...'` |
| `JSX_COMPONENT` | JSX element usage | `<Button>` |
| `JSX_PROP` | JSX prop on a component | `<Button variant="primary">` |
| `FUNCTION_CALL` | Function call expression | `getByRole('button')` |
| `TYPE_REFERENCE` | TypeScript type reference | `const x: ButtonProps = ...` |

### `frontend.referenced` filters

Conditions can be scoped with additional filters:

| Filter | Purpose | Example |
|--------|---------|---------|
| `component` | Scope JSX_PROP to a specific component (regex) | `"^Button$"` |
| `parent` | Require JSX_COMPONENT to be inside this parent | `"^Tbody$"` |
| `notParent` | Require JSX_COMPONENT to NOT be inside this parent | `"^(Tbody\|Thead)$"` |
| `child` | Require parent to contain this child | `"^DataListControl$"` |
| `notChild` | Require parent to NOT contain non-matching children | Exclusive wrapper validation |
| `requiresChild` | Require parent to contain at least one of these children | `"^(Tab)$"` |
| `value` | Match a specific prop value (regex) | `"^tertiary$"` |
| `from` | Scope by import source package (regex) | `"@patternfly/react-core"` |
| `filePattern` | File path filter (regex) | `"\\.(test\|spec)\\."` |

## Fix Strategies

Fix strategies describe how to resolve each rule violation. They are written to `fix-strategies.json` and consumed by the frontend-analyzer-provider's fix engine.

| Strategy | Description | Automated? |
|----------|-------------|------------|
| `Rename` | Find-and-replace: old name -> new name | Yes |
| `RemoveProp` | Remove a prop from a component | Yes |
| `CssVariablePrefix` | Replace CSS variable/class prefix | Yes |
| `ImportPathChange` | Update import path | Yes |
| `PropValueChange` | Change a prop's value | Partial |
| `PropTypeChange` | Prop type changed, needs update | Partial |
| `UpdateDependency` | Update package.json version | Yes |
| `PropToChild` | Prop moved to child component | Review needed |
| `ChildToProp` | Child component replaced by prop | Review needed |
| `CompositionChange` | Component nesting changed | Review needed |
| `DeprecatedMigration` | Component moved between deprecated/main paths | Review needed |
| `LlmAssisted` | Complex restructuring, needs LLM or manual review | No |
| `Manual` | No automated fix available | No |

Each strategy may include additional fields like `from`/`to` (for renames), `component`/`prop` (for prop operations), `mappings` (for batch renames), and `replacement` (for migrations).

## Customization

### Rename Patterns File

The `--rename-patterns` flag accepts a YAML file that supplements algorithmic detection with explicit rules. This is useful for renames that the analyzer can't detect automatically (e.g., naming conventions with no lexical similarity) or for adding domain-specific rules.

#### `rename_patterns` -- Regex symbol renames

Apply regex find-and-replace to removed symbol names. Capture groups are supported.

```yaml
rename_patterns:
  - match: "^c_(.+)_PaddingTop$"
    replace: "c_${1}_PaddingBlockStart"
```

#### `composition_rules` -- Parent-child nesting

Add custom composition rules for component nesting validation.

```yaml
composition_rules:
  - child_pattern: "^Icon$"
    parent: "^Button$"
    category: mandatory     # mandatory | potential (default: mandatory)
    description: "Icon must be wrapped in Button"
    effort: 2               # default: 2
    package: "@patternfly/react-core"  # optional scope
```

#### `prop_renames` -- Explicit prop renames

Declare prop renames that aren't detected by the diff engine.

```yaml
prop_renames:
  - old_prop: isOpen
    new_prop: isExpanded
    components: "^(Accordion|Dropdown)$"  # regex
    package: "@patternfly/react-core"
    description: "isOpen renamed to isExpanded"
```

#### `value_reviews` -- Flag specific prop values

Flag specific prop values for manual review when their semantics changed.

```yaml
value_reviews:
  - prop: variant
    component: "^Button$"
    value: "^tertiary$"
    package: "@patternfly/react-core"
    category: potential
    description: "Button variant 'tertiary' removed"
    effort: 1
```

#### `missing_imports` -- Co-requisite import detection

Detect when a required co-import is missing.

```yaml
missing_imports:
  - has_pattern: "import.*from '@patternfly/react-core'"
    missing_pattern: "import.*createIcon"
    file_pattern: "\\.(ts|tsx|js|jsx)$"
    category: mandatory
    description: "Missing createIcon import alongside react-core"
    effort: 1
```

#### `component_warnings` -- DOM/CSS rendering changes

Flag components whose internal rendering changed without an API surface change. These won't be caught by structural analysis but may affect visual output or tests.

```yaml
component_warnings:
  - pattern: "^TextArea$"
    package: "@patternfly/react-core"
    category: potential
    description: "TextArea internal DOM restructured -- test visual output"
    effort: 1
```

#### `token_mappings` -- Explicit constant renames

Override algorithmic constant rename detection with explicit mappings.

```yaml
token_mappings:
  global_success_color_100: "t_global_color_status_success_100"
  global_Color_dark_100: "t_global_text_color_regular"
```

#### `css_var_renames` -- CSS custom property renames

Explicit CSS custom property rename rules.

```yaml
css_var_renames:
  - from: "--pf-v5-global--BackgroundColor--100"
    to: "--pf-t--global--background--color--100"
```

#### Complete example

```yaml
rename_patterns:
  - match: "^c_(.+)_PaddingTop$"
    replace: "c_${1}_PaddingBlockStart"
  - match: "^global_(.+)_Color_100$"
    replace: "t_global_${1}_color_100"

prop_renames:
  - old_prop: isOpen
    new_prop: isExpanded
    components: "^(Accordion|Dropdown|Select)$"

value_reviews:
  - prop: variant
    component: "^Button$"
    value: "^tertiary$"
    category: potential
    description: "tertiary variant removed, use 'secondary'"

component_warnings:
  - pattern: "^TextArea$"
    category: potential
    description: "TextArea DOM restructured"

token_mappings:
  global_success_color_100: "t_global_color_status_success_100"

css_var_renames:
  - from: "--pf-v5-global--BackgroundColor--100"
    to: "--pf-t--global--background--color--100"
```

### Rule Consolidation

By default, rules are consolidated: related rules targeting the same file, kind, and change type are merged into a single rule with a combined message and `or`-combined conditions. This reduces rule count significantly (from thousands to hundreds for large libraries).

Use `--no-consolidate` to keep one rule per declaration change. This is useful for:

- Debugging which specific changes generated which rules
- Generating per-change fix strategies (each rule maps to exactly one fix)
- Integration with tools that expect fine-grained rules

Consolidation also runs post-processing passes:

- **Redundant prop suppression**: Removes individual prop rules when a broader component migration rule covers the same component
- **Redundant value suppression**: Removes prop value rules when a type-changed rule covers the same prop
- **Duplicate condition merging**: Merges rules with identical `when` clauses

## Labels Reference

Every rule carries labels for filtering and categorization.

| Label | Values | Description |
|-------|--------|-------------|
| `source` | `semver-analyzer` | Provenance -- all rules carry this |
| `change-type` | See [Rule Categories](#rule-categories) | Type of breaking change |
| `kind` | `property`, `function`, `method`, `class`, `interface`, `type-alias`, `constant`, `module-export` | API symbol kind |
| `has-codemod` | `true`, `false` | Whether a deterministic code transformation is possible |
| `package` | npm package name | Package scope (e.g., `@patternfly/react-core`) |
| `family` | Component family name | Component family (conformance/composition rules) |
| `target-component` | Component name | Target component for prop migrations |
| `target-package` | Package name | Target package for deprecated migrations |
| `impact` | `frontend-testing`, `visual-regression` | What area is affected |
| `change-scope` | `additive` | Non-breaking additive change |
| `ai-generated` | *(present or absent)* | Rule produced from LLM analysis |

Labels can be used to filter rules with kantra's `--label-selector` flag:

```bash
# Only mandatory CSS changes
kantra analyze --rules ./rules \
  --label-selector "change-type=css-class || change-type=css-variable"

# Only rules with automated fixes
kantra analyze --rules ./rules \
  --label-selector "has-codemod=true"
```
