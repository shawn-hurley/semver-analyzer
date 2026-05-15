#!/usr/bin/env bash
set -eo pipefail

# ── Colors ───────────────────────────────────────────────────────────────
if [[ -z "${NO_COLOR:-}" ]] && [[ -t 1 ]]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
    BOLD='\033[1m'; NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; BOLD=''; NC=''
fi

info()  { printf "${GREEN}[INFO]${NC}  %s\n" "$*"; }
warn()  { printf "${YELLOW}[WARN]${NC}  %s\n" "$*"; }
error() { printf "${RED}[ERROR]${NC} %s\n" "$*" >&2; }
die()   { error "$@"; exit 1; }

# ── Cleanup ──────────────────────────────────────────────────────────────
CONTAINER_IDS=""
BAKE_IMAGES=""
TEMP_FILES=""

cleanup() {
    local exit_code=$?
    if [[ "$exit_code" -ne 0 ]]; then
        printf "\n" >&2
        error "run_container.sh failed with exit code $exit_code"
        if [[ -n "$CONTAINER_IDS" ]]; then
            warn "Containers kept for debugging: $CONTAINER_IDS"
            warn "Inspect: $RUNTIME logs <container_id>"
            warn "Shell:   $RUNTIME exec -it <container_id> bash"
            warn "Remove:  $RUNTIME rm -f <container_id>"
        fi
    elif [[ "$KEEP_CONTAINER" == true ]]; then
        if [[ -n "$CONTAINER_IDS" ]]; then
            info "Containers kept (--keep): $CONTAINER_IDS"
            info "Shell:   $RUNTIME exec -it <container_id> bash"
            info "Remove:  $RUNTIME rm -f <container_id>"
        fi
    else
        for cid in $CONTAINER_IDS; do
            "$RUNTIME" stop "$cid" > /dev/null 2>&1 || true
            "$RUNTIME" rm "$cid" > /dev/null 2>&1 || true
        done
        for img in $BAKE_IMAGES; do
            "$RUNTIME" rmi "$img" > /dev/null 2>&1 || true
        done
    fi
    # Always clean temp files
    for f in $TEMP_FILES; do
        rm -f "$f" 2>/dev/null || true
    done
}
trap cleanup EXIT INT TERM

track_container() {
    CONTAINER_IDS="$CONTAINER_IDS $1"
}

track_bake_image() {
    BAKE_IMAGES="$BAKE_IMAGES $1"
}

track_temp_file() {
    TEMP_FILES="$TEMP_FILES $1"
}

# ── Defaults ─────────────────────────────────────────────────────────────
DEFAULT_IMAGE="quay.io/pranavgaikwad/patternfly-tools:latest"
CONTAINER_WORKSPACE="/workspace"

MODE="mount"
IMAGE="$DEFAULT_IMAGE"
GOOSE_CONFIG=""
APP_PATH=""
ENABLE_EVAL=false
EVAL_ONLY_BRANCH=""
BASE_BRANCH="main"
AGENT="goose"
KEEP_CONTAINER=false
NO_MEMORY=false
PASSTHROUGH_ARGS=()

# ── Usage ────────────────────────────────────────────────────────────────
usage() {
    cat <<'EOF'
PatternFly Migration Tools — Container Runner

Usage: ./run_container.sh --migrate <PATH> [OPTIONS]

Required:
  --migrate <PATH>           Path to the application to migrate

Container options:
  --bake                     Bake app into image instead of mounting (for slow mounts)
  --goose-config <PATH>      Override goose config directory
  --image <NAME>             Container image (default: quay.io/pranavgaikwad/patternfly-tools:latest)
  --keep                     Keep container after completion (for debugging)
  --no-memory                Disable memory extension and skip memory volume mount
  --log-dir <PATH>           Directory to sync logs to (default: $PWD/.pf-migration-logs)

Evaluation options:
  --enable-eval              Run evaluation after migration
  --eval-only <BRANCH>       Run evaluation only against an existing migrated branch (skips migration)

Migration options (forwarded to run.sh):
  --base-branch <NAME>       Branch of the application to migrate (default: main)
  --llm-timeout <SECS>       LLM timeout (default: 300)
  --non-interactive          Skip all prompts

  -h, --help                 Show help
EOF
    exit 0
}

# ── Container runtime detection ──────────────────────────────────────────
detect_runtime() {
    if command -v podman >/dev/null 2>&1; then
        echo "podman"
    elif command -v docker >/dev/null 2>&1; then
        echo "docker"
    else
        die "Neither podman nor docker found. Install one to run the container."
    fi
}

