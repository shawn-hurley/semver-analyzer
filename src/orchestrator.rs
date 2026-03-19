//! Concurrent TD/BU pipeline orchestrator.
//!
//! Runs the Top-Down (structural) and Bottom-Up (behavioral) pipelines
//! concurrently via `tokio::join!`, coordinated through `SharedFindings`.
//!
//! ## Pipeline flow
//!
//! ```text
//! TD (Top-Down):                      BU (Bottom-Up):
//! 1. Extract API @ from_ref           1. Parse git diff → changed functions
//! 2. Extract API @ to_ref             2. For each changed function:
//! 3. diff_surfaces()                     a. Check SharedFindings (skip if TD found)
//! 4. Insert breaks → SharedFindings      b. Find test files, check assertion changes
//!                                        c. If test assertions changed → behavioral break
//!                                        d. If private + breaking → walk UP call graph
//!                                     3. Insert behavioral breaks → SharedFindings
//! ```
//!
//! After both complete, results are merged into a single `AnalysisReport`.

use anyhow::{Context, Result};
use semver_analyzer_core::{
    BehavioralBreak, BehavioralChange, BehavioralChangeKind, CallGraphBuilder,
    DiffParser, EvidenceSource, SharedFindings, TestAnalyzer, Visibility,
};
use semver_analyzer_llm::LlmBehaviorAnalyzer;
use semver_analyzer_ts::{OxcExtractor, TsCallGraphBuilder, TsDiffParser, TsTestAnalyzer};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Run the full concurrent TD+BU analysis pipeline.
///
/// Returns structural changes, behavioral breaks, and package.json changes.
/// The caller (`cmd_analyze`) assembles these into the final report.
pub async fn run_concurrent_analysis(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    no_llm: bool,
    _llm_command: Option<&str>,
    build_command: Option<&str>,
    llm_all_files: bool,
) -> Result<AnalysisResult> {
    let shared = Arc::new(SharedFindings::new());

    // Clone values for the async tasks
    let repo_td = repo.to_path_buf();
    let repo_bu = repo.to_path_buf();
    let from_td = from_ref.to_string();
    let to_td = to_ref.to_string();
    let from_bu = from_ref.to_string();
    let to_bu = to_ref.to_string();
    let build_cmd = build_command.map(|s| s.to_string());
    let llm_cmd = _llm_command.map(|s| s.to_string());
    let shared_td = shared.clone();
    let shared_bu = shared.clone();

    // Run TD and BU Phase 1 (test-based analysis) concurrently
    let (td_result, bu_phase1_result) = tokio::join!(
        // TD: Extract API surfaces and compute structural diff
        tokio::task::spawn_blocking(move || {
            run_td(&repo_td, &from_td, &to_td, build_cmd.as_deref(), &shared_td)
        }),
        // BU Phase 1: Parse diff, analyze tests, walk call graph (no LLM)
        tokio::task::spawn_blocking(move || {
            run_bu_phase1(&repo_bu, &from_bu, &to_bu, llm_cmd, llm_all_files, &shared_bu)
        }),
    );

    // Unwrap JoinHandle results, then inner Results
    let td = td_result
        .map_err(|e| anyhow::anyhow!("TD task panicked: {}", e))?
        .context("TD pipeline failed")?;

    let phase1 = bu_phase1_result
        .map_err(|e| anyhow::anyhow!("BU Phase 1 task panicked: {}", e))?
        .context("BU Phase 1 pipeline failed")?;

    // ── Rename Inference Phase (between TD and BU Phase 2) ─────────
    //
    // Uses LLM to discover systematic rename patterns for constants and
    // interfaces. Requires TD results (structural changes) and API surfaces.
    let empty_surface = semver_analyzer_core::ApiSurface { symbols: vec![] };
    let inferred_rename_patterns = if !no_llm {
        let old_surf = shared.try_get_old_surface().unwrap_or(&empty_surface);
        let new_surf = shared.try_get_new_surface().unwrap_or(&empty_surface);
        let llm_cmd = phase1
            .llm_command
            .as_deref()
            .unwrap_or("goose run --no-session -q -t");
        infer_rename_patterns(
            &td.structural_changes,
            old_surf,
            new_surf,
            llm_cmd,
            from_ref,
            to_ref,
        )
    } else {
        None
    };

    // BU Phase 2: Concurrent LLM file analysis (async, 5 at a time)
    let mut llm_stats = LlmPhaseStats::default();
    let llm_api_entries = Arc::new(std::sync::Mutex::new(Vec::<LlmApiChangeEntry>::new()));
    if !no_llm && !phase1.files_for_llm.is_empty() {
        llm_stats = run_bu_phase2_llm(
            repo,
            from_ref,
            to_ref,
            &phase1.llm_command,
            &phase1.files_for_llm,
            &shared,
            &llm_api_entries,
        )
        .await;
    }

    // ── BU Phase 3: Composition pattern analysis from test/example diffs ──
    //
    // Analyzes test and example file diffs to detect JSX nesting changes
    // (e.g., MastheadToggle moving inside MastheadMain). These patterns
    // are usage changes visible in tests/examples but not in the component API.
    let composition_changes: Vec<(String, Vec<semver_analyzer_core::CompositionPatternChange>)> =
        if !no_llm {
            analyze_composition_patterns(repo, from_ref, to_ref, phase1.llm_command.as_deref())
        } else {
            vec![]
        };

    // Merge results
    let behavioral_changes = merge_behavioral_breaks(&shared);
    let llm_api_changes = match Arc::try_unwrap(llm_api_entries) {
        Ok(mutex) => mutex.into_inner().unwrap_or_default(),
        Err(arc) => arc.lock().unwrap().clone(),
    };

    let bu_stats = BuStats {
        changed_function_count: phase1.stats.changed_function_count,
        skipped_by_td: phase1.stats.skipped_by_td,
        test_behavioral_breaks: phase1.stats.test_behavioral_breaks,
        llm_behavioral_breaks: llm_stats.llm_behavioral_breaks,
        llm_calls: llm_stats.llm_calls,
        call_graph_propagated: phase1.stats.call_graph_propagated,
    };

    eprintln!(
        "[BU]   {} skipped (TD found), {} test-based breaks, {} LLM breaks ({} calls), {} LLM API, {} propagated up",
        bu_stats.skipped_by_td,
        bu_stats.test_behavioral_breaks,
        bu_stats.llm_behavioral_breaks,
        bu_stats.llm_calls,
        llm_api_changes.len(),
        bu_stats.call_graph_propagated,
    );

    // Extract API surfaces from shared state before it's dropped.
    // These are used by build_report() to compute component summaries.
    let old_surface = shared
        .try_get_old_surface()
        .cloned()
        .unwrap_or_else(|| semver_analyzer_core::ApiSurface { symbols: vec![] });
    let new_surface = shared
        .try_get_new_surface()
        .cloned()
        .unwrap_or_else(|| semver_analyzer_core::ApiSurface { symbols: vec![] });

    Ok(AnalysisResult {
        structural_changes: td.structural_changes,
        behavioral_changes,
        manifest_changes: td.manifest_changes,
        llm_api_changes,
        td_stats: td.stats,
        bu_stats,
        old_surface,
        new_surface,
        inferred_rename_patterns,
        composition_changes,
    })
}

