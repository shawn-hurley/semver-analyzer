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
    /// The migration target metadata.
    pub target: MigrationTarget,
}

/// Detect structural migration patterns among removed symbols and the full
/// old + new surfaces.
///
/// For each removed interface/class, looks for surviving or added interfaces
/// in the same component directory with significant member name overlap.
///
/// When no same-directory candidate is found, `dir_renames` provides a fallback:
/// if a Phase 2 rename detected a cross-directory move (e.g., `TextVariants` in
/// `Text/` renamed to `ContentVariants` in `Content/`), the directory mapping
/// `Text/ → Content/` lets us search the target directory for candidates.
///
/// `removed` — symbols removed between old and new surfaces (not relocated, not renamed).
/// `old_symbols` — all symbols from the old surface.
/// `new_symbols` — all symbols from the new surface.
/// `dir_renames` — directory mappings from Phase 2 rename detection (old_dir → [new_dirs]).
pub(super) fn detect_migrations<'a>(
    removed: &[&'a Symbol],
    old_symbols: &[&'a Symbol],
    new_symbols: &[&'a Symbol],
    semantics: &dyn crate::traits::LanguageSemantics,
    dir_renames: &HashMap<String, Vec<String>>,
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

    // Collect new container symbols for candidate matching.
    let new_containers: Vec<&Symbol> = new_symbols
        .iter()
        .filter(|s| is_container_kind(s.kind))
        .copied()
        .collect();

    // Also index old surviving symbols (interfaces that exist in both versions
    // but gained new members). We detect "added members" by checking which
    // members exist in the new version but not the old.
    let old_by_qname: HashMap<&str, &Symbol> = old_symbols
        .iter()
        .filter(|s| is_container_kind(s.kind))
        .map(|s| (s.qualified_name.as_str(), *s))
        .collect();

    let mut results = Vec::new();
    let mut matched_removed: HashSet<&str> = HashSet::new();

    for removed_sym in &removed_interfaces {
        let removed_members: HashSet<&str> = removed_sym
            .members
            .iter()
            .map(|m| m.name.as_str())
            .collect();

        if removed_members.is_empty() {
            continue;
        }

        // Find candidate replacements in the same family (same_family check).
        let mut candidates: Vec<&&Symbol> = new_containers
            .iter()
            .filter(|c| semantics.same_family(removed_sym, c))
            .collect();

        // Cross-directory fallback: if no same-family candidates exist but a
        // Phase 2 rename links this symbol's directory to others, search there.
        if candidates.is_empty() && !dir_renames.is_empty() {
            let removed_dir = removed_sym
                .file
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Some(target_dirs) = dir_renames.get(&removed_dir) {
                candidates = new_containers
                    .iter()
                    .filter(|c| {
                        c.file
                            .parent()
                            .map(|p| {
                                let ps = p.to_string_lossy();
                                target_dirs.iter().any(|td| ps == td.as_str())
                            })
                            .unwrap_or(false)
                    })
                    .collect();
                if !candidates.is_empty() {
                    tracing::debug!(
                        removed = %removed_sym.name,
                        from_dir = %removed_dir,
                        target_dirs = ?target_dirs,
                        candidate_count = candidates.len(),
                        "Cross-directory fallback via rename chain"
                    );
                }
            }
        }

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

            // Also check same-identity replacement: when a removed interface
            // represents the same concept as a surviving candidate (e.g.,
            // deprecated Select -> main Select, or ButtonProps -> Button),
            // compare ALL members, not just added.
            let is_same_identity = semantics.same_identity(removed_sym, candidate);

            let effective_candidate_members: HashSet<&str> = if is_same_identity {
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
                .is_none_or(|(_, r, _, _)| best_ratio > *r)
            {
                best_match = Some((*candidate, best_ratio, matching, removed_only));
            }
        }

        if let Some((replacement, ratio, matching_members, removed_only_members)) = best_match {
            if !matched_removed.contains(removed_sym.qualified_name.as_str()) {
                matched_removed.insert(&removed_sym.qualified_name);
                // Capture base type changes (extends clause) when they differ.
                // This tells the LLM that inherited members may have changed —
                // e.g., extending React.HTMLProps → MenuItemProps means HTML
                // attributes like `label`, `title`, etc. are no longer inherited.
                let old_extends = removed_sym.extends.clone();
                let new_extends = replacement.extends.clone();
                let (old_ext, new_ext) = if old_extends != new_extends {
                    (old_extends, new_extends)
                } else {
                    (None, None)
                };

                results.push(MigrationMatch {
                    removed: removed_sym,
                    target: MigrationTarget {
                        removed_symbol: removed_sym.name.clone(),
                        removed_qualified_name: removed_sym.qualified_name.clone(),
                        removed_package: removed_sym.package.clone(),
                        replacement_symbol: replacement.name.clone(),
                        replacement_qualified_name: replacement.qualified_name.clone(),
                        replacement_package: replacement.package.clone(),
                        matching_members,
                        removed_only_members,
                        overlap_ratio: ratio,
                        old_extends: old_ext,
                        new_extends: new_ext,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::MinimalSemantics;
    use crate::types::{Symbol, SymbolKind, Visibility};

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

        let results = detect_migrations(
            &removed,
            &old_symbols,
            &new_symbols,
            &MinimalSemantics,
            &HashMap::<String, Vec<String>>::new(),
        );
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
        // This test exercises deprecated/ → main path migration which requires
        // language-specific same_family behavior (stripping /deprecated/).
        // MinimalSemantics uses plain directory comparison, so this test
        // is covered by the ts crate's baseline_migration tests instead.
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

        let results = detect_migrations(
            &removed,
            &old_symbols,
            &new_symbols,
            &MinimalSemantics,
            &HashMap::<String, Vec<String>>::new(),
        );
        // MinimalSemantics uses plain directory comparison, so deprecated/
        // and non-deprecated paths are different families. No migration is
        // detected. Language-specific same_family behavior (stripping
        // /deprecated/) is tested in the ts crate's baseline_migration tests.
        assert_eq!(results.len(), 0);
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

        let results = detect_migrations(
            &removed,
            &old_symbols,
            &new_symbols,
            &MinimalSemantics,
            &HashMap::<String, Vec<String>>::new(),
        );
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

        let results = detect_migrations(
            &removed,
            &old_symbols,
            &new_symbols,
            &MinimalSemantics,
            &HashMap::<String, Vec<String>>::new(),
        );
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

        let results = detect_migrations(
            &removed,
            &old_symbols,
            &new_symbols,
            &MinimalSemantics,
            &HashMap::<String, Vec<String>>::new(),
        );
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

        let results = detect_migrations(
            &removed,
            &old_symbols,
            &new_symbols,
            &MinimalSemantics,
            &HashMap::<String, Vec<String>>::new(),
        );

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

        let results = detect_migrations(
            &removed,
            &old_symbols,
            &new_symbols,
            &MinimalSemantics,
            &HashMap::<String, Vec<String>>::new(),
        );
        assert!(
            results.is_empty(),
            "Should NOT produce migration for non-matching single-member interface"
        );
    }

    #[test]
    fn test_cross_directory_migration_via_rename_chain() {
        // Simulates Text/ -> Content/ rename: TextProps (in Text/) should match
        // ContentProps (in Content/) when a dir_renames mapping exists.
        let old_text_props = make_interface(
            "TextProps",
            "components/Text/Text.d.ts",
            &[
                "component",
                "children",
                "className",
                "isVisitedLink",
                "ouiaId",
                "ouiaSafe",
            ],
        );

        let new_content_props = make_interface(
            "ContentProps",
            "components/Content/Content.d.ts",
            &[
                "component",
                "children",
                "className",
                "isVisitedLink",
                "ouiaId",
                "ouiaSafe",
                "isEditorial",
            ],
        );

        let old_symbols: Vec<&Symbol> = vec![&old_text_props];
        let new_symbols: Vec<&Symbol> = vec![&new_content_props];
        let removed: Vec<&Symbol> = vec![&old_text_props];

        // Without dir_renames: no match (different directories)
        let results = detect_migrations(
            &removed,
            &old_symbols,
            &new_symbols,
            &MinimalSemantics,
            &HashMap::<String, Vec<String>>::new(),
        );
        assert!(
            results.is_empty(),
            "Without dir_renames, cross-directory should NOT match"
        );

        // With dir_renames from a Phase 2 rename: should match
        let mut dir_renames = HashMap::new();
        dir_renames.insert(
            "components/Text".to_string(),
            vec!["components/Content".to_string()],
        );

        let results = detect_migrations(
            &removed,
            &old_symbols,
            &new_symbols,
            &MinimalSemantics,
            &dir_renames,
        );
        assert_eq!(
            results.len(),
            1,
            "Should find TextProps -> ContentProps via dir_renames"
        );
        assert_eq!(results[0].target.replacement_symbol, "ContentProps");
        assert_eq!(results[0].target.matching_members.len(), 6);
        assert!(results[0].target.removed_only_members.is_empty());
        assert!(results[0].target.overlap_ratio > 0.85);
    }
}
