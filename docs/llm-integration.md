# LLM Integration

The semver-analyzer can optionally use an external LLM for behavioral analysis. LLM integration is **only used with the `--behavioral` pipeline** -- the default source-level diff (SD) pipeline is fully deterministic and requires no LLM.

## Overview

- The LLM is invoked as an **external CLI subprocess** -- there are no direct API integrations
- No API keys are configured in semver-analyzer itself -- the external tool handles authentication
- The LLM command receives a prompt as its last argument and returns JSON via stdout
- Any CLI tool that follows this contract can be used (see [Using a Different Provider](#using-a-different-provider))

## When to Use LLM Analysis

The default SD pipeline covers most use cases deterministically:

| SD Pipeline (default) | BU + LLM Pipeline (`--behavioral`) |
|----------------------|-------------------------------------|
| API surface diff | API surface diff |
| Component composition trees | Test-delta correlation |
| CSS token analysis | LLM behavioral inference |
| React API pattern detection | Call graph propagation |
| DOM/ARIA/role changes | Constant rename inference |
| Prop defaults, bindings | Interface rename detection |
| Deterministic, fast, free | Non-deterministic, slower, costs money |

Use `--behavioral` with LLM when you need:
- Analysis of behavioral changes that don't affect the API surface or source profiles
- Test-based confidence signals (test assertions changed = high confidence break)
- LLM-powered understanding of complex implementation changes

For most library migration analysis, the default SD pipeline is sufficient.

## Installing Goose

[Goose](https://github.com/aaif-goose/goose) is the recommended LLM CLI tool. It's an open-source AI agent from the [Agentic AI Foundation](https://aaif.io/) that supports 15+ LLM providers.

### Install

```bash
# macOS / Linux
curl -fsSL https://github.com/aaif-goose/goose/releases/download/stable/download_cli.sh | bash

# Or see full install options at:
# https://goose-docs.ai/docs/getting-started/installation
```

### Verify installation

```bash
goose --version
```

### Configure a model

Goose manages its own model configuration. On first run, it will prompt you to configure a provider (Anthropic, OpenAI, Google, Ollama, etc.) and API key. See the [goose documentation](https://goose-docs.ai/docs/quickstart) for setup details.

No specific model is required by semver-analyzer -- whatever model you configure in goose is what gets used. More capable models (Claude Sonnet/Opus, GPT-4) produce better behavioral analysis.

## Usage

### Basic usage

```bash
semver-analyzer analyze typescript \
  --repo /path/to/library \
  --from v1.0.0 --to v2.0.0 \
  --behavioral \
  --llm-command "goose run --no-session -q -t" \
  -o report.json
```

### Goose flags explained

| Flag | Purpose |
|------|---------|
| `run` | Run goose in single-prompt mode |
| `--no-session` | Don't create or use a persistent session |
| `-q` | Quiet mode -- suppress interactive UI output |
| `-t` | Treat the final argument as the prompt text (not a file path) |

### Key options

| Flag | Default | Description |
|------|---------|-------------|
| `--llm-command <cmd>` | `goose run --no-session -q -t` | CLI command to invoke. If not specified, this default is used when `--behavioral` is set |
| `--llm-timeout <secs>` | `120` | Timeout per LLM invocation |
| `--llm-all-files` | off | Send all changed files to LLM, not just those with test changes. Increases coverage but also cost |
| `--no-llm` | off | Skip LLM analysis entirely (static behavioral analysis only) |

### Running without LLM

You can use `--behavioral` with `--no-llm` to get test-delta heuristics without LLM calls:

```bash
semver-analyzer analyze typescript \
  --repo /path/to/library \
  --from v1.0.0 --to v2.0.0 \
  --behavioral --no-llm \
  -o report.json
```

This detects behavioral breaks via test assertion changes and call graph analysis, but skips the LLM-powered semantic analysis.

## Using a Different Provider

Any CLI tool can be used as long as it follows this contract:

### CLI Contract

1. **Prompt as last argument**: The `--llm-command` string is split on whitespace, and the entire prompt is appended as a single final argument
2. **Response on stdout**: The tool must write its response to stdout
3. **Exit code 0**: Non-zero exit codes are treated as errors
4. **JSON in response**: The response must contain valid JSON, either in a fenced code block (`` ```json ... ``` ``) or inline. The parser tries multiple extraction strategies

### How invocation works

Given `--llm-command "my-tool --format json"`, the analyzer runs:

```
my-tool --format json "<entire prompt text>"
```

The prompt text can be very long (thousands of characters), containing code diffs, function signatures, and structured instructions. The tool must handle large arguments.

### Example with a custom wrapper

If your LLM tool doesn't accept prompts as arguments, write a wrapper script:

```bash
#!/bin/bash
# llm-wrapper.sh -- adapts stdin-based tools for semver-analyzer
echo "$1" | my-llm-tool --stdin --json
```

Then use:

```bash
semver-analyzer analyze typescript \
  --repo /path/to/library \
  --from v1.0.0 --to v2.0.0 \
  --behavioral \
  --llm-command "./llm-wrapper.sh"
```

## What the LLM Analyzes

When the behavioral pipeline runs with LLM enabled, the analyzer makes multiple LLM calls for different analysis tasks:

| Task | What it does | When it runs |
|------|-------------|-------------|
| Function spec inference | Infers preconditions, postconditions, error behavior for changed functions | Per changed function |
| Breaking verdict | Determines if a spec change is breaking | Per changed function |
| Propagation check | Checks if a private function's break propagates to public API | Per private function with breaks |
| File behavioral analysis | Analyzes full file diffs for behavioral changes | When `--llm-all-files` is set |
| Constant rename inference | Detects rename patterns in large constant groups | Once, if many constants changed |
| Interface rename detection | Finds renamed interfaces with low lexical similarity | Once, if many interfaces changed |
| Hierarchy inference | Infers component parent-child relationships | Once per package |

## Cost Considerations

- Each analysis can make **dozens to hundreds** of LLM calls depending on the number of changed functions
- For large libraries (e.g., PatternFly v5 -> v6), expect hundreds of calls
- Cost depends entirely on the model configured in your LLM tool
- The `--llm-timeout` flag prevents individual calls from hanging (default: 120s)
- Use the default SD pipeline (no `--behavioral` flag) for **zero LLM cost** -- it covers most migration analysis needs deterministically
- Use `--behavioral --no-llm` for test-delta analysis without any LLM cost

LLM usage statistics are included in the report's `metadata.llm_usage` field:

```json
{
  "metadata": {
    "llm_usage": {
      "total_calls": 142,
      "total_input_tokens": 850000,
      "total_output_tokens": 120000,
      "estimated_cost_usd": 3.45
    }
  }
}
```
