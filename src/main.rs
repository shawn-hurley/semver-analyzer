mod cli;
mod konveyor;
mod orchestrator;



use anyhow::{Context, Result};
use clap::Parser;
use std::collections::HashMap;
use std::path::Path;

use cli::{Cli, Command};
use semver_analyzer_core::{
    AnalysisMetadata, AnalysisReport, ApiChange, ApiChangeKind, ApiChangeType, ApiSurface,
    BehavioralChange, Comparison, FileChanges, FileStatus, ManifestChange, StructuralChange,
    Summary,
};
use semver_analyzer_ts::OxcExtractor;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Extract {
            repo,
            git_ref,
            output,
            build_command,
        } => {
            cmd_extract(&repo, &git_ref, output.as_deref(), build_command.as_deref())?;
        }

        Command::Diff { from, to, output } => {
            cmd_diff(&from, &to, output.as_deref())?;
        }

        Command::Analyze {
            repo,
            from,
            to,
            output,
            no_llm,
            llm_command,
            max_llm_cost,
            build_command,
            llm_all_files,
        } => {
            cmd_analyze(
                &repo,
                &from,
                &to,
                output.as_deref(),
                no_llm,
                llm_command.as_deref(),
                max_llm_cost,
                build_command.as_deref(),
                llm_all_files,
            )
            .await?;
        }

        Command::Konveyor {
            from_report,
            repo,
            from,
            to,
            output_dir,
            file_pattern,
            ruleset_name,
            no_llm,
            llm_command,
            max_llm_cost,
            build_command,
            llm_all_files,
            no_consolidate,
            rename_patterns,
        } => {
            cmd_konveyor(
                from_report.as_deref(),
                repo.as_deref(),
                from.as_deref(),
                to.as_deref(),
                &output_dir,
                &file_pattern,
                &ruleset_name,
                no_llm,
                llm_command.as_deref(),
                max_llm_cost,
                build_command.as_deref(),
                llm_all_files,
                no_consolidate,
                rename_patterns.as_deref(),
            )
            .await?;
        }

        Command::Serve => {
            eprintln!("MCP server not yet implemented");
        }
    }

    Ok(())
}

// ─── Extract command ─────────────────────────────────────────────────────

fn cmd_extract(
    repo: &Path,
    git_ref: &str,
    output: Option<&Path>,
    build_command: Option<&str>,
) -> Result<()> {
    eprintln!(
        "Extracting API surface from {} at ref {}",
        repo.display(),
        git_ref
    );

    let extractor = OxcExtractor::new();
    let surface = extractor
        .extract_at_ref(repo, git_ref, build_command)
        .context("Failed to extract API surface")?;

    eprintln!(
        "Extracted {} symbols from {} files",
        surface.symbols.len(),
        count_unique_files(&surface)
    );

    write_json_output(&surface, output)?;
    Ok(())
}

// ─── Diff command ────────────────────────────────────────────────────────

fn cmd_diff(from_path: &Path, to_path: &Path, output: Option<&Path>) -> Result<()> {
    eprintln!(
        "Diffing {} vs {}",
        from_path.display(),
        to_path.display()
    );

    let old_json = std::fs::read_to_string(from_path)
        .with_context(|| format!("Failed to read {}", from_path.display()))?;
    let new_json = std::fs::read_to_string(to_path)
        .with_context(|| format!("Failed to read {}", to_path.display()))?;

    let old: ApiSurface = serde_json::from_str(&old_json)
        .with_context(|| format!("Failed to parse {} as ApiSurface", from_path.display()))?;
    let new: ApiSurface = serde_json::from_str(&new_json)
        .with_context(|| format!("Failed to parse {} as ApiSurface", to_path.display()))?;

    let changes = semver_analyzer_core::diff::diff_surfaces(&old, &new);

    let breaking = changes.iter().filter(|c| c.is_breaking).count();
    let non_breaking = changes.len() - breaking;
    eprintln!(
        "Found {} changes ({} breaking, {} non-breaking)",
        changes.len(),
        breaking,
        non_breaking
    );

    write_json_output(&changes, output)?;
    Ok(())
}

// ─── Analyze command (concurrent TD+BU pipeline) ────────────────────────

async fn cmd_analyze(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    output: Option<&Path>,
    no_llm: bool,
    llm_command: Option<&str>,
    _max_llm_cost: f64,
    build_command: Option<&str>,
    llm_all_files: bool,
) -> Result<()> {
    eprintln!(
        "Analyzing {} from {} to {}",
        repo.display(),
        from_ref,
        to_ref
    );
    if no_llm {
        eprintln!("Mode: static analysis only (--no-llm)");
    }

    // Run concurrent TD+BU pipeline
    let result = orchestrator::run_concurrent_analysis(
        repo,
        from_ref,
        to_ref,
        no_llm,
        llm_command,
        build_command,
        llm_all_files,
    )
    .await?;

    // Print summary stats
    let manifest_breaking = result.manifest_changes.iter().filter(|c| c.is_breaking).count();
    if !result.manifest_changes.is_empty() {
        eprintln!(
            "[TD]   {} manifest changes ({} breaking)",
            result.manifest_changes.len(),
            manifest_breaking
        );
    }

    // Build report
    let mut report = build_report(
        repo,
        from_ref,
        to_ref,
        result.structural_changes,
        result.behavioral_changes,
        result.manifest_changes,
        result.llm_api_changes,
        &result.old_surface,
        &result.new_surface,
        result.inferred_rename_patterns,
    );

    // Merge composition pattern changes into the report's file entries.
    for (source_path, comp_changes) in result.composition_changes {
        // Find or create a FileChanges entry for the source component
        let existing = report.changes.iter_mut().find(|fc| {
            fc.file.to_string_lossy().starts_with(&source_path)
        });
        if let Some(fc) = existing {
            fc.composition_pattern_changes.extend(comp_changes);
        } else if !comp_changes.is_empty() {
            // Create a new entry for the component directory
            report.changes.push(semver_analyzer_core::FileChanges {
                file: std::path::PathBuf::from(&source_path),
                status: semver_analyzer_core::FileStatus::Modified,
                renamed_from: None,
                breaking_api_changes: vec![],
                breaking_behavioral_changes: vec![],
                composition_pattern_changes: comp_changes,
            });
        }
    }

    // ── Merge hierarchy deltas into the report ─────────────────────────
    //
    // The orchestrator computed hierarchy deltas (added/removed children
    // per component). Now enrich them with prop migration data by
    // matching removed parent props against child component props from
    // the new API surface, and populate expected_children on each
    // ComponentSummary.
    if !result.hierarchy_deltas.is_empty() || !result.new_hierarchies.is_empty() {
        enrich_hierarchy_deltas(
            &mut report,
            result.hierarchy_deltas,
            &result.new_surface,
            &result.new_hierarchies,
        );
    }

    // ── Infer CSS suffix renames via LLM ─────────────────────────────
    //
    // Extract suffix inventory from compound token diffs, then ask the
    // LLM to identify CSS physical→logical property renames (e.g.,
    // PaddingTop → PaddingBlockStart). Store the resulting member_renames
    // in the report so the konveyor step can generate CSS var rules.
    if !no_llm {
        if let Some(llm_cmd) = llm_command {
            let (removed_suffixes, added_suffixes) =
                konveyor::extract_suffix_inventory(&report);
            if !removed_suffixes.is_empty() && !added_suffixes.is_empty() {
                eprintln!(
                    "[Suffix] Extracted {} removed, {} added suffixes from token diffs",
                    removed_suffixes.len(),
                    added_suffixes.len()
                );

                let suffix_result = tokio::task::spawn_blocking({
                    let cmd = llm_cmd.to_string();
                    let removed: Vec<String> =
                        removed_suffixes.into_iter().collect();
                    let added: Vec<String> =
                        added_suffixes.into_iter().collect();
                    move || {
                        let analyzer =
                            semver_analyzer_llm::LlmBehaviorAnalyzer::new(&cmd);
                        let removed_refs: Vec<&str> =
                            removed.iter().map(|s| s.as_str()).collect();
                        let added_refs: Vec<&str> =
                            added.iter().map(|s| s.as_str()).collect();
                        analyzer.infer_suffix_renames(&removed_refs, &added_refs)
                    }
                })
                .await;

                match suffix_result {
                    Ok(Ok(renames)) if !renames.is_empty() => {
                        eprintln!(
                            "[Suffix] LLM identified {} CSS suffix renames:",
                            renames.len()
                        );
                        let suffix_map: HashMap<String, String> = renames
                            .iter()
                            .map(|r| {
                                eprintln!("  {} → {}", r.from, r.to);
                                (r.from.clone(), r.to.clone())
                            })
                            .collect();

                        // Apply suffix mappings to compound tokens and store
                        // the resulting member renames on the report
                        let member_renames =
                            konveyor::apply_suffix_renames(&report, &suffix_map);

                        if !member_renames.is_empty() {
                            eprintln!(
                                "[Suffix] Applied suffix mappings: {} member renames",
                                member_renames.len()
                            );
                            report.member_renames = member_renames;
                        }
                    }
                    Ok(Ok(_)) => {
                        eprintln!("[Suffix] LLM returned no suffix renames");
                    }
                    Ok(Err(e)) => {
                        eprintln!("[Suffix] WARN: LLM suffix inference failed: {}", e);
                    }
                    Err(e) => {
                        eprintln!("[Suffix] WARN: spawn_blocking failed: {}", e);
                    }
                }
            }
        }
    }

    let total_breaking = report.summary.total_breaking_changes;
    eprintln!();
    if total_breaking == 0 {
        eprintln!("No breaking changes detected.");
    } else {
        eprintln!(
            "BREAKING: {} total breaking change(s) detected.",
            total_breaking
        );
        eprintln!(
            "  {} API changes, {} behavioral changes",
            report.summary.breaking_api_changes,
            report.summary.breaking_behavioral_changes
        );
    }

    write_json_output(&report, output)?;
    Ok(())
}

