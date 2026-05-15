#!/usr/bin/env bash
set -eo pipefail

GLOBAL_START=$SECONDS
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ── Colors ───────────────────────────────────────────────────────────────
if [[ -z "${NO_COLOR:-}" ]] && [[ -t 1 ]]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
    BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; BLUE=''; BOLD=''; NC=''
fi

# ── Layout ───────────────────────────────────────────────────────────────
EVAL_PROMPT_FILE="$SCRIPT_DIR/eval_prompt.md"
LOGS_DIR="${LOGS_DIR:-$SCRIPT_DIR/logs/$(date -u +%Y%m%dT%H%M%S)}"

# ── Defaults ─────────────────────────────────────────────────────────────
MIGRATE_PATH=""
BASE_BRANCH="main"
MIGRATION_BRANCH=""
AGENT="goose"
NON_INTERACTIVE=false

# ── Utilities ────────────────────────────────────────────────────────────
info()  { printf "${GREEN}[INFO]${NC}  %s\n" "$*"; }
warn()  { printf "${YELLOW}[WARN]${NC}  %s\n" "$*"; }
error() { printf "${RED}[ERROR]${NC} %s\n" "$*" >&2; }
step()  { printf "\n${BLUE}[STEP %s]${NC} %s\n" "$1" "$2"; }
die()   { error "$@"; exit 1; }

require_file() {
    [[ -f "$1" ]] || die "Required file not found: $1"
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "Required command not found: $1"
}

# ── Cleanup ──────────────────────────────────────────────────────────────
cleanup() {
    local exit_code=$?
    local total_elapsed=$(($SECONDS - GLOBAL_START))
    if [[ "$exit_code" -ne 0 ]]; then
        printf "\n" >&2
        error "Eval failed with exit code $exit_code (${total_elapsed}s)"
        error "Check logs in $LOGS_DIR/ for details"
    else
        info "Total eval runtime: $((total_elapsed/60))m$((total_elapsed%60))s"
    fi
    exit "$exit_code"
}
trap cleanup EXIT INT TERM

# ── Argument parsing ─────────────────────────────────────────────────────
usage() {
    cat <<'EOF'
PatternFly Migration Evaluation

Evaluates a migrated branch by comparing it against pf-codemods output and the base branch.

Usage: ./eval.sh --migrate <PATH> --branch <BRANCH> [OPTIONS]

Required:
  --migrate <PATH>           Path to the application
  --branch <BRANCH>          Migration branch to evaluate

Options:
  --base-branch <NAME>       Base branch (default: main)
  --agent <NAME>             Agent: goose (default), claude, opencode
  --non-interactive          Skip all prompts
  -h, --help                 Show help
EOF
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --migrate)         MIGRATE_PATH="$2"; shift 2 ;;
        --branch)          MIGRATION_BRANCH="$2"; shift 2 ;;
        --base-branch)     BASE_BRANCH="$2"; shift 2 ;;
        --agent)           AGENT="$2"; shift 2 ;;
        --non-interactive) NON_INTERACTIVE=true; shift ;;
        -h|--help)         usage ;;
        *)                 die "Unknown option: $1" ;;
    esac
done

[[ -z "$MIGRATE_PATH" ]] && die "Missing required --migrate <PATH>"
[[ -d "$MIGRATE_PATH" ]] || die "Not a directory: $MIGRATE_PATH"
MIGRATE_PATH="$(cd "$MIGRATE_PATH" && pwd)"
[[ -z "$MIGRATION_BRANCH" ]] && die "Missing required --branch <BRANCH>"

mkdir -p "$LOGS_DIR"

TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/pf-eval.XXXXXX")

info "Project:    $MIGRATE_PATH"
info "Base:       $BASE_BRANCH"
info "Branch:     $MIGRATION_BRANCH"
info "Agent:      $AGENT"
info "Logs:       $LOGS_DIR/"

# ── Step 1: Run pf-codemods on a new branch ──────────────────────────────
step "1/3" "Running pf-codemods"

CODEMODS_BRANCH="pf-codemods-$(date -u +%m%d%y-%H%M)"
info "Creating pf-codemods branch: $CODEMODS_BRANCH"

(cd "$MIGRATE_PATH" \
    && git checkout "$BASE_BRANCH" \
    && git checkout -b "$CODEMODS_BRANCH") \
    || die "Failed to create pf-codemods branch from $BASE_BRANCH"

