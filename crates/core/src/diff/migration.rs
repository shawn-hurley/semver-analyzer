//! Structural migration detection via same-directory member overlap analysis.
//!
//! When a symbol (interface, class) is removed and a surviving or added symbol
//! in the same component directory shares significant member name overlap, this
//! module detects the relationship and suggests a migration target.
//!
//! This catches three common patterns:
//!
//! 1. **Merge child into parent**: Child component removed, its props added to
//!    the parent (e.g., EmptyStateHeader/EmptyStateIcon merged into EmptyState).
//!
//! 2. **Decompose into children**: Parent props removed, new sibling components
//!    promoted (e.g., Modal's title/actions props → ModalHeader/ModalBody/ModalFooter).
//!
//! 3. **Same-name replacement**: Component removed from deprecated path, same-named
//!    component survives at non-deprecated path with overlapping props
//!    (e.g., old Select → new composable Select).

use crate::types::{MemberMapping, MigrationTarget, Symbol, SymbolKind};
use std::collections::{HashMap, HashSet};

/// Minimum overlap ratio to consider a migration suggestion.
/// Set at 0.25 (25%) to catch cases like Select where only 11/86 old props
/// match, but 11/28 (39%) of the new interface matches.
const MIN_OVERLAP_RATIO: f64 = 0.25;

/// Adaptive minimum overlap count based on the removed interface's size.
///
/// A fixed minimum of 3 makes it impossible to detect absorption for small
/// interfaces like `EmptyStateIconProps` (2-3 members) even when 100% of
/// unique props match. The minimum scales with interface size:
///
/// - 1-3 members: require at least 1 match (ratio threshold catches false positives)
/// - 4-6 members: require at least 2 matches
/// - 7+ members: require at least 3 matches
fn min_overlap_count(member_count: usize) -> usize {
    match member_count {
        0 => 1,
        1..=3 => 1,
        4..=6 => 2,
        _ => 3,
    }
}

/// A detected migration relationship between a removed symbol and a candidate
/// replacement in the same component directory.
pub(super) struct MigrationMatch<'a> {
    /// The removed symbol (interface/class).
    pub removed: &'a Symbol,
    /// The candidate replacement symbol.
    pub replacement: &'a Symbol,
    /// The migration target metadata.
    pub target: MigrationTarget,
}

