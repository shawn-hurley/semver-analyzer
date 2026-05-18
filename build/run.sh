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

# ── Archive layout ───────────────────────────────────────────────────────
KANTRA_DIR="$SCRIPT_DIR/.kantra"
BIN_DIR="$SCRIPT_DIR/bin"
RULES_DIR="$SCRIPT_DIR/rules"
STRATEGIES_DIR="$RULES_DIR"
SEMVER_BIN="$BIN_DIR/semver-analyzer"
FAP_BIN="$BIN_DIR/frontend-analyzer-provider"
FIX_BIN="$BIN_DIR/fix-engine-cli"
KANTRA_BIN="$KANTRA_DIR/kantra"
TOKEN_MAPPINGS="$SCRIPT_DIR/patternfly-token-mappings.yaml"
PROMPT_FILE="$SCRIPT_DIR/prompt.md"
GOOSEHINTS_SRC="$HOME/.config/goose/.goosehints"
LOGS_DIR="${LOGS_DIR:-$SCRIPT_DIR/logs/$(date -u +%Y%m%dT%H%M%S)}"
PROVIDER_PORT=9002

# ── MemPalace init ──────────────────────────────────────────────────────
if [[ "${DISABLE_MEMORY:-}" == "1" ]]; then
    if [[ -f "$HOME/.config/goose/config.yaml" ]]; then
        chmod 644 "$HOME/.config/goose/config.yaml"
        yq -i '.extensions.mempalace.enabled = false' "$HOME/.config/goose/config.yaml" 2>/dev/null || true
        chmod 444 "$HOME/.config/goose/config.yaml"
    fi
elif [ ! -f "$HOME/.mempalace/mempalace.yaml" ]; then
    mkdir -p "$HOME/.mempalace"
    mempalace init "$HOME/.mempalace" --yes --no-llm 2>/dev/null || true
fi

# ── Defaults ─────────────────────────────────────────────────────────────
MODE=""
MIGRATE_PATH=""
CUSTOM_RULES_DIR=""
RULES_PATH=""
AGENT="goose"
LLM_TIMEOUT=300
NON_INTERACTIVE=false
BASE_BRANCH="main"
SKIP_AGENT=false
GEN_FROM="" GEN_TO="" GEN_DEP_FROM="" GEN_DEP_TO=""
GEN_FROM_NODE_VERSION="" GEN_TO_NODE_VERSION=""
GEN_FROM_INSTALL_CMD="" GEN_TO_INSTALL_CMD=""
GEN_FROM_BUILD_CMD="" GEN_TO_BUILD_CMD=""
PROVIDER_PID=""
TEMP_DIR=""

# ── Utilities ────────────────────────────────────────────────────────────
info()  { printf "${GREEN}[INFO]${NC}  %s\n" "$*"; }
warn()  { printf "${YELLOW}[WARN]${NC}  %s\n" "$*"; }
error() { printf "${RED}[ERROR]${NC} %s\n" "$*" >&2; }
step()  { printf "\n${BLUE}[STEP %s]${NC} %s\n" "$1" "$2"; }
die()   { error "$@"; exit 1; }

run_timed() {
    local label="$1" logfile="$2"; shift 2
    local pid elapsed
    info "Running '$*'"
    "$@" > "$logfile" 2>&1 &
    pid=$!
    CHILD_PIDS="$CHILD_PIDS $pid"
    info "Follow logs: tail -f $logfile"
    local start=$SECONDS
    while kill -0 "$pid" 2>/dev/null; do
        elapsed=$(($SECONDS - start))
        printf "\r${GREEN}[INFO]${NC}  %s ... %dm%02ds" "$label" $((elapsed/60)) $((elapsed%60)) >&2
        sleep 1
    done
    wait "$pid"
    local rc=$?
    elapsed=$(($SECONDS - start))
    printf "\r${GREEN}[INFO]${NC}  %s complete (%dm%02ds)          \n" "$label" $((elapsed/60)) $((elapsed%60)) >&2
    return $rc
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "Required command not found: $1"
}

require_file() {
    [[ -f "$1" ]] || die "Required file not found: $1"
}

