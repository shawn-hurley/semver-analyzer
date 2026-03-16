#!/usr/bin/env bash
#
# run-patternfly.sh -- Clone patternfly-react to a temp directory and run
# semver-analyzer against it, then optionally generate Konveyor rules
# and fix guidance.
#
# Usage:
#   ./hack/run-patternfly.sh [OPTIONS]
#
# Options:
#   --from REF       Old git ref (default: v5.4.0)
#   --to REF         New git ref (default: v6.4.0)
#   --output FILE    Output report path (default: patternfly-report.json)
#   --keep           Keep the cloned repo after analysis (default: clean up)
#   --repo DIR       Use an existing clone instead of fetching a new one
#   --build-command  Custom build command (default: "yarn build")
#   --llm-command    LLM command for behavioral analysis (omit for static-only)
#   --release        Build semver-analyzer in release mode
#   --konveyor       Generate Konveyor rules and fix guidance from the report
#   --konveyor-dir   Output directory for Konveyor rules (default: ./konveyor-rules)
#   --help           Show this help message
#
# Examples:
#   # Default: v5.4.0 vs v6.4.0, static analysis only
#   ./hack/run-patternfly.sh
#
#   # Custom refs
#   ./hack/run-patternfly.sh --from v5.0.0 --to v5.4.0
#
#   # Keep the clone for repeated runs
#   ./hack/run-patternfly.sh --keep
#
#   # Re-use an existing clone
#   ./hack/run-patternfly.sh --repo /tmp/patternfly-react
#
#   # With LLM behavioral analysis
#   ./hack/run-patternfly.sh --llm-command "goose run --no-session -q -t"
#
#   # Generate Konveyor rules after analysis
#   ./hack/run-patternfly.sh --konveyor
#
#   # Full pipeline: analyze + generate rules + fix guidance
#   ./hack/run-patternfly.sh --konveyor --konveyor-dir ./pf-migration-rules

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

FROM_REF="v5.4.0"
TO_REF="v6.4.0"
OUTPUT="patternfly-report.json"
KEEP=false
REPO=""
BUILD_COMMAND="yarn build"
LLM_COMMAND=""
RELEASE=false
KONVEYOR=false
KONVEYOR_DIR="./konveyor-rules"
PF_REPO_URL="https://github.com/patternfly/patternfly-react.git"

# ── Parse arguments ──────────────────────────────────────────────────────────

usage() {
    sed -n '/^# Usage:/,/^[^#]/p' "$0" | head -n -1 | sed 's/^# \?//'
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --from)       FROM_REF="$2";       shift 2 ;;
        --to)         TO_REF="$2";         shift 2 ;;
        --output)     OUTPUT="$2";         shift 2 ;;
        --keep)       KEEP=true;           shift   ;;
        --repo)       REPO="$2";           shift 2 ;;
        --build-command) BUILD_COMMAND="$2"; shift 2 ;;
        --llm-command)   LLM_COMMAND="$2";   shift 2 ;;
        --release)    RELEASE=true;        shift   ;;
        --konveyor)   KONVEYOR=true;       shift   ;;
        --konveyor-dir) KONVEYOR_DIR="$2"; shift 2 ;;
        --help|-h)    usage ;;
        *)
            echo "Unknown option: $1" >&2
            echo "Run with --help for usage." >&2
            exit 1
            ;;
    esac
done

# ── Build semver-analyzer ────────────────────────────────────────────────────

echo "==> Building semver-analyzer..."

BUILD_FLAGS=()
if [[ "$RELEASE" == true ]]; then
    BUILD_FLAGS+=(--release)
    BINARY="$PROJECT_ROOT/target/release/semver-analyzer"
else
    BINARY="$PROJECT_ROOT/target/debug/semver-analyzer"
fi

(cd "$PROJECT_ROOT" && cargo build "${BUILD_FLAGS[@]}")

if [[ ! -x "$BINARY" ]]; then
    echo "ERROR: Binary not found at $BINARY" >&2
    exit 1
fi

echo "    Binary: $BINARY"

# ── Clone or reuse patternfly-react ──────────────────────────────────────────

CLEANUP=""

if [[ -n "$REPO" ]]; then
    if [[ ! -d "$REPO/.git" ]]; then
        echo "ERROR: --repo path is not a git repository: $REPO" >&2
        exit 1
    fi
    PF_DIR="$REPO"
    echo "==> Using existing repo: $PF_DIR"
else
    PF_DIR="$(mktemp -d "${TMPDIR:-/tmp}/patternfly-react.XXXXXX")"
    echo "==> Cloning patternfly-react to $PF_DIR..."
    echo "    This may take a few minutes for the initial clone."

    git clone --no-checkout "$PF_REPO_URL" "$PF_DIR"

    # Only fetch the refs we need to keep clone fast
    (cd "$PF_DIR" && git fetch origin "refs/tags/$FROM_REF:refs/tags/$FROM_REF" "refs/tags/$TO_REF:refs/tags/$TO_REF" 2>/dev/null || true)

    if [[ "$KEEP" == false ]]; then
        CLEANUP="$PF_DIR"
    else
        echo "    --keep: repo will be preserved at $PF_DIR"
    fi
