#!/usr/bin/env bash
#
# run-full-pipeline.sh -- End-to-end integration pipeline:
#
#   1. Run semver-analyzer against PatternFly v5 → v6
#   2. Generate Konveyor rules (builtin + frontend provider)
#   3. Clone quipucords-ui at v5 commit
#   4. Build frontend-analyzer-provider
#   5. Run kantra with the generated rules
#   6. Apply pattern-based fixes
#   7. Compare auto-fixed vs hand-migrated quipucords
#   8. Generate markdown comparison analysis
#
# Usage:
#   ./hack/integration/run-full-pipeline.sh [OPTIONS]
#
# Options:
#   --skip-analysis    Skip semver-analyzer run (reuse existing report)
#   --skip-build       Skip building semver-analyzer and frontend-analyzer-provider
#   --skip-kantra      Skip kantra run (reuse existing analysis)
#   --skip-fix         Skip applying fixes
#   --release          Build in release mode
#   --work-dir DIR     Working directory (default: /tmp/semver-integration)
#   --rename-patterns  Path to token mappings YAML (default: patternfly-token-mappings.yaml)
#   --no-pipeline-v2   Use v1 (BU) pipeline instead of v2 (SD)
#   --help             Show this help message
#
set -euo pipefail

# ── Defaults ─────────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SEMVER_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
FAP_DIR="$(cd "$SEMVER_DIR/../frontend-analyzer-provider" && pwd)"
PF_REPO="${SEMVER_DIR}/../testdata/patternfly-react"
PF_CSS_REPO_URL="https://github.com/patternfly/patternfly.git"

WORK_DIR="/tmp/semver-integration"
SKIP_ANALYSIS=false
SKIP_BUILD=false
SKIP_KANTRA=false
SKIP_FIX=false
RELEASE=false

PF_FROM="v5.4.0"
PF_TO="v6.4.1"
QUIPUCORDS_REPO="git@github.com:jwmatthews/quipucords-ui.git"
QUIPUCORDS_V5_COMMIT="3b3ce52"
RENAME_PATTERNS="$SCRIPT_DIR/patternfly-token-mappings.yaml"
PIPELINE_V2=true  # v2 is the default; use --no-pipeline-v2 to disable

# ── Parse args ───────────────────────────────────────────────────────────

usage() {
    head -24 "$0" | tail -18
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-analysis) SKIP_ANALYSIS=true; shift ;;
        --skip-build)    SKIP_BUILD=true;    shift ;;
        --skip-kantra)   SKIP_KANTRA=true;   shift ;;
        --skip-fix)      SKIP_FIX=true;      shift ;;
        --release)       RELEASE=true;       shift ;;
        --work-dir)      WORK_DIR="$2";      shift 2 ;;
        --rename-patterns) RENAME_PATTERNS="$2"; shift 2 ;;
        --no-pipeline-v2) PIPELINE_V2=false; shift ;;
        --help|-h)       usage ;;
        *) echo "Unknown option: $1"; usage ;;
    esac
done

# ── Directories ──────────────────────────────────────────────────────────

PF_CSS_REPO="$WORK_DIR/patternfly"
REPORT="$WORK_DIR/patternfly-report.json"
RULES_BUILTIN="$WORK_DIR/konveyor-rules-builtin"
RULES_FRONTEND="$WORK_DIR/konveyor-rules-frontend"
FIX_GUIDANCE="$WORK_DIR/fix-guidance"
RULES_STRATEGIES="$FAP_DIR/rules/patternfly-v5-to-v6/fix-strategies.json"
QUIPUCORDS="$WORK_DIR/quipucords-ui"
QUIPUCORDS_FIXED="$WORK_DIR/quipucords-ui-fixed"
KANTRA_OUTPUT="$WORK_DIR/kantra-output"
KANTRA_REPORT="$KANTRA_OUTPUT/output.yaml"
FIX_INPUT="$WORK_DIR/kantra-violations.json"
COMPARISON_MD="$WORK_DIR/comparison-analysis.md"