confirm_step() {
    if [[ "$NON_INTERACTIVE" == true ]]; then return 0; fi
    local answer
    answer=$(prompt_choice "$1 (y/n)" "y")
    [[ "$answer" == "y" || "$answer" == "Y" ]]
}

prompt_choice() {
    local prompt="$1" default="$2" input
    printf "${BOLD}%s${NC} [%s]: " "$prompt" "$default" >&2
    read -r input < /dev/tty
    echo "${input:-$default}"
}

prompt_select() {
    local prompt="$1"; shift
    local i=1 choice=""
    for opt in "$@"; do
        printf "  %d) %s\n" "$i" "$opt" >&2
        i=$((i + 1))
    done
    printf "${BOLD}%s${NC} [1]: " "$prompt" >&2
    read -r choice < /dev/tty
    choice="${choice:-1}"
    i=1
    for opt in "$@"; do
        if [[ "$i" -eq "$choice" ]]; then
            echo "$opt"
            return
        fi
        i=$((i + 1))
    done
    echo "$1"
}

# ── Cleanup ──────────────────────────────────────────────────────────────
CHILD_PIDS=""

cleanup() {
    local exit_code=$?
    rm -f "${MIGRATE_PATH:+$MIGRATE_PATH/.goosehints}"
    for pid in $PROVIDER_PID $CHILD_PIDS; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    local total_elapsed=$(($SECONDS - GLOBAL_START))
    if [[ "$exit_code" -ne 0 ]]; then
        printf "\n" >&2
        error "Script failed with exit code $exit_code (${total_elapsed}s)"
        error "Check logs in $LOGS_DIR/ for details"
    else
        info "Total runtime: $((total_elapsed/60))m$((total_elapsed%60))s"
    fi
    exit "$exit_code"
}
trap cleanup EXIT INT TERM

# ── Argument parsing ─────────────────────────────────────────────────────
usage() {
    cat <<'EOF'
PatternFly Migration Tools

Usage: ./run.sh [OPTIONS]

Options:
  --migrate <PATH>           Migrate the project at PATH
  --generate-rules           Generate new PatternFly rules
  --agent <NAME>             Agent: goose (default), claude, opencode
  --rules-dir <PATH>         Custom rules directory
  --llm-timeout <SECS>       LLM timeout (default: 300)
  --from <REF>               --from for rule generation
  --to <REF>                 --to for rule generation
  --dep-from <REF>           --dep-from for rule generation
  --dep-to <REF>             --dep-to for rule generation
  --base-branch <NAME>       Base branch to create migration branch from (default: main)
  --skip-agent               Skip AI agent step (Phase 2)

  Per-Ref Build (rule generation):
  --from-node-version <V>    Node version for --from ref
  --to-node-version <V>      Node version for --to ref
  --from-install-command <C>  Install command for --from ref
  --to-install-command <C>    Install command for --to ref
  --from-build-command <C>    Build command for --from ref
  --to-build-command <C>      Build command for --to ref

  --non-interactive          Skip all prompts
  -h, --help                 Show help
EOF
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --migrate)         MODE="migrate"; MIGRATE_PATH="$2"; shift 2 ;;
        --generate-rules)  MODE="generate-rules"; shift ;;
        --agent)           AGENT="$2"; shift 2 ;;
        --rules-dir)       CUSTOM_RULES_DIR="$2"; shift 2 ;;
        --llm-timeout)     LLM_TIMEOUT="$2"; shift 2 ;;
        --from)            GEN_FROM="$2"; shift 2 ;;
        --to)              GEN_TO="$2"; shift 2 ;;
        --dep-from)        GEN_DEP_FROM="$2"; shift 2 ;;
        --dep-to)          GEN_DEP_TO="$2"; shift 2 ;;
        --base-branch)     BASE_BRANCH="$2"; shift 2 ;;
        --skip-agent)      SKIP_AGENT=true; shift ;;
        --from-node-version)    GEN_FROM_NODE_VERSION="$2"; shift 2 ;;
        --to-node-version)      GEN_TO_NODE_VERSION="$2"; shift 2 ;;
        --from-install-command) GEN_FROM_INSTALL_CMD="$2"; shift 2 ;;
        --to-install-command)   GEN_TO_INSTALL_CMD="$2"; shift 2 ;;
        --from-build-command)   GEN_FROM_BUILD_CMD="$2"; shift 2 ;;
        --to-build-command)     GEN_TO_BUILD_CMD="$2"; shift 2 ;;
        --non-interactive) NON_INTERACTIVE=true; shift ;;
        -h|--help)         usage ;;
        *)                 die "Unknown option: $1" ;;
    esac
