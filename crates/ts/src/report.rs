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

use semver_analyzer_core::ApiSurface as CoreApiSurface;
use semver_analyzer_core::Symbol as CoreSymbol;
use semver_analyzer_core::{
    AddedExport, AnalysisMetadata, AnalysisReport, AnalysisResult, ApiChange, ApiChangeKind,
    ApiChangeType, BehavioralChange, ChangeSubject, Comparison, ConstantGroup, ExpectedChild,
    FileChanges, FileStatus, HierarchyDelta, InferredRenamePatterns, LlmApiChange, ManifestChange,
    MemberSummary, MigratedMember, MigrationTarget, PackageChanges, RemovalDisposition,
    RemovedMember, StructuralChange, StructuralChangeType, SuffixRename, Summary, SymbolKind,
    TypeChange, TypeStatus, TypeSummary,
};

use crate::language::{ChildComponent, ChildComponentStatus, TsReportData};

use crate::TsSymbolData;

/// Type aliases: all Symbols/ApiSurfaces in report.rs carry `TsSymbolData`.
type Symbol = CoreSymbol<TsSymbolData>;
type ApiSurface = CoreApiSurface<TsSymbolData>;

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

    // Enrich removal_disposition using SD prop replacement detection.
    // This catches prop renames that the TD pipeline's rename detector missed
    // due to threshold boundaries (e.g., isActive→isClicked at 0.444 similarity
    // vs 0.45 threshold) by using per-component SD data: prop_style_bindings,
    // old/new component props, and prop types.
    enrich_removal_dispositions_from_sd(&mut report);

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
    old_surface: &ApiSurface,
    new_surface: &ApiSurface,
    inferred_rename_patterns: Option<InferredRenamePatterns>,
) -> AnalysisReport<TypeScript> {
    // Group breaking structural changes by file, converting to v2 ApiChange format.
    // Non-breaking changes (symbol_added, etc.) are excluded from the report.
    let mut file_api_map: BTreeMap<PathBuf, Vec<ApiChange>> = BTreeMap::new();

    // Track (symbol, change_type) pairs from non-barrel files so we can
    // suppress duplicates that appear in barrel index.d.ts re-exports.
    let mut seen_in_submodule: HashSet<(String, String)> = HashSet::new();

    for change in structural_changes {
        if !change.is_breaking {
            continue;
        }
        let file = qualified_name_to_file(&change.qualified_name);
        let is_barrel = file.file_name().map(|n| n == "index.d.ts").unwrap_or(false);
        let api_change = structural_to_api_change(change);

        if !is_barrel {
            seen_in_submodule.insert((
                api_change.symbol.clone(),
                format!("{:?}", api_change.change),
            ));
        }

        file_api_map.entry(file).or_default().push(api_change);
    }

    // Remove barrel index.d.ts entries that duplicate a sub-module entry.
    // When the same symbol+change appears in both a sub-module file and its
    // barrel re-export, the sub-module entry is preferred because it carries
    // richer type information and maps to the actual source file.
    for (file, changes) in file_api_map.iter_mut() {
        let is_barrel = file.file_name().map(|n| n == "index.d.ts").unwrap_or(false);
        if is_barrel {
            changes.retain(|c| {
                !seen_in_submodule.contains(&(c.symbol.clone(), format!("{:?}", c.change)))
            });
        }
    }
    // Remove empty barrel files after deduplication.
    file_api_map.retain(|_, changes| !changes.is_empty());

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
            qualified_name: String::new(),
            kind,
            change: change_type,
            before: None,
            after: None,
            description: entry.description.clone(),
            migration_target: None,
            removal_disposition,
        };
        // Only add if not already present (avoid duplicating TD findings).
        // If the symbol already exists from TD, enrich it with LLM data
        // (removal_disposition) that TD doesn't produce.
        let existing = file_api_map.entry(file).or_default();
        if let Some(td_entry) = existing.iter_mut().find(|c| c.symbol == api_change.symbol) {
            if td_entry.removal_disposition.is_none() && api_change.removal_disposition.is_some() {
                td_entry.removal_disposition = api_change.removal_disposition;
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
                if !matches!(
                    change.change,
                    ApiChangeType::Removed | ApiChangeType::Renamed
                ) {
                    continue;
                }
                let new_member = match &change.removal_disposition {
                    Some(RemovalDisposition::ReplacedByMember { new_member }) => new_member.clone(),
                    _ => {
                        // For Renamed changes, derive the replacement member
                        // from the `after` field (e.g., "splitButtonItems" or
                        // "MenuToggleProps.splitButtonItems").
                        if change.change == ApiChangeType::Renamed {
                            if let Some(ref after) = change.after {
                                after.rsplit('.').next().unwrap_or(after).to_string()
                            } else {
                                continue;
                            }
                        } else {
                            continue;
                        }
                    }
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
                        // Keep the ReplacedByMember disposition so downstream
                        // rule generation can include migration guidance
                        // pointing to the replacement prop. The change type is
                        // SignatureChanged (not Removed), so the fix engine
                        // will use LLM-assisted fixing instead of a mechanical
                        // rename codemod.
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
                                        qualified_name: String::new(),
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
        extensions: crate::extensions::TsAnalysisExtensions::default(),
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
    old_surface: &ApiSurface,
    new_surface: &ApiSurface,
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
    let _old_by_qn: HashMap<&str, &Symbol> = old_surface
        .symbols
        .iter()
        .map(|s| (s.qualified_name.as_str(), s))
        .collect();

    let _new_by_qn: HashMap<&str, &Symbol> = new_surface
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
                TypeStatus::Modified
            } else {
                TypeStatus::Removed
            }
        } else if removal_ratio > 0.5 && removed >= 3 {
            TypeStatus::Removed
        } else {
            TypeStatus::Modified
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
                    if let Some(props_target) = c
                        .migration_target
                        .as_ref()
                        .filter(|_| c.symbol == props_name)
                    {
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

        let summary = TypeSummary {
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
            language_data: TsReportData {
                child_components,
                expected_children: Vec::new(),
            },
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

        // Also exclude constants whose companion Props interface has any
        // breaking change. These components already get individual rules
        // from the TypeSummary built in Step 4, so including them in a
        // combined constant group creates redundant duplicate incidents.
        let props_with_breaking_changes: Vec<String> = structural_changes
            .iter()
            .filter(|c| {
                c.is_breaking
                    && c.symbol.ends_with("Props")
                    && matches!(c.kind, SymbolKind::Interface | SymbolKind::TypeAlias)
            })
            .filter_map(|c| c.symbol.strip_suffix("Props").map(|s| s.to_string()))
            .collect();
        set.extend(props_with_breaking_changes);

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
        // Skip import-path relocations — these are component variables whose
        // import path changed (e.g., Chart moved from @patternfly/react-charts
        // to @patternfly/react-charts/victory). They need individual rules
        // with the specific new import path, not a generic collapsed group.
        if matches!(change.change_type, StructuralChangeType::Relocated { .. }) {
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
            "LlmAssisted".to_string()
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

    enrich_cross_family_absorption(&mut package_map);

    package_map.into_values().collect()
}

// ─── Cross-family absorption enrichment ──────────────────────────────────

/// After all TypeSummaries are built, cross-check child components with
/// empty absorbed_members against removed props from OTHER family members.
///
/// `discover_child_components` uses name-prefix matching to assign children
/// to parents. This means a child like `MastheadLogo` (starts with
/// "Masthead") gets assigned to `Masthead`, not `MastheadBrand`. But
/// `MastheadBrand.component` was removed and `MastheadLogo.component`
/// was added — the absorption belongs to MastheadBrand, not Masthead.
///
/// This enrichment pass fixes that by checking all family members' removed
/// props against each un-absorbed child's known_members. When cross-family
/// absorption is found, the child is moved to the correct parent and its
/// absorbed_members are updated.
fn enrich_cross_family_absorption(package_map: &mut BTreeMap<String, PackageChanges<TypeScript>>) {
    for pkg in package_map.values_mut() {
        // Group type summary indices by family directory (parent of source_file).
        let mut family_groups: HashMap<PathBuf, Vec<usize>> = HashMap::new();
        for (idx, ts) in pkg.type_summaries.iter().enumerate() {
            if let Some(src) = ts.source_files.first() {
                if let Some(dir) = src.parent() {
                    family_groups
                        .entry(dir.to_path_buf())
                        .or_default()
                        .push(idx);
                }
            }
        }

        // For each family with >1 members, cross-check absorption.
        // Collect move operations first, then apply them.
        struct MoveOp {
            child: ChildComponent,
            from_host_idx: usize,
            child_pos: usize,
            to_host_idx: usize,
        }
        let mut moves: Vec<MoveOp> = Vec::new();

        for indices in family_groups.values() {
            if indices.len() < 2 {
                continue;
            }

            // Build a map: removed_member_name -> [(ts_index, ts_name)]
            let mut removed_by_member: HashMap<&str, Vec<(usize, &str)>> = HashMap::new();
            for &idx in indices {
                let ts = &pkg.type_summaries[idx];
                for rm in &ts.removed_members {
                    removed_by_member
                        .entry(rm.name.as_str())
                        .or_default()
                        .push((idx, ts.name.as_str()));
                }
            }

            // Find children with 0 absorbed members and cross-check.
            for &host_idx in indices {
                let ts = &pkg.type_summaries[host_idx];
                for (child_pos, child) in ts.language_data.child_components.iter().enumerate() {
                    if child.status != ChildComponentStatus::Added
                        || !child.absorbed_members.is_empty()
                    {
                        continue;
                    }

                    // Check child's known_members against all family members'
                    // removed props. Collect matches per potential absorbing parent.
                    let mut best_parent: Option<(usize, Vec<String>)> = None;

                    for member_name in &child.known_members {
                        // Skip ubiquitous props that don't indicate absorption.
                        if member_name == "children" || member_name == "className" {
                            continue;
                        }
                        if let Some(sources) = removed_by_member.get(member_name.as_str()) {
                            for &(src_idx, _) in sources {
                                if src_idx != host_idx {
                                    let entry =
                                        best_parent.get_or_insert_with(|| (src_idx, Vec::new()));
                                    if entry.0 == src_idx {
                                        entry.1.push(member_name.clone());
                                    }
                                }
                            }
                        }
                    }

                    if let Some((to_idx, mut absorbed)) = best_parent {
                        absorbed.sort();
                        let mut updated_child = child.clone();
                        updated_child.absorbed_members = absorbed.clone();

                        tracing::info!(
                            child = %updated_child.name,
                            from_parent = %ts.name,
                            to_parent = %pkg.type_summaries[to_idx].name,
                            absorbed = ?absorbed,
                            "Cross-family absorption: moving child to correct parent"
                        );

                        moves.push(MoveOp {
                            child: updated_child,
                            from_host_idx: host_idx,
                            child_pos,
                            to_host_idx: to_idx,
                        });
                    }
                }
            }
        }

        // Apply move operations: add to new parent, then remove from old.
        // Process removals in reverse order within each host to preserve indices.
        for mv in &moves {
            pkg.type_summaries[mv.to_host_idx]
                .language_data
                .child_components
                .push(mv.child.clone());
        }

        // Sort removals by (from_host_idx, child_pos) descending to avoid
        // index shifting problems.
        let mut removals: Vec<(usize, usize)> = moves
            .iter()
            .map(|mv| (mv.from_host_idx, mv.child_pos))
            .collect();
        removals.sort_by(|a, b| b.cmp(a));
        for (host_idx, child_pos) in removals {
            pkg.type_summaries[host_idx]
                .language_data
                .child_components
                .remove(child_pos);
        }
    }
}

// ─── Child component discovery ───────────────────────────────────────────

/// Discover child/sibling components for a given parent component.
#[allow(clippy::too_many_arguments)]
fn discover_child_components(
    component_name: &str,
    parent_qn: &str,
    old_surface: &ApiSurface,
    new_surface: &ApiSurface,
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
/// `expected_children` on each `TypeSummary`.
fn enrich_hierarchy_deltas(
    report: &mut AnalysisReport<TypeScript>,
    mut deltas: Vec<HierarchyDelta>,
    new_surface: &ApiSurface,
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

    // Populate expected_children on TypeSummary entries from the FULL
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
                            if !comp
                                .language_data
                                .expected_children
                                .iter()
                                .any(|e| e.name == ec.name)
                            {
                                comp.language_data.expected_children.push(ec.clone());
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
                    report.packages[idx].type_summaries.push(TypeSummary {
                        name: comp_name.clone(),
                        definition_name: format!("{}Props", comp_name),
                        status: TypeStatus::Modified,
                        member_summary: MemberSummary::default(),
                        removed_members: vec![],
                        type_changes: vec![],
                        migration_target: None,
                        behavioral_changes: vec![],
                        language_data: TsReportData {
                            child_components: vec![],
                            expected_children: expected,
                        },
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
                if !comp.language_data.expected_children.is_empty() {
                    comp_children.insert(
                        comp.name.clone(),
                        comp.language_data.expected_children.clone(),
                    );
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
                if !comp.language_data.expected_children.is_empty() {
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
                            if !comp
                                .language_data
                                .expected_children
                                .iter()
                                .any(|e| e.name == ec.name)
                            {
                                comp.language_data.expected_children.push(ec.clone());
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
                if comp.language_data.expected_children.is_empty() {
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

                for child in &mut comp.language_data.expected_children {
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
        let mut deprecated_families: HashMap<String, Vec<&TypeSummary<TypeScript>>> =
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
            // Find the TypeSummary that has expected_children for
            // the new composition structure. This is the main module's entry
            // (or the sole entry when deprecated inherits expected_children).
            let main_comp = report
                .packages
                .iter()
                .flat_map(|pkg| &pkg.type_summaries)
                .find(|c| c.name == *family && !c.language_data.expected_children.is_empty());

            // Build added_children from the main module's expected_children
            let new_children: Vec<ExpectedChild> = main_comp
                .map(|mc| mc.language_data.expected_children.clone())
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
                                for ec in &comp.language_data.expected_children {
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
                if !comp.language_data.expected_children.is_empty() {
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
                    comp.language_data.expected_children = children;
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

// ─── SD-based prop replacement detection ─────────────────────────────────

/// Enrich `removal_disposition` on removed prop entries using SD pipeline data.
///
/// The TD pipeline's rename detector uses type fingerprinting and name similarity
/// to match removed/added props as renames. Some legitimate renames miss the
/// thresholds:
///
/// - `isActive → isClicked` (0.444 similarity, needs 0.45 for primitive-ambiguous)
/// - `border → isBordered` (0.500 similarity, needs 0.60 for name-only pass)
/// - `variant → color` (0.143 similarity, fundamentally a prop split)
///
/// The SD pipeline has per-component data that provides additional correlation
/// signals: `prop_style_bindings` (which props drive which CSS modifiers),
/// `old/new_component_props` (removed/added prop sets), and prop type maps.
///
/// This function uses three tiers of matching:
///
/// **Tier 1 (1:1 ratio):** Single removed prop + single added prop on a
/// component → strong rename signal (Avatar `border → isBordered`).
///
/// **Tier 2 (CSS binding):** Removed prop had a `prop_style_bindings` entry
/// and one or more added props also have bindings. Score by combined prop name +
/// modifier name similarity, pick the best (Button `isActive → isClicked`).
///
/// **Tier 3 (1:N prop split):** Single removed enum prop with multiple added
/// props. Match old values' semantic domain to new prop names
/// (Banner `variant → color`).
fn enrich_removal_dispositions_from_sd(report: &mut AnalysisReport<TypeScript>) {
    let sd = match report.extensions.sd_result {
        Some(ref sd) => sd,
        None => return,
    };

    // ── Step 0: Verify TD renames using CSS binding continuity ────────
    //
    // The TD pipeline's rename detector pairs removed/added props by type
    // fingerprint and name similarity. It can produce false renames when
    // multiple new props have the same type (e.g., boolean) and one happens
    // to pass the similarity threshold despite being unrelated.
    //
    // Example: Label `isOverflowLabel → isClickable` is a false rename.
    // The SD profiles show that `isOverflowLabel` controlled CSS modifier
    // `overflow`, which still exists in v6 (now via `variant="overflow"`).
    // Meanwhile `isClickable` controls a different modifier (`clickable`).
    //
    // Detection: a TD rename is invalidated when:
    //   1. The old prop had a prop_style_binding (CSS modifier)
    //   2. That modifier still exists in the new version's BEM modifiers
    //   3. The supposed rename target has a DIFFERENT CSS modifier
    //
    // When invalidated, reclassify from Renamed → Removed so downstream
    // generates RemoveProp instead of a wrong Rename codemod.
    {
        let mut invalidated = 0usize;
        for fc in report.changes.iter_mut() {
            for change in fc.breaking_api_changes.iter_mut() {
                if change.change != ApiChangeType::Renamed {
                    continue;
                }
                if !matches!(change.kind, ApiChangeKind::Property | ApiChangeKind::Field) {
                    continue;
                }
                let (comp, old_prop) = match change.symbol.rsplit_once('.') {
                    Some((c, p)) => (c, p),
                    None => continue,
                };
                let new_prop = match change.after.clone() {
                    Some(a) => a,
                    None => continue,
                };

                // Look up old prop's CSS modifier — first from prop_style_bindings,
                // then fall back to deriving from the prop name (strip is/has/use prefix).
                let old_profile = match sd.old_profiles.get(comp) {
                    Some(p) => p,
                    None => continue,
                };
                let old_modifier = old_profile
                    .prop_style_bindings
                    .get(old_prop)
                    .and_then(extract_css_modifier_name)
                    .or_else(|| prop_to_bem_modifier(old_prop));
                let old_modifier = match old_modifier {
                    Some(m) => m,
                    None => continue, // Can't derive modifier name → can't verify
                };

                // Check if old modifier still exists in new BEM modifiers
                let new_profile = match sd.new_profiles.get(comp) {
                    Some(p) => p,
                    None => continue,
                };
                // ── CSS resolved-value validation ────────────────────────
                //
                // Whether the old modifier was removed or survives in v6 BEM,
                // compare the resolved CSS effects of the old and new modifiers.
                // If both have CSS modifier data and there's zero overlap in
                // normalized property keys → the modifiers affect completely
                // different CSS properties → false rename.
                match check_css_resolved_value_mismatch(sd, old_profile, old_prop, &new_prop) {
                    CssRenameVerdict::Invalidate(reason) => {
                        tracing::info!(
                            component = comp,
                            old_prop = old_prop,
                            false_target = &new_prop,
                            reason = %reason,
                            "Invalidating TD rename: CSS resolved-value comparison \
                             shows old and new modifiers affect different CSS properties"
                        );
                        change.change = ApiChangeType::Removed;
                        change.after = None;
                        change.before = Some(format!("property: {}: boolean", old_prop));
                        change.removal_disposition = None;
                        change.description = format!(
                            "property `{}` was removed from `{}` \
                             (TD rename to `{}` invalidated: {})",
                            old_prop, comp, new_prop, reason
                        );
                        invalidated += 1;
                        continue;
                    }
                    CssRenameVerdict::Validate => {
                        // CSS confirms the modifiers affect the same properties.
                        // The rename is valid regardless of BEM modifier survival.
                        continue;
                    }
                    CssRenameVerdict::Inconclusive => {
                        // No CSS data available. Fall back to BEM modifier survival check.
                    }
                }

                if !new_profile.bem_modifiers.contains(&old_modifier) {
                    continue; // Modifier was removed, CSS check inconclusive → allow rename
                }

                // Old modifier survives in v6 BEM. Check if the rename target
                // maps to the same modifier (continuity).
                let new_css_tokens = new_profile.prop_style_bindings.get(&new_prop);
                let new_modifier_from_bindings = new_css_tokens.and_then(extract_css_modifier_name);

                if new_modifier_from_bindings.as_deref() == Some(old_modifier.as_str()) {
                    continue; // Same modifier → rename is valid (modifier continuity)
                }

                // Different (or absent) modifier on the rename target, but the old
                // modifier still exists in BEM → false rename. Invalidate.
                tracing::info!(
                    component = comp,
                    old_prop = old_prop,
                    false_target = new_prop,
                    old_modifier = %old_modifier,
                    new_modifier = ?new_modifier_from_bindings,
                    "Invalidating TD rename: old CSS modifier still exists in new BEM, \
                     rename target has different modifier"
                );

                // Check if the old modifier name was absorbed as a new value
                // on an existing enum prop (e.g., variant gained 'overflow').
                let absorbed_by = sd
                    .old_component_prop_types
                    .get(comp)
                    .and_then(|old_types| {
                        sd.new_component_prop_types.get(comp).and_then(|new_types| {
                            // Look for props that exist in both versions where the
                            // new version's type gained the old modifier as a value
                            let old_prop_set: HashSet<&String> = sd
                                .old_component_props
                                .get(comp)
                                .map(|p| p.iter().collect())
                                .unwrap_or_default();
                            let new_prop_set: HashSet<&String> = sd
                                .new_component_props
                                .get(comp)
                                .map(|p| p.iter().collect())
                                .unwrap_or_default();

                            // Only consider props that exist in both versions
                            for shared_prop in old_prop_set.intersection(&new_prop_set) {
                                let old_type = old_types.get(shared_prop.as_str());
                                let new_type = new_types.get(shared_prop.as_str());

                                if let (Some(ot), Some(nt)) = (old_type, new_type) {
                                    // Check if new type has the modifier as a value
                                    // that the old type didn't have
                                    if nt.contains('|') && nt.contains(&old_modifier) {
                                        let old_has = ot.contains(&old_modifier);
                                        if !old_has {
                                            return Some((
                                                shared_prop.to_string(),
                                                old_modifier.clone(),
                                            ));
                                        }
                                    }
                                }
                            }
                            None
                        })
                    });

                change.change = ApiChangeType::Removed;
                change.after = None;

                if let Some((absorbing_prop, value)) = absorbed_by {
                    // The old boolean was absorbed as a value on an existing enum prop.
                    // Format: isOverflowLabel → variant="overflow"
                    // Set before to the old prop name (not quoted → hits prop rename path)
                    // and ReplacedByMember with the value expression.
                    let replacement = format!("{}=\"{}\"", absorbing_prop, value);
                    tracing::info!(
                        component = comp,
                        old_prop = old_prop,
                        replacement = %replacement,
                        "Prop-to-value absorption detected: boolean prop absorbed \
                         as enum value on existing prop"
                    );
                    change.before = Some(old_prop.to_string());
                    change.removal_disposition = Some(RemovalDisposition::ReplacedByMember {
                        new_member: replacement,
                    });
                    change.description = format!(
                        "property `{}` was removed from `{}`: \
                         use `{}=\"{}\"` instead",
                        old_prop, comp, absorbing_prop, value
                    );
                } else {
                    change.before = Some(format!("property: {}: boolean", old_prop));
                    change.removal_disposition = None;
                    change.description = format!(
                        "property `{}` was removed from `{}` \
                         (TD rename to `{}` invalidated by CSS binding analysis)",
                        old_prop, comp, new_prop
                    );
                }
                invalidated += 1;
            }
        }
        if invalidated > 0 {
            tracing::info!(
                count = invalidated,
                "Invalidated TD renames via CSS binding verification"
            );
        }
    }

    // ── Step 1: Build per-component removed/added prop sets ───────────
    let mut matches: Vec<(String, String, String)> = Vec::new(); // (component, old_prop, new_prop)

    // Clone member_renames to avoid borrow conflict with report.changes
    let member_renames = report.member_renames.clone();

    for (component, old_props) in &sd.old_component_props {
        // Look up new props: first try same component name, then check
        // if the component was renamed and look up under the new name.
        let new_props = match sd.new_component_props.get(component) {
            Some(p) => p,
            None => match member_renames.get(component) {
                Some(new_name) => match sd.new_component_props.get(new_name) {
                    Some(p) => p,
                    None => continue,
                },
                None => continue,
            },
        };

        let old_set: HashSet<&String> = old_props.iter().collect();
        let new_set: HashSet<&String> = new_props.iter().collect();
        let removed: Vec<&String> = old_set.difference(&new_set).copied().collect();
        let added: Vec<&String> = new_set.difference(&old_set).copied().collect();

        if removed.is_empty() || added.is_empty() {
            continue;
        }

        // Step 2: Filter out props already matched by TD rename detector.
        // Props with a removal_disposition are excluded (already handled).
        // Renamed props are included in the matching pool so the N:M greedy
        // matcher can detect and fix duplicate rename targets (TC028).
        let already_matched: HashSet<String> = report
            .changes
            .iter()
            .flat_map(|fc| fc.breaking_api_changes.iter())
            .filter(|c| {
                let comp = c.symbol.rsplit_once('.').map(|(c, _)| c).unwrap_or("");
                comp == component && c.removal_disposition.is_some()
            })
            .filter_map(|c| c.symbol.rsplit_once('.').map(|(_, p)| p.to_string()))
            .collect();

        // Collect TD rename targets for this component. If two props
        // share the same target, both need to go through the matcher.
        let mut rename_target_counts: HashMap<String, Vec<String>> = HashMap::new();
        for fc in report.changes.iter() {
            for c in &fc.breaking_api_changes {
                let comp = c.symbol.rsplit_once('.').map(|(co, _)| co).unwrap_or("");
                if comp == component && c.change == ApiChangeType::Renamed {
                    if let Some(target) = c.after.as_ref() {
                        let prop = c.symbol.rsplit_once('.').map(|(_, p)| p.to_string())
                            .unwrap_or_default();
                        rename_target_counts
                            .entry(target.clone())
                            .or_default()
                            .push(prop);
                    }
                }
            }
        }

        // Props involved in duplicate rename targets need re-matching
        let duplicate_rename_props: HashSet<String> = rename_target_counts
            .values()
            .filter(|props| props.len() > 1)
            .flatten()
            .cloned()
            .collect();

        // Non-duplicate renamed props: exclude from matching, pre-seed their
        // targets as used so the matcher doesn't reassign them
        let mut rename_preseed: HashSet<String> = HashSet::new();
        for (target, props) in &rename_target_counts {
            if props.len() == 1 {
                rename_preseed.insert(target.clone());
            }
        }

        let mut unmatched_removed: Vec<&String> = removed
            .iter()
            .filter(|p| {
                !already_matched.contains(p.as_str())
                    && (
                        // Include if it's a Removed prop without disposition
                        !rename_target_counts.values().any(|props| props.contains(p))
                        // OR if it's a Renamed prop involved in a duplicate
                        || duplicate_rename_props.contains(p.as_str())
                    )
            })
            .copied()
            .collect();

        // Also add duplicate-renamed props that aren't in the removed set
        // (they were renamed, so they're in old_props but not in the removed
        // diff because the new name exists in new_props too)
        let removed_set: HashSet<&str> = removed.iter().map(|s| s.as_str()).collect();
        let extra_renamed: Vec<String> = duplicate_rename_props
            .iter()
            .filter(|p| !removed_set.contains(p.as_str()) && !already_matched.contains(p.as_str()))
            .cloned()
            .collect();

        // We need owned references for the extra renamed props
        let mut unmatched_removed_owned: Vec<String> = unmatched_removed
            .iter()
            .map(|s| s.to_string())
            .collect();
        unmatched_removed_owned.extend(extra_renamed);

        // Rebuild unmatched_removed from owned data
        unmatched_removed = unmatched_removed_owned.iter().collect();

        let unmatched_added: Vec<&String> = added.clone();

        if unmatched_removed.is_empty() {
            continue;
        }

        // Get profiles for CSS binding data
        let old_profile = sd.old_profiles.get(component);
        let new_profile = sd.new_profiles.get(component);
        let old_bindings = old_profile
            .map(|p| &p.prop_style_bindings)
            .cloned()
            .unwrap_or_default();
        let new_bindings = new_profile
            .map(|p| &p.prop_style_bindings)
            .cloned()
            .unwrap_or_default();

        // Track which added props have been used (greedy matching).
        // Pre-seed with non-duplicate rename targets so the matcher
        // doesn't reassign props that are already correctly matched.
        let mut used_added: HashSet<String> = rename_preseed.clone();

        // ── Tier 1: N:M greedy matching with augmented similarity ────────
        //
        // Score every (removed, added) pair using augmented name similarity
        // (with boolean prefix stripping and component prefix stripping).
        // When types are incompatible (boolean↔enum), require higher
        // similarity threshold (0.6) to compensate for the type mismatch.
        // When types are compatible or unavailable, use lower threshold (0.4).
        //
        // Greedy assignment: sort by score descending, pick the best
        // unmatched pair, repeat. This handles 1:1, N:N, and partial matches.
        {
            let old_types_map = sd.old_component_prop_types.get(component);
            let new_types_map = sd
                .new_component_prop_types
                .get(component)
                .or_else(|| {
                    member_renames
                        .get(component)
                        .and_then(|new_name| sd.new_component_prop_types.get(new_name))
                });

            let mut candidates: Vec<(&String, &String, f64)> = Vec::new();
            for rem in &unmatched_removed {
                for add in &unmatched_added {
                    let sim = augmented_prop_similarity_with_component(rem, add, component);

                    // Type compatibility check with relaxation:
                    // - Types compatible or unknown: threshold 0.4
                    // - Types incompatible (boolean↔enum): threshold 0.5
                    let old_type = old_types_map.and_then(|m| m.get(rem.as_str()));
                    let new_type = new_types_map.and_then(|m| m.get(add.as_str()));
                    let types_compatible = match (old_type, new_type) {
                        (Some(ot), Some(nt)) => prop_types_compatible(ot, nt),
                        _ => true, // No type info: assume compatible
                    };
                    let threshold = if types_compatible { 0.4 } else { 0.5 };

                    if sim >= threshold {
                        // Apply a ranking penalty for type-incompatible
                        // matches so that type-compatible candidates win
                        // when scores are close. This prevents cases like
                        // isActive→state (sim=0.50, incompatible) beating
                        // isActive→isClicked (sim=0.44, compatible). (TC008)
                        let ranking_score = if types_compatible {
                            sim
                        } else {
                            sim * 0.8
                        };
                        candidates.push((rem, add, ranking_score));
                    }
                }
            }

            // Sort by score descending for greedy assignment
            candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

            let mut used_removed: HashSet<&str> = HashSet::new();
            for (rem, add, sim) in &candidates {
                if used_removed.contains(rem.as_str()) || used_added.contains(add.as_str()) {
                    continue;
                }
                tracing::info!(
                    component = %component,
                    removed = %rem,
                    added = %add,
                    similarity = sim,
                    tier = 1,
                    "SD prop replacement: greedy N:M match"
                );
                matches.push((
                    component.clone(),
                    rem.to_string(),
                    add.to_string(),
                ));
                used_added.insert(add.to_string());
                used_removed.insert(rem.as_str());
            }

            // ── Residual matching: last-resort pairing for leftovers ────
            // After greedy assignment, if there are remaining unmatched
            // removed and added props AND the greedy pass already matched
            // at least one pair (establishing a naming pattern), try
            // pairing leftovers with a lower threshold (0.25).
            // This catches cases like errorDescription→bodyText (sim=0.27)
            // after errorTitle→titleText has already been assigned.
            let leftover_removed: Vec<&&String> = unmatched_removed
                .iter()
                .filter(|r| !used_removed.contains(r.as_str()))
                .collect();
            let leftover_added: Vec<&&String> = unmatched_added
                .iter()
                .filter(|a| !used_added.contains(a.as_str()))
                .collect();

            // Only do residual matching if the primary greedy pass
            // already matched at least one pair (evidence of naming pattern)
            if !used_removed.is_empty()
                && !leftover_removed.is_empty()
                && !leftover_added.is_empty()
            {
                let mut residual: Vec<(&&String, &&String, f64)> = Vec::new();
                for rem in &leftover_removed {
                    for add in &leftover_added {
                        let sim = augmented_prop_similarity_with_component(rem, add, component);
                        if sim >= 0.2 {
                            // Only check type compatibility as a hard veto for
                            // truly incompatible pairs (boolean vs named interface)
                            let old_type = old_types_map.and_then(|m| m.get(rem.as_str()));
                            let new_type = new_types_map.and_then(|m| m.get(add.as_str()));
                            let type_vetoed = matches!(
                                (old_type, new_type),
                                (Some(ot), Some(nt)) if !prop_types_compatible(ot, nt)
                            ) && sim < 0.5;
                            if !type_vetoed {
                                residual.push((rem, add, sim));
                            }
                        }
                    }
                }
                residual.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
                let mut res_used_rem: HashSet<&str> = HashSet::new();
                for (rem, add, sim) in &residual {
                    if res_used_rem.contains(rem.as_str()) || used_added.contains(add.as_str()) {
                        continue;
                    }
                    tracing::info!(
                        component = %component,
                        removed = %rem,
                        added = %add,
                        similarity = sim,
                        tier = "1-residual",
                        "SD prop replacement: residual match"
                    );
                    matches.push((
                        component.clone(),
                        rem.to_string(),
                        add.to_string(),
                    ));
                    used_added.insert(add.to_string());
                    res_used_rem.insert(rem.as_str());
                    used_removed.insert(rem.as_str());
                }
            }

            // Skip to next component if all removed props are matched
            if used_removed.len() == unmatched_removed.len() {
                continue;
            }
        }

        for removed_prop in &unmatched_removed {
            // Skip props already matched by Tier 1 greedy
            if matches.iter().any(|(c, r, _)| c == component && r == removed_prop.as_str()) {
                continue;
            }

            // Tier 2: CSS binding correlation
            let old_css = old_bindings.get(removed_prop.as_str());
            if let Some(old_css_tokens) = old_css {
                let old_modifier = extract_css_modifier_name(old_css_tokens);

                let mut best_candidate: Option<(&String, f64)> = None;
                for added_prop in &unmatched_added {
                    if used_added.contains(added_prop.as_str()) {
                        continue;
                    }
                    let new_css = new_bindings.get(added_prop.as_str());
                    if let Some(new_css_tokens) = new_css {
                        let new_modifier = extract_css_modifier_name(new_css_tokens);
                        let prop_sim =
                            semver_analyzer_core::diff::name_similarity(removed_prop, added_prop);
                        let mod_sim = match (&old_modifier, &new_modifier) {
                            (Some(om), Some(nm)) => {
                                semver_analyzer_core::diff::name_similarity(om, nm)
                            }
                            _ => 0.0,
                        };
                        let combined = prop_sim + mod_sim;
                        if combined > best_candidate.map(|(_, s)| s).unwrap_or(0.0) {
                            best_candidate = Some((added_prop, combined));
                        }
                    }
                }

                // Require minimum combined score to avoid false matches
                if let Some((best_prop, score)) = best_candidate {
                    if score >= 0.3 {
                        tracing::info!(
                            component = %component,
                            removed = %removed_prop,
                            added = %best_prop,
                            score = score,
                            tier = 2,
                            "SD prop replacement: CSS binding match"
                        );
                        matches.push((
                            component.clone(),
                            removed_prop.to_string(),
                            best_prop.to_string(),
                        ));
                        used_added.insert(best_prop.to_string());
                        continue;
                    }
                }
            }

            // Tier 3: 1:N prop split (single removed enum prop, multiple added)
            if unmatched_removed.len() == 1 && unmatched_added.len() >= 2 {
                let old_types = sd.old_component_prop_types.get(component);
                let new_types = sd.new_component_prop_types.get(component);
                if let Some(old_type) = old_types.and_then(|t| t.get(removed_prop.as_str())) {
                    if old_type.contains('|') {
                        // Parse the old enum values
                        let old_values = parse_union_string_values(old_type);
                        if !old_values.is_empty() {
                            let mut candidates: Vec<(&String, f64)> = Vec::new();
                            for added_prop in &unmatched_added {
                                if used_added.contains(added_prop.as_str()) {
                                    continue;
                                }
                                // Signal 1: old values contain the added prop name
                                // or vice versa (e.g., "blue" matches domain of "color")
                                let prop_lower = added_prop.to_lowercase();
                                let semantic_hits = old_values
                                    .iter()
                                    .filter(|v| {
                                        let vl = v.to_lowercase();
                                        vl.contains(&prop_lower) || prop_lower.contains(&vl)
                                    })
                                    .count();
                                let semantic_ratio =
                                    semantic_hits as f64 / old_values.len() as f64;

                                // Signal 2: new prop's type name matches the old prop
                                // name pattern. E.g., old prop "variant" with new type
                                // "BannerColor" → the type encodes the replacement domain.
                                // Check if the new type is a named enum (not inline union)
                                // that shares a prefix with the component name.
                                let type_signal = if let Some(new_type) =
                                    new_types.and_then(|t| t.get(added_prop.as_str()))
                                {
                                    // New type is a named reference (e.g., BannerColor) —
                                    // check if it starts with the component name
                                    if !new_type.contains('|')
                                        && !new_type.contains('{')
                                        && new_type
                                            .starts_with(component.as_str())
                                    {
                                        // The type references are specific to this component
                                        // (BannerColor, BannerStatus). Prefer the one that
                                        // is the "primary" replacement — the one whose
                                        // prop name is more general. Use a small bonus.
                                        0.1
                                    } else {
                                        0.0
                                    }
                                } else {
                                    0.0
                                };

                                // Name similarity is intentionally NOT used here.
                                // In prop splits, the old and new prop names are
                                // semantically different ("variant" → "color"/"status")
                                // and name similarity is misleading (it would prefer
                                // "status" over "color" for "variant"). Instead we rely
                                // on semantic_ratio, type_signal, and alphabetical
                                // tiebreaker for deterministic results.
                                let score = semantic_ratio + type_signal;
                                candidates.push((added_prop, score));
                            }

                            // Sort by score descending, then alphabetically for ties.
                            // Alphabetical tiebreaker ensures deterministic results and
                            // tends to prefer simpler/shorter names (e.g., "color" < "status").
                            candidates.sort_by(|a, b| {
                                b.1.partial_cmp(&a.1)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                                    .then_with(|| a.0.cmp(b.0))
                            });

                            if let Some((best_prop, _score)) = candidates.first() {
                                tracing::info!(
                                    component = %component,
                                    removed = %removed_prop,
                                    added = %best_prop,
                                    tier = 3,
                                    "SD prop replacement: 1:N prop split match"
                                );
                                matches.push((
                                    component.clone(),
                                    removed_prop.to_string(),
                                    best_prop.to_string(),
                                ));
                                used_added.insert(best_prop.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // Step 4: Apply matches to the report's ApiChange entries.
    if matches.is_empty() {
        return;
    }

    let match_map: HashMap<(String, String), String> = matches
        .into_iter()
        .map(|(comp, old, new)| ((comp, old), new))
        .collect();

    let mut enriched = 0usize;
    let mut renamed_fixed = 0usize;
    for fc in report.changes.iter_mut() {
        for change in fc.breaking_api_changes.iter_mut() {
            if !matches!(change.kind, ApiChangeKind::Property | ApiChangeKind::Field) {
                continue;
            }
            if let Some((comp, prop)) = change.symbol.rsplit_once('.') {
                let key = (comp.to_string(), prop.to_string());
                if let Some(new_member) = match_map.get(&key) {
                    if change.change == ApiChangeType::Removed
                        && change.removal_disposition.is_none()
                    {
                        // Standard case: set disposition on Removed changes
                        change.removal_disposition =
                            Some(RemovalDisposition::ReplacedByMember {
                                new_member: new_member.clone(),
                            });
                        enriched += 1;
                    } else if change.change == ApiChangeType::Renamed {
                        // Fix duplicate TD rename targets: update the
                        // rename target if the matcher found a better one
                        let current_target = change.after.as_deref().unwrap_or("");
                        if current_target != new_member {
                            tracing::info!(
                                symbol = %change.symbol,
                                old_target = current_target,
                                new_target = %new_member,
                                "SD enrichment: correcting duplicate TD rename target"
                            );
                            change.after = Some(new_member.clone());
                            // Update description to reflect the corrected target
                            if let Some(old_name) = change.symbol.rsplit_once('.').map(|(_, p)| p) {
                                change.description = format!(
                                    "property `{}` was renamed to `{}`",
                                    old_name, new_member
                                );
                            }
                            renamed_fixed += 1;
                        }
                    }
                }
            }
        }
    }

    if enriched > 0 || renamed_fixed > 0 {
        tracing::info!(
            enriched = enriched,
            renamed_fixed = renamed_fixed,
            "Enriched removal_disposition from SD prop replacement detection"
        );
    }
}

/// Extract the CSS modifier name from a set of CSS tokens.
///
/// Given `{"styles.modifiers.active"}`, returns `Some("active")`.
/// Check if two prop type strings are structurally compatible for
/// 1:1 replacement inference in the Tier 1 matcher.
///
/// Rejects pairings where one type is a simple primitive (`boolean`,
/// `string`, etc.) and the other is a complex/named type (interface
/// reference, object literal, array, function). This prevents false
/// renames like `isHidden: boolean` → `contentBodyProps: AccordionExpandableContentBodyProps`.
///
/// Union types (`'dark' | 'light'`) are treated as non-primitive since
/// they have specific domain semantics.
fn prop_types_compatible(old_type: &str, new_type: &str) -> bool {
    const PRIMITIVES: &[&str] = &[
        "boolean", "string", "number", "void", "null",
        "undefined", "never", "any", "unknown",
    ];
    fn is_primitive(t: &str) -> bool {
        let trimmed = t.trim();
        PRIMITIVES.iter().any(|p| trimmed.eq_ignore_ascii_case(p))
    }

    let old_prim = is_primitive(old_type);
    let new_prim = is_primitive(new_type);

    // Reject when one is primitive and the other is not
    // (e.g., boolean vs AccordionExpandableContentBodyProps)
    if old_prim != new_prim {
        return false;
    }

    true
}

/// Boolean prefix patterns commonly used in React prop names.
const BOOLEAN_PREFIXES: &[&str] = &["is", "has", "use", "with", "should", "can"];

/// Strip a boolean prefix (is/has/use/with/should/can) from a prop name.
/// Returns the remainder with its first character lowercased.
/// E.g., "isBordered" → "bordered", "usePageInsets" → "pageInsets".
/// If no prefix matches, returns the original name.
fn strip_boolean_prefix(name: &str) -> String {
    for prefix in BOOLEAN_PREFIXES {
        if name.len() > prefix.len() && name.starts_with(prefix) {
            let rest = &name[prefix.len()..];
            // Must start with uppercase (camelCase boundary)
            if rest.starts_with(|c: char| c.is_ascii_uppercase()) {
                // Lowercase the first char of the remainder
                let mut chars = rest.chars();
                let first = chars.next().unwrap().to_ascii_lowercase();
                return format!("{}{}", first, chars.as_str());
            }
        }
    }
    name.to_string()
}

/// Compute augmented name similarity between two prop names.
/// Takes the max of:
/// 1. Raw name similarity
/// 2. Similarity after stripping boolean prefixes from both
fn augmented_prop_similarity(a: &str, b: &str) -> f64 {
    let raw = semver_analyzer_core::diff::name_similarity(a, b);
    let sa = strip_boolean_prefix(a);
    let sb = strip_boolean_prefix(b);
    let stripped = semver_analyzer_core::diff::name_similarity(&sa, &sb);
    raw.max(stripped)
}

/// Like `augmented_prop_similarity` but also tries stripping the component
/// name as a lowercase prefix from prop names.
/// E.g., "errorTitle" on component "ErrorState" → strip "error" → "title".
fn augmented_prop_similarity_with_component(a: &str, b: &str, component: &str) -> f64 {
    let base = augmented_prop_similarity(a, b);

    // Try stripping component-derived prefix.
    // For "ErrorState", try prefix "error" (lowercase of first word before
    // a capitalized boundary). For "NotAuthorized", try "notAuthorized" etc.
    // Simple heuristic: take the component name, lowercase it, see if either
    // prop name starts with that prefix.
    let comp_lower = component[..1].to_lowercase() + &component[1..];

    // Try all camelCase boundary prefixes of the component name
    let mut best = base;
    for i in 1..comp_lower.len() {
        if comp_lower.as_bytes().get(i).is_some_and(|c| c.is_ascii_uppercase()) {
            let prefix = &comp_lower[..i];
            let sa = strip_component_prefix(a, prefix);
            let sb = strip_component_prefix(b, prefix);
            let sim = semver_analyzer_core::diff::name_similarity(&sa, &sb);
            best = best.max(sim);
        }
    }
    // Also try full component name as prefix (lowercase)
    let full_prefix = component.to_lowercase();
    let sa = strip_component_prefix(a, &full_prefix);
    let sb = strip_component_prefix(b, &full_prefix);
    let sim = semver_analyzer_core::diff::name_similarity(&sa, &sb);
    best = best.max(sim);

    best
}

/// Derive a BEM modifier name from a boolean prop name by stripping common
/// prefixes (`is`, `has`, `use`, `should`).
///
/// Returns `None` if no prefix matches (prop doesn't follow the boolean naming
/// convention).
///
/// Examples:
/// - `isActive` → `Some("active")`
/// - `usePageInsets` → `Some("pageInsets")`
/// - `hasNoPadding` → `Some("noPadding")`
/// - `variant` → `None` (no boolean prefix)
fn prop_to_bem_modifier(prop: &str) -> Option<String> {
    for prefix in &["is", "has", "use", "should"] {
        if let Some(rest) = prop.strip_prefix(prefix) {
            if !rest.is_empty() && rest.starts_with(|c: char| c.is_uppercase()) {
                let mut result = String::with_capacity(rest.len());
                let mut chars = rest.chars();
                if let Some(first) = chars.next() {
                    result.push(first.to_ascii_lowercase());
                }
                result.extend(chars);
                return Some(result);
            }
        }
    }
    None
}

/// Convert a camelCase modifier name to a kebab-case CSS class name with `pf-m-` prefix.
///
/// Examples:
/// - `"pageInsets"` → `"pf-m-page-insets"`
/// - `"noPadding"` → `"pf-m-no-padding"`
/// - `"active"` → `"pf-m-active"`
fn modifier_to_css_class(modifier: &str) -> String {
    let mut result = String::from("pf-m-");
    for (i, ch) in modifier.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            result.push('-');
        }
        result.push(ch.to_ascii_lowercase());
    }
    result
}

/// Result of CSS resolved-value comparison for rename validation.
enum CssRenameVerdict {
    /// CSS data confirms the modifiers affect different properties → false rename.
    Invalidate(String),
    /// CSS data confirms the modifiers affect the same properties → true rename.
    Validate,
    /// Insufficient CSS data to make a determination.
    Inconclusive,
}

/// Check whether a proposed boolean prop rename is supported or contradicted
/// by CSS resolved-value comparison.
///
/// Derives BEM modifier names from old/new prop names, looks up their
/// CSS effects from `old_css_modifiers`/`new_css_modifiers`, and compares
/// the normalized resolved override keys. If zero overlap, the modifiers
/// affect completely different CSS properties → false rename.
fn check_css_resolved_value_mismatch(
    sd: &crate::sd_types::SdPipelineResult,
    old_profile: &crate::sd_types::ComponentSourceProfile,
    old_prop: &str,
    new_prop: &str,
) -> CssRenameVerdict {
    // Derive modifier names from prop names
    let (old_modifier, new_modifier) = match (prop_to_bem_modifier(old_prop), prop_to_bem_modifier(new_prop)) {
        (Some(o), Some(n)) => (o, n),
        _ => return CssRenameVerdict::Inconclusive,
    };

    // Get the component's BEM block name for CSS modifier lookup
    let bem_block = match old_profile.bem_block.as_deref() {
        Some(b) if !b.is_empty() => b,
        _ => return CssRenameVerdict::Inconclusive,
    };

    // Look up CSS modifier effects
    let (old_css_mods, new_css_mods) = match (sd.old_css_modifiers.get(bem_block), sd.new_css_modifiers.get(bem_block)) {
        (Some(o), Some(n)) => (o, n),
        _ => return CssRenameVerdict::Inconclusive,
    };

    let old_css_class = modifier_to_css_class(&old_modifier);
    let new_css_class = modifier_to_css_class(&new_modifier);

    let (old_effect, new_effect) = match (old_css_mods.get(&old_css_class), new_css_mods.get(&new_css_class)) {
        (Some(o), Some(n)) => (o, n),
        _ => return CssRenameVerdict::Inconclusive,
    };

    // Normalize resolved override keys: strip --pf-v5-c-{block} / --pf-v6-c-{block}
    // prefix to get the essence (e.g., `__content--PaddingLeft`).
    let strip_prefix = |key: &str| -> String {
        // Pattern: --pf-v{N}-c-{block-name}--{rest} or --pf-v{N}-c-{block}__...
        // We want to strip everything up to and including the block name.
        if let Some(idx) = key.find("--c-") {
            let after_c = &key[idx + 4..]; // skip "--c-"
            // Find the next "--" or "__" after the block name
            if let Some(sep) = after_c.find("--").or_else(|| after_c.find("__")) {
                return after_c[sep..].to_string();
            }
        }
        key.to_string()
    };

    let old_keys: HashSet<String> = old_effect
        .resolved_overrides
        .keys()
        .map(|k| strip_prefix(k))
        .collect();
    let new_keys: HashSet<String> = new_effect
        .resolved_overrides
        .keys()
        .map(|k| strip_prefix(k))
        .collect();

    // If either has no resolved overrides, check direct properties
    if old_keys.is_empty() && new_keys.is_empty() {
        let old_direct: HashSet<&str> = old_effect.direct_properties.keys().map(|s| s.as_str()).collect();
        let new_direct: HashSet<&str> = new_effect.direct_properties.keys().map(|s| s.as_str()).collect();
        if old_direct.is_empty() || new_direct.is_empty() {
            return CssRenameVerdict::Inconclusive;
        }
        let overlap = old_direct.intersection(&new_direct).count();
        if overlap == 0 {
            return CssRenameVerdict::Invalidate(format!(
                "CSS modifiers {} and {} affect completely different CSS properties \
                 (old: {:?}, new: {:?})",
                old_css_class, new_css_class, old_direct, new_direct
            ));
        }
        return CssRenameVerdict::Validate;
    }

    if old_keys.is_empty() || new_keys.is_empty() {
        return CssRenameVerdict::Inconclusive;
    }

    let overlap = old_keys.intersection(&new_keys).count();
    if overlap == 0 {
        CssRenameVerdict::Invalidate(format!(
            "CSS modifiers {} and {} affect completely different CSS properties \
             (old: {:?}, new: {:?})",
            old_css_class, new_css_class, old_keys, new_keys
        ))
    } else {
        CssRenameVerdict::Validate
    }
}

/// Strip a component-derived prefix from a prop name if it matches.
/// E.g., strip_component_prefix("errorTitle", "error") → "title"
fn strip_component_prefix(prop: &str, prefix: &str) -> String {
    let prop_lower = prop.to_lowercase();
    if prop_lower.starts_with(prefix) && prop.len() > prefix.len() {
        let rest = &prop[prefix.len()..];
        // Lowercase the first char of the remainder
        let mut chars = rest.chars();
        let first = chars.next().unwrap().to_ascii_lowercase();
        return format!("{}{}", first, chars.as_str());
    }
    prop.to_string()
}

/// Given `{"styles.modifiers[status]"}`, returns `Some("status")`.
/// Returns `None` if no modifier token is found.
fn extract_css_modifier_name(tokens: &BTreeSet<String>) -> Option<String> {
    for token in tokens {
        if let Some(rest) = token.strip_prefix("styles.modifiers.") {
            return Some(rest.to_string());
        }
        // Handle bracket syntax: styles.modifiers[foo]
        if let Some(rest) = token.strip_prefix("styles.modifiers[") {
            if let Some(name) = rest.strip_suffix(']') {
                return Some(name.to_string());
            }
        }
    }
    None
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
        qualified_name: sc.qualified_name.clone(),
        kind,
        change,
        before: sc.before.clone(),
        after: sc.after.clone(),
        description: sc.description.clone(),
        migration_target: sc.migration_target.clone(),
        removal_disposition: None,
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
        SymbolKind::EnumMember => ApiChangeKind::EnumMember,
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
pub fn count_unique_files(surface: &ApiSurface) -> usize {
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
        BehavioralChange, BehavioralChangeKind, MemberMapping, Signature, SymbolKind, Visibility,
    };
    use std::sync::Arc;

    #[test]
    fn build_report_empty() {
        let results = AnalysisResult {
            structural_changes: Arc::new(vec![]),
            behavioral_changes: vec![],
            manifest_changes: vec![],
            llm_api_changes: vec![],
            old_surface: Arc::new(ApiSurface::default()),
            new_surface: Arc::new(ApiSurface::default()),
            inferred_rename_patterns: None,
            container_changes: vec![],
            extensions: crate::extensions::TsAnalysisExtensions::default(),
            degradation: Arc::new(semver_analyzer_core::diagnostics::DegradationTracker::new()),
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
            source_package: None,
        }];

        let results = AnalysisResult {
            structural_changes: Arc::new(changes),
            behavioral_changes: vec![],
            manifest_changes: manifest,
            llm_api_changes: vec![],
            old_surface: Arc::new(ApiSurface::default()),
            new_surface: Arc::new(ApiSurface::default()),
            inferred_rename_patterns: None,
            container_changes: vec![],
            extensions: crate::extensions::TsAnalysisExtensions::default(),
            degradation: Arc::new(semver_analyzer_core::diagnostics::DegradationTracker::new()),
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
            old_surface: Arc::new(ApiSurface::default()),
            new_surface: Arc::new(ApiSurface::default()),
            inferred_rename_patterns: None,
            container_changes: vec![],
            extensions: crate::extensions::TsAnalysisExtensions::default(),
            degradation: Arc::new(semver_analyzer_core::diagnostics::DegradationTracker::new()),
        };
        let report = build_report(&results, Path::new("/tmp/repo"), "v1", "v2");
        assert_eq!(report.summary.breaking_api_changes, 0);
        assert_eq!(report.summary.breaking_behavioral_changes, 1);
        assert_eq!(report.summary.total_breaking_changes, 1);
        assert_eq!(report.summary.files_with_breaking_changes, 1);
    }

    #[test]
    fn truly_removed_component_still_marked_removed() {
        let old_surface = ApiSurface {
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
                    language_data: TsSymbolData::default(),
                }],
                language_data: TsSymbolData::default(),
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
            extensions: crate::extensions::TsAnalysisExtensions::default(),
            degradation: Arc::new(semver_analyzer_core::diagnostics::DegradationTracker::new()),
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
            TypeStatus::Removed,
            "Foo should be marked Removed when component doesn't exist in new surface"
        );
    }

    #[test]
    fn helper_interface_removal_does_not_mark_component_removed() {
        fn make_sym(name: &str, kind: SymbolKind, qn: &str) -> Symbol {
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
                    language_data: TsSymbolData::default(),
                }],
                language_data: TsSymbolData::default(),
            }
        }

        let old_surface = ApiSurface {
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

        let new_surface = ApiSurface {
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
            extensions: crate::extensions::TsAnalysisExtensions::default(),
            degradation: Arc::new(semver_analyzer_core::diagnostics::DegradationTracker::new()),
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
                TypeStatus::Removed,
                "Icon should NOT be marked Removed when the component still exists in new surface. Status: {:?}",
                comp.status
            );
        }
    }

    #[test]
    fn discover_child_detects_same_dir_pascal_case() {
        fn make_symbol(name: &str, kind: SymbolKind, dir: &str) -> Symbol {
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
                language_data: TsSymbolData::default(),
            }
        }

        let dir = "packages/react-core/src/components/Modal";
        let old_surface = ApiSurface {
            symbols: vec![make_symbol("Modal", SymbolKind::Variable, dir)],
        };
        let new_surface = ApiSurface {
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
        fn make_symbol(name: &str, kind: SymbolKind, dir: &str) -> Symbol {
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
                language_data: TsSymbolData::default(),
            }
        }

        let modal_dir = "packages/react-core/src/components/Modal";
        let button_dir = "packages/react-core/src/components/Button";
        let old_surface = ApiSurface {
            symbols: vec![make_symbol("Modal", SymbolKind::Variable, modal_dir)],
        };
        let new_surface = ApiSurface {
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
        fn make_symbol(name: &str, kind: SymbolKind, dir: &str) -> Symbol {
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
                language_data: TsSymbolData::default(),
            }
        }

        let dir = "packages/react-core/src/components/Button";

        let old_surface = ApiSurface {
            symbols: vec![make_symbol("Button", SymbolKind::Variable, dir)],
        };

        let new_surface = ApiSurface {
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
        fn make_symbol(name: &str, kind: SymbolKind, qn: &str) -> Symbol {
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
                language_data: TsSymbolData::default(),
            }
        }

        let main_dir = "packages/react-core/src/components/Modal";
        let depr_dir = "packages/react-core/src/deprecated/components/Modal";

        let old_surface = ApiSurface {
            symbols: vec![make_symbol(
                "Modal",
                SymbolKind::Variable,
                &format!("{}/Modal", main_dir),
            )],
        };

        let new_surface = ApiSurface {
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

        let old_surface = ApiSurface {
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
                    language_data: TsSymbolData::default(),
                }],
                language_data: TsSymbolData::default(),
            }],
        };

        let new_surface = ApiSurface {
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
                language_data: TsSymbolData::default(),
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
        type_summaries: Vec<TypeSummary<TypeScript>>,
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
            extensions: crate::extensions::TsAnalysisExtensions::default(),
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
            vec![TypeSummary {
                name: "Dropdown".to_string(),
                definition_name: "DropdownProps".to_string(),
                status: TypeStatus::Modified,
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
                language_data: TsReportData {
                    child_components: vec![],
                    expected_children: vec![
                        ExpectedChild::new("DropdownList", true),
                        ExpectedChild::new("DropdownGroup", false),
                    ],
                },
                source_files: vec![],
            }],
            vec![FileChanges {
                file: PathBuf::from("packages/react-core/src/deprecated/components/Dropdown/Dropdown.d.ts"),
                status: FileStatus::Deleted,
                renamed_from: None,
                breaking_api_changes: vec![
                    ApiChange {
                        symbol: "DropdownToggle".to_string(),
                        qualified_name: String::new(),
                        kind: ApiChangeKind::Constant,
                        change: ApiChangeType::Removed,
                        before: None,
                        after: None,
                        description: "removed".to_string(),
                        migration_target: None,
                        removal_disposition: None,
                    },
                    ApiChange {
                        symbol: "KebabToggle".to_string(),
                        qualified_name: String::new(),
                        kind: ApiChangeKind::Constant,
                        change: ApiChangeType::Removed,
                        before: None,
                        after: None,
                        description: "removed".to_string(),
                        migration_target: None,
                        removal_disposition: None,
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
            vec![TypeSummary {
                name: "ApplicationLauncher".to_string(),
                definition_name: "ApplicationLauncherProps".to_string(),
                status: TypeStatus::Removed,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                language_data: TsReportData {
                    child_components: vec![],
                    expected_children: vec![],
                },
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
            vec![TypeSummary {
                name: "Dropdown".to_string(),
                definition_name: "DropdownProps".to_string(),
                status: TypeStatus::Modified,
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
                language_data: TsReportData {
                    child_components: vec![],
                    expected_children: vec![ExpectedChild::new("DropdownList", true)],
                },
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
            vec![TypeSummary {
                name: "FormGroup".to_string(),
                definition_name: "FormGroupProps".to_string(),
                status: TypeStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                language_data: TsReportData {
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
                },
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
                    language_data: TsSymbolData::default(),
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
                    language_data: TsSymbolData::default(),
                },
            ],
            language_data: TsSymbolData::default(),
        };

        let new_surface = ApiSurface {
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
            .language_data
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
            .language_data
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
            vec![TypeSummary {
                name: "FormGroup".to_string(),
                definition_name: "FormGroupProps".to_string(),
                status: TypeStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                language_data: TsReportData {
                    child_components: vec![],
                    expected_children: vec![ExpectedChild {
                        name: "FormGroupLabelHelp".to_string(),
                        required: false,
                        mechanism: "child".to_string(), // LLM got this wrong
                        prop_name: None,
                    }],
                },
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
                language_data: TsSymbolData::default(),
            }],
            language_data: TsSymbolData::default(),
        };

        let new_surface = ApiSurface {
            symbols: vec![form_group_props],
        };
        let new_hierarchies = HashMap::new();
        enrich_hierarchy_deltas(&mut report, vec![], &new_surface, &new_hierarchies);

        let label_help = report.packages[0].type_summaries[0]
            .language_data
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
                language_data: TsSymbolData::default(),
            }],
            language_data: TsSymbolData::default(),
        };

        let new_surface = ApiSurface {
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
            vec![TypeSummary {
                name: "Modal".to_string(),
                definition_name: "ModalProps".to_string(),
                status: TypeStatus::Modified,
                member_summary: MemberSummary::default(),
                removed_members: vec![],
                type_changes: vec![],
                migration_target: None,
                behavioral_changes: vec![],
                language_data: TsReportData {
                    child_components: vec![],
                    expected_children: vec![ExpectedChild {
                        name: "ModalTitle".to_string(),
                        required: false,
                        mechanism: "child".to_string(),
                        prop_name: None,
                    }],
                },
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
                language_data: TsSymbolData::default(),
            }],
            language_data: TsSymbolData::default(),
        };

        let new_surface = ApiSurface {
            symbols: vec![modal_props],
        };
        let new_hierarchies = HashMap::new();
        enrich_hierarchy_deltas(&mut report, vec![], &new_surface, &new_hierarchies);

        let modal_title = report.packages[0].type_summaries[0]
            .language_data
            .expected_children[0]
            .clone();

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
            vec![TypeSummary {
                name: "Modal".to_string(),
                definition_name: "ModalProps".to_string(),
                status: TypeStatus::Modified,
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
                language_data: TsReportData {
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
                },
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
                language_data: TsSymbolData::default(),
            }],
            language_data: TsSymbolData::default(),
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
                    language_data: TsSymbolData::default(),
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
                    language_data: TsSymbolData::default(),
                },
            ],
            language_data: TsSymbolData::default(),
        };

        // new_surface contains BOTH — simulating what the real extraction produces
        let new_surface = ApiSurface {
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
            .language_data
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
            .language_data
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
            .language_data
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

    // ── enrich_cross_family_absorption tests ─────────────────────────

    #[test]
    fn cross_family_absorption_moves_child_to_correct_parent() {
        // Simulates the MastheadLogo scenario:
        //   - Masthead (root) has child MastheadLogo with 0 absorbed members
        //   - MastheadBrand has removed member "component"
        //   - MastheadLogo has "component" in known_members
        //   → enrichment should move MastheadLogo to MastheadBrand with
        //     absorbed_members = ["component"]

        let dir = "packages/react-core/src/components/Masthead";

        let mut package_map: BTreeMap<String, PackageChanges<TypeScript>> = BTreeMap::new();

        let masthead_ts = TypeSummary {
            name: "Masthead".into(),
            definition_name: "MastheadProps".into(),
            status: TypeStatus::Modified,
            member_summary: MemberSummary {
                total: 3,
                removed: 1,
                renamed: 0,
                type_changed: 0,
                added: 0,
                removal_ratio: 0.33,
            },
            removed_members: vec![RemovedMember {
                name: "backgroundColor".into(),
                old_type: Some("'dark' | 'light' | 'light200'".into()),
                removal_disposition: None,
            }],
            type_changes: vec![],
            migration_target: None,
            behavioral_changes: vec![],
            language_data: TsReportData {
                child_components: vec![ChildComponent {
                    name: "MastheadLogo".into(),
                    status: ChildComponentStatus::Added,
                    known_members: vec!["children".into(), "className".into(), "component".into()],
                    absorbed_members: vec![], // <-- empty, the bug
                }],
                expected_children: vec![],
            },
            source_files: vec![PathBuf::from(format!("{}/Masthead", dir))],
        };

        let masthead_brand_ts = TypeSummary {
            name: "MastheadBrand".into(),
            definition_name: "MastheadBrandProps".into(),
            status: TypeStatus::Modified,
            member_summary: MemberSummary {
                total: 3,
                removed: 1,
                renamed: 0,
                type_changed: 0,
                added: 0,
                removal_ratio: 0.33,
            },
            removed_members: vec![RemovedMember {
                name: "component".into(),
                old_type: Some("ComponentType | ElementType".into()),
                removal_disposition: None,
            }],
            type_changes: vec![],
            migration_target: None,
            behavioral_changes: vec![],
            language_data: TsReportData {
                child_components: vec![], // no children discovered by name-prefix
                expected_children: vec![],
            },
            source_files: vec![PathBuf::from(format!("{}/MastheadBrand", dir))],
        };

        package_map.insert(
            "@patternfly/react-core".into(),
            PackageChanges {
                name: "@patternfly/react-core".into(),
                old_version: None,
                new_version: None,
                type_summaries: vec![masthead_ts, masthead_brand_ts],
                constants: vec![],
                added_exports: vec![],
            },
        );

        enrich_cross_family_absorption(&mut package_map);

        let pkg = &package_map["@patternfly/react-core"];

        // MastheadLogo should have been moved from Masthead to MastheadBrand
        let masthead = &pkg.type_summaries[0];
        assert_eq!(masthead.name, "Masthead");
        assert!(
            masthead.language_data.child_components.is_empty(),
            "Masthead should have 0 children after move, found: {:?}",
            masthead
                .language_data
                .child_components
                .iter()
                .map(|c| &c.name)
                .collect::<Vec<_>>()
        );

        let masthead_brand = &pkg.type_summaries[1];
        assert_eq!(masthead_brand.name, "MastheadBrand");
        assert_eq!(
            masthead_brand.language_data.child_components.len(),
            1,
            "MastheadBrand should have 1 child after move"
        );

        let logo = &masthead_brand.language_data.child_components[0];
        assert_eq!(logo.name, "MastheadLogo");
        assert_eq!(
            logo.absorbed_members,
            vec!["component"],
            "MastheadLogo should have absorbed 'component' from MastheadBrand"
        );
    }

    #[test]
    fn cross_family_absorption_skips_already_absorbed() {
        // If a child already has absorbed_members, don't touch it.
        let dir = "packages/react-core/src/components/Modal";

        let mut package_map: BTreeMap<String, PackageChanges<TypeScript>> = BTreeMap::new();

        let modal_ts = TypeSummary {
            name: "Modal".into(),
            definition_name: "ModalProps".into(),
            status: TypeStatus::Modified,
            member_summary: MemberSummary::default(),
            removed_members: vec![RemovedMember {
                name: "title".into(),
                old_type: Some("string".into()),
                removal_disposition: None,
            }],
            type_changes: vec![],
            migration_target: None,
            behavioral_changes: vec![],
            language_data: TsReportData {
                child_components: vec![ChildComponent {
                    name: "ModalHeader".into(),
                    status: ChildComponentStatus::Added,
                    known_members: vec!["title".into(), "children".into()],
                    absorbed_members: vec!["title".into()], // already absorbed
                }],
                expected_children: vec![],
            },
            source_files: vec![PathBuf::from(format!("{}/Modal", dir))],
        };

        package_map.insert(
            "@patternfly/react-core".into(),
            PackageChanges {
                name: "@patternfly/react-core".into(),
                old_version: None,
                new_version: None,
                type_summaries: vec![modal_ts],
                constants: vec![],
                added_exports: vec![],
            },
        );

        enrich_cross_family_absorption(&mut package_map);

        let pkg = &package_map["@patternfly/react-core"];
        let modal = &pkg.type_summaries[0];
        assert_eq!(modal.language_data.child_components.len(), 1);
        assert_eq!(
            modal.language_data.child_components[0].absorbed_members,
            vec!["title"],
            "Already-absorbed child should not be modified"
        );
    }

    #[test]
    fn cross_family_absorption_no_false_positives_on_ubiquitous_props() {
        // children and className should not trigger cross-absorption
        let dir = "packages/react-core/src/components/Test";

        let mut package_map: BTreeMap<String, PackageChanges<TypeScript>> = BTreeMap::new();

        let parent_a = TypeSummary {
            name: "TestRoot".into(),
            definition_name: "TestRootProps".into(),
            status: TypeStatus::Modified,
            member_summary: MemberSummary::default(),
            removed_members: vec![],
            type_changes: vec![],
            migration_target: None,
            behavioral_changes: vec![],
            language_data: TsReportData {
                child_components: vec![ChildComponent {
                    name: "TestChild".into(),
                    status: ChildComponentStatus::Added,
                    known_members: vec!["children".into(), "className".into()],
                    absorbed_members: vec![],
                }],
                expected_children: vec![],
            },
            source_files: vec![PathBuf::from(format!("{}/TestRoot", dir))],
        };

        let parent_b = TypeSummary {
            name: "TestSibling".into(),
            definition_name: "TestSiblingProps".into(),
            status: TypeStatus::Modified,
            member_summary: MemberSummary::default(),
            removed_members: vec![
                RemovedMember {
                    name: "children".into(),
                    old_type: Some("ReactNode".into()),
                    removal_disposition: None,
                },
                RemovedMember {
                    name: "className".into(),
                    old_type: Some("string".into()),
                    removal_disposition: None,
                },
            ],
            type_changes: vec![],
            migration_target: None,
            behavioral_changes: vec![],
            language_data: TsReportData {
                child_components: vec![],
                expected_children: vec![],
            },
            source_files: vec![PathBuf::from(format!("{}/TestSibling", dir))],
        };

        package_map.insert(
            "test-pkg".into(),
            PackageChanges {
                name: "test-pkg".into(),
                old_version: None,
                new_version: None,
                type_summaries: vec![parent_a, parent_b],
                constants: vec![],
                added_exports: vec![],
            },
        );

        enrich_cross_family_absorption(&mut package_map);

        let pkg = &package_map["test-pkg"];
        let root = &pkg.type_summaries[0];
        // TestChild should NOT have been moved — only ubiquitous props match
        assert_eq!(
            root.language_data.child_components.len(),
            1,
            "TestChild should remain with TestRoot (only ubiquitous props match)"
        );
        assert!(
            root.language_data.child_components[0]
                .absorbed_members
                .is_empty(),
            "TestChild should still have empty absorbed_members"
        );
    }

    // ── SD-based prop replacement detection tests ─────────────────────

    use crate::sd_types::{ComponentSourceProfile, SdPipelineResult};

    /// Helper: build a minimal AnalysisReport with SD data for prop replacement tests.
    fn build_report_with_sd(
        changes: Vec<FileChanges<TypeScript>>,
        sd: SdPipelineResult,
    ) -> AnalysisReport<TypeScript> {
        let mut report = AnalysisReport {
            repository: PathBuf::new(),
            comparison: Comparison {
                from_ref: String::new(),
                to_ref: String::new(),
                from_sha: String::new(),
                to_sha: String::new(),
                commit_count: 0,
                analysis_timestamp: String::new(),
            },
            summary: Summary {
                total_breaking_changes: 0,
                breaking_api_changes: 0,
                breaking_behavioral_changes: 0,
                files_with_breaking_changes: 0,
            },
            changes,
            manifest_changes: vec![],
            added_files: vec![],
            packages: vec![],
            member_renames: HashMap::new(),
            inferred_rename_patterns: None,
            metadata: AnalysisMetadata {
                call_graph_analysis: String::new(),
                tool_version: String::new(),
                llm_usage: None,
            },
            extensions: crate::extensions::TsAnalysisExtensions::default(),
        };
        report.extensions.sd_result = Some(sd);
        report
    }

    /// Helper: build a FileChanges with one removed prop entry.
    fn removed_prop_change(component: &str, prop: &str, before: &str) -> FileChanges<TypeScript> {
        FileChanges {
            file: PathBuf::from(format!("src/{}.d.ts", component)),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: format!("{}.{}", component, prop),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::Removed,
                before: Some(format!("property: {}: {}", prop, before)),
                after: None,
                description: format!("property `{}` was removed", prop),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }
    }

    #[test]
    fn sd_prop_replacement_tier1_one_to_one() {
        // Avatar: border (removed) → isBordered (added) — 1:1 ratio
        let changes = vec![removed_prop_change("Avatar", "border", "'dark' | 'light'")];

        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "Avatar".into(),
            ["alt", "border", "className", "size", "src"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "Avatar".into(),
            ["alt", "isBordered", "className", "size", "src"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        let change = &report.changes[0].breaking_api_changes[0];
        assert!(
            change.removal_disposition.is_some(),
            "border should have removal_disposition set"
        );
        match &change.removal_disposition {
            Some(RemovalDisposition::ReplacedByMember { new_member }) => {
                assert_eq!(new_member, "isBordered");
            }
            other => panic!("Expected ReplacedByMember, got {:?}", other),
        }
    }

    /// When type information is available, the Tier 1 (1:1 ratio) matcher
    /// must reject pairings where the types are structurally incompatible
    /// (e.g., boolean vs a named interface type). This prevents false renames
    /// like AccordionContent `isHidden → contentBodyProps`.
    #[test]
    fn sd_prop_replacement_tier1_rejects_incompatible_types() {
        let changes = vec![removed_prop_change(
            "AccordionContent",
            "isHidden",
            "boolean",
        )];

        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "AccordionContent".into(),
            ["isHidden", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "AccordionContent".into(),
            ["contentBodyProps", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );

        // Provide type information so the guard fires
        sd.old_component_prop_types.insert(
            "AccordionContent".into(),
            [
                ("isHidden".into(), "boolean".into()),
                ("className".into(), "string".into()),
            ]
            .into_iter()
            .collect(),
        );
        sd.new_component_prop_types.insert(
            "AccordionContent".into(),
            [
                ("contentBodyProps".into(), "AccordionExpandableContentBodyProps".into()),
                ("className".into(), "string".into()),
            ]
            .into_iter()
            .collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        let change = &report.changes[0].breaking_api_changes[0];
        assert!(
            change.removal_disposition.is_none(),
            "isHidden should NOT be paired with contentBodyProps (incompatible types: boolean vs named interface)"
        );
    }

    #[test]
    fn sd_prop_replacement_tier2_css_binding() {
        // Button: isActive (removed) → isClicked (added) via CSS binding match
        let changes = vec![removed_prop_change("Button", "isActive", "boolean")];

        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "Button".into(),
            ["isActive", "variant", "isBlock"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "Button".into(),
            [
                "isClicked",
                "isExpanded",
                "isFavorite",
                "isHamburger",
                "isSettings",
                "variant",
                "isBlock",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        );

        // Old profile with CSS binding for isActive
        let mut old_profile = ComponentSourceProfile::default();
        old_profile.prop_style_bindings.insert(
            "isActive".into(),
            ["styles.modifiers.active"].iter().map(|s| s.to_string()).collect(),
        );
        sd.old_profiles.insert("Button".into(), old_profile);

        // New profile with CSS bindings for multiple new props
        let mut new_profile = ComponentSourceProfile::default();
        new_profile.prop_style_bindings.insert(
            "isClicked".into(),
            ["styles.modifiers.clicked"].iter().map(|s| s.to_string()).collect(),
        );
        new_profile.prop_style_bindings.insert(
            "isFavorite".into(),
            ["styles.modifiers.favorite"].iter().map(|s| s.to_string()).collect(),
        );
        new_profile.prop_style_bindings.insert(
            "isHamburger".into(),
            ["styles.modifiers.hamburger"].iter().map(|s| s.to_string()).collect(),
        );
        sd.new_profiles.insert("Button".into(), new_profile);

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        let change = &report.changes[0].breaking_api_changes[0];
        match &change.removal_disposition {
            Some(RemovalDisposition::ReplacedByMember { new_member }) => {
                assert_eq!(
                    new_member, "isClicked",
                    "isActive should match isClicked (active→clicked has best combined score)"
                );
            }
            other => panic!("Expected ReplacedByMember(isClicked), got {:?}", other),
        }
    }

    #[test]
    fn sd_prop_replacement_tier3_prop_split() {
        // Banner: variant (removed, enum) → color + status (added)
        let changes = vec![removed_prop_change(
            "Banner",
            "variant",
            "'blue' | 'default' | 'gold' | 'green' | 'red'",
        )];

        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "Banner".into(),
            ["variant", "isSticky"].iter().map(|s| s.to_string()).collect(),
        );
        sd.new_component_props.insert(
            "Banner".into(),
            ["color", "status", "isSticky"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.old_component_prop_types.insert(
            "Banner".into(),
            [("variant".into(), "'blue' | 'default' | 'gold' | 'green' | 'red'".into())]
                .into_iter()
                .collect(),
        );
        sd.new_component_prop_types.insert(
            "Banner".into(),
            [
                ("color".into(), "BannerColor".into()),
                ("status".into(), "BannerStatus".into()),
            ]
            .into_iter()
            .collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        let change = &report.changes[0].breaking_api_changes[0];
        match &change.removal_disposition {
            Some(RemovalDisposition::ReplacedByMember { new_member }) => {
                assert_eq!(
                    new_member, "color",
                    "variant with color values should match 'color' prop"
                );
            }
            other => panic!("Expected ReplacedByMember(color), got {:?}", other),
        }
    }

    #[test]
    fn sd_prop_replacement_no_match_many_to_many() {
        // Modal: 4 removed string props, 2 added — too ambiguous, no match
        let changes = vec![FileChanges {
            file: PathBuf::from("src/Modal.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![
                ApiChange {
                    symbol: "Modal.bodyAriaLabel".into(),
                    qualified_name: String::new(),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Removed,
                    before: Some("property: bodyAriaLabel: string".into()),
                    after: None,
                    description: "removed".into(),
                    migration_target: None,
                    removal_disposition: None,
                },
                ApiChange {
                    symbol: "Modal.title".into(),
                    qualified_name: String::new(),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Removed,
                    before: Some("property: title: string".into()),
                    after: None,
                    description: "removed".into(),
                    migration_target: None,
                    removal_disposition: None,
                },
            ],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "Modal".into(),
            ["bodyAriaLabel", "title", "isOpen"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "Modal".into(),
            ["backdropClassName", "backdropId", "isOpen"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        // Neither prop should be matched — too ambiguous
        for change in &report.changes[0].breaking_api_changes {
            assert!(
                change.removal_disposition.is_none(),
                "Modal props should not be matched (many-to-many): {}",
                change.symbol
            );
        }
    }

    #[test]
    fn sd_prop_replacement_skips_already_matched() {
        // A prop that already has removal_disposition should not be overwritten
        let changes = vec![FileChanges {
            file: PathBuf::from("src/Test.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Test.oldProp".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::Removed,
                before: Some("property: oldProp: boolean".into()),
                after: None,
                description: "removed".into(),
                migration_target: None,
                removal_disposition: Some(RemovalDisposition::TrulyRemoved),
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "Test".into(),
            ["oldProp"].iter().map(|s| s.to_string()).collect(),
        );
        sd.new_component_props.insert(
            "Test".into(),
            ["newProp"].iter().map(|s| s.to_string()).collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        // Should remain TrulyRemoved, not overwritten
        match &report.changes[0].breaking_api_changes[0].removal_disposition {
            Some(RemovalDisposition::TrulyRemoved) => {}
            other => panic!(
                "Expected TrulyRemoved (unchanged), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_extract_css_modifier_name() {
        let tokens: BTreeSet<String> =
            ["styles.modifiers.active"].iter().map(|s| s.to_string()).collect();
        assert_eq!(extract_css_modifier_name(&tokens), Some("active".into()));

        let bracket: BTreeSet<String> =
            ["styles.modifiers[status]"].iter().map(|s| s.to_string()).collect();
        assert_eq!(extract_css_modifier_name(&bracket), Some("status".into()));

        let empty: BTreeSet<String> =
            ["styles.menu"].iter().map(|s| s.to_string()).collect();
        assert_eq!(extract_css_modifier_name(&empty), None);
    }

    #[test]
    fn sd_rename_verification_invalidates_false_rename() {
        // Label: TD said isOverflowLabel → isClickable (false rename)
        // SD shows: isOverflowLabel mapped to modifier "overflow" which still exists
        //           isClickable maps to modifier "clickable" (different)
        let changes = vec![FileChanges {
            file: PathBuf::from("src/Label.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Label.isOverflowLabel".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::Renamed,
                before: Some("isOverflowLabel".into()),
                after: Some("isClickable".into()),
                description: "property isOverflowLabel renamed to isClickable".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let mut sd = SdPipelineResult::default();

        // Old profile: isOverflowLabel → modifiers.overflow
        let mut old_profile = ComponentSourceProfile::default();
        old_profile.prop_style_bindings.insert(
            "isOverflowLabel".into(),
            ["styles.modifiers.overflow"].iter().map(|s| s.to_string()).collect(),
        );
        sd.old_profiles.insert("Label".into(), old_profile);

        // New profile: isClickable → modifiers.clickable, "overflow" still in BEM
        let mut new_profile = ComponentSourceProfile::default();
        new_profile.prop_style_bindings.insert(
            "isClickable".into(),
            ["styles.modifiers.clickable"].iter().map(|s| s.to_string()).collect(),
        );
        new_profile.bem_modifiers.insert("overflow".into());
        new_profile.bem_modifiers.insert("clickable".into());
        sd.new_profiles.insert("Label".into(), new_profile);

        // Prop sets (needed for the rest of enrichment to not crash)
        sd.old_component_props.insert(
            "Label".into(),
            ["isOverflowLabel", "variant"]
                .iter().map(|s| s.to_string()).collect(),
        );
        sd.new_component_props.insert(
            "Label".into(),
            ["isClickable", "status", "variant"]
                .iter().map(|s| s.to_string()).collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        let change = &report.changes[0].breaking_api_changes[0];
        assert_eq!(
            change.change,
            ApiChangeType::Removed,
            "False rename should be reclassified to Removed"
        );
        assert!(
            change.after.is_none(),
            "after should be cleared on invalidated rename"
        );
    }

    #[test]
    fn sd_rename_verification_with_prop_to_value_absorption() {
        // Label: isOverflowLabel → variant="overflow" (prop-to-value absorption)
        // The old modifier "overflow" matches a new value on the variant prop
        let changes = vec![FileChanges {
            file: PathBuf::from("src/Label.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Label.isOverflowLabel".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::Renamed,
                before: Some("isOverflowLabel".into()),
                after: Some("isClickable".into()),
                description: "property isOverflowLabel renamed to isClickable".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let mut sd = SdPipelineResult::default();

        let mut old_profile = ComponentSourceProfile::default();
        old_profile.prop_style_bindings.insert(
            "isOverflowLabel".into(),
            ["styles.modifiers.overflow"].iter().map(|s| s.to_string()).collect(),
        );
        sd.old_profiles.insert("Label".into(), old_profile);

        let mut new_profile = ComponentSourceProfile::default();
        new_profile.prop_style_bindings.insert(
            "isClickable".into(),
            ["styles.modifiers.clickable"].iter().map(|s| s.to_string()).collect(),
        );
        new_profile.bem_modifiers.insert("overflow".into());
        new_profile.bem_modifiers.insert("clickable".into());
        sd.new_profiles.insert("Label".into(), new_profile);

        // variant prop exists in both, type expanded with 'overflow'
        sd.old_component_props.insert(
            "Label".into(),
            ["isOverflowLabel", "variant"]
                .iter().map(|s| s.to_string()).collect(),
        );
        sd.new_component_props.insert(
            "Label".into(),
            ["isClickable", "status", "variant"]
                .iter().map(|s| s.to_string()).collect(),
        );
        sd.old_component_prop_types.insert(
            "Label".into(),
            [("variant".into(), "'outline' | 'filled'".into())]
                .into_iter().collect(),
        );
        sd.new_component_prop_types.insert(
            "Label".into(),
            [("variant".into(), "'outline' | 'filled' | 'overflow' | 'add'".into())]
                .into_iter().collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        let change = &report.changes[0].breaking_api_changes[0];
        assert_eq!(change.change, ApiChangeType::Removed);
        match &change.removal_disposition {
            Some(RemovalDisposition::ReplacedByMember { new_member }) => {
                assert_eq!(
                    new_member, "variant=\"overflow\"",
                    "Should detect absorption into variant='overflow'"
                );
            }
            other => panic!(
                "Expected ReplacedByMember with variant=\"overflow\", got {:?}",
                other
            ),
        }
    }

    #[test]
    fn sd_rename_verification_preserves_valid_rename() {
        // FormGroup: labelIcon → labelHelp (valid rename)
        // CSS modifier "help" didn't exist before, so the old modifier
        // is NOT in new BEM → rename should NOT be invalidated
        let changes = vec![FileChanges {
            file: PathBuf::from("src/FormGroup.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "FormGroup.labelIcon".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::Renamed,
                before: Some("labelIcon".into()),
                after: Some("labelHelp".into()),
                description: "property labelIcon renamed to labelHelp".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let mut sd = SdPipelineResult::default();
        // No CSS bindings for labelIcon → no verification → rename preserved
        sd.old_profiles
            .insert("FormGroup".into(), ComponentSourceProfile::default());
        sd.new_profiles
            .insert("FormGroup".into(), ComponentSourceProfile::default());
        sd.old_component_props.insert(
            "FormGroup".into(),
            ["labelIcon"].iter().map(|s| s.to_string()).collect(),
        );
        sd.new_component_props.insert(
            "FormGroup".into(),
            ["labelHelp"].iter().map(|s| s.to_string()).collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        let change = &report.changes[0].breaking_api_changes[0];
        assert_eq!(
            change.change,
            ApiChangeType::Renamed,
            "Valid rename should be preserved"
        );
        assert_eq!(change.after.as_deref(), Some("labelHelp"));
    }

    // ═══════════════════════════════════════════════════════════════════
    // TC-driven prop disposition tests
    // Each test corresponds to a failing TC from the PF6 migration bench.
    // These tests should FAIL with the current code and PASS after fixes.
    // ═══════════════════════════════════════════════════════════════════

    /// TC005: Avatar `border` (enum) should match `isBordered` (boolean)
    /// even when type information is available.
    ///
    /// Currently fails because prop_types_compatible() rejects boolean
    /// vs non-primitive (enum) pairings. The type guard should be relaxed
    /// for boolean↔enum when name similarity is high (strip "is" prefix →
    /// sim("border","Bordered") = 0.75).
    #[test]
    fn sd_prop_replacement_boolean_to_enum_with_types() {
        let changes = vec![removed_prop_change("Avatar", "border", "'none' | 'dark' | 'light'")];

        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "Avatar".into(),
            ["alt", "border", "className", "size", "src"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "Avatar".into(),
            ["alt", "isBordered", "className", "size", "src"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );

        // Provide type info — this is what makes it different from the
        // existing sd_prop_replacement_tier1_one_to_one test
        sd.old_component_prop_types.insert(
            "Avatar".into(),
            [
                ("border".into(), "'none' | 'dark' | 'light'".into()),
                ("alt".into(), "string".into()),
            ]
            .into_iter()
            .collect(),
        );
        sd.new_component_prop_types.insert(
            "Avatar".into(),
            [
                ("isBordered".into(), "boolean".into()),
                ("alt".into(), "string".into()),
            ]
            .into_iter()
            .collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        let change = &report.changes[0].breaking_api_changes[0];
        assert!(
            change.removal_disposition.is_some(),
            "TC005: border should be matched to isBordered even with type info (boolean↔enum)"
        );
        match &change.removal_disposition {
            Some(RemovalDisposition::ReplacedByMember { new_member }) => {
                assert_eq!(new_member, "isBordered");
            }
            other => panic!(
                "TC005: Expected ReplacedByMember(isBordered), got {:?}",
                other
            ),
        }
    }

    /// TC011: Checkbox `isLabelBeforeButton` (boolean) should match
    /// `labelPosition` (enum 'start'|'end').
    ///
    /// Currently fails because:
    /// 1. prop_types_compatible() rejects boolean vs enum
    /// 2. Name similarity is borderline (~0.45 raw, ~0.56 with "is" stripped)
    #[test]
    fn sd_prop_replacement_boolean_to_enum_label_position() {
        let changes = vec![removed_prop_change(
            "Checkbox",
            "isLabelBeforeButton",
            "boolean",
        )];

        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "Checkbox".into(),
            [
                "id",
                "isLabelBeforeButton",
                "isChecked",
                "isDisabled",
                "label",
                "className",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        );
        sd.new_component_props.insert(
            "Checkbox".into(),
            [
                "id",
                "labelPosition",
                "isChecked",
                "isDisabled",
                "label",
                "className",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        );

        sd.old_component_prop_types.insert(
            "Checkbox".into(),
            [
                ("isLabelBeforeButton".into(), "boolean".into()),
                ("isChecked".into(), "boolean".into()),
                ("isDisabled".into(), "boolean".into()),
            ]
            .into_iter()
            .collect(),
        );
        sd.new_component_prop_types.insert(
            "Checkbox".into(),
            [
                ("labelPosition".into(), "'start' | 'end'".into()),
                ("isChecked".into(), "boolean".into()),
                ("isDisabled".into(), "boolean".into()),
            ]
            .into_iter()
            .collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        let change = &report.changes[0].breaking_api_changes[0];
        assert!(
            change.removal_disposition.is_some(),
            "TC011: isLabelBeforeButton should be matched to labelPosition (boolean→enum)"
        );
        match &change.removal_disposition {
            Some(RemovalDisposition::ReplacedByMember { new_member }) => {
                // Accept either plain prop name or value-absorbed form
                assert!(
                    new_member == "labelPosition"
                        || new_member.starts_with("labelPosition="),
                    "TC011: Expected labelPosition or labelPosition=\"start\", got {}",
                    new_member
                );
            }
            other => panic!(
                "TC011: Expected ReplacedByMember(labelPosition), got {:?}",
                other
            ),
        }
    }

    /// TC028: ErrorState `errorTitle`→`titleText` and `errorDescription`→`bodyText`
    /// with component name prefix stripping and N:N matching.
    ///
    /// Currently fails because:
    /// 1. Tier 1 requires 1:1 ratio (2 removed, 2 added → skipped)
    /// 2. No CSS bindings → Tier 2 skipped
    /// 3. Not a union type → Tier 3 skipped
    #[test]
    fn sd_prop_replacement_n_to_n_component_prefix_strip() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/ErrorState.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![
                ApiChange {
                    symbol: "ErrorState.errorTitle".into(),
                    qualified_name: String::new(),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Removed,
                    before: Some("property: errorTitle: string".into()),
                    after: None,
                    description: "property `errorTitle` was removed".into(),
                    migration_target: None,
                    removal_disposition: None,
                },
                ApiChange {
                    symbol: "ErrorState.errorDescription".into(),
                    qualified_name: String::new(),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Removed,
                    before: Some("property: errorDescription: string".into()),
                    after: None,
                    description: "property `errorDescription` was removed".into(),
                    migration_target: None,
                    removal_disposition: None,
                },
            ],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "ErrorState".into(),
            ["errorTitle", "errorDescription", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "ErrorState".into(),
            ["titleText", "bodyText", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.old_component_prop_types.insert(
            "ErrorState".into(),
            [
                ("errorTitle".into(), "string".into()),
                ("errorDescription".into(), "string".into()),
            ]
            .into_iter()
            .collect(),
        );
        sd.new_component_prop_types.insert(
            "ErrorState".into(),
            [
                ("titleText".into(), "string".into()),
                ("bodyText".into(), "string".into()),
            ]
            .into_iter()
            .collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        // errorTitle should match titleText (strip "error" prefix:
        // "Title" vs "titleText" → high similarity)
        let title_change = report.changes[0]
            .breaking_api_changes
            .iter()
            .find(|c| c.symbol.contains("errorTitle"))
            .expect("errorTitle change should exist");
        assert!(
            title_change.removal_disposition.is_some(),
            "TC028: errorTitle should match titleText via component prefix stripping"
        );
        match &title_change.removal_disposition {
            Some(RemovalDisposition::ReplacedByMember { new_member }) => {
                assert_eq!(
                    new_member, "titleText",
                    "TC028: errorTitle should map to titleText"
                );
            }
            other => panic!(
                "TC028: Expected ReplacedByMember(titleText), got {:?}",
                other
            ),
        }

        // errorDescription should match bodyText (strip "error" prefix:
        // "Description" vs "bodyText" — lower similarity but should still match
        // via N:N greedy assignment after errorTitle→titleText is taken)
        let desc_change = report.changes[0]
            .breaking_api_changes
            .iter()
            .find(|c| c.symbol.contains("errorDescription"))
            .expect("errorDescription change should exist");
        assert!(
            desc_change.removal_disposition.is_some(),
            "TC028: errorDescription should match bodyText via N:N greedy matching"
        );
        match &desc_change.removal_disposition {
            Some(RemovalDisposition::ReplacedByMember { new_member }) => {
                assert_eq!(
                    new_member, "bodyText",
                    "TC028: errorDescription should map to bodyText"
                );
            }
            other => panic!(
                "TC028: Expected ReplacedByMember(bodyText), got {:?}",
                other
            ),
        }
    }

    /// TC028 real-world scenario: TD pipeline already emitted
    /// `errorDescription` as `Renamed → titleText` (wrong — same target as
    /// `errorTitle`). The SD enrichment should detect the duplicate rename
    /// target and reassign `errorDescription → bodyText` via the N:M greedy
    /// matcher.
    ///
    /// This tests the case where `Renamed` changes are included in the SD
    /// enrichment matching pool so the greedy matcher can fix duplicate
    /// assignments from the TD pipeline.
    #[test]
    fn sd_enrichment_fixes_duplicate_td_rename_target() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/ErrorState.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![
                // TD pipeline correctly renamed errorTitle → titleText
                ApiChange {
                    symbol: "ErrorState.errorTitle".into(),
                    qualified_name: String::new(),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Renamed,
                    before: Some("property: errorTitle: string".into()),
                    after: Some("titleText".into()),
                    description: "property `errorTitle` was renamed to `titleText`".into(),
                    migration_target: None,
                    removal_disposition: None,
                },
                // TD pipeline INCORRECTLY renamed errorDescription → titleText
                // (should be bodyText)
                ApiChange {
                    symbol: "ErrorState.errorDescription".into(),
                    qualified_name: String::new(),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Renamed,
                    before: Some("property: errorDescription: string".into()),
                    after: Some("titleText".into()),
                    description: "property `errorDescription` was renamed to `titleText`".into(),
                    migration_target: None,
                    removal_disposition: None,
                },
            ],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "ErrorState".into(),
            ["errorTitle", "errorDescription", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "ErrorState".into(),
            ["titleText", "bodyText", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.old_component_prop_types.insert(
            "ErrorState".into(),
            [
                ("errorTitle".into(), "string".into()),
                ("errorDescription".into(), "string".into()),
            ]
            .into_iter()
            .collect(),
        );
        sd.new_component_prop_types.insert(
            "ErrorState".into(),
            [
                ("titleText".into(), "string".into()),
                ("bodyText".into(), "string".into()),
            ]
            .into_iter()
            .collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        // errorTitle should stay as Renamed → titleText (correct)
        let title_change = report.changes[0]
            .breaking_api_changes
            .iter()
            .find(|c| c.symbol.contains("errorTitle"))
            .expect("errorTitle change should exist");
        assert_eq!(
            title_change.change,
            ApiChangeType::Renamed,
            "TC028: errorTitle should stay Renamed"
        );
        assert_eq!(
            title_change.after.as_deref(),
            Some("titleText"),
            "TC028: errorTitle rename target should stay titleText"
        );

        // errorDescription should be corrected from titleText → bodyText
        let desc_change = report.changes[0]
            .breaking_api_changes
            .iter()
            .find(|c| c.symbol.contains("errorDescription"))
            .expect("errorDescription change should exist");
        assert_eq!(
            desc_change.after.as_deref(),
            Some("bodyText"),
            "TC028: errorDescription should be corrected from titleText to bodyText. \
             The TD pipeline incorrectly assigned both errorTitle and errorDescription \
             to titleText. SD enrichment should detect the duplicate and reassign."
        );
    }

    /// Renamed changes that don't have duplicate targets should NOT be modified.
    /// Only intervene when there's a provable conflict.
    #[test]
    fn sd_enrichment_does_not_touch_valid_renames() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/FormGroup.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![
                // Correct 1:1 rename — no duplicate target
                ApiChange {
                    symbol: "FormGroup.labelIcon".into(),
                    qualified_name: String::new(),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Renamed,
                    before: Some("property: labelIcon: ReactElement".into()),
                    after: Some("labelHelp".into()),
                    description: "property `labelIcon` was renamed to `labelHelp`".into(),
                    migration_target: None,
                    removal_disposition: None,
                },
            ],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "FormGroup".into(),
            ["labelIcon", "label", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "FormGroup".into(),
            ["labelHelp", "label", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        let change = report.changes[0]
            .breaking_api_changes
            .iter()
            .find(|c| c.symbol.contains("labelIcon"))
            .expect("labelIcon change should exist");
        assert_eq!(
            change.change,
            ApiChangeType::Renamed,
            "Valid rename should stay Renamed"
        );
        assert_eq!(
            change.after.as_deref(),
            Some("labelHelp"),
            "Valid rename target should stay unchanged"
        );
    }

    /// TC008: Button `isActive` should match `isClicked` (both boolean),
    /// NOT `state` (enum). The greedy matcher incorrectly picks `state`
    /// because boolean-prefix stripping boosts "active" vs "state" to 0.50,
    /// beating "isActive" vs "isClicked" at 0.44.
    ///
    /// Fix: apply a penalty factor to type-incompatible candidates so
    /// type-compatible matches win when scores are close.
    #[test]
    fn sd_enrichment_prefers_type_compatible_match() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/Button.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Button.isActive".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::Removed,
                before: Some("property: isActive: boolean".into()),
                after: None,
                description: "property `isActive` was removed".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let mut sd = SdPipelineResult::default();
        sd.old_component_props.insert(
            "Button".into(),
            ["isActive", "variant", "children"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "Button".into(),
            ["isClicked", "state", "variant", "children"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        // Type info: isActive is boolean, isClicked is boolean, state is enum
        sd.old_component_prop_types.insert(
            "Button".into(),
            [("isActive".into(), "boolean".into())]
                .into_iter()
                .collect(),
        );
        sd.new_component_prop_types.insert(
            "Button".into(),
            [
                ("isClicked".into(), "boolean".into()),
                ("state".into(), "'attention' | 'read' | 'unread'".into()),
            ]
            .into_iter()
            .collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        let change = report.changes[0]
            .breaking_api_changes
            .iter()
            .find(|c| c.symbol.contains("isActive"))
            .expect("isActive change should exist");

        match &change.removal_disposition {
            Some(RemovalDisposition::ReplacedByMember { new_member }) => {
                assert_eq!(
                    new_member, "isClicked",
                    "TC008: isActive should match isClicked (both boolean), \
                     not state (enum). Type-compatible candidates should be \
                     preferred when scores are close."
                );
            }
            other => panic!(
                "TC008: Expected ReplacedByMember(isClicked), got {:?}",
                other
            ),
        }
    }

    /// TC082: TD falsely renamed `usePageInsets`→`hasNoPadding` on Toolbar.
    ///
    /// Known limitation: the TD pipeline matches these via type fingerprint
    /// (both boolean, same component) with sim=0.40. A name-quality check
    /// in Step 0 cannot distinguish this false rename from valid low-similarity
    /// renames like `chips→labels` (sim=0.17) and `spacer→gap` (sim=0.17).
    /// Fixing this requires changes to the TD pipeline's boolean pool
    /// threshold, not the SD enrichment step.
    #[test]
    #[ignore = "TC082: needs TD pipeline boolean pool threshold fix, not Step 0"]
    fn sd_step0_invalidates_low_quality_td_rename() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/Toolbar.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Toolbar.usePageInsets".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::Renamed,
                before: Some("usePageInsets".into()),
                after: Some("hasNoPadding".into()),
                description: "property usePageInsets renamed to hasNoPadding".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let mut sd = SdPipelineResult::default();
        // No CSS bindings for either prop
        sd.old_profiles
            .insert("Toolbar".into(), ComponentSourceProfile::default());
        sd.new_profiles
            .insert("Toolbar".into(), ComponentSourceProfile::default());
        sd.old_component_props.insert(
            "Toolbar".into(),
            ["usePageInsets", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "Toolbar".into(),
            ["hasNoPadding", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        let change = &report.changes[0].breaking_api_changes[0];
        assert_eq!(
            change.change,
            ApiChangeType::Removed,
            "TC082: False rename usePageInsets→hasNoPadding should be invalidated to Removed"
        );
        assert!(
            change.after.is_none(),
            "TC082: after should be cleared on invalidated rename"
        );
    }

    /// TC054: NotAuthorized→UnauthorizedAccess component rename should
    /// carry prop mappings: `title`→`titleText`, `description`→`bodyText`.
    ///
    /// Currently fails because:
    /// 1. SD enrichment keys old/new props by component name, but old name
    ///    (NotAuthorized) != new name (UnauthorizedAccess)
    /// 2. Migration matching threshold (0.60) rejects title→titleText (0.56)
    #[test]
    fn sd_prop_replacement_component_rename_carries_props() {
        // The report has a Removed change for NotAuthorized component
        // with a MigrationTarget pointing to UnauthorizedAccess
        let changes = vec![FileChanges {
            file: PathBuf::from("src/NotAuthorized.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![
                ApiChange {
                    symbol: "NotAuthorized.title".into(),
                    qualified_name: String::new(),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Removed,
                    before: Some("property: title: string".into()),
                    after: None,
                    description: "property `title` was removed".into(),
                    migration_target: None,
                    removal_disposition: None,
                },
                ApiChange {
                    symbol: "NotAuthorized.description".into(),
                    qualified_name: String::new(),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Removed,
                    before: Some("property: description: string".into()),
                    after: None,
                    description: "property `description` was removed".into(),
                    migration_target: None,
                    removal_disposition: None,
                },
            ],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let mut sd = SdPipelineResult::default();
        // Old props under old name, new props under new name
        sd.old_component_props.insert(
            "NotAuthorized".into(),
            ["title", "description", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        sd.new_component_props.insert(
            "UnauthorizedAccess".into(),
            ["titleText", "bodyText", "className"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );

        // Tell the report about the component rename
        let mut report = build_report_with_sd(changes, sd);
        report
            .member_renames
            .insert("NotAuthorized".into(), "UnauthorizedAccess".into());
        enrich_removal_dispositions_from_sd(&mut report);

        let title_change = report.changes[0]
            .breaking_api_changes
            .iter()
            .find(|c| c.symbol.contains("title") && !c.symbol.contains("description"))
            .expect("title change should exist");
        assert!(
            title_change.removal_disposition.is_some(),
            "TC054: title should match titleText via component rename prop mapping"
        );
        match &title_change.removal_disposition {
            Some(RemovalDisposition::ReplacedByMember { new_member }) => {
                assert_eq!(
                    new_member, "titleText",
                    "TC054: title should map to titleText"
                );
            }
            other => panic!(
                "TC054: Expected ReplacedByMember(titleText), got {:?}",
                other
            ),
        }
    }

    /// TC082: usePageInsets → hasNoPadding is a false rename. Both are boolean
    /// props on Toolbar, but they control completely different CSS modifiers:
    /// - usePageInsets → pf-m-page-insets (horizontal content padding)
    /// - hasNoPadding → pf-m-no-padding (vertical root padding)
    ///
    /// The CSS resolved-value comparison should detect zero overlap in
    /// normalized CSS property keys and invalidate the rename.
    #[test]
    fn step0_css_resolved_value_invalidates_false_boolean_rename() {
        use crate::sd_types::{
            ComponentCssModifiers, ComponentSourceProfile, CssModifierEffect, CssModifierMap,
        };

        // Build a Renamed change: Toolbar.usePageInsets → hasNoPadding
        let changes = vec![FileChanges {
            file: PathBuf::from("src/Toolbar.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Toolbar.usePageInsets".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::Renamed,
                before: Some("usePageInsets".into()),
                after: Some("hasNoPadding".into()),
                description: "property `usePageInsets` was renamed to `hasNoPadding`".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let mut sd = SdPipelineResult::default();

        // Old profile: Toolbar has BEM block "toolbar", usePageInsets is in BEM modifiers
        let mut old_profile = ComponentSourceProfile::default();
        old_profile.name = "Toolbar".into();
        old_profile.bem_block = Some("toolbar".into());
        old_profile.bem_modifiers = ["fullHeight", "pageInsets", "static", "sticky"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        sd.old_profiles.insert("Toolbar".into(), old_profile);

        // New profile: Toolbar's BEM modifiers replaced pageInsets with noPadding
        let mut new_profile = ComponentSourceProfile::default();
        new_profile.name = "Toolbar".into();
        new_profile.bem_block = Some("toolbar".into());
        new_profile.bem_modifiers = ["fullHeight", "noPadding", "static", "sticky"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        sd.new_profiles.insert("Toolbar".into(), new_profile);

        // Old CSS modifiers: pf-m-page-insets controls horizontal content padding
        let mut old_page_insets = CssModifierEffect::default();
        old_page_insets.resolved_overrides.insert(
            "--pf-v5-c-toolbar__content--PaddingLeft".into(),
            "1rem".into(),
        );
        old_page_insets.resolved_overrides.insert(
            "--pf-v5-c-toolbar__content--PaddingRight".into(),
            "1rem".into(),
        );
        let mut old_toolbar_mods = CssModifierMap::new();
        old_toolbar_mods.insert("pf-m-page-insets".into(), old_page_insets);

        let mut old_css = ComponentCssModifiers::new();
        old_css.insert("toolbar".into(), old_toolbar_mods);
        sd.old_css_modifiers = old_css;

        // New CSS modifiers: pf-m-no-padding controls vertical root padding
        let mut new_no_padding = CssModifierEffect::default();
        new_no_padding.resolved_overrides.insert(
            "--pf-v6-c-toolbar--PaddingBlockEnd".into(),
            "0".into(),
        );
        new_no_padding.resolved_overrides.insert(
            "--pf-v6-c-toolbar--m-sticky--PaddingBlockEnd".into(),
            "0".into(),
        );
        new_no_padding.resolved_overrides.insert(
            "--pf-v6-c-toolbar--m-sticky--PaddingBlockStart".into(),
            "0".into(),
        );
        let mut new_toolbar_mods = CssModifierMap::new();
        new_toolbar_mods.insert("pf-m-no-padding".into(), new_no_padding);

        let mut new_css = ComponentCssModifiers::new();
        new_css.insert("toolbar".into(), new_toolbar_mods);
        sd.new_css_modifiers = new_css;

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        let change = &report.changes[0].breaking_api_changes[0];
        assert_eq!(
            change.change,
            ApiChangeType::Removed,
            "TC082: usePageInsets → hasNoPadding should be invalidated (reclassified as Removed) \
             because CSS modifiers pf-m-page-insets and pf-m-no-padding affect completely \
             different CSS properties. Got: {:?}",
            change.change
        );
        assert!(
            change.after.is_none(),
            "TC082: after field should be cleared when rename is invalidated"
        );
    }

    /// Tabs.isSecondary → isSubtab is a TRUE rename. Both modifiers affect
    /// the same CSS properties (font sizes for tab sub-elements).
    /// The CSS resolved-value check should NOT invalidate this rename.
    #[test]
    fn step0_css_resolved_value_preserves_true_boolean_rename() {
        use crate::sd_types::{
            ComponentCssModifiers, ComponentSourceProfile, CssModifierEffect, CssModifierMap,
        };

        let changes = vec![FileChanges {
            file: PathBuf::from("src/Tabs.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Tabs.isSecondary".into(),
                qualified_name: String::new(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::Renamed,
                before: Some("isSecondary".into()),
                after: Some("isSubtab".into()),
                description: "property `isSecondary` was renamed to `isSubtab`".into(),
                migration_target: None,
                removal_disposition: None,
            }],
            breaking_behavioral_changes: vec![],
            container_changes: vec![],
        }];

        let mut sd = SdPipelineResult::default();

        // Old profile: secondary is in BEM modifiers
        let mut old_profile = ComponentSourceProfile::default();
        old_profile.name = "Tabs".into();
        old_profile.bem_block = Some("tabs".into());
        old_profile.bem_modifiers = ["secondary", "box"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        sd.old_profiles.insert("Tabs".into(), old_profile);

        // New profile: secondary still exists (different purpose), subtab added
        let mut new_profile = ComponentSourceProfile::default();
        new_profile.name = "Tabs".into();
        new_profile.bem_block = Some("tabs".into());
        new_profile.bem_modifiers = ["secondary", "subtab", "box"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        sd.new_profiles.insert("Tabs".into(), new_profile);

        // Old CSS: pf-m-secondary overrides font sizes
        let mut old_secondary = CssModifierEffect::default();
        old_secondary.resolved_overrides.insert(
            "--pf-v5-c-tabs__add--c-button--FontSize".into(),
            ".75rem".into(),
        );
        old_secondary.resolved_overrides.insert(
            "--pf-v5-c-tabs__item-action--c-button--FontSize".into(),
            ".75rem".into(),
        );
        old_secondary.resolved_overrides.insert(
            "--pf-v5-c-tabs__link--FontSize".into(),
            ".875rem".into(),
        );
        let mut old_tabs_mods = CssModifierMap::new();
        old_tabs_mods.insert("pf-m-secondary".into(), old_secondary);

        let mut old_css = ComponentCssModifiers::new();
        old_css.insert("tabs".into(), old_tabs_mods);
        sd.old_css_modifiers = old_css;

        // New CSS: pf-m-subtab overrides the SAME font size properties
        let mut new_subtab = CssModifierEffect::default();
        new_subtab.resolved_overrides.insert(
            "--pf-v6-c-tabs__add--c-button--FontSize".into(),
            ".75rem".into(),
        );
        new_subtab.resolved_overrides.insert(
            "--pf-v6-c-tabs__item-action--c-button--FontSize".into(),
            ".75rem".into(),
        );
        new_subtab.resolved_overrides.insert(
            "--pf-v6-c-tabs__link--FontSize".into(),
            ".75rem".into(),
        );
        let mut new_tabs_mods = CssModifierMap::new();
        new_tabs_mods.insert("pf-m-subtab".into(), new_subtab);

        let mut new_css = ComponentCssModifiers::new();
        new_css.insert("tabs".into(), new_tabs_mods);
        sd.new_css_modifiers = new_css;

        let mut report = build_report_with_sd(changes, sd);
        enrich_removal_dispositions_from_sd(&mut report);

        let change = &report.changes[0].breaking_api_changes[0];
        assert_eq!(
            change.change,
            ApiChangeType::Renamed,
            "Tabs.isSecondary → isSubtab should remain Renamed because CSS modifiers \
             pf-m-secondary and pf-m-subtab affect the same CSS properties (font sizes). \
             Got: {:?}",
            change.change
        );
    }
}