mkdir -p "$WORK_DIR"

echo "============================================================"
echo "  semver-analyzer Integration Pipeline"
echo "============================================================"
echo "  Work dir:     $WORK_DIR"
echo "  PF range:     $PF_FROM → $PF_TO"
echo "  semver-analyzer: $SEMVER_DIR"
echo "  frontend-analyzer-provider: $FAP_DIR"
echo "============================================================"
echo ""

# ── Step 0: Build tools ─────────────────────────────────────────────────

if [[ "$SKIP_BUILD" == false ]]; then
    echo "==> Building semver-analyzer..."
    if [[ "$RELEASE" == true ]]; then
        (cd "$SEMVER_DIR" && cargo build --release 2>&1 | tail -3)
        SEMVER_BIN="$SEMVER_DIR/target/release/semver-analyzer"
    else
        (cd "$SEMVER_DIR" && cargo build 2>&1 | tail -3)
        SEMVER_BIN="$SEMVER_DIR/target/debug/semver-analyzer"
    fi

    echo "==> Building frontend-analyzer-provider..."
    if [[ "$RELEASE" == true ]]; then
        (cd "$FAP_DIR" && cargo build --release 2>&1 | tail -3)
        FAP_BIN="$FAP_DIR/target/release/frontend-analyzer-provider"
    else
        (cd "$FAP_DIR" && cargo build 2>&1 | tail -3)
        FAP_BIN="$FAP_DIR/target/debug/frontend-analyzer-provider"
    fi
else
    if [[ "$RELEASE" == true ]]; then
        SEMVER_BIN="$SEMVER_DIR/target/release/semver-analyzer"
        FAP_BIN="$FAP_DIR/target/release/frontend-analyzer-provider"
    else
        SEMVER_BIN="$SEMVER_DIR/target/debug/semver-analyzer"
        FAP_BIN="$FAP_DIR/target/debug/frontend-analyzer-provider"
    fi
fi

echo "  semver-analyzer:  $SEMVER_BIN"
echo "  provider:         $FAP_BIN"
echo ""

# ── Step 1: Run semver-analyzer against PatternFly ───────────────────────

if [[ "$SKIP_ANALYSIS" == false ]]; then
    echo "==> Step 1: Analyzing PatternFly $PF_FROM → $PF_TO..."
    echo "    Repo: $PF_REPO"

    # Clone the CSS repo for CSS profile extraction + dep-update rule
    if [[ ! -d "$PF_CSS_REPO/.git" ]]; then
        echo "    Cloning PatternFly CSS from $PF_CSS_REPO_URL..."
        git clone "$PF_CSS_REPO_URL" "$PF_CSS_REPO"
    fi
    (cd "$PF_CSS_REPO" && git checkout "$PF_TO" --force 2>/dev/null || true)

    PIPELINE_FLAG=""
    if [[ "$PIPELINE_V2" == true ]]; then
        PIPELINE_FLAG="--pipeline-v2"
    fi

    DEP_REPO_FLAG=""
    if [[ -d "$PF_CSS_REPO/.git" ]]; then
        DEP_REPO_FLAG="--dep-repo $PF_CSS_REPO"
        echo "    CSS dep repo: $PF_CSS_REPO"
    fi

    "$SEMVER_BIN" analyze \
        --repo "$PF_REPO" \
        --from "$PF_FROM" \
        --to "$PF_TO" \
        --no-llm \
        --build-command "yarn build:generate && yarn build:esm" \
        $PIPELINE_FLAG \
        $DEP_REPO_FLAG \
        -o "$REPORT"

    echo ""
    echo "    Report: $REPORT"
    echo "    Size: $(wc -c < "$REPORT" | tr -d ' ') bytes"

    if command -v jq >/dev/null 2>&1; then
        echo "    Breaking changes: $(jq '.summary.total_breaking_changes' "$REPORT")"
        echo "    API changes: $(jq '.summary.breaking_api_changes' "$REPORT")"
        echo "    Behavioral: $(jq '.summary.breaking_behavioral_changes' "$REPORT")"
        if [[ "$PIPELINE_V2" == true ]]; then
            echo "    SD source-level: $(jq '.sd_result.source_level_changes | length' "$REPORT")"
            echo "    SD composition trees: $(jq '.sd_result.composition_trees | length' "$REPORT")"
            echo "    SD conformance checks: $(jq '.sd_result.conformance_checks | length' "$REPORT")"
        fi
    fi
    echo ""
