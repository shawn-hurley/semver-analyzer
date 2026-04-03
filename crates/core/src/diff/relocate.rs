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
pub(super) struct RelocationMatch<'a, M: Default + Clone = ()> {
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
/// with `/deprecated/` and `/next/` segments stripped out. When the canonical
/// paths match, the symbol moved rather than being removed+added.
///
/// Returns: (matched relocations, indices of removed to skip, indices of added to skip)
pub(super) fn detect_relocations<'a, M: Default + Clone>(
    removed: &[&'a Symbol<M>],
    added: &[&'a Symbol<M>],
) -> (Vec<RelocationMatch<'a, M>>, Vec<usize>, Vec<usize>) {
    if removed.is_empty() || added.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new());
    }

    // Build a map of added symbols by (canonical_path, kind)
    // Multiple added symbols might share the same canonical path (rare but possible).
    // We use Vec to handle that, and match greedily.
    let mut added_by_canonical: HashMap<(String, SymbolKind), Vec<(usize, &'a Symbol<M>)>> =
        HashMap::new();
    for (ai, sym) in added.iter().enumerate() {
        let canonical = canonical_path(&sym.qualified_name);
        added_by_canonical
            .entry((canonical, sym.kind))
            .or_default()
            .push((ai, sym));
    }

    let mut matches = Vec::new();
    let mut skip_removed = Vec::new();
    let mut skip_added = Vec::new();

    for (ri, rsym) in removed.iter().enumerate() {
        let canonical = canonical_path(&rsym.qualified_name);
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
                let relocation_type =
                    classify_relocation(&rsym.qualified_name, &asym.qualified_name);
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

/// Normalize a qualified_name by stripping `/deprecated/` and `/next/`
/// path segments, producing a canonical path for matching relocations.
///
/// Examples:
/// - `packages/react-core/dist/esm/deprecated/components/Chip/Chip.Chip`
///   → `packages/react-core/dist/esm/components/Chip/Chip.Chip`
/// - `packages/react-core/dist/esm/next/components/Modal/Modal.Modal`
///   → `packages/react-core/dist/esm/components/Modal/Modal.Modal`
/// - `packages/react-core/dist/esm/components/Button/Button.Button`
///   → unchanged
fn canonical_path(qualified_name: &str) -> String {
    qualified_name
        .replace("/deprecated/", "/")
        .replace("/next/", "/")
}

/// Classify the type of relocation based on path changes.
fn classify_relocation(old_qname: &str, new_qname: &str) -> RelocationType {
    let old_deprecated = old_qname.contains("/deprecated/");
    let new_deprecated = new_qname.contains("/deprecated/");
    let old_next = old_qname.contains("/next/");
    let new_next = new_qname.contains("/next/");

    match (old_deprecated, new_deprecated, old_next, new_next) {
        (false, true, _, _) => RelocationType::MovedToDeprecated,
        (true, false, _, _) => RelocationType::PromotedFromDeprecated,
        (_, _, true, false) => RelocationType::PromotedFromNext,
        (_, _, false, true) => RelocationType::MovedToNext,
        _ => RelocationType::Relocated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_strips_deprecated() {
        assert_eq!(
            canonical_path("pkg/dist/esm/deprecated/components/Chip/Chip.Chip"),
            "pkg/dist/esm/components/Chip/Chip.Chip"
        );
    }

    #[test]
    fn canonical_strips_next() {
        assert_eq!(
            canonical_path("pkg/dist/esm/next/components/Modal/Modal.Modal"),
            "pkg/dist/esm/components/Modal/Modal.Modal"
        );
    }

    #[test]
    fn canonical_preserves_normal_path() {
        let path = "pkg/dist/esm/components/Button/Button.Button";
        assert_eq!(canonical_path(path), path);
    }

    #[test]
    fn classify_moved_to_deprecated() {
        assert_eq!(
            classify_relocation(
                "pkg/dist/esm/components/Chip/Chip.Chip",
                "pkg/dist/esm/deprecated/components/Chip/Chip.Chip"
            ),
            RelocationType::MovedToDeprecated
        );
    }

    #[test]
    fn classify_promoted_from_deprecated() {
        assert_eq!(
            classify_relocation(
                "pkg/dist/esm/deprecated/components/Modal/Modal.Modal",
                "pkg/dist/esm/components/Modal/Modal.Modal"
            ),
            RelocationType::PromotedFromDeprecated
        );
    }

    #[test]
    fn classify_relocated() {
        assert_eq!(
            classify_relocation(
                "pkg/dist/esm/components/Chip/Chip.Chip",
                "pkg/dist/esm/components/Label/Chip.Chip"
            ),
            RelocationType::Relocated
        );
    }

    #[test]
    fn classify_promoted_from_next() {
        assert_eq!(
            classify_relocation(
                "pkg/dist/esm/next/components/Modal/ModalBody.ModalBody",
                "pkg/dist/esm/components/Modal/ModalBody.ModalBody"
            ),
            RelocationType::PromotedFromNext
        );
    }

    #[test]
    fn classify_moved_to_next() {
        assert_eq!(
            classify_relocation(
                "pkg/dist/esm/components/Foo/Foo.Foo",
                "pkg/dist/esm/next/components/Foo/Foo.Foo"
            ),
            RelocationType::MovedToNext
        );
    }
}
