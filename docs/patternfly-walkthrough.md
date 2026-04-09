# PatternFly Migration Analysis Walkthrough

Step-by-step guide for analyzing breaking changes between PatternFly React v5 and v6. This serves as both a practical tutorial and a reference for how to run the analyzer against a large real-world React component library.

## Prerequisites

- **Rust** (stable toolchain) -- to build semver-analyzer
- **Node.js >= 18** -- required by PatternFly v6. Use [nvm](https://github.com/nvm-sh/nvm) or [fnm](https://github.com/Schniz/fnm) to manage versions
- **Yarn** -- PatternFly's package manager (`npm install -g yarn`)
- **Git** -- for cloning repos and creating worktrees
- **~10 GB disk space** -- the analyzer creates worktrees at both refs, each with full `node_modules`

Verify your setup:

```bash
rustc --version    # any stable version
node --version     # v18.x or later
yarn --version     # 1.x (classic)
git --version
```

## Quick Path: Using the Script

The fastest way to run against PatternFly:

```bash
# Build the analyzer
cargo build --release

# Run with defaults (v5.4.0 -> v6.4.0, static analysis only)
hack/run-patternfly.sh

# Or customize refs and output
hack/run-patternfly.sh \
  --from v5.4.0 \
  --to v6.4.1 \
  --output my-report.json \
  --konveyor           # also generate Konveyor rules
```

The script clones PatternFly React from GitHub, runs the analysis, and writes the report. Use `--keep` to preserve the cloned repo for subsequent runs. Use `--repo /path/to/existing/clone` to skip cloning.

## Manual Setup

### Step 1: Clone the repos

```bash
# PatternFly React (main library)
git clone https://github.com/patternfly/patternfly-react.git
cd patternfly-react
git fetch --tags

# PatternFly CSS (optional, for CSS profile analysis)
cd ..
git clone https://github.com/patternfly/patternfly.git patternfly-css
cd patternfly-css
git fetch --tags
```

### Step 2: Identify the CSS dependency tags

The CSS repo tags correspond to specific PatternFly React versions. For v5 -> v6:

```bash
# Find the latest v5 and v6 CSS tags
cd patternfly-css
git tag --list 'v5.*' --sort=-v:refname | head -1   # e.g., v5.4.2
git tag --list 'v6.*' --sort=-v:refname | head -1   # e.g., v6.1.0
```

### Step 3: Run the analysis

```bash
# Build semver-analyzer
cargo build --release

# Run analysis with CSS dependency repo
semver-analyzer analyze typescript \
  --repo ./patternfly-react \
  --from v5.4.0 \
  --to v6.4.1 \
  --build-command "yarn build:generate && yarn build:esm" \
  --dep-repo ./patternfly-css \
  --dep-from v5.4.2 \
  --dep-to v6.1.0 \
  --dep-build-command "yarn install && npx gulp buildPatternfly" \
  -o report.json
```

**Build command notes:**

| Flag | Command | Why |
|------|---------|-----|
| `--build-command` | `yarn build:generate && yarn build:esm` | Generates code (icons, etc.) then builds ESM `.d.ts` files. Using `yarn build` also works but is slower |
| `--dep-build-command` | `yarn install && npx gulp buildPatternfly` | Installs CSS repo dependencies and compiles SASS to CSS |

The analysis takes 3-10 minutes depending on hardware (most time is spent in `yarn install` and `tsc` in the worktrees).

### Step 4: Generate Konveyor rules

```bash
semver-analyzer konveyor typescript \
  --from-report report.json \
  --output-dir ./rules
```

For more precise rules, supply a rename patterns file if you have one:

```bash
semver-analyzer konveyor typescript \
  --from-report report.json \
  --output-dir ./rules \
  --rename-patterns rename-patterns.yaml
```

See [docs/konveyor-rules.md](konveyor-rules.md) for details on rule customization.

## Understanding the Output

### Report summary

For PatternFly v5.4.0 -> v6.4.1, expect approximately:

| Metric | Count |
|--------|-------|
| Total breaking changes | ~15,500 |
| Non-token removals | ~340 |
| Renames (mostly CSS tokens) | ~4,094 |
| Type changes | ~3,866 |
| Files with breaking changes | ~650 |

The high total is driven by CSS token renames (the `pf-v5` -> `pf-v6` prefix change affects thousands of constants).

### Key change categories

**Component deprecations** -- Several v5 components moved to `deprecated/` and were replaced:

| v5 Component | v6 Replacement | Detection |
|-------------|----------------|-----------|
| `Chip` | `Label` | Rendering swap (zero name similarity) |
| `Modal` (old API) | `Modal` (new composition API) | Relocation + prop-to-child |
| `Wizard` (old API) | `Wizard` (new composition API) | Relocation |
| `DualListSelector` (old API) | `DualListSelector` (new API) | Relocation |

**Prop-to-child migrations** -- Several components changed from prop-driven to composition-based APIs:

```jsx
// v5: props on parent
<Modal title="My Modal" actions={[<Button>OK</Button>]}>
  Content
</Modal>

// v6: composition with child components
<Modal>
  <ModalHeader title="My Modal" />
  <ModalBody>Content</ModalBody>
  <ModalFooter><Button>OK</Button></ModalFooter>
</Modal>
```

**CSS token prefix changes** -- The `pf-v5` prefix changed to `pf-v6` across all CSS classes and variables. The analyzer detects these as constant renames.

**Composition tree changes** -- New required wrapper components (e.g., `DropdownList` between `Dropdown` and `DropdownItem`).

### Navigating the report

For a structured view, use the `packages` array rather than the flat `changes` array:

```bash
# Count changes per package
cat report.json | jq '.packages[] | {name, types: (.type_summaries | length), constants: (.constants | length)}'

# Find changes for a specific component
cat report.json | jq '.packages[].type_summaries[] | select(.name == "Modal")'
```

See [docs/report-format.md](report-format.md) for the complete report schema.

## Running Rules Against a Consumer App

After generating rules, run them against your application using kantra:

```bash
kantra analyze \
  --rules ./rules \
  --input /path/to/your-app
```

For AST-level rules (`frontend.referenced` conditions), kantra needs a frontend-analyzer-provider server. See [docs/konveyor-rules.md](konveyor-rules.md) for details.

## Troubleshooting

### Build failures

- **`yarn install` fails**: Ensure Node >= 18. PatternFly v6 dropped support for older Node versions.
- **`tsc` fails at one ref but not the other**: Normal -- the build tooling may differ between major versions. The analyzer tries multiple fallback strategies automatically.
- **`gulp` not found for CSS repo**: The CSS repo needs `npx gulp` (gulp is a devDependency, not global). The `--dep-build-command` should use `npx gulp`.

### Disk space

Each worktree includes a full `node_modules` install. For PatternFly React, that's ~2 GB per worktree. With 2 worktrees for the main repo and 2 for the CSS dep repo, total disk usage is ~8-10 GB. Worktrees are cleaned up automatically after analysis.

### Performance

| Phase | Typical time |
|-------|-------------|
| Worktree creation | ~5 seconds |
| `yarn install` (per worktree) | 30-90 seconds |
| `tsc` build (per worktree) | 30-60 seconds |
| API surface extraction | ~10 seconds |
| Diffing | ~5 seconds |
| SD pipeline (source profiles + composition) | 30-60 seconds |
| CSS profile extraction (dep repo) | 10-20 seconds |
| **Total** | **3-10 minutes** |

### Common flags for PatternFly runs

```bash
# Skip CSS dependency analysis (faster, fewer rules)
semver-analyzer analyze typescript \
  --repo ./patternfly-react \
  --from v5.4.0 --to v6.4.1 \
  --build-command "yarn build:generate && yarn build:esm" \
  -o report.json

# With debug logging
semver-analyzer analyze typescript \
  --repo ./patternfly-react \
  --from v5.4.0 --to v6.4.1 \
  --build-command "yarn build:generate && yarn build:esm" \
  --log-file debug.log --log-level debug \
  -o report.json
```
