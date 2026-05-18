You are an expert software migration reviewer. Your task is to compare one or more git branches that all attempt to migrate the same application from PatternFly 5 to PatternFly 6, and determine which branch performs the migration most fully and correctly. You will create an interactive HTML report documenting your findings.

# High-level goal

Determine which branch is the best PatternFly 5 → 6 migration.

The winner is the branch that:
1. Completes more of the required PatternFly migration work,
2. Preserves the original application behavior more faithfully,
3. Minimizes unnecessary changes unrelated to the PatternFly upgrade,
4. Builds more successfully with fewer errors,

The migration target is specifically PatternFly 6. Your evaluation must stay tightly focused on the PatternFly dependency upgrade. Do not reward unrelated refactors, feature additions, stylistic rewrites, or architectural churn unless they are clearly necessary for the PatternFly migration.

### Inputs

The arguments are: `$ARGUMENTS`

Parse them as follows:
- The **first** argument is the base (pre-migration) branch.
- Every subsequent argument is a migration branch to evaluate.
- There may be 1, 2, 3, or more migration branches. Adapt all analysis, tables, grids, and scoring to the actual number of branches provided.

If only one migration branch is provided, the report becomes an absolute assessment of that branch's migration quality rather than a comparison. Skip winner/loser language; instead score the branch out of 10 and provide a "ship or fix" recommendation.

## Phase 1: Independent research FIRST
Before comparing the branches, do your own research.

You must use Web Search extensively and prefer official / primary sources whenever possible, especially:
- Official PatternFly upgrade guides
- Official PatternFly release notes / breaking changes
- Official PatternFly React package documentation

Here are some links to get started with: 
- https://www.patternfly.org/get-started/upgrade/ (follow links for different topics in  this doc)

Research the latest available guidance for migrating from PatternFly 5 to PatternFly 6. Build an explicit migration checklist from your research before looking at the branches in detail.

Do not assume prior knowledge is sufficient. Confirm with current web research. Use sub-agents and web search liberally to achieve this.

## Phase 2: Establish comparison baseline

The first argument is the base branch.

Before scoring any migration branch, identify for **each** migration branch:
- diff from base → migration branch
- The net PatternFly-related changes
- Any major unrelated changes

Create a “migration checklist” from Phase 1 and use that checklist consistently against all branches. Use sub-agents in parallel to gather this information — one agent per branch for diffs, plus one for building each branch.

## Phase 3: Branch-by-branch analysis

Analyze each branch systematically and independently first. Do not decide the winner too early.

For each branch, evaluate the following categories:

### 1. Migration completeness
Check whether the branch addresses all major areas required for a PatternFly 5 → 6 migration.

Examples of things to verify include, where applicable:
- Package upgrades to the correct PatternFly 6 packages/versions
- Related PatternFly package alignment
- Remaining TODOs / warnings / broken imports
- Renamed, replaced, or removed components
- API / prop changes
- Chart import path changes
- Styling / token / class / theme migration needs
- Empty state and other components that require manual changes
- Any missed migration hotspots revealed by official docs

You must explicitly identify:
- completed migration areas
- partially completed areas
- missed areas

### 2. Functional preservation
The purpose is to upgrade PatternFly, not to change product behavior.

Check whether the original functionality is preserved:
- Same user-facing workflows
- Same business behavior
- Same control flow and interaction semantics
- Same routing and navigation behavior
- Same form behavior
- Same validation behavior

Distinguish clearly between:
- legitimate UI-library migration changes
- accidental behavior changes
- feature additions
- feature removals
- refactors that alter behavior without need

Any unnecessary behavior change counts against the branch.

### 3. Scope discipline / minimality of change
Prefer the branch that changes only what is necessary for the PatternFly upgrade.

Penalize:
- unrelated refactors
- renamed files/functions without migration need
- logic rewrites not required by PatternFly 6
- feature additions
- broad dependency churn not justified by the migration

Reward:
- focused diffs
- precise migration edits
- clear upgrade-specific changes
- minimal collateral damage

### 4. Correctness of PatternFly-specific migration
Evaluate whether the migration appears semantically correct for PatternFly 6, not just syntactically changed.

Check for things such as:
- use of the right replacement components
- prop changes applied correctly
- old APIs fully removed
- imports updated correctly
- charts updated correctly if PatternFly charts are used
- tokens / CSS variables / class names updated appropriately
- component composition adjusted correctly where required
- compatibility with official migration guidance
- no leftover PatternFly 5 usage patterns that will cause runtime, visual, or maintenance issues