# ── Argument parsing ─────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --migrate)        APP_PATH="$2"; shift 2 ;;
        --bake)           MODE="bake"; shift ;;
        --goose-config)   GOOSE_CONFIG="$2"; shift 2 ;;
        --image)          IMAGE="$2"; shift 2 ;;
        --enable-eval)    ENABLE_EVAL=true; shift ;;
        --keep)           KEEP_CONTAINER=true; shift ;;
        --no-memory)      NO_MEMORY=true; shift ;;
        --log-dir)        LOGS_DEST="$2"; shift 2 ;;
        --eval-only)      ENABLE_EVAL=true; EVAL_ONLY_BRANCH="$2"; shift 2 ;;
        --base-branch)    BASE_BRANCH="$2"; PASSTHROUGH_ARGS+=("--base-branch" "$2"); shift 2 ;;
        --agent)          AGENT="$2"; PASSTHROUGH_ARGS+=("--agent" "$2"); shift 2 ;;
        -h|--help)        usage ;;
        --)               shift; PASSTHROUGH_ARGS+=("$@"); break ;;
        *)                PASSTHROUGH_ARGS+=("$1"); shift ;;
    esac
done

# ── Validation ───────────────────────────────────────────────────────────
[[ -z "$APP_PATH" ]] && die "Missing required --migrate <PATH>"
[[ -d "$APP_PATH" ]] || die "Not a directory: $APP_PATH"
APP_PATH="$(cd "$APP_PATH" && pwd)"

if [[ "${GOOSE_PROVIDER:-gcp_vertex_ai}" == "gcp_vertex_ai" ]]; then
    [[ -z "${GCP_PROJECT_ID:-}" ]] && die "GCP_PROJECT_ID is not set. Export it before running (e.g., export GCP_PROJECT_ID=my-project)"
    [[ -z "${GCP_LOCATION:-}" ]] && die "GCP_LOCATION is not set. Export it before running (e.g., export GCP_LOCATION=us-east5)"
fi

RUNTIME=$(detect_runtime)
info "Container runtime: $RUNTIME"
info "Mode: $MODE"
info "Image: $IMAGE"
info "App: $APP_PATH"

# ── Goose config ─────────────────────────────────────────────────────────
MOUNT_ARGS=()
if [[ -n "$GOOSE_CONFIG" ]]; then
    if [[ -d "$GOOSE_CONFIG" ]]; then
        MOUNT_ARGS+=(-v "$GOOSE_CONFIG:/root/.config/goose:z")
        info "Goose config: $GOOSE_CONFIG (mounted)"
    else
        die "Goose config directory not found: $GOOSE_CONFIG"
    fi
else
    info "Goose config: using default (baked into image)"
fi

# ── GCP credentials ─────────────────────────────────────────────────────
GCP_CREDS_DIR="$HOME/.config/gcloud"
if [[ -d "$GCP_CREDS_DIR" ]]; then
    MOUNT_ARGS+=(-v "$GCP_CREDS_DIR:/root/.config/gcloud:ro,z")
    info "GCP credentials: $GCP_CREDS_DIR"
fi

# ── Environment variable passthrough ─────────────────────────────────────
ENV_ARGS=()
for var in GOOSE_PROVIDER GOOSE_MODEL GOOSE_API_KEY \
           ANTHROPIC_API_KEY OPENAI_API_KEY GOOGLE_API_KEY \
           GCP_PROJECT_ID GCP_LOCATION; do
    if [[ -n "${!var:-}" ]]; then
        ENV_ARGS+=(-e "$var=${!var}")
    fi
done

# Point GOOGLE_APPLICATION_CREDENTIALS to the mounted path inside the container
if [[ -d "$GCP_CREDS_DIR" ]]; then
    ENV_ARGS+=(-e "GOOGLE_APPLICATION_CREDENTIALS=/root/.config/gcloud/application_default_credentials.json")
fi

# MemPalace persistence volume
if [[ "$NO_MEMORY" == true ]]; then
    ENV_ARGS+=(-e "DISABLE_MEMORY=1")
    info "Memory: disabled"
else
    MOUNT_ARGS+=(-v "mempalace-data:/root/.mempalace:z")
    info "Memory: enabled (mempalace-data volume)"
fi

# ── Paths inside container ────────────────────────────────────────────────
CONTAINER_LOGS="/opt/patternfly-tools/logs"
LOGS_DEST="${LOGS_DEST:-$PWD/.pf-migration-logs}"

