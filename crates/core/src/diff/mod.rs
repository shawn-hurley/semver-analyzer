//! Structural diff engine for comparing two API surfaces.
//!
//! This module is language-agnostic. It operates on `ApiSurface` instances
//! produced by language-specific `ApiExtractor` implementations and produces
//! `StructuralChange` entries.
//!
//! Type comparison is done via canonicalized string equality — the
//! `ApiExtractor` is responsible for normalizing types before they reach
//! this function.
//!
//! ## Module structure
//!
//! - [`compare`] — Individual comparison functions (visibility, modifiers,
//!   hierarchy, signatures, members).
//! - [`rename`] — Rename detection via fingerprint matching + name similarity.
//! - [`relocate`] — Path-based relocation detection (moved to deprecated, etc.).
//! - [`helpers`] — Shared utility functions (change builders, summaries).

mod compare;
mod helpers;
mod migration;
mod relocate;
mod rename;

#[cfg(test)]
mod tests;

use crate::traits::LanguageSemantics;
use crate::types::{ApiSurface, StructuralChange, StructuralChangeType, Symbol, SymbolKind};
use std::collections::{HashMap, HashSet};

use compare::diff_symbol;
use helpers::{is_star_reexport, kind_label, symbol_summary};
use migration::detect_migrations;
use relocate::{detect_relocations, RelocationType};
use rename::detect_renames;