done

case "${AGENT}" in
    goose|claude|opencode) ;;
    *) die "Invalid agent: $AGENT. Must be goose, claude, or opencode" ;;
esac

# ── Interactive menu ─────────────────────────────────────────────────────
show_menu() {
    printf "\n${BOLD}PatternFly Migration Tools${NC}\n"
    printf "==========================\n\n"
    printf "  1. Migrate a PatternFly project\n"
    printf "  2. Generate PatternFly rules (optional)\n\n"
    local choice
    printf "${BOLD}Choose an option${NC} [1]: "
    read -r choice
    case "${choice:-1}" in
        1) MODE="migrate" ;;
        2) MODE="generate-rules" ;;
        *) die "Invalid choice: $choice" ;;
    esac
}

# ── Migration functions ──────────────────────────────────────────────────
resolve_rules_dir() {
    if [[ -n "$CUSTOM_RULES_DIR" ]]; then
        RULES_PATH="$CUSTOM_RULES_DIR"
        info "Using custom rules from: $RULES_PATH"
        return
    fi

    if [[ -f "$PWD/.semver_runner" ]]; then
        local saved_path
        saved_path="$(cat "$PWD/.semver_runner")"
        if [[ -d "$saved_path/semver_rules" ]]; then
            if [[ "$NON_INTERACTIVE" == true ]]; then
                RULES_PATH="$saved_path"
                info "Using rules from .semver_runner: $RULES_PATH"
                return
            fi
            local use_saved
            use_saved=$(prompt_choice "Found generated rules at $saved_path. Use these? (y/n)" "y")
            if [[ "$use_saved" == "y" || "$use_saved" == "Y" ]]; then
                RULES_PATH="$saved_path"
                info "Using generated rules from: $RULES_PATH"
                return
            fi
        fi
    fi

    RULES_PATH="$RULES_DIR"
    info "Using pre-packaged rules"
}

prompt_migrate_options() {
    if [[ -z "$MIGRATE_PATH" ]]; then
        printf "${BOLD}Enter project path to migrate:${NC} "
        read -r MIGRATE_PATH
        [[ -z "$MIGRATE_PATH" ]] && die "Project path is required"
    fi

    MIGRATE_PATH="$(cd "$MIGRATE_PATH" 2>/dev/null && pwd)" || die "Invalid project path: $MIGRATE_PATH"
    [[ -d "$MIGRATE_PATH" ]] || die "Not a directory: $MIGRATE_PATH"

    resolve_rules_dir

    if [[ "$NON_INTERACTIVE" != true ]]; then
        printf "\n${BOLD}Select AI agent for final fix pass:${NC}\n" >&2
        AGENT=$(prompt_select "Choose agent" "Goose" "Claude Code" "OpenCode")
        case "$AGENT" in
            "Goose")       AGENT="goose" ;;
            "Claude Code") AGENT="claude" ;;
            "OpenCode")    AGENT="opencode" ;;
        esac
    fi
}

generate_provider_settings() {
    local target_dir="$1" output_file="$2"
    cat > "$output_file" <<PSJSON
[
    {
        "name": "frontend",
        "address": "localhost:$PROVIDER_PORT",
        "initConfig": [
            {
                "location": "$target_dir"
            }
        ]
    },
    {
        "name": "builtin",
        "initConfig": [
            {
                "location": "$target_dir"
            }
        ]
    }
]
PSJSON
}

