# feat/per-ref-node-version

## Problem

semver-analyzer creates two temporary git worktrees to compare framework
versions: one for the "from" ref (old version) and one for the "to" ref
(new version). In each worktree it runs `npm install` / `yarn install`,
then `tsc --declaration` to generate `.d.ts` files for API surface extraction.

All of these subprocess calls inherit the parent process's `PATH`, which
means both worktrees use whichever Node.js version happens to be active.
This breaks when the two framework versions require different Node.js
major versions. For example, PatternFly v5 requires Node 16 while
PatternFly v6 requires Node 18+ — analyzing the v5-to-v6 upgrade fails
because one worktree always gets the wrong Node version.

The same class of problem applies to install commands. The old version
may use Yarn Classic (`yarn install --frozen-lockfile`) while the new
version uses Yarn Berry (`yarn install --immutable`). Although the tool
auto-detects the package manager from lockfiles, there is no mechanism
for the user to override the install command per ref when auto-detection
is insufficient.

There is also only a single `--build-command` flag, which applies the
same custom build command to both refs. Some upgrades require different
build steps for the old and new versions.

## Goal

Allow users to specify different Node.js versions, install commands, and
build commands for the "from" and "to" refs via CLI flags. Thread that
per-ref configuration through the entire pipeline so every subprocess
call in a worktree uses the correct environment.

## Approach

### Key architectural decision

The `TypeScript` struct implements the `Language` trait (defined in the
language-agnostic `core` crate) and is shared via `Arc` for both ref
extractions. Rather than modifying the `Language` trait — which would
affect all language implementations — we:

1. Introduce a `RefBuildConfig` struct in the TypeScript worktree module
   that bundles per-ref overrides (node version, install command, build
   command)
2. Add a `ref_config: RefBuildConfig` field to the `TypeScript` struct
   with a new `TypeScript::with_ref_config()` constructor
3. Add `lang_from` / `lang_to` fields to the `Analyzer<L>` struct so
   the orchestrator uses separate language instances for from-ref and
   to-ref extraction, while `lang` remains the shared instance for
   diff and report operations

When no per-ref flags are provided, `RefBuildConfig` defaults to all
`None` and the behavior is identical to the previous code.

### Node.js version resolution

nvm is a shell function, not a binary. To resolve a version specifier
(e.g., `"18"`) to a filesystem path, the tool runs:

```
bash -c "source $NVM_DIR/nvm.sh && nvm which 18"
```

This returns the full path to the node binary (e.g.,
`/Users/x/.nvm/versions/node/v18.20.4/bin/node`). The parent directory
is prepended to `PATH` in every `Command::envs()` call within that
worktree's build pipeline. Git commands are not affected — only
npm/yarn/pnpm/tsc calls.

## Implementation plan

### Step 1: Create `RefBuildConfig` and `nvm` module

**Files:**
- `crates/ts/src/worktree/mod.rs` — add `pub mod nvm`, define and export `RefBuildConfig`
- `crates/ts/src/worktree/nvm.rs` — new file

```rust
#[derive(Debug, Clone, Default)]
pub struct RefBuildConfig {
    pub node_version: Option<String>,
    pub install_command: Option<String>,
    pub build_command: Option<String>,
}
```

`nvm.rs` provides two functions:
- `resolve_node_bin_dir(version) -> Result<PathBuf>` — resolves a version
  via `nvm which`, returns the bin directory
- `build_node_env(node_version) -> Result<Vec<(String, String)>>` — resolves
  the version and returns env vars for `Command::envs()`, or an empty vec
  when `node_version` is `None`

### Step 2: Thread `node_env` through tsc functions

**File:** `crates/ts/src/worktree/tsc.rs`

Add a `node_env: &[(String, String)]` parameter to all four functions:
- `run_tsc_declaration`
- `run_tsc_build`
- `run_tsc_single`
- `run_project_build`

At each `Command::new(...)` call site, add `.envs(node_env)`.

### Step 3: Modify `WorktreeGuard::new()` to accept `RefBuildConfig`

**File:** `crates/ts/src/worktree/guard.rs`

Change the signature from `new(repo, git_ref, build_command)` to
`new(repo, git_ref, config: &RefBuildConfig)`.

Inside `new()`:
1. Resolve the node env once via `nvm::build_node_env()`
2. When `config.install_command` is set, run that command instead of
   auto-detecting the package manager
3. Thread `node_env` to `run_package_install()` and all tsc calls
4. Use `config.build_command` instead of the old bare `build_command`
   parameter

Add a `run_custom_install()` helper for user-provided install commands,
with shell handling for compound commands (`&&`, `||`, `;`).

