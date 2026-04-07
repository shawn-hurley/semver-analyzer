//! Integration test: full pipeline for design-token renames.
//!
//! Loads the complete set of 4028 entries extracted from the real
//! PatternFly v5.4.0 → v6.4.1 migration report and verifies that:
//!
//! 1. `api_change_to_strategy` produces the correct strategy for every entry:
//!    - `Rename` for design-token renames (before/after are symbol summaries)
//!    - `ImportPathChange` for import-path relocations (e.g., chart components
//!      moving from `@patternfly/react-charts` to `@patternfly/react-charts/victory`)
//! 2. Rules built from those strategies consolidate correctly.
//! 3. The consolidated fix-strategy mappings all contain clean from/to names.
//! 4. A lookup by old token name always succeeds in the mappings.

use std::collections::HashMap;

use semver_analyzer_core::{ApiChange, ApiChangeKind, ApiChangeType};
use semver_analyzer_konveyor_core::{
    api_change_to_strategy, consolidate_rules, extract_fix_strategies, extract_name_from_summary,
    KonveyorCondition, KonveyorRule, RenamePatterns,
};

/// Returns `true` if the entry represents an import-path relocation rather
/// than a design-token rename.  Import-path entries have npm-style paths in
/// `before`/`after` (e.g., `@patternfly/react-charts`) instead of
/// `symbol_summary` strings (which always contain `": "`).
fn is_import_path_relocation(entry: &TokenRenameEntry) -> bool {
    fn looks_like_import_path(s: &str) -> bool {
        if s.contains(": ") || s.is_empty() {
            return false;
        }
        if s.starts_with('@') || s.contains('/') {
            return true;
        }
        s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
    }

    !entry.before.is_empty()
        && !entry.after.is_empty()
        && entry.before != entry.after
        && !entry.before.contains("packages/")
        && !entry.after.contains("packages/")
        && looks_like_import_path(&entry.before)
        && looks_like_import_path(&entry.after)
}

/// A single entry from the fixture file.
#[derive(serde::Deserialize)]
struct TokenRenameEntry {
    file: String,
    symbol: String,
    before: String,
    after: String,
}

fn load_fixture() -> Vec<TokenRenameEntry> {
    let data = include_str!("fixtures/token_renames.json");
    serde_json::from_str(data).expect("failed to parse token_renames.json fixture")
}

// ── 1. api_change_to_strategy produces correct strategy for every entry ──

