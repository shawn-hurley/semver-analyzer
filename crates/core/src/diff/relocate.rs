//! Path-based relocation detection for the diff engine.
//!
//! Detects when a symbol's file path changed between versions while
//! its name and kind remained the same. The most common case is a symbol
//! moving from `components/X/` to `deprecated/components/X/`.
//!
//! This runs BEFORE fingerprint-based rename detection because:
//! 1. Exact name matching is O(n+m) vs O(n*m) for fingerprints
//! 2. It removes matched pairs early, reducing the search space for renames
//! 3. It avoids the MAX_GROUP_SIZE cap that blocks thousands of Variable
//!    symbols from being matched by fingerprint

use crate::types::{Symbol, SymbolKind};
use std::collections::HashMap;

/// A detected symbol relocation: same name+kind but different file path.
#[derive(Debug)]
pub(super) struct RelocationMatch<'a, M: Default + Clone + PartialEq = ()> {
    pub old: &'a Symbol<M>,
    pub new: &'a Symbol<M>,
    pub relocation_type: RelocationType,
}

/// What kind of path change occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RelocationType {
    /// Moved from a non-deprecated path to a deprecated path.
    MovedToDeprecated,
    /// Moved from a deprecated path to a non-deprecated path (promotion).
    PromotedFromDeprecated,
    /// Moved from a `next/` (preview) path to a main path (stabilized).
    PromotedFromNext,
    /// Moved from a main path to a `next/` (preview) path.
    MovedToNext,
    /// Moved between non-deprecated paths (restructuring).
    Relocated,
}

/// Detect symbol relocations among removed and added symbol lists.
///
/// Matches removed and added symbols by canonical path — the qualified_name
/// normalized by `canonical_fn` to strip lifecycle path segments (e.g.,
/// `/deprecated/`, `/next/`). When the canonical paths match, the symbol
/// moved rather than being removed+added.
///
/// Returns: (matched relocations, indices of removed to skip, indices of added to skip)
pub(super) fn detect_relocations<'a, M, F, C>(
    removed: &[&'a Symbol<M>],
    added: &[&'a Symbol<M>],
    canonical_fn: F,
    classify_fn: C,
) -> (Vec<RelocationMatch<'a, M>>, Vec<usize>, Vec<usize>)
where
    M: Default + Clone + PartialEq,
    F: Fn(&str) -> String,
    C: Fn(&str, &str) -> RelocationType,
{
    if removed.is_empty() || added.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new());
    }

    // Build a map of added symbols by (canonical_path, kind)
    // Multiple added symbols might share the same canonical path (rare but possible).
    // We use Vec to handle that, and match greedily.
    #[allow(clippy::type_complexity)]
    let mut added_by_canonical: HashMap<(String, SymbolKind), Vec<(usize, &'a Symbol<M>)>> =
        HashMap::new();
    for (ai, sym) in added.iter().enumerate() {
        let canonical = canonical_fn(&sym.qualified_name);
        added_by_canonical
            .entry((canonical, sym.kind))
            .or_default()
            .push((ai, sym));
    }

    let mut matches = Vec::new();
    let mut skip_removed = Vec::new();
    let mut skip_added = Vec::new();

    for (ri, rsym) in removed.iter().enumerate() {
        let canonical = canonical_fn(&rsym.qualified_name);
        let key = (canonical, rsym.kind);

        if let Some(added_syms) = added_by_canonical.get_mut(&key) {
            // Find the best match: prefer exact name match, then first available
            let best_idx = added_syms
                .iter()
                .position(|(_, asym)| asym.name == rsym.name)
                .or({
                    // If no exact name match, take first available
                    if !added_syms.is_empty() {
                        Some(0)
                    } else {
                        None
                    }
                });

            if let Some(idx) = best_idx {
                let (ai, asym) = added_syms.remove(idx);
                let relocation_type = classify_fn(&rsym.qualified_name, &asym.qualified_name);
                matches.push(RelocationMatch {
                    old: rsym,
                    new: asym,
                    relocation_type,
                });
                skip_removed.push(ri);
                skip_added.push(ai);
            }
        }
    }

    (matches, skip_removed, skip_added)
}

// `canonical_path` and `classify_relocation` have been extracted to
// `LanguageSemantics::canonical_name_for_relocation` and
// `LanguageSemantics::classify_relocation` trait methods.
// Tests for the TypeScript-specific implementations are in
// `crates/ts/src/language.rs`.