/// Results from the full analysis pipeline.
pub struct AnalysisResult {
    pub structural_changes: Vec<semver_analyzer_core::StructuralChange>,
    pub behavioral_changes: Vec<BehavioralChange>,
    pub manifest_changes: Vec<semver_analyzer_core::ManifestChange>,
    /// API type-level changes detected by LLM (interface extends, optionality, etc.)
    pub llm_api_changes: Vec<LlmApiChangeEntry>,
    pub td_stats: TdStats,
    pub bu_stats: BuStats,
    /// Full API surface at the old ref (for build_report aggregation).
    /// Not serialized into the report — used only during report building
    /// to compute component summaries, removal ratios, etc.
    pub old_surface: semver_analyzer_core::ApiSurface,
    /// Full API surface at the new ref (for build_report aggregation).
    pub new_surface: semver_analyzer_core::ApiSurface,
    /// LLM-inferred rename patterns (None when --no-llm).
    pub inferred_rename_patterns: Option<semver_analyzer_core::InferredRenamePatterns>,
    /// Composition pattern changes from test/example diffs.
    /// Keyed by source file path (the component these patterns are about).
    pub composition_changes: Vec<(String, Vec<semver_analyzer_core::CompositionPatternChange>)>,
}

/// An API change detected by the LLM during file-level analysis.
#[derive(Debug, Clone)]
pub struct LlmApiChangeEntry {
    pub file_path: String,
    pub symbol: String,
    pub change: String,
    pub description: String,
    /// LLM-determined disposition for removed props.
    pub removal_disposition: Option<semver_analyzer_llm::invoke::LlmRemovalDisposition>,
    /// HTML element the component renders.
    pub renders_element: Option<String>,
}

/// Stats from the TD pipeline.
pub struct TdStats {
    pub old_symbol_count: usize,
    pub new_symbol_count: usize,
    pub structural_change_count: usize,
    pub structural_breaking_count: usize,
}

/// Stats from the BU pipeline.
pub struct BuStats {
    pub changed_function_count: usize,
    pub skipped_by_td: usize,
    pub test_behavioral_breaks: usize,
    pub llm_behavioral_breaks: usize,
    pub llm_calls: usize,
    pub call_graph_propagated: usize,
}

/// A file prepared for LLM analysis with its diff and changed functions.
struct LlmFileTask {
    file_path: String,
    diff_content: String,
    functions: Vec<semver_analyzer_core::ChangedFunction>,
}

/// Output from BU Phase 1 (test-based analysis), including files queued for LLM.
struct BuPhase1Result {
    stats: BuStats,
    files_for_llm: Vec<LlmFileTask>,
    llm_command: Option<String>,
}

/// Stats from the async LLM phase.
#[derive(Default)]
struct LlmPhaseStats {
    llm_calls: usize,
    llm_behavioral_breaks: usize,
    llm_api_changes: usize,
}

// ── TD Pipeline ─────────────────────────────────────────────────────────

struct TdResult {
    structural_changes: Vec<semver_analyzer_core::StructuralChange>,
    manifest_changes: Vec<semver_analyzer_core::ManifestChange>,
    stats: TdStats,
}

fn run_td(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    build_command: Option<&str>,
    shared: &SharedFindings,
) -> Result<TdResult> {
    let extractor = OxcExtractor::new();

    // Step 1: Extract API surface at old ref
    eprintln!("[TD] Extracting API surface at {} ...", from_ref);
    let old_surface = extractor
        .extract_at_ref(repo, from_ref, build_command)
        .with_context(|| format!("Failed to extract API surface at ref {}", from_ref))?;
    let old_count = old_surface.symbols.len();
    eprintln!("[TD]   {} symbols extracted", old_count);

    // Store in shared state so BU can access if needed
    shared.set_old_surface(old_surface.clone());

    // Step 2: Extract API surface at new ref
    eprintln!("[TD] Extracting API surface at {} ...", to_ref);
    let new_surface = extractor
        .extract_at_ref(repo, to_ref, build_command)
        .with_context(|| format!("Failed to extract API surface at ref {}", to_ref))?;
    let new_count = new_surface.symbols.len();
    eprintln!("[TD]   {} symbols extracted", new_count);

    shared.set_new_surface(new_surface.clone());

    // Step 3: Structural diff
    eprintln!("[TD] Computing structural diff ...");
    let structural_changes =
        semver_analyzer_core::diff::diff_surfaces(&old_surface, &new_surface);
    let breaking = structural_changes.iter().filter(|c| c.is_breaking).count();
    eprintln!(
        "[TD]   {} structural changes ({} breaking)",
        structural_changes.len(),
        breaking
    );

    // Insert all breaking changes into shared state (broadcasts to BU)
    let breaking_changes: Vec<_> = structural_changes
        .iter()
        .filter(|c| c.is_breaking)
        .cloned()
        .collect();
    shared.insert_structural_breaks(breaking_changes);

    // Step 4: Package.json diff
    let manifest_changes = diff_package_json(repo, from_ref, to_ref);

    let total_changes = structural_changes.len();

    Ok(TdResult {
        structural_changes,
        manifest_changes,
        stats: TdStats {
            old_symbol_count: old_count,
            new_symbol_count: new_count,
            structural_change_count: total_changes,
            structural_breaking_count: breaking,
        },
    })
}

// ── BU Pipeline ─────────────────────────────────────────────────────────

