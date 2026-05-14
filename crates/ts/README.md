# semver-analyzer-ts

TypeScript/JavaScript language plugin for the semver-analyzer. Implements the `Language` trait from `semver-analyzer-core` and provides complete TypeScript-specific analysis: API surface extraction, type canonicalization, diff parsing, test discovery, JSX/CSS diffing, manifest diffing, Konveyor rule generation, and report building.

Uses the [OXC](https://oxc.rs/) parser (a fast Rust-native JavaScript/TypeScript toolchain) for all AST operations.

## Modules

### `extract` -- API Surface Extraction

`OxcExtractor` extracts the public API surface from `.d.ts` declaration files using a multi-phase approach:

1. **Reachability analysis** -- traces `export * from` re-export graphs from `index.d.ts` entry points
2. **Global namespace scanning** -- discovers `@types/*` global namespace declarations
3. **Import map building** -- collects per-file imports, merges into a global import map
4. **Symbol extraction** -- parses declarations with OXC, extracts functions, classes, interfaces, type aliases, enums, variables, namespaces, and re-exports
5. **Package/import path resolution** -- sets npm package names and subpath entry point provenance

End-to-end flow via `extract_at_ref()`: creates a git worktree, detects the package manager, installs dependencies, runs `tsc --declaration`, then extracts from the generated `.d.ts` files.

### `canon` -- Type Canonicalization

Normalizes type annotation strings so that structurally equivalent types compare as equal. Applies 6 rules on top of `tsc --declaration` output:

1. **Union/Intersection ordering** -- sort members alphabetically, flatten nested
2. **Array syntax** -- `Array<T>` to `T[]`, `ReadonlyArray<T>` to `readonly T[]`
3. **Parenthesization** -- remove unnecessary parens, keep required ones
4. **Whitespace** -- collapse to single spaces, normalize object type formatting
5. **never/unknown absorption** -- `T | never` = `T`, `T & unknown` = `T`, etc.
6. **Import resolution** -- normalize `React.X`, `X`, and `import("react").X` to the same form

Implementation: wraps the type in `type T = ...;`, parses with OXC, walks the AST recursively.

### `diff_parser` -- Changed Function Detection

`TsDiffParser` parses `git diff` between two refs to identify functions whose implementations changed. Filters to source files (skipping `.d.ts`, tests, configs, `dist/`), parses both versions with OXC, extracts all function-like declarations, and compares normalized bodies (stripped of comments and whitespace).

### `call_graph` -- Same-File Call Graph

`TsCallGraphBuilder` detects callers within the same file using OXC. Supports direct calls, method calls (`this.target()`), HOF arguments (`arr.map(target)`), and HOC wrappers (`React.forwardRef`, `React.memo`). Uses whole-word boundary matching to avoid substring false positives.

### `test_analyzer` -- Test Discovery and Assertion Diffing

`TsTestAnalyzer` discovers test files using 7 strategies:

1. Sibling `.test.*` files
2. Sibling `.spec.*` files
3. `__tests__/` directory (exact name match)
4. `__tests__/` subdirectories
5. Parent-level `__tests__/` directory
6. Parent component name inference (e.g., `SliderStep.tsx` finds `Slider.test.tsx`)
7. Directory-level: all test files in the same `__tests__/` directory

Assertion detection supports Jest/Vitest, Mocha/Chai, Node assert, and Testing Library patterns via 36+ regex patterns.

### `jsx_diff` -- Deterministic JSX Diffing

Compares JSX render output between two function versions without LLM. Detects 5 categories:

- **Element tags** (DomStructure) -- added/removed HTML/component elements
- **ARIA attributes** (Accessibility) -- added/removed aria-* attributes
- **Role attributes** (Accessibility) -- changed role values
- **CSS classes** (CssClass) -- AST-aware extraction that skips JS identifiers
- **Data attributes** (DataAttribute) -- changed data-* attributes

### `css_scan` -- CSS Variable/Class Scanning

Deterministic scanner for CSS custom property changes (`--pf-v5-*` to `--pf-v6-*`) and CSS class prefix changes (`pf-v5-c-*` to `pf-v6-c-*`).

### `manifest` -- Package.json Diffing

Deterministic `package.json` diff engine checking 6 areas: entry points (`main`, `module`, `types`), module system (`"type"` field), exports map (subpath entries and conditions), peer dependencies, engine constraints, and bin entries.

### `konveyor` -- Konveyor Rule Generation

Generates [Konveyor](https://www.konveyor.io/) migration rules from analysis reports. Key functions:

- `generate_rules()` -- main rule generation from breaking API and behavioral changes
- `generate_dependency_update_rules()` -- `builtin.json` rules for package dependency detection
- `generate_conformance_rules()` -- rules for expected child component composition patterns
- `write_ruleset_dir()` -- writes `ruleset.yaml` and `breaking-changes.yaml` output

Re-exports all shared types from `semver-analyzer-konveyor-core`.

### `report` -- Report Building

Builds `AnalysisReport<TypeScript>` from raw `AnalysisResult<TypeScript>`. Handles per-file change grouping, component summary aggregation, constant group detection, child component discovery, hierarchy delta enrichment, and package-level change aggregation.

### `worktree` -- Git Worktree Management

`WorktreeGuard` provides RAII git worktree lifecycle management:

1. Validates repo and ref
2. Creates worktree via `git worktree add`
3. Detects package manager (yarn, npm, pnpm) and installs dependencies
4. Runs `tsc --declaration --emitDeclarationOnly` with monorepo-aware fallbacks
5. Cleans up on drop (even on panic)

### `language` -- TypeScript Implementation

The `TypeScript` struct implements four core traits:

- **`Language`** -- binds associated types (`TsCategory`, `TsManifestChangeType`, `TsEvidence`, `TsReportData`) and delegates to sub-modules
- **`LanguageSemantics`** -- TypeScript-specific breaking-change rules (interface member addition semantics, React component family grouping, Props identity matching, union type parsing, Promise async detection)
- **`HierarchySemantics`** -- React component hierarchy analysis
- **`BodyAnalysisSemantics`** -- runs JSX diff and CSS scan on changed function bodies

## Usage

```rust
use semver_analyzer_ts::{TypeScript, OxcExtractor};
use semver_analyzer_core::Language;

let ts = TypeScript::new();

// Extract API surface at a git ref
let surface = ts.extract(repo_path, "v5.0.0")?;

// Find changed functions between two refs
let changed = ts.parse_changed_functions(repo_path, "v5.0.0", "v6.0.0")?;

// Find test files for a source file
let tests = ts.find_tests(repo_path, source_path)?;
```

## Dependencies

| Crate | Purpose |
|-------|---------|
| `semver-analyzer-core` | Shared traits and types |
| `semver-analyzer-konveyor-core` | Shared Konveyor rule types |
| `oxc_parser`, `oxc_ast`, `oxc_allocator`, `oxc_span` | Rust-native JS/TS parser |
| `serde`, `serde_json`, `serde_yaml` | Serialization |
| `clap` | CLI argument parsing |
| `anyhow`, `thiserror` | Error handling |
| `regex` | Test assertion detection and pattern matching |
| `chrono` | Timestamps |
| `tracing` | Structured logging |

## License

Apache-2.0