// ─── Konveyor command ────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn cmd_konveyor(
    from_report: Option<&Path>,
    repo: Option<&Path>,
    from_ref: Option<&str>,
    to_ref: Option<&str>,
    output_dir: &Path,
    file_pattern: &str,
    ruleset_name: &str,
    no_llm: bool,
    llm_command: Option<&str>,
    _max_llm_cost: f64,
    build_command: Option<&str>,
    llm_all_files: bool,
    no_consolidate: bool,
    rename_patterns_path: Option<&Path>,
) -> Result<()> {
    let mut rename_patterns = if let Some(path) = rename_patterns_path {
        konveyor::RenamePatterns::load(path)?
    } else {
        konveyor::RenamePatterns::empty()
    };

    let mut report = if let Some(report_path) = from_report {
        // Mode 1: Load pre-existing report
        eprintln!("Loading report from {}", report_path.display());
        let json = std::fs::read_to_string(report_path)
            .with_context(|| format!("Failed to read {}", report_path.display()))?;
        let report: semver_analyzer_core::AnalysisReport = serde_json::from_str(&json)
            .with_context(|| format!("Failed to parse {} as AnalysisReport", report_path.display()))?;
        report
    } else {
        // Mode 2: Run analysis internally
        let repo = repo.context("--repo is required when --from-report is not provided")?;
        let from = from_ref.context("--from is required when --from-report is not provided")?;
        let to = to_ref.context("--to is required when --from-report is not provided")?;

        eprintln!(
            "Analyzing {} from {} to {}",
            repo.display(),
            from,
            to
        );
        if no_llm {
            eprintln!("Mode: static analysis only (--no-llm)");
        }

        let result = orchestrator::run_concurrent_analysis(
            repo, from, to, no_llm, llm_command, build_command, llm_all_files,
        )
        .await?;

        build_report(
            repo,
            from,
            to,
            result.structural_changes,
            result.behavioral_changes,
            result.manifest_changes,
            result.llm_api_changes,
            &result.old_surface,
            &result.new_surface,
            result.inferred_rename_patterns,
        )
    };

    // Build package info cache (name + version) from package.json files.
    // The name-only cache is derived from this for generate_rules().
    let pkg_info_cache = konveyor::build_package_info_cache(&report);
    let pkg_cache: HashMap<String, String> = pkg_info_cache
        .iter()
        .map(|(k, v)| (k.clone(), v.name.clone()))
        .collect();

    // Analyze token member objects for redundancy suppression and member renames.
    // The report may already contain member_renames from the analyze step
    // (populated via LLM suffix inference). analyze_token_members adds any
    // additional renames from explicit rename patterns.
    let (covered_symbols, mut member_renames) =
        konveyor::analyze_token_members(&report, &rename_patterns);
    // Merge in any LLM-inferred renames already on the report
    for (k, v) in &report.member_renames {
        member_renames.entry(k.clone()).or_insert_with(|| v.clone());
    }
    if !covered_symbols.is_empty() {
        eprintln!(
            "Found {} token member keys covered by parent objects, {} member renames",
            covered_symbols.len(),
            member_renames.len()
        );
    }

    // Store member renames into the report so they're available in the JSON
    // output and for --from-report paths
    if !member_renames.is_empty() {
        report.member_renames = member_renames.clone();

        // Enrich constant groups with suffix renames extracted from member_renames
        let suffix_renames = extract_suffix_renames(&member_renames);
        if !suffix_renames.is_empty() {
            for pkg in &mut report.packages {
                for group in &mut pkg.constants {
                    if group.strategy_hint == "CssVariablePrefix" {
                        group.suffix_renames = suffix_renames.clone();
                    }
                }
            }
        }
    }

    // Enrich package entries with npm package names and versions from the cache
    for pkg in &mut report.packages {
        if let Some(info) = pkg_info_cache.get(&pkg.name) {
            pkg.name = info.name.clone();
            pkg.old_version = info.version.clone();
        }
    }

    // Merge LLM-inferred constant rename patterns into rename_patterns.
    // These are discovered by the rename inference phase and stored in the
    // report. They supplement (or replace) the manually authored YAML patterns.
    if let Some(ref inferred) = report.inferred_rename_patterns {
        for pat in &inferred.constant_patterns {
            rename_patterns.add_pattern(&pat.match_regex, &pat.replace);
        }
        if !inferred.constant_patterns.is_empty() {
            eprintln!(
                "Merged {} LLM-inferred constant rename patterns into rename_patterns",
                inferred.constant_patterns.len()
            );
        }
    }

    // Generate rules — each rule carries its own fix_strategy
    let raw_rules = konveyor::generate_rules(
        &report,
        file_pattern,
        &pkg_cache,
        &rename_patterns,
        &member_renames,
    );
    let raw_count = raw_rules.len();

    // Suppress redundant individual token removal rules
    let filtered_rules = konveyor::suppress_redundant_token_rules(raw_rules, &covered_symbols);

    let rules = if no_consolidate {
        filtered_rules
    } else {
        let (consolidated, _id_mapping) = konveyor::consolidate_rules(filtered_rules);
        eprintln!(
            "Consolidated {} rules → {} rules",
            raw_count,
            consolidated.len()
        );
        consolidated
    };

    // Suppress prop-level RemoveProp rules when a component-level
    // component-import-deprecated rule already covers the same component.
    let rules = konveyor::suppress_redundant_prop_rules(rules);

    // Suppress prop-value-change rules that duplicate type-changed rules
    // for the same component/prop/value trigger.
    let rules = konveyor::suppress_redundant_prop_value_rules(rules);

    // Extract strategies from the final rules (strategies were merged during
    // consolidation by merge_rule_group)
    let mut strategies = konveyor::extract_fix_strategies(&rules);

    // Generate dependency update rules (package.json version bumps)
    let (dep_update_rules, dep_update_strategies) =
        konveyor::generate_dependency_update_rules(&report, &pkg_info_cache);

    // Merge dependency update strategies into the main strategies map
    strategies.extend(dep_update_strategies);

    // Combine all rules for writing (API/behavioral rules + dependency update rules)
    let mut all_rules = rules;
    all_rules.extend(dep_update_rules);

    let fix_guidance = konveyor::generate_fix_guidance(&report, &all_rules, file_pattern);
    let rule_count = all_rules.len();

    // Write ruleset directory
    konveyor::write_ruleset_dir(output_dir, ruleset_name, &report, &all_rules)?;

    // Write fix guidance to sibling directory
    let fix_dir = konveyor::write_fix_guidance_dir(output_dir, &fix_guidance)?;

    // Write fix strategies
    konveyor::write_fix_strategies(&fix_dir, &strategies)?;

    // Generate conformance rules (separate from migration rules)
    let conformance_rules = konveyor::generate_conformance_rules(&report);
    if !conformance_rules.is_empty() {
        let conformance_strategies = konveyor::extract_fix_strategies(&conformance_rules);
        konveyor::write_conformance_rules(output_dir, &conformance_rules)?;

        // Merge conformance strategies into the main strategies file
        strategies.extend(conformance_strategies);
        konveyor::write_fix_strategies(&fix_dir, &strategies)?;
    }

    eprintln!(
        "Generated {} Konveyor rules in {}",
        rule_count,
        output_dir.display()
    );
    eprintln!("  Ruleset:  {}/ruleset.yaml", output_dir.display());
    eprintln!("  Rules:    {}/breaking-changes.yaml", output_dir.display());
    if !conformance_rules.is_empty() {
        eprintln!(
            "  Conformance: {}/conformance-rules.yaml ({} rules)",
            output_dir.display(),
            conformance_rules.len(),
        );
    }
    eprintln!("  Fixes:    {}/fix-guidance.yaml", fix_dir.display());
    eprintln!("  Strategies: {}/fix-strategies.json ({} entries)", fix_dir.display(), strategies.len());
    eprintln!(
        "  Summary:  {} auto-fixable, {} need review, {} manual only",
        fix_guidance.summary.auto_fixable,
        fix_guidance.summary.needs_review,
        fix_guidance.summary.manual_only,
    );
    eprintln!();
    eprintln!(
        "Use with: konveyor-analyzer --rules {}",
        output_dir.display()
    );

    Ok(())
}

