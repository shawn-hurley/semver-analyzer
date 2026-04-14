//! TypeScript/JavaScript-specific Konveyor rule helpers.
//!
//! Re-exports JS/TS-specific functions and types from `konveyor-core`.
//! These contain npm package resolution, JSX/React condition building,
//! and CSS-specific logic that doesn't belong in the language-agnostic
//! shared layer.
//!
//! Consumers should import from this module rather than directly from
//! `konveyor-core` to maintain the correct dependency direction.

// ── Public functions ──────────────────────────────────────────────────
pub use semver_analyzer_konveyor_core::{
    api_change_to_strategy, build_frontend_condition, extract_package_from_path,
    read_package_json_at_ref, read_package_json_from_file, resolve_npm_package,
    suppress_redundant_prop_rules, suppress_redundant_prop_value_rules,
};

// ── Config types ──────────────────────────────────────────────────────
pub use semver_analyzer_konveyor_core::{
    ComponentWarningEntry, CompositionRuleEntry, CssVarRenameEntry, MissingImportEntry,
    PackageInfo, PropRenameEntry, ValueReviewEntry,
};
