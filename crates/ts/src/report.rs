//! Report-building logic for TypeScript analysis.
//!
//! Builds `AnalysisReport<TypeScript>` from `AnalysisResult<TypeScript>`,
//! including per-file change grouping, component summary aggregation,
//! constant group detection, child component discovery, and hierarchy
//! delta enrichment.
//!
//! All functions are TypeScript-specific (no generic `L: Language`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::hierarchy_types::{HierarchyDelta, MigratedMember};
use crate::symbol_data::TsSymbolData;
use semver_analyzer_core::{
    AddedExport, AnalysisMetadata, AnalysisReport, AnalysisResult, ApiChange, ApiChangeKind,
    ApiChangeType, ApiSurface, BehavioralChange, ChangeSubject, ChildComponent,
    ChildComponentStatus, Comparison, ComponentStatus, ComponentSummary, ConstantGroup,
    ExpectedChild, FileChanges, FileStatus, InferredRenamePatterns, LlmApiChange, ManifestChange,
    MemberSummary, MigrationTarget, PackageChanges, RemovalDisposition, RemovedMember,
    StructuralChange, StructuralChangeType, SuffixRename, Summary, Symbol, SymbolKind, TypeChange,
};

use crate::TypeScript;
use semver_analyzer_konveyor_core::parse_union_string_values;

// ─── Public entry point ──────────────────────────────────────────────────

/// Build the complete `AnalysisReport<TypeScript>` from analysis results.
///
/// This is the primary entry point called by `TypeScript::build_report()`.
/// It merges structural, behavioral, LLM, composition, and hierarchy data
/// into the final report.
pub(crate) fn build_report(
    results: &AnalysisResult<TypeScript>,
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
) -> AnalysisReport<TypeScript> {
    let mut report = build_report_inner(
        repo,
        from_ref,
        to_ref,
        &results.structural_changes,
        &results.behavioral_changes,
        &results.manifest_changes,
        &results.llm_api_changes,
        &results.old_surface,
        &results.new_surface,
        results.inferred_rename_patterns.clone(),
    );

    // Merge container changes into the report's file entries.
    for (source_path, comp_changes) in &results.container_changes {
        let existing = report
            .changes
            .iter_mut()
            .find(|fc| fc.file.to_string_lossy().starts_with(source_path));
        if let Some(fc) = existing {
            fc.container_changes.extend(comp_changes.clone());
        } else if !comp_changes.is_empty() {
            report.changes.push(FileChanges {
                file: PathBuf::from(source_path),
                status: FileStatus::Modified,
                renamed_from: None,
                breaking_api_changes: vec![],
                breaking_behavioral_changes: vec![],
                container_changes: comp_changes.clone(),
            });
        }
    }

    // Enrich hierarchy deltas and populate expected_children.
    if !results.extensions.hierarchy_deltas.is_empty()
        || !results.extensions.new_hierarchies.is_empty()
    {
        enrich_hierarchy_deltas(
            &mut report,
            results.extensions.hierarchy_deltas.clone(),
            &results.new_surface,
            &results.extensions.new_hierarchies,
        );
    }

    // Pass through SD pipeline results when present (v2 pipeline).
    report.extensions.sd_result = results.extensions.sd_result.clone();

    report
}

