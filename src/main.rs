mod cli;
mod orchestrator;

use semver_analyzer_ts::konveyor;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::HashMap;
use std::fs::{read_to_string, write};
use std::path::Path;
use std::sync::Arc;

use cli::{Cli, Command};
use semver_analyzer_core::diff::diff_surfaces;
use semver_analyzer_core::traits::Language;
use semver_analyzer_core::{
    AnalysisReport, AnalysisSummary, ApiSurface, BehavioralChange, ChangeTypeCounts,
    ReportEnvelope, StructuralChange, StructuralChangeType,
};
use semver_analyzer_llm::LlmBehaviorAnalyzer;
use semver_analyzer_ts::report::{count_unique_files, extract_suffix_renames};
use semver_analyzer_ts::TypeScript;

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

    let ts = TypeScript::new(build_command.map(|s| s.to_string()));
    let surface = ts
        .extract(repo, git_ref)
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

    let old_json = read_to_string(from_path)
        .with_context(|| format!("Failed to read {}", from_path.display()))?;
    let new_json = read_to_string(to_path)
        .with_context(|| format!("Failed to read {}", to_path.display()))?;

    let old: ApiSurface = serde_json::from_str(&old_json)
        .with_context(|| format!("Failed to parse {} as ApiSurface", from_path.display()))?;
    let new: ApiSurface = serde_json::from_str(&new_json)
        .with_context(|| format!("Failed to parse {} as ApiSurface", to_path.display()))?;

    let changes = diff_surfaces(&old, &new);

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
    let analyzer = orchestrator::Analyzer {
        lang: Arc::new(TypeScript::default()),
    };
    let result = analyzer.run(
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

    // Build report (includes composition changes + hierarchy enrichment)
    let mut report = <TypeScript as Language>::build_report(&result, repo, from_ref, to_ref);

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
                            LlmBehaviorAnalyzer::new(&cmd);
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
        let json = read_to_string(report_path)
            .with_context(|| format!("Failed to read {}", report_path.display()))?;
        let report: AnalysisReport<TypeScript> = serde_json::from_str(&json)
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

        let analyzer = orchestrator::Analyzer {
            lang: Arc::new(TypeScript::default()),
        };
        let result = analyzer.run(
            repo, from, to, no_llm, llm_command, build_command, llm_all_files,
        )
        .await?;

        <TypeScript as Language>::build_report(&result, repo, from, to)
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

// ─── ReportEnvelope production ────────────────────────────────────────────

/// Build a language-agnostic `ReportEnvelope` from a typed `AnalysisReport<L>`.
///
/// The envelope separates language-agnostic data (summary, structural changes)
/// from language-specific data (behavioral/manifest changes), which is serialized
/// as a `serde_json::Value` so consumers can read the envelope without knowing `L`.
fn build_envelope<L: Language>(
    report: &AnalysisReport<L>,
    structural_changes: &[StructuralChange],
) -> anyhow::Result<ReportEnvelope> {
    // Build summary from the report
    let summary = AnalysisSummary {
        total_structural_breaking: structural_changes
            .iter()
            .filter(|c| c.is_breaking)
            .count(),
        total_structural_non_breaking: structural_changes
            .iter()
            .filter(|c| !c.is_breaking)
            .count(),
        total_behavioral_changes: report
            .changes
            .iter()
            .map(|fc| fc.breaking_behavioral_changes.len())
            .sum(),
        total_manifest_changes: report.manifest_changes.len(),
        packages_analyzed: report.packages.len(),
        files_changed: report.changes.len(),
        by_change_type: count_change_types(structural_changes),
    };

    // Build language-specific report section by serializing the typed data
    // directly into a JSON object. We serialize BehavioralChange<L> and
    // ManifestChange<L> as-is (they implement Serialize) rather than
    // constructing LanguageReport<L> which would require L::Evidence and
    // L::ReportData values we don't have.
    let behavioral_changes: Vec<&BehavioralChange<L>> = report
        .changes
        .iter()
        .flat_map(|fc| fc.breaking_behavioral_changes.iter())
        .collect();

    let language_report_value = serde_json::json!({
        "behavioral_changes": serde_json::to_value(&behavioral_changes)
            .unwrap_or(serde_json::Value::Array(vec![])),
        "manifest_changes": serde_json::to_value(&report.manifest_changes)
            .unwrap_or(serde_json::Value::Array(vec![])),
    });

    Ok(ReportEnvelope {
        language: L::NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        summary,
        structural_changes: structural_changes.to_vec(),
        language_report: language_report_value,
    })
}

/// Count structural changes by lifecycle type for the envelope summary.
fn count_change_types(structural_changes: &[StructuralChange]) -> ChangeTypeCounts {
    let mut counts = ChangeTypeCounts::default();
    for change in structural_changes {
        match &change.change_type {
            StructuralChangeType::Added(_) => counts.added += 1,
            StructuralChangeType::Removed(_) => counts.removed += 1,
            StructuralChangeType::Changed(_) => counts.changed += 1,
            StructuralChangeType::Renamed { .. } => counts.renamed += 1,
            StructuralChangeType::Relocated { .. } => counts.relocated += 1,
        }
    }
    counts
}

// ─── Output helpers ──────────────────────────────────────────────────────

fn write_json_output(value: &impl serde::Serialize, output: Option<&Path>) -> Result<()> {
    let json = serde_json::to_string_pretty(value)?;
    if let Some(path) = output {
        write(path, &json)
            .with_context(|| format!("Failed to write output to {}", path.display()))?;
        eprintln!("Output written to {}", path.display());
    } else {
        println!("{}", json);
    }
    Ok(())
}
