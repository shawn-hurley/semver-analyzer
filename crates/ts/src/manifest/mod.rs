//! Package manifest (`package.json`) diff engine.
//!
//! Analyzes structural changes to `package.json` between two refs.
//! These are deterministic checks that don't require code parsing —
//! just JSON comparison with semantic awareness of entry points,
//! exports maps, module systems, peer dependencies, engines, and bin entries.

use crate::language::{TsManifestChangeType, TypeScript};
use semver_analyzer_core::ManifestChange;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// Compare two `package.json` files and produce manifest changes.
pub fn diff_manifests(old: &Value, new: &Value) -> Vec<ManifestChange<TypeScript>> {
    let mut changes = Vec::new();

    diff_entry_points(old, new, &mut changes);
    diff_module_system(old, new, &mut changes);
    diff_exports(old, new, &mut changes);
    diff_peer_dependencies(old, new, &mut changes);
    diff_engines(old, new, &mut changes);
    diff_bin(old, new, &mut changes);

    changes
}

/// Parse a `package.json` file and diff it against another.
pub fn diff_manifest_files(
    old_path: &Path,
    new_path: &Path,
) -> anyhow::Result<Vec<ManifestChange<TypeScript>>> {
    let old_content = std::fs::read_to_string(old_path)?;
    let new_content = std::fs::read_to_string(new_path)?;
    let old: Value = serde_json::from_str(&old_content)?;
    let new: Value = serde_json::from_str(&new_content)?;
    Ok(diff_manifests(&old, &new))
}

// ─── Entry points ────────────────────────────────────────────────────────

/// Diff `main`, `module`, and `types` entry point fields.
fn diff_entry_points(old: &Value, new: &Value, changes: &mut Vec<ManifestChange<TypeScript>>) {
    for field in &["main", "module", "types", "typings"] {
        let old_val = old.get(field).and_then(|v| v.as_str());
        let new_val = new.get(field).and_then(|v| v.as_str());

        match (old_val, new_val) {
            (Some(o), Some(n)) if o != n => {
                changes.push(ManifestChange {
                    field: field.to_string(),
                    change_type: TsManifestChangeType::EntryPointChanged,
                    before: Some(o.to_string()),
                    after: Some(n.to_string()),
                    description: format!("`{}` entry point changed from `{}` to `{}`", field, o, n),
                    is_breaking: true,
                    source_package: None,
                });
            }
            (Some(o), None) => {
                changes.push(ManifestChange {
                    field: field.to_string(),
                    change_type: TsManifestChangeType::EntryPointChanged,
                    before: Some(o.to_string()),
                    after: None,
                    description: format!("`{}` entry point was removed (was `{}`)", field, o),
                    is_breaking: true,
                    source_package: None,
                });
            }
            // Adding an entry point is not breaking
            _ => {}
        }
    }
}

// ─── Module system ───────────────────────────────────────────────────────

/// Diff `"type"` field (CJS ↔ ESM transition).
fn diff_module_system(old: &Value, new: &Value, changes: &mut Vec<ManifestChange<TypeScript>>) {
    let old_type = old
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("commonjs");
    let new_type = new
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("commonjs");

    if old_type != new_type {
        let description = if new_type == "module" {
            "Package switched from CJS to ESM (`\"type\": \"module\"` added). `require()` consumers will break."
        } else {
            "Package switched from ESM to CJS (`\"type\": \"module\"` removed). `import` consumers will break."
        };
        changes.push(ManifestChange {
            field: "type".to_string(),
            change_type: TsManifestChangeType::ModuleSystemChanged,
            before: Some(old_type.to_string()),
            after: Some(new_type.to_string()),
            description: description.to_string(),
            is_breaking: true,
            source_package: None,
        });
    }
}

// ─── Exports map ─────────────────────────────────────────────────────────