/// BU Phase 1: Synchronous test-based analysis + file list for LLM.
///
/// Returns test-based behavioral breaks and a list of files prepared for LLM analysis.
fn run_bu_phase1(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    llm_command: Option<String>,
    llm_all_files: bool,
    shared: &SharedFindings,
) -> Result<BuPhase1Result> {
    let diff_parser = TsDiffParser::new();
    let test_analyzer = TsTestAnalyzer::new();
    let call_graph = TsCallGraphBuilder::new();

    // Step 1: Parse git diff to find all changed functions
    eprintln!("[BU] Parsing changed functions ...");
    let changed_fns = diff_parser
        .parse_changed_functions(repo, from_ref, to_ref)
        .context("Failed to parse changed functions")?;
    eprintln!("[BU]   {} changed functions found", changed_fns.len());

    // Subscribe to TD's broadcast channel
    let mut receiver = shared.subscribe_to_td();

    let mut stats = BuStats {
        changed_function_count: changed_fns.len(),
        skipped_by_td: 0,
        test_behavioral_breaks: 0,
        llm_behavioral_breaks: 0,
        llm_calls: 0,
        call_graph_propagated: 0,
    };

    // ── Test-based analysis (per-function, no LLM) ──────────────────
    for func in &changed_fns {
        if semver_analyzer_core::shared::should_skip_for_bu(
            shared,
            &mut receiver,
            &func.qualified_name,
        ) {
            stats.skipped_by_td += 1;
            continue;
        }

        if func.old_body.is_empty() || func.new_body.is_empty() {
            continue;
        }

        let test_files = test_analyzer
            .find_tests(repo, &func.file)
            .unwrap_or_default();

        let test_diff = test_files.iter().find_map(|tf| {
            test_analyzer
                .diff_test_assertions(repo, tf, from_ref, to_ref)
                .ok()
                .filter(|td| td.has_assertion_changes)
        });

        if let Some(td) = test_diff {
            let description = format!(
                "Test assertions changed: {} removed, {} added",
                td.removed_assertions.len(),
                td.added_assertions.len()
            );
            let brk = BehavioralBreak {
                symbol: func.qualified_name.clone(),
                caused_by: func.qualified_name.clone(),
                call_path: vec![func.name.clone()],
                evidence: EvidenceSource::TestDelta { test_diff: td },
                confidence: 0.95,
                description,
                category: None, // Test-delta: category inferred later or by JSX differ
            };
            stats.test_behavioral_breaks += 1;

            if func.visibility == Visibility::Exported || func.visibility == Visibility::Public {
                shared.insert_behavioral_break(brk);
            } else {
                let source_file = repo.join(&func.file);
                if source_file.exists() {
                    let propagated = walk_up_call_graph(
                        &call_graph,
                        &source_file,
                        &func.name,
                        &func.qualified_name,
                        &brk,
                        shared,
                    );
                    stats.call_graph_propagated += propagated;
                }
            }
        }
    }

    // ── JSX diff analysis (per-function, deterministic, no LLM) ─────
    let mut jsx_change_count = 0;
    for func in &changed_fns {
        // Skip functions without JSX or without both bodies
        if func.old_body.is_empty()
            || func.new_body.is_empty()
            || !semver_analyzer_ts::jsx_diff::body_contains_jsx(&func.old_body)
            || !semver_analyzer_ts::jsx_diff::body_contains_jsx(&func.new_body)
        {
            continue;
        }

        // Only analyze exported functions (these are the component render outputs consumers see)
        if func.visibility != Visibility::Exported && func.visibility != Visibility::Public {
            continue;
        }

        let jsx_changes = semver_analyzer_ts::jsx_diff::diff_jsx_bodies(
            &func.old_body,
            &func.new_body,
            &func.name,
            &func.file,
        );

        for jsx_change in jsx_changes {
            // Check if TD already found this symbol (avoid duplicates)
            if semver_analyzer_core::shared::should_skip_for_bu(
                shared,
                &mut receiver,
                &func.qualified_name,
            ) {
                continue;
            }

            let brk = BehavioralBreak {
                symbol: func.qualified_name.clone(),
                caused_by: func.qualified_name.clone(),
                call_path: vec![func.name.clone()],
                evidence: EvidenceSource::JsxDiff {
                    change_description: jsx_change.description.clone(),
                },
                confidence: 0.90,
                description: jsx_change.description,
                category: Some(jsx_change.category),
            };
            shared.insert_behavioral_break(brk);
            jsx_change_count += 1;
        }
    }

    // ── CSS variable/class scanning (per-function, deterministic) ────
    let mut css_change_count = 0;
    for func in &changed_fns {
        if func.old_body.is_empty() || func.new_body.is_empty() {
            continue;
        }
        if func.visibility != Visibility::Exported && func.visibility != Visibility::Public {
            continue;
        }
        if !semver_analyzer_ts::css_scan::body_contains_css_refs(&func.old_body)
            && !semver_analyzer_ts::css_scan::body_contains_css_refs(&func.new_body)
        {
            continue;
        }

        let css_changes = semver_analyzer_ts::css_scan::diff_css_references(
            &func.old_body,
            &func.new_body,
            &func.name,
            &func.file,
        );

        for css_change in css_changes {
            let brk = BehavioralBreak {
                symbol: func.qualified_name.clone(),
                caused_by: func.qualified_name.clone(),
                call_path: vec![func.name.clone()],
                evidence: EvidenceSource::JsxDiff {
                    change_description: css_change.description.clone(),
                },
                confidence: 0.90,
                description: css_change.description,
                category: Some(css_change.category),
            };
            shared.insert_behavioral_break(brk);
            css_change_count += 1;
        }
    }

    if jsx_change_count > 0 || css_change_count > 0 {
        eprintln!(
            "  BU Phase 1: {} JSX + {} CSS changes detected deterministically",
            jsx_change_count, css_change_count,
        );
    }

    // ── Prepare file list for LLM Phase 2 ───────────────────────────
    let mut files_for_llm = Vec::new();

    if llm_command.is_some() {
        // Group functions by file
        let mut by_file: std::collections::BTreeMap<String, Vec<&semver_analyzer_core::ChangedFunction>> =
            std::collections::BTreeMap::new();
        for func in &changed_fns {
            if func.old_body.is_empty() || func.new_body.is_empty() {
                continue;
            }
            let file_key = func.file.to_string_lossy().to_string();
            by_file.entry(file_key).or_default().push(func);
        }

        let unfiltered_count = by_file.len();
        let filtered: Vec<_> = by_file
            .into_iter()
            .filter(|(path, funcs)| {
                let has_exported = funcs
                    .iter()
                    .any(|f| f.visibility == Visibility::Exported || f.visibility == Visibility::Public);
                if !has_exported {
                    return false;
                }

                let basename = Path::new(path)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();
                if basename == "index.ts" || basename == "index.tsx" || basename == "index.js" {
                    return false;
                }

                if !llm_all_files {
                    let source_path = std::path::Path::new(path);
                    let test_files = test_analyzer
                        .find_tests(repo, source_path)
                        .unwrap_or_default();

                    if test_files.is_empty() {
                        return false;
                    }

                    let any_test_changed = test_files.iter().any(|tf| {
                        test_analyzer
                            .diff_test_assertions(repo, tf, from_ref, to_ref)
                            .ok()
                            .map(|td| !td.full_diff.is_empty())
                            .unwrap_or(false)
                    });

                    if !any_test_changed {
                        return false;
                    }
                }

                true
            })
            .collect();

        if llm_all_files {
            eprintln!(
                "[BU] LLM file-level analysis: {} files (--llm-all-files)",
                filtered.len()
            );
        } else {
            eprintln!(
                "[BU] LLM file-level analysis: {} files with test changes (of {} with exported functions)",
                filtered.len(),
                unfiltered_count
            );
        }

        // Pre-fetch git diffs for each file
        for (file_path, funcs) in filtered {
            let diff_output = std::process::Command::new("git")
                .args([
                    "-C",
                    &repo.to_string_lossy(),
                    "diff",
                    from_ref,
                    to_ref,
                    "--",
                    &file_path,
                ])
                .output();

            let diff_content = match diff_output {
                Ok(output) => String::from_utf8_lossy(&output.stdout).to_string(),
                Err(_) => continue,
            };

            if diff_content.trim().is_empty() {
                continue;
            }

            let owned_funcs: Vec<semver_analyzer_core::ChangedFunction> =
                funcs.iter().map(|f| (*f).clone()).collect();

            files_for_llm.push(LlmFileTask {
                file_path,
                diff_content,
                functions: owned_funcs,
            });
        }

        // ── Include changed files without function body changes ─────
        // Some files have behavioral/type changes (CSS module imports,
        // forwardRef wrappers with unchanged bodies, enum-only changes)
        // that the diff_parser doesn't detect as function body changes.
        // If the file changed AND has changed tests, include it for LLM
        // analysis with an empty functions list.
        if !llm_all_files {
            let already_included: std::collections::HashSet<String> = files_for_llm
                .iter()
                .map(|t| t.file_path.clone())
                .collect();

            let all_changed_output = std::process::Command::new("git")
                .args([
                    "-C",
                    &repo.to_string_lossy(),
                    "diff",
                    "--name-only",
                    &format!("{}..{}", from_ref, to_ref),
                    "--",
                    "*.ts",
                    "*.tsx",
                ])
                .output();

            if let Ok(output) = all_changed_output {
                let all_changed_files: Vec<String> = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(|l| l.to_string())
                    .collect();

                let mut extra_count = 0;
                for file_path in all_changed_files {
                    // Skip if already included
                    if already_included.contains(&file_path) {
                        continue;
                    }

                    // Skip test files, .d.ts, index files, dist/ build output
                    let basename = Path::new(&file_path)
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if basename == "index.ts"
                        || basename == "index.tsx"
                        || basename == "index.js"
                        || basename.ends_with(".d.ts")
                        || basename.contains(".test.")
                        || basename.contains(".spec.")
                        || file_path.contains("__tests__")
                        || file_path.contains("/dist/")
                        || file_path.starts_with("dist/")
                    {
                        continue;
                    }

                    // Must have changed tests
                    let source_path = std::path::Path::new(&file_path);
                    let test_files = test_analyzer
                        .find_tests(repo, source_path)
                        .unwrap_or_default();

                    if test_files.is_empty() {
                        continue;
                    }

                    let any_test_changed = test_files.iter().any(|tf| {
                        test_analyzer
                            .diff_test_assertions(repo, tf, from_ref, to_ref)
                            .ok()
                            .map(|td| !td.full_diff.is_empty())
                            .unwrap_or(false)
                    });

                    if !any_test_changed {
                        continue;
                    }

                    // Get the diff
                    let diff_output = std::process::Command::new("git")
                        .args([
                            "-C",
                            &repo.to_string_lossy(),
                            "diff",
                            from_ref,
                            to_ref,
                            "--",
                            &file_path,
                        ])
                        .output();

                    let diff_content = match diff_output {
                        Ok(output) => String::from_utf8_lossy(&output.stdout).to_string(),
                        Err(_) => continue,
                    };

                    if diff_content.trim().is_empty() {
                        continue;
                    }

                    extra_count += 1;
                    files_for_llm.push(LlmFileTask {
                        file_path,
                        diff_content,
                        functions: vec![], // No function body changes detected
                    });
                }

                if extra_count > 0 {
                    eprintln!(
                        "[BU] + {} extra files (changed + tests changed, no function body changes)",
                        extra_count
                    );
                }
            }
        }
    }

    Ok(BuPhase1Result {
        stats,
        files_for_llm,
        llm_command,
    })
}

