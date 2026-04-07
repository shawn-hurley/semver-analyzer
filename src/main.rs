mod cli;
mod orchestrator;
mod progress;

use semver_analyzer_ts::konveyor;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::HashMap;
use std::fs::{self, read_to_string};
use std::path::Path;
use std::sync::Arc;

use tracing::{debug, info, info_span, warn};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

use cli::{AnalyzeLanguage, Cli, Command, ExtractLanguage, KonveyorLanguage};
use progress::ProgressReporter;
use semver_analyzer_core::cli::{DiffArgs, LoggingArgs};
use semver_analyzer_core::diff::diff_surfaces;
use semver_analyzer_core::traits::Language;
use semver_analyzer_core::{
    AnalysisReport, AnalysisSummary, ApiSurface, BehavioralChange, ChangeTypeCounts,
    ReportEnvelope, StructuralChange, StructuralChangeType,
};
use semver_analyzer_llm::LlmBehaviorAnalyzer;
use semver_analyzer_ts::cli::{TsAnalyzeArgs, TsExtractArgs, TsKonveyorArgs};
use semver_analyzer_ts::report::{count_unique_files, extract_suffix_renames};
use semver_analyzer_ts::TypeScript;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialise progress reporter + tracing subscriber
    let reporter = ProgressReporter::new();
    init_tracing(cli.logging_args(), &reporter);

    match cli.command {
        Command::Extract { language } => match language {
            ExtractLanguage::Typescript(args) => cmd_extract_ts(args, &reporter)?,
        },

        Command::Diff(args) => cmd_diff(args, &reporter)?,

        Command::Analyze { language } => match language {
            AnalyzeLanguage::Typescript(args) => cmd_analyze_ts(args, &reporter).await?,
        },

        Command::Konveyor { language } => match language {
            KonveyorLanguage::Typescript(args) => cmd_konveyor_ts(args, &reporter).await?,
        },

        Command::Serve => {
            warn!("MCP server not yet implemented");
        }
    }

    Ok(())
}

// ─── Tracing initialisation ────────────────────────────────────────────

fn init_tracing(logging: &LoggingArgs, reporter: &ProgressReporter) {
    // Stderr: only warnings and errors reach the console.
    // All user-facing progress is handled by progress bars and
    // reporter.println() — tracing events would just bury them.
    let stderr_layer = fmt::layer()
        .with_writer(reporter.make_writer())
        .with_target(false)
        .without_time()
        .with_filter(EnvFilter::new("warn"));

    let registry = tracing_subscriber::registry().with(stderr_layer);

    if let Some(ref path) = logging.log_file {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        // File: full detail at --log-level (default: info)
        let file_filter =
            EnvFilter::try_new(&logging.log_level).unwrap_or_else(|_| EnvFilter::new("info"));

        let file = fs::File::create(path).expect("cannot open log file");
        let file_layer = fmt::layer()
            .with_writer(file)
            .with_ansi(false)
            .with_filter(file_filter);

        registry.with(file_layer).init();
    } else {
        registry.init();
    }
}

// ─── Extract command (TypeScript) ───────────────────────────────────────

fn cmd_extract_ts(args: TsExtractArgs, reporter: &ProgressReporter) -> Result<()> {
    let _span =
        info_span!("extract", repo = %args.common.repo.display(), git_ref = %args.common.git_ref)
            .entered();
    let common = &args.common;

    let phase = reporter.start_phase(&format!(
        "Extracting API surface from {} at ref {}",
        common.repo.display(),
        common.git_ref
    ));

    let ts = TypeScript::new(args.build_command);
    let surface = ts
        .extract(&common.repo, &common.git_ref)
        .context("Failed to extract API surface")?;

    let sym_count = surface.symbols.len();
    let file_count = count_unique_files(&surface);
    phase.finish_with_detail(
        "Extracted API surface",
        &format!("{} symbols from {} files", sym_count, file_count),
    );
    info!(
        symbols = sym_count,
        files = file_count,
        "extraction complete"
    );

    write_json_output(&surface, common.output.as_deref(), reporter)?;
    Ok(())
}