/// Detect structural migration patterns among removed symbols and the full
/// old + new surfaces.
///
/// For each removed interface/class, looks for surviving or added interfaces
/// in the same component directory with significant member name overlap.
///
/// `removed` — symbols removed between old and new surfaces (not relocated, not renamed).
/// `old_symbols` — all symbols from the old surface.
/// `new_symbols` — all symbols from the new surface.
/// `added_members_by_parent` — for each surviving interface, the set of member
///     names that were added in the new version (empty for newly added interfaces).
pub(super) fn detect_migrations<'a>(
    removed: &[&'a Symbol],
    old_symbols: &[&'a Symbol],
    new_symbols: &[&'a Symbol],
) -> Vec<MigrationMatch<'a>> {
    // Only consider removed interfaces and classes — these are the container
    // types whose members might have moved.
    let removed_interfaces: Vec<&&Symbol> = removed
        .iter()
        .filter(|s| is_container_kind(s.kind))
        .collect();

    if removed_interfaces.is_empty() {
        return Vec::new();
    }

    // Build a map of surviving/added interface symbols by canonical directory.
    // "Canonical directory" strips /deprecated/ and /next/ from the path so
    // that `deprecated/components/Select/` matches `components/Select/`.
    let mut new_by_dir: HashMap<String, Vec<&Symbol>> = HashMap::new();
    for sym in new_symbols {
        if !is_container_kind(sym.kind) {
            continue;
        }
        let dir = canonical_component_dir(&sym.file.to_string_lossy());
        new_by_dir.entry(dir).or_default().push(sym);
    }

    // Also index old surviving symbols (interfaces that exist in both versions
    // but gained new members). We detect "added members" by checking which
    // members exist in the new version but not the old.
    let old_by_qname: HashMap<&str, &Symbol> = old_symbols
        .iter()
        .filter(|s| is_container_kind(s.kind))
        .map(|s| (s.qualified_name.as_str(), *s))
        .collect();

    let new_by_qname: HashMap<&str, &Symbol> = new_symbols
        .iter()
        .filter(|s| is_container_kind(s.kind))
        .map(|s| (s.qualified_name.as_str(), *s))
        .collect();

    let mut results = Vec::new();
    let mut matched_removed: HashSet<&str> = HashSet::new();

    for removed_sym in &removed_interfaces {
        let removed_dir = canonical_component_dir(&removed_sym.file.to_string_lossy());
        let removed_members: HashSet<&str> = removed_sym
            .members
            .iter()
            .map(|m| m.name.as_str())
            .collect();

        if removed_members.is_empty() {
            continue;
        }

        // Find candidate replacements in the same canonical directory.
        let candidates = match new_by_dir.get(&removed_dir) {
            Some(c) => c,
            None => continue,
        };

        let mut best_match: Option<(&Symbol, f64, Vec<MemberMapping>, Vec<String>)> = None;

        for candidate in candidates {
            // Skip if it's the exact same symbol instance (same qualified_name).
            // Don't use canonical_path here — we WANT to match same-named symbols
            // at different paths (e.g., deprecated/Select vs components/Select).
            if candidate.qualified_name == removed_sym.qualified_name {
                continue;
            }

            // Compute member overlap. For surviving interfaces (exist in both
            // old and new), only count members that were ADDED in the new version.
            // For newly added interfaces, count all members.
            let candidate_members: HashSet<&str> =
                if let Some(old_candidate) = old_by_qname.get(candidate.qualified_name.as_str()) {
                    // Surviving interface: only consider newly added members.
                    let old_members: HashSet<&str> = old_candidate
                        .members
                        .iter()
                        .map(|m| m.name.as_str())
                        .collect();
                    candidate
                        .members
                        .iter()
                        .map(|m| m.name.as_str())
                        .filter(|name| !old_members.contains(name))
                        .collect()
                } else {
                    // Newly added interface: all members are candidates.
                    candidate.members.iter().map(|m| m.name.as_str()).collect()
                };

            // Also check same-name replacement: when a removed interface has
            // the same canonical base name as a surviving candidate (e.g.,
            // deprecated Select -> main Select), compare ALL members, not just added.
            let is_same_name =
                strip_props_suffix(&removed_sym.name) == strip_props_suffix(&candidate.name);

            let effective_candidate_members: HashSet<&str> = if is_same_name {
                // Same-name replacement: compare full member lists.
                candidate.members.iter().map(|m| m.name.as_str()).collect()
            } else {
                candidate_members
            };

            if effective_candidate_members.is_empty() {
                continue;
            }

            // Compute overlap.
            let matching: Vec<MemberMapping> = removed_members
                .iter()
                .filter(|m| effective_candidate_members.contains(*m))
                .map(|m| MemberMapping {
                    old_name: m.to_string(),
                    new_name: m.to_string(),
                })
                .collect();

            if matching.len() < min_overlap_count(removed_members.len()) {
                continue;
            }

            // Compute ratio: what fraction of the removed interface's members overlap.
            // For same-name replacements, also check the reverse ratio (what fraction
            // of the new interface overlaps with the old).
            let ratio_removed = matching.len() as f64 / removed_members.len() as f64;
            let ratio_new = matching.len() as f64 / effective_candidate_members.len() as f64;
            let best_ratio = ratio_removed.max(ratio_new);

            if best_ratio < MIN_OVERLAP_RATIO {
                continue;
            }

            let removed_only: Vec<String> = removed_members
                .iter()
                .filter(|m| !effective_candidate_members.contains(*m))
                .map(|m| m.to_string())
                .collect();

            // Keep the best match (highest overlap ratio).
            if best_match
                .as_ref()
                .map_or(true, |(_, r, _, _)| best_ratio > *r)
            {
                best_match = Some((*candidate, best_ratio, matching, removed_only));
            }
        }

        if let Some((replacement, ratio, matching_members, removed_only_members)) = best_match {
            if !matched_removed.contains(removed_sym.qualified_name.as_str()) {
                matched_removed.insert(&removed_sym.qualified_name);
                results.push(MigrationMatch {
                    removed: removed_sym,
                    replacement,
                    target: MigrationTarget {
                        removed_symbol: removed_sym.name.clone(),
                        removed_qualified_name: removed_sym.qualified_name.clone(),
                        replacement_symbol: replacement.name.clone(),
                        replacement_qualified_name: replacement.qualified_name.clone(),
                        matching_members,
                        removed_only_members,
                        overlap_ratio: ratio,
                    },
                });
            }
        }
    }

    results
}

/// Check if a symbol kind is a "container" (has members).
fn is_container_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Interface | SymbolKind::Class | SymbolKind::Enum
    )
}

