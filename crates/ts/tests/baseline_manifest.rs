//! Baseline integration tests for manifest (package.json) diffing.
//!
//! These tests capture the output of `diff_manifests` as insta snapshots.

mod helpers;

use helpers::*;
use semver_analyzer_ts::manifest::diff_manifests;
use serde_json::json;

fn manifest_diff(old: serde_json::Value, new: serde_json::Value) -> Vec<NormalizedManifestChange> {
    normalize_manifest(&diff_manifests(&old, &new))
}

// ── Entry points ─────────────────────────────────────────────────

#[test]
fn baseline_main_entry_changed() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({ "main": "./dist/index.js" }),
        json!({ "main": "./lib/index.js" }),
    ));
}

#[test]
fn baseline_types_entry_removed() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({ "types": "./dist/index.d.ts" }),
        json!({}),
    ));
}

#[test]
fn baseline_adding_entry_point_not_breaking() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({}),
        json!({ "main": "./dist/index.js" }),
    ));
}

#[test]
fn baseline_identical_entry_points() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({ "main": "./index.js", "types": "./index.d.ts" }),
        json!({ "main": "./index.js", "types": "./index.d.ts" }),
    ));
}

// ── Module system ────────────────────────────────────────────────

#[test]
fn baseline_cjs_to_esm() {
    insta::assert_yaml_snapshot!(manifest_diff(json!({}), json!({ "type": "module" }),));
}

#[test]
fn baseline_esm_to_cjs() {
    insta::assert_yaml_snapshot!(manifest_diff(json!({ "type": "module" }), json!({}),));
}

#[test]
fn baseline_same_module_system() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({ "type": "module" }),
        json!({ "type": "module" }),
    ));
}

// ── Exports map ──────────────────────────────────────────────────

#[test]
fn baseline_exports_entry_removed() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({
            "exports": {
                ".": "./index.js",
                "./utils": "./utils.js"
            }
        }),
        json!({
            "exports": {
                ".": "./index.js"
            }
        }),
    ));
}

#[test]
fn baseline_exports_entry_added() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({ "exports": { ".": "./index.js" } }),
        json!({ "exports": { ".": "./index.js", "./utils": "./utils.js" } }),
    ));
}

#[test]
fn baseline_exports_condition_removed() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({
            "exports": {
                ".": {
                    "import": "./index.mjs",
                    "require": "./index.cjs"
                }
            }
        }),
        json!({
            "exports": {
                ".": {
                    "import": "./index.mjs"
                }
            }
        }),
    ));
}

#[test]
fn baseline_exports_entirely_removed() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({ "exports": { ".": "./index.js" } }),
        json!({}),
    ));
}

#[test]
fn baseline_exports_string_form() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({ "exports": "./index.js" }),
        json!({ "exports": "./dist/index.js" }),
    ));
}

// ── Peer dependencies ────────────────────────────────────────────

#[test]
fn baseline_peer_dependency_added() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({}),
        json!({ "peerDependencies": { "react": "^18.0.0" } }),
    ));
}

#[test]
fn baseline_peer_dependency_removed() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({ "peerDependencies": { "react": "^18.0.0" } }),
        json!({}),
    ));
}

#[test]
fn baseline_peer_dependency_range_changed() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({ "peerDependencies": { "react": "^17.0.0 || ^18.0.0" } }),
        json!({ "peerDependencies": { "react": "^18.0.0" } }),
    ));
}

// ── Engines ──────────────────────────────────────────────────────

#[test]
fn baseline_engine_constraint_added() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({}),
        json!({ "engines": { "node": ">=18" } }),
    ));
}

#[test]
fn baseline_engine_constraint_changed() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({ "engines": { "node": ">=16" } }),
        json!({ "engines": { "node": ">=18" } }),
    ));
}

// ── Bin entries ──────────────────────────────────────────────────

#[test]
fn baseline_bin_removed() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({
            "name": "myapp",
            "bin": { "myapp": "./cli.js", "myapp-dev": "./dev.js" }
        }),
        json!({
            "name": "myapp",
            "bin": { "myapp": "./cli.js" }
        }),
    ));
}

#[test]
fn baseline_bin_string_form_removed() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({ "name": "myapp", "bin": "./cli.js" }),
        json!({ "name": "myapp" }),
    ));
}

// ── No changes ───────────────────────────────────────────────────

#[test]
fn baseline_identical_manifests() {
    insta::assert_yaml_snapshot!(manifest_diff(
        json!({
            "name": "my-lib",
            "version": "1.0.0",
            "main": "./dist/index.js",
            "types": "./dist/index.d.ts",
            "exports": {
                ".": {
                    "import": "./dist/index.mjs",
                    "require": "./dist/index.cjs"
                }
            },
            "peerDependencies": { "react": "^18.0.0" },
            "engines": { "node": ">=16" },
            "bin": { "myapp": "./cli.js" }
        }),
        json!({
            "name": "my-lib",
            "version": "1.0.0",
            "main": "./dist/index.js",
            "types": "./dist/index.d.ts",
            "exports": {
                ".": {
                    "import": "./dist/index.mjs",
                    "require": "./dist/index.cjs"
                }
            },
            "peerDependencies": { "react": "^18.0.0" },
            "engines": { "node": ">=16" },
            "bin": { "myapp": "./cli.js" }
        }),
    ));
}
