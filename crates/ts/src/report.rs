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

use semver_analyzer_core::{
    AddedComponent, AnalysisMetadata, AnalysisReport, AnalysisResult, ApiChange, ApiChangeKind,
    ApiChangeType, ApiSurface, BehavioralChange, ChangeSubject, ChildComponent,
    ChildComponentStatus, Comparison, ComponentStatus, ComponentSummary, ConstantGroup,
    ExpectedChild, FileChanges, FileStatus, HierarchyDelta, InferredRenamePatterns, LlmApiChange,
    ManifestChange, MigratedProp, PackageChanges, PropertySummary, RemovalDisposition,
    RemovedProperty, StructuralChange, StructuralChangeType, SuffixRename, Summary, Symbol,
    SymbolKind, TypeChange,
};

use crate::TypeScript;

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

    // Merge composition pattern changes into the report's file entries.
    for (source_path, comp_changes) in &results.composition_changes {
        let existing = report
            .changes
            .iter_mut()
            .find(|fc| fc.file.to_string_lossy().starts_with(source_path));
        if let Some(fc) = existing {
            fc.composition_pattern_changes.extend(comp_changes.clone());
        } else if !comp_changes.is_empty() {
            report.changes.push(FileChanges {
                file: PathBuf::from(source_path),
                status: FileStatus::Modified,
                renamed_from: None,
                breaking_api_changes: vec![],
                breaking_behavioral_changes: vec![],
                composition_pattern_changes: comp_changes.clone(),
            });
        }
    }

    // Enrich hierarchy deltas and populate expected_children.
    if !results.hierarchy_deltas.is_empty() || !results.new_hierarchies.is_empty() {
        enrich_hierarchy_deltas(
            &mut report,
            results.hierarchy_deltas.clone(),
            &results.new_surface,
            &results.new_hierarchies,
        );
    }

    report
}