else
    echo "==> Step 1: SKIPPED (reusing $REPORT)"
    echo ""
fi

# ── Step 2: Generate Konveyor rules ─────────────────────────────────────

echo "==> Step 2: Generating Konveyor rules..."

# Build flags
RENAME_FLAG=""
if [[ -f "$RENAME_PATTERNS" ]]; then
    RENAME_FLAG="--rename-patterns $RENAME_PATTERNS"
    echo "    Using token mappings: $RENAME_PATTERNS"
fi

PIPELINE_FLAG=""
if [[ "$PIPELINE_V2" == true ]]; then
    PIPELINE_FLAG="--pipeline-v2"
    echo "    Pipeline: v2 (TD+SD)"
fi

# Generate Konveyor rules
echo "    2a: generating rules..."
"$SEMVER_BIN" konveyor \
    --from-report "$REPORT" \
    --output-dir "$RULES_BUILTIN" \
    $RENAME_FLAG \
    $PIPELINE_FLAG

# Also generate in the frontend rules directory (same output, for kantra)
echo "    2b: copying to frontend rules dir..."
"$SEMVER_BIN" konveyor \
    --from-report "$REPORT" \
    --output-dir "$RULES_FRONTEND" \
    $RENAME_FLAG \
    $PIPELINE_FLAG

echo ""
if command -v yq >/dev/null 2>&1; then
    BUILTIN_COUNT=$(yq 'length' "$RULES_BUILTIN/breaking-changes.yaml" 2>/dev/null || echo "?")
    FRONTEND_COUNT=$(yq 'length' "$RULES_FRONTEND/breaking-changes.yaml" 2>/dev/null || echo "?")
    echo "    Builtin rules:  $BUILTIN_COUNT"
    echo "    Frontend rules: $FRONTEND_COUNT"

    FIX_DIR_BUILTIN="$(dirname "$RULES_BUILTIN")/fix-guidance"
    if [[ -f "$FIX_DIR_BUILTIN/fix-guidance.yaml" ]]; then
        echo "    Fix guidance:"
        yq '.summary' "$FIX_DIR_BUILTIN/fix-guidance.yaml" 2>/dev/null | sed 's/^/      /'
    fi
fi
echo ""

# ── Step 3: Clone quipucords-ui ─────────────────────────────────────────

echo "==> Step 3: Preparing quipucords-ui..."

if [[ -d "$QUIPUCORDS/.git" ]]; then
    echo "    Using existing clone at $QUIPUCORDS, hard-resetting to $QUIPUCORDS_V5_COMMIT"
    (cd "$QUIPUCORDS" && git checkout "$QUIPUCORDS_V5_COMMIT" --force 2>/dev/null && git clean -fd 2>/dev/null)
else
    echo "    Cloning from $QUIPUCORDS_REPO..."
    if [[ -d "/tmp/quipucords-ui/.git" ]]; then
        echo "    (Copying from cached /tmp/quipucords-ui)"
        cp -a /tmp/quipucords-ui "$QUIPUCORDS"
        (cd "$QUIPUCORDS" && git checkout "$QUIPUCORDS_V5_COMMIT" --force 2>/dev/null && git clean -fd 2>/dev/null)
    else
        git clone "$QUIPUCORDS_REPO" "$QUIPUCORDS"
        (cd "$QUIPUCORDS" && git checkout "$QUIPUCORDS_V5_COMMIT" --force 2>/dev/null)
    fi