info "Running 'npx @patternfly/pf-codemods@latest $MIGRATE_PATH --v6 --fix'"
(cd "$MIGRATE_PATH" && npx @patternfly/pf-codemods@latest "$MIGRATE_PATH" --v6 --fix) \
    > "$LOGS_DIR/pf-codemods.log" 2>&1 || {
    warn "pf-codemods exited with non-zero status. Check $LOGS_DIR/pf-codemods.log"
}

# Commit codemods changes
(cd "$MIGRATE_PATH" && \
    git add -A && \
    git diff --cached --quiet || \
    git commit -m "Apply pf-codemods v6 migration") \
    > /dev/null 2>&1 || true
info "Committed pf-codemods changes on $CODEMODS_BRANCH"

# Switch back to migration branch for eval context
(cd "$MIGRATE_PATH" && git checkout "$MIGRATION_BRANCH") \
    || die "Failed to checkout migration branch $MIGRATION_BRANCH"

# ── Step 2: Run evaluation agent ─────────────────────────────────────────
step "2/3" "Running evaluation agent"

require_file "$EVAL_PROMPT_FILE"

EVAL_ARGS="$BASE_BRANCH $CODEMODS_BRANCH $MIGRATION_BRANCH"
eval_prompt=$(sed "s|\$ARGUMENTS|$EVAL_ARGS|g" "$EVAL_PROMPT_FILE")

eval_prompt_tmp="$TEMP_DIR/eval_prompt.md"
echo "$eval_prompt" > "$eval_prompt_tmp"

pushd "$MIGRATE_PATH" > /dev/null || die "Failed to cd into $MIGRATE_PATH"

info "Evaluating: $BASE_BRANCH → $CODEMODS_BRANCH vs $MIGRATION_BRANCH"
info "Follow logs: tail -f $LOGS_DIR/eval-agent.log"

case "$AGENT" in
    goose)
        require_command goose
        info "Running 'GOOSE_MODE=auto goose run -i $eval_prompt_tmp'"
        unbuffer env GOOSE_MODE=auto goose run -i "$eval_prompt_tmp" \
            > "$LOGS_DIR/eval-agent.log" 2>&1 || {
            warn "Evaluation agent exited with non-zero status. Check $LOGS_DIR/eval-agent.log"
        }
        ;;
    claude)
        require_command claude
        info "Running 'claude --allowedTools ... -p $eval_prompt_tmp'"
        unbuffer claude --allowedTools "Bash" "Edit" "Write" "Read" "WebSearch" "WebFetch" \
            -p "$(cat "$eval_prompt_tmp")" \
            > "$LOGS_DIR/eval-agent.log" 2>&1 || {
            warn "Evaluation agent exited with non-zero status. Check $LOGS_DIR/eval-agent.log"
        }
        ;;
    opencode)
        require_command opencode
        info "Running 'opencode run $eval_prompt_tmp'"
        unbuffer opencode run "$(cat "$eval_prompt_tmp")" \
            > "$LOGS_DIR/eval-agent.log" 2>&1 || {
            warn "Evaluation agent exited with non-zero status. Check $LOGS_DIR/eval-agent.log"
        }
        ;;
    *)
        die "Invalid agent: $AGENT. Must be goose, claude, or opencode"
        ;;
esac

popd > /dev/null

# Copy evaluation report to logs and inject stats
REPORT_FILE="$MIGRATE_PATH/pf-migration-comparison-report.html"
STATS_FILE="$MIGRATE_PATH/.pf-migration/stats.json"

if [[ -f "$REPORT_FILE" ]]; then
    # Inject stats section if stats.json exists
    if [[ -f "$STATS_FILE" ]]; then
        info "Injecting migration stats into report"
        python3 -c "
import json, sys

with open(sys.argv[1]) as f:
    stats = json.load(f)

timing = stats.get('timing', {})
migration = stats.get('migration', {})
tokens = stats.get('tokens', {})

def fmt_time(secs):
    m, s = divmod(secs, 60)
    return f'{m}m {s}s'