start_provider() {
    pkill frontend-analyzer-provider 2>/dev/null || true
    sleep 1

    info "Running '$FAP_BIN serve -p $PROVIDER_PORT'"
    "$FAP_BIN" serve -p "$PROVIDER_PORT" > "$LOGS_DIR/provider.log" 2>&1 &
    PROVIDER_PID=$!

    local attempts=0
    while [[ $attempts -lt 30 ]]; do
        if ! kill -0 "$PROVIDER_PID" 2>/dev/null; then
            die "frontend-analyzer-provider exited unexpectedly. Check $LOGS_DIR/provider.log"
        fi
        if lsof -i ":$PROVIDER_PORT" >/dev/null 2>&1 || nc -z localhost "$PROVIDER_PORT" 2>/dev/null; then
            break
        fi
        sleep 0.5
        attempts=$((attempts + 1))
    done

    info "frontend-analyzer-provider started (PID $PROVIDER_PID, port $PROVIDER_PORT)"
}

stop_provider() {
    if [[ -n "${PROVIDER_PID:-}" ]]; then
        kill "$PROVIDER_PID" 2>/dev/null || true
        wait "$PROVIDER_PID" 2>/dev/null || true
        PROVIDER_PID=""
        info "frontend-analyzer-provider stopped"
    fi
}

yaml_to_json() {
    local input="$1" output="$2"
    if command -v yq >/dev/null 2>&1; then
        yq -o=json '.' "$input" > "$output" || die "yq YAML-to-JSON conversion failed"
    elif command -v python3 >/dev/null 2>&1; then
        python3 -c "
import yaml, json
with open('$input') as f:
    data = yaml.safe_load(f)
with open('$output', 'w') as f:
    json.dump(data, f, indent=2)
" || die "python3 YAML-to-JSON conversion failed"
    else
        die "Neither yq nor python3 (with PyYAML) found. Install one for YAML-to-JSON conversion."
    fi
}

run_agent() {
    local project_dir="$1"
    require_file "$PROMPT_FILE"

    pushd "$project_dir" > /dev/null || die "Failed to cd into $project_dir"

    case "$AGENT" in
        goose)
            require_command goose
            run_timed "Goose agent" "$LOGS_DIR/agent-goose.log" \
                env GOOSE_MODE=auto goose run -i "$PROMPT_FILE" || {
                warn "goose exited with non-zero status. Check $LOGS_DIR/agent-goose.log"
            }
            ;;
        claude)
            require_command claude
            run_timed "Claude Code agent" "$LOGS_DIR/agent-claude.log" \
                claude --allowedTools "Bash" "Edit" "Write" "Read" "WebSearch" "WebFetch" -p "$(cat "$PROMPT_FILE")" || {
                warn "claude exited with non-zero status. Check $LOGS_DIR/agent-claude.log"
            }
            ;;
        opencode)
            require_command opencode
            run_timed "OpenCode agent" "$LOGS_DIR/agent-opencode.log" \
                opencode run "$(cat "$PROMPT_FILE")" || {
                warn "opencode exited with non-zero status. Check $LOGS_DIR/agent-opencode.log"
            }
            ;;
    esac

    popd > /dev/null
}