fi

# Make a copy for fixing (preserve original v5)
rm -rf "$QUIPUCORDS_FIXED"
cp -a "$QUIPUCORDS" "$QUIPUCORDS_FIXED"

echo "    v5 baseline:     $QUIPUCORDS (commit $QUIPUCORDS_V5_COMMIT)"
echo "    Fix target:      $QUIPUCORDS_FIXED"
echo ""

# ── Step 4: Run kantra analysis ─────────────────────────────────────────

if [[ "$SKIP_KANTRA" == false ]]; then
    echo "==> Step 4: Running kantra analysis with frontend-analyzer-provider..."

    # Start the gRPC provider in the background
    echo "    Starting frontend-analyzer-provider on port 9001..."

    # Write provider settings pointing at the fix target
    cat > "$WORK_DIR/provider_settings.json" <<PSJSON
[
    {
        "name": "frontend",
        "address": "localhost:9001",
        "initConfig": [
            {
                "analysisMode": "source-only",
                "location": "$QUIPUCORDS_FIXED"
            }
        ]
    },
    {
        "name": "builtin",
        "initConfig": [
            {
                "location": "$QUIPUCORDS_FIXED"
            }
        ]
    }
]
PSJSON

    "$FAP_BIN" serve --port 9001 &
    PROVIDER_PID=$!
    echo "    Provider PID: $PROVIDER_PID"
    sleep 2

    # Run kantra with the HAND-CRAFTED rules (to compare against)
    echo "    Running kantra with hand-crafted rules..."
    rm -rf "$KANTRA_OUTPUT"
    mkdir -p "$KANTRA_OUTPUT"

    kantra analyze \
        --input "$QUIPUCORDS_FIXED" \
        --output "$KANTRA_OUTPUT" \
        --rules "$FAP_DIR/rules/patternfly-v5-to-v6" \
        --override-provider-settings "$WORK_DIR/provider_settings.json" \
        --enable-default-rulesets=false \
        --skip-static-report \
        --no-dependency-rules \
        --mode source-only \
        --run-local \
        --overwrite \
        --provider java \
        2>&1 | tail -5

    # Also run with our auto-generated rules
    echo "    Running kantra with auto-generated rules..."
    KANTRA_OUTPUT_AUTO="$WORK_DIR/kantra-output-auto"

    kantra analyze \
        --input "$QUIPUCORDS_FIXED" \
        --output "$KANTRA_OUTPUT_AUTO" \
        --rules "$RULES_FRONTEND" \
        --override-provider-settings "$WORK_DIR/provider_settings.json" \
        --enable-default-rulesets=false \
        --skip-static-report \
        --no-dependency-rules \
        --mode source-only \
        --run-local \
        --overwrite \
        --provider java \
        2>&1 | tail -5

    # Stop the provider
    kill "$PROVIDER_PID" 2>/dev/null || true
    wait "$PROVIDER_PID" 2>/dev/null || true
    echo "    Provider stopped."
    echo ""

    # Convert YAML to JSON for fix engine
    if [[ -f "$KANTRA_OUTPUT/output.yaml" ]]; then
        echo "    Converting hand-crafted output to JSON..."
        yq -o=json '.' "$KANTRA_OUTPUT/output.yaml" > "$FIX_INPUT"
    fi

    # Count violations
    if command -v yq >/dev/null 2>&1; then
        HANDCRAFTED_VIOLATIONS=$(yq '.[].violations | length' "$KANTRA_OUTPUT/output.yaml" 2>/dev/null | awk '{sum+=$1} END {print sum}')
        AUTO_VIOLATIONS=$(yq '.[].violations | length' "$KANTRA_OUTPUT_AUTO/output.yaml" 2>/dev/null | awk '{sum+=$1} END {print sum}')
        echo "    Hand-crafted violations: $HANDCRAFTED_VIOLATIONS"
        echo "    Auto-generated violations: $AUTO_VIOLATIONS"
    fi
    echo ""