/// Extract the component directory from a file path, stripping /deprecated/
/// and /next/ segments for canonical matching.
///
/// `packages/react-core/dist/esm/deprecated/components/Select/Select.d.ts`
/// → `packages/react-core/dist/esm/components/Select`
///
/// `packages/react-core/dist/esm/components/EmptyState/EmptyStateHeader.d.ts`
/// → `packages/react-core/dist/esm/components/EmptyState`
fn canonical_component_dir(file_path: &str) -> String {
    // Strip /deprecated/ and /next/ for canonical matching.
    // Handle both mid-path (`foo/deprecated/bar`) and start-of-path (`deprecated/bar`).
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

    // Extract directory (everything up to the last `/`).
    match canonical.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => canonical,
    }
}

/// Strip a "Props" suffix from a symbol name for comparison.
///
/// `EmptyStateHeaderProps` → `EmptyStateHeader`
/// `SelectProps` → `Select`
/// `Modal` → `Modal`
fn strip_props_suffix(name: &str) -> &str {
    name.strip_suffix("Props").unwrap_or(name)
}

/// Normalize a qualified_name by stripping `/deprecated/` and `/next/`.
fn canonical_path(qualified_name: &str) -> String {
    let result = qualified_name
        .replace("/deprecated/", "/")
        .replace("/next/", "/");
    let result = if result.starts_with("deprecated/") {
        result.strip_prefix("deprecated/").unwrap().to_string()
    } else {
        result
    };
    if result.starts_with("next/") {
        result.strip_prefix("next/").unwrap().to_string()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Symbol, SymbolKind, Visibility};
    use std::path::PathBuf;

    fn make_interface(name: &str, file: &str, members: &[&str]) -> Symbol {
        let mut sym = Symbol::new(
            name,
            &format!("{}.{}", file, name),
            SymbolKind::Interface,
            Visibility::Exported,
            file,
            1,
        );
        for member_name in members {
            sym.members.push(Symbol::new(
                *member_name,
                &format!("{}.{}.{}", file, name, member_name),
                SymbolKind::Property,
                Visibility::Public,
                file,
                1,
            ));
        }
        sym
    }

    #[test]
    fn test_merge_child_into_parent_emptystate_pattern() {
        // EmptyStateHeaderProps removed, EmptyStateProps gained matching members.
        let old_header = make_interface(
            "EmptyStateHeaderProps",
            "components/EmptyState/EmptyStateHeader.d.ts",
            &[
                "children",
                "className",
                "headingLevel",
                "icon",
                "titleClassName",
                "titleText",
            ],
        );

        // Old EmptyStateProps (in v5, no titleText/icon/headingLevel).
        let old_parent = make_interface(
            "EmptyStateProps",
            "components/EmptyState/EmptyState.d.ts",
            &["children", "className", "variant"],
        );

        // New EmptyStateProps (in v6, gained titleText, headingLevel, icon, etc.).
        let new_parent = make_interface(
            "EmptyStateProps",
            "components/EmptyState/EmptyState.d.ts",
            &[
                "children",
                "className",
                "variant",
                "titleText",
                "headingLevel",
                "icon",
                "status",
                "titleClassName",
                "headerClassName",
            ],
        );

        let old_symbols: Vec<&Symbol> = vec![&old_header, &old_parent];
        let new_symbols: Vec<&Symbol> = vec![&new_parent];
        let removed: Vec<&Symbol> = vec![&old_header];

        let results = detect_migrations(&removed, &old_symbols, &new_symbols);
        assert_eq!(results.len(), 1);

        let m = &results[0];
        assert_eq!(m.target.removed_symbol, "EmptyStateHeaderProps");
        assert_eq!(m.target.replacement_symbol, "EmptyStateProps");
        assert!(
            m.target.overlap_ratio > 0.5,
            "Expected >50% overlap, got {}",
            m.target.overlap_ratio
        );

        // Should match: headingLevel, icon, titleClassName, titleText (4 members added to parent
        // that were in the removed child).
        assert!(
            m.target.matching_members.len() >= 4,
            "Expected >= 4 matching members, got {}: {:?}",
            m.target.matching_members.len(),
            m.target
                .matching_members
                .iter()
                .map(|m| &m.old_name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_same_name_replacement_select_pattern() {
        // Old Select removed from deprecated path, new Select survives at main path.
        let old_select = make_interface(
            "SelectProps",
            "deprecated/components/Select/Select.d.ts",
            &[
                "children",
                "className",
                "isOpen",
                "isPlain",
                "onSelect",
                "isDisabled",
                "isGrouped",
                "isCreatable",
                "selections",
                "onToggle",
                "direction",
                "position",
                "toggleRef",
                "width",
                "zIndex",
                "variant",
            ],
        );

        // Old main SelectProps (existed in v5).
        let old_main_select = make_interface(
            "SelectProps",
            "components/Select/Select.d.ts",
            &[
                "children",
                "className",
                "isOpen",
                "isPlain",
                "onSelect",
                "direction",
                "position",
                "selected",
                "toggle",
                "toggleRef",
                "width",
                "zIndex",
                "onOpenChange",
                "innerRef",
                "isScrollable",
            ],
        );

        // New main SelectProps (v6, gained a few more props).
        let new_main_select = make_interface(
            "SelectProps",
            "components/Select/Select.d.ts",
            &[
                "children",
                "className",
                "isOpen",
                "isPlain",
                "onSelect",
                "direction",
                "position",
                "selected",
                "toggle",
                "toggleRef",
                "width",
                "zIndex",
                "onOpenChange",
                "innerRef",
                "isScrollable",
                "variant",
                "focusTimeoutDelay",
                "onToggleKeydown",
                "shouldPreventScrollOnItemFocus",
            ],
        );

        let old_symbols: Vec<&Symbol> = vec![&old_select, &old_main_select];
        let new_symbols: Vec<&Symbol> = vec![&new_main_select];
        let removed: Vec<&Symbol> = vec![&old_select];

        let results = detect_migrations(&removed, &old_symbols, &new_symbols);
        assert_eq!(results.len(), 1);

        let m = &results[0];
        assert_eq!(m.target.removed_symbol, "SelectProps");
        assert_eq!(m.target.replacement_symbol, "SelectProps");
        assert!(
            m.target.matching_members.len() >= 10,
            "Expected >= 10 matching members for Select, got {}",
            m.target.matching_members.len()
        );
    }

    #[test]
    fn test_decompose_into_children_modal_pattern() {
        // ModalProps lost title/actions/description, ModalHeaderProps gained title/description.
        let old_modal = make_interface(
            "ModalProps",
            "components/Modal/Modal.d.ts",
            &[
                "children",
                "className",
                "isOpen",
                "variant",
                "title",
                "actions",
                "description",
                "footer",
                "header",
                "help",
                "titleIconVariant",
                "onClose",
                "showClose",
            ],
        );

        // New ModalProps (v6, title/actions/etc removed, only base props remain).
        let new_modal = make_interface(
            "ModalProps",
            "components/Modal/Modal.d.ts",
            &["children", "className", "isOpen", "variant", "onClose"],
        );

        // New ModalHeaderProps (promoted from next/).
        let new_header = make_interface(
            "ModalHeaderProps",
            "components/Modal/ModalHeader.d.ts",
            &[
                "children",
                "className",
                "title",
                "description",
                "help",
                "titleIconVariant",
                "labelId",
                "descriptorId",
                "titleScreenReaderText",
            ],
        );

        let old_symbols: Vec<&Symbol> = vec![&old_modal];
        let new_symbols: Vec<&Symbol> = vec![&new_modal, &new_header];
        // ModalProps is not "removed" — it survived but lost members.
        // The removed members are detected as PropertyRemoved changes.
        // But we can also detect that ModalHeaderProps is new and shares members
        // with the old ModalProps.
        //
        // For this test: nothing should match because ModalProps wasn't removed.
        // The Modal decomposition pattern is detected differently — via
        // PropertyRemoved on ModalProps + new ModalHeaderProps in same dir.
        // This module handles the case where the INTERFACE is removed, not props.
        let removed: Vec<&Symbol> = vec![];

        let results = detect_migrations(&removed, &old_symbols, &new_symbols);
        assert_eq!(
            results.len(),
            0,
            "No removed interfaces => no migration suggestions"
        );
    }

    #[test]
    fn test_no_false_positive_unrelated_interfaces() {
        // Two unrelated interfaces in different directories should not match.
        let removed_foo = make_interface(
            "FooProps",
            "components/Foo/Foo.d.ts",
            &["children", "className", "onClick"],
        );

        let new_bar = make_interface(
            "BarProps",
            "components/Bar/Bar.d.ts",
            &["children", "className", "onSubmit"],
        );

        let old_symbols: Vec<&Symbol> = vec![&removed_foo];
        let new_symbols: Vec<&Symbol> = vec![&new_bar];
        let removed: Vec<&Symbol> = vec![&removed_foo];

        let results = detect_migrations(&removed, &old_symbols, &new_symbols);
        assert_eq!(results.len(), 0, "Different directories should not match");
    }

    #[test]
    fn test_small_overlap_with_adaptive_threshold() {
        // FooHeaderProps (2 members: title, subtitle) removed.
        // FooProps (new, 8 members) includes "title".
        // With adaptive thresholds: 1 match from 2-member interface = 50% ratio.
        // This IS a valid absorption signal — the removed child's primary prop
        // appeared on the parent.
        let removed_header = make_interface(
            "FooHeaderProps",
            "components/Foo/FooHeader.d.ts",
            &["title", "subtitle"],
        );

        let new_foo = make_interface(
            "FooProps",
            "components/Foo/Foo.d.ts",
            &[
                "children",
                "className",
                "title",
                "variant",
                "size",
                "isOpen",
                "onClose",
                "isFullscreen",
            ],
        );

        let old_symbols: Vec<&Symbol> = vec![&removed_header];
        let new_symbols: Vec<&Symbol> = vec![&new_foo];
        let removed: Vec<&Symbol> = vec![&removed_header];

        let results = detect_migrations(&removed, &old_symbols, &new_symbols);
        // 1 match (title) from 2-member interface = 50% ratio.
        // Adaptive min_overlap_count(2) = 1, ratio 50% > 25%.
        // This is correctly detected as an absorption.
        assert_eq!(
            results.len(),
            1,
            "Small interface with 50% match should be detected as absorption"
        );
        assert_eq!(results[0].target.removed_symbol, "FooHeaderProps");
        assert_eq!(results[0].target.replacement_symbol, "FooProps");
    }

    #[test]
    fn test_canonical_component_dir() {
        assert_eq!(
            canonical_component_dir(
                "packages/react-core/dist/esm/deprecated/components/Select/Select.d.ts"
            ),
            "packages/react-core/dist/esm/components/Select"
        );
        assert_eq!(
            canonical_component_dir(
                "packages/react-core/dist/esm/components/EmptyState/EmptyStateHeader.d.ts"
            ),
            "packages/react-core/dist/esm/components/EmptyState"
        );
        assert_eq!(
            canonical_component_dir(
                "packages/react-core/dist/esm/next/components/Modal/ModalHeader.d.ts"
            ),
            "packages/react-core/dist/esm/components/Modal"
        );
    }

    // ── Adaptive threshold: small interface absorption ──────────────

    #[test]
    fn test_small_interface_absorption_emptystate_icon() {
        // EmptyStateIconProps has only 2 unique members (icon, className).
        // className already exists on EmptyStateProps, so only icon is a new match.
        // With the old fixed MIN_OVERLAP_COUNT=3, this would NOT be detected.
        // With adaptive thresholds, 1 match from a 2-member interface (50%) passes.
        let old_icon_props = make_interface(
            "EmptyStateIconProps",
            "components/EmptyState/EmptyStateIcon.d.ts",
            &["icon", "className"],
        );

        let old_parent = make_interface(
            "EmptyStateProps",
            "components/EmptyState/EmptyState.d.ts",
            &["children", "className", "variant"],
        );

        let new_parent = make_interface(
            "EmptyStateProps",
            "components/EmptyState/EmptyState.d.ts",
            &[
                "children",
                "className",
                "variant",
                "icon", // newly added — matches EmptyStateIconProps.icon
                "titleText",
                "headingLevel",
                "status",
            ],
        );

        let old_symbols: Vec<&Symbol> = vec![&old_icon_props, &old_parent];
        let new_symbols: Vec<&Symbol> = vec![&new_parent];
        let removed: Vec<&Symbol> = vec![&old_icon_props];

        let results = detect_migrations(&removed, &old_symbols, &new_symbols);

        assert_eq!(
            results.len(),
            1,
            "Should detect EmptyStateIconProps → EmptyStateProps absorption. Got {} results.",
            results.len()
        );

        let m = &results[0];
        assert_eq!(m.target.removed_symbol, "EmptyStateIconProps");
        assert_eq!(m.target.replacement_symbol, "EmptyStateProps");

        // Should match: icon (1 member). className already existed on parent, not counted.
        let matched_names: Vec<&str> = m
            .target
            .matching_members
            .iter()
            .map(|mm| mm.old_name.as_str())
            .collect();
        assert!(
            matched_names.contains(&"icon"),
            "Should match 'icon'. Matched: {:?}",
            matched_names
        );

        // Ratio: 1 match / 2 members = 0.5
        assert!(
            m.target.overlap_ratio >= 0.25,
            "Overlap ratio should be >= 25%, got {}",
            m.target.overlap_ratio
        );
    }

    #[test]
    fn test_adaptive_threshold_values() {
        assert_eq!(min_overlap_count(0), 1);
        assert_eq!(min_overlap_count(1), 1);
        assert_eq!(min_overlap_count(2), 1);
        assert_eq!(min_overlap_count(3), 1);
        assert_eq!(min_overlap_count(4), 2);
        assert_eq!(min_overlap_count(5), 2);
        assert_eq!(min_overlap_count(6), 2);
        assert_eq!(min_overlap_count(7), 3);
        assert_eq!(min_overlap_count(10), 3);
        assert_eq!(min_overlap_count(50), 3);
    }

    #[test]
    fn test_single_member_interface_no_false_positive() {
        // A 1-member interface with no matching props on the candidate
        // should NOT produce a migration, even with relaxed thresholds.
        let old_tiny = make_interface("TinyProps", "components/Foo/Tiny.d.ts", &["uniqueProp"]);

        let old_parent = make_interface(
            "FooProps",
            "components/Foo/Foo.d.ts",
            &["children", "className"],
        );

        let new_parent = make_interface(
            "FooProps",
            "components/Foo/Foo.d.ts",
            &["children", "className", "newProp"], // no overlap with uniqueProp
        );

        let old_symbols: Vec<&Symbol> = vec![&old_tiny, &old_parent];
        let new_symbols: Vec<&Symbol> = vec![&new_parent];
        let removed: Vec<&Symbol> = vec![&old_tiny];

        let results = detect_migrations(&removed, &old_symbols, &new_symbols);
        assert!(
            results.is_empty(),
            "Should NOT produce migration for non-matching single-member interface"
        );
    }
}