// ─── Report building ─────────────────────────────────────────────────────

fn build_report(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    structural_changes: Vec<StructuralChange>,
    behavioral_changes: Vec<BehavioralChange>,
    manifest_changes: Vec<ManifestChange>,
    llm_api_changes: Vec<orchestrator::LlmApiChangeEntry>,
    old_surface: &semver_analyzer_core::ApiSurface,
    new_surface: &semver_analyzer_core::ApiSurface,
    inferred_rename_patterns: Option<semver_analyzer_core::InferredRenamePatterns>,
) -> AnalysisReport {
    // Group breaking structural changes by file, converting to v2 ApiChange format.
    // Non-breaking changes (symbol_added, etc.) are excluded from the report.
    let mut file_api_map: std::collections::BTreeMap<std::path::PathBuf, Vec<ApiChange>> =
        std::collections::BTreeMap::new();

    for change in &structural_changes {
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
    for entry in &llm_api_changes {
        let file = std::path::PathBuf::from(&entry.file_path);
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
        // Convert LLM removal disposition to core type
        let removal_disposition = entry.removal_disposition.as_ref().map(|d| {
            use semver_analyzer_llm::invoke::LlmRemovalDisposition;
            match d {
                LlmRemovalDisposition::MovedToChild {
                    target_component,
                    mechanism,
                } => semver_analyzer_core::RemovalDisposition::MovedToChild {
                    target_component: target_component.clone(),
                    mechanism: mechanism.clone(),
                },
                LlmRemovalDisposition::ReplacedByProp { new_prop } => {
                    semver_analyzer_core::RemovalDisposition::ReplacedByProp {
                        new_prop: new_prop.clone(),
                    }
                }
                LlmRemovalDisposition::MadeAutomatic => {
                    semver_analyzer_core::RemovalDisposition::MadeAutomatic
                }
                LlmRemovalDisposition::TrulyRemoved => {
                    semver_analyzer_core::RemovalDisposition::TrulyRemoved
                }
            }
        });

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
        let mut disposition_by_prop: std::collections::HashMap<
            String,
            semver_analyzer_core::RemovalDisposition,
        > = std::collections::HashMap::new();
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
                        && change.change == semver_analyzer_core::ApiChangeType::Removed
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
        &structural_changes,
        &behavioral_changes,
        old_surface,
        new_surface,
        &llm_api_changes,
    );

    // Merge behavioral changes into file map using the source_file
    // extracted from the BU pipeline's qualified names.
    let mut file_behavioral_map: std::collections::BTreeMap<
        std::path::PathBuf,
        Vec<BehavioralChange>,
    > = std::collections::BTreeMap::new();
    for bc in behavioral_changes {
        let file = if let Some(ref src) = bc.source_file {
            std::path::PathBuf::from(src)
        } else {
            std::path::PathBuf::from("(behavioral)")
        };
        file_behavioral_map.entry(file).or_default().push(bc);
    }

    // Build the combined file changes list
    let mut all_files: std::collections::BTreeSet<std::path::PathBuf> =
        std::collections::BTreeSet::new();
    all_files.extend(file_api_map.keys().cloned());
    all_files.extend(file_behavioral_map.keys().cloned());

    let changes: Vec<FileChanges> = all_files
        .into_iter()
        .map(|file| {
            let api_changes = file_api_map.remove(&file).unwrap_or_default();
            let behavioral = file_behavioral_map.remove(&file).unwrap_or_default();

            let status = if api_changes
                .iter()
                .all(|c| c.change == semver_analyzer_core::ApiChangeType::Removed)
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
    // These are new exports (new components, new modules) that consumers
    // may need to adopt when migrating from from_ref to to_ref.
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
        manifest_changes,
        added_files,
        packages,
        member_renames: std::collections::HashMap::new(), // Populated in Phase 2e
        inferred_rename_patterns,
        hierarchy_deltas: Vec::new(), // Populated by hierarchy inference phase
        metadata: AnalysisMetadata {
            call_graph_analysis: call_graph_info.to_string(),
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            llm_usage: None,
        },
    }
}

// ─── Package/Component summary building ──────────────────────────────────

/// Build hierarchical package summaries from the API surfaces and change lists.
///
/// This is the core of the report redesign: using the full AST data (Symbol
/// trees with members) to compute per-component views that downstream code
/// (rule generators) can read directly without reconstruction.
///
/// Replaces the 6 compensations in the rule generator:
/// 1. P0-C component aggregation → ComponentSummary with removal_ratio
/// 2. detect_collapsible_constant_groups → ConstantGroup entries
/// 3. New sibling detection → ChildComponent entries
/// 4. resolve_npm_package → PackageChanges.name
/// 5. CSS suffix extraction → ConstantGroup.suffix_renames
/// 6. analyze_token_members → computed from surfaces directly
fn build_package_summaries(
    structural_changes: &[StructuralChange],
    behavioral_changes: &[BehavioralChange],
    old_surface: &semver_analyzer_core::ApiSurface,
    new_surface: &semver_analyzer_core::ApiSurface,
    llm_api_changes: &[orchestrator::LlmApiChangeEntry],
) -> Vec<semver_analyzer_core::PackageChanges> {
    use semver_analyzer_core::{
        AddedComponent, ChangeSubject, ComponentStatus, ComponentSummary,
        ConstantGroup, PackageChanges, PropertySummary, RemovalDisposition, RemovedProperty,
        StructuralChangeType, SymbolKind, TypeChange,
    };
    use std::collections::{BTreeMap, HashMap, HashSet};

    if old_surface.symbols.is_empty() && new_surface.symbols.is_empty() {
        return Vec::new();
    }

    // ── Step 1: Resolve package names from qualified_name paths ──────

    // Build package directory → npm package name mapping.
    // qualified_names look like: packages/react-core/dist/esm/components/Card/Card.CardProps
    // We extract "react-core" and map it to the full npm name.
    // For now, use the directory name as the package name — the rule generator
    // will have the full npm name from build_package_info_cache if needed.
    let resolve_package = |qualified_name: &str| -> Option<String> {
        let parts: Vec<&str> = qualified_name.split('/').collect();
        if let Some(pkg_idx) = parts.iter().position(|&p| p == "packages") {
            if pkg_idx + 1 < parts.len() {
                return Some(parts[pkg_idx + 1].to_string());
            }
        }
        // Fallback: use the first path component
        if !parts.is_empty() && parts.len() > 1 {
            Some(parts[0].to_string())
        } else {
            None
        }
    };

    // ── Step 2: Index structural changes by parent qualified_name ────

    // Build a map: parent_qualified_name → Vec<&StructuralChange>
    // For member-level changes (dotted qnames), the parent is everything before
    // the last dot. For top-level changes, the symbol IS the parent.
    let mut changes_by_parent: HashMap<String, Vec<&StructuralChange>> = HashMap::new();
    let mut top_level_changes: Vec<&StructuralChange> = Vec::new();

    for change in structural_changes {
        // Count dots in the qualified_name after the file stem
        // Format: "packages/.../Card.CardProps.isFlat"
        //   - "packages/.../Card.CardProps" is parent
        //   - "isFlat" is member
        let qn = &change.qualified_name;
        if let Some((parent_qn, _member)) = qn.rsplit_once('.') {
            // Check if this is a member-level change (parent has a dot too = file.Interface.Member)
            if parent_qn.contains('.') {
                changes_by_parent
                    .entry(parent_qn.to_string())
                    .or_default()
                    .push(change);
            } else {
                // Only one dot: file.Symbol — this is a top-level symbol change
                top_level_changes.push(change);
            }
        } else {
            top_level_changes.push(change);
        }
    }

    // ── Step 3: Index old/new surface symbols by qualified_name ──────

    let _old_by_qn: HashMap<&str, &semver_analyzer_core::Symbol> = old_surface
        .symbols
        .iter()
        .map(|s| (s.qualified_name.as_str(), s))
        .collect();

    let _new_by_qn: HashMap<&str, &semver_analyzer_core::Symbol> = new_surface
        .symbols
        .iter()
        .map(|s| (s.qualified_name.as_str(), s))
        .collect();

    // ── Step 3b: Build LLM removal disposition lookup ─────────────────
    //
    // Index LLM-detected API changes by symbol name so we can look up
    // removal_disposition when building RemovedProperty entries.
    // Key: "InterfaceName.propName" → LlmApiChangeEntry
    let llm_disposition_map: HashMap<&str, &orchestrator::LlmApiChangeEntry> = llm_api_changes
        .iter()
        .filter(|e| e.removal_disposition.is_some())
        .map(|e| (e.symbol.as_str(), e))
        .collect();

    // ── Step 4: Build component summaries ────────────────────────────

    // Find all interface/class symbols that have members (potential components).
    // We look at the OLD surface since we want to know what existed before.
    let mut package_map: BTreeMap<String, PackageChanges> = BTreeMap::new();

    // Process interfaces/classes with members from the old surface
    for old_sym in &old_surface.symbols {
        // Only process interfaces and classes with members
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

        // Get member-level changes for this interface
        let member_changes = changes_by_parent.get(&old_sym.qualified_name);

        // Also check if the interface itself was removed/renamed at the top level
        let self_change = top_level_changes.iter().find(|c| {
            c.qualified_name == old_sym.qualified_name
                && (matches!(c.change_type, StructuralChangeType::Removed(ChangeSubject::Symbol { .. }))
                    || matches!(c.change_type, StructuralChangeType::Renamed { from: ChangeSubject::Symbol { .. }, .. }))
        });

        // Skip if no changes at all
        if member_changes.is_none() && self_change.is_none() {
            continue;
        }

        let interface_name = &old_sym.name;
        let component_name = interface_name
            .strip_suffix("Props")
            .unwrap_or(interface_name)
            .to_string();

        let total_members = old_sym.members.len();

        // Count changes by type
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
                        // Look up LLM-provided removal disposition
                        let lookup_key = format!("{}.{}", interface_name, change.symbol);
                        let disposition = llm_disposition_map
                            .get(lookup_key.as_str())
                            .and_then(|entry| {
                                entry.removal_disposition.as_ref().map(|d| {
                                    use semver_analyzer_llm::invoke::LlmRemovalDisposition;
                                    match d {
                                        LlmRemovalDisposition::MovedToChild {
                                            target_component,
                                            mechanism,
                                        } => RemovalDisposition::MovedToChild {
                                            target_component: target_component.clone(),
                                            mechanism: mechanism.clone(),
                                        },
                                        LlmRemovalDisposition::ReplacedByProp { new_prop } => {
                                            RemovalDisposition::ReplacedByProp {
                                                new_prop: new_prop.clone(),
                                            }
                                        }
                                        LlmRemovalDisposition::MadeAutomatic => {
                                            RemovalDisposition::MadeAutomatic
                                        }
                                        LlmRemovalDisposition::TrulyRemoved => {
                                            RemovalDisposition::TrulyRemoved
                                        }
                                    }
                                })
                            });
                        removed_properties.push(RemovedProperty {
                            name: change.symbol.clone(),
                            old_type: change.before.clone(),
                            removal_disposition: disposition,
                        });
                    }
                    StructuralChangeType::Renamed { from: ChangeSubject::Member { .. }, .. } => {
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
                    _ => {
                        // Other change types (visibility, modifiers, etc.)
                        // count as modifications but not removals
                    }
                }
            }
        }

        let removal_ratio = if total_members > 0 {
            removed as f64 / total_members as f64
        } else {
            0.0
        };

        // Determine component status.
        //
        // When the props interface was removed (self_change is Some), verify
        // the derived component name doesn't still exist in the new API
        // surface before marking it Removed. A helper interface like
        // `IconProps` in `EmptyStateIcon.tsx` can be removed without the
        // actual `Icon` component being removed.
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

        // Get migration target from self or any Removed(Symbol) change with migration_target
        let migration_target = self_change
            .and_then(|c| c.migration_target.clone())
            .or_else(|| {
                top_level_changes.iter().find_map(|c| {
                    if c.qualified_name == old_sym.qualified_name
                        && matches!(c.change_type, StructuralChangeType::Removed(ChangeSubject::Symbol { .. }))
                        && c.migration_target.is_some()
                    {
                        c.migration_target.clone()
                    } else {
                        None
                    }
                })
            });

        // Cross-reference behavioral changes by component name
        let component_behavioral: Vec<BehavioralChange> = behavioral_changes
            .iter()
            .filter(|bc| {
                bc.symbol == component_name
                    || bc.symbol == *interface_name
                    || bc
                        .referenced_components
                        .iter()
                        .any(|r| r == &component_name)
            })
            .cloned()
            .collect();

        // Source files from the qualified_name
        let source_file = old_sym
            .qualified_name
            .split('.')
            .next()
            .map(std::path::PathBuf::from);

        // Discover child components by scanning the new API surface for
        // components that share the parent's name prefix and directory.
        // Cross-references removed props against child AST members to build
        // exact prop→child mappings. Uses LLM removal_disposition data to
        // catch cases where a prop becomes children of a child component
        // (e.g., Modal.actions → ModalFooter children).
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
            expected_children: Vec::new(), // Populated by hierarchy inference phase
            source_files: source_file.into_iter().collect(),
        };

        let pkg_entry = package_map.entry(pkg_name.clone()).or_insert_with(|| {
            PackageChanges {
                name: pkg_name,
                old_version: None,
                new_version: None,
                components: Vec::new(),
                constants: Vec::new(),
                added_components: Vec::new(),
            }
        });
        pkg_entry.components.push(summary);
    }

    // ── Step 5: Build constant groups ────────────────────────────────

    // Group non-dotted constant changes by (package, change_type)
    let mut constant_groups: HashMap<(String, semver_analyzer_core::ApiChangeType), Vec<String>> =
        HashMap::new();

    for change in structural_changes {
        if !change.is_breaking {
            continue;
        }
        // Only constants
        if change.kind != SymbolKind::Constant && change.kind != SymbolKind::Variable {
            continue;
        }
        // Skip dotted symbols (those are interface members handled above)
        let after_file = change
            .qualified_name
            .rsplit('/')
            .next()
            .unwrap_or(&change.qualified_name);
        let dot_count = after_file.chars().filter(|c| *c == '.').count();
        if dot_count > 1 {
            // file.Parent.Member — skip
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

    // Only keep groups with >= 10 members (the collapse threshold)
    let constant_collapse_threshold = 10;
    for ((pkg_name, change_type), symbols) in &constant_groups {
        if symbols.len() < constant_collapse_threshold {
            continue;
        }

        // Build prefix pattern from symbol names
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
            suffix_renames: Vec::new(), // Populated below if member_renames available
        };

        let pkg_entry = package_map.entry(pkg_name.clone()).or_insert_with(|| {
            PackageChanges {
                name: pkg_name.clone(),
                old_version: None,
                new_version: None,
                components: Vec::new(),
                constants: Vec::new(),
                added_components: Vec::new(),
            }
        });
        pkg_entry.constants.push(group);
    }

    // ── Step 6: Discover added components ────────────────────────────

    // Find symbols that exist in new_surface but not old_surface
    // and are PascalCase interfaces/classes (likely new components)
    let old_qnames: HashSet<&str> = old_surface
        .symbols
        .iter()
        .map(|s| s.qualified_name.as_str())
        .collect();

    for new_sym in &new_surface.symbols {
        if old_qnames.contains(new_sym.qualified_name.as_str()) {
            continue;
        }
        // Must be a PascalCase interface or class
        match new_sym.kind {
            SymbolKind::Interface | SymbolKind::Class | SymbolKind::Function => {}
            _ => continue,
        }
        if !is_pascal_case(&new_sym.name) {
            continue;
        }
        // Skip Props/Variants interfaces — they're associated with components
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

        let pkg_entry = package_map.entry(pkg_name.clone()).or_insert_with(|| {
            PackageChanges {
                name: pkg_name,
                old_version: None,
                new_version: None,
                components: Vec::new(),
                constants: Vec::new(),
                added_components: Vec::new(),
            }
        });
        pkg_entry.added_components.push(added);
    }

    package_map.into_values().collect()
}