else
    echo "==> Step 4: SKIPPED (reusing $KANTRA_OUTPUT)"
    if [[ -f "$KANTRA_OUTPUT/output.yaml" ]]; then
        yq -o=json '.' "$KANTRA_OUTPUT/output.yaml" > "$FIX_INPUT"
    fi
    echo ""
fi

# ── Step 5: Apply fixes ─────────────────────────────────────────────────

if [[ "$SKIP_FIX" == false ]] && [[ -f "$FIX_INPUT" ]]; then
    echo "==> Step 5: Applying pattern-based fixes..."

    # Reset the fixed copy from the clean baseline so re-runs
    # don't stack fixes on top of a previous run's output.
    echo "    Resetting quipucords-ui-fixed from clean baseline..."
    rm -rf "$QUIPUCORDS_FIXED"
    cp -a "$QUIPUCORDS" "$QUIPUCORDS_FIXED"

    "$FAP_BIN" fix "$QUIPUCORDS_FIXED" \
        --input "$FIX_INPUT" \
        --apply \
        --rules-strategies "$RULES_STRATEGIES" \
        2>&1 | tail -10

    echo ""
    echo "    Fixed files:"
    (cd "$QUIPUCORDS_FIXED" && git diff --stat 2>/dev/null | tail -5)
    echo ""
else
    echo "==> Step 5: SKIPPED"
    echo ""
fi

# ── Step 6: Compare auto-fixed vs hand-migrated ─────────────────────────

echo "==> Step 6: Generating comparison analysis..."

# Get the hand-migrated version
QUIPUCORDS_V6="$WORK_DIR/quipucords-ui-v6"
if [[ ! -d "$QUIPUCORDS_V6/.git" ]]; then
    cp -a "$QUIPUCORDS" "$QUIPUCORDS_V6"
    (cd "$QUIPUCORDS_V6" && git checkout main --force 2>/dev/null)
fi

# Count changes
FIXED_CHANGES=$(cd "$QUIPUCORDS_FIXED" && git diff --numstat HEAD 2>/dev/null | wc -l | tr -d ' ')
if [[ "$FIXED_CHANGES" == "0" ]]; then
    # Maybe fix already committed or repo was reset; count vs original commit
    FIXED_CHANGES=$(cd "$QUIPUCORDS_FIXED" && git diff --numstat "$QUIPUCORDS_V5_COMMIT" HEAD 2>/dev/null | wc -l | tr -d ' ')
fi
V5_TO_V6_CHANGES=$(cd "$QUIPUCORDS_V6" && git diff "$QUIPUCORDS_V5_COMMIT"..main --numstat 2>/dev/null | wc -l | tr -d ' ')

# Generate diff between auto-fixed and hand-migrated for source files
DIFF_FILE="$WORK_DIR/auto-vs-hand-migrated.diff"
diff -rN --unified=3 \
    --exclude='.git' --exclude='node_modules' --exclude='*.lock' --exclude='dist' --exclude='__snapshots__' \
    "$QUIPUCORDS_FIXED/src" "$QUIPUCORDS_V6/src" \
    > "$DIFF_FILE" 2>/dev/null || true

DIFF_LINES=$(wc -l < "$DIFF_FILE" | tr -d ' ')

# Generate the comparison markdown
cat > "$COMPARISON_MD" <<'HEADER'
# Integration Pipeline: Auto-Fix vs Hand-Migrated Comparison

## Pipeline Summary

HEADER

