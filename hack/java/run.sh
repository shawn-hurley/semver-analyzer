#!/usr/bin/env bash
# Java breaking change analysis test harness
#
# Supports three test targets:
#   spring-boot      — The framework itself (v3.5.0 → v4.0.0)
#   petclinic        — Canonical sample app (SB 3.5.6 → SB 4.0.0)
#   microservices    — Distributed petclinic (SB 3.4.1 → SB 4.0.0)
#
# Usage:
#   ./hack/java/run.sh spring-boot             # Spring Boot framework analysis
#   ./hack/java/run.sh petclinic               # Petclinic migration analysis
#   ./hack/java/run.sh microservices           # Microservices migration analysis
#   ./hack/java/run.sh all                     # Run all three
#   ./hack/java/run.sh --extract-only <target> <ref>  # Extract API surface only

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
OUTPUT_DIR="$SCRIPT_DIR/output"

# ── Colors ──────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${BLUE}[INFO]${NC}  $*"; }
ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
err()   { echo -e "${RED}[ERROR]${NC} $*"; }
header(){ echo -e "\n${BOLD}═══ $* ═══${NC}\n"; }

# ── Target lookup ───────────────────────────────────────────────────────
get_target() {
    local name="$1"
    case "$name" in
        spring-boot)
            REPO_URL="https://github.com/spring-projects/spring-boot.git"
            CLONE_DIR="/tmp/spring-boot"
            FROM_REF="v3.5.0"
            TO_REF="v4.0.0"
            DESC="Spring Boot framework (3.5→4.0)"
            ;;
        petclinic)
            REPO_URL="https://github.com/spring-projects/spring-petclinic.git"
            CLONE_DIR="/tmp/spring-petclinic"
            FROM_REF="66747e3"
            TO_REF="a9b7c6b"
            DESC="Spring Petclinic sample app (SB3→SB4)"
            ;;
        microservices)
            REPO_URL="https://github.com/spring-petclinic/spring-petclinic-microservices.git"
            CLONE_DIR="/tmp/spring-petclinic-microservices"
            FROM_REF="e8827d8"
            TO_REF="17cad88"
            DESC="Petclinic Microservices (SB 3.4→4.0)"
            ;;
        jhipster)
            REPO_URL="https://github.com/jhipster/jhipster-sample-app.git"
            CLONE_DIR="/tmp/jhipster-sample"
            FROM_REF="v8.11.0"
            TO_REF="v9.0.0"
            DESC="JHipster Sample App (SB 3.4→4.0, 134 Java files)"
            ;;
        gateway)
            REPO_URL="https://github.com/spring-cloud/spring-cloud-gateway.git"
            CLONE_DIR="/tmp/spring-cloud-gateway"
            FROM_REF="v4.3.4"
            TO_REF="v5.0.0"
            DESC="Spring Cloud Gateway (SB 3→4, 640 Java files, deep SB integration)"
            ;;
        *)
            err "Unknown target: $name"
            echo "Valid targets: spring-boot, petclinic, microservices, jhipster, gateway, all"
            exit 1
            ;;
    esac
}

# ── Functions ───────────────────────────────────────────────────────────

ensure_repo() {
    local url="$1" dir="$2"
    if [ ! -d "$dir/.git" ] && [ ! -f "$dir/HEAD" ]; then
        info "Cloning $(basename "$dir") ..."
        git clone --filter=blob:none --no-checkout "$url" "$dir" 2>&1 | tail -1
        ok "Cloned to $dir"
    else
        info "Using existing repo at $dir"
    fi
}

verify_ref() {
    local dir="$1" ref="$2"
    (cd "$dir" && git rev-parse --verify "$ref" >/dev/null 2>&1) || {
        # Try fetching first
        (cd "$dir" && git fetch origin 2>/dev/null || true)
        (cd "$dir" && git rev-parse --verify "$ref" >/dev/null 2>&1) || {
            err "Ref '$ref' not found in $(basename "$dir")"
            return 1
        }
    }
}

run_analysis() {
    local target_name="$1"
    get_target "$target_name"

    header "$DESC"

    ensure_repo "$REPO_URL" "$CLONE_DIR"
    verify_ref "$CLONE_DIR" "$FROM_REF"
    verify_ref "$CLONE_DIR" "$TO_REF"
    ok "Refs verified: $FROM_REF → $TO_REF"

    local report_file="$OUTPUT_DIR/report-${target_name}.json"
    local log_file="$OUTPUT_DIR/${target_name}.log"

    info "Analyzing: $FROM_REF → $TO_REF"
    info "Output: $report_file"
    echo ""

    time "$ANALYZER" analyze java \
        --repo "$CLONE_DIR" \
        --from "$FROM_REF" \
        --to "$TO_REF" \
        --no-llm \
        --pipeline-v2 \
        -o "$report_file" \
        --log-file "$log_file" \
        --log-level debug

    ok "Analysis complete: $report_file"
    print_summary "$report_file"
}