/// Discover child/sibling components for a given parent component.
///
/// Looks for symbols in the new surface that:
/// - Are in the same directory as the parent
/// - Have PascalCase names
/// - Were either added (not in old surface) or modified
fn discover_child_components(
    component_name: &str,
    parent_qn: &str,
    old_surface: &semver_analyzer_core::ApiSurface,
    new_surface: &semver_analyzer_core::ApiSurface,
    structural_changes: &[StructuralChange],
    _behavioral_changes: &[BehavioralChange],
    removed_prop_names: &[&str],
    removed_properties: &[semver_analyzer_core::RemovedProperty],
) -> Vec<semver_analyzer_core::ChildComponent> {
    use semver_analyzer_core::{
        ChangeSubject, ChildComponent, ChildComponentStatus, RemovalDisposition, StructuralChangeType,
    };
    use std::collections::{BTreeMap, HashSet};

    // Extract directory from parent's qualified_name
    // e.g., "packages/react-core/src/components/Modal/Modal.ModalProps"
    //     → "packages/react-core/src/components/Modal"
    let parent_dir = parent_qn.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    if parent_dir.is_empty() {
        return Vec::new();
    }

    // Component directory family (e.g., "Modal" from ".../Modal/")
    let component_dir = parent_dir.rsplit('/').next().unwrap_or("");

    let old_qnames: HashSet<&str> = old_surface
        .symbols
        .iter()
        .map(|s| s.qualified_name.as_str())
        .collect();

    let removed_set: HashSet<&str> = removed_prop_names.iter().copied().collect();

    // Collect candidate child component symbols from the new surface.
    // A child component is a symbol that:
    //   1. Has a name starting with the parent component name (e.g., ModalHeader)
    //   2. Is a PascalCase component (not a Props/Variants type)
    //   3. Is in the same directory family OR was promoted from next/
    //   4. Is new (not in old surface) OR was renamed/promoted into this family
    let mut children_map: BTreeMap<String, ChildComponent> = BTreeMap::new();

    // ── Strategy: scan the new API surface directly ──
    //
    // The new surface's AST has the complete type information for every
    // exported symbol. We find child components by name prefix and
    // directory, then read their props from Symbol.members. No guessing.
    for sym in &new_surface.symbols {
        let name = &sym.name;
        if !name.starts_with(component_name) || name == component_name {
            continue;
        }
        if !is_child_component_candidate(name, component_name) {
            continue;
        }

        // Only consider component-like symbols (Variable = function component,
        // Class = class component, Function = function component,
        // Constant = `export const Foo = ...` function component).
        // Skip enums, interfaces, type aliases -- those are types,
        // not renderable components.
        match sym.kind {
            semver_analyzer_core::SymbolKind::Variable
            | semver_analyzer_core::SymbolKind::Class
            | semver_analyzer_core::SymbolKind::Function
            | semver_analyzer_core::SymbolKind::Constant => {}
            _ => continue,
        }
        if children_map.contains_key(name) {
            continue;
        }

        // Check directory family match
        let sym_dir = sym
            .qualified_name
            .rsplit_once('/')
            .map(|(dir, _)| dir)
            .unwrap_or("");
        let in_family = sym_dir.ends_with(&format!("/{}", component_dir))
            || sym_dir == parent_dir;
        if !in_family {
            continue;
        }

        // Skip symbols from deprecated/ paths -- these are backward-compat
        // re-exports of old internal components, not new child components.
        if sym.qualified_name.contains("/deprecated/") {
            continue;
        }

        // Determine if this is new: either not in old surface or promoted
        let is_new = !old_qnames.contains(sym.qualified_name.as_str());
        let is_promoted = structural_changes.iter().any(|c| {
            matches!(c.change_type, StructuralChangeType::Renamed { from: ChangeSubject::Symbol { .. }, .. })
                && c.symbol == *name
                && c.after
                    .as_ref()
                    .map(|a| a.contains(component_dir))
                    .unwrap_or(false)
        });

        if !is_new && !is_promoted {
            continue;
        }

        // Read props directly from the AST members
        let known_props: Vec<String> = sym.members.iter().map(|m| m.name.clone()).collect();

        // Also check the Props interface (e.g., ModalHeaderProps) for
        // additional member info
        let props_iface_name = format!("{}Props", name);
        let props_members: Vec<String> = new_surface
            .symbols
            .iter()
            .find(|s| s.name == props_iface_name && s.qualified_name.contains(component_dir))
            .map(|s| s.members.iter().map(|m| m.name.clone()).collect())
            .unwrap_or_default();

        // Merge props from both the component and its Props interface
        let mut all_props: HashSet<String> = known_props.into_iter().collect();
        all_props.extend(props_members);
        let all_props_sorted: Vec<String> = {
            let mut v: Vec<String> = all_props.into_iter().collect();
            v.sort();
            v
        };

        // Cross-reference: which removed parent props does this child absorb?
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
    //
    // The AST name-matching above catches cases where a removed prop has
    // the same name on a child component (e.g., title → ModalHeader.title).
    // The LLM removal_disposition catches cases where a prop becomes
    // children of a child component (e.g., actions → ModalFooter children).
    // Merge these in.
    for rp in removed_properties {
        if let Some(RemovalDisposition::MovedToChild {
            target_component,
            mechanism,
        }) = &rp.removal_disposition
        {
            if let Some(child) = children_map.get_mut(target_component) {
                // Add to absorbed_props if not already there
                if !child.absorbed_props.contains(&rp.name) {
                    child.absorbed_props.push(rp.name.clone());
                    child.absorbed_props.sort();
                }
            } else {
                // LLM says prop moves to a child we didn't find in the surface.
                // Create a placeholder entry so the rule message includes it.
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

/// Check if a symbol name is a plausible child component of a parent component.
/// This is a name-based pre-filter; the caller should also check `Symbol.kind`
/// to ensure the symbol is actually a component (Variable/Class/Function).
fn is_child_component_candidate(name: &str, parent_name: &str) -> bool {
    if !is_pascal_case(name) {
        return false;
    }
    if name == parent_name {
        return false;
    }
    // Skip Props interfaces -- these are type definitions, not components.
    // (Enums, type aliases, etc. are filtered by Symbol.kind in the caller.)
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
///
/// Looks for PascalCase trailing suffixes in token member names
/// (e.g., "c_table__caption_PaddingTop" → "PaddingTop") and detects
/// when the old suffix maps to a different new suffix.
fn extract_suffix_renames(
    member_renames: &std::collections::HashMap<String, String>,
) -> Vec<semver_analyzer_core::SuffixRename> {
    use std::collections::BTreeMap;

    let mut suffix_map: BTreeMap<String, String> = BTreeMap::new();
    for (old_name, new_name) in member_renames {
        let old_suffix = extract_trailing_suffix(old_name);
        let new_suffix = extract_trailing_suffix(new_name);
        if let (Some(old_s), Some(new_s)) = (old_suffix, new_suffix) {
            if old_s != new_s {
                suffix_map.entry(old_s.to_string()).or_insert_with(|| new_s.to_string());
            }
        }
    }
    suffix_map
        .into_iter()
        .map(|(from, to)| semver_analyzer_core::SuffixRename { from, to })
        .collect()
}

/// Extract the trailing PascalCase suffix from a token name.
///
/// e.g., "c_table__caption_PaddingTop" → Some("PaddingTop")
/// e.g., "c_button_after_Color" → Some("Color")
/// e.g., "c_button" → None (no PascalCase suffix)
fn extract_trailing_suffix(name: &str) -> Option<&str> {
    let last_underscore = name.rfind('_')?;
    let suffix = &name[last_underscore + 1..];
    if !suffix.is_empty()
        && suffix.chars().next().map_or(false, |c| c.is_ascii_uppercase())
        && suffix.chars().any(|c| c.is_ascii_lowercase())
        && !suffix.contains('_')
    {
        Some(suffix)
    } else {
        None
    }
}

/// Build a regex prefix pattern from a list of constant symbol names.
///
/// Extracts the first `_`-delimited segment from each name and builds
/// a pattern like `^(c_|global_|chart_)\w+$`.
fn build_constant_prefix_pattern(symbols: &[String]) -> String {
    use std::collections::BTreeSet;
    let mut prefixes = BTreeSet::new();
    for name in symbols {
        if let Some(idx) = name.find('_') {
            prefixes.insert(&name[..=idx]); // includes the underscore
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
///
/// Returns paths of `.ts`/`.tsx` files that exist at `to_ref` but not at
/// `from_ref`.  These represent new exports (components, modules) that
/// consumers may need to use when migrating.
fn collect_added_files(repo: &Path, from_ref: &str, to_ref: &str) -> Vec<std::path::PathBuf> {
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
            let files: Vec<std::path::PathBuf> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|line| {
                    // Format: "A\tpath/to/file.tsx"
                    let path = line.strip_prefix("A\t")?;
                    // Only include source files, not tests/examples/stories/dist
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
                    Some(std::path::PathBuf::from(path))
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

    // Build the v2 symbol name: `ComponentName.propName` format.
    // The qualified_name is like "packages/react-core/dist/esm/components/Card/Card.CardProps.isFlat"
    // We want: "Card.isFlat" or just "Card" for top-level symbols.
    let symbol = qualified_name_to_display_symbol(&sc.qualified_name, &sc.symbol);

    ApiChange {
        symbol,
        kind,
        change,
        before: sc.before.clone(),
        after: sc.after.clone(),
        description: sc.description.clone(),
        migration_target: sc.migration_target.clone(),
        removal_disposition: None, // Structural analysis doesn't determine disposition
        renders_element: None,     // Determined by LLM analysis
    }
}

/// Map internal SymbolKind to v2 ApiChangeKind.
fn symbol_kind_to_api_kind(kind: semver_analyzer_core::SymbolKind) -> ApiChangeKind {
    use semver_analyzer_core::SymbolKind;
    match kind {
        SymbolKind::Function => ApiChangeKind::Function,
        SymbolKind::Method => ApiChangeKind::Method,
        SymbolKind::Class => ApiChangeKind::Class,
        SymbolKind::Interface => ApiChangeKind::Interface,
        SymbolKind::TypeAlias => ApiChangeKind::TypeAlias,
        SymbolKind::Constant | SymbolKind::Variable => ApiChangeKind::Constant,
        SymbolKind::Enum => ApiChangeKind::TypeAlias, // v2 uses type_alias for enums
        SymbolKind::Property => ApiChangeKind::Property,
        SymbolKind::Namespace => ApiChangeKind::ModuleExport,
        SymbolKind::Struct => ApiChangeKind::Class,
        SymbolKind::EnumMember => ApiChangeKind::Property,
        SymbolKind::Constructor => ApiChangeKind::Method,
        SymbolKind::GetAccessor | SymbolKind::SetAccessor => ApiChangeKind::Property,
    }
}

/// Convert a qualified name to a human-readable display symbol.
///
/// Input: `packages/react-core/dist/esm/components/Card/Card.CardProps.isFlat`
/// Output: `CardProps.isFlat`
///
/// Input: `packages/react-core/dist/esm/components/Card/Card.Card`
/// Output: `Card`
///
/// Strategy: take the parts after the last path separator (the `.`-separated
/// symbol chain from the file stem), then drop the file stem if it matches
/// the first symbol part.
fn qualified_name_to_display_symbol(qualified_name: &str, symbol_name: &str) -> String {
    // Split by '/' to get path components, then take the last one which
    // contains the file stem + symbol chain
    let last_path = qualified_name.rsplit('/').next().unwrap_or(qualified_name);

    // Split by '.' to get [file_stem, symbol_parts...]
    let parts: Vec<&str> = last_path.split('.').collect();

    if parts.len() <= 1 {
        return symbol_name.to_string();
    }

    // parts[0] is the file stem (e.g., "Card"), parts[1..] are symbol parts
    let symbol_parts = &parts[1..];

    if symbol_parts.is_empty() {
        return symbol_name.to_string();
    }

    // If there's only one symbol part, it's the top-level symbol
    if symbol_parts.len() == 1 {
        return symbol_parts[0].to_string();
    }

    // Multiple parts: join with '.' (e.g., "CardProps.isFlat")
    // But use Component name instead of Props interface name where possible:
    // "CardProps.isFlat" → "Card.isFlat" if the first part ends with "Props"
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
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .ok()
    } else {
        None
    }
}

// ─── Output helpers ──────────────────────────────────────────────────────

fn write_json_output(value: &impl serde::Serialize, output: Option<&Path>) -> Result<()> {
    let json = serde_json::to_string_pretty(value)?;
    if let Some(path) = output {
        std::fs::write(path, &json)
            .with_context(|| format!("Failed to write output to {}", path.display()))?;
        eprintln!("Output written to {}", path.display());
    } else {
        println!("{}", json);
    }
    Ok(())
}

fn count_unique_files(surface: &ApiSurface) -> usize {
    let files: std::collections::HashSet<&std::path::Path> =
        surface.symbols.iter().map(|s| s.file.as_path()).collect();
    files.len()
}

/// Convert a qualified name to a file path.
///
/// Qualified names use the form `<file_stem>.<SymbolParts>` where the file
/// stem had its `.d.ts` extension stripped during extraction.  After the
/// dist-to-src remapping, the stem references `src/` paths, so we reconstruct
/// the source file name rather than the `.d.ts` declaration.
///
/// Examples:
///   `packages/react-core/src/components/Button/Button.ButtonProps`
///     → `packages/react-core/src/components/Button/Button.d.ts`
///
/// The `.d.ts` extension is kept because the `qualified_name` was built from
/// a `.d.ts` file (with double extension stripping: `.ts` then `.d`).  The
/// actual source file on disk may be `.tsx` or `.ts`, but the file path in
/// the report is a logical identifier — not a filesystem lookup.
fn qualified_name_to_file(qualified_name: &str) -> std::path::PathBuf {
    if let Some(dot_pos) = qualified_name.rfind('.') {
        let file_part = &qualified_name[..dot_pos];
        std::path::PathBuf::from(format!("{}.d.ts", file_part))
    } else {
        std::path::PathBuf::from(qualified_name)
    }
}

// ─── Hierarchy Delta Enrichment ──────────────────────────────────────────

/// Enrich hierarchy deltas with prop migration data and populate
/// `expected_children` on each `ComponentSummary`.
///
/// For each hierarchy delta:
///  1. Find the parent component's `ComponentSummary` and its `removed_properties`.
///  2. For each added child, check if the child component's props (from the new
///     surface AST) include any of the removed parent props.
///  3. Store the matches as `MigratedProp` entries on the delta.
///  4. Populate `expected_children` on the parent's `ComponentSummary`.
fn enrich_hierarchy_deltas(
    report: &mut semver_analyzer_core::AnalysisReport,
    mut deltas: Vec<semver_analyzer_core::HierarchyDelta>,
    new_surface: &semver_analyzer_core::ApiSurface,
    new_hierarchies: &std::collections::HashMap<
        String,
        std::collections::HashMap<String, Vec<semver_analyzer_llm::LlmExpectedChild>>,
    >,
) {
    use semver_analyzer_core::{ExpectedChild, MigratedProp, SymbolKind};
    use std::collections::{HashMap, HashSet};

    // Build a lookup of component name → props from the new surface.
    // For each exported interface/class ending in "Props", extract member names.
    // Also check the component symbol's own members.
    let mut component_props: HashMap<String, HashSet<String>> = HashMap::new();

    for sym in &new_surface.symbols {
        // Interface/TypeAlias: FooProps → component "Foo" gets these props
        if matches!(sym.kind, SymbolKind::Interface | SymbolKind::TypeAlias) {
            if let Some(comp_name) = sym.name.strip_suffix("Props") {
                let props: HashSet<String> = sym.members.iter().map(|m| m.name.clone()).collect();
                component_props
                    .entry(comp_name.to_string())
                    .or_default()
                    .extend(props);
            }
        }

        // Component symbol itself may have members (from destructured props)
        if matches!(
            sym.kind,
            SymbolKind::Variable
                | SymbolKind::Function
                | SymbolKind::Class
                | SymbolKind::Constant
        ) && sym.name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
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
        // Find the parent component's removed properties
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

        // For each added child, check if its props overlap with removed parent props
        for child in &delta.added_children {
            let child_props = match component_props.get(&child.name) {
                Some(props) => props,
                None => continue,
            };

            for removed_prop in &removed_props {
                // Direct name match: parent had "title", child has "title"
                if child_props.contains(removed_prop) {
                    delta.migrated_props.push(MigratedProp {
                        prop_name: removed_prop.clone(),
                        target_child: child.name.clone(),
                        target_prop_name: None, // same name
                    });
                }
                // TODO: fuzzy matching (bodyAriaRole → role) could be added
                // later via the purpose field from LLM or string similarity
            }
        }
    }

    // Populate expected_children on ComponentSummary entries from the FULL
    // new-version hierarchy (not just deltas). This ensures every component
    // gets its complete expected_children list, including children that
    // existed in both old and new versions (no delta).
    //
    // When a component from the hierarchy inference doesn't have a
    // ComponentSummary (because it has no breaking API changes), create
    // one so that conformance rules can be generated for it.
    for (family, family_hierarchy) in new_hierarchies {
        for (comp_name, children) in family_hierarchy {
            if children.is_empty() {
                continue;
            }

            let expected: Vec<ExpectedChild> = children
                .iter()
                .map(|c| ExpectedChild {
                    name: c.name.clone(),
                    required: c.required,
                })
                .collect();

            // Try to find existing ComponentSummary
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

            // If no ComponentSummary exists, create one in the package that
            // contains sibling components from the same family. The family
            // name (e.g., "Masthead") matches the directory where the
            // component lives, so look for a package that already has a
            // component whose name starts with the family prefix.
            if !found {
                // First pass: find the target package index
                let target_idx = report
                    .packages
                    .iter()
                    .position(|p| {
                        p.components
                            .iter()
                            .any(|c| c.name.starts_with(family))
                    })
                    .or_else(|| {
                        report
                            .packages
                            .iter()
                            .position(|p| !p.components.is_empty())
                    });

                if let Some(idx) = target_idx {
                    report.packages[idx]
                        .components
                        .push(semver_analyzer_core::ComponentSummary {
                            name: comp_name.clone(),
                            interface_name: format!("{}Props", comp_name),
                            status: semver_analyzer_core::ComponentStatus::Modified,
                            property_summary: semver_analyzer_core::PropertySummary::default(),
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
    //
    // When the LLM hierarchy inference returns empty expected_children
    // for a component (non-deterministic), check if the component's Props
    // interface extends a base component that DOES have expected_children.
    // If so, infer the wrapper's children by mapping through the extends
    // chain (e.g., DropdownProps extends MenuProps, MenuList → DropdownList).
    //
    // This provides a deterministic fallback for the delegation pattern
    // (Dropdown/Select/etc. wrapping Menu).
    {
        // Build extends map: "DropdownProps" → "MenuProps"
        // Handles Omit<MenuGroupProps, 'ref'> → extracts "MenuGroupProps"
        let mut extends_map: HashMap<String, String> = HashMap::new();
        for sym in &new_surface.symbols {
            if matches!(sym.kind, SymbolKind::Interface | SymbolKind::TypeAlias) {
                if let Some(ref parent) = sym.extends {
                    // Extract the base type from utility wrappers like Omit<T, K>
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

        // Build component→expected_children lookup from current report data
        let mut comp_children: HashMap<String, Vec<ExpectedChild>> = HashMap::new();
        for pkg in &report.packages {
            for comp in &pkg.components {
                if !comp.expected_children.is_empty() {
                    comp_children.insert(comp.name.clone(), comp.expected_children.clone());
                }
            }
        }

        // Build reverse map: base interface → base component name
        // e.g., "MenuProps" → "Menu", "MenuListProps" → "MenuList"
        let mut interface_to_component: HashMap<String, String> = HashMap::new();
        for pkg in &report.packages {
            for comp in &pkg.components {
                interface_to_component.insert(comp.interface_name.clone(), comp.name.clone());
            }
        }

        // For each component with empty expected_children, try the extends fallback
        let mut inferred_count = 0;
        let mut inferred: Vec<(String, Vec<ExpectedChild>)> = Vec::new();

        for pkg in &report.packages {
            for comp in &pkg.components {
                if !comp.expected_children.is_empty() {
                    continue;
                }

                // Check if this component's Props extends a base Props
                let base_interface = match extends_map.get(&comp.interface_name) {
                    Some(b) => b,
                    None => continue,
                };

                // Find the base component name
                let base_component = match interface_to_component.get(base_interface) {
                    Some(c) => c,
                    None => continue,
                };

                // Does the base component have expected_children?
                let base_children = match comp_children.get(base_component) {
                    Some(c) if !c.is_empty() => c,
                    _ => continue,
                };

                // Map base children to wrapper family equivalents.
                // For each base child (e.g., MenuList), find a component in the
                // same family whose Props extends that base child's Props.
                // "Same family" = same name prefix as the current component.
                //
                // When a base child has no wrapper equivalent but HAS its own
                // expected_children, recurse one level — the wrapper might
                // render the base child internally (e.g., Dropdown renders
                // MenuContent internally, so we need MenuContent's children
                // — MenuList, MenuGroup — mapped to DropdownList, DropdownGroup).
                let prefix = &comp.name; // e.g., "Dropdown"
                let mut mapped_children: Vec<ExpectedChild> = Vec::new();

                let mut base_queue: Vec<&ExpectedChild> = base_children.iter().collect();
                let mut seen_bases: HashSet<String> = HashSet::new();

                while let Some(base_child) = base_queue.pop() {
                    if !seen_bases.insert(base_child.name.clone()) {
                        continue;
                    }

                    let base_child_interface = format!("{}Props", base_child.name);

                    // Find a wrapper component whose Props extends this base child's Props
                    let wrapper = extends_map.iter().find(|(iface, parent)| {
                        *parent == &base_child_interface && iface.starts_with(prefix)
                    });

                    if let Some((wrapper_iface, _)) = wrapper {
                        let wrapper_name = wrapper_iface
                            .strip_suffix("Props")
                            .unwrap_or(wrapper_iface);
                        mapped_children.push(ExpectedChild {
                            name: wrapper_name.to_string(),
                            required: base_child.required,
                        });
                    } else {
                        // No wrapper for this base child — check if the base child
                        // has its own expected_children (the wrapper component might
                        // render this base child internally).
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

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use semver_analyzer_core::StructuralChangeType;

    #[test]
    fn qualified_name_to_file_simple() {
        assert_eq!(
            qualified_name_to_file("test.greet"),
            std::path::PathBuf::from("test.d.ts")
        );
    }

    #[test]
    fn qualified_name_to_file_nested() {
        assert_eq!(
            qualified_name_to_file("src/api/users.createUser"),
            std::path::PathBuf::from("src/api/users.d.ts")
        );
    }

    #[test]
    fn qualified_name_to_file_class_member() {
        // "test.Foo.bar" → last dot separates the member name
        assert_eq!(
            qualified_name_to_file("test.Foo.bar"),
            std::path::PathBuf::from("test.Foo.d.ts")
        );
    }

    #[test]
    fn qualified_name_to_file_src_path() {
        // After dist-to-src remapping, qualified names use src/ paths
        assert_eq!(
            qualified_name_to_file(
                "packages/react-core/src/components/Button/Button.ButtonProps"
            ),
            std::path::PathBuf::from(
                "packages/react-core/src/components/Button/Button.d.ts"
            )
        );
    }

    #[test]
    fn build_report_empty() {
        let report = build_report(
            Path::new("/tmp/repo"),
            "v1.0.0",
            "v2.0.0",
            vec![],
            vec![],
            vec![],
            vec![],
            &semver_analyzer_core::ApiSurface { symbols: vec![] },
            &semver_analyzer_core::ApiSurface { symbols: vec![] },
            None,
        );
        assert_eq!(report.summary.total_breaking_changes, 0);
        assert!(report.changes.is_empty());
        assert!(report.manifest_changes.is_empty());
    }

    #[test]
    fn build_report_counts_breaking() {
        use semver_analyzer_core::{ChangeSubject, SymbolKind};
        let changes = vec![
            StructuralChange {
                symbol: "foo".into(),
                qualified_name: "test.foo".into(),
                kind: SymbolKind::Function,
                change_type: StructuralChangeType::Removed(ChangeSubject::Symbol { kind: SymbolKind::Function }),
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
                change_type: StructuralChangeType::Added(ChangeSubject::Symbol { kind: SymbolKind::Function }),
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
            change_type: semver_analyzer_core::ManifestChangeType::ModuleSystemChanged,
            before: Some("commonjs".into()),
            after: Some("module".into()),
            description: "CJS to ESM".into(),
            is_breaking: true,
        }];

        let report = build_report(
            Path::new("/tmp/repo"),
            "v1",
            "v2",
            changes,
            vec![], // no behavioral changes
            manifest,
            vec![],
            &semver_analyzer_core::ApiSurface { symbols: vec![] },
            &semver_analyzer_core::ApiSurface { symbols: vec![] },
            None,
        );
        // Only the breaking structural change counts (non-breaking excluded)
        assert_eq!(report.summary.breaking_api_changes, 1);
        assert_eq!(report.summary.breaking_behavioral_changes, 0);
        assert_eq!(report.summary.total_breaking_changes, 1);
        assert_eq!(report.summary.files_with_breaking_changes, 1);
        // Non-breaking changes excluded from output
        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].breaking_api_changes.len(), 1);
    }

    #[test]
    fn build_report_with_behavioral_changes() {
        use semver_analyzer_core::BehavioralChangeKind;

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

        let report = build_report(
            Path::new("/tmp/repo"),
            "v1",
            "v2",
            vec![],
            behavioral,
            vec![],
            vec![],
            &semver_analyzer_core::ApiSurface { symbols: vec![] },
            &semver_analyzer_core::ApiSurface { symbols: vec![] },
            None,
        );
        assert_eq!(report.summary.breaking_api_changes, 0);
        assert_eq!(report.summary.breaking_behavioral_changes, 1);
        assert_eq!(report.summary.total_breaking_changes, 1);
        assert_eq!(report.summary.files_with_breaking_changes, 1);
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
        // CardProps.isFlat → Card.isFlat
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
        // AccordionContent.isHidden (not a Props interface)
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

    // ── discover_child_components: filters non-component symbols ──────

    #[test]
    fn discover_child_components_filters_enums_and_types() {
        use semver_analyzer_core::{ApiSurface, Symbol, SymbolKind, Visibility};

        fn make_symbol(name: &str, kind: SymbolKind, dir: &str) -> Symbol {
            Symbol {
                name: name.to_string(),
                qualified_name: format!("{}/{}", dir, name),
                kind,
                visibility: Visibility::Exported,
                file: format!("{}/{}.d.ts", dir, name).into(),
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

        // Old surface: just Button
        let old_surface = ApiSurface {
            symbols: vec![make_symbol("Button", SymbolKind::Variable, dir)],
        };

        // New surface: Button + ButtonState (enum) + ButtonSize (enum) +
        // ButtonType (enum) + ButtonProps (interface) + ButtonGroup (component)
        let new_surface = ApiSurface {
            symbols: vec![
                make_symbol("Button", SymbolKind::Variable, dir),
                make_symbol("ButtonState", SymbolKind::Enum, dir),
                make_symbol("ButtonSize", SymbolKind::Enum, dir),
                make_symbol("ButtonType", SymbolKind::Enum, dir),
                make_symbol("ButtonProps", SymbolKind::Interface, dir),
                make_symbol("ButtonTypeAlias", SymbolKind::TypeAlias, dir),
                make_symbol("ButtonGroup", SymbolKind::Variable, dir), // actual component
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

        // Only ButtonGroup should be detected -- it's a Variable (component)
        assert!(
            child_names.contains(&"ButtonGroup"),
            "Should include ButtonGroup (Variable = component). Found: {:?}",
            child_names
        );

        // Enums, interfaces, type aliases should NOT be detected
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
        use semver_analyzer_core::{ApiSurface, Symbol, SymbolKind, Visibility};

        fn make_symbol(name: &str, kind: SymbolKind, qn: &str) -> Symbol {
            Symbol {
                name: name.to_string(),
                qualified_name: qn.to_string(),
                kind,
                visibility: Visibility::Exported,
                file: format!("{}.d.ts", qn).into(),
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
                make_symbol("Modal", SymbolKind::Variable, &format!("{}/Modal", main_dir)),
                // Public child (main path)
                make_symbol("ModalHeader", SymbolKind::Variable, &format!("{}/ModalHeader", main_dir)),
                // Deprecated re-export (should be excluded)
                make_symbol("ModalBox", SymbolKind::Variable, &format!("{}/ModalBox", depr_dir)),
                make_symbol("ModalContent", SymbolKind::Variable, &format!("{}/ModalContent", depr_dir)),
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

    // ── ComponentStatus: helper interface removal doesn't remove component ──

    #[test]
    fn helper_interface_removal_does_not_mark_component_removed() {
        // Scenario: IconProps (helper interface in EmptyStateIcon.tsx) is removed,
        // but the Icon component (in Icon/Icon.tsx) still exists in the new surface.
        // The component "Icon" should NOT be marked Removed.
        use semver_analyzer_core::{
            ApiSurface, Symbol, SymbolKind, Visibility, StructuralChangeType,
        };

        fn make_sym(name: &str, kind: SymbolKind, qn: &str) -> Symbol {
            Symbol {
                name: name.to_string(),
                qualified_name: qn.to_string(),
                kind,
                visibility: Visibility::Exported,
                file: format!("{}.d.ts", qn).into(),
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

        // Old surface: IconProps interface (in EmptyStateIcon file) + Icon variable (in Icon file)
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

        // New surface: IconProps REMOVED, but Icon component still exists
        let new_surface = ApiSurface {
            symbols: vec![make_sym(
                "Icon",
                SymbolKind::Variable,
                "packages/react-core/src/components/Icon/Icon.Icon",
            )],
        };

        // Structural change: IconProps was removed
        let structural_changes = vec![StructuralChange {
            symbol: "IconProps".to_string(),
            qualified_name: "packages/react-core/src/components/EmptyState/EmptyStateIcon.IconProps"
                .to_string(),
            kind: SymbolKind::Interface,
            change_type: StructuralChangeType::Removed(semver_analyzer_core::ChangeSubject::Symbol { kind: SymbolKind::Interface }),
            before: Some("IconProps".to_string()),
            after: None,
            description: "IconProps was removed".to_string(),
            is_breaking: true,
            impact: None,
            migration_target: None,
        }];

        let report = build_report(
            Path::new("/tmp/repo"),
            "v5",
            "v6",
            structural_changes,
            vec![],
            vec![],
            vec![],
            &old_surface,
            &new_surface,
            None,
        );

        // Find the "Icon" component in the report
        let icon_comp = report
            .packages
            .iter()
            .flat_map(|p| &p.components)
            .find(|c| c.name == "Icon");

        if let Some(comp) = icon_comp {
            assert_ne!(
                comp.status,
                semver_analyzer_core::ComponentStatus::Removed,
                "Icon should NOT be marked Removed when the component still exists in new surface. Status: {:?}",
                comp.status
            );
        }
        // If Icon doesn't appear in the report at all, that's also acceptable --
        // it means the helper interface was correctly identified as not being
        // a component's props.
    }

    #[test]
    fn truly_removed_component_still_marked_removed() {
        // Scenario: FooProps interface removed AND no Foo component in new surface.
        // The component "Foo" should be marked Removed.
        use semver_analyzer_core::{
            ApiSurface, Symbol, SymbolKind, Visibility, StructuralChangeType,
        };

        let old_surface = ApiSurface {
            symbols: vec![Symbol {
                name: "FooProps".to_string(),
                qualified_name: "src/components/Foo/Foo.FooProps".to_string(),
                kind: SymbolKind::Interface,
                visibility: Visibility::Exported,
                file: "src/components/Foo/Foo.FooProps.d.ts".into(),
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

        // New surface: Foo component does NOT exist
        let new_surface = ApiSurface { symbols: vec![] };

        let structural_changes = vec![StructuralChange {
            symbol: "FooProps".to_string(),
            qualified_name: "src/components/Foo/Foo.FooProps".to_string(),
            kind: SymbolKind::Interface,
            change_type: StructuralChangeType::Removed(semver_analyzer_core::ChangeSubject::Symbol { kind: SymbolKind::Interface }),
            before: Some("FooProps".to_string()),
            after: None,
            description: "FooProps was removed".to_string(),
            is_breaking: true,
            impact: None,
            migration_target: None,
        }];

        let report = build_report(
            Path::new("/tmp/repo"),
            "v5",
            "v6",
            structural_changes,
            vec![],
            vec![],
            vec![],
            &old_surface,
            &new_surface,
            None,
        );

        let foo_comp = report
            .packages
            .iter()
            .flat_map(|p| &p.components)
            .find(|c| c.name == "Foo");

        assert!(
            foo_comp.is_some(),
            "Foo should appear in the report"
        );
        assert_eq!(
            foo_comp.unwrap().status,
            semver_analyzer_core::ComponentStatus::Removed,
            "Foo should be marked Removed when component doesn't exist in new surface"
        );
    }
}