cat >> "$COMPARISON_MD" <<SUMMARY
| Step | Output |
|---|---|
| PatternFly analysis | \`$REPORT\` |
| Konveyor rules (builtin) | \`$RULES_BUILTIN/\` |
| Konveyor rules (frontend) | \`$RULES_FRONTEND/\` |
| Fix guidance | \`$(dirname "$RULES_BUILTIN")/fix-guidance/\` |
| Kantra output (hand-crafted) | \`$KANTRA_OUTPUT/\` |
| Kantra output (auto-generated) | \`$WORK_DIR/kantra-output-auto/\` |
| Auto-fixed quipucords | \`$QUIPUCORDS_FIXED/\` |
| Hand-migrated quipucords | \`$QUIPUCORDS_V6/\` |
| Full diff | \`$DIFF_FILE\` |

SUMMARY

# Breaking changes summary from the report
if command -v jq >/dev/null 2>&1 && [[ -f "$REPORT" ]]; then
    TOTAL=$(jq '.summary.total_breaking_changes' "$REPORT")
    API=$(jq '.summary.breaking_api_changes' "$REPORT")
    BEHAVIORAL=$(jq '.summary.breaking_behavioral_changes' "$REPORT")
    FILES=$(jq '.summary.files_with_breaking_changes' "$REPORT")
    cat >> "$COMPARISON_MD" <<ANALYSIS
## Breaking Changes Detected

| Metric | Count |
|---|---|
| Total breaking changes | $TOTAL |
| API changes | $API |
| Behavioral changes | $BEHAVIORAL |
| Files with changes | $FILES |

ANALYSIS
fi

# Kantra violation counts
if command -v yq >/dev/null 2>&1; then
    if [[ -f "$KANTRA_OUTPUT/output.yaml" ]]; then
        HC_TOTAL=$(yq '.[].violations | length' "$KANTRA_OUTPUT/output.yaml" 2>/dev/null | awk '{sum+=$1} END {print sum}')
        HC_INCIDENTS=$(yq '[.[].violations[].incidents[]] | length' "$KANTRA_OUTPUT/output.yaml" 2>/dev/null || echo "N/A")
    else
        HC_TOTAL="N/A"
        HC_INCIDENTS="N/A"
    fi

    if [[ -f "$WORK_DIR/kantra-output-auto/output.yaml" ]]; then
        AUTO_TOTAL=$(yq '.[].violations | length' "$WORK_DIR/kantra-output-auto/output.yaml" 2>/dev/null | awk '{sum+=$1} END {print sum}')
        AUTO_INCIDENTS=$(yq '[.[].violations[].incidents[]] | length' "$WORK_DIR/kantra-output-auto/output.yaml" 2>/dev/null || echo "N/A")
    else
        AUTO_TOTAL="N/A"
        AUTO_INCIDENTS="N/A"
    fi

    cat >> "$COMPARISON_MD" <<KANTRA_SECTION
## Kantra Analysis: Hand-Crafted vs Auto-Generated Rules

| Metric | Hand-Crafted Rules | Auto-Generated Rules |
|---|---|---|
| Violation types | $HC_TOTAL | $AUTO_TOTAL |
| Total incidents | $HC_INCIDENTS | $AUTO_INCIDENTS |

KANTRA_SECTION

    # Extract violation details for hand-crafted rules
    if [[ -f "$KANTRA_OUTPUT/output.yaml" ]]; then
        echo "### Hand-Crafted Rule Violations" >> "$COMPARISON_MD"
        echo "" >> "$COMPARISON_MD"
        echo "| Rule ID | Description | Incidents |" >> "$COMPARISON_MD"
        echo "|---|---|---|" >> "$COMPARISON_MD"
        yq '.[].violations[] | .description + " | " + (.incidents | length | tostring)' \
            "$KANTRA_OUTPUT/output.yaml" 2>/dev/null | \
            while IFS= read -r line; do
                # Extract description and count from the combined string
                desc="${line% | *}"
                count="${line##* | }"
                echo "| - | $desc | $count |" >> "$COMPARISON_MD"
            done
        echo "" >> "$COMPARISON_MD"
    fi

    # Extract violation details for auto-generated rules
    if [[ -f "$WORK_DIR/kantra-output-auto/output.yaml" ]]; then
        echo "### Auto-Generated Rule Violations" >> "$COMPARISON_MD"
        echo "" >> "$COMPARISON_MD"
        echo "| Rule ID | Description | Incidents |" >> "$COMPARISON_MD"
        echo "|---|---|---|" >> "$COMPARISON_MD"
        yq '.[].violations[] | .description + " | " + (.incidents | length | tostring)' \
            "$WORK_DIR/kantra-output-auto/output.yaml" 2>/dev/null | \
            while IFS= read -r line; do
                desc="${line% | *}"
                count="${line##* | }"
                echo "| - | $desc | $count |" >> "$COMPARISON_MD"
            done
        echo "" >> "$COMPARISON_MD"
    fi
fi

# Diff analysis
cat >> "$COMPARISON_MD" <<DIFF_SECTION
## Auto-Fixed vs Hand-Migrated Diff

| Metric | Value |
|---|---|
| Files changed by auto-fix | $FIXED_CHANGES |
| Files changed in hand migration (v5→v6) | $V5_TO_V6_CHANGES |
| Diff lines (auto-fixed vs hand-migrated) | $DIFF_LINES |

DIFF_SECTION

# Per-file diff — categorize into migration vs non-migration
if [[ -f "$DIFF_FILE" ]] && [[ "$DIFF_LINES" -gt 0 ]]; then
    echo "### Diff Categorization" >> "$COMPARISON_MD"
    echo "" >> "$COMPARISON_MD"

    # Categorize differing files
    DIFF_LIST="$WORK_DIR/diff-file-list.txt"
    diff -rq \
        --exclude='.git' --exclude='node_modules' --exclude='*.lock' --exclude='dist' --exclude='__snapshots__' \
        "$QUIPUCORDS_FIXED/src" "$QUIPUCORDS_V6/src" \
        2>/dev/null > "$DIFF_LIST" || true

    TOTAL_DIFFS=$(wc -l < "$DIFF_LIST" | tr -d ' ')
    NEW_IN_V6=$(grep -c "^Only in.*quipucords-ui-v6" "$DIFF_LIST" 2>/dev/null || echo "0")
    REMOVED_IN_V6=$(grep -c "^Only in.*quipucords-ui-fixed" "$DIFF_LIST" 2>/dev/null || echo "0")
    MODIFIED=$(grep -c "^Files.*differ$" "$DIFF_LIST" 2>/dev/null || echo "0")

    # Count PF-related diffs (files with @patternfly imports that differ)
    PF_RELATED=0
    NON_PF=0
    while IFS= read -r line; do
        if [[ "$line" == Files* ]]; then
            file=$(echo "$line" | awk '{print $2}')
            if grep -q "@patternfly" "$file" 2>/dev/null; then
                PF_RELATED=$((PF_RELATED + 1))
            else
                NON_PF=$((NON_PF + 1))
            fi
        fi
    done < "$DIFF_LIST"

    # Count vendor files
    VENDOR_DIFFS=$(grep -c "vendor/" "$DIFF_LIST" 2>/dev/null || echo "0")
    LOCALES_DIFFS=$(grep -c "locales/" "$DIFF_LIST" 2>/dev/null || echo "0")
    TEST_DIFFS=$(grep -c "__test\|\.test\." "$DIFF_LIST" 2>/dev/null || echo "0")

    cat >> "$COMPARISON_MD" <<CATEGORIZATION
| Category | Count | Description |
|---|---|---|
| **Total files differing** | $TOTAL_DIFFS | All differences between auto-fixed and hand-migrated |
| New files in v6 | $NEW_IN_V6 | New features added during migration (not migration debt) |
| Files removed in v6 | $REMOVED_IN_V6 | Files deleted during migration |
| Files modified | $MODIFIED | Content differs between the two versions |
| **PF-related modified** | $PF_RELATED | Files with \`@patternfly\` imports that still differ |
| Non-PF modified | $NON_PF | Files without PF imports (business logic, config) |
| Vendor files | $VENDOR_DIFFS | Third-party vendored code updates |
| Locales/i18n files | $LOCALES_DIFFS | Externalized translation strings |
| Test files | $TEST_DIFFS | Test file changes |

**True PF migration gap: $PF_RELATED files** — these are the files where PF component API
changes were not fully addressed by auto-fix. The remaining $NON_PF non-PF files, $NEW_IN_V6
new files, and $VENDOR_DIFFS vendor updates are not migration debt.

CATEGORIZATION

    # Show just the PF-related differing files
    echo "### PF-Related Files Still Differing" >> "$COMPARISON_MD"
    echo "" >> "$COMPARISON_MD"
    echo '```' >> "$COMPARISON_MD"
    while IFS= read -r line; do
        if [[ "$line" == Files* ]]; then
            file=$(echo "$line" | awk '{print $2}')
            if grep -q "@patternfly" "$file" 2>/dev/null; then
                basename "$file"
            fi
        fi
    done < "$DIFF_LIST" >> "$COMPARISON_MD"
    echo '```' >> "$COMPARISON_MD"
    echo "" >> "$COMPARISON_MD"