/// Diff the `"exports"` field.
///
/// The exports map can be:
/// - A string: `"exports": "./index.js"`
/// - An object with subpath keys: `"exports": { ".": "./index.js", "./utils": "./utils.js" }`
/// - Nested with conditions: `"exports": { ".": { "import": "./index.mjs", "require": "./index.cjs" } }`
fn diff_exports(old: &Value, new: &Value, changes: &mut Vec<ManifestChange<TypeScript>>) {
    let old_exports = old.get("exports");
    let new_exports = new.get("exports");

    match (old_exports, new_exports) {
        (None, None) => {}    // Neither has exports
        (None, Some(_)) => {} // Adding exports is not breaking
        (Some(old_exp), None) => {
            // Removing exports entirely is breaking
            changes.push(ManifestChange {
                field: "exports".to_string(),
                change_type: TsManifestChangeType::ExportsEntryRemoved,
                before: Some(old_exp.to_string()),
                after: None,
                description: "The `exports` field was removed entirely".to_string(),
                is_breaking: true,
                source_package: None,
            });
        }
        (Some(old_exp), Some(new_exp)) => {
            // Flatten both exports into a map of path → conditions
            let old_flat = flatten_exports(old_exp, ".");
            let new_flat = flatten_exports(new_exp, ".");

            // Find removed entries
            for (path, old_conditions) in &old_flat {
                if let Some(new_conditions) = new_flat.get(path.as_str()) {
                    // Path exists in both — check for removed conditions
                    for (cond, old_target) in old_conditions {
                        if let Some(new_target) = new_conditions.get(cond.as_str()) {
                            // Condition exists in both — check target change
                            if old_target != new_target {
                                changes.push(ManifestChange {
                                    field: format!("exports.{}.{}", path, cond),
                                    change_type: TsManifestChangeType::EntryPointChanged,
                                    before: Some(old_target.clone()),
                                    after: Some(new_target.clone()),
                                    description: format!(
                                        "Export `{}` condition `{}` target changed from `{}` to `{}`",
                                        path, cond, old_target, new_target
                                    ),
                                    is_breaking: true,
                    source_package: None,
                                });
                            }
                        } else {
                            // Condition removed
                            changes.push(ManifestChange {
                                field: format!("exports.{}.{}", path, cond),
                                change_type: TsManifestChangeType::ExportsConditionRemoved,
                                before: Some(old_target.clone()),
                                after: None,
                                description: format!(
                                    "Export condition `{}` was removed from `{}`",
                                    cond, path
                                ),
                                is_breaking: true,
                                source_package: None,
                            });
                        }
                    }
                } else {
                    // Entire path removed
                    changes.push(ManifestChange {
                        field: format!("exports.{}", path),
                        change_type: TsManifestChangeType::ExportsEntryRemoved,
                        before: Some(format!("{:?}", old_conditions)),
                        after: None,
                        description: format!("Export path `{}` was removed", path),
                        is_breaking: true,
                        source_package: None,
                    });
                }
            }

            // Find added entries
            for path in new_flat.keys() {
                if !old_flat.contains_key(path.as_str()) {
                    changes.push(ManifestChange {
                        field: format!("exports.{}", path),
                        change_type: TsManifestChangeType::ExportsEntryAdded,
                        before: None,
                        after: Some(path.clone()),
                        description: format!("Export path `{}` was added", path),
                        is_breaking: false,
                        source_package: None,
                    });
                }
            }
        }
    }
}

/// Flatten a potentially nested exports value into a map of
/// `subpath → { condition → target }`.
///
/// Examples:
/// - `"./index.js"` → `{ "." → { "default" → "./index.js" } }`
/// - `{ ".": "./index.js" }` → `{ "." → { "default" → "./index.js" } }`
/// - `{ ".": { "import": "./index.mjs", "require": "./index.cjs" } }` →
///   `{ "." → { "import" → "./index.mjs", "require" → "./index.cjs" } }`
fn flatten_exports(value: &Value, prefix: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut result = BTreeMap::new();

    match value {
        Value::String(s) => {
            let mut conditions = BTreeMap::new();
            conditions.insert("default".to_string(), s.clone());
            result.insert(prefix.to_string(), conditions);
        }
        Value::Object(map) => {
            // Check if this is a conditions object (keys are conditions like "import", "require")
            // or a subpath object (keys are paths like ".", "./utils")
            let is_conditions = map.keys().all(|k| !k.starts_with('.'));

            if is_conditions && !map.is_empty() {
                let mut conditions = BTreeMap::new();
                for (key, val) in map {
                    match val {
                        Value::String(s) => {
                            conditions.insert(key.clone(), s.clone());
                        }
                        Value::Object(_) => {
                            // Nested conditions — recurse
                            let nested = flatten_exports(val, prefix);
                            for (path, conds) in nested {
                                result.entry(path).or_default().extend(conds);
                            }
                        }
                        _ => {}
                    }
                }
                if !conditions.is_empty() {
                    result
                        .entry(prefix.to_string())
                        .or_default()
                        .extend(conditions);
                }
            } else {
                // Subpath keys
                for (key, val) in map {
                    let nested = flatten_exports(val, key);
                    result.extend(nested);
                }
            }
        }
        _ => {}
    }

    result
}

