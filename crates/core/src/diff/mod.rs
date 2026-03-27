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
use crate::types::{ApiSurface, ChangeSubject, StructuralChange, StructuralChangeType, Symbol};
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
                    kind: reloc.old.kind,
                    package: reloc.old.package.clone(),
                    change_type: StructuralChangeType::Relocated {
                        from: ChangeSubject::Symbol {
                            kind: reloc.old.kind,
                        },
                        to: ChangeSubject::Symbol {
                            kind: reloc.old.kind,
                        },
                    },
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
                    kind: reloc.old.kind,
                    package: reloc.old.package.clone(),
                    change_type: StructuralChangeType::Added(ChangeSubject::Symbol {
                        kind: reloc.old.kind,
                    }),
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
                    kind: reloc.old.kind,
                    package: reloc.old.package.clone(),
                    change_type: StructuralChangeType::Renamed {
                        from: ChangeSubject::Symbol { kind: reloc.old.kind },
                        to: ChangeSubject::Symbol { kind: reloc.new.kind },
                    },
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
                    kind: reloc.old.kind,
                    package: reloc.old.package.clone(),
                    change_type: StructuralChangeType::Renamed {
                        from: ChangeSubject::Symbol {
                            kind: reloc.old.kind,
                        },
                        to: ChangeSubject::Symbol {
                            kind: reloc.new.kind,
                        },
                    },
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
            tracing::trace!(
                name = %rm.old.name,
                from = %rm.old.qualified_name,
                to = %rm.new.qualified_name,
                "Skipping no-op rename (moved without export name change)"
            );
            continue;
        }

        changes.push(StructuralChange {
            symbol: rm.old.name.clone(),
            qualified_name: rm.old.qualified_name.clone(),
            kind: rm.old.kind,
            package: rm.old.package.clone(),
            change_type: StructuralChangeType::Renamed {
                from: ChangeSubject::Symbol { kind: rm.old.kind },
                to: ChangeSubject::Symbol { kind: rm.new.kind },
            },
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
            kind: sym.kind,
            package: sym.package.clone(),
            change_type: StructuralChangeType::Removed(ChangeSubject::Symbol { kind: sym.kind }),
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
            kind: sym.kind,
            package: sym.package.clone(),
            change_type: StructuralChangeType::Added(ChangeSubject::Symbol { kind: sym.kind }),
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

        // Annotate existing Removed(Symbol) changes with migration targets.
        // MigrationSuggested is now represented as Removed(Symbol) with migration_target set.
        for mig in &migrations {
            for change in changes.iter_mut() {
                if change.qualified_name == mig.removed.qualified_name
                    && matches!(
                        change.change_type,
                        StructuralChangeType::Removed(ChangeSubject::Symbol { .. })
                    )
                {
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
                        // Use package fields (set by the language's extractor) for import paths.
                        // Falls back to qualified names if package is not set.
                        let old_import = mig
                            .target
                            .removed_package
                            .as_deref()
                            .unwrap_or(&mig.target.removed_qualified_name);
                        let new_import = mig
                            .target
                            .replacement_package
                            .as_deref()
                            .unwrap_or(&mig.target.replacement_qualified_name);

                        if old_import != new_import {
                            format!(
                                "\n  Import change: {}",
                                semantics.format_import_change(
                                    &mig.target.removed_symbol,
                                    old_import,
                                    new_import,
                                ),
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

/// Compare two API surfaces using minimal semantics (no language-specific rules).
///
/// For language-aware diffing, use `diff_surfaces_with_semantics` instead.
pub fn diff_surfaces(old: &ApiSurface, new: &ApiSurface) -> Vec<StructuralChange> {
    diff_surfaces_with_semantics(old, new, &MinimalSemantics)
}

/// Minimal semantics for testing the diff engine without language-specific rules.
///
/// Returns conservative defaults: no member additions are breaking,
/// symbols in the same directory are the same family, identity is by name only.
/// No union parsing, no post-processing.
pub(crate) struct MinimalSemantics;

impl LanguageSemantics for MinimalSemantics {
    fn is_member_addition_breaking(&self, _container: &Symbol, _member: &Symbol) -> bool {
        false
    }

    fn same_family(&self, a: &Symbol, b: &Symbol) -> bool {
        // Same directory = same family (generic, no TS assumptions)
        let a_dir = a
            .file
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let b_dir = b
            .file
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        a_dir == b_dir
    }

    fn same_identity(&self, a: &Symbol, b: &Symbol) -> bool {
        a.name == b.name
    }

    fn visibility_rank(&self, v: crate::types::Visibility) -> u8 {
        helpers::visibility_rank(v)
    }
}
