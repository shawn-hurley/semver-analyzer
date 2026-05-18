#!/usr/bin/env bash
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ── Colors ───────────────────────────────────────────────────────────────
if [[ -z "${NO_COLOR:-}" ]] && [[ -t 1 ]]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
    BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; BLUE=''; BOLD=''; NC=''
fi

# ── Platform lookups (bash 3.x compatible) ───────────────────────────────
platform_lookup() {
    local field="$1" platform="$2"
    case "$field:$platform" in
        kantra_suffix:Linux_x86)   echo "linux.amd64" ;;
        kantra_suffix:Linux_arm64) echo "linux.arm64" ;;
        kantra_suffix:Mac_x86)     echo "darwin.amd64" ;;
        kantra_suffix:Mac_arm64)   echo "darwin.arm64" ;;
        rust_target:Linux_x86)     echo "x86_64-unknown-linux-gnu" ;;
        rust_target:Linux_arm64)   echo "aarch64-unknown-linux-gnu" ;;
        rust_target:Mac_x86)       echo "x86_64-apple-darwin" ;;
        rust_target:Mac_arm64)     echo "aarch64-apple-darwin" ;;
        go_os:Linux_*)             echo "linux" ;;
        go_os:Mac_*)               echo "darwin" ;;
        go_arch:*_x86)             echo "amd64" ;;
        go_arch:*_arm64)           echo "arm64" ;;
        *) echo "" ;;
    esac
}

# ── Repo defaults ────────────────────────────────────────────────────────
KANTRA_REPO_URL="https://github.com/konveyor/kantra.git"
KANTRA_REPO_BRANCH="${KANTRA_REPO_BRANCH:-}"
SEMVER_REPO_URL="${SEMVER_REPO_URL:-https://github.com/konveyor-ecosystem/semver-analyzer.git}"
SEMVER_REPO_BRANCH="${SEMVER_REPO_BRANCH:-}"
KONVEYOR_CORE_REPO_URL="https://github.com/konveyor-ecosystem/konveyor-core.git"
KONVEYOR_CORE_REPO_BRANCH="${KONVEYOR_CORE_REPO_BRANCH:-}"
FAP_REPO_URL="https://github.com/konveyor-ecosystem/frontend-analyzer-provider.git"
FAP_REPO_BRANCH="${FAP_REPO_BRANCH:-}"
FIX_ENGINE_REPO_URL="https://github.com/konveyor-ecosystem/fix-engine.git"
FIX_ENGINE_REPO_BRANCH="${FIX_ENGINE_REPO_BRANCH:-}"
ANALYZER_LSP_REPO_URL="https://github.com/konveyor/analyzer-lsp.git"
ANALYZER_LSP_REPO_BRANCH="${ANALYZER_LSP_REPO_BRANCH:-}"
PF_REACT_REPO_URL="https://github.com/patternfly/patternfly-react.git"
PF_REACT_FROM="${PF_REACT_FROM:-v5.3.3}"
PF_REACT_TO="${PF_REACT_TO:-v6.4.1}"
PF_REPO_URL="https://github.com/patternfly/patternfly.git"
PF_DEP_FROM="${PF_DEP_FROM:-v5.4.0}"
PF_DEP_TO="${PF_DEP_TO:-v6.4.0}"
TOKEN_MAPPINGS_URL="https://raw.githubusercontent.com/konveyor-ecosystem/semver-analyzer/refs/heads/main/hack/integration/patternfly-token-mappings.yaml"

# PatternFly React Topology
TOPOLOGY_REPO_URL="https://github.com/patternfly/react-topology.git"
TOPOLOGY_FROM="${TOPOLOGY_FROM:-v5.4.1}"
TOPOLOGY_TO="${TOPOLOGY_TO:-v6.4.0}"
TOPOLOGY_INSTALL_CMD='npm install --ignore-scripts --legacy-peer-deps'
TOPOLOGY_BUILD_CMD='cd packages/module && npm run build'

# PatternFly React Component Groups
RCG_REPO_URL="https://github.com/patternfly/react-component-groups.git"
RCG_FROM="${RCG_FROM:-v5.5.3}"
RCG_TO="${RCG_TO:-v6.4.0}"
RCG_INSTALL_CMD='npm ci'
RCG_BUILD_CMD='npm run build'

# Dynamic Plugin SDK
SDK_REPO_URL="https://github.com/openshift/dynamic-plugin-sdk.git"
SDK_FROM_DATE="${SDK_FROM_DATE:-2023-04-13}"
SDK_TO_DATE="${SDK_TO_DATE:-2024-01-15}"
SDK_BUILD_CMD="yarn install && yarn build"

# Console SDK
CONSOLE_REPO_URL="https://github.com/openshift/console.git"
CONSOLE_FROM="${CONSOLE_FROM:-origin/release-4.17}"
CONSOLE_TO="${CONSOLE_TO:-origin/release-4.19}"
CONSOLE_SDK_FROM_VERSION="${CONSOLE_SDK_FROM_VERSION:-1.4.0}"
CONSOLE_SDK_TO_VERSION="${CONSOLE_SDK_TO_VERSION:-4.21.0}"
CONSOLE_INSTALL_CMD="cd frontend && corepack enable && YARN_ENABLE_SCRIPTS=false yarn install"
CONSOLE_BUILD_CMD="cd frontend && yarn build-plugin-sdk"