# ── Mode: Mount ──────────────────────────────────────────────────────────
run_mount_mode() {
    info "Mounting $APP_PATH at $CONTAINER_WORKSPACE"

    mkdir -p "$LOGS_DEST"

    local container_id
    container_id=$("$RUNTIME" run -d \
        -v "$APP_PATH:$CONTAINER_WORKSPACE:z" \
        -v "$LOGS_DEST:$CONTAINER_LOGS:z" \
        "${MOUNT_ARGS[@]}" \
        "${ENV_ARGS[@]}" \
        "$IMAGE" \
        --migrate "$CONTAINER_WORKSPACE" \
        --non-interactive \
        "${PASSTHROUGH_ARGS[@]}")
    track_container "$container_id"

    info "Container: $container_id"
    "$RUNTIME" logs -f "$container_id" || true
    "$RUNTIME" wait "$container_id" > /dev/null 2>&1 || true

    info "Results in: $APP_PATH"
    info "Logs in: $LOGS_DEST/"
}

# ── Mode: Bake ───────────────────────────────────────────────────────────
run_bake_mode() {
    local bake_tag="pf-baked-$(date +%s)"
    local temp_containerfile
    temp_containerfile=$(mktemp /tmp/pf-bake-XXXXXX)
    track_temp_file "$temp_containerfile"

    cat > "$temp_containerfile" <<EOF
FROM $IMAGE
COPY . $CONTAINER_WORKSPACE
RUN rm -f $CONTAINER_WORKSPACE/.git/index.lock
EOF

    info "Building baked image: $bake_tag"
    "$RUNTIME" build -t "$bake_tag" -f "$temp_containerfile" "$APP_PATH" \
        || die "Failed to build baked image"
    track_bake_image "$bake_tag"

    info "Running migration in baked image"
    local container_id
    container_id=$("$RUNTIME" run -d \
        "${MOUNT_ARGS[@]}" \
        "${ENV_ARGS[@]}" \
        "$bake_tag" \
        --migrate "$CONTAINER_WORKSPACE" \
        --non-interactive \
        "${PASSTHROUGH_ARGS[@]}")
    track_container "$container_id"

    info "Container: $container_id"
    "$RUNTIME" logs -f "$container_id" || true
    local container_exit=0
    "$RUNTIME" wait "$container_id" > /dev/null 2>&1 || container_exit=$?

    # Always sync results and logs, even on failure
    info "Syncing results from container"
    "$RUNTIME" cp "$container_id:$CONTAINER_WORKSPACE/." "$APP_PATH/" 2>/dev/null || true

    mkdir -p "$LOGS_DEST"
    "$RUNTIME" cp "$container_id:$CONTAINER_LOGS/." "$LOGS_DEST/" 2>/dev/null || true

    info "Results in: $APP_PATH"
    info "Logs in: $LOGS_DEST/"
}

# ── Run eval inside container ────────────────────────────────────────────
run_eval() {
    local branch="$1"
    info "Running evaluation for branch: $branch"

    local container_id
    mkdir -p "$LOGS_DEST"

    container_id=$("$RUNTIME" run -d \
        -v "$APP_PATH:$CONTAINER_WORKSPACE:z" \
        -v "$LOGS_DEST:$CONTAINER_LOGS:z" \
        "${MOUNT_ARGS[@]}" \
        "${ENV_ARGS[@]}" \
        --entrypoint /opt/patternfly-tools/eval.sh \
        "$IMAGE" \
        --migrate "$CONTAINER_WORKSPACE" \
        --branch "$branch" \
        --base-branch "$BASE_BRANCH" \
        --agent "$AGENT" \
        --non-interactive)
    track_container "$container_id"

    info "Eval container: $container_id"
    "$RUNTIME" logs -f "$container_id" || true
    "$RUNTIME" wait "$container_id" > /dev/null 2>&1 || true

    info "Eval logs in: $LOGS_DEST/"
}

# ── Detect migration branch from run.sh output ──────────────────────────
detect_migration_branch() {
    (cd "$APP_PATH" && git branch --list 'semver/goose/*' --sort=-committerdate | head -1 | tr -d ' *')
}

# ── Main ─────────────────────────────────────────────────────────────────
if [[ -n "$EVAL_ONLY_BRANCH" ]]; then
    # Eval-only mode: skip migration, run eval directly
    run_eval "$EVAL_ONLY_BRANCH"
else
    # Run migration
    case "$MODE" in
        mount) run_mount_mode ;;
        bake)  run_bake_mode ;;
        *)     die "Unknown mode: $MODE" ;;
    esac

    # Run eval after migration if enabled
    if [[ "$ENABLE_EVAL" == true ]]; then
        local_branch=$(detect_migration_branch)
        if [[ -n "$local_branch" ]]; then
            run_eval "$local_branch"
        else
            warn "Could not detect migration branch for evaluation"
        fi
    fi
fi
