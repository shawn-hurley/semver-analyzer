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

    Ok(AnalysisResult {
        structural_changes: td.structural_changes,
        behavioral_changes,
        manifest_changes: td.manifest_changes,
        llm_api_changes,
        td_stats: td.stats,
        bu_stats,
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
}

/// An API change detected by the LLM during file-level analysis.
#[derive(Debug, Clone)]
pub struct LlmApiChangeEntry {
    pub file_path: String,
    pub symbol: String,
    pub change: String,
    pub description: String,
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

                    // Skip test files, .d.ts, index files
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
                                    notes: vec![change.description.clone()],
                                },
                            },
                            confidence: 0.70,
                            description: change.description,
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
            };

            BehavioralChange {
                symbol: extract_display_name(&brk.symbol),
                kind,
                description: brk.description.clone(),
                source_file,
            }
        })
        .collect()
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
