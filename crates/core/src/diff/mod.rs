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
mod relocate;
mod rename;

#[cfg(test)]
mod tests;

use crate::types::{ApiSurface, StructuralChange, StructuralChangeType, Symbol};
use std::collections::{HashMap, HashSet};

use compare::diff_symbol;
use helpers::{is_star_reexport, kind_label, symbol_summary};
use relocate::{detect_relocations, RelocationType};
use rename::detect_renames;

/// Compare two API surfaces and produce a list of structural changes.
///
/// This is the core of the TD (Top-Down) pipeline. It matches symbols by
/// `qualified_name`, then compares every field to detect additions, removals,
/// and modifications.
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
pub fn diff_surfaces(old: &ApiSurface, new: &ApiSurface) -> Vec<StructuralChange> {
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
                });
            }
            RelocationType::PromotedFromDeprecated => {
                // Promotion from deprecated is generally non-breaking
                // (the symbol is still available, just at a better path).
                // We still record it but don't mark it breaking.
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
                });
            }
            RelocationType::PromotedFromNext => {
                // Promoted from next/ (preview) to main exports.
                // This is breaking: consumers importing from the `next/`
                // path need to update their imports.
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
                });
            }
            RelocationType::MovedToNext => {
                // Moved from main to next/ (preview) — breaking,
                // consumers importing from the main path lose access.
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
        diff_symbol(reloc.old, reloc.new, &mut changes);
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
        });
    }

    // ── Phase 4: Compare matched symbols ─────────────────────────────
    // Symbols that matched by exact qualified_name — diff their contents.
    for sym_old in &old_symbols {
        if let Some(sym_new) = new_map.get(sym_old.qualified_name.as_str()) {
            diff_symbol(sym_old, sym_new, &mut changes);
        }
    }

    // ── Phase 5: Deduplicate default exports ─────────────────────────
    // Many TypeScript files export both a named export and a default export
    // for the same symbol: `export { Foo }; export default Foo;`
    // When both are removed/added/changed, reporting both is redundant.
    // Suppress `default` changes when a sibling named export from the same
    // file has the same change type.
    dedup_default_exports(&mut changes);

    changes
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