// ── Composition pattern analysis ─────────────────────────────────────

/// Analyze test and example file diffs for composition pattern changes.
///
/// Finds test files (`__tests__/*.test.tsx`) and example files (`examples/*.tsx`)
/// that changed between from_ref and to_ref, gets their diffs, and runs
/// the LLM to detect JSX nesting structure changes.
fn analyze_composition_patterns(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    llm_command: Option<&str>,
) -> Vec<(String, Vec<semver_analyzer_core::CompositionPatternChange>)> {
    let llm_cmd = match llm_command {
        Some(cmd) => cmd,
        None => return vec![],
    };

    // Find changed test and example files
    let output = match std::process::Command::new("git")
        .args([
            "diff",
            "--name-only",
            &format!("{}..{}", from_ref, to_ref),
            "--",
            "*.tsx",
            "*.ts",
        ])
        .current_dir(repo)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[warn] Failed to list changed files for composition analysis: {}", e);
            return vec![];
        }
    };

    let changed_files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| {
            // Only include test and example files from component packages
            let is_test = line.contains("__tests__/") || line.contains(".test.");
            let is_example = line.contains("/examples/");
            let is_component = line.contains("/components/") || line.contains("/demos/");
            (is_test || is_example) && is_component
        })
        // Skip token files, node_modules, etc.
        .filter(|line| {
            !line.contains("node_modules")
                && !line.contains("react-tokens")
                && !line.contains("/dist/")
        })
        .map(|s| s.to_string())
        .collect();

    if changed_files.is_empty() {
        return vec![];
    }

    eprintln!(
        "[BU] Phase 3: Analyzing {} test/example files for composition patterns",
        changed_files.len()
    );

    // Group by component directory to avoid sending too many LLM calls.
    // Analyze at most 10 files (the most relevant ones).
    let mut results = Vec::new();
    let analyzer = semver_analyzer_llm::LlmBehaviorAnalyzer::new(llm_cmd);
    let mut analyzed = 0;
    let max_files = 10;

    for file_path in &changed_files {
        if analyzed >= max_files {
            break;
        }

        // Get the diff for this file
        let diff_output = match std::process::Command::new("git")
            .args([
                "diff",
                &format!("{}..{}", from_ref, to_ref),
                "--",
                file_path,
            ])
            .current_dir(repo)
            .output()
        {
            Ok(o) => o,
            Err(_) => continue,
        };

        let diff_content = String::from_utf8_lossy(&diff_output.stdout).to_string();
        if diff_content.is_empty() || diff_content.len() < 50 {
            continue;
        }

        match analyzer.analyze_composition_patterns(file_path, &diff_content) {
            Ok(changes) if !changes.is_empty() => {
                eprintln!(
                    "  {} composition changes in {}",
                    changes.len(),
                    file_path
                );
                let mapped: Vec<semver_analyzer_core::CompositionPatternChange> = changes
                    .into_iter()
                    .map(|c| semver_analyzer_core::CompositionPatternChange {
                        component: c.component,
                        old_parent: c.old_parent,
                        new_parent: c.new_parent,
                        description: c.description,
                    })
                    .collect();
                // Derive the source component path from the test/example path
                let source_path = derive_source_from_test_path(file_path);
                results.push((source_path, mapped));
                analyzed += 1;
            }
            Ok(_) => {} // No changes found
            Err(e) => {
                eprintln!("  [warn] Composition analysis failed for {}: {}", file_path, e);
            }
        }
    }

    if !results.is_empty() {
        let total: usize = results.iter().map(|(_, v)| v.len()).sum();
        eprintln!(
            "[BU] Phase 3: Found {} composition pattern changes across {} files",
            total,
            results.len()
        );
    }

    results
}