/// Compare two API surfaces using language-specific semantic rules.
///
/// This is the core of the TD (Top-Down) pipeline. It matches symbols by
/// `qualified_name`, then compares every field to detect additions, removals,
/// and modifications.
///
/// The `semantics` parameter provides language-specific rules for:
/// - Whether adding a member is breaking (`is_member_addition_breaking`)
/// - How to group related symbols (`same_family`, `same_identity`)
/// - How to rank visibility levels (`visibility_rank`)
/// - How to parse union/literal types (`parse_union_values`)
/// - Post-processing of the change list (`post_process`)
///
/// The matching pipeline is:
/// 1. **Exact qualified_name match** — symbols at the same path are compared directly.
/// 2. **Relocation detection** — symbols with the same canonical path (stripping
///    `/deprecated/` and `/next/`) are matched as path moves (e.g., moved to deprecated).
/// 3. **Rename detection** — remaining removed+added pairs are matched by type
///    fingerprint and name similarity.
/// 4. **Unmatched** — remaining removed symbols are reported as removed, added as added.
///
/// Star re-export symbols (`export * from './module'`) are filtered out.
pub fn diff_surfaces_with_semantics(
    old: &ApiSurface,
    new: &ApiSurface,
    semantics: &dyn LanguageSemantics,
) -> Vec<StructuralChange> {
    let mut changes = Vec::new();

    // Filter out star re-export symbols — they represent `export * from '...'`
    // directives in barrel files.
    let old_symbols: Vec<&Symbol> = old
        .symbols
        .iter()
        .filter(|s| !is_star_reexport(s))
        .collect();
    let new_symbols: Vec<&Symbol> = new
        .symbols
        .iter()
        .filter(|s| !is_star_reexport(s))
        .collect();

    // Build lookup maps by qualified_name
    let old_map: HashMap<&str, &Symbol> = old_symbols
        .iter()
        .map(|s| (s.qualified_name.as_str(), *s))
        .collect();
    let new_map: HashMap<&str, &Symbol> = new_symbols
        .iter()
        .map(|s| (s.qualified_name.as_str(), *s))
        .collect();

    // Collect removed and added symbols (not matched by exact qualified_name)
    let removed: Vec<&Symbol> = old_symbols
        .iter()
        .filter(|s| !new_map.contains_key(s.qualified_name.as_str()))
        .copied()
        .collect();
    let added: Vec<&Symbol> = new_symbols
        .iter()
        .filter(|s| !old_map.contains_key(s.qualified_name.as_str()))
        .copied()
        .collect();

    // ── Phase 1: Relocation detection ────────────────────────────────
    // Match removed+added symbols by canonical path (stripping /deprecated/
    // and /next/). This catches "moved to deprecated" patterns and runs
    // BEFORE rename detection to reduce the search space.
    let (relocations, _skip_removed, _skip_added) = detect_relocations(&removed, &added);

    let relocated_old: HashSet<&str> = relocations
        .iter()
        .map(|r| r.old.qualified_name.as_str())
        .collect();
    let relocated_new: HashSet<&str> = relocations
        .iter()
        .map(|r| r.new.qualified_name.as_str())
        .collect();

    // Emit relocation changes and diff members of relocated symbols
    for reloc in &relocations {
        match reloc.relocation_type {
            RelocationType::MovedToDeprecated => {
                changes.push(StructuralChange {
                    symbol: reloc.old.name.clone(),
                    qualified_name: reloc.old.qualified_name.clone(),
                    kind: format!("{:?}", reloc.old.kind),
                    change_type: StructuralChangeType::SymbolMovedToDeprecated,
                    before: Some(reloc.old.qualified_name.clone()),
                    after: Some(reloc.new.qualified_name.clone()),
                    description: format!(
                        "{} `{}` moved to deprecated exports",
                        kind_label(reloc.old.kind),
                        reloc.old.name
                    ),
                    is_breaking: true,
                    impact: None,
                    migration_target: None,
                });
            }
            RelocationType::PromotedFromDeprecated => {
                changes.push(StructuralChange {
                    symbol: reloc.old.name.clone(),
                    qualified_name: reloc.old.qualified_name.clone(),
                    kind: format!("{:?}", reloc.old.kind),
                    change_type: StructuralChangeType::SymbolAdded,
                    before: Some(reloc.old.qualified_name.clone()),
                    after: Some(reloc.new.qualified_name.clone()),
                    description: format!(
                        "{} `{}` promoted from deprecated to main exports",
                        kind_label(reloc.old.kind),
                        reloc.old.name
                    ),
                    is_breaking: false,
                    impact: None,
                    migration_target: None,
                });
            }
            RelocationType::PromotedFromNext => {
                changes.push(StructuralChange {
                    symbol: reloc.old.name.clone(),
                    qualified_name: reloc.old.qualified_name.clone(),
                    kind: format!("{:?}", reloc.old.kind),
                    change_type: StructuralChangeType::SymbolRenamed,
                    before: Some(reloc.old.qualified_name.clone()),
                    after: Some(reloc.new.qualified_name.clone()),
                    description: format!(
                        "{} `{}` promoted from next (preview) to main exports — import path changed",
                        kind_label(reloc.old.kind),
                        reloc.old.name
                    ),
                    is_breaking: true,
                    impact: None,
                    migration_target: None,
                });
            }
            RelocationType::MovedToNext => {
                changes.push(StructuralChange {
                    symbol: reloc.old.name.clone(),
                    qualified_name: reloc.old.qualified_name.clone(),
                    kind: format!("{:?}", reloc.old.kind),
                    change_type: StructuralChangeType::SymbolRenamed,
                    before: Some(reloc.old.qualified_name.clone()),
                    after: Some(reloc.new.qualified_name.clone()),
                    description: format!(
                        "{} `{}` moved to next (preview) exports — import path changed",
                        kind_label(reloc.old.kind),
                        reloc.old.name
                    ),
                    is_breaking: true,
                    impact: None,
                    migration_target: None,
                });
            }
            RelocationType::Relocated => {
                // General restructuring — non-breaking if the symbol
                // is still exported at the package level.
                // Don't emit a top-level change, just diff members below.
            }
        }

        // Diff members of relocated symbols to catch property-level changes
        // (e.g., ChipProps lost the `component` prop when moved to deprecated)
        diff_symbol(reloc.old, reloc.new, &mut changes, semantics);
    }

    // ── Phase 2: Rename detection ────────────────────────────────────
    // Match remaining removed+added pairs by type fingerprint + name similarity.
    // Relocated symbols are excluded.
    let remaining_removed: Vec<&Symbol> = removed
        .iter()
        .filter(|s| !relocated_old.contains(s.qualified_name.as_str()))
        .copied()
        .collect();
    let remaining_added: Vec<&Symbol> = added
        .iter()
        .filter(|s| !relocated_new.contains(s.qualified_name.as_str()))
        .copied()
        .collect();

    let renames = detect_renames(&remaining_removed, &remaining_added);
    let renamed_old: HashSet<&str> = renames
        .iter()
        .map(|r| r.old.qualified_name.as_str())
        .collect();
    let renamed_new: HashSet<&str> = renames
        .iter()
        .map(|r| r.new.qualified_name.as_str())
        .collect();

    // Emit rename changes
    for rm in &renames {
        // Skip no-op renames: same export name, different file path.
        // This happens when a symbol is relocated (e.g., src/components/ →
        // src/victory/components/) without changing its export name. The
        // consumer's import statement doesn't change, so no rule is needed.
        if rm.old.name == rm.new.name {
            eprintln!(
                "  [trace] Skipping no-op rename: {} (moved from {} to {})",
                rm.old.name, rm.old.qualified_name, rm.new.qualified_name
            );
            continue;
        }

        changes.push(StructuralChange {
            symbol: rm.old.name.clone(),
            qualified_name: rm.old.qualified_name.clone(),
            kind: format!("{:?}", rm.old.kind),
            change_type: StructuralChangeType::SymbolRenamed,
            before: Some(rm.old.name.clone()),
            after: Some(rm.new.name.clone()),
            description: format!(
                "Exported {} `{}` was renamed to `{}`",
                kind_label(rm.old.kind),
                rm.old.name,
                rm.new.name
            ),
            is_breaking: true,
            impact: None,
            migration_target: None,
        });
    }

    // ── Phase 3: Unmatched symbols ───────────────────────────────────
    // Emit remaining removed symbols (not relocated, not renamed)
    for sym in &removed {
        if relocated_old.contains(sym.qualified_name.as_str())
            || renamed_old.contains(sym.qualified_name.as_str())
        {
            continue;
        }
        changes.push(StructuralChange {
            symbol: sym.name.clone(),
            qualified_name: sym.qualified_name.clone(),
            kind: format!("{:?}", sym.kind),
            change_type: StructuralChangeType::SymbolRemoved,
            before: Some(symbol_summary(sym)),
            after: None,
            description: format!(
                "Exported {} `{}` was removed",
                kind_label(sym.kind),
                sym.name
            ),
            is_breaking: true,
            impact: None,
            migration_target: None,
        });
    }

    // Emit remaining added symbols (not relocated, not renamed)
    for sym in &added {
        if relocated_new.contains(sym.qualified_name.as_str())
            || renamed_new.contains(sym.qualified_name.as_str())
        {
            continue;
        }
        changes.push(StructuralChange {
            symbol: sym.name.clone(),
            qualified_name: sym.qualified_name.clone(),
            kind: format!("{:?}", sym.kind),
            change_type: StructuralChangeType::SymbolAdded,
            before: None,
            after: Some(symbol_summary(sym)),
            description: format!("Exported {} `{}` was added", kind_label(sym.kind), sym.name),
            is_breaking: false,
            impact: None,
            migration_target: None,
        });
    }

    // ── Phase 4: Compare matched symbols ─────────────────────────────
    // Symbols that matched by exact qualified_name — diff their contents.
    for sym_old in &old_symbols {
        if let Some(sym_new) = new_map.get(sym_old.qualified_name.as_str()) {
            diff_symbol(sym_old, sym_new, &mut changes, semantics);
        }
    }

    // ── Phase 5: Structural migration detection ────────────────────────
    // For removed interfaces/classes, look for surviving or added interfaces
    // in the same component directory with significant member name overlap.
    // This detects "merge child into parent" and "same-name replacement"
    // patterns and annotates the existing SymbolRemoved changes with
    // migration target metadata.
    {
        let final_removed: Vec<&Symbol> = removed
            .iter()
            .filter(|s| {
                !relocated_old.contains(s.qualified_name.as_str())
                    && !renamed_old.contains(s.qualified_name.as_str())
            })
            .copied()
            .collect();

        let migrations = detect_migrations(&final_removed, &old_symbols, &new_symbols, semantics);

        // Annotate existing SymbolRemoved changes with migration targets.
        for mig in &migrations {
            for change in changes.iter_mut() {
                if change.qualified_name == mig.removed.qualified_name
                    && change.change_type == StructuralChangeType::SymbolRemoved
                {
                    change.change_type = StructuralChangeType::MigrationSuggested;
                    change.migration_target = Some(mig.target.clone());
                    // Enrich the description with the full migration recipe
                    // so the rule message gives the LLM actionable context.
                    let mut matching_names: Vec<&str> = mig
                        .target
                        .matching_members
                        .iter()
                        .map(|m| m.old_name.as_str())
                        .collect();
                    matching_names.sort();
                    let mut removed_names: Vec<&str> = mig
                        .target
                        .removed_only_members
                        .iter()
                        .map(|s| s.as_str())
                        .collect();
                    removed_names.sort();
                    let base = change.description.trim_end_matches(" was removed");

                    // When removed and replacement have the same name but live
                    // in different packages (e.g., SelectOptionProps moved from
                    // deprecated to the main package), add explicit import
                    // guidance so the LLM changes the import source rather than
                    // creating a local replacement type.
                    let import_hint = if mig.target.removed_symbol == mig.target.replacement_symbol
                        && mig.target.removed_qualified_name
                            != mig.target.replacement_qualified_name
                    {
                        let old_path = &mig.target.removed_qualified_name;
                        let new_path = &mig.target.replacement_qualified_name;

                        // Derive npm import paths from qualified names
                        // e.g., "packages/react-core/src/deprecated/..." -> "@patternfly/react-core/deprecated"
                        //        "packages/react-core/src/components/..." -> "@patternfly/react-core"
                        let to_import_path = |qn: &str| -> String {
                            // Extract "packages/<pkg-name>/src/..." and convert
                            if let Some(rest) = qn.strip_prefix("packages/") {
                                if let Some(idx) = rest.find("/src/") {
                                    let pkg = &rest[..idx];
                                    let after_src = &rest[idx + 5..]; // skip "/src/"
                                    if after_src.starts_with("deprecated") {
                                        return format!("@patternfly/{}/deprecated", pkg);
                                    }
                                    return format!("@patternfly/{}", pkg);
                                }
                            }
                            qn.to_string()
                        };

                        let old_import = to_import_path(old_path);
                        let new_import = to_import_path(new_path);

                        if old_import != new_import {
                            format!(
                                "\n  Import change: replace `import {{ {} }} from '{}'` with `import {{ {} }} from '{}'`",
                                mig.target.removed_symbol, old_import,
                                mig.target.replacement_symbol, new_import,
                            )
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };

                    let mut desc = format!(
                        "{} was removed — migrate to `{}`.{}\n  Matching props (use on `{}` instead): {}",
                        base,
                        mig.target.replacement_symbol,
                        import_hint,
                        mig.target.replacement_symbol,
                        matching_names.join(", "),
                    );
                    if !removed_names.is_empty() {
                        desc.push_str(&format!(
                            "\n  Removed props with no direct equivalent: {}",
                            removed_names.join(", "),
                        ));
                    }
                    change.description = desc;
                    break;
                }
            }
        }
    }

    // ── Phase 6: Language-specific post-processing ────────────────────
    // Each language can clean up the change list. For TypeScript, this
    // deduplicates default export changes when a named sibling exists.
    semantics.post_process(&mut changes);

    changes
}

/// Backward-compatible wrapper that uses the default (TypeScript) semantics.
///
/// This exists so that existing callers (orchestrator, tests, convenience
/// function in traits.rs) continue to work without modification.
/// Will be removed once all callers migrate to `diff_surfaces_with_semantics`.
pub fn diff_surfaces(old: &ApiSurface, new: &ApiSurface) -> Vec<StructuralChange> {
    diff_surfaces_with_semantics(old, new, &DefaultSemantics)
}

/// Default semantics that replicates the original hardcoded TypeScript behavior.
///
/// This is a temporary shim that preserves backward compatibility. It will be
/// removed when all callers switch to passing an explicit `LanguageSemantics`.
pub(crate) struct DefaultSemantics;

impl LanguageSemantics for DefaultSemantics {
    fn is_member_addition_breaking(&self, container: &Symbol, member: &Symbol) -> bool {
        // Original TS behavior from compare.rs
        match container.kind {
            SymbolKind::Interface | SymbolKind::TypeAlias => {
                let is_optional = member
                    .signature
                    .as_ref()
                    .and_then(|s| s.parameters.first())
                    .map(|p| p.optional)
                    .unwrap_or(false);
                !is_optional
            }
            _ => false,
        }
    }

    fn same_family(&self, a: &Symbol, b: &Symbol) -> bool {
        canonical_component_dir(&a.file.to_string_lossy())
            == canonical_component_dir(&b.file.to_string_lossy())
    }

    fn same_identity(&self, a: &Symbol, b: &Symbol) -> bool {
        strip_props_suffix(&a.name) == strip_props_suffix(&b.name)
    }

    fn visibility_rank(&self, v: crate::types::Visibility) -> u8 {
        helpers::visibility_rank(v)
    }

    fn parse_union_values(&self, type_str: &str) -> Option<std::collections::BTreeSet<String>> {
        parse_union_literals(type_str)
    }

    fn post_process(&self, changes: &mut Vec<StructuralChange>) {
        dedup_default_exports(changes);
    }
}

/// Parse TypeScript string literal union type (used by DefaultSemantics).
fn parse_union_literals(type_str: &str) -> Option<std::collections::BTreeSet<String>> {
    if !type_str.contains('\'') && !type_str.contains('"') {
        return None;
    }
    if !type_str.contains('|') {
        return None;
    }
    let mut literals = std::collections::BTreeSet::new();
    for part in type_str.split('|') {
        let trimmed = part.trim();
        if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
            || (trimmed.starts_with('"') && trimmed.ends_with('"'))
        {
            let value = &trimmed[1..trimmed.len() - 1];
            if !value.is_empty() {
                literals.insert(value.to_string());
            }
        }
    }
    if literals.len() >= 2 {
        Some(literals)
    } else {
        None
    }
}

/// Extract component directory, stripping /deprecated/ and /next/ (used by DefaultSemantics).
fn canonical_component_dir(file_path: &str) -> String {
    let canonical = file_path
        .replace("/deprecated/", "/")
        .replace("/next/", "/");
    let canonical = if canonical.starts_with("deprecated/") {
        canonical.strip_prefix("deprecated/").unwrap().to_string()
    } else {
        canonical
    };
    let canonical = if canonical.starts_with("next/") {
        canonical.strip_prefix("next/").unwrap().to_string()
    } else {
        canonical
    };
    match canonical.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => canonical,
    }
}

