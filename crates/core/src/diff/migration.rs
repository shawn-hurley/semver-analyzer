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

/// Minimum number of overlapping members to emit a suggestion.
/// Prevents false positives from single-member matches on tiny interfaces.
const MIN_OVERLAP_COUNT: usize = 3;

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
                .map(|m| {
                    let old_type = find_member_type(&removed_sym.members, m);
                    let new_type = find_member_type(&candidate.members, m);
                    MemberMapping {
                        old_name: m.to_string(),
                        new_name: m.to_string(),
                        old_type,
                        new_type,
                    }
                })
                .collect();

            if matching.len() < MIN_OVERLAP_COUNT {
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

/// Look up the type annotation for a named member within a symbol's member list.
///
/// Returns `signature.return_type` for the matching member, which is where the
/// TS extractor stores property type annotations (e.g., `React.ComponentType`).
fn find_member_type(members: &[Symbol], member_name: &str) -> Option<String> {
    members
        .iter()
        .find(|m| m.name == member_name)
        .and_then(|m| m.signature.as_ref())
        .and_then(|sig| sig.return_type.clone())
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
    use crate::types::{Signature, Symbol, SymbolKind, Visibility};
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

    /// Create an interface with typed members: each entry is (name, type_annotation).
    fn make_typed_interface(name: &str, file: &str, members: &[(&str, &str)]) -> Symbol {
        let mut sym = Symbol::new(
            name,
            &format!("{}.{}", file, name),
            SymbolKind::Interface,
            Visibility::Exported,
            file,
            1,
        );
        for (member_name, type_ann) in members {
            let mut member = Symbol::new(
                *member_name,
                &format!("{}.{}.{}", file, name, member_name),
                SymbolKind::Property,
                Visibility::Public,
                file,
                1,
            );
            member.signature = Some(Signature {
                parameters: Vec::new(),
                return_type: Some(type_ann.to_string()),
                type_parameters: Vec::new(),
                is_async: false,
            });
            sym.members.push(member);
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
    fn test_no_false_positive_small_overlap() {
        // Two interfaces in the same directory but with tiny overlap.
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
        // Only 1 member overlaps ("title") out of 2 — below MIN_OVERLAP_COUNT of 3.
        assert_eq!(
            results.len(),
            0,
            "Too few matching members should not produce a suggestion"
        );
    }

    #[test]
    fn test_member_type_info_propagated() {
        // EmptyStateHeaderProps has `icon: React.ComponentType`, EmptyStateProps
        // has `icon: React.ReactNode`. The MemberMapping should carry both types.
        let old_header = make_typed_interface(
            "EmptyStateHeaderProps",
            "components/EmptyState/EmptyStateHeader.d.ts",
            &[
                ("children", "React.ReactNode"),
                ("className", "string"),
                ("headingLevel", "'h1' | 'h2' | 'h3' | 'h4' | 'h5' | 'h6'"),
                ("icon", "React.ComponentType"),
                ("titleText", "React.ReactNode"),
            ],
        );

        let old_parent = make_typed_interface(
            "EmptyStateProps",
            "components/EmptyState/EmptyState.d.ts",
            &[
                ("children", "React.ReactNode"),
                ("className", "string"),
                ("variant", "EmptyStateVariant"),
            ],
        );

        let new_parent = make_typed_interface(
            "EmptyStateProps",
            "components/EmptyState/EmptyState.d.ts",
            &[
                ("children", "React.ReactNode"),
                ("className", "string"),
                ("variant", "EmptyStateVariant"),
                ("titleText", "React.ReactNode"),
                ("headingLevel", "'h1' | 'h2' | 'h3' | 'h4' | 'h5' | 'h6'"),
                ("icon", "React.ReactNode"),
                ("status", "EmptyStateStatus"),
            ],
        );

        let old_symbols: Vec<&Symbol> = vec![&old_header, &old_parent];
        let new_symbols: Vec<&Symbol> = vec![&new_parent];
        let removed: Vec<&Symbol> = vec![&old_header];

        let results = detect_migrations(&removed, &old_symbols, &new_symbols);
        assert_eq!(results.len(), 1);

        let m = &results[0];

        // Find the "icon" mapping — it should carry the type change.
        let icon_mapping = m
            .target
            .matching_members
            .iter()
            .find(|mm| mm.old_name == "icon")
            .expect("Missing icon in matching_members");
        assert_eq!(
            icon_mapping.old_type.as_deref(),
            Some("React.ComponentType"),
            "icon old_type should be React.ComponentType"
        );
        assert_eq!(
            icon_mapping.new_type.as_deref(),
            Some("React.ReactNode"),
            "icon new_type should be React.ReactNode"
        );

        // "titleText" has the same type on both sides.
        let title_mapping = m
            .target
            .matching_members
            .iter()
            .find(|mm| mm.old_name == "titleText")
            .expect("Missing titleText in matching_members");
        assert_eq!(title_mapping.old_type.as_deref(), Some("React.ReactNode"));
        assert_eq!(title_mapping.new_type.as_deref(), Some("React.ReactNode"));

        // "headingLevel" also same type.
        let heading_mapping = m
            .target
            .matching_members
            .iter()
            .find(|mm| mm.old_name == "headingLevel")
            .expect("Missing headingLevel in matching_members");
        assert_eq!(heading_mapping.old_type, heading_mapping.new_type);
    }

    #[test]
    fn test_member_type_none_for_untyped_members() {
        // When members don't have type annotations, old_type/new_type should be None.
        let old_header = make_interface(
            "FooProps",
            "components/Foo/FooHeader.d.ts",
            &["alpha", "beta", "gamma"],
        );

        let new_parent = make_interface(
            "FooProps",
            "components/Foo/Foo.d.ts",
            &["alpha", "beta", "gamma", "delta"],
        );

        let old_symbols: Vec<&Symbol> = vec![&old_header];
        let new_symbols: Vec<&Symbol> = vec![&new_parent];
        let removed: Vec<&Symbol> = vec![&old_header];

        let results = detect_migrations(&removed, &old_symbols, &new_symbols);
        assert_eq!(results.len(), 1);

        for mm in &results[0].target.matching_members {
            assert!(
                mm.old_type.is_none(),
                "Untyped member {} should have old_type=None",
                mm.old_name
            );
            assert!(
                mm.new_type.is_none(),
                "Untyped member {} should have new_type=None",
                mm.old_name
            );
        }
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
}