/// Derive the source component path from a test or example file path.
///
/// e.g., `packages/react-core/src/components/Masthead/__tests__/Masthead.test.tsx`
///     → `packages/react-core/src/components/Masthead/`
///
/// e.g., `packages/react-core/src/components/Masthead/examples/MastheadBasic.tsx`
///     → `packages/react-core/src/components/Masthead/`
fn derive_source_from_test_path(test_path: &str) -> String {
    if let Some(pos) = test_path.find("__tests__/") {
        test_path[..pos].to_string()
    } else if let Some(pos) = test_path.find("/examples/") {
        test_path[..pos + 1].to_string()
    } else {
        // Fallback: use the directory
        test_path
            .rsplit_once('/')
            .map(|(dir, _)| format!("{}/", dir))
            .unwrap_or_else(|| test_path.to_string())
    }
}

// ── Rename inference ──────────────────────────────────────────────────

/// Infer rename patterns for constants and interfaces using LLM.
///
/// Called between the TD and BU phases. Makes up to 2 LLM calls:
/// 1. Constant rename patterns (when >50 removed + >50 added constants)
/// 2. Interface rename mappings (when >2 unmapped removed interfaces)
fn infer_rename_patterns(
    structural_changes: &[semver_analyzer_core::StructuralChange],
    old_surface: &semver_analyzer_core::ApiSurface,
    new_surface: &semver_analyzer_core::ApiSurface,
    llm_command: &str,
    from_ref: &str,
    to_ref: &str,
) -> Option<semver_analyzer_core::InferredRenamePatterns> {
    use semver_analyzer_core::{
        InferenceMetadata, InferredConstantPattern, InferredInterfaceMapping,
        InferredRenamePatterns, StructuralChangeType, SymbolKind,
    };
    use std::collections::{HashMap, HashSet};

    let mut llm_calls = 0;
    let mut constant_patterns = Vec::new();
    let mut interface_mappings = Vec::new();
    let mut constant_hit_rate = 0.0;

    // ── Call 1: Constant rename patterns ──────────────────────────

    // Group removed/added constants by package directory
    let mut removed_constants: HashMap<String, Vec<&str>> = HashMap::new();
    let mut added_constants: HashMap<String, Vec<&str>> = HashMap::new();

    for change in structural_changes {
        // Extract package from qualified name (e.g., "packages/react-tokens/src/...")
        let pkg = change
            .qualified_name
            .split('/')
            .take(2)
            .collect::<Vec<_>>()
            .join("/");

        match change.change_type {
            StructuralChangeType::SymbolRemoved => {
                // Check if it's a constant/variable (no members = standalone export)
                if !change.symbol.contains('.') {
                    removed_constants
                        .entry(pkg)
                        .or_default()
                        .push(&change.symbol);
                }
            }
            StructuralChangeType::SymbolAdded => {
                if !change.symbol.contains('.') {
                    added_constants
                        .entry(pkg)
                        .or_default()
                        .push(&change.symbol);
                }
            }
            _ => {}
        }
    }

    // Check each package for constant rename inference trigger
    for (pkg, removed) in &removed_constants {
        let added = match added_constants.get(pkg) {
            Some(a) if a.len() > 50 => a,
            _ => continue,
        };
        if removed.len() < 50 {
            continue;
        }

        eprintln!(
            "  Rename inference: {} has {} removed + {} added constants — inferring patterns",
            pkg,
            removed.len(),
            added.len()
        );

        // Sample: prioritize directional suffixes for better pattern discovery
        let directional_suffixes = [
            "Top", "Bottom", "Left", "Right", "Width", "Height",
            "MaxWidth", "MaxHeight", "MinWidth", "MinHeight",
        ];
        let mut removed_sample: Vec<&str> = removed
            .iter()
            .filter(|s| directional_suffixes.iter().any(|d| s.ends_with(d)))
            .take(20)
            .copied()
            .collect();
        // Fill remaining with random samples
        for s in removed.iter() {
            if removed_sample.len() >= 30 {
                break;
            }
            if !removed_sample.contains(s) {
                removed_sample.push(s);
            }
        }

        let mut added_sample: Vec<&str> = added
            .iter()
            .filter(|s| {
                ["BlockStart", "BlockEnd", "InlineStart", "InlineEnd", "InlineSize", "BlockSize"]
                    .iter()
                    .any(|d| s.contains(d))
            })
            .take(20)
            .copied()
            .collect();
        for s in added.iter() {
            if added_sample.len() >= 30 {
                break;
            }
            if !added_sample.contains(s) {
                added_sample.push(s);
            }
        }

        // Resolve package name from directory
        let pkg_name = pkg.replace("packages/", "@patternfly/");

        let analyzer = semver_analyzer_llm::LlmBehaviorAnalyzer::new(llm_command);
        match analyzer.infer_constant_renames(
            &removed_sample,
            &added_sample,
            &pkg_name,
            from_ref,
            to_ref,
        ) {
            Ok(patterns) => {
                llm_calls += 1;
                // Validate: apply each pattern against full lists
                let added_set: HashSet<&str> = added.iter().copied().collect();
                let total_removed = removed.len();
                let mut total_hits = 0;

                for llm_pat in patterns {
                    let re = match regex::Regex::new(&llm_pat.match_regex) {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!(
                                "    [warn] Invalid regex from LLM: '{}': {}",
                                llm_pat.match_regex, e
                            );
                            continue;
                        }
                    };

                    let mut hits = 0;
                    for name in removed.iter() {
                        if re.is_match(name) {
                            let replacement = re.replace(name, &llm_pat.replace);
                            if replacement == *name {
                                continue; // identity pattern, skip
                            }
                            if added_set.contains(replacement.as_ref()) {
                                hits += 1;
                            }
                        }
                    }

                    if hits > 0 {
                        eprintln!(
                            "    Pattern '{}' → '{}' matched {} constants",
                            llm_pat.match_regex, llm_pat.replace, hits
                        );
                        total_hits += hits;
                        constant_patterns.push(InferredConstantPattern {
                            match_regex: llm_pat.match_regex,
                            replace: llm_pat.replace,
                            hit_count: hits,
                            total_removed,
                        });
                    }
                }

                constant_hit_rate = if total_removed > 0 {
                    total_hits as f64 / total_removed as f64
                } else {
                    0.0
                };
                eprintln!(
                    "    Constant rename inference: {}/{} mapped ({:.0}%)",
                    total_hits,
                    total_removed,
                    constant_hit_rate * 100.0
                );
            }
            Err(e) => {
                eprintln!("  [warn] Constant rename inference failed: {}", e);
            }
        }
    }

    // ── Call 2: Interface/component rename mappings ───────────────

    // Find removed interfaces with no migration_target
    let removed_interfaces: Vec<(&str, Vec<String>)> = structural_changes
        .iter()
        .filter(|c| {
            c.change_type == StructuralChangeType::SymbolRemoved
                && c.migration_target.is_none()
                && (c.kind == "Interface" || c.kind == "Class")
                && !c.symbol.contains('.')
        })
        .filter_map(|c| {
            // Look up member names from old surface
            let sym = old_surface
                .symbols
                .iter()
                .find(|s| s.qualified_name == c.qualified_name)?;
            let members: Vec<String> = sym.members.iter().map(|m| m.name.clone()).collect();
            Some((c.symbol.as_str(), members))
        })
        .collect();

    // Find added interfaces
    let added_interfaces: Vec<(&str, Vec<String>)> = structural_changes
        .iter()
        .filter(|c| {
            c.change_type == StructuralChangeType::SymbolAdded
                && (c.kind == "Interface" || c.kind == "Class")
                && !c.symbol.contains('.')
        })
        .filter_map(|c| {
            let sym = new_surface
                .symbols
                .iter()
                .find(|s| s.qualified_name == c.qualified_name)?;
            let members: Vec<String> = sym.members.iter().map(|m| m.name.clone()).collect();
            Some((c.symbol.as_str(), members))
        })
        .collect();

    if removed_interfaces.len() > 2 && !added_interfaces.is_empty() {
        eprintln!(
            "  Rename inference: {} removed interfaces + {} added — inferring mappings",
            removed_interfaces.len(),
            added_interfaces.len()
        );

        // Cap at 20 each to keep the prompt manageable
        let removed_capped: Vec<(&str, &[String])> = removed_interfaces
            .iter()
            .take(20)
            .map(|(n, m)| (*n, m.as_slice()))
            .collect();
        let added_capped: Vec<(&str, &[String])> = added_interfaces
            .iter()
            .take(20)
            .map(|(n, m)| (*n, m.as_slice()))
            .collect();

        let analyzer = semver_analyzer_llm::LlmBehaviorAnalyzer::new(llm_command);
        match analyzer.infer_interface_renames(
            &removed_capped,
            &added_capped,
            "@patternfly/react-core", // TODO: determine from package context
            from_ref,
            to_ref,
        ) {
            Ok(mappings) => {
                llm_calls += 1;
                let removed_names: HashSet<&str> =
                    removed_interfaces.iter().map(|(n, _)| *n).collect();
                let added_names: HashSet<&str> =
                    added_interfaces.iter().map(|(n, _)| *n).collect();

                for mapping in mappings {
                    // Validate: both names must exist in the removed/added lists
                    if !removed_names.contains(mapping.old_name.as_str()) {
                        eprintln!(
                            "    [warn] LLM mapping old_name '{}' not in removed list, skipping",
                            mapping.old_name
                        );
                        continue;
                    }
                    if !added_names.contains(mapping.new_name.as_str()) {
                        eprintln!(
                            "    [warn] LLM mapping new_name '{}' not in added list, skipping",
                            mapping.new_name
                        );
                        continue;
                    }

                    // Compute member overlap for validation
                    let old_members: HashSet<&str> = removed_interfaces
                        .iter()
                        .find(|(n, _)| *n == mapping.old_name)
                        .map(|(_, m)| m.iter().map(|s| s.as_str()).collect())
                        .unwrap_or_default();
                    let new_members: HashSet<&str> = added_interfaces
                        .iter()
                        .find(|(n, _)| *n == mapping.new_name)
                        .map(|(_, m)| m.iter().map(|s| s.as_str()).collect())
                        .unwrap_or_default();
                    let overlap = old_members.intersection(&new_members).count();
                    let overlap_ratio = if old_members.is_empty() {
                        0.0
                    } else {
                        overlap as f64 / old_members.len() as f64
                    };

                    eprintln!(
                        "    Mapping '{}' → '{}' (confidence: {}, overlap: {:.0}%, reason: {})",
                        mapping.old_name,
                        mapping.new_name,
                        mapping.confidence,
                        overlap_ratio * 100.0,
                        mapping.reason
                    );

                    interface_mappings.push(InferredInterfaceMapping {
                        old_name: mapping.old_name,
                        new_name: mapping.new_name,
                        confidence: mapping.confidence,
                        reason: mapping.reason,
                        member_overlap_ratio: overlap_ratio,
                    });
                }
            }
            Err(e) => {
                eprintln!("  [warn] Interface rename inference failed: {}", e);
            }
        }
    }

    if llm_calls == 0 {
        return None;
    }

    Some(InferredRenamePatterns {
        constant_patterns,
        interface_mappings: interface_mappings.clone(),
        metadata: InferenceMetadata {
            llm_calls,
            constant_hit_rate,
            interface_mappings_found: interface_mappings.len(),
        },
    })
}