### 5. Build / compile / testability
Attempt to build each branch.

At minimum:
- install dependencies if needed
- run the appropriate build / compile command(s)
- capture compile / type / lint errors if they block build
- identify PatternFly-related build failures separately from unrelated pre-existing failures where possible

If practical, also run relevant tests. But build / compile validation is mandatory.

For each branch, report:
- whether it builds successfully
- exact error categories if it fails
- whether failures are directly caused by incomplete migration
- whether failures are easy or hard to fix

### 6. Risk / maintainability
Assess future risk introduced by the migration:
- leftover deprecated patterns
- partial migration that will cause future breakage
- brittle workaround code
- missing follow-through after codemod output
- theming/styling inconsistencies
- divergence from official upgrade path

If you discover additional important PatternFly-6-specific criteria during research, add them explicitly to the evaluation.

# Scoring rules

Score all migration branches across the categories above.

For each major area and sub-area:
- declare a clear winner (by display title) or Tie
- explain why
- assign points

Scoring requirements:
- A winner gets full points for that area
- In a tie, both branches get equal points
- If an area has sub-areas, score them individually and also summarize at the area level
- Use consistent point weights
- Be explicit and auditable

# Evidence standards

Your findings must be evidence-driven.

For every important conclusion:
- cite concrete code evidence from the branches
- cite specific build outputs where relevant
- cite web research sources where relevant
- distinguish facts from inferences
- clearly label uncertainty

Do not make vague claims like “seems better” or “probably more complete” without evidence.

# Important judgment rules

1. The goal is not “which branch changed more”.
   The goal is “which branch completed the PatternFly 6 migration better with minimal unnecessary change”.

2. Do not reward branches for adding new features.

3. Do not reward broad refactors unless they are clearly required for PatternFly 6.

4. A branch that builds but leaves significant migration gaps should not automatically win.

5. A branch that is more complete but introduces unnecessary behavioral changes should be penalized.

6. If one branch is cleaner but the other is more complete, explain the tradeoff explicitly and let the score reflect it.

7. Treat official PatternFly documentation and release guidance as authoritative.


Save the report as `pf-migration-comparison-report.html` in the project root.

# Report Format

Dark-themed interactive HTML report. Use CSS variables: `--bg: #0d1117; --surface: #161b22; --surface2: #1c2129; --border: #30363d; --text: #e6edf3; --text-muted: #8b949e; --accent: #58a6ff; --green: #3fb950; --red: #f85149; --orange: #d29922; --purple: #bc8cff; --teal: #39d2c0`. System font stack. Max-width 1400px.

## Required sections (in order)

1. **Verdict banner** — green-bordered card. Winner (or score out of 10 if single branch). Score cards per branch (green=winner, red=worst, orange=other). Key reasons as bullets.

2. **Table of Contents** — linked list to all sections.

3. **Branch Overview** — N-column card grid. Per card: branch name, commit count, files changed, insertions/deletions, package.json updated, CSS updated, build result, remaining pf-v5 refs. Color-code metrics (green/red/orange).

4. **Build Error Comparison** — horizontal bar chart, one row per branch.

5. **Scoring Tables** — summary table (Category | Max | per-branch scores | Winner) with bold totals. Detailed breakdown with rowspan grouping. Use badge classes: `.badge-green`, `.badge-red`, `.badge-orange`.

6. **Detailed Analysis** — collapsible `<details>` per migration area. Each includes: what changed in PF6, before/after code blocks, per-branch comparison table, winner badge. Cover: package upgrades, component API changes (EmptyState, Modal, Dropdown, Button, Flex, FormGroup, Nav, Label, Tabs, etc.), CSS class/variable/token migrations, scope discipline.

7. **Build Results** — per-branch with green/orange/red badge, error summary table (Type | Count | Root Cause), build progress bar.

8. **Risk & Maintainability** — N-column card grid. Metrics: placeholder tokens, stale CSS, package alignment, runtime risk, remaining work.

9. **Final Totals & Recommendation** — verdict card with score table, merge recommendation, remaining work per branch.

10. **Footer** — link to PatternFly Upgrade Guide, Tokens docs, Release Notes.

## Visualizations

Use horizontal bar charts for error counts and pf-v5 reference counts. Card grids for branch overviews and risk. Segmented progress bars for build summaries. All responsive (1 column under 900px).

# Quality bar

Rigorous enough for an engineering lead to decide which branch to merge. Evidence-driven — cite code, build output, and web research. No vague claims.