fi

# ── Validate refs exist ─────────────────────────────────────────────────────

echo "==> Validating git refs..."

validate_ref() {
    local ref="$1"
    if ! (cd "$PF_DIR" && git rev-parse --verify "$ref" >/dev/null 2>&1); then
        echo "ERROR: Git ref '$ref' not found in $PF_DIR" >&2
        echo "       Available tags matching v5/v6:" >&2
        (cd "$PF_DIR" && git tag -l 'v5.*' 'v6.*' | tail -10) >&2
        exit 1
    fi
}

validate_ref "$FROM_REF"
validate_ref "$TO_REF"

FROM_SHA="$(cd "$PF_DIR" && git rev-parse --short "$FROM_REF")"
TO_SHA="$(cd "$PF_DIR" && git rev-parse --short "$TO_REF")"
echo "    $FROM_REF ($FROM_SHA) -> $TO_REF ($TO_SHA)"

# ── Run analysis ─────────────────────────────────────────────────────────────

echo "==> Running semver-analyzer..."
echo "    From: $FROM_REF"
echo "    To:   $TO_REF"
echo "    Output: $OUTPUT"
echo ""

ANALYZE_ARGS=(
    analyze
    --repo "$PF_DIR"
    --from "$FROM_REF"
    --to "$TO_REF"
    --output "$OUTPUT"
    --build-command "$BUILD_COMMAND"
)

if [[ -z "$LLM_COMMAND" ]]; then
    ANALYZE_ARGS+=(--no-llm)
else
    ANALYZE_ARGS+=(--llm-command "$LLM_COMMAND")
fi

START_TIME="$(date +%s)"

"$BINARY" "${ANALYZE_ARGS[@]}"

END_TIME="$(date +%s)"
ELAPSED=$(( END_TIME - START_TIME ))

echo ""
echo "==> Analysis complete in ${ELAPSED}s"
echo "    Report written to: $OUTPUT"

# ── Summary ──────────────────────────────────────────────────────────────────

if command -v jq >/dev/null 2>&1 && [[ -f "$OUTPUT" ]]; then
    echo ""
    echo "==> Report summary:"
    jq '{
        from: .comparison.from_ref,
        to: .comparison.to_ref,
        total_breaking_changes: .summary.total_breaking_changes,
        breaking_api_changes: .summary.breaking_api_changes,
        breaking_behavioral_changes: .summary.breaking_behavioral_changes,
        files_with_breaking_changes: .summary.files_with_breaking_changes
    }' "$OUTPUT"
elif command -v yq >/dev/null 2>&1 && [[ -f "$OUTPUT" ]]; then
    echo ""
    echo "==> Report summary (via yq):"
    yq -P '.comparison.from_ref as $from | .comparison.to_ref as $to |
        {
            "from": $from,
            "to": $to,
            "total_breaking_changes": .summary.total_breaking_changes,
            "breaking_api_changes": .summary.breaking_api_changes,
            "breaking_behavioral_changes": .summary.breaking_behavioral_changes,
            "files_with_breaking_changes": .summary.files_with_breaking_changes
        }' "$OUTPUT"
fi

# ── Generate Konveyor rules ──────────────────────────────────────────────────

if [[ "$KONVEYOR" == true ]] && [[ -f "$OUTPUT" ]]; then
    echo ""
    echo "==> Generating Konveyor rules and fix guidance..."
    echo "    Rules dir:  $KONVEYOR_DIR"
    echo "    Fix dir:    $(dirname "$KONVEYOR_DIR")/fix-guidance"
    echo ""

    "$BINARY" konveyor \
        --from-report "$OUTPUT" \
        --output-dir "$KONVEYOR_DIR"

    echo ""
    echo "==> Konveyor output summary:"

    # Show ruleset metadata
    if command -v yq >/dev/null 2>&1; then
        echo "    Ruleset:"
        yq '.' "$KONVEYOR_DIR/ruleset.yaml" 2>/dev/null | sed 's/^/      /'

        FIX_DIR="$(dirname "$KONVEYOR_DIR")/fix-guidance"
        if [[ -f "$FIX_DIR/fix-guidance.yaml" ]]; then
            echo ""
            echo "    Fix guidance summary:"
            yq '.summary' "$FIX_DIR/fix-guidance.yaml" 2>/dev/null | sed 's/^/      /'
        fi
    fi

    # Count rules
    if command -v yq >/dev/null 2>&1 && [[ -f "$KONVEYOR_DIR/breaking-changes.yaml" ]]; then
        RULE_COUNT=$(yq 'length' "$KONVEYOR_DIR/breaking-changes.yaml" 2>/dev/null || echo "?")
        echo ""
        echo "    Generated $RULE_COUNT rules"
        echo ""
        echo "    Use with Konveyor:"
        echo "      konveyor-analyzer --rules $KONVEYOR_DIR"
    fi
fi

# ── Cleanup ──────────────────────────────────────────────────────────────────

if [[ -n "$CLEANUP" ]]; then
    echo ""
    echo "==> Cleaning up $CLEANUP..."
    rm -rf "$CLEANUP"
fi
