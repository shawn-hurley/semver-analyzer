# feat/per-ref-node-version — Implementation Notes

Notes from the implementation session to help future contributors
working on similar cross-cutting changes in this codebase.

## What went smoothly

### Bottom-up ordering eliminated cascading breakage

The plan was ordered so that each step introduced errors only in the
layer immediately above it. For example, step 2 (adding `node_env` to
tsc functions) broke only the 4 callers in `guard.rs`, which were fixed
in step 3. Step 3 (changing `WorktreeGuard::new()` signature) broke only
the 2 callers in `language.rs` and `extract/mod.rs`, which were fixed in
step 4. At no point were there errors more than one layer deep. This
made it safe to verify compilation incrementally without needing to
hold the entire change in memory.

### The `Analyzer` struct change was clean

Adding `lang_from`/`lang_to` to `Analyzer<L>` was the riskiest
structural change, but it turned out to be straightforward because:
- Only two construction sites exist (both in `main.rs`)
- The `run_v2()` method already had separate `lang_from`/`lang_to`
  variable names from `self.lang.clone()` — only the source of the
  clone changed
- Java analysis doesn't use the `Analyzer` struct at all (it calls
  `java.extract()` directly), so no Java changes were needed

### `RefBuildConfig::default()` preserved backward compatibility

Using `Default` derive with all `Option<String>` fields meant that
existing callers could pass `&RefBuildConfig::default()` and get
identical behavior to the old `None` / `Option<&str>` parameters.
No behavioral change until the user provides a flag.

## Problems encountered and solutions

### Test module path resolution for `RefBuildConfig`

In `guard.rs`, the test module used `super::RefBuildConfig::default()`
to construct the config for the `WorktreeGuard::new()` call in the
relative-path regression test. This failed because `super` inside
`mod tests` in `guard.rs` resolves to the `guard` module, not the
`worktree` module. `RefBuildConfig` lives in `worktree/mod.rs`.

**Fix:** Changed to `crate::worktree::RefBuildConfig::default()`.

**Lesson:** When adding types to a parent module (`mod.rs`) that are
consumed by child modules (`guard.rs`), use `crate::` paths in tests
rather than `super::` — the latter is ambiguous and depends on nesting
depth.

### `run_td()` is sequential, not concurrent

The plan agent initially assumed both pipelines extract concurrently.
In reality:
- `run_v2()` (default SD pipeline) extracts from/to **concurrently**
  via `tokio::join!` with separate `spawn_blocking` tasks — each task
  gets its own `Arc<L>` clone
- `run_td()` (behavioral BU pipeline) extracts from then to
  **sequentially** using a single `&L` reference

This meant `run_td()` needed `lang_from: &L` and `lang_to: &L` as
separate parameters (not just different `Arc` clones). The fix was
straightforward: add the parameters and use `lang_from` for the from-ref
extraction call and `lang_to` for the to-ref extraction call.

**Lesson:** When threading per-ref config through the orchestrator,
check both `run()` and `run_v2()` — they have different concurrency
models for extraction.

### `TypeScript::new()` must stay backward-compatible

The `TypeScript` struct gained a `ref_config` field, but `new()` still
needs to work for callers that only have a `build_command`. The
constructor populates `ref_config.build_command` from the `build_command`
argument, and `Default` impl sets both fields consistently.

**Lesson:** When adding a config struct alongside an existing parameter,
make sure the old constructor path produces equivalent config to avoid
silent behavioral changes.

### `.envs()` appends, it does not replace

`Command::envs()` **adds** environment variables to the inherited
environment — it does not replace the entire env. This is exactly what
we want: prepend the nvm Node bin directory to `PATH` while keeping
everything else. Passing an empty slice (`&[]`) is a no-op, which is
why the `None` node_version case works without special handling.

If we had needed to **replace** PATH entirely, we'd need
`Command::env("PATH", new_value)` (singular). The plural `.envs()` with
a vec of tuples was the right choice here.

## Codebase patterns worth knowing

### The `WorktreeAccess` trait and worktree sharing

The `WorktreeGuard` implements `WorktreeAccess` (a trait in core with
just `fn path() -> &Path`). The orchestrator wraps guards in
`Arc<dyn WorktreeAccess>` and sends them via `mpsc::channel` from the
TD pipeline to the SD pipeline. This lets SD reuse the worktree
filesystem for import resolution without creating its own worktree.

This means the worktree must stay alive (the `Arc` keeps it alive)
until both TD and SD are done. The `Drop` impl on `WorktreeGuard`
handles cleanup.

### Package manager detection happens after worktree creation

`PackageManager::detect()` reads lockfiles from the worktree directory,
not the original repo. This is correct because the lockfile at the
checked-out ref may differ from the current repo state. It also means
the install command override (`config.install_command`) can bypass
detection entirely — useful when the lockfile is missing or ambiguous.

### Corepack wrapping

When `package.json` has a `packageManager` field matching the detected
manager (e.g., `"yarn@4.5.0"` for a Yarn project), the install command
is automatically wrapped with `corepack` (e.g., `corepack yarn install
--immutable`). This interacts correctly with the nvm PATH override
because corepack ships with Node and the nvm bin directory contains the
correct corepack binary for that Node version.

### The `DiagnoseWithTip` error trait

`WorktreeGuard::new()` returns `Result<Self, WorktreeError>`. Callers
use `.diagnose()?` (from `semver_analyzer_core::error::DiagnoseWithTip`)
to convert `WorktreeError` into `anyhow::Error` with helpful tips
attached. When adding new error paths (like nvm resolution failure),
the existing `.diagnose()` chain handles them automatically as long as
they're `WorktreeError` variants.

## Subprocess calls that need the node env

For future reference, these are all the `Command::new()` calls in the
worktree module that run Node.js tooling and therefore need the
`node_env` applied:

| File | Function | What it runs |
|------|----------|-------------|
| `guard.rs` | `run_package_install()` | npm ci / yarn install / pnpm install |
| `guard.rs` | `run_custom_install()` | user-provided install command |
| `tsc.rs` | `run_tsc_build()` | tsc --build (solution tsconfig) |
| `tsc.rs` | `run_tsc_single()` | tsc --project (2 calls: composite + standard) |
| `tsc.rs` | `run_project_build()` | npm run build / yarn build / custom |

The git commands in `guard.rs` (`validate_git_repo`, `validate_git_ref`,
`create_worktree`, `remove_worktree`) do **not** need the node env —
they are pure git operations unrelated to Node.js.