### Step 4: Update `TypeScript` struct and extract methods

**File:** `crates/ts/src/language.rs`

Add `ref_config: RefBuildConfig` field to `TypeScript`. Add
`with_ref_config(config)` constructor. Update `extract_keeping_worktree()`
to pass `&self.ref_config` to `WorktreeGuard::new()`.

**File:** `crates/ts/src/extract/mod.rs`

Update `extract_at_ref()` to construct a `RefBuildConfig` from its
`build_command` parameter.

### Step 5: Add `lang_from` / `lang_to` to `Analyzer`

**File:** `src/orchestrator.rs`

```rust
pub struct Analyzer<L: Language> {
    pub lang: Arc<L>,
    pub lang_from: Arc<L>,
    pub lang_to: Arc<L>,
}
```

In `run_v2()`: use `self.lang_from` / `self.lang_to` for the concurrent
extraction tasks (previously both used `self.lang`).

In `run_td()` (behavioral pipeline): add `lang_from` and `lang_to`
parameters. Use `lang_from` for from-ref extraction and `lang_to` for
to-ref extraction. `lang` continues to be used for `run_td_analyze`
(structural diff, manifest diff).

### Step 6: Add CLI flags

**File:** `crates/ts/src/cli.rs`

Add to `TsAnalyzeArgs` and `TsKonveyorArgs`:
- `--from-build-command` / `--to-build-command`
- `--from-node-version` / `--to-node-version`
- `--from-install-command` / `--to-install-command`

All grouped under the `"Per-Ref Build"` help heading.

For `TsExtractArgs` (single-ref command): add `--node-version` and
`--install-command`.

The existing `--build-command` flag is preserved as the default for
both refs, overridden by the per-ref variants.

### Step 7: Wire CLI args to Analyzer in `main.rs`

**File:** `src/main.rs`

In `cmd_analyze_ts()` and the konveyor analysis path:
1. Build two `RefBuildConfig` instances from CLI args (from-ref config
   falls back to `--build-command` when `--from-build-command` is absent)
2. Construct `Analyzer` with `lang_from: TypeScript::with_ref_config(from_config)`
   and `lang_to: TypeScript::with_ref_config(to_config)`

In `cmd_extract_ts()`: build a single `RefBuildConfig` from the new
`--node-version` and `--install-command` flags.

## Files changed

| File | Change |
|------|--------|
| `crates/ts/src/worktree/nvm.rs` | New file: nvm resolution + env builder |
| `crates/ts/src/worktree/mod.rs` | `RefBuildConfig` struct, `pub mod nvm` |
| `crates/ts/src/worktree/tsc.rs` | `node_env` param on 4 functions, `.envs()` on 5 Command calls |
| `crates/ts/src/worktree/guard.rs` | `new()` accepts `&RefBuildConfig`, `run_custom_install()`, node env threading |
| `crates/ts/src/language.rs` | `TypeScript` gets `ref_config` field + `with_ref_config()` |
| `crates/ts/src/extract/mod.rs` | `extract_at_ref()` builds `RefBuildConfig` from build_command |
| `crates/ts/src/cli.rs` | 6 new per-ref flags on analyze/konveyor, 2 on extract |
| `src/orchestrator.rs` | `Analyzer` gets `lang_from`/`lang_to`; `run_td()` takes separate lang refs |
| `src/main.rs` | Builds `RefBuildConfig` from CLI args, wires into Analyzer |

## Verification

1. `cargo test --workspace` — all 1,188 existing tests pass
2. `cargo clippy --workspace` — no new warnings
3. `semver-analyzer analyze typescript --help` — new flags appear under "Per-Ref Build"
4. Backward compatible: running without any new flags behaves identically
5. Integration test (requires nvm): analyze with `--from-node-version 18 --to-node-version 20`
   and verify via tracing logs that each worktree resolves the correct Node binary

## Risks and future work

- **nvm only.** The initial implementation supports nvm. Users of fnm, volta,
  or asdf would need future additions using the same pattern (resolve a bin
  directory, prepend to PATH).
- **Concurrent `nvm which` calls are safe.** We only call `nvm which` (read-only)
  to resolve a path — we never call `nvm use` (which mutates shell state).
- **Corepack interaction.** When `package.json` declares a `packageManager`
  field, the install command is wrapped with `corepack`. The corepack binary
  inside the nvm-resolved Node installation is the correct one for that Node
  version, so this should work naturally.
- **`find_tsc_binary()` is unaffected.** It resolves `node_modules/.bin/tsc`
  from the worktree, which was installed by the correct Node version's npm.