/// BU Phase 2: Concurrent LLM file analysis.
///
/// Runs up to `concurrency` LLM calls in parallel using tokio tasks.
async fn run_bu_phase2_llm(
    _repo: &Path,
    _from_ref: &str,
    _to_ref: &str,
    llm_command: &Option<String>,
    files: &[LlmFileTask],
    shared: &Arc<SharedFindings>,
    llm_api_entries: &Arc<std::sync::Mutex<Vec<LlmApiChangeEntry>>>,
) -> LlmPhaseStats {
    let cmd = match llm_command {
        Some(c) => c.clone(),
        None => return LlmPhaseStats::default(),
    };

    let total = files.len();
    let concurrency = 5;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let llm_calls = Arc::new(AtomicUsize::new(0));
    let llm_breaks = Arc::new(AtomicUsize::new(0));
    let llm_api_count = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    eprintln!("[BU] Starting LLM analysis ({} concurrent)...", concurrency);

    let mut handles = Vec::with_capacity(total);

    for task in files {
        let sem = semaphore.clone();
        let shared_ref = shared.clone();
        let api_entries = llm_api_entries.clone();
        let calls = llm_calls.clone();
        let breaks = llm_breaks.clone();
        let api_count = llm_api_count.clone();
        let done = completed.clone();
        let cmd = cmd.clone();
        let file_path = task.file_path.clone();
        let diff_content = task.diff_content.clone();
        let functions = task.functions.clone();
        let total = total;

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let idx = done.fetch_add(1, Ordering::Relaxed) + 1;
            let label = format!("[BU] [{}/{}]", idx, total);

            eprintln!("{} START {}", label, file_path);

            // Run the LLM call in a blocking task since it spawns a child process
            let result = tokio::task::spawn_blocking(move || {
                let analyzer = LlmBehaviorAnalyzer::new(&cmd);
                analyzer.analyze_file_diff(&file_path, &diff_content, &functions)
                    .map(|result| (file_path, result))
            })
            .await;

            calls.fetch_add(1, Ordering::Relaxed);

            match result {
                Ok(Ok((file_path, (beh_changes, api_changes)))) => {
                    let beh_count = beh_changes.len();
                    let api_cnt = api_changes.len();

                    for change in beh_changes {
                        breaks.fetch_add(1, Ordering::Relaxed);
                        let category = change.category.as_deref().and_then(parse_behavioral_category);
                        // Encode is_internal_only into the notes for downstream extraction
                        let mut notes = vec![change.description.clone()];
                        if change.is_internal_only == Some(true) {
                            notes.push("__is_internal_only__".to_string());
                        }
                        let brk = BehavioralBreak {
                            symbol: format!("{}::{}", file_path, change.symbol),
                            caused_by: format!("{}::{}", file_path, change.symbol),
                            call_path: vec![change.symbol.clone()],
                            evidence: EvidenceSource::LlmOnly {
                                spec_old: semver_analyzer_core::FunctionSpec {
                                    preconditions: vec![],
                                    postconditions: vec![],
                                    error_behavior: vec![],
                                    side_effects: vec![],
                                    notes: vec![],
                                },
                                spec_new: semver_analyzer_core::FunctionSpec {
                                    preconditions: vec![],
                                    postconditions: vec![],
                                    error_behavior: vec![],
                                    side_effects: vec![],
                                    notes,
                                },
                            },
                            confidence: 0.70,
                            description: change.description,
                            category,
                        };
                        shared_ref.insert_behavioral_break(brk);
                    }

                    for change in api_changes {
                        api_count.fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut entries) = api_entries.lock() {
                            entries.push(LlmApiChangeEntry {
                                file_path: file_path.clone(),
                                symbol: change.symbol,
                                change: change.change,
                                description: change.description,
                                removal_disposition: change.removal_disposition,
                                renders_element: change.renders_element,
                            });
                        }
                    }

                    match (beh_count, api_cnt) {
                        (0, 0) => eprintln!("{} DONE  (no breaks)", label),
                        (b, 0) => eprintln!("{} DONE  ({} behavioral)", label, b),
                        (0, a) => eprintln!("{} DONE  ({} API)", label, a),
                        (b, a) => eprintln!("{} DONE  ({} behavioral, {} API)", label, b, a),
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("{} ERROR ({})", label, e);
                }
                Err(e) => {
                    eprintln!("{} PANIC ({})", label, e);
                }
            }
        });

        handles.push(handle);
    }

    // Wait for all tasks
    for handle in handles {
        let _ = handle.await;
    }

    LlmPhaseStats {
        llm_calls: llm_calls.load(Ordering::Relaxed),
        llm_behavioral_breaks: llm_breaks.load(Ordering::Relaxed),
        llm_api_changes: llm_api_count.load(Ordering::Relaxed),
    }
}

