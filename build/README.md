# PatternFly Migration Tools

Automated migration of PatternFly 5 applications to PatternFly 6 using static analysis, pattern-based fixes, LLM-assisted fixes, and AI agent refinement. Analyzes breaking changes across 7 libraries (PatternFly React, PF Topology, PF Component Groups, Dynamic Plugin SDK, Console Plugin SDK, React, React Types).

There are two ways to run the migration:

1. **Container** (recommended) — uses a pre-built image with all tools and rules. Only requires Podman/Docker and LLM credentials. See [Container Runner](#container-runner).
2. **Local archive** — build tools from source with `build.sh`, then run with `run.sh`. See [Building Archives](#building-archives) and [Running Without Container](#running-without-container).

## Table of Contents

- [Quick Start](#quick-start)
- [Prerequisites](#prerequisites)
- [Container Runner (run_container.sh)](#container-runner)
- [Run Modes](#run-modes)
- [Evaluation](#evaluation)
- [Environment Variables](#environment-variables)
- [Logs](#logs)
- [Examples](#examples)
- [Building the Container Image](#building-the-container-image)
- [Running Without Container (run.sh)](#running-without-container)
- [Building Archives (build.sh)](#building-archives)
- [Source Repositories](#source-repositories)

## Quick Start

```bash
# Using GCP Vertex AI (default)
export GCP_PROJECT_ID=my-gcp-project
export GCP_LOCATION=us-east5
./run_container.sh --migrate /path/to/your/app

# Or using OpenAI
export GOOSE_PROVIDER=openai OPENAI_API_KEY=sk-...
./run_container.sh --migrate /path/to/your/app
```

## Prerequisites

| Requirement | Description |
|-------------|-------------|
| Podman or Docker | Container runtime |
| LLM credentials | See [LLM Providers](#llm-providers) below |

## Container Runner

### CLI Reference (`run_container.sh`)

#### Required

| Option | Description |
|--------|-------------|
| `--migrate <PATH>` | Path to the application to migrate |

#### Optional — Container

| Option | Default | Description |
|--------|---------|-------------|
| `--bake` | off | Bake app into image instead of mounting |
| `--goose-config <PATH>` | baked default | Override goose config directory |
| `--image <NAME>` | `quay.io/pranavgaikwad/patternfly-tools:latest` | Container image |
| `--keep` | off | Keep container after completion (for debugging) |
| `--no-memory` | off | Disable memory extension and skip memory volume mount |
| `--log-dir <PATH>` | `.pf-migration-logs/` | Directory to sync logs to |

#### Optional — Migration

| Option | Default | Description |
|--------|---------|-------------|
| `--base-branch <NAME>` | `main` | Branch of the application to migrate |
| `--skip-agent` | off | Skip AI agent step (Phase 2) |
| `--llm-timeout <SECS>` | `300` | LLM timeout per fix |
| `--non-interactive` | off | Skip all prompts |

#### Optional — Evaluation

| Option | Default | Description |
|--------|---------|-------------|
| `--enable-eval` | off | Run evaluation after migration |
| `--eval-only <BRANCH>` | — | Evaluate an existing migrated branch (skips migration) |

## Run Modes

### Mount mode (default)

Mounts the app directory into the container. Changes are applied in real-time on the host.

```bash
./run_container.sh --migrate /path/to/app
```

### Bake mode

Copies the app into a temporary image, runs migration inside it, then syncs results back. Use this when mount performance is slow (e.g., Docker on Mac).

```bash
./run_container.sh --bake --migrate /path/to/app
```

## Migration Pipeline

The migration runs in two phases:

| Phase | Steps | Description |
|-------|-------|-------------|
| **Phase 1** | 1–7 | Automated analysis and fixes |
| **Phase 2** | 8 | AI agent for remaining issues |

### Phase 1 — Automated

| Step | Description |
|------|-------------|
| 1 | Generate provider settings |
| 2 | Start frontend-analyzer-provider |
| 3 | Run kantra static analysis |
| 4 | Stop provider |
| 5 | Convert kantra YAML output to JSON |
| 6 | Apply pattern-based fixes (deterministic) |
| 7 | Apply LLM-based fixes (via Goose + Vertex AI) |

After Phase 1, all changes are committed as "Apply automated migration fixes (pattern-based + LLM)".

### Phase 2 — AI Agent

Step 8 runs Goose to fix remaining build errors, type issues, and test failures. Changes are committed as "Apply AI agent fixes (goose)".

Skip with `--skip-agent`.

## Evaluation

Evaluation compares a migration branch against a pf-codemods baseline and generates an HTML report.

### After migration

```bash
./run_container.sh --enable-eval --migrate /path/to/app
```

### Evaluate an existing branch

```bash
./run_container.sh --eval-only my-migration-branch --migrate /path/to/app
```

The evaluation:
1. Creates a `pf-codemods-MMDDYY-HHMM` branch from the base, runs `npx @patternfly/pf-codemods@latest --v6 --fix`
2. Runs the evaluation agent comparing base → pf-codemods → migration branch
3. Generates `pf-migration-comparison-report.html` in the logs directory
4. Deletes the pf-codemods branch

## LLM Providers

The default provider is GCP Vertex AI. Override by setting `GOOSE_PROVIDER` and the corresponding API key.

### GCP Vertex AI (default)

```bash
export GCP_PROJECT_ID=my-gcp-project
export GCP_LOCATION=us-east5
./run_container.sh --migrate /path/to/app
```

Requires `~/.config/gcloud/application_default_credentials.json` (auto-mounted when present). Generate with `gcloud auth application-default login`.

### OpenAI

```bash
export GOOSE_PROVIDER=openai
export GOOSE_MODEL=gpt-4o
export OPENAI_API_KEY=sk-...
./run_container.sh --migrate /path/to/app
```

### Google Gemini

```bash
export GOOSE_PROVIDER=google
export GOOSE_MODEL=gemini-2.5-flash
export GOOGLE_API_KEY=AIza...
./run_container.sh --migrate /path/to/app
```

### Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `GOOSE_PROVIDER` | No | LLM provider (default: `gcp_vertex_ai`) |
| `GOOSE_MODEL` | No | Model name (default: `claude-opus-4-6`) |
| `GCP_PROJECT_ID` | Vertex AI only | GCP project ID |
| `GCP_LOCATION` | Vertex AI only | GCP region |
| `OPENAI_API_KEY` | OpenAI only | OpenAI API key |
| `GOOGLE_API_KEY` | Gemini only | Google AI API key |
| `ANTHROPIC_API_KEY` | Anthropic only | Anthropic API key |

## Goose Configuration

The image includes a default Goose config using GCP Vertex AI. To use your own:

```bash
./run_container.sh --goose-config ~/.config/goose --migrate /path/to/app
```

## Logs

Logs are saved to `.pf-migration-logs/<timestamp>/` in the directory where you run the script.

| File | Contents |
|------|----------|
| `kantra.log` | Static analysis output |
| `provider.log` | Frontend analyzer provider |
| `fix-pattern.log` | Pattern-based fix output |
| `fix-llm.log` | LLM-assisted fix output |
| `fix-debug/` | Per-file fix-engine debug logs |
| `agent-goose.log` | AI agent transcript |
| `eval-agent.log` | Evaluation agent transcript (if `--enable-eval`) |
| `pf-migration-comparison-report.html` | Evaluation report (if `--enable-eval`) |

## Examples

### Basic migration

```bash
export GCP_PROJECT_ID=my-project GCP_LOCATION=us-east5
./run_container.sh --migrate ~/code/my-pf5-app
```

### Migrate from a specific branch

```bash
./run_container.sh --migrate ~/code/my-app --base-branch develop
```

### Migration without AI agent (Phase 1 only)

```bash
./run_container.sh --migrate ~/code/my-app --skip-agent
```

### Migration with evaluation

```bash
./run_container.sh --enable-eval --migrate ~/code/my-app
```

### Evaluate an existing migration branch

```bash
./run_container.sh --eval-only semver/goose/042926-1043 --migrate ~/code/my-app
```

### Bake mode with custom goose config

```bash
./run_container.sh --bake --goose-config ~/.config/goose --migrate ~/code/my-app
```

### Use a custom container image

```bash
./run_container.sh --image localhost/semver-runner:latest --migrate ~/code/my-app
```

---

## Building the Container Image

The `Containerfile` uses a 10-stage multi-stage build:

| Stage | Purpose |
|-------|---------|
| 1 (go-builder) | Build kantra (Go) |
| 2 (rust-builder) | Build semver-analyzer, frontend-analyzer-provider, fix-engine-cli (Rust) |
| 3a–3g | Generate rules for each of the 7 libraries (run in parallel) |
| 4 (runtime) | Final image with all tools, rules, and runtime dependencies |

```bash
podman build --format docker --layers=false \
  -t quay.io/pranavgaikwad/patternfly-tools:latest \
  -f Containerfile .
```

Use `--format docker` for SHELL directive support. Use `--layers=false` to save disk on large builds. Use `--build-arg KANTRA_ARCH=arm64` when building on ARM.

### Build args

All repos, branches, version refs, and build commands are overridable:

| Arg | Default | Description |
|-----|---------|-------------|
| `KANTRA_VERSION` | `v0.9.2-rc.1` | Kantra release for assets |
| `KANTRA_ARCH` | `amd64` | Kantra release architecture (`amd64` or `arm64`) |
| `SEMVER_REPO` | `konveyor-ecosystem/semver-analyzer` | semver-analyzer repo URL |
| `SEMVER_BRANCH` | `main` | semver-analyzer branch |
| `FIX_ENGINE_REPO` | `konveyor-ecosystem/fix-engine` | fix-engine repo URL |
| `FIX_ENGINE_BRANCH` | `main` | fix-engine branch |
| `PF_REACT_FROM` | `v5.3.3` | PatternFly React source version |
| `PF_REACT_TO` | `v6.4.1` | PatternFly React target version |
| `PF_DEP_FROM` | `v5.4.0` | PatternFly CSS source version |
| `PF_DEP_TO` | `v6.4.0` | PatternFly CSS target version |

Each library stage has its own ARGs for repo, from/to refs, and install/build commands. See the Containerfile for the full list.

---

## Running Without Container

### run.sh

Runs the migration directly on the host. Requires Goose CLI, yq or python3, git, unbuffer.

| Option | Default | Description |
|--------|---------|-------------|
| `--migrate <PATH>` | — | Project to migrate (required) |
| `--base-branch <NAME>` | `main` | Base branch |
| `--agent <NAME>` | `goose` | AI agent: goose, claude, opencode |
| `--skip-agent` | off | Skip AI agent step |
| `--rules-dir <PATH>` | pre-packaged | Custom rules directory |
| `--llm-timeout <SECS>` | `300` | LLM timeout |
| `--non-interactive` | off | Skip prompts |
| `--generate-rules` | — | Generate new rules instead of migrating |

### eval.sh

Runs evaluation against an existing migrated branch.

| Option | Default | Description |
|--------|---------|-------------|
| `--migrate <PATH>` | — | Project path (required) |
| `--branch <BRANCH>` | — | Migration branch to evaluate (required) |
| `--base-branch <NAME>` | `main` | Base branch |
| `--agent <NAME>` | `goose` | AI agent |
| `--non-interactive` | off | Skip prompts |

---

## Building Archives

Builds all tools from source and generates rules for all 7 libraries into a distributable ZIP archive.

```bash
./build.sh
```

Prompts for target platform and kantra release. All repo URLs/branches are overridable via environment variables (e.g., `SEMVER_REPO_BRANCH=updates ./build.sh`).

Requires: Go 1.23+, Rust (via rustup), Node.js 18+ and 20+ (via nvm), git, curl, unzip, python3.

---

## Source Repositories

| Repo | Purpose |
|------|---------|
| [konveyor/kantra](https://github.com/konveyor/kantra) | Static analysis CLI |
| [konveyor/analyzer-lsp](https://github.com/konveyor/analyzer-lsp) | java-external-provider |
| [konveyor-ecosystem/semver-analyzer](https://github.com/konveyor-ecosystem/semver-analyzer) | Breaking change detection |
| [konveyor-ecosystem/konveyor-core](https://github.com/konveyor-ecosystem/konveyor-core) | Shared Konveyor types |
| [konveyor-ecosystem/frontend-analyzer-provider](https://github.com/konveyor-ecosystem/frontend-analyzer-provider) | Frontend analysis provider |
| [konveyor-ecosystem/fix-engine](https://github.com/konveyor-ecosystem/fix-engine) | Fix engine CLI |
| [patternfly/patternfly-react](https://github.com/patternfly/patternfly-react) | PatternFly React (analyzed) |
| [patternfly/patternfly](https://github.com/patternfly/patternfly) | PatternFly CSS (analyzed) |