/// Strip "Props" suffix (used by DefaultSemantics).
fn strip_props_suffix(name: &str) -> &str {
    name.strip_suffix("Props").unwrap_or(name)
}

/// Remove redundant `default` export changes when a named sibling from the
/// same file has the same change type.
///
/// Pattern: `packages/react-tokens/.../c_button.c_button` (named) and
/// `packages/react-tokens/.../c_button.default` (default) both removed.
/// We keep the named one and suppress the default.
fn dedup_default_exports(changes: &mut Vec<StructuralChange>) {
    // Build a set of (file_prefix, change_type) for all non-default changes.
    // We use owned Strings to avoid borrowing from `changes`.
    let named_changes: HashSet<(String, StructuralChangeType)> = changes
        .iter()
        .filter(|c| c.symbol != "default")
        .filter_map(|c| {
            file_prefix(&c.qualified_name).map(|prefix| (prefix.to_string(), c.change_type.clone()))
        })
        .collect();

    // Retain changes that are either not `default` or don't have a named sibling
    changes.retain(|c| {
        if c.symbol != "default" {
            return true;
        }
        // Check if there's a named sibling with the same change type
        if let Some(prefix) = file_prefix(&c.qualified_name) {
            !named_changes.contains(&(prefix.to_string(), c.change_type.clone()))
        } else {
            true
        }
    });
}

/// Extract the file prefix from a qualified_name (everything before the last `.`).
///
/// `packages/react-tokens/dist/esm/c_button.default` → `packages/react-tokens/dist/esm/c_button`
fn file_prefix(qualified_name: &str) -> Option<&str> {
    qualified_name.rsplit_once('.').map(|(prefix, _)| prefix)
}