// ─── Diff command (language-agnostic) ───────────────────────────────────

fn cmd_diff(args: DiffArgs, reporter: &ProgressReporter) -> Result<()> {
    let _span = info_span!("diff", from = %args.from.display(), to = %args.to.display()).entered();

    let phase = reporter.start_phase(&format!(
        "Diffing {} vs {}",
        args.from.display(),
        args.to.display()
    ));

    let old_json = read_to_string(&args.from)
        .with_context(|| format!("Failed to read {}", args.from.display()))?;
    let new_json = read_to_string(&args.to)
        .with_context(|| format!("Failed to read {}", args.to.display()))?;

    let old: ApiSurface = serde_json::from_str(&old_json)
        .with_context(|| format!("Failed to parse {} as ApiSurface", args.from.display()))?;
    let new: ApiSurface = serde_json::from_str(&new_json)
        .with_context(|| format!("Failed to parse {} as ApiSurface", args.to.display()))?;

    let changes = diff_surfaces(&old, &new);

    let breaking = changes.iter().filter(|c| c.is_breaking).count();
    let non_breaking = changes.len() - breaking;
    phase.finish_with_detail(
        "Diff complete",
        &format!(
            "{} changes ({} breaking, {} non-breaking)",
            changes.len(),
            breaking,
            non_breaking
        ),
    );
    info!(
        total = changes.len(),
        breaking, non_breaking, "diff complete"
    );

    write_json_output(&changes, args.output.as_deref(), reporter)?;
    Ok(())
}

// ─── Analyze command (TypeScript) ───────────────────────────────────────