// ─── Peer dependencies ───────────────────────────────────────────────────

/// Diff `peerDependencies`.
fn diff_peer_dependencies(old: &Value, new: &Value, changes: &mut Vec<ManifestChange<TypeScript>>) {
    let old_peers = extract_string_map(old, "peerDependencies");
    let new_peers = extract_string_map(new, "peerDependencies");

    // Added peer deps
    for (name, version) in &new_peers {
        if !old_peers.contains_key(name.as_str()) {
            changes.push(ManifestChange {
                field: format!("peerDependencies.{}", name),
                change_type: TsManifestChangeType::PeerDependencyAdded,
                before: None,
                after: Some(version.clone()),
                description: format!(
                    "Peer dependency `{}@{}` was added. Consumers must install it.",
                    name, version
                ),
                is_breaking: true,
                source_package: None,
            });
        }
    }

    // Removed peer deps
    for (name, version) in &old_peers {
        if !new_peers.contains_key(name.as_str()) {
            changes.push(ManifestChange {
                field: format!("peerDependencies.{}", name),
                change_type: TsManifestChangeType::PeerDependencyRemoved,
                before: Some(version.clone()),
                after: None,
                description: format!("Peer dependency `{}` was removed", name),
                is_breaking: false,
                source_package: None,
            });
        }
    }

    // Changed peer dep ranges
    for (name, old_range) in &old_peers {
        if let Some(new_range) = new_peers.get(name.as_str()) {
            if old_range != new_range {
                // Determine if the range was narrowed or widened.
                // Simple heuristic: if the new range is a subset string-wise,
                // or if major versions differ, it's likely narrowed.
                // Full semver range comparison would need a semver crate.
                // For now, report all range changes and let the description indicate both values.
                changes.push(ManifestChange {
                    field: format!("peerDependencies.{}", name),
                    change_type: TsManifestChangeType::PeerDependencyRangeChanged,
                    before: Some(old_range.clone()),
                    after: Some(new_range.clone()),
                    description: format!(
                        "Peer dependency `{}` range changed from `{}` to `{}`",
                        name, old_range, new_range
                    ),
                    // Conservative: treat any range change as potentially breaking
                    is_breaking: true,
                    source_package: None,
                });
            }
        }
    }
}

// ─── Engines ─────────────────────────────────────────────────────────────

/// Diff `engines` field (e.g., `node`, `npm`).
fn diff_engines(old: &Value, new: &Value, changes: &mut Vec<ManifestChange<TypeScript>>) {
    let old_engines = extract_string_map(old, "engines");
    let new_engines = extract_string_map(new, "engines");

    // Engine constraint added
    for (engine, constraint) in &new_engines {
        if !old_engines.contains_key(engine.as_str()) {
            changes.push(ManifestChange {
                field: format!("engines.{}", engine),
                change_type: TsManifestChangeType::EngineConstraintChanged,
                before: None,
                after: Some(constraint.clone()),
                description: format!(
                    "Engine constraint `{}: {}` was added. Consumers on unsupported runtimes will be affected.",
                    engine, constraint
                ),
                is_breaking: true,
                    source_package: None,
            });
        }
    }

    // Engine constraint changed
    for (engine, old_constraint) in &old_engines {
        if let Some(new_constraint) = new_engines.get(engine.as_str()) {
            if old_constraint != new_constraint {
                changes.push(ManifestChange {
                    field: format!("engines.{}", engine),
                    change_type: TsManifestChangeType::EngineConstraintChanged,
                    before: Some(old_constraint.clone()),
                    after: Some(new_constraint.clone()),
                    description: format!(
                        "Engine constraint for `{}` changed from `{}` to `{}`",
                        engine, old_constraint, new_constraint
                    ),
                    // Conservative: any engine constraint change could be breaking
                    is_breaking: true,
                    source_package: None,
                });
            }
        }
        // Engine constraint removed — loosening, not breaking
    }
}

// ─── Bin entries ─────────────────────────────────────────────────────────

/// Diff `bin` field (CLI entry points).
fn diff_bin(old: &Value, new: &Value, changes: &mut Vec<ManifestChange<TypeScript>>) {
    let old_bins = extract_bin_map(old);
    let new_bins = extract_bin_map(new);

    // Removed bin entries
    for (name, path) in &old_bins {
        if !new_bins.contains_key(name.as_str()) {
            changes.push(ManifestChange {
                field: format!("bin.{}", name),
                change_type: TsManifestChangeType::BinEntryRemoved,
                before: Some(path.clone()),
                after: None,
                description: format!("CLI command `{}` was removed", name),
                is_breaking: true,
                source_package: None,
            });
        }
    }
    // Added bin entries are not breaking
}