fi

# Category breakdown of what was fixed vs what remains
if [[ -f "$(dirname "$RULES_BUILTIN")/fix-guidance/fix-guidance.yaml" ]]; then
    echo "### Fix Guidance Summary" >> "$COMPARISON_MD"
    echo "" >> "$COMPARISON_MD"
    echo '```yaml' >> "$COMPARISON_MD"
    yq '.summary' "$(dirname "$RULES_BUILTIN")/fix-guidance/fix-guidance.yaml" \
        2>/dev/null >> "$COMPARISON_MD" || true
    echo '```' >> "$COMPARISON_MD"
    echo "" >> "$COMPARISON_MD"
fi

# Conclusion
cat >> "$COMPARISON_MD" <<'CONCLUSION'
## Conclusions

### What the auto-generated pipeline covers
- API-level breaking changes (renamed/removed components, props, types)
- DOM structure changes (wrapper elements, element type changes)
- Accessibility changes (ARIA attributes, role changes)
- CSS class and variable renames
- Fix guidance with strategy, confidence, and concrete instructions
- Machine-readable fix strategies (fix-strategies.json) for the provider fix engine

### What still requires manual attention
- Prop value changes (e.g., `variant="tertiary"` → `variant="horizontal-subnav"`)
- Complex structural refactors (e.g., Modal → ModalHeader/ModalBody composition)
- CSS-in-JS style changes

### What is NOT migration debt (exclude from gap count)
- New features added during the hand migration
- i18n externalization (architecture decision)
- Type system refactoring (business logic)
- Vendor library updates
- API layer improvements

### Key metric
The true PF migration gap is measured by the number of PF-related files
that still differ after auto-fix — not the total diff count. Non-PF changes
are feature additions and refactors done concurrently with the migration.
CONCLUSION

echo ""
echo "============================================================"
echo "  Pipeline Complete"
echo "============================================================"
echo ""
echo "  Report:         $REPORT"
echo "  Rules:          $RULES_BUILTIN/ (builtin)"
echo "                  $RULES_FRONTEND/ (frontend)"
echo "  Fix guidance:   $(dirname "$RULES_BUILTIN")/fix-guidance/"
echo "  Kantra output:  $KANTRA_OUTPUT/ (hand-crafted)"
echo "                  $WORK_DIR/kantra-output-auto/ (auto-generated)"
echo "  Fixed code:     $QUIPUCORDS_FIXED/"
echo "  Comparison:     $COMPARISON_MD"
echo "  Full diff:      $DIFF_FILE"
echo ""
echo "  Read the analysis:"
echo "    cat $COMPARISON_MD"
echo ""