async fn cmd_analyze_ts(args: TsAnalyzeArgs, reporter: &ProgressReporter) -> Result<()> {
    let _span = info_span!("analyze", repo = %args.common.repo.display(), from = %args.common.from, to = %args.common.to).entered();
    let common = &args.common;

    reporter.println(&format!(
        "Analyzing {} from {} to {}",
        common.repo.display(),
        common.from,
        common.to
    ));
    if common.no_llm {
        reporter.println("Mode: static analysis only (--no-llm)");
    }

    let analyzer = orchestrator::Analyzer {
        lang: Arc::new(TypeScript::new(args.build_command)),
    };
    let result = if common.pipeline_v2 {
        reporter.println("Pipeline: v2 (TD+SD, no BU)");
        analyzer
            .run_v2(
                &common.repo,
                &common.from,
                &common.to,
                common.no_llm,
                common.llm_command.as_deref(),
                None,
                common.dep_repo.as_deref(),
                common.dep_from.as_deref(),
                common.dep_to.as_deref(),
                common.dep_build_command.as_deref(),
                common.llm_timeout,
                reporter,
            )
            .await?
    } else {
        analyzer
            .run(
                &common.repo,
                &common.from,
                &common.to,
                common.no_llm,
                common.llm_command.as_deref(),
                None, // build_command already on TypeScript
                common.llm_all_files,
                common.llm_timeout,
                reporter,
            )
            .await?
    };

    // Print summary stats
    let manifest_breaking = result
        .manifest_changes
        .iter()
        .filter(|c| c.is_breaking)
        .count();
    if !result.manifest_changes.is_empty() {
        info!(
            total = result.manifest_changes.len(),
            breaking = manifest_breaking,
            "manifest changes"
        );
        reporter.println(&format!(
            "  [TD] {} manifest changes ({} breaking)",
            result.manifest_changes.len(),
            manifest_breaking
        ));
    }

    // Build report (includes composition changes + hierarchy enrichment)
    let phase = reporter.start_phase("Building analysis report");
    let mut report =
        <TypeScript as Language>::build_report(&result, &common.repo, &common.from, &common.to);
    phase.finish("Report built");

    // ── Infer CSS suffix renames via LLM ─────────────────────────────
    if !common.no_llm {
        if let Some(ref llm_cmd) = common.llm_command {
            let (removed_suffixes, added_suffixes) = konveyor::extract_suffix_inventory(&report);
            if !removed_suffixes.is_empty() && !added_suffixes.is_empty() {
                let suffix_phase = reporter.start_phase(&format!(
                    "[Suffix] Inferring CSS suffix renames ({} removed, {} added)",
                    removed_suffixes.len(),
                    added_suffixes.len()
                ));
                debug!(
                    removed = removed_suffixes.len(),
                    added = added_suffixes.len(),
                    "extracted suffix inventory"
                );

                let llm_timeout = common.llm_timeout;
                let suffix_result = tokio::task::spawn_blocking({
                    let cmd = llm_cmd.clone();
                    let removed: Vec<String> = removed_suffixes.into_iter().collect();
                    let added: Vec<String> = added_suffixes.into_iter().collect();
                    move || {
                        let analyzer = LlmBehaviorAnalyzer::new(&cmd).with_timeout(llm_timeout);
                        let removed_refs: Vec<&str> = removed.iter().map(|s| s.as_str()).collect();
                        let added_refs: Vec<&str> = added.iter().map(|s| s.as_str()).collect();
                        analyzer.infer_suffix_renames(&removed_refs, &added_refs)
                    }
                })
                .await;

                match suffix_result {
                    Ok(Ok(renames)) if !renames.is_empty() => {
                        info!(count = renames.len(), "LLM identified CSS suffix renames");
                        let suffix_map: HashMap<String, String> = renames
                            .iter()
                            .map(|r| {
                                debug!(from = %r.from, to = %r.to, "suffix rename");
                                (r.from.clone(), r.to.clone())
                            })
                            .collect();

                        let member_renames = konveyor::apply_suffix_renames(&report, &suffix_map);

                        if !member_renames.is_empty() {
                            info!(
                                count = member_renames.len(),
                                "applied suffix member renames"
                            );
                            report.member_renames = member_renames;
                        }
                        suffix_phase.finish_with_detail(
                            "[Suffix] Inference complete",
                            &format!("{} renames", renames.len()),
                        );
                    }
                    Ok(Ok(_)) => {
                        info!("LLM returned no suffix renames");
                        suffix_phase.finish("[Suffix] No renames found");
                    }
                    Ok(Err(e)) => {
                        warn!(%e, "LLM suffix inference failed");
                        suffix_phase.finish("[Suffix] Inference failed");
                    }
                    Err(e) => {
                        warn!(%e, "spawn_blocking failed for suffix inference");
                        suffix_phase.finish("[Suffix] Inference failed");
                    }
                }
            }
        }
    }

    let total_breaking = report.summary.total_breaking_changes;
    reporter.println("");
    if total_breaking == 0 {
        reporter.println("No breaking changes detected.");
        info!("no breaking changes detected");
    } else {
        reporter.println(&format!(
            "BREAKING: {} total breaking change(s) detected.",
            total_breaking
        ));
        reporter.println(&format!(
            "  {} API changes, {} behavioral changes",
            report.summary.breaking_api_changes, report.summary.breaking_behavioral_changes
        ));
        info!(
            total = total_breaking,
            api = report.summary.breaking_api_changes,
            behavioral = report.summary.breaking_behavioral_changes,
            "breaking changes detected"
        );
    }

    write_json_output(&report, common.output.as_deref(), reporter)?;
    Ok(())
}

// ─── Konveyor command (TypeScript) ──────────────────────────────────────

