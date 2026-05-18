# PatternFly Migration Tools — Runner

Migrate PatternFly 5 applications to PatternFly 6 using pre-packaged rules and AI-assisted fixes.

## Prerequisites

- **AI agent** (one of): [Goose](https://github.com/block/goose), [Claude Code](https://docs.anthropic.com/en/docs/claude-code), or [OpenCode](https://github.com/opencode-ai/opencode)
- **yq** or **python3** (for YAML-to-JSON conversion)
- **unbuffer** (`brew install expect` on macOS)
- **git**

For rule generation only:
- **git**
- **nvm** with Node.js

## Quick Start

```bash
cd patternfly-tools/
./run.sh --migrate /path/to/your/app
```

Or run interactively (prompts for options):

```bash
./run.sh
```

## CLI Reference

### Required

| Option | Description |
|--------|-------------|
| `--migrate <PATH>` | Path to the application to migrate |

### Optional

| Option | Default | Description |
|--------|---------|-------------|
| `--base-branch <NAME>` | `main` | Base branch to migrate from |
| `--agent <NAME>` | `goose` | AI agent: `goose`, `claude`, `opencode` |
| `--skip-agent` | off | Skip AI agent step (Phase 2), run only automated fixes |
| `--rules-dir <PATH>` | pre-packaged | Use custom rules directory |
| `--llm-timeout <SECS>` | `300` | Timeout per LLM operation |
| `--non-interactive` | off | Skip all confirmation prompts |
| `--generate-rules` | — | Generate new PatternFly rules (alternative mode) |
| `-h, --help` | — | Show help |

## Migration Pipeline

| Phase | Steps | Description |
|-------|-------|-------------|
| **Phase 1** | 1–7 | Kantra analysis → pattern fixes → LLM fixes → git commit |
| **Phase 2** | 8 | AI agent for build errors and remaining issues → git commit |

### Phase 1: Automated analysis and fixes (steps 1-7)

| Step | Description |
|------|-------------|
| 1 | Generate provider settings |
| 2 | Start frontend-analyzer-provider |
| 3 | Run kantra static analysis |
| 4 | Stop frontend-analyzer-provider |
| 5 | Convert analysis output (YAML to JSON) |
| 6 | Apply pattern-based fixes |
| 7 | Apply LLM-based fixes (via goose) |

After step 7, all automated changes are committed with the message "Apply automated migration fixes (pattern-based + LLM)".

### Phase 2: AI agent (step 8)

| Step | Description |
|------|-------------|
| 8 | Run AI agent for remaining fixes (build errors, test failures) |

The AI agent focuses on getting the app to build and pass tests — it collects all errors, groups them by root cause, and fixes them in batches.

A migration branch `semver/goose/MMDDYY-HHMM` is created from the base branch before any changes. Skip Phase 2 with `--skip-agent`.

## Rule Generation

To generate rules from a different PatternFly version range:

```bash
./run.sh --generate-rules
```

This clones PatternFly repos, prompts for version tags, and generates rules. The output path is saved to `.semver_runner` in the current directory and will be offered on the next `--migrate` run.

Non-interactive:

```bash
./run.sh --generate-rules --from v5.4.0 --to v6.4.1 --dep-from v5.4.0 --dep-to v6.4.0 --non-interactive
```

### Rule generation options

| Option | Description |
|--------|-------------|
| `--from <REF>` | PatternFly React source version tag |
| `--to <REF>` | PatternFly React target version tag |
| `--dep-from <REF>` | PatternFly CSS source version tag |
| `--dep-to <REF>` | PatternFly CSS target version tag |

### Per-ref build configuration

When the `--from` and `--to` refs require different Node.js versions or build commands, use the per-ref flags. For example, migrating **quipucords-ui** from PatternFly 5.3.3 (Node 18) to PatternFly 6.4.1 (Node 20):

```bash
./run.sh --generate-rules \
  --from v5.3.3 --to v6.4.1 \
  --dep-from v5.3.0 --dep-to v6.4.0 \
  --from-node-version 18 --to-node-version 20 \
  --from-install-command "corepack yarn install" \
  --non-interactive
```

| Flag | Description |
|------|-------------|
| `--from-node-version <V>` | Node version for the `--from` ref |
| `--to-node-version <V>` | Node version for the `--to` ref |
| `--from-install-command <C>` | Install command for the `--from` ref |
| `--to-install-command <C>` | Install command for the `--to` ref |
| `--from-build-command <C>` | Build command for the `--from` ref |
| `--to-build-command <C>` | Build command for the `--to` ref |

All are optional — when omitted, semver-analyzer uses its defaults.

## Using Custom Rules

```bash
./run.sh --migrate /path/to/app --rules-dir /path/to/rules
```

## Logs

Logs are written to `logs/<timestamp>/` relative to the script directory:

| File | Contents |
|------|----------|
| `kantra.log` | Static analysis output |
| `provider.log` | Frontend analyzer provider |
| `fix-pattern.log` | Pattern-based fix output |
| `fix-llm.log` | LLM-assisted fix output |
| `fix-debug/` | Per-file fix-engine debug logs |
| `agent-goose.log` | AI agent transcript |

## Examples

```bash
# Basic migration
./run.sh --migrate ~/code/my-app

# Migrate from a specific branch
./run.sh --migrate ~/code/my-app --base-branch develop

# Automated fixes only (no AI agent)
./run.sh --migrate ~/code/my-app --skip-agent --non-interactive

# Generate fresh rules
./run.sh --generate-rules --from v5.4.0 --to v6.4.1 --dep-from v5.4.0 --dep-to v6.4.0
```

## Evaluation

Use `eval.sh` to evaluate migration quality:

```bash
./eval.sh --migrate /path/to/app --branch semver/goose/042926-1043
```

| Option | Default | Description |
|--------|---------|-------------|
| `--migrate <PATH>` | — | Application path (required) |
| `--branch <BRANCH>` | — | Migration branch to evaluate (required) |
| `--base-branch <NAME>` | `main` | Base branch |
| `--agent <NAME>` | `goose` | AI agent |

Generates `pf-migration-comparison-report.html` in the logs directory.