// ─── Core report building ────────────────────────────────────────────────

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
                composition_pattern_changes: vec![],
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
        hierarchy_deltas: Vec::new(),
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
    let resolve_package = |qualified_name: &str| -> Option<String> {
        let parts: Vec<&str> = qualified_name.split('/').collect();
        if let Some(pkg_idx) = parts.iter().position(|&p| p == "packages") {
            if pkg_idx + 1 < parts.len() {
                return Some(parts[pkg_idx + 1].to_string());
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

        let interface_name = &old_sym.name;
        let component_name = interface_name
            .strip_suffix("Props")
            .unwrap_or(interface_name)
            .to_string();

        let total_members = old_sym.members.len();

        let mut removed = 0usize;
        let mut renamed = 0usize;
        let mut type_changed = 0usize;
        let mut added = 0usize;
        let mut removed_properties = Vec::new();
        let mut type_changes = Vec::new();

        if let Some(changes) = member_changes {
            for change in changes {
                match &change.change_type {
                    StructuralChangeType::Removed(ChangeSubject::Member { .. }) => {
                        removed += 1;
                        let lookup_key = format!("{}.{}", interface_name, change.symbol);
                        let disposition = llm_disposition_map
                            .get(lookup_key.as_str())
                            .and_then(|entry| entry.removal_disposition.clone());
                        removed_properties.push(RemovedProperty {
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
            });

        let component_behavioral: Vec<BehavioralChange<TypeScript>> = behavioral_changes
            .iter()
            .filter(|bc| {
                bc.symbol == component_name
                    || bc.symbol == *interface_name
                    || bc
                        .referenced_components
                        .iter()
                        .any(|r| r == &component_name)
            })
            .map(|bc| BehavioralChange {
                symbol: bc.symbol.clone(),
                kind: bc.kind.clone(),
                category: bc.category.clone(),
                description: bc.description.clone(),
                source_file: bc.source_file.clone(),
                confidence: bc.confidence,
                evidence_type: bc.evidence_type.clone(),
                referenced_components: bc.referenced_components.clone(),
                is_internal_only: bc.is_internal_only,
            })
            .collect();

        let source_file = old_sym.qualified_name.split('.').next().map(PathBuf::from);

        let removed_prop_names: Vec<&str> = removed_properties
            .iter()
            .map(|rp| rp.name.as_str())
            .collect();
        let child_components = discover_child_components(
            &component_name,
            &old_sym.qualified_name,
            old_surface,
            new_surface,
            structural_changes,
            behavioral_changes,
            &removed_prop_names,
            &removed_properties,
        );

        let summary = ComponentSummary {
            name: component_name.clone(),
            interface_name: interface_name.clone(),
            status,
            property_summary: PropertySummary {
                total: total_members,
                removed,
                renamed,
                type_changed,
                added,
                removal_ratio,
            },
            removed_properties,
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
                components: Vec::new(),
                constants: Vec::new(),
                added_components: Vec::new(),
            });
        pkg_entry.components.push(summary);
    }

    // ── Step 5: Build constant groups ────────────────────────────────
    let mut constant_groups: HashMap<(String, ApiChangeType), Vec<String>> = HashMap::new();

    for change in structural_changes {
        if !change.is_breaking {
            continue;
        }
        if change.kind != SymbolKind::Constant && change.kind != SymbolKind::Variable {
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
                components: Vec::new(),
                constants: Vec::new(),
                added_components: Vec::new(),
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

        let added = AddedComponent {
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
                components: Vec::new(),
                constants: Vec::new(),
                added_components: Vec::new(),
            });
        pkg_entry.added_components.push(added);
    }

    package_map.into_values().collect()
}

// ─── Child component discovery ───────────────────────────────────────────

/// Discover child/sibling components for a given parent component.
fn discover_child_components(
    component_name: &str,
    parent_qn: &str,
    old_surface: &ApiSurface,
    new_surface: &ApiSurface,
    structural_changes: &[StructuralChange],
    _behavioral_changes: &[BehavioralChange<TypeScript>],
    removed_prop_names: &[&str],
    removed_properties: &[RemovedProperty],
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

    let removed_set: HashSet<&str> = removed_prop_names.iter().copied().collect();

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

        let known_props: Vec<String> = sym.members.iter().map(|m| m.name.clone()).collect();

        let props_iface_name = format!("{}Props", name);
        let props_members: Vec<String> = new_surface
            .symbols
            .iter()
            .find(|s| s.name == props_iface_name && s.qualified_name.contains(component_dir))
            .map(|s| s.members.iter().map(|m| m.name.clone()).collect())
            .unwrap_or_default();

        let mut all_props: HashSet<String> = known_props.into_iter().collect();
        all_props.extend(props_members);
        let all_props_sorted: Vec<String> = {
            let mut v: Vec<String> = all_props.into_iter().collect();
            v.sort();
            v
        };

        let absorbed: Vec<String> = all_props_sorted
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
                known_props: all_props_sorted,
                absorbed_props: absorbed,
            },
        );
    }

    // ── Enrichment pass: LLM removal_disposition ──
    for rp in removed_properties {
        if let Some(RemovalDisposition::MovedToChild {
            target_component,
            mechanism,
        }) = &rp.removal_disposition
        {
            if let Some(child) = children_map.get_mut(target_component) {
                if !child.absorbed_props.contains(&rp.name) {
                    child.absorbed_props.push(rp.name.clone());
                    child.absorbed_props.sort();
                }
            } else {
                children_map.insert(
                    target_component.clone(),
                    ChildComponent {
                        name: target_component.clone(),
                        status: ChildComponentStatus::Added,
                        known_props: if mechanism == "children" {
                            vec!["children".to_string()]
                        } else {
                            vec![rp.name.clone()]
                        },
                        absorbed_props: vec![rp.name.clone()],
                    },
                );
            }
        }
    }

    children_map.into_values().collect()
}

// ─── Hierarchy Delta Enrichment ──────────────────────────────────────────

/// Enrich hierarchy deltas with prop migration data and populate
/// `expected_children` on each `ComponentSummary`.
fn enrich_hierarchy_deltas(
    report: &mut AnalysisReport<TypeScript>,
    mut deltas: Vec<HierarchyDelta>,
    new_surface: &ApiSurface,
    new_hierarchies: &HashMap<String, HashMap<String, Vec<ExpectedChild>>>,
) {
    // Build a lookup of component name → props from the new surface.
    let mut component_props: HashMap<String, HashSet<String>> = HashMap::new();

    for sym in &new_surface.symbols {
        if matches!(sym.kind, SymbolKind::Interface | SymbolKind::TypeAlias) {
            if let Some(comp_name) = sym.name.strip_suffix("Props") {
                let props: HashSet<String> = sym.members.iter().map(|m| m.name.clone()).collect();
                component_props
                    .entry(comp_name.to_string())
                    .or_default()
                    .extend(props);
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
            }
        }
    }

    // Enrich each delta with migrated props
    for delta in &mut deltas {
        let removed_props: Vec<String> = report
            .packages
            .iter()
            .flat_map(|pkg| &pkg.components)
            .find(|c| c.name == delta.component)
            .map(|c| {
                c.removed_properties
                    .iter()
                    .map(|rp| rp.name.clone())
                    .collect()
            })
            .unwrap_or_default();

        if removed_props.is_empty() {
            continue;
        }

        for child in &delta.added_children {
            let child_props = match component_props.get(&child.name) {
                Some(props) => props,
                None => continue,
            };

            for removed_prop in &removed_props {
                if child_props.contains(removed_prop) {
                    delta.migrated_props.push(MigratedProp {
                        prop_name: removed_prop.clone(),
                        target_child: child.name.clone(),
                        target_prop_name: None,
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
                for comp in &mut pkg.components {
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
                    .position(|p| p.components.iter().any(|c| c.name.starts_with(family)))
                    .or_else(|| {
                        report
                            .packages
                            .iter()
                            .position(|p| !p.components.is_empty())
                    });

                if let Some(idx) = target_idx {
                    report.packages[idx].components.push(ComponentSummary {
                        name: comp_name.clone(),
                        interface_name: format!("{}Props", comp_name),
                        status: ComponentStatus::Modified,
                        property_summary: PropertySummary::default(),
                        removed_properties: vec![],
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
            for comp in &pkg.components {
                if !comp.expected_children.is_empty() {
                    comp_children.insert(comp.name.clone(), comp.expected_children.clone());
                }
            }
        }

        let mut interface_to_component: HashMap<String, String> = HashMap::new();
        for pkg in &report.packages {
            for comp in &pkg.components {
                interface_to_component.insert(comp.interface_name.clone(), comp.name.clone());
            }
        }

        let mut inferred_count = 0;
        let mut inferred: Vec<(String, Vec<ExpectedChild>)> = Vec::new();

        for pkg in &report.packages {
            for comp in &pkg.components {
                if !comp.expected_children.is_empty() {
                    continue;
                }

                let base_interface = match extends_map.get(&comp.interface_name) {
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
                        });
                    } else {
                        if let Some(sub_children) = comp_children.get(&base_child.name) {
                            for sub in sub_children {
                                base_queue.push(sub);
                            }
                        }
                    }
                }

                if !mapped_children.is_empty() {
                    eprintln!(
                        "[Hierarchy] Inferred expected_children for {} from {} extends chain: {:?}",
                        comp.name,
                        base_component,
                        mapped_children.iter().map(|c| &c.name).collect::<Vec<_>>(),
                    );
                    inferred.push((comp.name.clone(), mapped_children));
                    inferred_count += 1;
                }
            }
        }

        // Apply inferred children
        for (comp_name, children) in inferred {
            for pkg in &mut report.packages {
                for comp in &mut pkg.components {
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
            eprintln!(
                "[Hierarchy] Inferred expected_children for {} components via extends chain fallback",
                inferred_count,
            );
        }
    }

    // Store deltas on the report
    report.hierarchy_deltas = deltas;

    let total_migrated: usize = report
        .hierarchy_deltas
        .iter()
        .map(|d| d.migrated_props.len())
        .sum();
    eprintln!(
        "[Hierarchy] Enriched {} deltas with {} migrated props, populated expected_children",
        report.hierarchy_deltas.len(),
        total_migrated,
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
            .map_or(false, |c| c.is_ascii_uppercase())
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
                eprintln!("Found {} added source files between refs", files.len());
            }
            files
        }
        Err(e) => {
            eprintln!("Warning: could not enumerate added files: {}", e);
            Vec::new()
        }
    }
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
        ApiSurface, BehavioralChange, BehavioralChangeKind, Symbol, SymbolKind, Visibility,
    };

    #[test]
    fn build_report_empty() {
        let results = AnalysisResult {
            structural_changes: vec![],
            behavioral_changes: vec![],
            manifest_changes: vec![],
            llm_api_changes: vec![],
            old_surface: ApiSurface { symbols: vec![] },
            new_surface: ApiSurface { symbols: vec![] },
            inferred_rename_patterns: None,
            composition_changes: vec![],
            hierarchy_deltas: vec![],
            new_hierarchies: HashMap::new(),
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
            structural_changes: changes,
            behavioral_changes: vec![],
            manifest_changes: manifest,
            llm_api_changes: vec![],
            old_surface: ApiSurface { symbols: vec![] },
            new_surface: ApiSurface { symbols: vec![] },
            inferred_rename_patterns: None,
            composition_changes: vec![],
            hierarchy_deltas: vec![],
            new_hierarchies: HashMap::new(),
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
            referenced_components: vec![],
            is_internal_only: None,
        }];

        let results = AnalysisResult {
            structural_changes: vec![],
            behavioral_changes: behavioral,
            manifest_changes: vec![],
            llm_api_changes: vec![],
            old_surface: ApiSurface { symbols: vec![] },
            new_surface: ApiSurface { symbols: vec![] },
            inferred_rename_patterns: None,
            composition_changes: vec![],
            hierarchy_deltas: vec![],
            new_hierarchies: HashMap::new(),
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
                }],
            }],
        };

        let new_surface = ApiSurface { symbols: vec![] };

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
            structural_changes,
            behavioral_changes: vec![],
            manifest_changes: vec![],
            llm_api_changes: vec![],
            old_surface,
            new_surface,
            inferred_rename_patterns: None,
            composition_changes: vec![],
            hierarchy_deltas: vec![],
            new_hierarchies: HashMap::new(),
        };
        let report = build_report(&results, Path::new("/tmp/repo"), "v5", "v6");

        let foo_comp = report
            .packages
            .iter()
            .flat_map(|p| &p.components)
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
        fn make_sym(name: &str, kind: SymbolKind, qn: &str) -> Symbol {
            Symbol {
                name: name.to_string(),
                qualified_name: qn.to_string(),
                kind,
                visibility: Visibility::Exported,
                file: format!("{}.d.ts", qn).into(),
                package: None,
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
                }],
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
            structural_changes,
            behavioral_changes: vec![],
            manifest_changes: vec![],
            llm_api_changes: vec![],
            old_surface,
            new_surface,
            inferred_rename_patterns: None,
            composition_changes: vec![],
            hierarchy_deltas: vec![],
            new_hierarchies: HashMap::new(),
        };
        let report = build_report(&results, Path::new("/tmp/repo"), "v5", "v6");

        let icon_comp = report
            .packages
            .iter()
            .flat_map(|p| &p.components)
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
        fn make_symbol(name: &str, kind: SymbolKind, dir: &str) -> Symbol {
            Symbol {
                name: name.to_string(),
                qualified_name: format!("{}/{}", dir, name),
                kind,
                visibility: Visibility::Exported,
                file: format!("{}/{}.d.ts", dir, name).into(),
                package: None,
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
}