# React
REACT_REPO_URL="https://github.com/facebook/react.git"
REACT_FROM="${REACT_FROM:-v17.0.2}"
REACT_TO="${REACT_TO:-v18.3.1}"
REACT_BUILD_CMD="npx yarn@1 build"

# React Types (DefinitelyTyped)
DT_REPO_URL="https://github.com/DefinitelyTyped/DefinitelyTyped.git"
REACT_TYPES_FROM="${REACT_TYPES_FROM:-v17}"
REACT_TYPES_TO="${REACT_TYPES_TO:-v18}"

# ── State ────────────────────────────────────────────────────────────────
HOST_PLATFORM=""
TARGET_PLATFORM=""
CROSS_COMPILE=false
KANTRA_VERSION=""
BUILD_ROOT=""
BUILD_DIR=""
BUILD_TMP=""
HOST_SEMVER_BIN=""

# ── Utilities ────────────────────────────────────────────────────────────
info()  { printf "${GREEN}[INFO]${NC}  %s\n" "$*"; }
warn()  { printf "${YELLOW}[WARN]${NC}  %s\n" "$*"; }
error() { printf "${RED}[ERROR]${NC} %s\n" "$*" >&2; }
step()  { printf "\n${BLUE}[STEP %s]${NC} %s\n" "$1" "$2"; }
die()   { error "$@"; exit 1; }

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "Required command not found: $1"
}

git_clone() {
    local url="$1" dest="$2" branch="${3:-}" logfile="${4:-/dev/null}" depth="${5:-}"
    local args=""
    [[ -n "$depth" ]] && args="--depth $depth"
    [[ -n "$branch" ]] && args="$args --branch $branch"
    # shellcheck disable=SC2086
    git clone $args "$url" "$dest" >> "$logfile" 2>&1
}

find_commit_by_date() {
    local repo_dir="$1" target_date="$2" pkg_path="$3"
    local before_date after_date commit
    before_date=$(date -j -v+2d -f "%Y-%m-%d" "$target_date" "+%Y-%m-%d" 2>/dev/null \
        || date -d "$target_date + 2 days" "+%Y-%m-%d" 2>/dev/null)
    after_date=$(date -j -v-2d -f "%Y-%m-%d" "$target_date" "+%Y-%m-%d" 2>/dev/null \
        || date -d "$target_date - 2 days" "+%Y-%m-%d" 2>/dev/null)
    commit=$(cd "$repo_dir" && git log \
        --after="$after_date" --before="$before_date" \
        --format="%H" -- "$pkg_path" 2>/dev/null | head -1)
    if [[ -z "$commit" ]]; then
        commit=$(cd "$repo_dir" && git log \
            --after="$after_date" --before="$before_date" \
            --format="%H" 2>/dev/null | head -1)
    fi
    echo "$commit"
}