check_migration_prerequisites() {
    local errors=()

    case "$AGENT" in
        goose)
            if ! command -v goose >/dev/null 2>&1; then
                errors+=("goose not found. Install from https://github.com/block/goose")
            fi ;;
        claude)
            if ! command -v claude >/dev/null 2>&1; then
                errors+=("claude not found. Install from https://docs.anthropic.com/en/docs/claude-code")
            fi ;;
        opencode)
            if ! command -v opencode >/dev/null 2>&1; then
                errors+=("opencode not found. Install from https://github.com/opencode-ai/opencode")
            fi ;;
    esac

    if ! command -v yq >/dev/null 2>&1 && ! command -v python3 >/dev/null 2>&1; then
        errors+=("Neither yq nor python3 found. One is required for YAML-to-JSON conversion.")
    fi

    if [[ ${#errors[@]} -gt 0 ]]; then
        error "Missing prerequisites:"
        for e in "${errors[@]}"; do
            error "  - $e"
        done
        exit 1
    fi
}

run_migration() {
    check_migration_prerequisites

    local total=8
    TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/pf-migrate.XXXXXX")
    mkdir -p "$LOGS_DIR" "$TEMP_DIR/kantra"

    local migration_branch

    if [[ -n "$EVAL_ONLY_BRANCH" ]]; then
        migration_branch="$EVAL_ONLY_BRANCH"
        info "Eval-only mode: using existing branch $migration_branch"
        info "Project:   $MIGRATE_PATH"
        info "Branch:    $migration_branch"
    else
        # Create migration branch from base
        migration_branch="semver/goose/$(date -u +%m%d%y-%H%M)"
        info "Base branch: $BASE_BRANCH"
        info "Creating migration branch: $migration_branch"
        (cd "$MIGRATE_PATH" \
            && git checkout "$BASE_BRANCH" \
            && git checkout -b "$migration_branch") \
            || die "Failed to create migration branch. Ensure '$BASE_BRANCH' exists in $MIGRATE_PATH"

        info "Project:   $MIGRATE_PATH"
        info "Branch:    $migration_branch"
        info "Rules:     $RULES_PATH"
        info "Agent:     $AGENT"
        info "Temp dir:  $TEMP_DIR"
    fi

    local kantra_rules_dir="$RULES_PATH"

    local provider_settings="$TEMP_DIR/provider_settings.json"
    local kantra_yaml="$TEMP_DIR/kantra/output.yaml"
    local kantra_json="$TEMP_DIR/kantra/output.json"
    # Collect all fix-guidance JSON files as --strategies flags
    local strategies_args=()
    while IFS= read -r -d '' f; do
        strategies_args+=(--strategies "$f")
    done < <(find "$STRATEGIES_DIR" -name "*.json" -path "*/fix-guidance/*" -print0 2>/dev/null || true)

    local phase1_start=$SECONDS
    local phase1_secs=0
    local phase2_secs=0

    # ── Phase 1: Automated analysis and fixes ──
    if confirm_step "Phase 1: Run automated analysis and fixes?"; then

        step "1/$total" "Generating provider settings"
        generate_provider_settings "$MIGRATE_PATH" "$provider_settings"

        step "2/$total" "Starting frontend-analyzer-provider"
        start_provider

        step "3/$total" "Running kantra analysis"
        export KANTRA_DIR="$KANTRA_DIR"
        run_timed "Kantra analysis" "$LOGS_DIR/kantra.log" \
            "$KANTRA_BIN" analyze \
            --input "$MIGRATE_PATH" \
            --output "$TEMP_DIR/kantra" \
            --rules "$kantra_rules_dir" \
            --override-provider-settings "$provider_settings" \
            --enable-default-rulesets=false \
            --run-local \
            --overwrite || {
                error "kantra analysis failed. Check $LOGS_DIR/kantra.log"
                stop_provider
                return 1
            }

        step "4/$total" "Stopping frontend-analyzer-provider"
        stop_provider

        step "5/$total" "Converting kantra output to JSON"
        require_file "$kantra_yaml"
        yaml_to_json "$kantra_yaml" "$kantra_json"
        info "Converted: $kantra_json"

        step "6/$total" "Applying pattern-based fixes"

        run_timed "Pattern-based fixes" "$LOGS_DIR/fix-pattern.log" \
            unbuffer "$FIX_BIN" fix "$MIGRATE_PATH" \
            --input "$kantra_json" \
            --log-dir "$LOGS_DIR/fix-debug" \
            "${strategies_args[@]}" || {
                die "Pattern-based fix failed. Check $LOGS_DIR/fix-pattern.log"
            }

        step "7/$total" "Applying LLM-based fixes"
        run_timed "LLM-based fixes" "$LOGS_DIR/fix-llm.log" \
            unbuffer "$FIX_BIN" fix "$MIGRATE_PATH" \
            --input "$kantra_json" \
            --llm-provider goose \
            --goose-timeout "$LLM_TIMEOUT" \
            --log-dir "$LOGS_DIR/fix-debug" \
            "${strategies_args[@]}" || {
                warn "LLM-based fix returned non-zero (some or all fixes may have failed). Check $LOGS_DIR/fix-llm.log"
            }

        # Commit automated fixes
        (cd "$MIGRATE_PATH" && \
            git add -A && git reset HEAD -- .goosehints progress.md 2>/dev/null; \
            git diff --cached --quiet || \
            git commit -m "Apply automated migration fixes (pattern-based + LLM)") \
            > /dev/null 2>&1 || true
        info "Committed automated fixes"
        phase1_secs=$(($SECONDS - phase1_start))

    else
        info "Skipping Phase 1"
    fi

    local phase2_start=$SECONDS

    # ── Phase 2: AI agent ──
    if [[ "$SKIP_AGENT" == true ]]; then
        info "Skipping AI agent (--skip-agent)"
    elif confirm_step "Phase 2: Run AI agent ($AGENT) for remaining fixes?"; then
        step "8/$total" "Running $AGENT for remaining fixes"
        if [[ "${DISABLE_MEMORY:-}" != "1" ]] && [[ -f "$GOOSEHINTS_SRC" ]]; then
            cp "$GOOSEHINTS_SRC" "$MIGRATE_PATH/.goosehints"
        fi
        run_agent "$MIGRATE_PATH"

        # Commit AI agent fixes
        (cd "$MIGRATE_PATH" && \
            git add -A && git reset HEAD -- .goosehints progress.md 2>/dev/null; \
            git diff --cached --quiet || \
            git commit -m "Apply AI agent fixes ($AGENT)") \
            > /dev/null 2>&1 || true
        info "Committed AI agent fixes"
        phase2_secs=$(($SECONDS - phase2_start))
    else
        info "Skipping Phase 2"
    fi

    # ── Write stats.json ──
    local total_secs=$(($SECONDS - GLOBAL_START))
    local stats_dir="$MIGRATE_PATH/.pf-migration"
    mkdir -p "$stats_dir"
    echo "*" > "$stats_dir/.gitignore"

    # Collect token usage from goose sessions DB
    local tokens_json="{}"
    if command -v npx >/dev/null 2>&1; then
        tokens_json=$(npx --yes tokscale@latest -c goose --json 2>/dev/null | python3 -c "
import sys, json
raw = sys.stdin.read()
start = raw.find('{')
if start >= 0:
    try:
        obj = json.loads(raw[start:])
        print(json.dumps(obj))
    except: print('{}')
else: print('{}')
" 2>/dev/null || echo "{}")
    fi

    python3 -c "
import json, sys
tokens_raw = sys.argv[1]
try:
    tokens = json.loads(tokens_raw)
except:
    tokens = {}
stats = {
    'migration': {
        'branch': sys.argv[2],
        'base_branch': sys.argv[3],
        'timestamp': sys.argv[4]
    },
    'timing': {
        'phase1_secs': int(sys.argv[5]),
        'phase2_secs': int(sys.argv[6]),
        'total_secs': int(sys.argv[7])
    },
    'tokens': tokens
}
with open(sys.argv[8], 'w') as f:
    json.dump(stats, f, indent=2)
" "$tokens_json" \
  "$migration_branch" \
  "$BASE_BRANCH" \
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  "$phase1_secs" \
  "$phase2_secs" \
  "$total_secs" \
  "$stats_dir/stats.json" 2>/dev/null || true

    if [[ -f "$stats_dir/stats.json" ]]; then
        info "Stats written to: $stats_dir/stats.json"
    fi

    printf "\n"
    info "Migration complete!"
    info "Project: $MIGRATE_PATH"
    info "Kantra output: $TEMP_DIR/kantra/"
    info "Logs: $LOGS_DIR/"
}

# ── Rule generation functions ────────────────────────────────────────────
check_generate_prerequisites() {
    local errors=()

    if ! command -v git >/dev/null 2>&1; then
        errors+=("git not found. Install git.")
    fi

    local nvm_dir="${NVM_DIR:-$HOME/.nvm}"
    if [[ ! -f "$nvm_dir/nvm.sh" ]]; then
        errors+=("nvm not found. Install from https://github.com/nvm-sh/nvm")
    fi

    if [[ ! -f "$SEMVER_BIN" ]]; then
        errors+=("semver-analyzer binary not found at $SEMVER_BIN")
    fi

    if [[ ${#errors[@]} -gt 0 ]]; then
        error "Missing prerequisites for rule generation:"
        for e in "${errors[@]}"; do
            error "  - $e"
        done
        exit 1
    fi
}

prompt_tag_selection() {
    local repo_path="$1" prefix="$2" label="$3" count="${4:-3}"
    local tag_list
    tag_list=$(cd "$repo_path" && git tag -l "${prefix}*" --sort=-v:refname | head -n "$count")

    if [[ -z "$tag_list" ]]; then
        die "No tags matching '${prefix}*' found in $repo_path"
    fi

    printf "\n${BOLD}Select $label:${NC}\n" >&2
    # shellcheck disable=SC2086
    prompt_select "Choose tag" $tag_list
}

run_generate_rules() {
    local total=4
    check_generate_prerequisites

    TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/pf-rules.XXXXXX")
    mkdir -p "$LOGS_DIR"

    echo "$TEMP_DIR" > "$PWD/.semver_runner"
    info "Temp directory: $TEMP_DIR"
    info "Path saved to .semver_runner"

    # Step 1
    step "1/$total" "Cloning repositories"
    info "Cloning patternfly-react..."
    git clone https://github.com/patternfly/patternfly-react.git "$TEMP_DIR/patternfly-react" \
        > "$LOGS_DIR/clone-pf-react.log" 2>&1 \
        || die "Failed to clone patternfly-react. Check $LOGS_DIR/clone-pf-react.log"

    info "Cloning patternfly..."
    git clone https://github.com/patternfly/patternfly.git "$TEMP_DIR/patternfly" \
        > "$LOGS_DIR/clone-pf.log" 2>&1 \
        || die "Failed to clone patternfly. Check $LOGS_DIR/clone-pf.log"

    # Step 2
    step "2/$total" "Selecting version tags"
    local pf_from pf_to dep_from dep_to

    if [[ -n "$GEN_FROM" ]]; then pf_from="$GEN_FROM"
    elif [[ "$NON_INTERACTIVE" == true ]]; then die "--from is required in non-interactive mode"
    else pf_from=$(prompt_tag_selection "$TEMP_DIR/patternfly-react" "v5." "patternfly-react --from tag"); fi

    if [[ -n "$GEN_TO" ]]; then pf_to="$GEN_TO"
    elif [[ "$NON_INTERACTIVE" == true ]]; then die "--to is required in non-interactive mode"
    else pf_to=$(prompt_tag_selection "$TEMP_DIR/patternfly-react" "v6." "patternfly-react --to tag"); fi

    if [[ -n "$GEN_DEP_FROM" ]]; then dep_from="$GEN_DEP_FROM"
    elif [[ "$NON_INTERACTIVE" == true ]]; then die "--dep-from is required in non-interactive mode"
    else dep_from=$(prompt_tag_selection "$TEMP_DIR/patternfly" "v5." "patternfly --dep-from tag"); fi

    if [[ -n "$GEN_DEP_TO" ]]; then dep_to="$GEN_DEP_TO"
    elif [[ "$NON_INTERACTIVE" == true ]]; then die "--dep-to is required in non-interactive mode"
    else dep_to=$(prompt_tag_selection "$TEMP_DIR/patternfly" "v6." "patternfly --dep-to tag"); fi

    info "Analysis range: patternfly-react $pf_from -> $pf_to"
    info "Dependency range: patternfly $dep_from -> $dep_to"

    # Step 3
    step "3/$total" "Running semver-analyzer analyze"
    local dep_build_cmd="source ~/.nvm/nvm.sh && nvm exec 20.11.0 bash -c 'export NODE_ENV=development && yarn install && npx gulp buildPatternfly'"

    local per_ref_args=()
    [[ -n "$GEN_FROM_NODE_VERSION" ]] && per_ref_args+=(--from-node-version "$GEN_FROM_NODE_VERSION")
    [[ -n "$GEN_TO_NODE_VERSION" ]]   && per_ref_args+=(--to-node-version "$GEN_TO_NODE_VERSION")
    [[ -n "$GEN_FROM_INSTALL_CMD" ]]  && per_ref_args+=(--from-install-command "$GEN_FROM_INSTALL_CMD")
    [[ -n "$GEN_TO_INSTALL_CMD" ]]    && per_ref_args+=(--to-install-command "$GEN_TO_INSTALL_CMD")
    [[ -n "$GEN_FROM_BUILD_CMD" ]]    && per_ref_args+=(--from-build-command "$GEN_FROM_BUILD_CMD")
    [[ -n "$GEN_TO_BUILD_CMD" ]]      && per_ref_args+=(--to-build-command "$GEN_TO_BUILD_CMD")

    run_timed "Semver analysis" "$LOGS_DIR/semver_analyze.stdout" \
        "$SEMVER_BIN" analyze typescript \
        --repo "$TEMP_DIR/patternfly-react" \
        --from "$pf_from" \
        --to "$pf_to" \
        --dep-repo "$TEMP_DIR/patternfly" \
        --dep-from "$dep_from" \
        --dep-to "$dep_to" \
        --dep-build-command "$dep_build_cmd" \
        --build-command 'corepack yarn build' \
        "${per_ref_args[@]}" \
        --no-llm \
        --log-file "$LOGS_DIR/semver_analyze.log" \
        --log-level info \
        -o "$TEMP_DIR/semver_report.json" || {
            die "semver-analyzer analyze failed. Check $LOGS_DIR/semver_analyze.log"
        }
    info "Report: $TEMP_DIR/semver_report.json"

    # Step 4
    step "4/$total" "Generating Konveyor rules"
    mkdir -p "$TEMP_DIR/semver_rules"

    local rename_flag=""
    if [[ -f "$TOKEN_MAPPINGS" ]]; then
        rename_flag="--rename-patterns $TOKEN_MAPPINGS"
    fi

    run_timed "Rule generation" "$LOGS_DIR/semver_konveyor.stdout" \
        "$SEMVER_BIN" konveyor typescript \
        --from-report "$TEMP_DIR/semver_report.json" \
        --output-dir "$TEMP_DIR/semver_rules" \
        --log-file "$LOGS_DIR/semver_konveyor.log" \
        --log-level info \
        $rename_flag || {
            die "semver-analyzer konveyor failed. Check $LOGS_DIR/semver_konveyor.log"
        }

    printf "\n"
    info "Rule generation complete!"
    info "Rules:  $TEMP_DIR/semver_rules/"
    info "Report: $TEMP_DIR/semver_report.json"
    info "Path saved to .semver_runner for use with --migrate"
}

# ── Main ─────────────────────────────────────────────────────────────────
check_archive_integrity() {
    local missing=()
    for f in "$SEMVER_BIN" "$FAP_BIN" "$FIX_BIN" "$KANTRA_BIN"; do
        [[ -f "$f" ]] || missing+=("$f")
    done
    [[ -f "$TOKEN_MAPPINGS" ]] || missing+=("$TOKEN_MAPPINGS")
    [[ -f "$PROMPT_FILE" ]] || missing+=("$PROMPT_FILE")
    [[ -d "$RULES_DIR" ]] || missing+=("$RULES_DIR/")

    if [[ ${#missing[@]} -gt 0 ]]; then
        error "Archive is incomplete. Missing files:"
        for f in "${missing[@]}"; do
            error "  $f"
        done
        die "Re-run build.sh to generate a complete archive."
    fi
}

main() {
    require_command unbuffer
    check_archive_integrity
    mkdir -p "$LOGS_DIR"

    if [[ -z "$MODE" ]]; then
        if [[ "$NON_INTERACTIVE" == true ]]; then
            die "Must specify --migrate or --generate-rules in non-interactive mode"
        fi
        show_menu
    fi

    case "$MODE" in
        migrate)
            prompt_migrate_options
            run_migration
            ;;
        generate-rules)
            run_generate_rules
            ;;
        *) die "Unknown mode: $MODE" ;;
    esac
}

main