#[test]
fn test_every_token_rename_produces_rename_strategy() {
    let entries = load_fixture();
    assert!(
        entries.len() > 4000,
        "Expected 4000+ entries in fixture, got {}",
        entries.len()
    );

    let patterns = RenamePatterns::empty();
    let member_renames = HashMap::new();

    let mut failures = Vec::new();
    let mut rename_count = 0;
    let mut import_path_count = 0;

    for entry in &entries {
        let change = ApiChange {
            symbol: entry.symbol.clone(),
            qualified_name: String::new(),
            kind: ApiChangeKind::Constant,
            change: ApiChangeType::Renamed,
            before: Some(entry.before.clone()),
            after: Some(entry.after.clone()),
            description: format!("Exported constant `{}` was renamed", entry.symbol),
            migration_target: None,
            removal_disposition: None,
            renders_element: None,
        };

        let strat = api_change_to_strategy(&change, &patterns, &member_renames, &entry.file);

        match strat {
            None => {
                failures.push(format!("{}: no strategy produced", entry.symbol));
            }
            Some(s) => {
                if is_import_path_relocation(entry) {
                    // Import-path relocations (e.g., chart components) must
                    // produce ImportPathChange, NOT Rename.  A Rename strategy
                    // here would tell the consumer to find-and-replace the
                    // symbol name (e.g., "Chart") with the import path string
                    // (e.g., "@patternfly/react-charts/victory"), corrupting
                    // every file that references the symbol.
                    if s.strategy != "ImportPathChange" {
                        failures.push(format!(
                            "{}: expected ImportPathChange for import-path relocation, got {}",
                            entry.symbol, s.strategy
                        ));
                        continue;
                    }
                    let from = s.from.as_deref().unwrap_or("");
                    let to = s.to.as_deref().unwrap_or("");
                    if from != entry.before {
                        failures.push(format!(
                            "{}: ImportPathChange from mismatch: expected '{}', got '{}'",
                            entry.symbol, entry.before, from
                        ));
                    }
                    if to != entry.after {
                        failures.push(format!(
                            "{}: ImportPathChange to mismatch: expected '{}', got '{}'",
                            entry.symbol, entry.after, to
                        ));
                    }
                    import_path_count += 1;
                } else {
                    // Design-token renames must produce Rename.
                    if s.strategy != "Rename" {
                        failures.push(format!(
                            "{}: expected Rename, got {}",
                            entry.symbol, s.strategy
                        ));
                        continue;
                    }

                    // from must be the clean symbol name
                    let from = s.from.as_deref().unwrap_or("");
                    if from != entry.symbol {
                        failures.push(format!(
                            "{}: from mismatch: expected '{}', got '{}'",
                            entry.symbol, entry.symbol, from
                        ));
                    }

                    // to must be a clean name, not a symbol_summary string
                    let to = s.to.as_deref().unwrap_or("");
                    if to.contains("variable: ") || to.contains("constant: ") {
                        failures.push(format!(
                            "{}: 'to' is a raw symbol_summary: {}",
                            entry.symbol, to
                        ));
                    }

                    // to must match what extract_name_from_summary returns
                    let expected_new = extract_name_from_summary(&entry.after);
                    if to != expected_new {
                        failures.push(format!(
                            "{}: to mismatch: expected '{}', got '{}'",
                            entry.symbol, expected_new, to
                        ));
                    }
                    rename_count += 1;
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} / {} entries failed:\n{}",
        failures.len(),
        entries.len(),
        failures[..failures.len().min(20)].join("\n")
    );

    // Sanity: expect ~39 import-path relocations and ~3989 token renames
    assert!(
        import_path_count >= 30,
        "Expected at least 30 ImportPathChange entries, got {}",
        import_path_count
    );
    assert!(
        rename_count >= 3900,
        "Expected at least 3900 Rename entries, got {}",
        rename_count
    );
}

// ── 2. Consolidation + fix-strategy mappings are all clean ───────────────

#[test]
fn test_consolidated_token_strategies_have_clean_mappings() {
    let entries = load_fixture();
    let patterns = RenamePatterns::empty();
    let member_renames = HashMap::new();

    let relocation_count = entries
        .iter()
        .filter(|e| is_import_path_relocation(e))
        .count();
    let token_rename_count = entries.len() - relocation_count;

    // Build one KonveyorRule per entry (mimicking generate_rules).
    let rules: Vec<KonveyorRule> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let change = ApiChange {
                symbol: entry.symbol.clone(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Constant,
                change: ApiChangeType::Renamed,
                before: Some(entry.before.clone()),
                after: Some(entry.after.clone()),
                description: format!("Exported constant `{}` was renamed", entry.symbol),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            };

            let fix_strategy =
                api_change_to_strategy(&change, &patterns, &member_renames, &entry.file);

            KonveyorRule {
                rule_id: format!("semver-token-rename-{}", i),
                labels: vec![
                    "change-type=renamed".to_string(),
                    "kind=constant".to_string(),
                    "has-codemod=true".to_string(),
                    format!("package=@patternfly/react-tokens"),
                ],
                effort: 1,
                category: "mandatory".to_string(),
                description: format!("Exported constant `{}` was renamed", entry.symbol),
                message: format!("File: {}\ntoken renamed", entry.file),
                links: vec![],
                when: KonveyorCondition::FileContent {
                    filecontent: semver_analyzer_konveyor_core::FileContentFields {
                        pattern: format!("\\b{}\\b", entry.symbol),
                        file_pattern: "*.{ts,tsx}".to_string(),
                    },
                },
                fix_strategy,
            }
        })
        .collect();

    let pre_count = rules.len();
    assert!(pre_count > 4000);

    // Consolidate
    let (consolidated, _id_map) = consolidate_rules(rules);

    // Should consolidate into far fewer rules
    let token_rules: Vec<&KonveyorRule> = consolidated
        .iter()
        .filter(|r| r.labels.contains(&"kind=constant".to_string()))
        .filter(|r| r.labels.contains(&"change-type=renamed".to_string()))
        .collect();

    let total_consolidated = token_rules.len();
    // has-codemod=true rules are kept as singletons (not consolidated)
    // to preserve their per-token Rename mappings. So the count should
    // remain the same or very close to the original.
    assert!(
        total_consolidated > 0,
        "Expected token rules to survive consolidation, got 0",
    );

    // Extract fix strategies
    let strategies = extract_fix_strategies(&consolidated);

    // Collect all Rename mappings across all consolidated token rules.
    // Import-path relocation rules (ImportPathChange) are counted separately.
    // Some PascalCase constants (e.g., Chart, ChartArea) stay as individual
    // rules because `consolidation_key` treats them differently. Count
    // mappings from both the big groups and the individual rules.
    let mut rename_mappings = Vec::new();
    let mut import_path_rules = 0;
    for rule in &token_rules {
        if let Some(strat) = strategies.get(&rule.rule_id) {
            if strat.strategy == "ImportPathChange" {
                // Import-path relocations have from/to as import specifiers,
                // not symbol name mappings.
                import_path_rules += 1;
                continue;
            }
            assert_eq!(
                strat.strategy, "Rename",
                "Rule {} should have Rename or ImportPathChange strategy, got {}",
                rule.rule_id, strat.strategy
            );
            if strat.mappings.is_empty() {
                // Individual (non-consolidated) rule: from/to on the entry itself
                if let (Some(from), Some(to)) = (&strat.from, &strat.to) {
                    rename_mappings.push((from.clone(), to.clone()));
                }
            } else {
                for m in &strat.mappings {
                    if let (Some(from), Some(to)) = (&m.from, &m.to) {
                        rename_mappings.push((from.clone(), to.clone()));
                    }
                }
            }
        }
    }

    // Every token rename entry should have a Rename mapping
    assert!(
        rename_mappings.len() >= token_rename_count,
        "Expected at least {} Rename mappings, got {}",
        token_rename_count,
        rename_mappings.len()
    );

    // Import-path relocations should have ImportPathChange rules
    assert!(
        import_path_rules >= relocation_count,
        "Expected at least {} ImportPathChange rules, got {}",
        relocation_count,
        import_path_rules
    );

    // Every Rename mapping must have clean names (no symbol_summary strings)
    let mut dirty = Vec::new();
    for (from, to) in &rename_mappings {
        if from.contains("variable: ") || from.contains("constant: ") {
            dirty.push(format!("from: {}", from));
        }
        if to.contains("variable: ") || to.contains("constant: ") {
            dirty.push(format!("to: {}", to));
        }
    }

    assert!(
        dirty.is_empty(),
        "{} mappings have symbol_summary pollution:\n{}",
        dirty.len(),
        dirty[..dirty.len().min(20)].join("\n")
    );
}

// ── 3. Every original symbol is findable in the consolidated mappings ────

#[test]
fn test_every_token_findable_in_consolidated_mappings() {
    let entries = load_fixture();
    let patterns = RenamePatterns::empty();
    let member_renames = HashMap::new();

    // Collect import-path relocation symbols so we can check them separately.
    let relocation_symbols: std::collections::HashSet<&str> = entries
        .iter()
        .filter(|e| is_import_path_relocation(e))
        .map(|e| e.symbol.as_str())
        .collect();

    let rules: Vec<KonveyorRule> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let change = ApiChange {
                symbol: entry.symbol.clone(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Constant,
                change: ApiChangeType::Renamed,
                before: Some(entry.before.clone()),
                after: Some(entry.after.clone()),
                description: format!("Exported constant `{}` was renamed", entry.symbol),
                migration_target: None,
                removal_disposition: None,
                renders_element: None,
            };

            let fix_strategy =
                api_change_to_strategy(&change, &patterns, &member_renames, &entry.file);

            KonveyorRule {
                rule_id: format!("semver-token-rename-{}", i),
                labels: vec![
                    "change-type=renamed".to_string(),
                    "kind=constant".to_string(),
                    "has-codemod=true".to_string(),
                    format!("package=@patternfly/react-tokens"),
                ],
                effort: 1,
                category: "mandatory".to_string(),
                description: format!("Exported constant `{}` was renamed", entry.symbol),
                message: format!("File: {}\ntoken renamed", entry.file),
                links: vec![],
                when: KonveyorCondition::FileContent {
                    filecontent: semver_analyzer_konveyor_core::FileContentFields {
                        pattern: format!("\\b{}\\b", entry.symbol),
                        file_pattern: "*.{ts,tsx}".to_string(),
                    },
                },
                fix_strategy,
            }
        })
        .collect();

    let (consolidated, _) = consolidate_rules(rules);
    let strategies = extract_fix_strategies(&consolidated);

    // Build a lookup: old_name → new_name from all consolidated Rename mappings.
    // Import-path relocations (ImportPathChange) use from/to for the import
    // specifier, not the symbol name, so they are tracked separately.
    let mut rename_map: HashMap<String, String> = HashMap::new();
    let mut import_path_symbols: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for rule in &consolidated {
        if let Some(strat) = strategies.get(&rule.rule_id) {
            if strat.strategy == "ImportPathChange" {
                // For ImportPathChange rules, extract the symbol from the
                // rule description (the rule pattern matches the symbol).
                // The symbol is findable via the rule's when condition.
                if let KonveyorCondition::FileContent { ref filecontent } = rule.when {
                    // Pattern is "\bSymbolName\b", extract the symbol name.
                    let sym = filecontent
                        .pattern
                        .trim_start_matches("\\b")
                        .trim_end_matches("\\b");
                    import_path_symbols.insert(sym.to_string());
                }
                continue;
            }
            if strat.mappings.is_empty() {
                if let (Some(from), Some(to)) = (&strat.from, &strat.to) {
                    rename_map.insert(from.clone(), to.clone());
                }
            } else {
                for m in &strat.mappings {
                    if let (Some(from), Some(to)) = (&m.from, &m.to) {
                        rename_map.insert(from.clone(), to.clone());
                    }
                }
            }
        }
    }

    // Every unique token rename symbol must be findable in the rename map.
    // Import-path relocation symbols are checked separately.
    let unique_symbols: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.symbol.as_str()).collect();

    let mut missing = Vec::new();
    let mut dirty_targets = Vec::new();

    for sym in &unique_symbols {
        if relocation_symbols.contains(sym) {
            // Import-path relocations: verify the symbol has an
            // ImportPathChange rule, not a Rename mapping.
            if !import_path_symbols.contains(*sym) {
                missing.push(format!("{} (expected ImportPathChange rule)", sym));
            }
        } else {
            match rename_map.get(*sym) {
                None => missing.push(sym.to_string()),
                Some(target) => {
                    // Target must be a clean name (no symbol_summary strings)
                    if target.contains("variable: ") || target.contains("constant: ") {
                        dirty_targets.push(format!("{} → {}", sym, target));
                    }
                }
            }
        }
    }

    assert!(
        missing.is_empty(),
        "{} / {} unique tokens not findable in consolidated mappings:\n{}",
        missing.len(),
        unique_symbols.len(),
        missing[..missing.len().min(20)].join("\n")
    );

    assert!(
        dirty_targets.is_empty(),
        "{} tokens have symbol_summary targets:\n{}",
        dirty_targets.len(),
        dirty_targets[..dirty_targets.len().min(20)].join("\n")
    );

    // Sanity: at least 2500 unique symbols exist in the rename map
    let found = unique_symbols
        .iter()
        .filter(|s| rename_map.contains_key(**s) || import_path_symbols.contains(**s))
        .count();
    assert!(
        found >= 2500,
        "Expected at least 2500 unique tokens in consolidated output, got {}",
        found
    );
}
