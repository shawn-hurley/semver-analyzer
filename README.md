# semver-analyzer

Deterministic, structured analysis of semantic versioning breaking changes between two git refs. Extracts API surfaces, diffs them, performs source-level analysis, and generates [Konveyor](https://www.konveyor.io/) migration rules with fix strategies.

Currently supports TypeScript/JavaScript/React projects.

## Quick Start

```bash
# Build
cargo build --release

# Analyze breaking changes between two tags
semver-analyzer analyze typescript \
  --repo /path/to/your-ts-project \
  --from v1.0.0 \
  --to v2.0.0 \
  -o report.json

# Generate Konveyor migration rules from the report
semver-analyzer konveyor typescript \
  --from-report report.json \
  --output-dir ./rules
```

A convenience script is provided for running against [PatternFly](https://github.com/patternfly/patternfly-react), the primary validation target. See [docs/patternfly-walkthrough.md](docs/patternfly-walkthrough.md) for the full setup guide.

```bash
hack/run-patternfly.sh
```

## Prerequisites

- **Rust** (stable toolchain) -- build the analyzer
- **Node.js >= 18** and **npm/yarn/pnpm** -- required by target projects for `tsc` and dependency installation
- **Git** -- worktree creation and diff parsing
- **TypeScript** (`tsc`) -- installed as a dev dependency in the target project, or globally

## Installation

```bash
git clone <repo-url>
cd semver-analyzer
cargo build --release

# Binary is at target/release/semver-analyzer
# Optionally add to PATH:
export PATH="$PWD/target/release:$PATH"
```

## Commands

### `analyze typescript` -- Full Pipeline

Runs the complete analysis: extract API surfaces at both refs, diff them, perform source-level analysis, and diff `package.json` manifests.

```bash
semver-analyzer analyze typescript \
  --repo /path/to/repo \
  --from v5.0.0 \
  --to v6.0.0 \
  -o report.json
```

| Option | Description |
|--------|-------------|
| `--repo <path>` | Path to local git repository |
| `--from <ref>` | Old git ref (tag, branch, SHA) |
| `--to <ref>` | New git ref (tag, branch, SHA) |
| `-o, --output <path>` | Output file (JSON). Defaults to stdout |
| **Pipeline** | |
| `--behavioral` | Use the behavioral analysis (BU) pipeline instead of the default source-level diff (SD). See [Pipelines](#how-it-works) |
| **LLM Options** | |
| `--no-llm` | Skip LLM-based behavioral analysis (static only) |
| `--llm-command <cmd>` | Command to invoke for LLM analysis (see [LLM Integration](#llm-integration)) |
| `--llm-timeout <secs>` | Timeout per LLM invocation (default: 120) |
| `--llm-all-files` | Send all changed files to LLM, not just those with test changes. Requires `--behavioral` |
| **Build** | |
| `--build-command <cmd>` | Custom build command. If not set, the analyzer detects the package manager and runs tsc with monorepo-aware fallbacks |
| **Dependency Repo** | |
| `--dep-repo <path>` | Path to a dependency git repo (e.g., a CSS framework repo). Enables CSS profile extraction |
| `--dep-from <ref>` | Old git ref for the dependency repo |
| `--dep-to <ref>` | New git ref for the dependency repo |
| `--dep-build-command <cmd>` | Build command for the dependency repo |

### `konveyor typescript` -- Generate Migration Rules

Generates [Konveyor](https://www.konveyor.io/)-compatible YAML rules from breaking change analysis. Rules can be consumed by [kantra](https://github.com/konveyor/kantra) or the Konveyor frontend analyzer to detect migration issues in consumer codebases. See [docs/konveyor-rules.md](docs/konveyor-rules.md) for detailed documentation on rule types, conditions, fix strategies, and customization.

**Two modes:**

1. **From a report** (recommended for iteration):
   ```bash
   semver-analyzer konveyor typescript \
     --from-report report.json \
     --output-dir ./rules
   ```

2. **Inline analysis** (runs the full pipeline then generates rules):
   ```bash
   semver-analyzer konveyor typescript \
     --repo /path/to/repo \
     --from v5.0.0 --to v6.0.0 \
     --output-dir ./rules
   ```

| Option | Description |
|--------|-------------|
| `--from-report <path>` | Load a pre-existing analysis report (mutually exclusive with `--repo`) |
| `--repo <path>` | Path to git repository (runs full analysis) |
| `--from <ref>` | Old git ref |
| `--to <ref>` | New git ref |
| `--output-dir <path>` | Output directory for the generated ruleset |
| **Rule Generation** | |
| `--rename-patterns <path>` | YAML file with regex-based rename patterns |
| `--no-consolidate` | Keep one rule per declaration change (disable merging) |
| `--file-pattern <glob>` | File glob for filecontent rules (default: `*.{ts,tsx,js,jsx,mjs,cjs}`) |
| `--ruleset-name <name>` | Name for the generated ruleset (default: `semver-breaking-changes`) |

The `konveyor` command also accepts `--behavioral`, LLM, build, and dependency repo flags when running in inline analysis mode (`--repo`). Run `semver-analyzer konveyor typescript --help` for the full list.

**Output structure:**

```
rules/
├── ruleset.yaml              # Ruleset metadata
└── breaking-changes.yaml     # Migration rules
```

**Example rule:**

```yaml
- ruleID: component-prop-removed-button-variant
  labels:
    - "source=semver-analyzer"
    - "change-type=prop-removed"
  effort: 3
  category: mandatory
  description: "Property 'variant' was removed from Button"
  message: |
    The `variant` prop was removed from `Button`.
    Remove this prop or migrate to the replacement API.
  when:
    frontend.referenced:
      pattern: "^variant$"
      location: JSX_PROP
      component: "^Button$"
```

### `extract typescript` -- Extract API Surface

Extracts the public API surface at a single git ref. Useful for inspecting or caching surfaces.

```bash
semver-analyzer extract typescript \
  --repo /path/to/repo \
  --ref v5.0.0 \
  -o surface.json
```

| Option | Description |
|--------|-------------|
| `--repo <path>` | Path to local git repository |
| `--ref <ref>` | Git ref to extract from |
| `-o, --output <path>` | Output file (JSON). Defaults to stdout |
| `--build-command <cmd>` | Custom build command |

### `diff` -- Compare Two Surfaces

Compares two previously extracted API surface JSON files. This command is language-agnostic.

```bash
semver-analyzer diff \
  --from old-surface.json \
  --to new-surface.json \
  -o changes.json
```

## How It Works

The analyzer combines two pipelines. The **TD** (structural) pipeline always runs. By default, the **SD** (source-level) pipeline runs alongside it. Optionally, the **BU** (behavioral) pipeline can be used instead via `--behavioral`.

### TD (Top-Down) Pipeline -- Structural Analysis

Always runs. Extracts and diffs the public API surface:

1. Creates git worktrees for each ref
2. Detects the package manager (npm/yarn/pnpm) and installs dependencies
3. Runs `tsc --declaration --emitDeclarationOnly` with monorepo-aware fallbacks:
   - Solution tsconfig detection (`tsc --build`)
   - Project build script fallback (`yarn build`)
   - Custom `--build-command` override
4. Parses generated `.d.ts` files with [OXC](https://oxc.rs/)
5. Builds the `ApiSurface` with type canonicalization (union/intersection sorting, `Array<T>` normalization, whitespace, `never/unknown` absorption, import resolution)
6. Diffs old vs new surface with 4-phase matching: exact name, relocation/deprecated detection, fingerprint+LCS rename detection, unmatched
7. Detects 30+ categories of structural changes (removed exports, signature changes, type changes, visibility, generics, class hierarchy, enum members, etc.)
8. Diffs `package.json` for manifest-level breaks (entry points, module system, exports map, peer deps, engines, bins)

### SD (Source-Level Diff) Pipeline -- Source Analysis (default)

Runs by default alongside TD. Performs deterministic, AST-based analysis of source code changes between refs:

- **Component composition trees** -- Builds parent-child relationship trees for component families using 10 evidence-based signals (internal rendering, CSS selectors, React context, DOM nesting, cloneElement). Generates conformance rules that detect incorrect component nesting in consumer code.
- **CSS token analysis** -- Extracts BEM-structured CSS class/variable usage per component. Detects removed CSS classes, renamed variables, and layout-affecting changes (grid, flex context).
- **React API changes** -- Tracks portal usage, forwardRef/memo wrapping, context dependencies, and cloneElement injection patterns across versions.
- **Prop defaults and bindings** -- Extracts default values from destructuring patterns and detects prop-to-CSS-class binding changes.
- **DOM structure** -- Compares rendered element trees, ARIA attributes, roles, and data attributes.
- **Deprecated replacement detection** -- When a component is relocated to `/deprecated/` and replaced by a differently-named component (e.g., `Chip` -> `Label`), detects the replacement via rendering swap signals.

The SD pipeline produces fully deterministic results -- no LLM or heuristics involved.

### BU (Bottom-Up) Pipeline -- Behavioral Analysis (opt-in)

Opt-in via `--behavioral`. Replaces the SD pipeline with test-delta heuristics and optional LLM inference:

1. Parses `git diff` to find changed source files
2. Extracts function bodies at both refs using OXC
3. Identifies functions whose implementations changed
4. Cross-references with TD findings to avoid duplicates (via `DashMap` + broadcast channel)
5. Discovers associated test files (7 strategies covering common project layouts)
6. If test assertions changed: HIGH confidence behavioral break
7. If LLM enabled: sends diffs to an external LLM for semantic analysis
8. Walks up the call graph for private functions with behavioral breaks

### Output

The report is a JSON document. Key top-level fields:

```json
{
  "repository": "/path/to/repo",
  "comparison": {
    "from_ref": "v5.0.0",
    "to_ref": "v6.0.0",
    "from_sha": "abc123",
    "to_sha": "def456",
    "commit_count": 142,
    "analysis_timestamp": "2026-03-16T12:00:00Z"
  },
  "summary": {
    "total_breaking_changes": 1523,
    "breaking_api_changes": 1500,
    "breaking_behavioral_changes": 23,
    "files_with_breaking_changes": 87
  },
  "changes": [
    {
      "file": "packages/react-core/src/components/Card/Card.d.ts",
      "status": "modified",
      "breaking_api_changes": [
        {
          "symbol": "CardProps.isFlat",
          "kind": "property",
          "change": "removed",
          "before": "isFlat?: boolean",
          "after": null,
          "description": "Property 'isFlat' was removed from CardProps"
        }
      ]
    }
  ],
  "packages": [ "..." ],
  "sd_result": { "..." },
  "manifest_changes": [],
  "metadata": { "tool_version": "0.0.4" }
}
```

The `packages` field contains a per-package hierarchical view used by rule generation. The `sd_result` field (populated by the SD pipeline) contains source-level changes, composition trees, and conformance checks.

## LLM Integration

The analyzer can optionally use any CLI-accessible LLM for behavioral analysis. LLM analysis is only used with the `--behavioral` pipeline -- the default SD pipeline is fully deterministic and requires no LLM.

```bash
# Using goose with the behavioral pipeline
semver-analyzer analyze typescript \
  --repo /path/to/repo \
  --from v5.0.0 --to v6.0.0 \
  --behavioral \
  --llm-command "goose run --no-session -q -t"

# Using any command that accepts a prompt as its last argument
semver-analyzer analyze typescript \
  --repo /path/to/repo \
  --from v5.0.0 --to v6.0.0 \
  --behavioral \
  --llm-command "my-llm-cli"
```

See [docs/llm-integration.md](docs/llm-integration.md) for detailed setup instructions, goose installation, the CLI contract for custom providers, and cost considerations.

## Architecture

```
semver-analyzer (binary)
├── src/main.rs              # CLI entry, report building
├── src/orchestrator.rs      # Pipeline orchestrator (TD+SD or TD+BU)
└── src/cli/mod.rs           # Clap CLI definitions

crates/
├── core/                    # Language-agnostic types and diff engine
│   └── src/
│       ├── traits.rs        # Pluggable language support trait
│       ├── shared.rs        # SharedFindings (DashMap + broadcast)
│       ├── diff/            # 6-phase API surface differ
│       └── types/           # ApiSurface, Symbol, AnalysisReport
├── ts/                      # TypeScript/JavaScript support
│   └── src/
│       ├── extract/         # OXC-based .d.ts API extraction
│       ├── canon/           # 6-rule type canonicalization
│       ├── source_profile/  # Component source profile extraction
│       ├── composition/     # Composition tree builder (v2)
│       ├── sd_pipeline.rs   # Source-level diff pipeline
│       ├── diff_parser/     # Git diff -> changed functions
│       ├── test_analyzer/   # Test discovery + assertion detection
│       ├── call_graph/      # Same-file caller detection
│       ├── jsx_diff/        # Deterministic JSX render diffing
│       ├── css_scan/        # CSS variable/class prefix scanning
│       ├── manifest/        # package.json diff
│       ├── konveyor.rs      # Konveyor rule generation (TD pipeline)
│       ├── konveyor_v2.rs   # Konveyor rule generation (SD pipeline)
│       └── worktree/        # Git worktree lifecycle, tsc, pkg mgr
├── konveyor-core/           # Shared Konveyor rule types and utilities
│   └── src/lib.rs           # Rule construction, consolidation, fix strategies
└── llm/                     # LLM behavioral analysis
    └── src/
        ├── invoke.rs        # External LLM command execution
        ├── prompts.rs       # Structured prompt templates
        └── spec_compare.rs  # Structural spec comparison
```

The core crate defines a `Language` trait, making the architecture language-pluggable. TypeScript is the first (and currently only) implementation.

## Development

```bash
# Run all tests
cargo test

# Run tests for a specific crate
cargo test -p semver-analyzer-core
cargo test -p semver-analyzer-ts

# Build in debug mode
cargo build

# Build release
cargo build --release
```

## Documentation

| Guide | Description |
|-------|-------------|
| [TypeScript/React Guide](docs/typescript-guide.md) | What the analyzer detects, how to interpret results |
| [Konveyor Rules](docs/konveyor-rules.md) | Rule types, conditions, fix strategies, customization |
| [Report Format](docs/report-format.md) | Complete JSON report schema reference |
| [PatternFly Walkthrough](docs/patternfly-walkthrough.md) | Step-by-step guide for analyzing PatternFly v5 -> v6 |
| [LLM Integration](docs/llm-integration.md) | Goose setup, CLI contract, cost considerations |

## Known Limitations

- **ESM/CJS declaration deduplication**: Projects that emit both ESM and CJS builds will have roughly doubled symbol counts. The analyzer picks up `.d.ts` from both output directories.
- **MCP server**: The `serve` subcommand is defined but not yet implemented.
- **Language support**: Only TypeScript/JavaScript is currently supported.

## License

See [LICENSE](LICENSE) for details.