html = '''
<section id=\"migration-stats\" style=\"margin-top: 2rem;\">
<h2 style=\"color: var(--accent, #58a6ff); border-bottom: 2px solid var(--accent, #58a6ff); padding-bottom: 0.5rem;\">Migration Stats</h2>
<div style=\"display: grid; grid-template-columns: repeat(3, 1fr); gap: 1rem; margin-top: 1rem;\">
  <div style=\"background: var(--surface, #161b22); border: 1px solid var(--border, #30363d); border-radius: 8px; padding: 1rem;\">
    <h3 style=\"color: var(--teal, #39d2c0); margin: 0 0 0.5rem 0; font-size: 1rem;\">Timing</h3>
    <div style=\"display: flex; justify-content: space-between; padding: 0.25rem 0; border-bottom: 1px solid var(--border, #30363d);\">
      <span style=\"color: var(--text-muted, #8b949e);\">Fix Engine fixes</span>
      <span>''' + fmt_time(timing.get('phase1_secs', 0)) + '''</span>
    </div>
    <div style=\"display: flex; justify-content: space-between; padding: 0.25rem 0; border-bottom: 1px solid var(--border, #30363d);\">
      <span style=\"color: var(--text-muted, #8b949e);\">Agent fixes</span>
      <span>''' + fmt_time(timing.get('phase2_secs', 0)) + '''</span>
    </div>
    <div style=\"display: flex; justify-content: space-between; padding: 0.25rem 0; font-weight: bold;\">
      <span>Total</span>
      <span>''' + fmt_time(timing.get('total_secs', 0)) + '''</span>
    </div>
  </div>
  <div style=\"background: var(--surface, #161b22); border: 1px solid var(--border, #30363d); border-radius: 8px; padding: 1rem;\">
    <h3 style=\"color: var(--teal, #39d2c0); margin: 0 0 0.5rem 0; font-size: 1rem;\">Migration</h3>
    <div style=\"display: flex; justify-content: space-between; padding: 0.25rem 0; border-bottom: 1px solid var(--border, #30363d);\">
      <span style=\"color: var(--text-muted, #8b949e);\">Branch</span>
      <span><code>''' + migration.get('branch', 'N/A') + '''</code></span>
    </div>
    <div style=\"display: flex; justify-content: space-between; padding: 0.25rem 0; border-bottom: 1px solid var(--border, #30363d);\">
      <span style=\"color: var(--text-muted, #8b949e);\">Base</span>
      <span><code>''' + migration.get('base_branch', 'N/A') + '''</code></span>
    </div>
    <div style=\"display: flex; justify-content: space-between; padding: 0.25rem 0;\">
      <span style=\"color: var(--text-muted, #8b949e);\">Timestamp</span>
      <span>''' + migration.get('timestamp', 'N/A') + '''</span>
    </div>
  </div>
  <div style=\"background: var(--surface, #161b22); border: 1px solid var(--border, #30363d); border-radius: 8px; padding: 1rem;\">
    <h3 style=\"color: var(--teal, #39d2c0); margin: 0 0 0.5rem 0; font-size: 1rem;\">Token Usage</h3>
    <pre style=\"background: var(--surface2, #1c2129); padding: 0.5rem; border-radius: 4px; font-size: 0.8rem; overflow-x: auto;\">''' + json.dumps(tokens, indent=2) + '''</pre>
  </div>
</div>
</section>
'''

with open(sys.argv[2]) as f:
    report = f.read()

report = report.replace('</body>', html + '</body>')

with open(sys.argv[2], 'w') as f:
    f.write(report)
" "$STATS_FILE" "$REPORT_FILE" 2>/dev/null || warn "Failed to inject stats into report"
    fi

    cp "$REPORT_FILE" "$LOGS_DIR/"
    info "Evaluation report: $LOGS_DIR/pf-migration-comparison-report.html"
else
    warn "Evaluation report not found at $REPORT_FILE"
fi

# ── Step 3: Cleanup pf-codemods branch ───────────────────────────────────
step "3/3" "Cleaning up"

(cd "$MIGRATE_PATH" && git branch -D "$CODEMODS_BRANCH") \
    > /dev/null 2>&1 || warn "Failed to delete $CODEMODS_BRANCH"
info "Deleted pf-codemods branch: $CODEMODS_BRANCH"

printf "\n"
info "Evaluation complete!"
info "Project: $MIGRATE_PATH"
info "Logs: $LOGS_DIR/"