/// Walk UP the call graph from a private function with a behavioral break.
///
/// Finds all callers of the function within the same file. For each caller:
/// - If exported/public → record a transitive behavioral break
/// - If private → continue walking up
///
/// Uses a visited set for cycle detection.
fn walk_up_call_graph(
    call_graph: &impl CallGraphBuilder,
    source_file: &Path,
    symbol_name: &str,
    qualified_name: &str,
    original_break: &BehavioralBreak,
    shared: &SharedFindings,
) -> usize {
    let mut propagated = 0;
    let mut to_check = vec![(symbol_name.to_string(), qualified_name.to_string())];
    let mut visited = HashSet::new();

    while let Some((current_name, current_qname)) = to_check.pop() {
        if !visited.insert(current_qname.clone()) {
            continue; // Cycle detection
        }

        let callers = match call_graph.find_callers(source_file, &current_name) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for caller in callers {
            // Skip if TD already found this symbol
            if shared.has_structural_break(&caller.qualified_name) {
                continue;
            }

            if caller.visibility == Visibility::Exported
                || caller.visibility == Visibility::Public
            {
                // Public caller affected by private behavioral change
                let mut call_path = original_break.call_path.clone();
                // Build the path from public caller down to root cause
                let caller_name = caller
                    .qualified_name
                    .rsplit("::")
                    .next()
                    .unwrap_or(&caller.qualified_name)
                    .to_string();
                call_path.insert(0, caller_name);

                shared.insert_behavioral_break(BehavioralBreak {
                    symbol: caller.qualified_name.clone(),
                    caused_by: original_break.caused_by.clone(),
                    call_path,
                    evidence: original_break.evidence.clone(),
                    confidence: original_break.confidence * 0.9, // Slight confidence decay for transitive
                    description: format!(
                        "Behavioral change in {} propagated through call chain",
                        original_break.caused_by
                    ),
                    category: original_break.category.clone(), // Propagate parent's category
                });
                propagated += 1;
            } else {
                // Private caller — continue walking up
                let caller_name = caller
                    .qualified_name
                    .rsplit("::")
                    .next()
                    .unwrap_or(&caller.qualified_name)
                    .to_string();
                to_check.push((caller_name, caller.qualified_name));
            }
        }
    }

    propagated
}

// ── Report Merging ──────────────────────────────────────────────────────

