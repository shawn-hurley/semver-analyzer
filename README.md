# semver-analyzer

Deterministic, structured analysis of semantic versioning breaking changes between two git refs of a TypeScript/JavaScript project. Combines static API surface extraction with optional LLM-based behavioral analysis.

## Quick Start

```bash
# Build
cargo build --release

# Analyze breaking changes between two tags (static analysis only)
semver-analyzer analyze \
  --repo /path/to/your-ts-project \
  --from v1.0.0 \
  --to v2.0.0 \
  --no-llm \
  -o report.json
```

A convenience script is provided for running against [PatternFly](https://github.com/patternfly/patternfly-react), the primary validation target:

```bash
hack/run-patternfly.sh
```

## Prerequisites

- **Rust** (stable toolchain) -- build the analyzer
- **Node.js** and **npm/yarn/pnpm** -- required by target projects for `tsc` and dependency installation
- **Git** -- worktree creation and diff parsing
- **TypeScript** (`tsc`) -- installed as a dev dependency in the target project, or globally

## Installation

```bash
git clone https://github.com/your-org/semver-analyzer.git
cd semver-analyzer
cargo build --release

# Binary is at target/release/semver-analyzer
# Optionally add to PATH:
export PATH="$PWD/target/release:$PATH"
```

## Commands

### `analyze` -- Full Pipeline

Runs the complete analysis: extract API surfaces at both refs, diff them, detect behavioral changes, and diff `package.json` manifests.

```bash
semver-analyzer analyze \
  --repo /path/to/repo \
  --from v5.0.0 \
  --to v6.0.0 \
  --no-llm \
  -o report.json
```

| Option | Description |
|--------|-------------|
| `--repo <path>` | Path to local git repository |
| `--from <ref>` | Old git ref (tag, branch, SHA) |
| `--to <ref>` | New git ref (tag, branch, SHA) |
| `-o, --output <path>` | Output file (JSON). Defaults to stdout |
| `--no-llm` | Skip LLM behavioral analysis (static only) |
| `--llm-command <cmd>` | Command to invoke for LLM analysis (see below) |
| `--llm-all-files` | Send all changed files to LLM, not just those with test changes |
| `--max-llm-cost <usd>` | Cost circuit breaker in USD (default: 5.0) |
| `--build-command <cmd>` | Custom build command instead of `tsc --declaration` |

### `extract` -- Extract API Surface

Extracts the public API surface at a single git ref. Useful for inspecting or caching surfaces.

```bash
semver-analyzer extract \
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

Compares two previously extracted API surface JSON files.

```bash
semver-analyzer diff \
  --from old-surface.json \
  --to new-surface.json \
  -o changes.json
```

## How It Works

The analyzer runs two concurrent pipelines and merges their results:

### TD (Top-Down) Pipeline -- Structural Analysis

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

### BU (Bottom-Up) Pipeline -- Behavioral Analysis

1. Parses `git diff` to find changed source files
2. Extracts function bodies at both refs using OXC
3. Identifies functions whose implementations changed
4. Cross-references with TD findings to avoid duplicates (via `DashMap` + broadcast channel)
5. Discovers associated test files (7 strategies covering common project layouts)
6. If test assertions changed: HIGH confidence behavioral break
7. If LLM enabled: sends diffs to an external LLM for semantic analysis
8. Walks up the call graph for private functions with behavioral breaks

### Output

The report is a JSON document with this structure:

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
          "symbol": "Card.isFlat",
          "kind": "property",
          "change": "removed",
          "before": "isFlat?: boolean",
          "after": null,
          "description": "Property 'isFlat' was removed from Card"
        }
      ],
      "breaking_behavioral_changes": []
    }
  ],
  "manifest_changes": [],
  "metadata": {
    "call_graph_analysis": true,
    "tool_version": "0.1.0",
    "llm_usage": null
  }
}
```

## LLM Integration

The analyzer can optionally use any CLI-accessible LLM for behavioral analysis. The LLM is invoked as an external command with the analysis prompt as the final argument.

```bash
# Using goose
semver-analyzer analyze \
  --repo /path/to/repo \
  --from v5.0.0 --to v6.0.0 \
  --llm-command "goose run --no-session -q -t"

# Using any command that accepts a prompt as its last argument
semver-analyzer analyze \
  --repo /path/to/repo \
  --from v5.0.0 --to v6.0.0 \
  --llm-command "my-llm-cli"
```

The `--max-llm-cost` flag (default: $5.00) acts as a circuit breaker to prevent runaway costs.

## Architecture

```
semver-analyzer (binary)
├── src/main.rs              # CLI entry, report building
├── src/orchestrator.rs      # Concurrent TD/BU pipeline
└── src/cli/mod.rs           # Clap CLI definitions

crates/
├── core/                    # Language-agnostic types and diff engine
│   └── src/
│       ├── traits.rs        # Pluggable language support trait
│       ├── shared.rs        # SharedFindings (DashMap + broadcast)
│       ├── diff/            # 4-phase API surface differ
│       └── types/           # ApiSurface, Symbol, AnalysisReport
├── ts/                      # TypeScript/JavaScript support
│   └── src/
│       ├── extract/         # OXC-based .d.ts API extraction
│       ├── canon/           # 6-rule type canonicalization
│       ├── diff_parser/     # Git diff -> changed functions
│       ├── test_analyzer/   # Test discovery + assertion detection
│       ├── call_graph/      # Same-file caller detection
│       ├── manifest/        # package.json diff
│       └── worktree/        # Git worktree lifecycle, tsc, pkg mgr
└── llm/                     # LLM behavioral analysis
    └── src/
        ├── invoke.rs        # External LLM command execution
        ├── prompts.rs       # Structured prompt templates
        └── spec_compare.rs  # Structural spec comparison
```

The core crate defines a `LanguageSupport` trait, making the architecture language-pluggable. TypeScript is the first (and currently only) implementation.

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

## Known Limitations

- **ESM/CJS declaration deduplication**: Projects that emit both ESM and CJS builds will have roughly doubled symbol counts. The analyzer picks up `.d.ts` from both output directories.
- **MCP server**: The `serve` subcommand is defined but not yet implemented.
- **Language support**: Only TypeScript/JavaScript is currently supported.

## License

See [LICENSE](LICENSE) for details.