// ─── Core report building ────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn build_report_inner(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    structural_changes: &[StructuralChange],
    behavioral_changes: &[BehavioralChange<TypeScript>],
    manifest_changes: &[ManifestChange<TypeScript>],
    llm_api_changes: &[LlmApiChange],
    old_surface: &ApiSurface<TsSymbolData>,
    new_surface: &ApiSurface<TsSymbolData>,
    inferred_rename_patterns: Option<InferredRenamePatterns>,
) -> AnalysisReport<TypeScript> {
    // Group breaking structural changes by file, converting to v2 ApiChange format.
    // Non-breaking changes (symbol_added, etc.) are excluded from the report.
    let mut file_api_map: BTreeMap<PathBuf, Vec<ApiChange>> = BTreeMap::new();

    for change in structural_changes {
        if !change.is_breaking {
            continue;
        }
        let file = qualified_name_to_file(&change.qualified_name);
        let api_change = structural_to_api_change(change);
        file_api_map.entry(file).or_default().push(api_change);
    }

    // Merge LLM-detected API changes into the file map.
    // These catch type-level changes that static .d.ts analysis misses
    // (interface extends, optionality, enum member changes, etc.)
    for entry in llm_api_changes {
        let file = PathBuf::from(&entry.file_path);
        let change_type = match entry.change.as_str() {
            "type_changed" => ApiChangeType::TypeChanged,
            "removed" => ApiChangeType::Removed,
            "default_changed" => ApiChangeType::SignatureChanged,
            _ => ApiChangeType::SignatureChanged,
        };
        let kind = if entry.symbol.contains('.') {
            ApiChangeKind::Property
        } else {
            ApiChangeKind::Interface
        };
        // removal_disposition is already core's RemovalDisposition — use directly
        let removal_disposition = entry.removal_disposition.clone();

        let api_change = ApiChange {
            symbol: entry.symbol.clone(),
            kind,
            change: change_type,
            before: None,
            after: None,
            description: entry.description.clone(),
            migration_target: None,
            removal_disposition,
            renders_element: entry.renders_element.clone(),
        };
        // Only add if not already present (avoid duplicating TD findings).
        // If the symbol already exists from TD, enrich it with LLM data
        // (removal_disposition, renders_element) that TD doesn't produce.
        let existing = file_api_map.entry(file).or_default();
        if let Some(td_entry) = existing.iter_mut().find(|c| c.symbol == api_change.symbol) {
            if td_entry.removal_disposition.is_none() && api_change.removal_disposition.is_some() {
                td_entry.removal_disposition = api_change.removal_disposition;
            }
            if td_entry.renders_element.is_none() && api_change.renders_element.is_some() {
                td_entry.renders_element = api_change.renders_element;
            }
        } else {
            existing.push(api_change);
        }
    }

    // ── Cross-file enrichment pass: propagate removal_disposition ──
    //
    // The TD analysis (from .d.ts files) and BU analysis (from .tsx files)
    // produce separate ApiChange entries for the same logical prop removal.
    // The symbols differ in prefix (e.g., "ToolbarFilter.chips" from TD vs
    // "ToolbarFilterProps.chips" from BU) and live in different files.
    // BU entries carry removal_disposition from LLM analysis; TD entries don't.
    // Propagate dispositions from BU entries to any TD entry whose prop name
    // suffix matches (after the last dot).
    {
        // Collect all dispositions keyed by prop suffix (the part after the last dot)
        let mut disposition_by_prop: HashMap<String, RemovalDisposition> = HashMap::new();
        for changes in file_api_map.values() {
            for change in changes {
                if let Some(ref disp) = change.removal_disposition {
                    if let Some(prop) = change.symbol.rsplit_once('.').map(|(_, p)| p) {
                        disposition_by_prop
                            .entry(prop.to_string())
                            .or_insert_with(|| disp.clone());
                    }
                }
            }
        }
        // Apply to entries missing disposition
        if !disposition_by_prop.is_empty() {
            for changes in file_api_map.values_mut() {
                for change in changes.iter_mut() {
                    if change.removal_disposition.is_none()
                        && change.change == ApiChangeType::Removed
                    {
                        if let Some(prop) = change.symbol.rsplit_once('.').map(|(_, p)| p) {
                            if let Some(disp) = disposition_by_prop.get(prop) {
                                change.removal_disposition = Some(disp.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    // ── Reclassify ReplacedByMember with incompatible types ──────
    //
    // When a removed prop has ReplacedByMember disposition, check whether the
    // old and new types are structurally compatible. If the types are
    // fundamentally different (e.g., a named interface → an array), the change
    // is a signature restructure, not a simple rename. Reclassify these from
    // Removed → SignatureChanged so downstream generates LLM-assisted rules
    // instead of a mechanical Rename codemod.
    //
    // Example: splitButtonOptions: SplitButtonOptions → splitButtonItems: ReactNode[]
    // The codemod rename produces splitButtonItems={{ items: [...] }} which is wrong.
    {
        // First, build a lookup of replacement member types
        let mut new_member_types: HashMap<String, String> = HashMap::new();
        for changes in file_api_map.values() {
            for change in changes {
                if matches!(
                    change.change,
                    ApiChangeType::SignatureChanged | ApiChangeType::TypeChanged
                ) {
                    if let Some(ref after) = change.after {
                        new_member_types.insert(change.symbol.clone(), after.clone());
                    }
                }
            }
        }

        let mut reclassified = 0usize;
        for changes in file_api_map.values_mut() {
            for change in changes.iter_mut() {
                if change.change != ApiChangeType::Removed {
                    continue;
                }
                let new_member = match &change.removal_disposition {
                    Some(RemovalDisposition::ReplacedByMember { new_member }) => new_member.clone(),
                    _ => continue,
                };

                // Look up the replacement member's type
                let parent = change.symbol.rsplit_once('.').map(|(p, _)| p);
                let replacement_sym = parent
                    .map(|p| format!("{}.{}", p, new_member))
                    .unwrap_or_else(|| new_member.clone());

                let new_type_sig = match new_member_types.get(&replacement_sym) {
                    Some(sig) => sig.clone(),
                    None => continue,
                };

                let old_type = extract_type_from_signature(change.before.as_deref().unwrap_or(""));
                let new_type = extract_type_from_signature(&new_type_sig);

                if let (Some(old_t), Some(new_t)) = (old_type, new_type) {
                    if !types_structurally_compatible(old_t, new_t) {
                        tracing::info!(
                            symbol = %change.symbol,
                            old_type = old_t,
                            new_type = new_t,
                            new_member = %new_member,
                            "Reclassifying ReplacedByMember as SignatureChanged \
                             (incompatible types)"
                        );
                        change.change = ApiChangeType::SignatureChanged;
                        change.after = Some(new_type_sig.clone());
                        change.removal_disposition = None;
                        reclassified += 1;
                    }
                }
            }
        }
        if reclassified > 0 {
            tracing::info!(
                count = reclassified,
                "Reclassified ReplacedByMember props with incompatible types \
                 as SignatureChanged"
            );
        }
    }

    // ── Value rename detection for ReplacedByMember props ──────
    //
    // When a prop like `spaceItems` is removed and replaced by `gap`, the
    // old prop's union values (spaceItemsMd, spaceItemsNone) need to be
    // mapped to the new prop's values (gapMd, gapNone). We do this by
    // suffix matching: strip the old prop name prefix, match to a new
    // value with the same suffix. Emit Renamed changes so the fix engine
    // can replace both the prop name and values.
    {
        let mut value_renames: Vec<(PathBuf, ApiChange)> = Vec::new();

        for (file, changes) in file_api_map.iter() {
            for change in changes {
                if change.change != ApiChangeType::Removed {
                    continue;
                }
                let new_member = match &change.removal_disposition {
                    Some(RemovalDisposition::ReplacedByMember { new_member }) => new_member,
                    _ => continue,
                };

                let old_prop = change
                    .symbol
                    .rsplit_once('.')
                    .map(|(_, p)| p)
                    .unwrap_or(&change.symbol);

                // Extract old values from the removed prop's before type
                let old_values = change
                    .before
                    .as_deref()
                    .map(parse_union_string_values)
                    .unwrap_or_default();
                if old_values.is_empty() {
                    continue;
                }

                // Find the replacement member's change to get new values
                let parent = change.symbol.rsplit_once('.').map(|(p, _)| p);
                let replacement_sym = parent
                    .map(|p| format!("{}.{}", p, new_member))
                    .unwrap_or_else(|| new_member.clone());
                let new_values: BTreeSet<String> = changes
                    .iter()
                    .find(|c| c.symbol == replacement_sym)
                    .and_then(|c| c.after.as_deref())
                    .map(parse_union_string_values)
                    .unwrap_or_default();

                if new_values.is_empty() {
                    continue;
                }

                // Match old values to new by suffix
                let old_lower = old_prop.to_lowercase();
                for old_val in &old_values {
                    let val_lower = old_val.to_lowercase();
                    if let Some(suffix) = val_lower.strip_prefix(&old_lower) {
                        // Find a new value with the same suffix
                        let new_lower = new_member.to_lowercase();
                        let candidate = format!("{}{}", new_lower, suffix);
                        if let Some(new_val) =
                            new_values.iter().find(|v| v.to_lowercase() == candidate)
                        {
                            if old_val != new_val {
                                let parent_component = parent.unwrap_or("");
                                value_renames.push((
                                    file.clone(),
                                    ApiChange {
                                        symbol: format!(
                                            "{}.{} (value:{})",
                                            parent_component, old_prop, old_val
                                        ),
                                        kind: ApiChangeKind::Property,
                                        change: ApiChangeType::Renamed,
                                        before: Some(format!(
                                            "{}.{} = {}",
                                            parent_component, old_prop, old_val
                                        )),
                                        after: Some(format!(
                                            "{}.{} = {}",
                                            parent_component, new_member, new_val
                                        )),
                                        description: format!(
                                            "Prop value '{}' renamed to '{}' (prop '{}' → '{}')",
                                            old_val, new_val, old_prop, new_member
                                        ),
                                        migration_target: None,
                                        removal_disposition: None,
                                        renders_element: None,
                                    },
                                ));
                            }
                        }
                    }
                }
            }
        }

        if !value_renames.is_empty() {
            tracing::info!(
                count = value_renames.len(),
                "Detected prop value renames from ReplacedByMember dispositions"
            );
            for (file, change) in value_renames {
                file_api_map.entry(file).or_default().push(change);
            }
        }
    }

    // Sort changes within each file by symbol name
    for changes in file_api_map.values_mut() {
        changes.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    }

    let api_breaking: usize = file_api_map.values().map(|v| v.len()).sum();
    let behavioral_breaking = behavioral_changes.len();

    // Build hierarchical package/component view from surfaces.
    // Must be done before behavioral_changes is consumed by the file map loop.
    let packages = build_package_summaries(
        structural_changes,
        behavioral_changes,
        old_surface,
        new_surface,
        llm_api_changes,
    );

    // Merge behavioral changes into file map using the source_file
    // extracted from the BU pipeline's qualified names.
    let mut file_behavioral_map: BTreeMap<PathBuf, Vec<BehavioralChange<TypeScript>>> =
        BTreeMap::new();
    for bc in behavioral_changes {
        let file = if let Some(ref src) = bc.source_file {
            PathBuf::from(src)
        } else {
            PathBuf::from("(behavioral)")
        };
        file_behavioral_map
            .entry(file)
            .or_default()
            .push(bc.clone());
    }

    // Build the combined file changes list
    let mut all_files: BTreeSet<PathBuf> = BTreeSet::new();
    all_files.extend(file_api_map.keys().cloned());
    all_files.extend(file_behavioral_map.keys().cloned());

    let changes: Vec<FileChanges<TypeScript>> = all_files
        .into_iter()
        .map(|file| {
            let api_changes = file_api_map.remove(&file).unwrap_or_default();
            let behavioral = file_behavioral_map.remove(&file).unwrap_or_default();

            let status = if api_changes
                .iter()
                .all(|c| c.change == ApiChangeType::Removed)
                && behavioral.is_empty()
            {
                FileStatus::Deleted
            } else {
                FileStatus::Modified
            };
            FileChanges {
                file,
                status,
                renamed_from: None,
                breaking_api_changes: api_changes,
                breaking_behavioral_changes: behavioral,
                container_changes: vec![],
            }
        })
        .collect();

    let files_with_breaking = changes.len();

    // Get SHAs
    let from_sha = resolve_sha(repo, from_ref).unwrap_or_else(|| from_ref.to_string());
    let to_sha = resolve_sha(repo, to_ref).unwrap_or_else(|| to_ref.to_string());
    let commit_count = count_commits(repo, from_ref, to_ref).unwrap_or(0);

    let call_graph_info = if behavioral_breaking > 0 {
        "static_with_hof_heuristics"
    } else {
        "none (no behavioral analysis)"
    };

    // Collect files added between the two refs.
    let added_files = collect_added_files(repo, from_ref, to_ref);

    AnalysisReport {
        repository: repo.to_path_buf(),
        comparison: Comparison {
            from_ref: from_ref.to_string(),
            to_ref: to_ref.to_string(),
            from_sha,
            to_sha,
            commit_count,
            analysis_timestamp: chrono::Utc::now().to_rfc3339(),
        },
        summary: Summary {
            total_breaking_changes: api_breaking + behavioral_breaking,
            breaking_api_changes: api_breaking,
            breaking_behavioral_changes: behavioral_breaking,
            files_with_breaking_changes: files_with_breaking,
        },
        changes,
        manifest_changes: manifest_changes.to_vec(),
        added_files,
        packages,
        member_renames: HashMap::new(),
        inferred_rename_patterns,
        extensions: crate::TsAnalysisExtensions::default(),
        metadata: AnalysisMetadata {
            call_graph_analysis: call_graph_info.to_string(),
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            llm_usage: None,
        },
    }
}

// ─── Package/Component summary building ──────────────────────────────────

/// Build hierarchical package summaries from the API surfaces and change lists.
fn build_package_summaries(
    structural_changes: &[StructuralChange],
    behavioral_changes: &[BehavioralChange<TypeScript>],
    old_surface: &ApiSurface<TsSymbolData>,
    new_surface: &ApiSurface<TsSymbolData>,
    llm_api_changes: &[LlmApiChange],
) -> Vec<PackageChanges<TypeScript>> {
    if old_surface.symbols.is_empty() && new_surface.symbols.is_empty() {
        return Vec::new();
    }

    // ── Step 1: Resolve package names from qualified_name paths ──────
    // Build a directory-name -> npm-scoped-name mapping from the API surfaces.
    // Symbol.package carries the full scoped name (e.g., "@patternfly/react-core")
    // while qualified_name paths use bare directory names (e.g., "packages/react-core/...").
    let mut dir_to_npm: HashMap<String, String> = HashMap::new();
    for sym in old_surface.symbols.iter().chain(new_surface.symbols.iter()) {
        if let Some(ref npm_name) = sym.package {
            let path_str = sym.file.to_string_lossy();
            let parts: Vec<&str> = path_str.split('/').collect();
            if let Some(pkg_idx) = parts.iter().position(|&p| p == "packages") {
                if let Some(dir_name) = parts.get(pkg_idx + 1) {
                    dir_to_npm
                        .entry(dir_name.to_string())
                        .or_insert_with(|| npm_name.clone());
                }
            }
        }
    }

    let resolve_package = |qualified_name: &str| -> Option<String> {
        let parts: Vec<&str> = qualified_name.split('/').collect();
        if let Some(pkg_idx) = parts.iter().position(|&p| p == "packages") {
            if pkg_idx + 1 < parts.len() {
                let dir_name = parts[pkg_idx + 1];
                // Prefer the scoped npm name from Symbol.package if available
                if let Some(npm_name) = dir_to_npm.get(dir_name) {
                    return Some(npm_name.clone());
                }
                return Some(dir_name.to_string());
            }
        }
        if !parts.is_empty() && parts.len() > 1 {
            Some(parts[0].to_string())
        } else {
            None
        }
    };

    // ── Step 2: Index structural changes by parent qualified_name ────
    let mut changes_by_parent: HashMap<String, Vec<&StructuralChange>> = HashMap::new();
    let mut top_level_changes: Vec<&StructuralChange> = Vec::new();

    for change in structural_changes {
        let qn = &change.qualified_name;
        if let Some((parent_qn, _member)) = qn.rsplit_once('.') {
            if parent_qn.contains('.') {
                changes_by_parent
                    .entry(parent_qn.to_string())
                    .or_default()
                    .push(change);
            } else {
                top_level_changes.push(change);
            }
        } else {
            top_level_changes.push(change);
        }
    }

    // ── Step 3: Index old/new surface symbols by qualified_name ──────
    let _old_by_qn: HashMap<&str, &Symbol<TsSymbolData>> = old_surface
        .symbols
        .iter()
        .map(|s| (s.qualified_name.as_str(), s))
        .collect();

    let _new_by_qn: HashMap<&str, &Symbol<TsSymbolData>> = new_surface
        .symbols
        .iter()
        .map(|s| (s.qualified_name.as_str(), s))
        .collect();

    // ── Step 3b: Build LLM removal disposition lookup ─────────────────
    let llm_disposition_map: HashMap<&str, &LlmApiChange> = llm_api_changes
        .iter()
        .filter(|e| e.removal_disposition.is_some())
        .map(|e| (e.symbol.as_str(), e))
        .collect();

    // ── Step 4: Build component summaries ────────────────────────────
    let mut package_map: BTreeMap<String, PackageChanges<TypeScript>> = BTreeMap::new();

    for old_sym in &old_surface.symbols {
        if old_sym.members.is_empty() {
            continue;
        }
        match old_sym.kind {
            SymbolKind::Interface | SymbolKind::Class | SymbolKind::TypeAlias => {}
            _ => continue,
        }

        let pkg_name = match resolve_package(&old_sym.qualified_name) {
            Some(p) => p,
            None => continue,
        };

        let member_changes = changes_by_parent.get(&old_sym.qualified_name);

        let self_change = top_level_changes.iter().find(|c| {
            c.qualified_name == old_sym.qualified_name
                && (matches!(
                    c.change_type,
                    StructuralChangeType::Removed(ChangeSubject::Symbol { .. })
                ) || matches!(
                    c.change_type,
                    StructuralChangeType::Renamed {
                        from: ChangeSubject::Symbol { .. },
                        ..
                    }
                ))
        });

        if member_changes.is_none() && self_change.is_none() {
            continue;
        }

        let definition_name = &old_sym.name;
        let component_name = definition_name
            .strip_suffix("Props")
            .unwrap_or(definition_name)
            .to_string();

        let total_members = old_sym.members.len();

        let mut removed = 0usize;
        let mut renamed = 0usize;
        let mut type_changed = 0usize;
        let mut added = 0usize;
        let mut removed_members = Vec::new();
        let mut type_changes = Vec::new();

        if let Some(changes) = member_changes {
            for change in changes {
                match &change.change_type {
                    StructuralChangeType::Removed(ChangeSubject::Member { .. }) => {
                        removed += 1;
                        let lookup_key = format!("{}.{}", definition_name, change.symbol);
                        let disposition = llm_disposition_map
                            .get(lookup_key.as_str())
                            .and_then(|entry| entry.removal_disposition.clone());
                        removed_members.push(RemovedMember {
                            name: change.symbol.clone(),
                            old_type: change.before.clone(),
                            removal_disposition: disposition,
                        });
                    }
                    StructuralChangeType::Renamed {
                        from: ChangeSubject::Member { .. },
                        ..
                    } => {
                        renamed += 1;
                    }
                    StructuralChangeType::Changed(ChangeSubject::Parameter { .. })
                    | StructuralChangeType::Changed(ChangeSubject::ReturnType)
                    | StructuralChangeType::Removed(ChangeSubject::UnionValue { .. })
                    | StructuralChangeType::Added(ChangeSubject::UnionValue { .. }) => {
                        type_changed += 1;
                        type_changes.push(TypeChange {
                            property: change.symbol.clone(),
                            before: change.before.clone(),
                            after: change.after.clone(),
                        });
                    }
                    StructuralChangeType::Added(ChangeSubject::Member { .. }) => {
                        added += 1;
                    }
                    _ => {}
                }
            }
        }

        let removal_ratio = if total_members > 0 {
            removed as f64 / total_members as f64
        } else {
            0.0
        };

        let status = if self_change.is_some() {
            let component_still_exists = new_surface.symbols.iter().any(|s| {
                s.name == component_name
                    && matches!(
                        s.kind,
                        SymbolKind::Variable
                            | SymbolKind::Class
                            | SymbolKind::Function
                            | SymbolKind::Constant
                    )
            });
            if component_still_exists {
                ComponentStatus::Modified
            } else {
                ComponentStatus::Removed
            }
        } else if removal_ratio > 0.5 && removed >= 3 {
            ComponentStatus::Removed
        } else {
            ComponentStatus::Modified
        };

        let migration_target = self_change
            .and_then(|c| c.migration_target.clone())
            .or_else(|| {
                top_level_changes.iter().find_map(|c| {
                    if c.qualified_name == old_sym.qualified_name
                        && matches!(
                            c.change_type,
                            StructuralChangeType::Removed(ChangeSubject::Symbol { .. })
                        )
                        && c.migration_target.is_some()
                    {
                        c.migration_target.clone()
                    } else {
                        None
                    }
                })
            })
            // Fallback: inherit migration_target from the companion Props
            // interface. In React/TS, component symbols (Variable/Constant)
            // like `DropdownItem` are not processed by `detect_migrations`
            // (which only handles Interface/Class/Enum). But the companion
            // `DropdownItemProps` interface IS processed and may have a
            // migration_target pointing to the replacement in the main module.
            //
            // This links deprecated component removals to their main module
            // replacements without modifying the language-agnostic migration
            // detection engine.
            .or_else(|| {
                let props_name = format!("{}Props", component_name);
                top_level_changes.iter().find_map(|c| {
                    if c.symbol == props_name && c.migration_target.is_some() {
                        let props_target = c.migration_target.as_ref().unwrap();
                        // Adapt the Props migration target for the component:
                        // - replacement_symbol: strip "Props" suffix
                        // - removed/replacement names: use component name
                        let replacement_component = props_target
                            .replacement_symbol
                            .strip_suffix("Props")
                            .unwrap_or(&props_target.replacement_symbol)
                            .to_string();
                        Some(MigrationTarget {
                            removed_symbol: component_name.clone(),
                            removed_qualified_name: old_sym.qualified_name.clone(),
                            removed_package: props_target.removed_package.clone(),
                            replacement_symbol: replacement_component,
                            replacement_qualified_name: props_target
                                .replacement_qualified_name
                                .replace("Props", ""),
                            replacement_package: props_target.replacement_package.clone(),
                            matching_members: props_target.matching_members.clone(),
                            removed_only_members: props_target.removed_only_members.clone(),
                            overlap_ratio: props_target.overlap_ratio,
                            old_extends: props_target.old_extends.clone(),
                            new_extends: props_target.new_extends.clone(),
                        })
                    } else {
                        None
                    }
                })
            });

        let component_behavioral: Vec<BehavioralChange<TypeScript>> = behavioral_changes
            .iter()
            .filter(|bc| {
                bc.symbol == component_name
                    || bc.symbol == *definition_name
                    || bc.referenced_symbols.iter().any(|r| r == &component_name)
            })
            .map(|bc| BehavioralChange {
                symbol: bc.symbol.clone(),
                kind: bc.kind.clone(),
                category: bc.category.clone(),
                description: bc.description.clone(),
                source_file: bc.source_file.clone(),
                confidence: bc.confidence,
                evidence_type: bc.evidence_type.clone(),
                referenced_symbols: bc.referenced_symbols.clone(),
                is_internal_only: bc.is_internal_only,
            })
            .collect();

        let source_file = old_sym.qualified_name.split('.').next().map(PathBuf::from);

        let removed_member_names: Vec<&str> =
            removed_members.iter().map(|rp| rp.name.as_str()).collect();
        let child_components = discover_child_components(
            &component_name,
            &old_sym.qualified_name,
            old_surface,
            new_surface,
            structural_changes,
            behavioral_changes,
            &removed_member_names,
            &removed_members,
        );

        let summary = ComponentSummary {
            name: component_name.clone(),
            definition_name: definition_name.clone(),
            status,
            member_summary: MemberSummary {
                total: total_members,
                removed,
                renamed,
                type_changed,
                added,
                removal_ratio,
            },
            removed_members,
            type_changes,
            migration_target,
            behavioral_changes: component_behavioral,
            child_components,
            expected_children: Vec::new(),
            source_files: source_file.into_iter().collect(),
        };

        let pkg_entry = package_map
            .entry(pkg_name.clone())
            .or_insert_with(|| PackageChanges {
                name: pkg_name,
                old_version: None,
                new_version: None,
                type_summaries: Vec::new(),
                constants: Vec::new(),
                added_exports: Vec::new(),
            });
        pkg_entry.type_summaries.push(summary);
    }

    // ── Step 5: Build constant groups ────────────────────────────────
    //
    // Exclude symbols that have a migration_target (either directly or via
    // their companion Props interface). These get their own per-component
    // rules with specific migration guidance instead of being lumped into
    // the bulk "N constants removed" group.
    let symbols_with_migration: HashSet<String> = {
        // Collect symbol names that have migration_target directly
        let mut set: HashSet<String> = structural_changes
            .iter()
            .filter(|c| c.migration_target.is_some())
            .map(|c| c.symbol.clone())
            .collect();

        // Also collect component names whose companion Props has a migration_target.
        // e.g., if DropdownItemProps has migration_target, add "DropdownItem".
        let props_with_migration: Vec<String> = structural_changes
            .iter()
            .filter(|c| {
                c.migration_target.is_some()
                    && c.symbol.ends_with("Props")
                    && matches!(c.kind, SymbolKind::Interface)
            })
            .filter_map(|c| c.symbol.strip_suffix("Props").map(|s| s.to_string()))
            .collect();
        set.extend(props_with_migration);
        set
    };

    let mut constant_groups: HashMap<(String, ApiChangeType), Vec<String>> = HashMap::new();

    for change in structural_changes {
        if !change.is_breaking {
            continue;
        }
        if change.kind != SymbolKind::Constant && change.kind != SymbolKind::Variable {
            continue;
        }
        // Skip symbols that have migration guidance — they get per-component rules.
        if symbols_with_migration.contains(&change.symbol) {
            continue;
        }
        let after_file = change
            .qualified_name
            .rsplit('/')
            .next()
            .unwrap_or(&change.qualified_name);
        let dot_count = after_file.chars().filter(|c| *c == '.').count();
        if dot_count > 1 {
            continue;
        }

        let pkg_name = match resolve_package(&change.qualified_name) {
            Some(p) => p,
            None => continue,
        };

        let api_change_type = change.change_type.to_api_change_type();
        constant_groups
            .entry((pkg_name, api_change_type))
            .or_default()
            .push(change.symbol.clone());
    }

    let constant_collapse_threshold = 10;
    for ((pkg_name, change_type), symbols) in &constant_groups {
        if symbols.len() < constant_collapse_threshold {
            continue;
        }

        let prefix_pattern = build_constant_prefix_pattern(symbols);
        let strategy_hint = if symbols
            .iter()
            .any(|s| s.starts_with("c_") || s.starts_with("global_") || s.starts_with("chart_"))
        {
            "CssVariablePrefix".to_string()
        } else {
            "ConstantGroup".to_string()
        };

        let group = ConstantGroup {
            change_type: change_type.clone(),
            count: symbols.len(),
            symbols: symbols.clone(),
            common_prefix_pattern: prefix_pattern,
            strategy_hint,
            suffix_renames: Vec::new(),
        };

        let pkg_entry = package_map
            .entry(pkg_name.clone())
            .or_insert_with(|| PackageChanges {
                name: pkg_name.clone(),
                old_version: None,
                new_version: None,
                type_summaries: Vec::new(),
                constants: Vec::new(),
                added_exports: Vec::new(),
            });
        pkg_entry.constants.push(group);
    }

    // ── Step 6: Discover added components ────────────────────────────
    let old_qnames: HashSet<&str> = old_surface
        .symbols
        .iter()
        .map(|s| s.qualified_name.as_str())
        .collect();

    for new_sym in &new_surface.symbols {
        if old_qnames.contains(new_sym.qualified_name.as_str()) {
            continue;
        }
        match new_sym.kind {
            SymbolKind::Interface | SymbolKind::Class | SymbolKind::Function => {}
            _ => continue,
        }
        if !is_pascal_case(&new_sym.name) {
            continue;
        }
        if new_sym.name.ends_with("Props") || new_sym.name.ends_with("Variants") {
            continue;
        }

        let pkg_name = match resolve_package(&new_sym.qualified_name) {
            Some(p) => p,
            None => continue,
        };

        let added = AddedExport {
            name: new_sym.name.clone(),
            qualified_name: new_sym.qualified_name.clone(),
            package: pkg_name.clone(),
        };

        let pkg_entry = package_map
            .entry(pkg_name.clone())
            .or_insert_with(|| PackageChanges {
                name: pkg_name,
                old_version: None,
                new_version: None,
                type_summaries: Vec::new(),
                constants: Vec::new(),
                added_exports: Vec::new(),
            });
        pkg_entry.added_exports.push(added);
    }

    package_map.into_values().collect()
}

// ─── Child component discovery ───────────────────────────────────────────

/// Discover child/sibling components for a given parent component.
#[allow(clippy::too_many_arguments)]
fn discover_child_components(
    component_name: &str,
    parent_qn: &str,
    old_surface: &ApiSurface<TsSymbolData>,
    new_surface: &ApiSurface<TsSymbolData>,
    structural_changes: &[StructuralChange],
    _behavioral_changes: &[BehavioralChange<TypeScript>],
    removed_member_names: &[&str],
    removed_members: &[RemovedMember],
) -> Vec<ChildComponent> {
    let parent_dir = parent_qn.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    if parent_dir.is_empty() {
        return Vec::new();
    }

    let component_dir = parent_dir.rsplit('/').next().unwrap_or("");

    let old_qnames: HashSet<&str> = old_surface
        .symbols
        .iter()
        .map(|s| s.qualified_name.as_str())
        .collect();

    let removed_set: HashSet<&str> = removed_member_names.iter().copied().collect();

    let mut children_map: BTreeMap<String, ChildComponent> = BTreeMap::new();

    for sym in &new_surface.symbols {
        let name = &sym.name;
        if !name.starts_with(component_name) || name == component_name {
            continue;
        }
        if !is_child_component_candidate(name, component_name) {
            continue;
        }

        match sym.kind {
            SymbolKind::Variable
            | SymbolKind::Class
            | SymbolKind::Function
            | SymbolKind::Constant => {}
            _ => continue,
        }
        if children_map.contains_key(name) {
            continue;
        }

        let sym_dir = sym
            .qualified_name
            .rsplit_once('/')
            .map(|(dir, _)| dir)
            .unwrap_or("");
        let in_family = sym_dir.ends_with(&format!("/{}", component_dir)) || sym_dir == parent_dir;
        if !in_family {
            continue;
        }

        if sym.qualified_name.contains("/deprecated/") {
            continue;
        }

        let is_new = !old_qnames.contains(sym.qualified_name.as_str());
        let is_promoted = structural_changes.iter().any(|c| {
            matches!(
                c.change_type,
                StructuralChangeType::Renamed {
                    from: ChangeSubject::Symbol { .. },
                    ..
                }
            ) && c.symbol == *name
                && c.after
                    .as_ref()
                    .map(|a| a.contains(component_dir))
                    .unwrap_or(false)
        });

        if !is_new && !is_promoted {
            continue;
        }

        let known_members: Vec<String> = sym.members.iter().map(|m| m.name.clone()).collect();

        let props_iface_name = format!("{}Props", name);
        let props_members: Vec<String> = new_surface
            .symbols
            .iter()
            .find(|s| s.name == props_iface_name && s.qualified_name.contains(component_dir))
            .map(|s| s.members.iter().map(|m| m.name.clone()).collect())
            .unwrap_or_default();

        let mut all_members: HashSet<String> = known_members.into_iter().collect();
        all_members.extend(props_members);
        let all_members_sorted: Vec<String> = {
            let mut v: Vec<String> = all_members.into_iter().collect();
            v.sort();
            v
        };

        let absorbed: Vec<String> = all_members_sorted
            .iter()
            .filter(|p| removed_set.contains(p.as_str()))
            .cloned()
            .collect();

        children_map.insert(
            name.clone(),
            ChildComponent {
                name: name.clone(),
                status: if is_promoted {
                    ChildComponentStatus::Modified
                } else {
                    ChildComponentStatus::Added
                },
                known_members: all_members_sorted,
                absorbed_members: absorbed,
            },
        );
    }

    // ── Enrichment pass: LLM removal_disposition ──
    for rp in removed_members {
        if let Some(RemovalDisposition::MovedToRelatedType {
            target_type,
            mechanism,
        }) = &rp.removal_disposition
        {
            if let Some(child) = children_map.get_mut(target_type) {
                if !child.absorbed_members.contains(&rp.name) {
                    child.absorbed_members.push(rp.name.clone());
                    child.absorbed_members.sort();
                }
            } else {
                children_map.insert(
                    target_type.clone(),
                    ChildComponent {
                        name: target_type.clone(),
                        status: ChildComponentStatus::Added,
                        known_members: if mechanism == "children" {
                            vec!["children".to_string()]
                        } else {
                            vec![rp.name.clone()]
                        },
                        absorbed_members: vec![rp.name.clone()],
                    },
                );
            }
        }
    }

    children_map.into_values().collect()
}

// ─── Slot Prop Inference ────────────────────────────────────────────────

/// Check if a type string represents a "slot" prop — one that accepts a
/// React component instance (ReactElement, ReactNode, JSX.Element).
fn is_slot_prop_type(type_str: &str) -> bool {
    let t = type_str.trim();
    // Match common slot types, including union variants like
    // "ReactNode | undefined" or "ReactElement<any>"
    t.contains("ReactElement")
        || t.contains("ReactNode")
        || t.contains("JSX.Element")
        || t.contains("Element")
}

/// Try to infer which prop on the parent carries a given child component.
///
/// Strategy: strip the parent name prefix from the child name, lowercase
/// the first character, and check if a slot prop with that name exists.
///
/// Examples:
///   parent="FormGroup", child="FormGroupLabelHelp"
///     → strip "FormGroup" → "LabelHelp" → "labelHelp" → check prop types
///     → FormGroupProps.labelHelp: ReactElement<any> → match!
///   parent="Modal", child="ModalHeader"
///     → strip "Modal" → "Header" → "header" → check prop types
///     → v6 ModalProps has NO "header" prop (it was removed) → no match
///     → ModalHeader stays as mechanism="child" (correct — direct child in v6)
fn infer_prop_name_for_child(
    parent_name: &str,
    child_name: &str,
    prop_types: &HashMap<String, String>,
) -> Option<String> {
    // Strip parent prefix from child name
    let suffix = child_name.strip_prefix(parent_name)?;
    if suffix.is_empty() {
        return None;
    }

    // Lowercase first character: "LabelHelp" → "labelHelp"
    let mut chars = suffix.chars();
    let first = chars.next()?;
    let candidate = format!("{}{}", first.to_lowercase(), chars.as_str());

    // Check if this prop exists and has a slot type
    if let Some(type_str) = prop_types.get(&candidate) {
        if is_slot_prop_type(type_str) {
            return Some(candidate);
        }
    }

    None
}

// ─── Hierarchy Delta Enrichment ──────────────────────────────────────────

/// Enrich hierarchy deltas with prop migration data and populate
/// `expected_children` on each `ComponentSummary`.
fn enrich_hierarchy_deltas(
    report: &mut AnalysisReport<TypeScript>,
    mut deltas: Vec<HierarchyDelta>,
    new_surface: &ApiSurface<TsSymbolData>,
    new_hierarchies: &HashMap<String, HashMap<String, Vec<ExpectedChild>>>,
) {
    // Build a lookup of component name → props from the new surface.
    // Each prop maps to its type string (from signature.return_type) for
    // deterministic prop_name inference on slot props.
    let mut component_props: HashMap<String, HashSet<String>> = HashMap::new();
    let mut component_prop_types: HashMap<String, HashMap<String, String>> = HashMap::new();

    // Skip symbols from deprecated/next subpaths — these represent the OLD
    // API surface re-exported in v6 for backward compatibility. The prop_name
    // inference and component_props lookup should only use the MAIN module
    // (the migration target), not the deprecated module (the migration source).
    // Without this filter, deprecated ModalProps (with header/footer props)
    // would merge with main ModalProps (without them), causing the inference
    // to incorrectly classify ModalHeader as prop-passed.
    for sym in &new_surface.symbols {
        let file_str = sym.file.to_string_lossy();
        if file_str.contains("/deprecated/") || file_str.contains("/next/") {
            continue;
        }

        if matches!(sym.kind, SymbolKind::Interface | SymbolKind::TypeAlias) {
            if let Some(comp_name) = sym.name.strip_suffix("Props") {
                let props: HashSet<String> = sym.members.iter().map(|m| m.name.clone()).collect();
                component_props
                    .entry(comp_name.to_string())
                    .or_default()
                    .extend(props);

                // Collect type info for each member
                let types = component_prop_types
                    .entry(comp_name.to_string())
                    .or_default();
                for m in &sym.members {
                    if let Some(sig) = &m.signature {
                        if let Some(rt) = &sig.return_type {
                            types.insert(m.name.clone(), rt.clone());
                        }
                    }
                }
            }
        }

        if matches!(
            sym.kind,
            SymbolKind::Variable | SymbolKind::Function | SymbolKind::Class | SymbolKind::Constant
        ) && sym
            .name
            .chars()
            .next()
            .map(|c| c.is_uppercase())
            .unwrap_or(false)
        {
            let props: HashSet<String> = sym.members.iter().map(|m| m.name.clone()).collect();
            if !props.is_empty() {
                component_props
                    .entry(sym.name.clone())
                    .or_default()
                    .extend(props);

                let types = component_prop_types.entry(sym.name.clone()).or_default();
                for m in &sym.members {
                    if let Some(sig) = &m.signature {
                        if let Some(rt) = &sig.return_type {
                            types.insert(m.name.clone(), rt.clone());
                        }
                    }
                }
            }
        }
    }

    // Enrich each delta with migrated members
    for delta in &mut deltas {
        let removed_member_names: Vec<String> = report
            .packages
            .iter()
            .flat_map(|pkg| &pkg.type_summaries)
            .find(|c| c.name == delta.component)
            .map(|c| c.removed_members.iter().map(|rp| rp.name.clone()).collect())
            .unwrap_or_default();

        if removed_member_names.is_empty() {
            continue;
        }

        for child in &delta.added_children {
            let child_props = match component_props.get(&child.name) {
                Some(props) => props,
                None => continue,
            };

            for removed_member in &removed_member_names {
                if child_props.contains(removed_member) {
                    delta.migrated_members.push(MigratedMember {
                        member_name: removed_member.clone(),
                        target_child: child.name.clone(),
                        target_member_name: None,
                    });
                }
            }
        }
    }

    // Populate expected_children on ComponentSummary entries from the FULL
    // new-version hierarchy.
    for (family, family_hierarchy) in new_hierarchies {
        for (comp_name, children) in family_hierarchy {
            if children.is_empty() {
                continue;
            }

            let expected: Vec<ExpectedChild> = children.clone();

            let mut found = false;
            for pkg in &mut report.packages {
                for comp in &mut pkg.type_summaries {
                    if comp.name == *comp_name {
                        found = true;
                        for ec in &expected {
                            if !comp.expected_children.iter().any(|e| e.name == ec.name) {
                                comp.expected_children.push(ec.clone());
                            }
                        }
                    }
                }
            }

            if !found {
                let target_idx = report
                    .packages
                    .iter()
                    .position(|p| p.type_summaries.iter().any(|c| c.name.starts_with(family)))
                    .or_else(|| {
                        report
                            .packages
                            .iter()
                            .position(|p| !p.type_summaries.is_empty())
                    });

                if let Some(idx) = target_idx {
                    report.packages[idx].type_summaries.push(ComponentSummary {
                        name: comp_name.clone(),
                        definition_name: format!("{}Props", comp_name),
                        status: ComponentStatus::Modified,
                        member_summary: MemberSummary::default(),
                        removed_members: vec![],
                        type_changes: vec![],
                        migration_target: None,
                        behavioral_changes: vec![],
                        child_components: vec![],
                        expected_children: expected,
                        source_files: vec![],
                    });
                }
            }
        }
    }

    // ── Extends-based fallback for empty expected_children ─────────
    {
        let mut extends_map: HashMap<String, String> = HashMap::new();
        for sym in &new_surface.symbols {
            if matches!(sym.kind, SymbolKind::Interface | SymbolKind::TypeAlias) {
                if let Some(ref parent) = sym.extends {
                    let base = if parent.starts_with("Omit<")
                        || parent.starts_with("Pick<")
                        || parent.starts_with("Partial<")
                        || parent.starts_with("Required<")
                    {
                        parent
                            .split('<')
                            .nth(1)
                            .and_then(|s| s.split(&[',', '>'][..]).next())
                            .map(|s| s.trim().to_string())
                            .unwrap_or_else(|| parent.clone())
                    } else {
                        parent.clone()
                    };
                    extends_map.insert(sym.name.clone(), base);
                }
            }
        }

        let mut comp_children: HashMap<String, Vec<ExpectedChild>> = HashMap::new();
        for pkg in &report.packages {
            for comp in &pkg.type_summaries {
                if !comp.expected_children.is_empty() {
                    comp_children.insert(comp.name.clone(), comp.expected_children.clone());
                }
            }
        }

        let mut interface_to_component: HashMap<String, String> = HashMap::new();
        for pkg in &report.packages {
            for comp in &pkg.type_summaries {
                interface_to_component.insert(comp.definition_name.clone(), comp.name.clone());
            }
        }

        let mut inferred_count = 0;
        let mut inferred: Vec<(String, Vec<ExpectedChild>)> = Vec::new();

        for pkg in &report.packages {
            for comp in &pkg.type_summaries {
                if !comp.expected_children.is_empty() {
                    continue;
                }

                let base_interface = match extends_map.get(&comp.definition_name) {
                    Some(b) => b,
                    None => continue,
                };

                let base_component = match interface_to_component.get(base_interface) {
                    Some(c) => c,
                    None => continue,
                };

                let base_children = match comp_children.get(base_component) {
                    Some(c) if !c.is_empty() => c,
                    _ => continue,
                };

                let prefix = &comp.name;
                let mut mapped_children: Vec<ExpectedChild> = Vec::new();

                let mut base_queue: Vec<&ExpectedChild> = base_children.iter().collect();
                let mut seen_bases: HashSet<String> = HashSet::new();

                while let Some(base_child) = base_queue.pop() {
                    if !seen_bases.insert(base_child.name.clone()) {
                        continue;
                    }

                    let base_child_interface = format!("{}Props", base_child.name);

                    let wrapper = extends_map.iter().find(|(iface, parent)| {
                        *parent == &base_child_interface && iface.starts_with(prefix)
                    });

                    if let Some((wrapper_iface, _)) = wrapper {
                        let wrapper_name =
                            wrapper_iface.strip_suffix("Props").unwrap_or(wrapper_iface);
                        mapped_children.push(ExpectedChild {
                            name: wrapper_name.to_string(),
                            required: base_child.required,
                            mechanism: base_child.mechanism.clone(),
                            prop_name: base_child.prop_name.clone(),
                        });
                    } else if let Some(sub_children) = comp_children.get(&base_child.name) {
                        for sub in sub_children {
                            base_queue.push(sub);
                        }
                    }
                }

                if !mapped_children.is_empty() {
                    tracing::debug!(
                        component = %comp.name,
                        base = %base_component,
                        children = ?mapped_children.iter().map(|c| &c.name).collect::<Vec<_>>(),
                        "Inferred expected_children from extends chain"
                    );
                    inferred.push((comp.name.clone(), mapped_children));
                    inferred_count += 1;
                }
            }
        }

        // Apply inferred children
        for (comp_name, children) in inferred {
            for pkg in &mut report.packages {
                for comp in &mut pkg.type_summaries {
                    if comp.name == comp_name {
                        for ec in &children {
                            if !comp.expected_children.iter().any(|e| e.name == ec.name) {
                                comp.expected_children.push(ec.clone());
                            }
                        }
                    }
                }
            }
        }

        if inferred_count > 0 {
            tracing::debug!(
                count = inferred_count,
                "Inferred expected_children via extends chain fallback"
            );
        }
    }

    // ── Deterministic prop_name inference for slot props ─────────
    //
    // For expected children with mechanism="prop" but no prop_name, or
    // children the LLM classified as "child" that are actually prop-passed,
    // infer the prop name from the parent's Props interface.
    //
    // A "slot prop" is a member of type ReactElement, ReactNode, or
    // JSX.Element — these accept a component instance as a value.
    // If the prop name matches the child component name (after stripping
    // the parent prefix), we can deterministically set mechanism="prop"
    // and prop_name.
    {
        let mut corrections = 0;

        for pkg in &mut report.packages {
            for comp in &mut pkg.type_summaries {
                if comp.expected_children.is_empty() {
                    continue;
                }

                let parent_name = &comp.name;
                let prop_types = match component_prop_types.get(parent_name.as_str()) {
                    Some(t) => t,
                    None => continue,
                };

                // Log available slot props for debugging
                let slot_props: Vec<(&String, &String)> = prop_types
                    .iter()
                    .filter(|(_, ty)| is_slot_prop_type(ty))
                    .collect();
                if !slot_props.is_empty() {
                    tracing::debug!(
                        parent = %parent_name,
                        slot_props = ?slot_props.iter().map(|(n, t)| format!("{}: {}", n, t)).collect::<Vec<_>>(),
                        "Slot props available for prop_name inference"
                    );
                }

                for child in &mut comp.expected_children {
                    // Skip if prop_name already set
                    if child.prop_name.is_some() {
                        continue;
                    }

                    // Try to match the child name to a slot prop on the parent
                    if let Some(prop_name) =
                        infer_prop_name_for_child(parent_name, &child.name, prop_types)
                    {
                        tracing::debug!(
                            parent = %parent_name,
                            child = %child.name,
                            prop_name = %prop_name,
                            old_mechanism = %child.mechanism,
                            "Inferred prop_name from Props interface"
                        );
                        child.mechanism = "prop".to_string();
                        child.prop_name = Some(prop_name);
                        corrections += 1;
                    }
                }
            }
        }

        // Also fix hierarchy deltas
        for delta in &mut deltas {
            let parent_name = &delta.component;
            let prop_types = match component_prop_types.get(parent_name.as_str()) {
                Some(t) => t,
                None => continue,
            };

            for child in &mut delta.added_children {
                if child.prop_name.is_some() {
                    continue;
                }
                if let Some(prop_name) =
                    infer_prop_name_for_child(parent_name, &child.name, prop_types)
                {
                    child.mechanism = "prop".to_string();
                    child.prop_name = Some(prop_name);
                }
            }
        }

        if corrections > 0 {
            tracing::info!(
                corrections,
                "Inferred prop_name for expected_children from Props interface types"
            );
        }
    }

    // ── Deprecated→main migration deltas ────────────────────────
    //
    // For component families that were removed from the deprecated module
    // and have replacements in the main module, create hierarchy deltas
    // that guide the migration. These deltas have `source_package` set
    // to the deprecated import path and include:
    // - The new composition structure (from main module expected_children)
    // - Prop mapping (from migration_target on the companion Props interface)
    // - Removed symbols with no replacement
    {
        // Collect deprecated components that have migration_target
        let mut deprecated_families: HashMap<String, Vec<&ComponentSummary<TypeScript>>> =
            HashMap::new();

        for pkg in &report.packages {
            for comp in &pkg.type_summaries {
                if comp.migration_target.is_none() {
                    continue;
                }
                let mt = comp.migration_target.as_ref().unwrap();
                // Only deprecated→main: the removed qualified_name must contain /deprecated/
                if !mt.removed_qualified_name.contains("/deprecated/") {
                    continue;
                }
                // Group by family: derive from qualified_name directory
                let family = mt
                    .removed_qualified_name
                    .rsplit('/')
                    .nth(1) // Get the parent directory name (e.g., "Dropdown")
                    .unwrap_or(&comp.name)
                    .to_string();

                deprecated_families.entry(family).or_default().push(comp);
            }
        }

        for (family, deprecated_comps) in &deprecated_families {
            // Find the ComponentSummary that has expected_children for
            // the new composition structure. This is the main module's entry
            // (or the sole entry when deprecated inherits expected_children).
            let main_comp = report
                .packages
                .iter()
                .flat_map(|pkg| &pkg.type_summaries)
                .find(|c| c.name == *family && !c.expected_children.is_empty());

            // Build added_children from the main module's expected_children
            let new_children: Vec<ExpectedChild> = main_comp
                .map(|mc| mc.expected_children.clone())
                .unwrap_or_default();

            // Find the best migration_target (highest overlap) for the delta
            let best_mt = deprecated_comps
                .iter()
                .filter_map(|c| c.migration_target.as_ref())
                .max_by(|a, b| {
                    a.overlap_ratio
                        .partial_cmp(&b.overlap_ratio)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .cloned();

            // Build migrated_members from all Props migration_targets in the family
            let mut migrated: Vec<MigratedMember> = Vec::new();
            for comp in deprecated_comps {
                if let Some(ref mt) = comp.migration_target {
                    let target_name = mt
                        .replacement_symbol
                        .strip_suffix("Props")
                        .unwrap_or(&mt.replacement_symbol);
                    for mm in &mt.matching_members {
                        migrated.push(MigratedMember {
                            member_name: mm.old_name.clone(),
                            target_child: target_name.to_string(),
                            target_member_name: if mm.old_name != mm.new_name {
                                Some(mm.new_name.clone())
                            } else {
                                None
                            },
                        });
                    }
                }
            }

            // Collect removed symbols with no replacement from the deprecated
            // family's file changes. These are the old components (DropdownToggle,
            // KebabToggle, DropdownSeparator, etc.) that don't exist in the new API.
            let deprecated_dir = format!("/deprecated/components/{}/", family);
            let mut removed_no_replacement: Vec<String> = Vec::new();
            for fc in &report.changes {
                let file_str = fc.file.to_string_lossy();
                if !file_str.contains(&deprecated_dir) {
                    continue;
                }
                for api in &fc.breaking_api_changes {
                    if api.change != ApiChangeType::Removed {
                        continue;
                    }
                    if api.migration_target.is_some() {
                        continue;
                    }
                    // Only component-level removals (not Properties)
                    if api.symbol.contains('.') {
                        continue;
                    }
                    // Skip Props interfaces — we only want component names
                    if api.symbol.ends_with("Props") {
                        continue;
                    }
                    removed_no_replacement.push(api.symbol.clone());
                }
            }
            removed_no_replacement.sort();
            removed_no_replacement.dedup();

            // Filter out symbols that exist in the new API composition tree.
            // e.g., DropdownItem appears in DropdownList's expected_children,
            // so it's NOT "removed with no replacement" — it's migrated to
            // the new composition structure.
            {
                let mut new_api_names: HashSet<String> = HashSet::new();
                // Collect names from all expected_children recursively
                let mut queue: Vec<String> = new_children.iter().map(|c| c.name.clone()).collect();
                while let Some(name) = queue.pop() {
                    if !new_api_names.insert(name.clone()) {
                        continue; // Already visited — prevent cycles
                    }
                    // Look up this component's expected_children
                    for pkg in report.packages.iter() {
                        for comp in &pkg.type_summaries {
                            if comp.name == name {
                                for ec in &comp.expected_children {
                                    queue.push(ec.name.clone());
                                }
                            }
                        }
                    }
                }
                // Also include the family component itself
                new_api_names.insert(family.clone());

                removed_no_replacement.retain(|name| !new_api_names.contains(name));
            }

            // Derive the deprecated import path
            let deprecated_pkg = best_mt
                .as_ref()
                .and_then(|mt| mt.removed_package.as_ref())
                .map(|pkg| format!("{}/deprecated", pkg))
                .unwrap_or_else(|| "@patternfly/react-core/deprecated".to_string());

            if !new_children.is_empty() || !removed_no_replacement.is_empty() {
                tracing::info!(
                    family = %family,
                    new_children = new_children.len(),
                    migrated_props = migrated.len(),
                    removed_symbols = removed_no_replacement.len(),
                    deprecated_pkg = %deprecated_pkg,
                    "created deprecated→main hierarchy delta"
                );

                deltas.push(HierarchyDelta {
                    component: family.clone(),
                    added_children: new_children,
                    removed_children: removed_no_replacement,
                    migrated_members: migrated,
                    source_package: Some(deprecated_pkg),
                    migration_target: best_mt,
                });
            }
        }
    }

    // Store deltas on the report
    report.extensions.hierarchy_deltas = deltas;

    // ── BEM CSS fallback for expected_children ─────────────────────
    //
    // For components that still have empty expected_children after all
    // other signals, use BEM CSS class analysis as a fallback.
    //
    // If component A uses `styles.fooBar` (BEM block) and component B
    // uses `styles.fooBarItem` (BEM element = block + PascalCase suffix),
    // B is structurally a child of A in the CSS layout.
    //
    // This catches cases like InputGroup/InputGroupItem where the types
    // don't express the parent/child requirement but the CSS does.
    {
        // Build a map of css token → component name from all symbols
        let mut token_to_component: HashMap<String, String> = HashMap::new();
        for sym in &new_surface.symbols {
            if sym.language_data.css.is_empty() {
                continue;
            }
            // Use the first non-modifier token as the component's primary CSS token
            for token in &sym.language_data.css {
                token_to_component
                    .entry(token.clone())
                    .or_insert_with(|| sym.name.clone());
            }
        }

        // For each component with empty expected_children, check if other
        // components use BEM element tokens derived from this component's
        // block token.
        let mut bem_additions = 0usize;
        for pkg in &mut report.packages {
            for comp in &mut pkg.type_summaries {
                if !comp.expected_children.is_empty() {
                    continue;
                }

                // Find this component's primary CSS block token from the surface
                let comp_sym = new_surface
                    .symbols
                    .iter()
                    .find(|s| s.name == comp.name && !s.language_data.css.is_empty());
                let block_token = match comp_sym {
                    Some(sym) => {
                        // The block token is the one that matches just the block
                        // (no BEM element suffix). For InputGroup, that's "inputGroup".
                        sym.language_data.css.first().cloned()
                    }
                    None => continue,
                };
                let block_token = match block_token {
                    Some(t) => t,
                    None => continue,
                };

                // Find other components whose CSS tokens start with this block
                // token followed by an uppercase letter (BEM element convention).
                // e.g., block="inputGroup" matches "inputGroupItem", "inputGroupText"
                let mut children: Vec<ExpectedChild> = Vec::new();
                for (token, child_name) in &token_to_component {
                    if token == &block_token {
                        continue; // same component
                    }
                    if child_name == &comp.name {
                        continue; // same component, different token
                    }
                    if token.starts_with(&block_token)
                        && token[block_token.len()..].starts_with(|c: char| c.is_ascii_uppercase())
                    {
                        // This is a BEM element of our block
                        if !children.iter().any(|c| c.name == *child_name) {
                            children.push(ExpectedChild::new(child_name, true));
                        }
                    }
                }

                if !children.is_empty() {
                    tracing::debug!(
                        parent = %comp.name,
                        children = ?children.iter().map(|c| &c.name).collect::<Vec<_>>(),
                        block_token = %block_token,
                        "BEM CSS fallback: inferred expected_children"
                    );
                    comp.expected_children = children;
                    bem_additions += 1;
                }
            }
        }

        if bem_additions > 0 {
            tracing::info!(
                count = bem_additions,
                "Inferred expected_children from BEM CSS class analysis"
            );
        }
    }

    let total_migrated: usize = report
        .extensions
        .hierarchy_deltas
        .iter()
        .map(|d| d.migrated_members.len())
        .sum();
    tracing::debug!(
        deltas = report.extensions.hierarchy_deltas.len(),
        migrated_members = total_migrated,
        "Enriched hierarchy deltas and populated expected_children"
    );
}

// ─── Helper functions ────────────────────────────────────────────────────

/// Check if a symbol name is a plausible child component of a parent component.
fn is_child_component_candidate(name: &str, parent_name: &str) -> bool {
    if !is_pascal_case(name) {
        return false;
    }
    if name == parent_name {
        return false;
    }
    if name.ends_with("Props") {
        return false;
    }
    true
}

/// Check if a name is PascalCase (starts with uppercase, has at least one lowercase).
fn is_pascal_case(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => chars.any(|c| c.is_ascii_lowercase()),
        _ => false,
    }
}

/// Extract suffix renames from member_renames.
pub fn extract_suffix_renames(member_renames: &HashMap<String, String>) -> Vec<SuffixRename> {
    let mut suffix_map: BTreeMap<String, String> = BTreeMap::new();
    for (old_name, new_name) in member_renames {
        let old_suffix = extract_trailing_suffix(old_name);
        let new_suffix = extract_trailing_suffix(new_name);
        if let (Some(old_s), Some(new_s)) = (old_suffix, new_suffix) {
            if old_s != new_s {
                suffix_map
                    .entry(old_s.to_string())
                    .or_insert_with(|| new_s.to_string());
            }
        }
    }
    suffix_map
        .into_iter()
        .map(|(from, to)| SuffixRename { from, to })
        .collect()
}

/// Extract the trailing PascalCase suffix from a token name.
fn extract_trailing_suffix(name: &str) -> Option<&str> {
    let last_underscore = name.rfind('_')?;
    let suffix = &name[last_underscore + 1..];
    if !suffix.is_empty()
        && suffix
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
        && suffix.chars().any(|c| c.is_ascii_lowercase())
        && !suffix.contains('_')
    {
        Some(suffix)
    } else {
        None
    }
}

/// Build a regex prefix pattern from a list of constant symbol names.
fn build_constant_prefix_pattern(symbols: &[String]) -> String {
    let mut prefixes = BTreeSet::new();
    for name in symbols {
        if let Some(idx) = name.find('_') {
            prefixes.insert(&name[..=idx]);
        }
    }
    if prefixes.is_empty() || prefixes.len() > 20 {
        return ".*".to_string();
    }
    let alts: Vec<&str> = prefixes.into_iter().collect();
    if alts.len() == 1 {
        format!("^{}\\w+$", alts[0])
    } else {
        format!("^({})\\w+$", alts.join("|"))
    }
}

/// Collect files added between two git refs.
fn collect_added_files(repo: &Path, from_ref: &str, to_ref: &str) -> Vec<PathBuf> {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            &repo.to_string_lossy(),
            "diff",
            "--name-status",
            "--diff-filter=A",
            &format!("{}..{}", from_ref, to_ref),
            "--",
            "*.ts",
            "*.tsx",
        ])
        .output();

    match output {
        Ok(out) => {
            let files: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|line| {
                    let path = line.strip_prefix("A\t")?;
                    if path.contains("/test")
                        || path.contains("/__tests__")
                        || path.contains("/examples/")
                        || path.contains("/stories/")
                        || path.contains("/demo/")
                        || path.contains("/dist/")
                        || path.starts_with("dist/")
                        || path.contains(".test.")
                        || path.contains(".spec.")
                    {
                        return None;
                    }
                    Some(PathBuf::from(path))
                })
                .collect();
            if !files.is_empty() {
                tracing::debug!(count = files.len(), "Found added source files between refs");
            }
            files
        }
        Err(e) => {
            tracing::warn!(error = %e, "Could not enumerate added files");
            Vec::new()
        }
    }
}

/// Extract the type portion from a property signature string.
///
/// Given `"property: splitButtonOptions: SplitButtonOptions"`, returns
/// `Some("SplitButtonOptions")`.
///
/// Given `"property: gap: { default?: 'gapMd' | ... }"`, returns
/// `Some("{ default?: 'gapMd' | ... }")`.
fn extract_type_from_signature(sig: &str) -> Option<&str> {
    // Format: "<kind>: <name>: <type>"
    // Find past the first ": " (skips the kind prefix like "property"),
    // then past the second ": " (skips the prop name).
    let after_kind = sig.split_once(": ")?.1;
    let type_part = after_kind.split_once(": ")?.1;
    let trimmed = type_part.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Check whether two type strings are structurally compatible for a rename.
///
/// Returns `true` if the types have the same structure (both responsive
/// breakpoint objects, both string unions, etc.) — meaning a prop rename
/// codemod is valid. Returns `false` if the types are fundamentally
/// different (e.g., a named interface vs. an array) — meaning the change
/// requires LLM-assisted migration.
fn types_structurally_compatible(old_type: &str, new_type: &str) -> bool {
    // Both are responsive breakpoint objects: { default?: ...; md?: ...; }
    let old_is_object = old_type.starts_with('{');
    let new_is_object = new_type.starts_with('{');
    if old_is_object && new_is_object {
        return true;
    }

    // Both are string literal unions: 'foo' | 'bar'
    let old_is_union = old_type.contains('|') && old_type.contains('\'');
    let new_is_union = new_type.contains('|') && new_type.contains('\'');
    if old_is_union && new_is_union {
        return true;
    }

    // Identical types (e.g., both `boolean`, both `string`)
    if old_type == new_type {
        return true;
    }

    // Otherwise, structurally incompatible
    // (e.g., named interface "SplitButtonOptions" vs array "ReactNode[]")
    false
}

/// Convert an internal StructuralChange to a v2-format ApiChange.
fn structural_to_api_change(sc: &StructuralChange) -> ApiChange {
    let kind = symbol_kind_to_api_kind(sc.kind);
    let change = sc.change_type.to_api_change_type();
    let symbol = qualified_name_to_display_symbol(&sc.qualified_name, &sc.symbol);

    ApiChange {
        symbol,
        kind,
        change,
        before: sc.before.clone(),
        after: sc.after.clone(),
        description: sc.description.clone(),
        migration_target: sc.migration_target.clone(),
        removal_disposition: None,
        renders_element: None,
    }
}

/// Map internal SymbolKind to v2 ApiChangeKind.
fn symbol_kind_to_api_kind(kind: SymbolKind) -> ApiChangeKind {
    match kind {
        SymbolKind::Function => ApiChangeKind::Function,
        SymbolKind::Method => ApiChangeKind::Method,
        SymbolKind::Class => ApiChangeKind::Class,
        SymbolKind::Interface => ApiChangeKind::Interface,
        SymbolKind::TypeAlias => ApiChangeKind::TypeAlias,
        SymbolKind::Constant | SymbolKind::Variable => ApiChangeKind::Constant,
        SymbolKind::Enum => ApiChangeKind::TypeAlias,
        SymbolKind::Property => ApiChangeKind::Property,
        SymbolKind::Namespace => ApiChangeKind::ModuleExport,
        SymbolKind::Struct => ApiChangeKind::Class,
        SymbolKind::EnumMember => ApiChangeKind::Property,
        SymbolKind::Constructor => ApiChangeKind::Method,
        SymbolKind::GetAccessor | SymbolKind::SetAccessor => ApiChangeKind::Property,
    }
}

/// Convert a qualified name to a human-readable display symbol.
fn qualified_name_to_display_symbol(qualified_name: &str, symbol_name: &str) -> String {
    let last_path = qualified_name.rsplit('/').next().unwrap_or(qualified_name);
    let parts: Vec<&str> = last_path.split('.').collect();

    if parts.len() <= 1 {
        return symbol_name.to_string();
    }

    let symbol_parts = &parts[1..];

    if symbol_parts.is_empty() {
        return symbol_name.to_string();
    }

    if symbol_parts.len() == 1 {
        return symbol_parts[0].to_string();
    }

    let mut display_parts = symbol_parts.to_vec();
    if display_parts[0].ends_with("Props") {
        let component = display_parts[0].strip_suffix("Props").unwrap();
        if !component.is_empty() {
            display_parts[0] = component;
        }
    }

    display_parts.join(".")
}

// ─── Git helpers ─────────────────────────────────────────────────────────

fn resolve_sha(repo: &Path, git_ref: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", git_ref])
        .current_dir(repo)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn count_commits(repo: &Path, from_ref: &str, to_ref: &str) -> Option<usize> {
    let output = std::process::Command::new("git")
        .args(["rev-list", "--count", &format!("{}..{}", from_ref, to_ref)])
        .current_dir(repo)
        .output()
        .ok()?;
    if output.status.success() {
        String::from_utf8_lossy(&output.stdout).trim().parse().ok()
    } else {
        None
    }
}

/// Count unique files in an API surface.
pub fn count_unique_files(surface: &ApiSurface<TsSymbolData>) -> usize {
    let files: HashSet<&Path> = surface.symbols.iter().map(|s| s.file.as_path()).collect();
    files.len()
}

/// Convert a qualified name to a file path.
pub(crate) fn qualified_name_to_file(qualified_name: &str) -> PathBuf {
    if let Some(dot_pos) = qualified_name.rfind('.') {
        let file_part = &qualified_name[..dot_pos];
        PathBuf::from(format!("{}.d.ts", file_part))
    } else {
        PathBuf::from(qualified_name)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TsManifestChangeType;
    use semver_analyzer_core::{
        ApiSurface, BehavioralChange, BehavioralChangeKind, MemberMapping, Signature, Symbol,
        SymbolKind, Visibility,
    };
    use std::sync::Arc;

    #[test]
    fn build_report_empty() {
        let results = AnalysisResult {
            structural_changes: Arc::new(vec![]),
            behavioral_changes: vec![],
            manifest_changes: vec![],
            llm_api_changes: vec![],
            old_surface: Arc::new(ApiSurface::<TsSymbolData>::default()),
            new_surface: Arc::new(ApiSurface::<TsSymbolData>::default()),
            inferred_rename_patterns: None,
            container_changes: vec![],
            extensions: crate::TsAnalysisExtensions::default(),
        };
        let report = build_report(&results, Path::new("/tmp/repo"), "v1.0.0", "v2.0.0");
        assert_eq!(report.summary.total_breaking_changes, 0);
        assert!(report.changes.is_empty());
        assert!(report.manifest_changes.is_empty());
    }

    #[test]
    fn build_report_counts_breaking() {
        let changes = vec![
            StructuralChange {
                symbol: "foo".into(),
                qualified_name: "test.foo".into(),
                kind: SymbolKind::Function,
                package: None,
                change_type: StructuralChangeType::Removed(ChangeSubject::Symbol {
                    kind: SymbolKind::Function,
                }),
                before: None,
                after: None,
                description: "removed".into(),
                is_breaking: true,
                impact: None,
                migration_target: None,
            },
            StructuralChange {
                symbol: "bar".into(),
                qualified_name: "test.bar".into(),
                kind: SymbolKind::Function,
                package: None,
                change_type: StructuralChangeType::Added(ChangeSubject::Symbol {
                    kind: SymbolKind::Function,
                }),
                before: None,
                after: None,
                description: "added".into(),
                is_breaking: false,
                impact: None,
                migration_target: None,
            },
        ];
        let manifest = vec![ManifestChange {
            field: "type".into(),
            change_type: TsManifestChangeType::ModuleSystemChanged,
            before: Some("commonjs".into()),
            after: Some("module".into()),
            description: "CJS to ESM".into(),
            is_breaking: true,
        }];

        let results = AnalysisResult {
            structural_changes: Arc::new(changes),
            behavioral_changes: vec![],
            manifest_changes: manifest,
            llm_api_changes: vec![],
            old_surface: Arc::new(ApiSurface::<TsSymbolData>::default()),
            new_surface: Arc::new(ApiSurface::<TsSymbolData>::default()),
            inferred_rename_patterns: None,
            container_changes: vec![],
            extensions: crate::TsAnalysisExtensions::default(),
        };
        let report = build_report(&results, Path::new("/tmp/repo"), "v1", "v2");
        assert_eq!(report.summary.breaking_api_changes, 1);
        assert_eq!(report.summary.breaking_behavioral_changes, 0);
        assert_eq!(report.summary.total_breaking_changes, 1);
        assert_eq!(report.summary.files_with_breaking_changes, 1);
        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].breaking_api_changes.len(), 1);
    }

    #[test]
    fn build_report_with_behavioral_changes() {
        let behavioral = vec![BehavioralChange {
            symbol: "createUser".into(),
            kind: BehavioralChangeKind::Function,
            category: None,
            description: "Email normalization now strips + aliases".into(),
            source_file: Some("src/api/users.ts".into()),
            confidence: None,
            evidence_type: None,
            referenced_symbols: vec![],
            is_internal_only: None,
        }];

        let results = AnalysisResult {
            structural_changes: Arc::new(vec![]),
            behavioral_changes: behavioral,
            manifest_changes: vec![],
            llm_api_changes: vec![],
            old_surface: Arc::new(ApiSurface::<TsSymbolData>::default()),
            new_surface: Arc::new(ApiSurface::<TsSymbolData>::default()),
            inferred_rename_patterns: None,
            container_changes: vec![],
            extensions: crate::TsAnalysisExtensions::default(),
        };
        let report = build_report(&results, Path::new("/tmp/repo"), "v1", "v2");
        assert_eq!(report.summary.breaking_api_changes, 0);
        assert_eq!(report.summary.breaking_behavioral_changes, 1);
        assert_eq!(report.summary.total_breaking_changes, 1);
        assert_eq!(report.summary.files_with_breaking_changes, 1);
    }

    #[test]
    fn truly_removed_component_still_marked_removed() {
        let old_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![Symbol {
                name: "FooProps".to_string(),
                qualified_name: "src/components/Foo/Foo.FooProps".to_string(),
                kind: SymbolKind::Interface,
                visibility: Visibility::Exported,
                file: "src/components/Foo/Foo.FooProps.d.ts".into(),
                package: None,
                import_path: None,
                line: 1,
                signature: None,
                extends: None,
                implements: vec![],
                is_abstract: false,
                type_dependencies: vec![],
                is_readonly: false,
                is_static: false,
                accessor_kind: None,
                members: vec![Symbol {
                    name: "bar".to_string(),
                    qualified_name: "src/components/Foo/Foo.FooProps.bar".to_string(),
                    kind: SymbolKind::Property,
                    visibility: Visibility::Exported,
                    file: "src/components/Foo/Foo.FooProps.d.ts".into(),
                    package: None,
                    import_path: None,
                    line: 2,
                    signature: None,
                    extends: None,
                    implements: vec![],
                    is_abstract: false,
                    type_dependencies: vec![],
                    is_readonly: false,
                    is_static: false,
                    accessor_kind: None,
                    members: vec![],
                    language_data: Default::default(),
                }],
                language_data: Default::default(),
            }],
        };

        let new_surface = ApiSurface::default();

        let structural_changes = vec![StructuralChange {
            symbol: "FooProps".to_string(),
            qualified_name: "src/components/Foo/Foo.FooProps".to_string(),
            kind: SymbolKind::Interface,
            package: None,
            change_type: StructuralChangeType::Removed(ChangeSubject::Symbol {
                kind: SymbolKind::Interface,
            }),
            before: Some("FooProps".to_string()),
            after: None,
            description: "FooProps was removed".to_string(),
            is_breaking: true,
            impact: None,
            migration_target: None,
        }];

        let results = AnalysisResult {
            structural_changes: Arc::new(structural_changes),
            behavioral_changes: vec![],
            manifest_changes: vec![],
            llm_api_changes: vec![],
            old_surface: Arc::new(old_surface),
            new_surface: Arc::new(new_surface),
            inferred_rename_patterns: None,
            container_changes: vec![],
            extensions: crate::TsAnalysisExtensions::default(),
        };
        let report = build_report(&results, Path::new("/tmp/repo"), "v5", "v6");

        let foo_comp = report
            .packages
            .iter()
            .flat_map(|p| &p.type_summaries)
            .find(|c| c.name == "Foo");

        assert!(foo_comp.is_some(), "Foo should appear in the report");
        assert_eq!(
            foo_comp.unwrap().status,
            ComponentStatus::Removed,
            "Foo should be marked Removed when component doesn't exist in new surface"
        );
    }

    #[test]
    fn helper_interface_removal_does_not_mark_component_removed() {
        fn make_sym(name: &str, kind: SymbolKind, qn: &str) -> Symbol<TsSymbolData> {
            Symbol {
                name: name.to_string(),
                qualified_name: qn.to_string(),
                kind,
                visibility: Visibility::Exported,
                file: format!("{}.d.ts", qn).into(),
                package: None,
                import_path: None,
                line: 1,
                signature: None,
                extends: None,
                implements: vec![],
                is_abstract: false,
                type_dependencies: vec![],
                is_readonly: false,
                is_static: false,
                accessor_kind: None,
                members: vec![Symbol {
                    name: "color".to_string(),
                    qualified_name: format!("{}.color", qn),
                    kind: SymbolKind::Property,
                    visibility: Visibility::Exported,
                    file: format!("{}.d.ts", qn).into(),
                    package: None,
                    import_path: None,
                    line: 2,
                    signature: None,
                    extends: None,
                    implements: vec![],
                    is_abstract: false,
                    type_dependencies: vec![],
                    is_readonly: false,
                    is_static: false,
                    accessor_kind: None,
                    members: vec![],
                    language_data: Default::default(),
                }],
                language_data: Default::default(),
            }
        }

        let old_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![
                make_sym(
                    "IconProps",
                    SymbolKind::Interface,
                    "packages/react-core/src/components/EmptyState/EmptyStateIcon.IconProps",
                ),
                make_sym(
                    "Icon",
                    SymbolKind::Variable,
                    "packages/react-core/src/components/Icon/Icon.Icon",
                ),
            ],
        };

        let new_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![make_sym(
                "Icon",
                SymbolKind::Variable,
                "packages/react-core/src/components/Icon/Icon.Icon",
            )],
        };

        let structural_changes = vec![StructuralChange {
            symbol: "IconProps".to_string(),
            qualified_name:
                "packages/react-core/src/components/EmptyState/EmptyStateIcon.IconProps".to_string(),
            kind: SymbolKind::Interface,
            package: None,
            change_type: StructuralChangeType::Removed(ChangeSubject::Symbol {
                kind: SymbolKind::Interface,
            }),
            before: Some("IconProps".to_string()),
            after: None,
            description: "IconProps was removed".to_string(),
            is_breaking: true,
            impact: None,
            migration_target: None,
        }];

        let results = AnalysisResult {
            structural_changes: Arc::new(structural_changes),
            behavioral_changes: vec![],
            manifest_changes: vec![],
            llm_api_changes: vec![],
            old_surface: Arc::new(old_surface),
            new_surface: Arc::new(new_surface),
            inferred_rename_patterns: None,
            container_changes: vec![],
            extensions: crate::TsAnalysisExtensions::default(),
        };
        let report = build_report(&results, Path::new("/tmp/repo"), "v5", "v6");

        let icon_comp = report
            .packages
            .iter()
            .flat_map(|p| &p.type_summaries)
            .find(|c| c.name == "Icon");

        if let Some(comp) = icon_comp {
            assert_ne!(
                comp.status,
                ComponentStatus::Removed,
                "Icon should NOT be marked Removed when the component still exists in new surface. Status: {:?}",
                comp.status
            );
        }
    }

    #[test]
    fn discover_child_detects_same_dir_pascal_case() {
        fn make_symbol(name: &str, kind: SymbolKind, dir: &str) -> Symbol<TsSymbolData> {
            Symbol {
                name: name.to_string(),
                qualified_name: format!("{}/{}", dir, name),
                kind,
                visibility: Visibility::Exported,
                file: format!("{}/{}.d.ts", dir, name).into(),
                package: None,
                import_path: None,
                line: 1,
                signature: None,
                extends: None,
                implements: vec![],
                is_abstract: false,
                type_dependencies: vec![],
                is_readonly: false,
                is_static: false,
                accessor_kind: None,
                members: vec![],
                language_data: Default::default(),
            }
        }

        let dir = "packages/react-core/src/components/Modal";
        let old_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![make_symbol("Modal", SymbolKind::Variable, dir)],
        };
        let new_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![
                make_symbol("Modal", SymbolKind::Variable, dir),
                make_symbol("ModalHeader", SymbolKind::Variable, dir),
                make_symbol("ModalFooter", SymbolKind::Variable, dir),
            ],
        };

        let children = discover_child_components(
            "Modal",
            &format!("{}/Modal.ModalProps", dir),
            &old_surface,
            &new_surface,
            &[],
            &[],
            &[],
            &[],
        );

        let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"ModalHeader"),
            "Should detect ModalHeader. Found: {:?}",
            names
        );
        assert!(
            names.contains(&"ModalFooter"),
            "Should detect ModalFooter. Found: {:?}",
            names
        );
    }

    #[test]
    fn discover_child_excludes_different_dir() {
        fn make_symbol(name: &str, kind: SymbolKind, dir: &str) -> Symbol<TsSymbolData> {
            Symbol {
                name: name.to_string(),
                qualified_name: format!("{}/{}", dir, name),
                kind,
                visibility: Visibility::Exported,
                file: format!("{}/{}.d.ts", dir, name).into(),
                package: None,
                import_path: None,
                line: 1,
                signature: None,
                extends: None,
                implements: vec![],
                is_abstract: false,
                type_dependencies: vec![],
                is_readonly: false,
                is_static: false,
                accessor_kind: None,
                members: vec![],
                language_data: Default::default(),
            }
        }

        let modal_dir = "packages/react-core/src/components/Modal";
        let button_dir = "packages/react-core/src/components/Button";
        let old_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![make_symbol("Modal", SymbolKind::Variable, modal_dir)],
        };
        let new_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![
                make_symbol("Modal", SymbolKind::Variable, modal_dir),
                // This has the right prefix but wrong directory
                make_symbol("ModalButton", SymbolKind::Variable, button_dir),
            ],
        };

        let children = discover_child_components(
            "Modal",
            &format!("{}/Modal.ModalProps", modal_dir),
            &old_surface,
            &new_surface,
            &[],
            &[],
            &[],
            &[],
        );

        let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        assert!(
            !names.contains(&"ModalButton"),
            "Should NOT include ModalButton from different dir. Found: {:?}",
            names
        );
    }

    #[test]
    fn discover_child_components_filters_enums_and_types() {
        fn make_symbol(name: &str, kind: SymbolKind, dir: &str) -> Symbol<TsSymbolData> {
            Symbol {
                name: name.to_string(),
                qualified_name: format!("{}/{}", dir, name),
                kind,
                visibility: Visibility::Exported,
                file: format!("{}/{}.d.ts", dir, name).into(),
                package: None,
                import_path: None,
                line: 1,
                signature: None,
                extends: None,
                implements: vec![],
                is_abstract: false,
                type_dependencies: vec![],
                is_readonly: false,
                is_static: false,
                accessor_kind: None,
                members: vec![],
                language_data: Default::default(),
            }
        }

        let dir = "packages/react-core/src/components/Button";

        let old_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![make_symbol("Button", SymbolKind::Variable, dir)],
        };

        let new_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![
                make_symbol("Button", SymbolKind::Variable, dir),
                make_symbol("ButtonState", SymbolKind::Enum, dir),
                make_symbol("ButtonSize", SymbolKind::Enum, dir),
                make_symbol("ButtonType", SymbolKind::Enum, dir),
                make_symbol("ButtonProps", SymbolKind::Interface, dir),
                make_symbol("ButtonTypeAlias", SymbolKind::TypeAlias, dir),
                make_symbol("ButtonGroup", SymbolKind::Variable, dir),
            ],
        };

        let children = discover_child_components(
            "Button",
            &format!("{}/Button.ButtonProps", dir),
            &old_surface,
            &new_surface,
            &[],
            &[],
            &[],
            &[],
        );

        let child_names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();

        assert!(
            child_names.contains(&"ButtonGroup"),
            "Should include ButtonGroup (Variable = component). Found: {:?}",
            child_names
        );

        assert!(
            !child_names.contains(&"ButtonState"),
            "Should NOT include ButtonState (Enum). Found: {:?}",
            child_names
        );
        assert!(
            !child_names.contains(&"ButtonSize"),
            "Should NOT include ButtonSize (Enum). Found: {:?}",
            child_names
        );
        assert!(
            !child_names.contains(&"ButtonType"),
            "Should NOT include ButtonType (Enum). Found: {:?}",
            child_names
        );
        assert!(
            !child_names.contains(&"ButtonProps"),
            "Should NOT include ButtonProps (Interface). Found: {:?}",
            child_names
        );
        assert!(
            !child_names.contains(&"ButtonTypeAlias"),
            "Should NOT include ButtonTypeAlias (TypeAlias). Found: {:?}",
            child_names
        );
    }

    #[test]
    fn discover_child_components_skips_deprecated_path() {
        fn make_symbol(name: &str, kind: SymbolKind, qn: &str) -> Symbol<TsSymbolData> {
            Symbol {
                name: name.to_string(),
                qualified_name: qn.to_string(),
                kind,
                visibility: Visibility::Exported,
                file: format!("{}.d.ts", qn).into(),
                package: None,
                import_path: None,
                line: 1,
                signature: None,
                extends: None,
                implements: vec![],
                is_abstract: false,
                type_dependencies: vec![],
                is_readonly: false,
                is_static: false,
                accessor_kind: None,
                members: vec![],
                language_data: Default::default(),
            }
        }

        let main_dir = "packages/react-core/src/components/Modal";
        let depr_dir = "packages/react-core/src/deprecated/components/Modal";

        let old_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![make_symbol(
                "Modal",
                SymbolKind::Variable,
                &format!("{}/Modal", main_dir),
            )],
        };

        let new_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![
                make_symbol(
                    "Modal",
                    SymbolKind::Variable,
                    &format!("{}/Modal", main_dir),
                ),
                make_symbol(
                    "ModalHeader",
                    SymbolKind::Variable,
                    &format!("{}/ModalHeader", main_dir),
                ),
                make_symbol(
                    "ModalBox",
                    SymbolKind::Variable,
                    &format!("{}/ModalBox", depr_dir),
                ),
                make_symbol(
                    "ModalContent",
                    SymbolKind::Variable,
                    &format!("{}/ModalContent", depr_dir),
                ),
            ],
        };

        let children = discover_child_components(
            "Modal",
            &format!("{}/Modal.ModalProps", main_dir),
            &old_surface,
            &new_surface,
            &[],
            &[],
            &[],
            &[],
        );

        let child_names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();

        assert!(
            child_names.contains(&"ModalHeader"),
            "Should include ModalHeader (main path). Found: {:?}",
            child_names
        );
        assert!(
            !child_names.contains(&"ModalBox"),
            "Should NOT include ModalBox (deprecated path). Found: {:?}",
            child_names
        );
        assert!(
            !child_names.contains(&"ModalContent"),
            "Should NOT include ModalContent (deprecated path). Found: {:?}",
            child_names
        );
    }

    // ── qualified_name helpers ─────────────────────────────────────

    #[test]
    fn qualified_name_to_file_simple() {
        assert_eq!(
            qualified_name_to_file("test.greet"),
            PathBuf::from("test.d.ts")
        );
    }

    #[test]
    fn qualified_name_to_file_nested() {
        assert_eq!(
            qualified_name_to_file("src/api/users.createUser"),
            PathBuf::from("src/api/users.d.ts")
        );
    }

    #[test]
    fn qualified_name_to_file_class_member() {
        assert_eq!(
            qualified_name_to_file("test.Foo.bar"),
            PathBuf::from("test.Foo.d.ts")
        );
    }

    #[test]
    fn qualified_name_to_file_src_path() {
        assert_eq!(
            qualified_name_to_file("packages/react-core/src/components/Button/Button.ButtonProps"),
            PathBuf::from("packages/react-core/src/components/Button/Button.d.ts")
        );
    }

    #[test]
    fn display_symbol_simple() {
        assert_eq!(
            qualified_name_to_display_symbol("test.greet", "greet"),
            "greet"
        );
    }

    #[test]
    fn display_symbol_component_prop() {
        assert_eq!(
            qualified_name_to_display_symbol(
                "packages/react-core/dist/esm/components/Card/Card.CardProps.isFlat",
                "isFlat"
            ),
            "Card.isFlat"
        );
    }

    #[test]
    fn display_symbol_top_level() {
        assert_eq!(
            qualified_name_to_display_symbol(
                "packages/react-core/dist/esm/components/Card/Card.Card",
                "Card"
            ),
            "Card"
        );
    }

    #[test]
    fn display_symbol_non_props_interface() {
        assert_eq!(
            qualified_name_to_display_symbol(
                "packages/react-core/dist/esm/components/Accordion/AccordionContent.AccordionContent.isHidden",
                "isHidden"
            ),
            "AccordionContent.isHidden"
        );
    }

    #[test]
    fn display_symbol_interface_member() {
        assert_eq!(
            qualified_name_to_display_symbol(
                "packages/react-core/dist/esm/components/Button/Button.ButtonProps.variant",
                "variant"
            ),
            "Button.variant"
        );
    }

    #[test]
    fn package_summaries_use_scoped_npm_name_from_symbol_package() {
        // When Symbol.package has a scoped npm name (e.g., "@patternfly/react-core"),
        // the PackageChanges.name should use that instead of the bare directory name.
        use semver_analyzer_core::StructuralChangeType;

        let old_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![Symbol {
                name: "ButtonProps".into(),
                qualified_name: "packages/react-core/src/components/Button/Button.ButtonProps"
                    .into(),
                kind: SymbolKind::Interface,
                visibility: Visibility::Exported,
                file: "packages/react-core/src/components/Button/Button.d.ts".into(),
                package: Some("@patternfly/react-core".into()),
                import_path: None,
                line: 1,
                signature: None,
                extends: None,
                implements: vec![],
                is_abstract: false,
                type_dependencies: vec![],
                is_readonly: false,
                is_static: false,
                accessor_kind: None,
                members: vec![Symbol {
                    name: "variant".into(),
                    qualified_name:
                        "packages/react-core/src/components/Button/Button.ButtonProps.variant"
                            .into(),
                    kind: SymbolKind::Property,
                    visibility: Visibility::Public,
                    file: "packages/react-core/src/components/Button/Button.d.ts".into(),
                    package: Some("@patternfly/react-core".into()),
                    import_path: None,
                    line: 5,
                    signature: None,
                    extends: None,
                    implements: vec![],
                    is_abstract: false,
                    type_dependencies: vec![],
                    is_readonly: false,
                    is_static: false,
                    accessor_kind: None,
                    members: vec![],
                    language_data: Default::default(),
                }],
                language_data: Default::default(),
            }],
        };

        let new_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![Symbol {
                name: "ButtonProps".into(),
                qualified_name: "packages/react-core/src/components/Button/Button.ButtonProps"
                    .into(),
                kind: SymbolKind::Interface,
                visibility: Visibility::Exported,
                file: "packages/react-core/src/components/Button/Button.d.ts".into(),
                package: Some("@patternfly/react-core".into()),
                import_path: None,
                line: 1,
                signature: None,
                extends: None,
                implements: vec![],
                is_abstract: false,
                type_dependencies: vec![],
                is_readonly: false,
                is_static: false,
                accessor_kind: None,
                members: vec![],
                language_data: Default::default(),
            }],
        };

        let structural_changes = vec![semver_analyzer_core::StructuralChange {
            symbol: "variant".into(),
            qualified_name: "packages/react-core/src/components/Button/Button.ButtonProps.variant"
                .into(),
            kind: SymbolKind::Property,
            package: Some("@patternfly/react-core".into()),
            change_type: StructuralChangeType::Removed(
                semver_analyzer_core::ChangeSubject::Member {
                    name: "variant".into(),
                    kind: SymbolKind::Property,
                },
            ),
            before: Some("property: variant: 'primary' | 'secondary'".into()),
            after: None,
            is_breaking: true,
            description: "Property 'variant' was removed from ButtonProps".into(),
            migration_target: None,
            impact: None,
        }];

        let packages =
            build_package_summaries(&structural_changes, &[], &old_surface, &new_surface, &[]);

        assert_eq!(packages.len(), 1, "Should have one package");
        assert_eq!(
            packages[0].name, "@patternfly/react-core",
            "Package name should be the scoped npm name from Symbol.package, not the bare directory name"
        );
    }

    // ─── Deprecated→main hierarchy delta tests ───────────────────

    fn make_test_report(
        type_summaries: Vec<ComponentSummary<TypeScript>>,
        file_changes: Vec<FileChanges<TypeScript>>,
    ) -> AnalysisReport<TypeScript> {
        AnalysisReport {
            repository: PathBuf::from("/tmp/repo"),
            comparison: semver_analyzer_core::Comparison {
                from_ref: "v5.0.0".to_string(),
                to_ref: "v6.0.0".to_string(),
                from_sha: "aaa".to_string(),
                to_sha: "bbb".to_string(),
                commit_count: 1,
                analysis_timestamp: "now".to_string(),
            },
            summary: semver_analyzer_core::Summary {
                total_breaking_changes: 0,
                breaking_api_changes: 0,
                breaking_behavioral_changes: 0,
                files_with_breaking_changes: 0,
            },
            changes: file_changes,
            packages: vec![semver_analyzer_core::PackageChanges {
                name: "@patternfly/react-core".to_string(),
                old_version: None,
                new_version: None,
                type_summaries,
                constants: vec![],
                added_exports: vec![],
            }],
            manifest_changes: vec![],
            added_files: vec![],
            member_renames: HashMap::new(),
            inferred_rename_patterns: None,
            extensions: crate::TsAnalysisExtensions::default(),
            metadata: semver_analyzer_core::AnalysisMetadata {
                call_graph_analysis: "none".to_string(),
                tool_version: "test".to_string(),
                llm_usage: None,
            },
        }
    }

    #[test]
    fn deprecated_to_main_hierarchy_delta_created() {
        let mut report = make_test_report(
            vec![ComponentSummary {
                name: "Dropdown".to_string(),
                definition_name: "DropdownProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary {
                    total: 18,
                    removed: 0,
                    renamed: 0,
                    type_changed: 2,
                    added: 3,
                    removal_ratio: 0.0,
                },
                removed_members: vec![],
                type_changes: vec![],
                migration_target: Some(MigrationTarget {
                    removed_symbol: "Dropdown".to_string(),
                    removed_qualified_name:
                        "packages/react-core/src/deprecated/components/Dropdown/Dropdown.DropdownProps"
                            .to_string(),
                    removed_package: Some("@patternfly/react-core".to_string()),
                    replacement_symbol: "Dropdown".to_string(),
                    replacement_qualified_name:
                        "packages/react-core/src/components/Dropdown/Dropdown.DropdownProps"
                            .to_string(),
                    replacement_package: Some("@patternfly/react-core".to_string()),
                    matching_members: vec![
                        MemberMapping {
                            old_name: "className".to_string(),
                            new_name: "className".to_string(),
                        },
                        MemberMapping {
                            old_name: "isOpen".to_string(),
                            new_name: "isOpen".to_string(),
                        },
                    ],
                    removed_only_members: vec!["dropdownItems".to_string()],
                    overlap_ratio: 0.43,
                    old_extends: None,
                    new_extends: None,
                }),
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![
                    ExpectedChild::new("DropdownList", true),
                    ExpectedChild::new("DropdownGroup", false),
                ],
                source_files: vec![],
            }],
            vec![FileChanges {
                file: PathBuf::from("packages/react-core/src/deprecated/components/Dropdown/Dropdown.d.ts"),
                status: FileStatus::Deleted,
                renamed_from: None,
                breaking_api_changes: vec![
                    ApiChange {
                        symbol: "DropdownToggle".to_string(),
                        kind: ApiChangeKind::Constant,
                        change: ApiChangeType::Removed,
                        before: None,
                        after: None,
                        description: "removed".to_string(),
                        migration_target: None,
                        removal_disposition: None,
                        renders_element: None,
                    },
                    ApiChange {
                        symbol: "KebabToggle".to_string(),
                        kind: ApiChangeKind::Constant,
                        change: ApiChangeType::Removed,
                        before: None,
                        after: None,
                        description: "removed".to_string(),
                        migration_target: None,
                        removal_disposition: None,
                        renders_element: None,
                    },
                ],
                breaking_behavioral_changes: vec![],
                container_changes: vec![],
            }],
        );

        let new_surface = ApiSurface::default();
        let new_hierarchies = HashMap::new();
        enrich_hierarchy_deltas(&mut report, vec![], &new_surface, &new_hierarchies);

        let deprecated_deltas: Vec<&HierarchyDelta> = report
            .extensions
            .hierarchy_deltas
            .iter()
            .filter(|d| d.source_package.is_some())
            .collect();

        assert_eq!(
            deprecated_deltas.len(),
            1,
            "Should create one deprecated→main delta"
        );

        let delta = deprecated_deltas[0];
        assert_eq!(delta.component, "Dropdown");
        assert!(delta
            .source_package
            .as_ref()
            .unwrap()
            .contains("deprecated"));

        // Should have expected_children from the main module
        let child_names: Vec<&str> = delta
            .added_children
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(child_names.contains(&"DropdownList"));
        assert!(child_names.contains(&"DropdownGroup"));

        // Should have migration_target
        assert!(delta.migration_target.is_some());

        // Should have removed symbols with no replacement
        assert!(delta
            .removed_children
            .contains(&"DropdownToggle".to_string()));
        assert!(delta.removed_children.contains(&"KebabToggle".to_string()));
    }

    #[test]
    fn no_deprecated_delta_without_migration_target() {
        let mut report = make_test_report(
            vec![ComponentSummary {
                name: "ApplicationLauncher".to_string(),
                definition_name: "ApplicationLauncherProps".to_string(),
                status: ComponentStatus::Removed,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![],
                source_files: vec![],
            }],
            vec![],
        );

        let new_surface = ApiSurface::default();
        let new_hierarchies = HashMap::new();
        enrich_hierarchy_deltas(&mut report, vec![], &new_surface, &new_hierarchies);

        let deprecated_deltas: Vec<&HierarchyDelta> = report
            .extensions
            .hierarchy_deltas
            .iter()
            .filter(|d| d.source_package.is_some())
            .collect();

        assert_eq!(
            deprecated_deltas.len(),
            0,
            "No deprecated delta without migration_target"
        );
    }

    #[test]
    fn deprecated_delta_has_migrated_members_from_props() {
        let mut report = make_test_report(
            vec![ComponentSummary {
                name: "Dropdown".to_string(),
                definition_name: "DropdownProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: Some(MigrationTarget {
                    removed_symbol: "Dropdown".to_string(),
                    removed_qualified_name:
                        "packages/react-core/src/deprecated/components/Dropdown/Dropdown.DropdownProps"
                            .to_string(),
                    removed_package: Some("@patternfly/react-core".to_string()),
                    replacement_symbol: "Dropdown".to_string(),
                    replacement_qualified_name:
                        "packages/react-core/src/components/Dropdown/Dropdown.DropdownProps".to_string(),
                    replacement_package: Some("@patternfly/react-core".to_string()),
                    matching_members: vec![
                        MemberMapping { old_name: "className".to_string(), new_name: "className".to_string() },
                        MemberMapping { old_name: "isOpen".to_string(), new_name: "isOpen".to_string() },
                    ],
                    removed_only_members: vec!["dropdownItems".to_string()],
                    overlap_ratio: 0.5,
                    old_extends: None,
                    new_extends: None,
                }),
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![ExpectedChild::new("DropdownList", true)],
                source_files: vec![],
            }],
            vec![],
        );

        let new_surface = ApiSurface::default();
        let new_hierarchies = HashMap::new();
        enrich_hierarchy_deltas(&mut report, vec![], &new_surface, &new_hierarchies);

        let deprecated_deltas: Vec<&HierarchyDelta> = report
            .extensions
            .hierarchy_deltas
            .iter()
            .filter(|d| d.source_package.is_some())
            .collect();

        assert_eq!(deprecated_deltas.len(), 1);
        let delta = &deprecated_deltas[0];

        assert_eq!(
            delta.migrated_members.len(),
            2,
            "Should have 2 migrated members"
        );
        assert!(
            delta
                .migrated_members
                .iter()
                .any(|m| m.member_name == "className" && m.target_child == "Dropdown"),
            "className should map to Dropdown"
        );
    }

    // ─── Slot prop inference tests ─────────────────────────────────

    #[test]
    fn test_is_slot_prop_type() {
        assert!(is_slot_prop_type("ReactElement<any>"));
        assert!(is_slot_prop_type("ReactNode"));
        assert!(is_slot_prop_type("ReactElement"));
        assert!(is_slot_prop_type("JSX.Element"));
        assert!(is_slot_prop_type("ReactNode | undefined"));
        assert!(is_slot_prop_type("Element | null"));
        // Non-slot types
        assert!(!is_slot_prop_type("string"));
        assert!(!is_slot_prop_type("boolean"));
        assert!(!is_slot_prop_type("number"));
        assert!(!is_slot_prop_type("() => void"));
    }

    #[test]
    fn test_infer_prop_name_for_child() {
        let mut prop_types = HashMap::new();
        prop_types.insert("labelHelp".to_string(), "ReactElement<any>".to_string());
        prop_types.insert("children".to_string(), "ReactNode".to_string());
        prop_types.insert("label".to_string(), "string".to_string());

        // FormGroupLabelHelp → strip FormGroup → LabelHelp → labelHelp → found
        assert_eq!(
            infer_prop_name_for_child("FormGroup", "FormGroupLabelHelp", &prop_types),
            Some("labelHelp".to_string())
        );

        // No prefix match
        assert_eq!(
            infer_prop_name_for_child("Modal", "FormGroupLabelHelp", &prop_types),
            None
        );

        // Match exists but type is string, not a slot
        prop_types.insert("labelHelp".to_string(), "string".to_string());
        assert_eq!(
            infer_prop_name_for_child("FormGroup", "FormGroupLabelHelp", &prop_types),
            None
        );
    }

    #[test]
    fn test_infer_prop_name_modal_header() {
        let mut prop_types = HashMap::new();
        prop_types.insert("header".to_string(), "ReactNode".to_string());
        prop_types.insert("footer".to_string(), "ReactNode".to_string());
        prop_types.insert("title".to_string(), "string".to_string());

        assert_eq!(
            infer_prop_name_for_child("Modal", "ModalHeader", &prop_types),
            Some("header".to_string())
        );
        assert_eq!(
            infer_prop_name_for_child("Modal", "ModalFooter", &prop_types),
            Some("footer".to_string())
        );
        // "title" is a string prop, not a slot — shouldn't match
        assert_eq!(
            infer_prop_name_for_child("Modal", "ModalTitle", &prop_types),
            None
        );
    }

    #[test]
    fn test_prop_name_inference_in_enrich() {
        // Simulate the FormGroup scenario: LLM returned mechanism="prop"
        // but prop_name is None. After enrichment, it should be "labelHelp".
        let mut report = make_test_report(
            vec![ComponentSummary {
                name: "FormGroup".to_string(),
                definition_name: "FormGroupProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![
                    ExpectedChild {
                        name: "FormGroupLabelHelp".to_string(),
                        required: false,
                        mechanism: "prop".to_string(),
                        prop_name: None, // Unknown — should be inferred
                    },
                    ExpectedChild::new("FormHelperText", false),
                ],
                source_files: vec![],
            }],
            vec![],
        );

        // Build a new surface with FormGroupProps that has labelHelp: ReactElement<any>
        let form_group_props = Symbol {
            name: "FormGroupProps".to_string(),
            qualified_name: "react-core/FormGroup.FormGroupProps".to_string(),
            kind: SymbolKind::Interface,
            visibility: Visibility::Public,
            file: PathBuf::from("react-core/FormGroup.d.ts"),
            package: Some("@patternfly/react-core".to_string()),
            import_path: None,
            line: 1,
            signature: None,
            extends: None,
            implements: vec![],
            is_abstract: false,
            type_dependencies: vec![],
            is_readonly: false,
            is_static: false,
            accessor_kind: None,
            members: vec![
                Symbol {
                    name: "labelHelp".to_string(),
                    qualified_name: "react-core/FormGroup.FormGroupProps.labelHelp".to_string(),
                    kind: SymbolKind::Property,
                    visibility: Visibility::Public,
                    file: PathBuf::from("react-core/FormGroup.d.ts"),
                    package: None,
                    import_path: None,
                    line: 2,
                    signature: Some(Signature {
                        parameters: vec![],
                        return_type: Some("ReactElement<any>".to_string()),
                        type_parameters: vec![],
                        is_async: false,
                    }),
                    extends: None,
                    implements: vec![],
                    is_abstract: false,
                    type_dependencies: vec!["ReactElement".to_string()],
                    is_readonly: false,
                    is_static: false,
                    accessor_kind: None,
                    members: vec![],
                    language_data: Default::default(),
                },
                Symbol {
                    name: "children".to_string(),
                    qualified_name: "react-core/FormGroup.FormGroupProps.children".to_string(),
                    kind: SymbolKind::Property,
                    visibility: Visibility::Public,
                    file: PathBuf::from("react-core/FormGroup.d.ts"),
                    package: None,
                    import_path: None,
                    line: 3,
                    signature: Some(Signature {
                        parameters: vec![],
                        return_type: Some("ReactNode".to_string()),
                        type_parameters: vec![],
                        is_async: false,
                    }),
                    extends: None,
                    implements: vec![],
                    is_abstract: false,
                    type_dependencies: vec!["ReactNode".to_string()],
                    is_readonly: false,
                    is_static: false,
                    accessor_kind: None,
                    members: vec![],
                    language_data: Default::default(),
                },
            ],
            language_data: Default::default(),
        };

        let new_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![form_group_props],
        };
        let new_hierarchies = HashMap::new();

        enrich_hierarchy_deltas(&mut report, vec![], &new_surface, &new_hierarchies);

        // Check that prop_name was inferred
        let form_group = report.packages[0]
            .type_summaries
            .iter()
            .find(|c| c.name == "FormGroup")
            .unwrap();

        let label_help = form_group
            .expected_children
            .iter()
            .find(|c| c.name == "FormGroupLabelHelp")
            .unwrap();

        assert_eq!(
            label_help.mechanism, "prop",
            "mechanism should remain 'prop'"
        );
        assert_eq!(
            label_help.prop_name,
            Some("labelHelp".to_string()),
            "prop_name should be inferred as 'labelHelp'"
        );

        // FormHelperText should remain as direct child
        let helper_text = form_group
            .expected_children
            .iter()
            .find(|c| c.name == "FormHelperText")
            .unwrap();
        assert_eq!(helper_text.mechanism, "child");
        assert_eq!(helper_text.prop_name, None);
    }

    #[test]
    fn test_prop_name_inference_flips_child_to_prop() {
        // LLM said mechanism="child" but the Props interface proves it's
        // prop-passed (the parent has a slot prop matching the child name).
        let mut report = make_test_report(
            vec![ComponentSummary {
                name: "FormGroup".to_string(),
                definition_name: "FormGroupProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![ExpectedChild {
                    name: "FormGroupLabelHelp".to_string(),
                    required: false,
                    mechanism: "child".to_string(), // LLM got this wrong
                    prop_name: None,
                }],
                source_files: vec![],
            }],
            vec![],
        );

        let form_group_props = Symbol {
            name: "FormGroupProps".to_string(),
            qualified_name: "react-core/FormGroup.FormGroupProps".to_string(),
            kind: SymbolKind::Interface,
            visibility: Visibility::Public,
            file: PathBuf::from("react-core/FormGroup.d.ts"),
            package: Some("@patternfly/react-core".to_string()),
            import_path: None,
            line: 1,
            signature: None,
            extends: None,
            implements: vec![],
            is_abstract: false,
            type_dependencies: vec![],
            is_readonly: false,
            is_static: false,
            accessor_kind: None,
            members: vec![Symbol {
                name: "labelHelp".to_string(),
                qualified_name: "react-core/FormGroup.FormGroupProps.labelHelp".to_string(),
                kind: SymbolKind::Property,
                visibility: Visibility::Public,
                file: PathBuf::from("react-core/FormGroup.d.ts"),
                package: None,
                import_path: None,
                line: 2,
                signature: Some(Signature {
                    parameters: vec![],
                    return_type: Some("ReactElement<any>".to_string()),
                    type_parameters: vec![],
                    is_async: false,
                }),
                extends: None,
                implements: vec![],
                is_abstract: false,
                type_dependencies: vec![],
                is_readonly: false,
                is_static: false,
                accessor_kind: None,
                members: vec![],
                language_data: Default::default(),
            }],
            language_data: Default::default(),
        };

        let new_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![form_group_props],
        };
        let new_hierarchies = HashMap::new();
        enrich_hierarchy_deltas(&mut report, vec![], &new_surface, &new_hierarchies);

        let label_help = report.packages[0].type_summaries[0]
            .expected_children
            .iter()
            .find(|c| c.name == "FormGroupLabelHelp")
            .unwrap();

        assert_eq!(
            label_help.mechanism, "prop",
            "mechanism should be flipped from 'child' to 'prop'"
        );
        assert_eq!(
            label_help.prop_name,
            Some("labelHelp".to_string()),
            "prop_name should be inferred"
        );
    }

    #[test]
    fn test_prop_name_inference_on_hierarchy_deltas() {
        // The inference should also fix hierarchy deltas, not just
        // expected_children on type_summaries.
        let mut report = make_test_report(vec![], vec![]);

        let deltas = vec![HierarchyDelta {
            component: "FormGroup".to_string(),
            added_children: vec![ExpectedChild {
                name: "FormGroupLabelHelp".to_string(),
                required: false,
                mechanism: "prop".to_string(),
                prop_name: None,
            }],
            removed_children: vec![],
            migrated_members: vec![],
            source_package: None,
            migration_target: None,
        }];

        let form_group_props = Symbol {
            name: "FormGroupProps".to_string(),
            qualified_name: "react-core/FormGroup.FormGroupProps".to_string(),
            kind: SymbolKind::Interface,
            visibility: Visibility::Public,
            file: PathBuf::from("react-core/FormGroup.d.ts"),
            package: Some("@patternfly/react-core".to_string()),
            import_path: None,
            line: 1,
            signature: None,
            extends: None,
            implements: vec![],
            is_abstract: false,
            type_dependencies: vec![],
            is_readonly: false,
            is_static: false,
            accessor_kind: None,
            members: vec![Symbol {
                name: "labelHelp".to_string(),
                qualified_name: "react-core/FormGroup.FormGroupProps.labelHelp".to_string(),
                kind: SymbolKind::Property,
                visibility: Visibility::Public,
                file: PathBuf::from("react-core/FormGroup.d.ts"),
                package: None,
                import_path: None,
                line: 2,
                signature: Some(Signature {
                    parameters: vec![],
                    return_type: Some("ReactElement<any>".to_string()),
                    type_parameters: vec![],
                    is_async: false,
                }),
                extends: None,
                implements: vec![],
                is_abstract: false,
                type_dependencies: vec![],
                is_readonly: false,
                is_static: false,
                accessor_kind: None,
                members: vec![],
                language_data: Default::default(),
            }],
            language_data: Default::default(),
        };

        let new_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![form_group_props],
        };
        let new_hierarchies = HashMap::new();
        enrich_hierarchy_deltas(&mut report, deltas, &new_surface, &new_hierarchies);

        let delta = report
            .extensions
            .hierarchy_deltas
            .iter()
            .find(|d| d.component == "FormGroup")
            .expect("FormGroup delta should exist");

        let child = &delta.added_children[0];
        assert_eq!(child.mechanism, "prop");
        assert_eq!(
            child.prop_name,
            Some("labelHelp".to_string()),
            "prop_name should be inferred on hierarchy delta"
        );
    }

    #[test]
    fn test_prop_name_no_false_positive_on_string_props() {
        // If the prop exists but has type "string" (not a slot type),
        // it should NOT be inferred as prop_name.
        let mut report = make_test_report(
            vec![ComponentSummary {
                name: "Modal".to_string(),
                definition_name: "ModalProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![ExpectedChild {
                    name: "ModalTitle".to_string(),
                    required: false,
                    mechanism: "child".to_string(),
                    prop_name: None,
                }],
                source_files: vec![],
            }],
            vec![],
        );

        let modal_props = Symbol {
            name: "ModalProps".to_string(),
            qualified_name: "react-core/Modal.ModalProps".to_string(),
            kind: SymbolKind::Interface,
            visibility: Visibility::Public,
            file: PathBuf::from("react-core/Modal.d.ts"),
            package: Some("@patternfly/react-core".to_string()),
            import_path: None,
            line: 1,
            signature: None,
            extends: None,
            implements: vec![],
            is_abstract: false,
            type_dependencies: vec![],
            is_readonly: false,
            is_static: false,
            accessor_kind: None,
            members: vec![Symbol {
                name: "title".to_string(),
                qualified_name: "react-core/Modal.ModalProps.title".to_string(),
                kind: SymbolKind::Property,
                visibility: Visibility::Public,
                file: PathBuf::from("react-core/Modal.d.ts"),
                package: None,
                import_path: None,
                line: 2,
                signature: Some(Signature {
                    parameters: vec![],
                    return_type: Some("string".to_string()),
                    type_parameters: vec![],
                    is_async: false,
                }),
                extends: None,
                implements: vec![],
                is_abstract: false,
                type_dependencies: vec![],
                is_readonly: false,
                is_static: false,
                accessor_kind: None,
                members: vec![],
                language_data: Default::default(),
            }],
            language_data: Default::default(),
        };

        let new_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![modal_props],
        };
        let new_hierarchies = HashMap::new();
        enrich_hierarchy_deltas(&mut report, vec![], &new_surface, &new_hierarchies);

        let modal_title = report.packages[0].type_summaries[0].expected_children[0].clone();

        assert_eq!(
            modal_title.mechanism, "child",
            "Should NOT flip to 'prop' when prop type is string"
        );
        assert_eq!(
            modal_title.prop_name, None,
            "Should NOT infer prop_name for non-slot type"
        );
    }

    #[test]
    fn test_deprecated_surface_does_not_contaminate_prop_inference() {
        // Reproduces the real Modal bug: v6 ships both a main ModalProps
        // (without header/footer) and a deprecated ModalProps (with
        // header/footer). The prop_name inference should only use the
        // main module, so ModalHeader stays as mechanism="child".
        let mut report = make_test_report(
            vec![ComponentSummary {
                name: "Modal".to_string(),
                definition_name: "ModalProps".to_string(),
                status: ComponentStatus::Modified,
                member_summary: MemberSummary {
                    total: 28,
                    removed: 11,
                    renamed: 0,
                    type_changed: 0,
                    added: 0,
                    removal_ratio: 0.39,
                },
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                child_components: vec![],
                expected_children: vec![
                    ExpectedChild {
                        name: "ModalHeader".to_string(),
                        required: false,
                        mechanism: "child".to_string(), // LLM correctly said child
                        prop_name: None,
                    },
                    ExpectedChild::new("ModalBody", false),
                    ExpectedChild {
                        name: "ModalFooter".to_string(),
                        required: false,
                        mechanism: "child".to_string(), // LLM correctly said child
                        prop_name: None,
                    },
                ],
                source_files: vec![],
            }],
            vec![],
        );

        // Main ModalProps (v6) — NO header/footer props, just children
        let main_modal_props = Symbol {
            name: "ModalProps".to_string(),
            qualified_name: "react-core/components/Modal/Modal.ModalProps".to_string(),
            kind: SymbolKind::Interface,
            visibility: Visibility::Public,
            file: PathBuf::from("react-core/src/components/Modal/Modal.d.ts"),
            package: Some("@patternfly/react-core".to_string()),
            import_path: None,
            line: 1,
            signature: None,
            extends: None,
            implements: vec![],
            is_abstract: false,
            type_dependencies: vec![],
            is_readonly: false,
            is_static: false,
            accessor_kind: None,
            members: vec![Symbol {
                name: "children".to_string(),
                qualified_name: "react-core/Modal.ModalProps.children".to_string(),
                kind: SymbolKind::Property,
                visibility: Visibility::Public,
                file: PathBuf::from("react-core/src/components/Modal/Modal.d.ts"),
                package: None,
                import_path: None,
                line: 2,
                signature: Some(Signature {
                    parameters: vec![],
                    return_type: Some("ReactNode".to_string()),
                    type_parameters: vec![],
                    is_async: false,
                }),
                extends: None,
                implements: vec![],
                is_abstract: false,
                type_dependencies: vec![],
                is_readonly: false,
                is_static: false,
                accessor_kind: None,
                members: vec![],
                language_data: Default::default(),
            }],
            language_data: Default::default(),
        };

        // Deprecated ModalProps (old API re-exported in v6) — HAS header/footer
        let deprecated_modal_props = Symbol {
            name: "ModalProps".to_string(),
            qualified_name: "react-core/deprecated/components/Modal/Modal.ModalProps".to_string(),
            kind: SymbolKind::Interface,
            visibility: Visibility::Public,
            file: PathBuf::from("react-core/src/deprecated/components/Modal/Modal.d.ts"),
            package: Some("@patternfly/react-core".to_string()),
            import_path: None,
            line: 1,
            signature: None,
            extends: None,
            implements: vec![],
            is_abstract: false,
            type_dependencies: vec![],
            is_readonly: false,
            is_static: false,
            accessor_kind: None,
            members: vec![
                Symbol {
                    name: "header".to_string(),
                    qualified_name: "react-core/deprecated/Modal.ModalProps.header".to_string(),
                    kind: SymbolKind::Property,
                    visibility: Visibility::Public,
                    file: PathBuf::from("react-core/src/deprecated/components/Modal/Modal.d.ts"),
                    package: None,
                    import_path: None,
                    line: 2,
                    signature: Some(Signature {
                        parameters: vec![],
                        return_type: Some("ReactNode".to_string()),
                        type_parameters: vec![],
                        is_async: false,
                    }),
                    extends: None,
                    implements: vec![],
                    is_abstract: false,
                    type_dependencies: vec![],
                    is_readonly: false,
                    is_static: false,
                    accessor_kind: None,
                    members: vec![],
                    language_data: Default::default(),
                },
                Symbol {
                    name: "footer".to_string(),
                    qualified_name: "react-core/deprecated/Modal.ModalProps.footer".to_string(),
                    kind: SymbolKind::Property,
                    visibility: Visibility::Public,
                    file: PathBuf::from("react-core/src/deprecated/components/Modal/Modal.d.ts"),
                    package: None,
                    import_path: None,
                    line: 3,
                    signature: Some(Signature {
                        parameters: vec![],
                        return_type: Some("ReactNode".to_string()),
                        type_parameters: vec![],
                        is_async: false,
                    }),
                    extends: None,
                    implements: vec![],
                    is_abstract: false,
                    type_dependencies: vec![],
                    is_readonly: false,
                    is_static: false,
                    accessor_kind: None,
                    members: vec![],
                    language_data: Default::default(),
                },
            ],
            language_data: Default::default(),
        };

        // new_surface contains BOTH — simulating what the real extraction produces
        let new_surface = ApiSurface::<TsSymbolData> {
            symbols: vec![main_modal_props, deprecated_modal_props],
        };
        let new_hierarchies = HashMap::new();

        enrich_hierarchy_deltas(&mut report, vec![], &new_surface, &new_hierarchies);

        let modal = report.packages[0]
            .type_summaries
            .iter()
            .find(|c| c.name == "Modal")
            .unwrap();

        let modal_header = modal
            .expected_children
            .iter()
            .find(|c| c.name == "ModalHeader")
            .unwrap();
        assert_eq!(
            modal_header.mechanism, "child",
            "ModalHeader should stay as 'child' — the deprecated header prop must not contaminate inference"
        );
        assert_eq!(
            modal_header.prop_name, None,
            "ModalHeader should have no prop_name — header was removed from main ModalProps"
        );

        let modal_footer = modal
            .expected_children
            .iter()
            .find(|c| c.name == "ModalFooter")
            .unwrap();
        assert_eq!(
            modal_footer.mechanism, "child",
            "ModalFooter should stay as 'child' — the deprecated footer prop must not contaminate inference"
        );
        assert_eq!(
            modal_footer.prop_name, None,
            "ModalFooter should have no prop_name — footer was removed from main ModalProps"
        );

        // ModalBody should also remain as child
        let modal_body = modal
            .expected_children
            .iter()
            .find(|c| c.name == "ModalBody")
            .unwrap();
        assert_eq!(modal_body.mechanism, "child");
    }

    // ── extract_type_from_signature tests ─────────────────────────────

    #[test]
    fn test_extract_type_named() {
        assert_eq!(
            extract_type_from_signature("property: splitButtonOptions: SplitButtonOptions"),
            Some("SplitButtonOptions")
        );
    }

    #[test]
    fn test_extract_type_array() {
        assert_eq!(
            extract_type_from_signature("property: splitButtonItems: ReactNode[]"),
            Some("ReactNode[]")
        );
    }

    #[test]
    fn test_extract_type_object() {
        let sig = "property: gap: { default?: 'gapMd' | 'gapNone'; md?: 'gapMd' }";
        let result = extract_type_from_signature(sig);
        assert!(result.unwrap().starts_with("{ default?:"));
    }

    #[test]
    fn test_extract_type_no_type() {
        assert_eq!(extract_type_from_signature("property: foo"), None);
    }

    #[test]
    fn test_extract_type_empty() {
        assert_eq!(extract_type_from_signature(""), None);
    }

    // ── types_structurally_compatible tests ───────────────────────────

    #[test]
    fn test_compatible_identical_types() {
        assert!(types_structurally_compatible("boolean", "boolean"));
    }

    #[test]
    fn test_compatible_both_objects() {
        assert!(types_structurally_compatible(
            "{ default?: 'spaceItemsMd' | 'spaceItemsNone' }",
            "{ default?: 'gapMd' | 'gapNone' }"
        ));
    }

    #[test]
    fn test_compatible_both_unions() {
        assert!(types_structurally_compatible(
            "'spaceItemsMd' | 'spaceItemsNone'",
            "'gapMd' | 'gapNone'"
        ));
    }

    #[test]
    fn test_incompatible_interface_to_array() {
        assert!(!types_structurally_compatible(
            "SplitButtonOptions",
            "ReactNode[]"
        ));
    }

    #[test]
    fn test_incompatible_object_to_array() {
        assert!(!types_structurally_compatible(
            "{ items: ReactNode[] }",
            "ReactNode[]"
        ));
    }

    #[test]
    fn test_incompatible_different_identifiers() {
        assert!(!types_structurally_compatible("FooType", "BarType"));
    }
}