/// Convert behavioral breaks from SharedFindings into v2 BehavioralChange entries.
fn merge_behavioral_breaks(shared: &SharedFindings) -> Vec<BehavioralChange> {
    shared
        .behavioral_breaks()
        .iter()
        .map(|entry| {
            let brk = entry.value();

            // Extract file path from the qualified name (e.g.,
            // "packages/react-core/.../Modal.tsx::Modal" → file is the part before "::")
            let source_file = if brk.symbol.contains("::") {
                let parts: Vec<&str> = brk.symbol.splitn(2, "::").collect();
                Some(parts[0].to_string())
            } else {
                None
            };

            // Determine kind from evidence or call path
            let kind = match &brk.evidence {
                EvidenceSource::LlmOnly { .. } | EvidenceSource::LlmWithTestContext { .. } => {
                    BehavioralChangeKind::Class // LLM file-level analysis = component-level
                }
                EvidenceSource::TestDelta { .. } => BehavioralChangeKind::Function,
                EvidenceSource::JsxDiff { .. } => BehavioralChangeKind::Class, // JSX diff = component-level
            };

            // Preserve evidence type from the BU pipeline
            let evidence_type = Some(match &brk.evidence {
                EvidenceSource::TestDelta { .. } => "TestDelta".to_string(),
                EvidenceSource::JsxDiff { .. } => "JsxDiff".to_string(),
                EvidenceSource::LlmOnly { .. } => "LlmOnly".to_string(),
                EvidenceSource::LlmWithTestContext { .. } => "LlmWithTestContext".to_string(),
            });

            // Extract component names referenced in the description
            let referenced_components = extract_component_refs(&brk.description);

            // Extract is_internal_only from notes (encoded by the LLM ingestion)
            let is_internal_only = match &brk.evidence {
                EvidenceSource::LlmOnly { spec_new, .. }
                | EvidenceSource::LlmWithTestContext { spec_new, .. } => {
                    if spec_new.notes.iter().any(|n| n == "__is_internal_only__") {
                        Some(true)
                    } else {
                        None
                    }
                }
                _ => None,
            };

            BehavioralChange {
                symbol: extract_display_name(&brk.symbol),
                kind,
                category: brk.category.clone(),
                description: brk.description.clone(),
                source_file,
                confidence: Some(brk.confidence),
                evidence_type,
                referenced_components,
                is_internal_only,
            }
        })
        .collect()
}

/// Extract PascalCase component name references from a behavioral change description.
///
/// Looks for patterns like `<ComponentName>`, `ComponentName component`, or
/// backtick-quoted PascalCase identifiers. Returns deduplicated component names.
fn extract_component_refs(description: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Pattern 1: JSX-style <ComponentName> or <ComponentName ...>
    let mut remaining = description;
    while let Some(start) = remaining.find('<') {
        let after_lt = &remaining[start + 1..];
        // Find the end of the tag name (space, >, or /)
        let end = after_lt
            .find(|c: char| c == '>' || c == ' ' || c == '/')
            .unwrap_or(after_lt.len());
        let name = &after_lt[..end];
        // Must be PascalCase (starts with uppercase, has lowercase)
        if !name.is_empty()
            && name.chars().next().map_or(false, |c| c.is_ascii_uppercase())
            && name.chars().all(|c| c.is_ascii_alphanumeric())
            && name.chars().any(|c| c.is_ascii_lowercase())
        {
            if seen.insert(name.to_string()) {
                refs.push(name.to_string());
            }
        }
        remaining = &remaining[start + 1..];
    }

    // Pattern 2: backtick-quoted PascalCase identifiers like `Modal`
    let mut remaining = description;
    while let Some(start) = remaining.find('`') {
        let after_tick = &remaining[start + 1..];
        if let Some(end) = after_tick.find('`') {
            let name = &after_tick[..end];
            if !name.is_empty()
                && name.chars().next().map_or(false, |c| c.is_ascii_uppercase())
                && name.chars().all(|c| c.is_ascii_alphanumeric())
                && name.chars().any(|c| c.is_ascii_lowercase())
                && !name.contains(' ')
            {
                if seen.insert(name.to_string()) {
                    refs.push(name.to_string());
                }
            }
            remaining = &after_tick[end + 1..];
        } else {
            break;
        }
    }

    refs
}

/// Extract a human-readable display name from a qualified name.
///
/// `src/api/users.ts::createUser` → `createUser`
/// `src/service.ts::Service::validate` → `Service.validate`
fn extract_display_name(qualified_name: &str) -> String {
    // Split on `::` to get file prefix and symbol parts
    let parts: Vec<&str> = qualified_name.split("::").collect();
    match parts.len() {
        0 | 1 => qualified_name.to_string(),
        2 => parts[1].to_string(),
        _ => {
            // class::method → Class.method
            parts[1..].join(".")
        }
    }
}

// ── Package.json Helper (moved from main.rs) ────────────────────────────

fn diff_package_json(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
) -> Vec<semver_analyzer_core::ManifestChange> {
    let old_json = read_git_file(repo, from_ref, "package.json");
    let new_json = read_git_file(repo, to_ref, "package.json");

    match (old_json, new_json) {
        (Some(old_str), Some(new_str)) => {
            let old: serde_json::Value = match serde_json::from_str(&old_str) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!(
                        "[TD]   Warning: could not parse package.json at {}: {}",
                        from_ref, e
                    );
                    return Vec::new();
                }
            };
            let new: serde_json::Value = match serde_json::from_str(&new_str) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!(
                        "[TD]   Warning: could not parse package.json at {}: {}",
                        to_ref, e
                    );
                    return Vec::new();
                }
            };
            semver_analyzer_ts::manifest::diff_manifests(&old, &new)
        }
        _ => Vec::new(),
    }
}

/// Parse a behavioral category string from an LLM response into the enum.
fn parse_behavioral_category(s: &str) -> Option<semver_analyzer_core::BehavioralCategory> {
    use semver_analyzer_core::BehavioralCategory;
    match s.trim().to_lowercase().replace('-', "_").as_str() {
        "dom_structure" | "dom" | "render" => Some(BehavioralCategory::DomStructure),
        "css_class" | "css" => Some(BehavioralCategory::CssClass),
        "css_variable" | "css_var" => Some(BehavioralCategory::CssVariable),
        "accessibility" | "a11y" => Some(BehavioralCategory::Accessibility),
        "default_value" | "default" => Some(BehavioralCategory::DefaultValue),
        "logic_change" | "logic" | "side_effect" => Some(BehavioralCategory::LogicChange),
        "data_attribute" | "data" | "ouia" => Some(BehavioralCategory::DataAttribute),
        "render_output" | "visual" => Some(BehavioralCategory::RenderOutput),
        _ => None,
    }
}

fn read_git_file(repo: &Path, git_ref: &str, file_path: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["show", &format!("{}:{}", git_ref, file_path)])
        .current_dir(repo)
        .output()
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_display_name_simple() {
        assert_eq!(
            extract_display_name("src/api/users.ts::createUser"),
            "createUser"
        );
    }

    #[test]
    fn extract_display_name_class_method() {
        assert_eq!(
            extract_display_name("src/service.ts::Service::validate"),
            "Service.validate"
        );
    }

    #[test]
    fn extract_display_name_no_separator() {
        assert_eq!(extract_display_name("createUser"), "createUser");
    }

    #[test]
    fn extract_display_name_file_only() {
        assert_eq!(
            extract_display_name("src/utils.ts::helper"),
            "helper"
        );
    }
}