run_analyze_and_rules() {
    local name="$1" report_path="$2" ruleset_name="$3"; shift 3
    local analyze_log="$BUILD_TMP/analyze-${name}.log"
    local rules_log="$BUILD_TMP/rules-${name}.log"
    local output_dir="$BUILD_DIR/rules/${name}/semver_rules"

    mkdir -p "$output_dir"

    info "Running semver-analyzer analyze for $name..."
    "$HOST_SEMVER_BIN" analyze typescript \
        "$@" \
        --no-llm \
        --log-file "$analyze_log" --log-level info \
        -o "$report_path" \
        > "$analyze_log.stdout" 2>&1 || die "analyze failed for $name. Check $analyze_log"

    info "Running semver-analyzer konveyor for $name..."
    local extra_konveyor_args=()
    [[ -n "${KONVEYOR_RENAME_PATTERNS:-}" ]] && extra_konveyor_args+=(--rename-patterns "$KONVEYOR_RENAME_PATTERNS")
    [[ -n "${KONVEYOR_PKG_NAME_MAP:-}" ]] && extra_konveyor_args+=(--package-name-map "$KONVEYOR_PKG_NAME_MAP")
    [[ -n "${KONVEYOR_PKG_VERSION:-}" ]] && extra_konveyor_args+=(--package-version "$KONVEYOR_PKG_VERSION")

    "$HOST_SEMVER_BIN" konveyor typescript \
        --from-report "$report_path" \
        --output-dir "$output_dir" \
        --ruleset-name "$ruleset_name" \
        --log-file "$rules_log" --log-level info \
        "${extra_konveyor_args[@]}" \
        > "$rules_log.stdout" 2>&1 || die "konveyor failed for $name. Check $rules_log"

    KONVEYOR_RENAME_PATTERNS=""
    KONVEYOR_PKG_NAME_MAP=""
    KONVEYOR_PKG_VERSION=""

    local rule_count=0
    for rf in "$output_dir"/*.yaml; do
        if [[ -f "$rf" ]]; then
            rule_count=$((rule_count + $(grep -c 'ruleID:' "$rf" 2>/dev/null | tr -d '[:space:]' || echo 0)))
        fi
    done
    info "$name: $rule_count rules generated"
}

prompt_select() {
    local prompt="$1"; shift
    local i=1 choice=""
    for opt in "$@"; do
        printf "  %d) %s\n" "$i" "$opt" >&2
        i=$((i + 1))
    done
    printf "${BOLD}%s${NC} [1]: " "$prompt" >&2
    read -r choice
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

# ── Prerequisites ────────────────────────────────────────────────────────
check_build_prerequisites() {
    info "Checking build prerequisites..."
    local errors=()

    if ! command -v go >/dev/null 2>&1; then
        errors+=("go not found. Install from https://go.dev/dl/")
    fi
    if ! command -v cargo >/dev/null 2>&1; then
        errors+=("cargo not found. Install Rust from https://rustup.rs/")
    fi
    if ! command -v rustup >/dev/null 2>&1; then
        errors+=("rustup not found. Install from https://rustup.rs/ (needed for cross-compile targets)")
    fi
    if ! command -v git >/dev/null 2>&1; then
        errors+=("git not found. Install git.")
    fi
    if ! command -v curl >/dev/null 2>&1; then
        errors+=("curl not found. Install curl.")
    fi
    if ! command -v unzip >/dev/null 2>&1; then
        errors+=("unzip not found. Install unzip.")
    fi
    if ! command -v python3 >/dev/null 2>&1; then
        errors+=("python3 not found. Needed for kantra release selection.")
    fi

    local nvm_dir="${NVM_DIR:-$HOME/.nvm}"
    if [[ ! -f "$nvm_dir/nvm.sh" ]]; then
        errors+=("nvm not found. Needed for rule generation. Install from https://github.com/nvm-sh/nvm")
    fi

    if [[ ${#errors[@]} -gt 0 ]]; then
        error "Missing build prerequisites:"
        for e in "${errors[@]}"; do
            error "  - $e"
        done
        exit 1
    fi

    if ! command -v cargo-zigbuild >/dev/null 2>&1; then
        warn "cargo-zigbuild not found. Cross-compilation will use plain cargo (may fail without a C linker for the target)."
        warn "  Install: cargo install cargo-zigbuild"
    fi

    info "All build prerequisites satisfied"
}

# ── Platform detection ───────────────────────────────────────────────────
detect_host_platform() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)
            case "$arch" in
                x86_64)  HOST_PLATFORM="Linux_x86" ;;
                aarch64) HOST_PLATFORM="Linux_arm64" ;;
                *)       die "Unsupported Linux architecture: $arch" ;;
            esac ;;
        Darwin)
            case "$arch" in
                x86_64)  HOST_PLATFORM="Mac_x86" ;;
                arm64)   HOST_PLATFORM="Mac_arm64" ;;
                *)       die "Unsupported macOS architecture: $arch" ;;
            esac ;;
        *) die "Unsupported OS: $os" ;;
    esac

    info "Host platform: $HOST_PLATFORM"
}

select_platform() {
    local platforms="Linux_x86 Linux_arm64 Mac_x86 Mac_arm64"
    local ordered="$HOST_PLATFORM"
    for p in $platforms; do
        [[ "$p" != "$HOST_PLATFORM" ]] && ordered="$ordered $p"
    done
    printf "\n${BOLD}Select target platform:${NC}\n" >&2
    # shellcheck disable=SC2086
    TARGET_PLATFORM=$(prompt_select "Choose platform" $ordered)
    info "Target platform: $TARGET_PLATFORM"

    if [[ "$TARGET_PLATFORM" == "$HOST_PLATFORM" ]]; then
        CROSS_COMPILE=false
        info "Target matches host — native compilation"
    else
        CROSS_COMPILE=true
        info "Cross-compiling for $TARGET_PLATFORM"
    fi
}

# ── Kantra ───────────────────────────────────────────────────────────────
select_kantra_release() {
    step "1/19" "Selecting kantra release"
    info "Querying GitHub for kantra releases..."

    local suffix
    suffix=$(platform_lookup kantra_suffix "$TARGET_PLATFORM")
    local tags
    tags=$(curl -sS "https://api.github.com/repos/konveyor/kantra/releases?per_page=20" | \
        python3 -c "
import json, sys
releases = json.load(sys.stdin)
count = 0
for r in releases:
    assets = [a['name'] for a in r.get('assets', [])]
    if any('$suffix' in a for a in assets):
        print(r['tag_name'])
        count += 1
        if count >= 3:
            break
" 2>/dev/null)

    if [[ -z "$tags" ]]; then
        die "No kantra releases found with assets for $suffix"
    fi

    printf "\n${BOLD}Available kantra releases:${NC}\n" >&2
    # shellcheck disable=SC2086
    KANTRA_VERSION=$(prompt_select "Choose release" $tags)
    info "Selected: $KANTRA_VERSION"
}

download_kantra() {
    step "2/19" "Downloading kantra release"

    local suffix
    suffix=$(platform_lookup kantra_suffix "$TARGET_PLATFORM")
    local url="https://github.com/konveyor/kantra/releases/download/${KANTRA_VERSION}/kantra.${suffix}.zip"
    local zip_path="$BUILD_TMP/kantra.zip"

    mkdir -p "$BUILD_DIR/.kantra"

    local log="$BUILD_TMP/download-kantra.log"
    info "Follow logs: tail -f $log"
    info "Downloading $url"
    curl -fSL -o "$zip_path" "$url" >> "$log" 2>&1 || die "Failed to download kantra. Check $log"

    info "Extracting to .kantra/"
    unzip -o "$zip_path" -d "$BUILD_DIR/.kantra/" >> "$log" 2>&1 \
        || die "Failed to extract kantra release. Check $log"

    if [[ -f "$BUILD_DIR/.kantra/darwin-kantra" ]]; then
        mv "$BUILD_DIR/.kantra/darwin-kantra" "$BUILD_DIR/.kantra/kantra"
    fi

    chmod +x "$BUILD_DIR/.kantra/kantra" "$BUILD_DIR/.kantra/java-external-provider" 2>/dev/null || true

    : > "$BUILD_DIR/.kantra/maven-index.txt"
    info "Created empty maven-index.txt"

    rm "$zip_path"
}

build_kantra_from_source() {
    step "3/19" "Building kantra from source (Go)"

    local kantra_src="$BUILD_TMP/kantra-src"

    local log="$BUILD_TMP/build-kantra.log"
    info "Follow logs: tail -f $log"

    info "Cloning konveyor/kantra..."
    git_clone "$KANTRA_REPO_URL" "$kantra_src" "$KANTRA_REPO_BRANCH" "$log" 1 \
        || die "Failed to clone kantra. Check $log"

    local goos goarch
    goos=$(platform_lookup go_os "$TARGET_PLATFORM")
    goarch=$(platform_lookup go_arch "$TARGET_PLATFORM")

    info "Building for ${goos}/${goarch}..."
    (
        cd "$kantra_src"
        GOOS="$goos" GOARCH="$goarch" CGO_ENABLED=0 \
        go build -o "$BUILD_DIR/.kantra/kantra" main.go
    ) >> "$log" 2>&1 || die "Failed to build kantra. Check $log"

    chmod +x "$BUILD_DIR/.kantra/kantra"
    info "kantra binary built"
}

build_java_external_provider() {
    step "4/19" "Building java-external-provider from analyzer-lsp"

    local analyzer_src="$BUILD_TMP/analyzer-lsp"

    local log="$BUILD_TMP/build-java-provider.log"
    info "Follow logs: tail -f $log"

    info "Cloning konveyor/analyzer-lsp..."
    git_clone "$ANALYZER_LSP_REPO_URL" "$analyzer_src" "$ANALYZER_LSP_REPO_BRANCH" "$log" 1 \
        || die "Failed to clone analyzer-lsp. Check $log"

    local goos goarch
    goos=$(platform_lookup go_os "$TARGET_PLATFORM")
    goarch=$(platform_lookup go_arch "$TARGET_PLATFORM")

    info "Building java-external-provider for ${goos}/${goarch}..."
    (
        cd "$analyzer_src/external-providers/java-external-provider"
        GOOS="$goos" GOARCH="$goarch" CGO_ENABLED=0 \
        go build -o "$BUILD_DIR/.kantra/java-external-provider" main.go
    ) >> "$log" 2>&1 || die "Failed to build java-external-provider. Check $log"

    chmod +x "$BUILD_DIR/.kantra/java-external-provider"
    info "java-external-provider built"
}

# ── Rust builds ──────────────────────────────────────────────────────────
rust_build() {
    local name="$1" src_dir="$2" output_binary="$3" target="$4"

    local log="$BUILD_TMP/build-${name}.log"
    info "Building $name for $target..."
    info "Follow logs: tail -f $log"

    # Use rustup-managed toolchain for cross-compile targets
    export PATH="$HOME/.cargo/bin:$PATH"
    rustup target add "$target" >> "$log" 2>&1 || true

    local build_cmd
    if [[ "$CROSS_COMPILE" == true ]] && command -v cargo-zigbuild >/dev/null 2>&1; then
        build_cmd="cargo zigbuild --release --target $target"
    else
        build_cmd="cargo build --release --target $target"
    fi

    # Override zig-bundled ar with llvm-ar for cross-compilation (zig ar is buggy with C deps)
    local ar_env=""
    if [[ "$CROSS_COMPILE" == true ]] && command -v cargo-zigbuild >/dev/null 2>&1; then
        local llvm_ar=""
        command -v llvm-ar >/dev/null 2>&1 && llvm_ar="$(command -v llvm-ar)"
        [[ -z "$llvm_ar" ]] && llvm_ar="$({ find /opt/homebrew/opt /usr/local/opt -maxdepth 3 -name llvm-ar 2>/dev/null || true; } | head -1)"
        if [[ -n "$llvm_ar" ]]; then
            local ar_var="AR_$(echo "$target" | tr '-' '_')"
            export AR="$llvm_ar"
            export "$ar_var=$llvm_ar"
        fi
    fi
    (cd "$src_dir" && $build_cmd) >> "$log" 2>&1 || die "Failed to build $name. Check $log"

    local binary_path="$src_dir/target/$target/release/$name"
    if [[ ! -f "$binary_path" ]]; then
        die "Expected binary not found: $binary_path"
    fi

    cp "$binary_path" "$output_binary"
    chmod +x "$output_binary"
    info "$name built successfully"
}

build_semver_analyzer() {
    step "5/19" "Building semver-analyzer"

    local semver_src="$BUILD_TMP/semver-analyzer"
    local konveyor_core_src="$BUILD_TMP/konveyor-core"

    local log="$BUILD_TMP/clone-semver.log"
    info "Follow logs: tail -f $log"

    info "Cloning konveyor-core (path dependency)..."
    git_clone "$KONVEYOR_CORE_REPO_URL" "$konveyor_core_src" "$KONVEYOR_CORE_REPO_BRANCH" "$log" \
        || die "Failed to clone konveyor-core. Check $log"

    info "Cloning semver-analyzer from $SEMVER_REPO_URL:$SEMVER_REPO_BRANCH ..."
    git_clone "$SEMVER_REPO_URL" "$semver_src" "$SEMVER_REPO_BRANCH" "$log" \
        || die "Failed to clone semver-analyzer. Check $log"

    local target
    target=$(platform_lookup rust_target "$TARGET_PLATFORM")
    mkdir -p "$BUILD_DIR/bin"

    rust_build "semver-analyzer" "$semver_src" "$BUILD_DIR/bin/semver-analyzer" "$target"
}

build_host_semver_analyzer() {
    if [[ "$CROSS_COMPILE" == false ]]; then
        HOST_SEMVER_BIN="$BUILD_DIR/bin/semver-analyzer"
        info "Using target binary as host binary (same platform)"
        return
    fi

    step "6/19" "Building semver-analyzer for host (needed for rule generation)"

    local semver_src="$BUILD_TMP/semver-analyzer"
    local host_target
    host_target=$(platform_lookup rust_target "$HOST_PLATFORM")

    HOST_SEMVER_BIN="$BUILD_TMP/semver-analyzer-host"
    rust_build "semver-analyzer" "$semver_src" "$HOST_SEMVER_BIN" "$host_target"
}

build_frontend_analyzer_provider() {
    step "7/19" "Building frontend-analyzer-provider"

    local fap_src="$BUILD_TMP/frontend-analyzer-provider"

    local log="$BUILD_TMP/clone-fap.log"
    info "Follow logs: tail -f $log"

    info "Cloning fix-engine (path dependency)..."
    git_clone "$FIX_ENGINE_REPO_URL" "$BUILD_TMP/fix-engine" "$FIX_ENGINE_REPO_BRANCH" "$log" \
        || die "Failed to clone fix-engine. Check $log"

    info "Cloning frontend-analyzer-provider..."
    git_clone "$FAP_REPO_URL" "$fap_src" "$FAP_REPO_BRANCH" "$log" \
        || die "Failed to clone frontend-analyzer-provider. Check $log"

    local target
    target=$(platform_lookup rust_target "$TARGET_PLATFORM")

    rust_build "frontend-analyzer-provider" "$fap_src" "$BUILD_DIR/bin/frontend-analyzer-provider" "$target"

    local fix_engine_src="$BUILD_TMP/fix-engine"
    rust_build "fix-engine-cli" "$fix_engine_src" "$BUILD_DIR/bin/fix-engine-cli" "$target"

    info "fix-engine-cli built"
}

# ── Rule generation for all libraries ────────────────────────────────────
ensure_nvm() {
    local nvm_dir="${NVM_DIR:-$HOME/.nvm}"
    [[ -f "$nvm_dir/nvm.sh" ]] || die "nvm required for rule generation. Install from https://github.com/nvm-sh/nvm"
}

generate_pf_rules() {
    step "9/19" "Generating PatternFly React rules"
    ensure_nvm

    local pf_react_src="$BUILD_TMP/patternfly-react"
    local pf_src="$BUILD_TMP/patternfly"
    local clone_log="$BUILD_TMP/clone-patternfly.log"

    [[ -d "$pf_react_src/.git" ]] || git_clone "$PF_REACT_REPO_URL" "$pf_react_src" "" "$clone_log" \
        || die "Failed to clone patternfly-react. Check $clone_log"
    [[ -d "$pf_src/.git" ]] || git_clone "$PF_REPO_URL" "$pf_src" "" "$clone_log" \
        || die "Failed to clone patternfly. Check $clone_log"

    info "patternfly-react: $PF_REACT_FROM -> $PF_REACT_TO"

    KONVEYOR_RENAME_PATTERNS="" KONVEYOR_PKG_NAME_MAP="" KONVEYOR_PKG_VERSION=""
    [[ -f "$BUILD_DIR/patternfly-token-mappings.yaml" ]] && KONVEYOR_RENAME_PATTERNS="$BUILD_DIR/patternfly-token-mappings.yaml"

    local dep_build_cmd="export NODE_ENV=development && yarn install && npx gulp buildPatternfly"

    run_analyze_and_rules "patternfly" "$BUILD_TMP/pf-report.json" "patternfly-breaking-changes" \
        --repo "$pf_react_src" \
        --from "$PF_REACT_FROM" --to "$PF_REACT_TO" \
        --dep-repo "$pf_src" \
        --dep-from "$PF_DEP_FROM" --dep-to "$PF_DEP_TO" \
        --dep-build-command "$dep_build_cmd" \
        --from-node-version 18 \
        --to-node-version 20 \
        --from-install-command "npx yarn@1 install --frozen-lockfile" \
        --from-build-command "npx yarn@1 build" \
        --to-build-command "yarn build:generate && yarn build:esm"
}

generate_topology_rules() {
    step "10/19" "Generating PatternFly Topology rules"
    ensure_nvm

    local repo_src="$BUILD_TMP/react-topology"
    local clone_log="$BUILD_TMP/clone-topology.log"
    [[ -d "$repo_src/.git" ]] || git_clone "$TOPOLOGY_REPO_URL" "$repo_src" "" "$clone_log" \
        || die "Failed to clone react-topology. Check $clone_log"

    info "react-topology: $TOPOLOGY_FROM -> $TOPOLOGY_TO"
    KONVEYOR_RENAME_PATTERNS="" KONVEYOR_PKG_NAME_MAP="" KONVEYOR_PKG_VERSION=""
    run_analyze_and_rules "topology" "$BUILD_TMP/topology-report.json" "topology-breaking-changes" \
        --repo "$repo_src" \
        --from "$TOPOLOGY_FROM" --to "$TOPOLOGY_TO" \
        --from-install-command "$TOPOLOGY_INSTALL_CMD" \
        --to-install-command "$TOPOLOGY_INSTALL_CMD" \
        --from-build-command "$TOPOLOGY_BUILD_CMD" \
        --to-build-command "$TOPOLOGY_BUILD_CMD"
}

generate_rcg_rules() {
    step "11/19" "Generating PatternFly Component Groups rules"
    ensure_nvm

    local repo_src="$BUILD_TMP/react-component-groups"
    local clone_log="$BUILD_TMP/clone-rcg.log"
    [[ -d "$repo_src/.git" ]] || git_clone "$RCG_REPO_URL" "$repo_src" "" "$clone_log" \
        || die "Failed to clone react-component-groups. Check $clone_log"

    info "react-component-groups: $RCG_FROM -> $RCG_TO"
    KONVEYOR_RENAME_PATTERNS="" KONVEYOR_PKG_NAME_MAP="" KONVEYOR_PKG_VERSION=""
    run_analyze_and_rules "rcg" "$BUILD_TMP/rcg-report.json" "react-component-groups-breaking-changes" \
        --repo "$repo_src" \
        --from "$RCG_FROM" --to "$RCG_TO" \
        --from-install-command "$RCG_INSTALL_CMD" \
        --to-install-command "$RCG_INSTALL_CMD" \
        --from-build-command "$RCG_BUILD_CMD" \
        --to-build-command "$RCG_BUILD_CMD"
}

generate_sdk_rules() {
    step "12/19" "Generating Dynamic Plugin SDK rules"
    ensure_nvm

    local repo_src="$BUILD_TMP/dynamic-plugin-sdk"
    local clone_log="$BUILD_TMP/clone-sdk.log"
    [[ -d "$repo_src/.git" ]] || git_clone "$SDK_REPO_URL" "$repo_src" "" "$clone_log" \
        || die "Failed to clone dynamic-plugin-sdk. Check $clone_log"

    local from_commit to_commit
    from_commit=$(find_commit_by_date "$repo_src" "$SDK_FROM_DATE" "packages/lib-core/package.json")
    to_commit=$(find_commit_by_date "$repo_src" "$SDK_TO_DATE" "packages/lib-core/package.json")

    if [[ -z "$from_commit" || -z "$to_commit" ]]; then
        warn "Could not resolve SDK commits from dates. Skipping."
        return
    fi

    info "dynamic-plugin-sdk: ${from_commit:0:10} -> ${to_commit:0:10}"
    KONVEYOR_RENAME_PATTERNS="" KONVEYOR_PKG_NAME_MAP="" KONVEYOR_PKG_VERSION=""
    run_analyze_and_rules "sdk" "$BUILD_TMP/sdk-report.json" "dynamic-plugin-sdk-breaking-changes" \
        --repo "$repo_src" \
        --from "$from_commit" --to "$to_commit" \
        --from-build-command "$SDK_BUILD_CMD" \
        --to-build-command "$SDK_BUILD_CMD"
}

generate_console_rules() {
    step "13/19" "Generating Console SDK rules"
    ensure_nvm

    local repo_src="$BUILD_TMP/console"
    local clone_log="$BUILD_TMP/clone-console.log"
    [[ -d "$repo_src/.git" ]] || git_clone "$CONSOLE_REPO_URL" "$repo_src" "" "$clone_log" \
        || die "Failed to clone openshift/console. Check $clone_log"

    info "console-sdk: $CONSOLE_FROM -> $CONSOLE_TO"
    KONVEYOR_RENAME_PATTERNS=""
    KONVEYOR_PKG_NAME_MAP="@console/dynamic-plugin-sdk=@openshift-console/dynamic-plugin-sdk"
    KONVEYOR_PKG_VERSION="@openshift-console/dynamic-plugin-sdk=${CONSOLE_SDK_FROM_VERSION}:${CONSOLE_SDK_TO_VERSION}"

    run_analyze_and_rules "console" "$BUILD_TMP/console-report.json" "console-sdk-breaking-changes" \
        --repo "$repo_src" \
        --from "$CONSOLE_FROM" --to "$CONSOLE_TO" \
        --from-install-command "$CONSOLE_INSTALL_CMD" \
        --to-install-command "$CONSOLE_INSTALL_CMD" \
        --from-build-command "$CONSOLE_BUILD_CMD" \
        --to-build-command "$CONSOLE_BUILD_CMD"
}

generate_react_rules() {
    step "14/19" "Generating React rules"
    ensure_nvm

    local repo_src="$BUILD_TMP/react"
    local clone_log="$BUILD_TMP/clone-react.log"
    if [[ ! -d "$repo_src/.git" ]]; then
        git clone --bare "$REACT_REPO_URL" "$repo_src/.git" >> "$clone_log" 2>&1 \
            || die "Failed to clone react. Check $clone_log"
        (cd "$repo_src" && git config core.bare false && git checkout "$REACT_TO" 2>/dev/null)
    fi

    info "react: $REACT_FROM -> $REACT_TO"
    KONVEYOR_RENAME_PATTERNS="" KONVEYOR_PKG_NAME_MAP="" KONVEYOR_PKG_VERSION=""

    local react_install_cmd="export ELECTRON_SKIP_BINARY_DOWNLOAD=1 && export NVM_DIR=\"\$HOME/.nvm\" && . \"\$NVM_DIR/nvm.sh\" && nvm exec 18 npx yarn@1 install --ignore-optional --ignore-scripts"

    (run_analyze_and_rules "react" "$BUILD_TMP/react-report.json" "react-breaking-changes" \
        --repo "$repo_src" \
        --from "$REACT_FROM" --to "$REACT_TO" \
        --from-node-version 14 \
        --to-node-version 14 \
        --from-install-command "$react_install_cmd" \
        --to-install-command "$react_install_cmd" \
        --from-build-command "$REACT_BUILD_CMD" \
        --to-build-command "$REACT_BUILD_CMD") \
        || warn "React rule generation failed (Node 14 may not be available on this platform)"
}

generate_react_types_rules() {
    step "15/19" "Generating React Types rules"

    local dt_src="$BUILD_TMP/DefinitelyTyped"
    local repo_src="$BUILD_TMP/react-types"
    local clone_log="$BUILD_TMP/clone-react-types.log"

    if [[ ! -d "$dt_src/.git" ]]; then
        info "Sparse-cloning DefinitelyTyped..."
        git clone --filter=blob:none --sparse "$DT_REPO_URL" "$dt_src" >> "$clone_log" 2>&1 \
            || die "Failed to clone DefinitelyTyped. Check $clone_log"
        (cd "$dt_src" && git sparse-checkout set types/react types/react-dom)
    fi

    if [[ ! -d "$repo_src/.git" ]]; then
        info "Building synthetic repo..."
        mkdir -p "$repo_src" && cd "$repo_src" && git init -q
        mkdir -p packages/react packages/react-dom
        cp -a "$dt_src/types/react/v17/"* packages/react/ 2>/dev/null || true
        rm -rf packages/react/test
        cp -a "$dt_src/types/react-dom/v17/"* packages/react-dom/ 2>/dev/null || true
        rm -rf packages/react-dom/test
        find packages -name package.json -exec sed -i.bak 's/\([0-9]\+\)\.\([0-9]\+\)\.9999/\1.\2.0/g' {} +
        find packages -name "*.bak" -delete
        git add -A && git commit -q -m "v17: @types/react v17" && git tag v17

        rm -rf packages/react/* packages/react-dom/*
        cp -a "$dt_src/types/react/v18/"* packages/react/ 2>/dev/null || true
        rm -rf packages/react/test packages/react/ts5.0
        cp -a "$dt_src/types/react-dom/v18/"* packages/react-dom/ 2>/dev/null || true
        rm -rf packages/react-dom/test packages/react-dom/ts5.0
        find packages -name package.json -exec sed -i.bak 's/\([0-9]\+\)\.\([0-9]\+\)\.9999/\1.\2.0/g' {} +
        find packages -name "*.bak" -delete
        git add -A && git commit -q -m "v18: @types/react v18" && git tag v18
        cd "$OLDPWD"
    fi

    info "react-types: $REACT_TYPES_FROM -> $REACT_TYPES_TO"
    KONVEYOR_RENAME_PATTERNS="" KONVEYOR_PKG_NAME_MAP="" KONVEYOR_PKG_VERSION=""
    (run_analyze_and_rules "react-types" "$BUILD_TMP/react-types-report.json" "react-types-breaking-changes" \
        --repo "$repo_src" \
        --from "$REACT_TYPES_FROM" --to "$REACT_TYPES_TO" \
        --from-install-command "true" \
        --to-install-command "true" \
        --from-build-command "true" \
        --to-build-command "true") \
        || warn "React Types rule generation failed"
}

# ── Extras ───────────────────────────────────────────────────────────────
download_token_mappings() {
    step "8/25" "Downloading token mappings (before rule generation)"

    curl -fSL -o "$BUILD_DIR/patternfly-token-mappings.yaml" "$TOKEN_MAPPINGS_URL" \
        >> "$BUILD_TMP/download-token-mappings.log" 2>&1 || die "Failed to download token mappings"

    info "Downloaded patternfly-token-mappings.yaml"
}

copy_prompt() {
    step "16/19" "Copying prompt.md"

    cp "$SCRIPT_DIR/prompt.md" "$BUILD_DIR/prompt.md" \
        || die "prompt.md not found in $SCRIPT_DIR"
    info "Copied prompt.md"
}

git_sha() {
    local repo_dir="$1"
    if [[ -d "$repo_dir/.git" ]]; then
        git -C "$repo_dir" rev-parse --short HEAD 2>/dev/null || echo "unknown"
    else
        echo "unknown"
    fi
}

generate_manifest() {
    step "17/19" "Generating MANIFEST"

    local build_date
    build_date=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

    cat > "$BUILD_DIR/MANIFEST" <<MANIFEST
# PatternFly Migration Tools
# Generated: ${build_date}

[build]
platform = ${TARGET_PLATFORM}
build_date = ${build_date}

[kantra]
repo = ${KANTRA_REPO_URL}
branch = ${KANTRA_REPO_BRANCH:-default}
release_version = ${KANTRA_VERSION}
source_sha = $(git_sha "$BUILD_TMP/kantra-src")

[analyzer-lsp]
repo = ${ANALYZER_LSP_REPO_URL}
branch = ${ANALYZER_LSP_REPO_BRANCH:-default}
source_sha = $(git_sha "$BUILD_TMP/analyzer-lsp")

[semver-analyzer]
repo = ${SEMVER_REPO_URL}
branch = ${SEMVER_REPO_BRANCH:-default}
source_sha = $(git_sha "$BUILD_TMP/semver-analyzer")

[frontend-analyzer-provider]
repo = ${FAP_REPO_URL}
branch = ${FAP_REPO_BRANCH:-default}
source_sha = $(git_sha "$BUILD_TMP/frontend-analyzer-provider")

[fix-engine]
repo = ${FIX_ENGINE_REPO_URL}
branch = ${FIX_ENGINE_REPO_BRANCH:-default}
source_sha = $(git_sha "$BUILD_TMP/fix-engine")

[konveyor-core]
repo = ${KONVEYOR_CORE_REPO_URL}
branch = ${KONVEYOR_CORE_REPO_BRANCH:-default}
source_sha = $(git_sha "$BUILD_TMP/konveyor-core")

[rules.patternfly]
repo = ${PF_REACT_REPO_URL}
from = ${PF_REACT_FROM}
to = ${PF_REACT_TO}

[rules.topology]
repo = ${TOPOLOGY_REPO_URL}
from = ${TOPOLOGY_FROM}
to = ${TOPOLOGY_TO}

[rules.react-component-groups]
repo = ${RCG_REPO_URL}
from = ${RCG_FROM}
to = ${RCG_TO}

[rules.dynamic-plugin-sdk]
repo = ${SDK_REPO_URL}
from_date = ${SDK_FROM_DATE}
to_date = ${SDK_TO_DATE}

[rules.console-sdk]
repo = ${CONSOLE_REPO_URL}
from = ${CONSOLE_FROM}
to = ${CONSOLE_TO}

[rules.react]
repo = ${REACT_REPO_URL}
from = ${REACT_FROM}
to = ${REACT_TO}

[rules.react-types]
repo = ${DT_REPO_URL}
from = ${REACT_TYPES_FROM}
to = ${REACT_TYPES_TO}
MANIFEST

    info "MANIFEST written"
    cat "$BUILD_DIR/MANIFEST"
}

copy_run_script() {
    step "18/19" "Copying run.sh"

    cp "$SCRIPT_DIR/run.sh" "$BUILD_DIR/run.sh"
    chmod +x "$BUILD_DIR/run.sh"
    if [[ -f "$SCRIPT_DIR/README.run.md" ]]; then
        cp "$SCRIPT_DIR/README.run.md" "$BUILD_DIR/README.md"
    fi
    info "Copied run.sh and README.md into archive"
}

package_archive() {
    step "19/25" "Packaging archive"

    # Preserve build logs in the archive
    mkdir -p "$BUILD_DIR/logs"
    cp "$BUILD_TMP"/*.log "$BUILD_DIR/logs/" 2>/dev/null || true
    rm -rf "$BUILD_TMP"

    local archive_name="patternfly_tools_${TARGET_PLATFORM}.zip"
    local archive_path="$SCRIPT_DIR/$archive_name"
    local parent_dir
    parent_dir="$(dirname "$BUILD_DIR")"

    info "Creating $archive_name..."

    (cd "$parent_dir" && zip -r "$archive_path" "$(basename "$BUILD_DIR")/") > /dev/null 2>&1 \
        || die "Failed to create zip archive"

    local size
    size=$(du -sh "$archive_path" | cut -f1)

    printf "\n"
    info "Archive created: $archive_path ($size)"
}

# ── Main ─────────────────────────────────────────────────────────────────
main() {
    printf "\n${BOLD}PatternFly Tools Builder${NC}\n"
    printf "========================\n\n"

    check_build_prerequisites
    detect_host_platform
    select_platform
    select_kantra_release

    BUILD_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/pf-build.XXXXXX")
    BUILD_DIR="$BUILD_ROOT/patternfly-tools"
    BUILD_TMP="$BUILD_ROOT/tmp"

    mkdir -p "$BUILD_DIR" "$BUILD_TMP"

    info "Build directory: $BUILD_ROOT"
    info "Build logs: $BUILD_TMP/*.log"

    download_kantra
    build_kantra_from_source
    build_java_external_provider
    build_semver_analyzer
    build_host_semver_analyzer
    build_frontend_analyzer_provider
    download_token_mappings
    generate_pf_rules
    generate_topology_rules
    generate_rcg_rules
    generate_sdk_rules
    generate_console_rules
    generate_react_rules
    generate_react_types_rules
    copy_prompt
    generate_manifest
    copy_run_script
    package_archive

    printf "\n"
    info "Build complete!"
    info "Archive: $SCRIPT_DIR/patternfly_tools_${TARGET_PLATFORM}.zip"
}

main