print_summary() {
    local report_file="$1"
    python3 << PYEOF || warn "python3 not available for summary"
import json
with open('$report_file') as f:
    data = json.load(f)

summary = data.get('summary', {})
print()
print(f'  Total breaking changes:    {summary.get("total_breaking_changes", 0)}')
print(f'  Breaking API changes:      {summary.get("breaking_api_changes", 0)}')
print(f'  Files with breaking:       {summary.get("files_with_breaking_changes", 0)}')

changes = data.get('changes', [])
all_changes = []
for fc in changes:
    for ac in fc.get('breaking_api_changes', []):
        all_changes.append(ac)

change_types = {}
for c in all_changes:
    ct = c.get('change', 'unknown')
    change_types[ct] = change_types.get(ct, 0) + 1

if change_types:
    print()
    print('  Change types:')
    for ct, count in sorted(change_types.items(), key=lambda x: -x[1]):
        print(f'    {ct}: {count}')

# Show migration targets (rename suggestions)
with_targets = [c for c in all_changes if c.get('migration_target')]
if with_targets:
    print()
    print(f'  Migration targets detected: {len(with_targets)}')
    for c in with_targets[:10]:
        mt = c['migration_target']
        old = c['symbol']
        new = mt.get('replacement_symbol', '?')
        print(f'    {old} -> {new}')
    if len(with_targets) > 10:
        print(f'    ... and {len(with_targets) - 10} more')

manifest = data.get('manifest_changes', [])
if manifest:
    breaking = [m for m in manifest if m.get('is_breaking')]
    print()
    print(f'  Manifest changes: {len(manifest)} ({len(breaking)} breaking)')
    for m in manifest[:5]:
        brk = 'BREAKING' if m.get('is_breaking') else '        '
        print(f'    [{brk}] {m["description"][:80]}')
    if len(manifest) > 5:
        print(f'    ... and {len(manifest) - 5} more')
PYEOF
}

print_surface_stats() {
    local surface_file="$1"
    python3 << PYEOF || true
import json
with open('$surface_file') as f:
    data = json.load(f)
symbols = data.get('symbols', [])
members = sum(len(s.get('members', [])) for s in symbols)
print(f'  Types: {len(symbols)}, Members: {members}')
kinds = {}
for s in symbols:
    k = s.get('kind', 'unknown')
    kinds[k] = kinds.get(k, 0) + 1
for k, v in sorted(kinds.items(), key=lambda x: -x[1]):
    print(f'    {k}: {v}')
PYEOF
}

# ── Build the analyzer ──────────────────────────────────────────────────
info "Building semver-analyzer (release)..."
(cd "$PROJECT_ROOT" && cargo build --release 2>&1 | tail -1)
ANALYZER="$PROJECT_ROOT/target/release/semver-analyzer"

if [ ! -x "$ANALYZER" ]; then
    err "Failed to build semver-analyzer"
    exit 1
fi
ok "Built: $ANALYZER"

mkdir -p "$OUTPUT_DIR"

# ── Parse arguments ─────────────────────────────────────────────────────
TARGET="${1:-spring-boot}"

if [ "$TARGET" = "--extract-only" ]; then
    EXTRACT_TARGET="${2:?Usage: run.sh --extract-only <target> <ref>}"
    EXTRACT_REF="${3:?Usage: run.sh --extract-only <target> <ref>}"
    get_target "$EXTRACT_TARGET"
    ensure_repo "$REPO_URL" "$CLONE_DIR"
    verify_ref "$CLONE_DIR" "$EXTRACT_REF"
    local_out="$OUTPUT_DIR/surface-${EXTRACT_TARGET}-${EXTRACT_REF}.json"
    "$ANALYZER" extract java \
        --repo "$CLONE_DIR" \
        --git-ref "$EXTRACT_REF" \
        -o "$local_out"
    ok "Surface written to $local_out"
    print_surface_stats "$local_out"
    exit 0
fi

if [ "$TARGET" = "all" ]; then
    for t in spring-boot petclinic microservices jhipster gateway; do
        run_analysis "$t"
    done
    header "All analyses complete"
    echo "  Reports in: $OUTPUT_DIR/"
    ls -la "$OUTPUT_DIR"/report-*.json 2>/dev/null
    exit 0
fi

run_analysis "$TARGET"