// ─── Utility functions ───────────────────────────────────────────────────

/// Extract a string map from a JSON object field.
fn extract_string_map(value: &Value, field: &str) -> BTreeMap<String, String> {
    value
        .get(field)
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Extract bin entries as a map. Handles both string and object forms:
/// - `"bin": "./cli.js"` → uses package name as key
/// - `"bin": { "myapp": "./cli.js" }` → as-is
fn extract_bin_map(value: &Value) -> BTreeMap<String, String> {
    match value.get("bin") {
        Some(Value::String(s)) => {
            let name = value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            let mut map = BTreeMap::new();
            map.insert(name.to_string(), s.clone());
            map
        }
        Some(Value::Object(obj)) => obj
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        _ => BTreeMap::new(),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn find_manifest_change(
        changes: &[ManifestChange<TypeScript>],
        ct: TsManifestChangeType,
    ) -> &ManifestChange<TypeScript> {
        changes
            .iter()
            .find(|c| c.change_type == ct)
            .unwrap_or_else(|| {
                panic!(
                    "ManifestChange {:?} not found in: {:?}",
                    ct,
                    changes.iter().map(|c| &c.change_type).collect::<Vec<_>>()
                )
            })
    }

    fn has_manifest_change(
        changes: &[ManifestChange<TypeScript>],
        ct: TsManifestChangeType,
    ) -> bool {
        changes.iter().any(|c| c.change_type == ct)
    }

    // ── Entry points ─────────────────────────────────────────────────

    #[test]
    fn detect_main_entry_changed() {
        let old = json!({ "main": "./dist/index.js" });
        let new = json!({ "main": "./lib/index.js" });
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::EntryPointChanged);
        assert!(c.is_breaking);
        assert_eq!(c.field, "main");
    }

    #[test]
    fn detect_types_entry_removed() {
        let old = json!({ "types": "./dist/index.d.ts" });
        let new = json!({});
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::EntryPointChanged);
        assert!(c.is_breaking);
        assert_eq!(c.field, "types");
    }

    #[test]
    fn adding_entry_point_not_breaking() {
        let old = json!({});
        let new = json!({ "main": "./dist/index.js" });
        let changes = diff_manifests(&old, &new);
        assert!(changes.is_empty());
    }

    #[test]
    fn identical_entry_points_no_changes() {
        let pkg = json!({ "main": "./index.js", "types": "./index.d.ts" });
        let changes = diff_manifests(&pkg, &pkg);
        assert!(changes.is_empty());
    }

    // ── Module system ────────────────────────────────────────────────

    #[test]
    fn detect_cjs_to_esm() {
        let old = json!({});
        let new = json!({ "type": "module" });
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::ModuleSystemChanged);
        assert!(c.is_breaking);
        assert!(c.description.contains("CJS to ESM"));
    }

    #[test]
    fn detect_esm_to_cjs() {
        let old = json!({ "type": "module" });
        let new = json!({});
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::ModuleSystemChanged);
        assert!(c.is_breaking);
        assert!(c.description.contains("ESM to CJS"));
    }

    #[test]
    fn same_module_system_no_change() {
        let old = json!({ "type": "module" });
        let new = json!({ "type": "module" });
        let changes = diff_manifests(&old, &new);
        assert!(!has_manifest_change(
            &changes,
            TsManifestChangeType::ModuleSystemChanged
        ));
    }

    // ── Exports map ──────────────────────────────────────────────────

    #[test]
    fn detect_exports_entry_removed() {
        let old = json!({
            "exports": {
                ".": "./index.js",
                "./utils": "./utils.js"
            }
        });
        let new = json!({
            "exports": {
                ".": "./index.js"
            }
        });
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::ExportsEntryRemoved);
        assert!(c.is_breaking);
        assert!(c.field.contains("./utils"));
    }

    #[test]
    fn detect_exports_entry_added() {
        let old = json!({
            "exports": { ".": "./index.js" }
        });
        let new = json!({
            "exports": { ".": "./index.js", "./utils": "./utils.js" }
        });
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::ExportsEntryAdded);
        assert!(!c.is_breaking);
    }

    #[test]
    fn detect_exports_condition_removed() {
        let old = json!({
            "exports": {
                ".": {
                    "import": "./index.mjs",
                    "require": "./index.cjs"
                }
            }
        });
        let new = json!({
            "exports": {
                ".": {
                    "import": "./index.mjs"
                }
            }
        });
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::ExportsConditionRemoved);
        assert!(c.is_breaking);
        assert!(c.field.contains("require"));
    }

    #[test]
    fn detect_exports_entirely_removed() {
        let old = json!({ "exports": { ".": "./index.js" } });
        let new = json!({});
        let changes = diff_manifests(&old, &new);

        assert!(has_manifest_change(
            &changes,
            TsManifestChangeType::ExportsEntryRemoved
        ));
    }

    #[test]
    fn exports_string_form() {
        let old = json!({ "exports": "./index.js" });
        let new = json!({ "exports": "./dist/index.js" });
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::EntryPointChanged);
        assert!(c.is_breaking);
    }

    // ── Peer dependencies ────────────────────────────────────────────

    #[test]
    fn detect_peer_dependency_added() {
        let old = json!({});
        let new = json!({ "peerDependencies": { "react": "^18.0.0" } });
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::PeerDependencyAdded);
        assert!(c.is_breaking);
        assert!(c.description.contains("react"));
    }

    #[test]
    fn detect_peer_dependency_removed() {
        let old = json!({ "peerDependencies": { "react": "^18.0.0" } });
        let new = json!({});
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::PeerDependencyRemoved);
        assert!(!c.is_breaking);
    }

    #[test]
    fn detect_peer_dependency_range_changed() {
        let old = json!({ "peerDependencies": { "react": "^17.0.0 || ^18.0.0" } });
        let new = json!({ "peerDependencies": { "react": "^18.0.0" } });
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::PeerDependencyRangeChanged);
        assert!(c.is_breaking);
    }

    // ── Engines ──────────────────────────────────────────────────────

    #[test]
    fn detect_engine_constraint_added() {
        let old = json!({});
        let new = json!({ "engines": { "node": ">=18" } });
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::EngineConstraintChanged);
        assert!(c.is_breaking);
    }

    #[test]
    fn detect_engine_constraint_changed() {
        let old = json!({ "engines": { "node": ">=16" } });
        let new = json!({ "engines": { "node": ">=18" } });
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::EngineConstraintChanged);
        assert!(c.is_breaking);
        assert_eq!(c.before.as_deref(), Some(">=16"));
        assert_eq!(c.after.as_deref(), Some(">=18"));
    }

    // ── Bin entries ──────────────────────────────────────────────────

    #[test]
    fn detect_bin_removed() {
        let old = json!({
            "name": "myapp",
            "bin": { "myapp": "./cli.js", "myapp-dev": "./dev.js" }
        });
        let new = json!({
            "name": "myapp",
            "bin": { "myapp": "./cli.js" }
        });
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::BinEntryRemoved);
        assert!(c.is_breaking);
        assert!(c.field.contains("myapp-dev"));
    }

    #[test]
    fn detect_bin_string_form_removed() {
        let old = json!({ "name": "myapp", "bin": "./cli.js" });
        let new = json!({ "name": "myapp" });
        let changes = diff_manifests(&old, &new);

        let c = find_manifest_change(&changes, TsManifestChangeType::BinEntryRemoved);
        assert!(c.is_breaking);
    }

    // ── No changes ───────────────────────────────────────────────────

    #[test]
    fn identical_manifests_no_changes() {
        let pkg = json!({
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
        });
        let changes = diff_manifests(&pkg, &pkg);
        assert!(changes.is_empty());
    }

    // ── Flatten exports tests ────────────────────────────────────────

    #[test]
    fn flatten_string_exports() {
        let val = json!("./index.js");
        let flat = flatten_exports(&val, ".");
        assert_eq!(flat.get(".").unwrap().get("default").unwrap(), "./index.js");
    }

    #[test]
    fn flatten_subpath_exports() {
        let val = json!({
            ".": "./index.js",
            "./utils": "./utils.js"
        });
        let flat = flatten_exports(&val, ".");
        assert_eq!(flat.len(), 2);
        assert!(flat.contains_key("."));
        assert!(flat.contains_key("./utils"));
    }

    #[test]
    fn flatten_conditional_exports() {
        let val = json!({
            ".": {
                "import": "./index.mjs",
                "require": "./index.cjs"
            }
        });
        let flat = flatten_exports(&val, ".");
        let root = flat.get(".").unwrap();
        assert_eq!(root.get("import").unwrap(), "./index.mjs");
        assert_eq!(root.get("require").unwrap(), "./index.cjs");
    }
}
