# Concerns Log: feat/per-ref-node-version

Concerns, decisions, and resolutions tracked during development.

---

## 2026-04-21 — Redundant build_command field on TypeScript struct

**Status:** resolved

The `TypeScript` struct now carries both a standalone `build_command: Option<String>` field and `ref_config: RefBuildConfig` which contains its own `build_command: Option<String>`. Both fields must always hold the same value, but two different code paths read from different fields:

- `Language::extract()` (via `OxcExtractor::extract_at_ref()` in `crates/ts/src/extract/mod.rs`) reads `self.build_command`
- `Language::extract_keeping_worktree()` (in `crates/ts/src/language.rs`) reads `self.ref_config`

The duplication exists because `extract_at_ref()` accepts a bare `Option<&str>` for the build command and constructs a throwaway `RefBuildConfig` internally, rather than accepting a `&RefBuildConfig` directly.

**Impact:** If someone modifies one constructor or field without updating the other, the `extract` CLI command would silently use a different build command than the `analyze` CLI command. This is a latent bug that won't surface until the two values diverge.

**Resolution:** Changed `OxcExtractor::extract_at_ref()` to accept `&RefBuildConfig` directly, removed the standalone `build_command` field from `TypeScript`. The struct now has a single `ref_config` field as the sole source of truth. Both `extract()` and `extract_keeping_worktree()` read from the same field.