async fn cmd_konveyor_ts(args: TsKonveyorArgs, reporter: &ProgressReporter) -> Result<()> {
    let _span = info_span!("konveyor").entered();
    let common = &args.common;

    let mut rename_patterns = if let Some(ref path) = common.rename_patterns {
        konveyor::RenamePatterns::load(path)?
    } else {
        konveyor::RenamePatterns::empty()
    };

    let mut report = if let Some(ref report_path) = common.from_report {
        info!(path = %report_path.display(), "loading report from file");
        reporter.println(&format!("Loading report from {}", report_path.display()));
        let json = read_to_string(report_path)
            .with_context(|| format!("Failed to read {}", report_path.display()))?;
        let report: AnalysisReport<TypeScript> =
            serde_json::from_str(&json).with_context(|| {
                format!(
                    "Failed to parse {} as AnalysisReport",
                    report_path.display()
                )
            })?;
        report
    } else {
        let repo = common
            .repo
            .as_ref()
            .context("--repo is required when --from-report is not provided")?;
        let from = common
            .from
            .as_ref()
            .context("--from is required when --from-report is not provided")?;
        let to = common
            .to
            .as_ref()
            .context("--to is required when --from-report is not provided")?;

        reporter.println(&format!(
            "Analyzing {} from {} to {}",
            repo.display(),
            from,
            to
        ));
        if common.no_llm {
            reporter.println("Mode: static analysis only (--no-llm)");
        }

        let analyzer = orchestrator::Analyzer {
            lang: Arc::new(TypeScript::new(args.build_command.clone())),
        };
        let result = if common.pipeline_v2 {
            reporter.println("Pipeline: v2 (TD+SD, no BU)");
            analyzer
                .run_v2(
                    repo,
                    from,
                    to,
                    common.no_llm,
                    common.llm_command.as_deref(),
                    None,
                    common.dep_repo.as_deref(),
                    common.dep_from.as_deref(),
                    common.dep_to.as_deref(),
                    None, // konveyor reads from report, no dep build needed
                    common.llm_timeout,
                    reporter,
                )
                .await?
        } else {
            analyzer
                .run(
                    repo,
                    from,
                    to,
                    common.no_llm,
                    common.llm_command.as_deref(),
                    None, // build_command already on TypeScript
                    common.llm_all_files,
                    common.llm_timeout,
                    reporter,
                )
                .await?
        };

        <TypeScript as Language>::build_report(&result, repo, from, to)
    };

    // Build package info cache
    let mut pkg_info_cache = konveyor::build_package_info_cache(&report);
    let pkg_cache: HashMap<String, String> = pkg_info_cache
        .iter()
        .map(|(k, v)| (k.clone(), v.name.clone()))
        .collect();

    // Analyze token members
    let phase = reporter.start_phase("Analyzing token members");
    let (covered_symbols, mut member_renames) =
        konveyor::analyze_token_members(&report, &rename_patterns);
    for (k, v) in &report.member_renames {
        member_renames.entry(k.clone()).or_insert_with(|| v.clone());
    }
    if !covered_symbols.is_empty() {
        info!(
            covered = covered_symbols.len(),
            renames = member_renames.len(),
            "token member analysis"
        );
    }
    phase.finish_with_detail(
        "Token members analyzed",
        &format!(
            "{} covered, {} renames",
            covered_symbols.len(),
            member_renames.len()
        ),
    );

    // Store member renames into the report
    if !member_renames.is_empty() {
        report.member_renames = member_renames.clone();

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

    // Enrich package entries with npm package names and versions
    for pkg in &mut report.packages {
        if let Some(info) = pkg_info_cache.get(&pkg.name) {
            pkg.name = info.name.clone();
            pkg.old_version = info.version.clone();
        }
    }

    // Merge LLM-inferred constant rename patterns
    if let Some(ref inferred) = report.inferred_rename_patterns {
        for pat in &inferred.constant_patterns {
            rename_patterns.add_pattern(&pat.match_regex, &pat.replace);
        }
        if !inferred.constant_patterns.is_empty() {
            info!(
                count = inferred.constant_patterns.len(),
                "merged LLM-inferred constant rename patterns"
            );
        }
    }

    // Generate rules
    let rule_phase = reporter.start_phase("Generating Konveyor rules");
    let raw_rules = konveyor::generate_rules(
        &report,
        &args.file_pattern,
        &pkg_cache,
        &rename_patterns,
        &member_renames,
    );
    let raw_count = raw_rules.len();

    let rules = if common.no_consolidate {
        raw_rules
    } else {
        let (consolidated, _id_mapping) = konveyor::consolidate_rules(raw_rules);
        info!(
            raw = raw_count,
            consolidated = consolidated.len(),
            "rule consolidation"
        );
        consolidated
    };

    let rules = konveyor::suppress_redundant_prop_rules(rules);
    let rules = konveyor::suppress_redundant_prop_value_rules(rules);
    let rules = konveyor::merge_duplicate_conditions(rules);

    let mut strategies = konveyor::extract_fix_strategies(&rules);

    // Add dep-repo packages (e.g., @patternfly/patternfly CSS package) to the
    // cache so they get dependency-update rules even though they're from a
    // separate repo.
    if let Some(ref sd) = report.sd_result {
        for (name, version) in &sd.dep_repo_packages {
            let dir_name = name.rsplit('/').next().unwrap_or(name);
            pkg_info_cache
                .entry(dir_name.to_string())
                .or_insert_with(|| semver_analyzer_konveyor_core::PackageInfo {
                    name: name.clone(),
                    version: Some(version.clone()),
                });
        }
    }

    // Generate dependency update rules
    let (dep_update_rules, dep_update_strategies) =
        konveyor::generate_dependency_update_rules(&report, &pkg_info_cache);
    strategies.extend(dep_update_strategies);

    let mut all_rules = rules;
    all_rules.extend(dep_update_rules);

    // v2 SD rules — composition, conformance, context, prop↔child migration
    if common.pipeline_v2 {
        if let Some(ref sd) = report.sd_result {
            let sd_rule_phase = reporter.start_phase("Generating v2 SD rules");
            let sd_rules =
                semver_analyzer_ts::konveyor_v2::generate_sd_rules(&report, sd, &pkg_cache);
            let sd_count = sd_rules.len();

            // Collect components covered by SD prop→child or deprecated-migration rules.
            // Suppress v1 "component-removal" rules for these components since the
            // SD rules provide more precise, actionable guidance.
            let sd_covered: std::collections::HashSet<String> = sd_rules
                .iter()
                .filter(|r| {
                    r.labels.iter().any(|l| {
                        l == "change-type=prop-to-child" || l == "change-type=deprecated-migration"
                    })
                })
                .filter_map(|r| r.fix_strategy.as_ref().and_then(|fs| fs.component.clone()))
                .collect();

            if !sd_covered.is_empty() {
                let before = all_rules.len();
                all_rules.retain(|r| {
                    // Suppress v1 rules that overlap with v2 prop→child rules:
                    // - component-removal (generic "move to child" message)
                    // - signature-changed that fires on IMPORT (interface-level,
                    //   e.g., "ModalProps base class changed") — these confuse
                    //   the LLM when v2 rules give precise prop-level guidance
                    // Keep prop-level v1 rules (JSX_PROP) like individual renames.
                    let is_component_removal = r
                        .labels
                        .iter()
                        .any(|l| l == "change-type=component-removal");
                    let is_import_level_change = r
                        .labels
                        .iter()
                        .any(|l| l == "change-type=signature-changed")
                        && semver_analyzer_konveyor_core::extract_frontend_refs(&r.when)
                            .iter()
                            .any(|f| f.location == "IMPORT");
                    if !is_component_removal && !is_import_level_change {
                        return true;
                    }
                    // Extract component name from fix_strategy or from the
                    // condition pattern (strip ^...$ anchors)
                    let component = r
                        .fix_strategy
                        .as_ref()
                        .and_then(|fs| fs.component.clone())
                        .or_else(|| {
                            // Extract from condition pattern
                            if let semver_analyzer_konveyor_core::KonveyorCondition::FrontendReferenced {
                                ref referenced,
                            } = r.when
                            {
                                let p = &referenced.pattern;
                                Some(
                                    p.trim_start_matches('^')
                                        .trim_end_matches('$')
                                        .to_string(),
                                )
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    !sd_covered.contains(&component)
                });
                let suppressed = before - all_rules.len();
                if suppressed > 0 {
                    info!(
                        suppressed,
                        "suppressed v1 rules covered by v2 SD prop→child/composition rules"
                    );
                }
            }

            let sd_strategies = konveyor::extract_fix_strategies(&sd_rules);
            strategies.extend(sd_strategies);

            all_rules.extend(sd_rules);
            sd_rule_phase.finish_with_detail("SD rules generated", &format!("{} rules", sd_count));
        }
    }

    let fix_guidance = konveyor::generate_fix_guidance(&report, &all_rules, &args.file_pattern);
    let rule_count = all_rules.len();
    rule_phase.finish_with_detail("Rules generated", &format!("{} rules", rule_count));

    // Write output
    let write_phase = reporter.start_phase("Writing output files");
    konveyor::write_ruleset_dir(&common.output_dir, &args.ruleset_name, &report, &all_rules)?;

    let fix_dir = konveyor::write_fix_guidance_dir(&common.output_dir, &fix_guidance)?;
    konveyor::write_fix_strategies(&fix_dir, &strategies)?;

    // Conformance rules are now generated by the v2 SD pipeline
    // (konveyor_v2::generate_conformance_rules) from composition trees.
    write_phase.finish("Output written");

    // Summary
    reporter.println(&format!(
        "\nGenerated {} Konveyor rules in {}",
        rule_count,
        common.output_dir.display()
    ));
    reporter.println(&format!(
        "  Ruleset:  {}/ruleset.yaml",
        common.output_dir.display()
    ));
    reporter.println(&format!(
        "  Rules:    {}/breaking-changes.yaml",
        common.output_dir.display()
    ));
    // Conformance rules disabled (see above)
    // if !conformance_rules.is_empty() {
    //     reporter.println(&format!(
    //         "  Conformance: {}/conformance-rules.yaml ({} rules)",
    //         common.output_dir.display(),
    //         conformance_rules.len(),
    //     ));
    // }
    reporter.println(&format!(
        "  Fixes:    {}/fix-guidance.yaml",
        fix_dir.display()
    ));
    reporter.println(&format!(
        "  Strategies: {}/fix-strategies.json ({} entries)",
        fix_dir.display(),
        strategies.len()
    ));
    reporter.println(&format!(
        "  Summary:  {} auto-fixable, {} need review, {} manual only",
        fix_guidance.summary.auto_fixable,
        fix_guidance.summary.needs_review,
        fix_guidance.summary.manual_only,
    ));
    reporter.println(&format!(
        "\nUse with: konveyor-analyzer --rules {}",
        common.output_dir.display()
    ));

    info!(
        rule_count,
        auto_fixable = fix_guidance.summary.auto_fixable,
        needs_review = fix_guidance.summary.needs_review,
        manual_only = fix_guidance.summary.manual_only,
        "konveyor generation complete"
    );

    Ok(())
}

// ─── ReportEnvelope production ──────────────────────────────────────────

/// Build a language-agnostic `ReportEnvelope` from a typed `AnalysisReport<L>`.
#[allow(dead_code)]
fn build_envelope<L: Language>(
    report: &AnalysisReport<L>,
    structural_changes: &[StructuralChange],
) -> anyhow::Result<ReportEnvelope> {
    let summary = AnalysisSummary {
        total_structural_breaking: structural_changes.iter().filter(|c| c.is_breaking).count(),
        total_structural_non_breaking: structural_changes.iter().filter(|c| !c.is_breaking).count(),
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

#[allow(dead_code)]
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

// ─── Output helpers ─────────────────────────────────────────────────────

fn write_json_output(
    value: &impl serde::Serialize,
    output: Option<&Path>,
    reporter: &ProgressReporter,
) -> Result<()> {
    let json = serde_json::to_string_pretty(value)?;
    if let Some(path) = output {
        std::fs::write(path, &json)
            .with_context(|| format!("Failed to write output to {}", path.display()))?;
        reporter.println(&format!("Output written to {}", path.display()));
        info!(path = %path.display(), "output written");
    } else {
        println!("{}", json);
    }
    Ok(())
}
