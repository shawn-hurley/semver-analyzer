# feat/per-ref-node-version — Next Steps

Follow-up work identified during the initial implementation.

## 1. Remove redundant `build_command` field from `TypeScript`

**Priority:** Do first — small cleanup, removes real duplication.

The `TypeScript` struct now carries both `build_command: Option<String>`
and `ref_config: RefBuildConfig` where `ref_config.build_command` is
always the same value. The standalone field exists only because
`OxcExtractor::extract_at_ref()` in `crates/ts/src/extract/mod.rs`
accepts a bare `Option<&str>` for the build command and constructs a
throwaway `RefBuildConfig` from it.

**What to do:**
- Change `extract_at_ref()` to accept `&RefBuildConfig` directly
- Remove the `build_command` field from `TypeScript`
- Update `TypeScript::new()` to only populate `ref_config`
- Update `Default` impl accordingly
- The `extract()` trait method in `Language` calls `extract_at_ref()`
  internally — update it to pass `&self.ref_config`

**Watch out for:** The `Language::extract()` trait method is defined in
core and must remain language-agnostic. The TypeScript impl of `extract()`
can use `self.ref_config` internally without changing the trait signature.

## 2. Consolidate orchestrator function parameters into a context struct

**Priority:** Second — improves readability across the orchestrator but
is a larger refactor.

`run_td()` now takes 10 parameters. `run_v2()` takes 13. `run()` takes
10. Many of these are the same values passed repeatedly: repo path,
from/to refs, shared findings, progress reporter, LLM config.

**What to do:**
- Define a `PipelineContext` struct bundling the common parameters:
  ```rust
  struct PipelineContext<'a> {
      repo: &'a Path,
      from_ref: &'a str,
      to_ref: &'a str,
      shared: Arc<SharedFindings<L>>,  // or &'a ...
      progress: &'a ProgressReporter,
      no_llm: bool,
      llm_command: Option<&'a str>,
      llm_timeout: u64,
  }
  ```
- Replace parameter lists in `run()`, `run_v2()`, `run_td()`,
  `run_td_analyze()` with `&PipelineContext`
- Per-ref language instances (`lang_from`, `lang_to`) and pipeline-
  specific params (dep repo, llm_all_files) stay as separate parameters
  or get their own sub-structs

**Watch out for:** Lifetime management. The context references repo,
refs, and progress which are borrowed from the caller. The `shared`
field uses `Arc` so it can be cloned into `spawn_blocking` tasks. Don't
try to put everything behind one lifetime — the async/spawn boundaries
make that painful. Consider whether `PipelineContext` should own
`String` copies of repo/refs instead of borrowing, since they get
`.to_string()` cloned for `spawn_blocking` anyway.

## 3. Support fnm, volta, and asdf alongside nvm

**Priority:** Third — only needed when someone actually uses a
non-nvm version manager. The pattern is identical for all of them.

The `nvm.rs` module currently only supports nvm. Other Node.js version
managers use the same concept (resolve a version to a bin directory)
but with different commands:

| Manager | Resolution command |
|---------|-------------------|
| nvm | `bash -c "source $NVM_DIR/nvm.sh && nvm which <version>"` |
| fnm | `fnm exec --using=<version> -- which node` |
| volta | `volta which node --version <version>` (hypothetical) |
| asdf | `asdf where nodejs <version>` + append `/bin` |

**What to do:**
- Auto-detect which version manager is available (check for `NVM_DIR`,
  `fnm` on PATH, `volta` on PATH, `asdf` on PATH)
- Try them in order of specificity, or let the user specify via a
  `--node-version-manager` flag
- Each manager returns a `PathBuf` bin directory — the rest of the
  pipeline (prepend to PATH, pass to `.envs()`) is unchanged

**Watch out for:** fnm and volta are actual binaries (not shell
functions), so they don't need `bash -c "source ..."` wrapping. This
makes them simpler to invoke but means the resolution function needs
to branch on which manager is being used. Consider a small trait or
enum dispatch rather than a chain of if/else.

Also note that volta manages the entire toolchain (Node + package
manager), so `volta which node` may not be sufficient — you might also
need `volta which npm` or `volta which yarn`. Test carefully before
claiming volta support.
