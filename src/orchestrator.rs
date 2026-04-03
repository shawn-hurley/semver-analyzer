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
    diff_surfaces_with_semantics, should_skip_for_bu, ApiSurface, BehavioralBreak,
    BehavioralChange, ChangeSubject, ChangedFunction, ContainerChange, EvidenceType,
    InferenceMetadata, InferredConstantPattern, InferredInterfaceMapping, InferredRenamePatterns,
    Language, LlmApiChange, ManifestChange, SharedFindings, StructuralChange,
    StructuralChangeType, Symbol, Visibility,
};
use semver_analyzer_llm::LlmBehaviorAnalyzer;

use crate::progress::ProgressReporter;

use regex::Regex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, info_span, trace, warn};

/// Maximum concurrent LLM calls for file analysis and hierarchy inference.
const LLM_CONCURRENCY: usize = 5;

/// Maximum number of interfaces to include in a single LLM rename inference prompt.
const MAX_INTERFACES_FOR_INFERENCE: usize = 20;

/// Bundles a language implementation with its analysis components.
///
/// Constructed once per analysis run. The `run()` method executes the
/// full concurrent TD+BU pipeline.
pub struct Analyzer<L: Language> {
    pub lang: Arc<L>,
}

impl<L: Language> Analyzer<L> {
    /// Run the full concurrent TD+BU analysis pipeline.
    ///
    /// Returns structural changes, behavioral breaks, and manifest changes.
    /// The caller (`cmd_analyze`) assembles these into the final report.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &self,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
        no_llm: bool,
        llm_command: Option<&str>,
        build_command: Option<&str>,
        llm_all_files: bool,
        progress: &ProgressReporter,
    ) -> Result<AnalysisResult<L>> {
        let _span = info_span!("analyze_pipeline", %from_ref, %to_ref).entered();
        let shared = Arc::new(SharedFindings::<L>::new());
        let llm_api_entries = Arc::new(Mutex::new(Vec::<LlmApiChange>::new()));
        let llm_cmd_default = llm_command.unwrap_or("goose run --no-session -q -t");

        // ── Stage 1: TD + (BU Phase 1 → Phase 2) concurrently ──────────
        //
        // TD:  Extract API surfaces + structural diff  (blocking, slow)
        // BU:  Phase 1 (git diff + tests, fast) then immediately starts
        //      Phase 2 (LLM file analysis) — overlaps with TD.

        // Owned clones for TD's spawn_blocking
        let lang_td = self.lang.clone();
        let repo_td = repo.to_path_buf();
        let from_td = from_ref.to_string();
        let to_td = to_ref.to_string();
        let build_cmd = build_command.map(|s| s.to_string());
        let shared_td = shared.clone();
        let progress_td = progress.clone();

        // Owned clones for BU's spawn_blocking (Phase 1) and async Phase 2
        let lang_bu = self.lang.clone();
        let repo_bu = repo.to_path_buf();
        let from_bu = from_ref.to_string();
        let to_bu = to_ref.to_string();
        let llm_cmd = llm_command.map(|s| s.to_string());
        let shared_bu_phase1 = shared.clone();
        let shared_bu_phase2 = shared.clone();
        let progress_bu_phase1 = progress.clone();
        let progress_bu_phase2 = progress.clone();
        let llm_api_entries_bu = llm_api_entries.clone();

        // Additional clones for rename inference (run inside TD branch)
        let lang_rename = self.lang.clone();
        let from_hierarchy = from_ref.to_string();
        let to_hierarchy = to_ref.to_string();
        let llm_cmd_rename = llm_cmd_default.to_string();
        let progress_rename = progress.clone();
        let shared_inference = shared.clone();

        let (td_inference_result, bu_result) = tokio::join!(
            // TD → Rename Inference + Hierarchy Inference (chained).
            // Rename and hierarchy start as soon as TD finishes, running
            // concurrently with BU Phase 2 LLM calls.
            async move {
                // TD: blocking (extract surfaces, structural diff)
                let td = tokio::task::spawn_blocking(move || {
                    Self::run_td(
                        lang_td.as_ref(),
                        &repo_td,
                        &from_td,
                        &to_td,
                        build_cmd.as_deref(),
                        &shared_td,
                        &progress_td,
                    )
                })
                .await
                .map_err(|e| anyhow::anyhow!("TD task panicked: {}", e))?
                .context("TD pipeline failed")?;

                // TD done — extract surfaces for inference phases (Arc clones, cheap)
                let default_surface = Arc::new(ApiSurface::default());
                let old_surface = shared_inference
                    .try_get_old_surface()
                    .cloned()
                    .unwrap_or_else(|| default_surface.clone());
                let new_surface = shared_inference
                    .try_get_new_surface()
                    .cloned()
                    .unwrap_or_else(|| default_surface.clone());

                // Run rename inference (overlaps with BU Phase 2)
                // NOTE: Hierarchy inference has been moved to
                // Language::run_extended_analysis() as part of the
                // genericization effort. It will be re-added when the
                // v1 pipeline calls run_extended_analysis after TD+BU.
                let inferred_rename_patterns = if !no_llm {
                    let changes_rename = td.structural_changes.clone();
                    let old_surf_rename = old_surface.clone();
                    let new_surf_rename = new_surface.clone();
                    let from_rename = from_hierarchy.clone();
                    let to_rename = to_hierarchy.clone();

                    tokio::task::spawn_blocking(move || {
                        let _span = info_span!("rename_inference").entered();
                        let rename_phase =
                            progress_rename.start_phase("Inferring rename patterns");
                        let result = Self::infer_rename_patterns(
                            lang_rename.as_ref(),
                            &changes_rename,
                            &old_surf_rename,
                            &new_surf_rename,
                            &llm_cmd_rename,
                            &from_rename,
                            &to_rename,
                        );
                        rename_phase.finish("Rename inference complete");
                        result
                    })
                    .await
                    .unwrap_or(None)
                } else {
                    None
                };

                Ok::<_, anyhow::Error>((
                    td,
                    old_surface,
                    new_surface,
                    inferred_rename_patterns,
                ))
            },
            // BU: Phase 1 → Phase 2 chained (independent of TD/inference)
            async move {
                // Phase 1: blocking (git diff parse, test analysis, body analysis)
                let phase1 = tokio::task::spawn_blocking(move || {
                    Self::run_bu_phase1(
                        lang_bu.as_ref(),
                        &repo_bu,
                        &from_bu,
                        &to_bu,
                        llm_cmd,
                        llm_all_files,
                        &shared_bu_phase1,
                        &progress_bu_phase1,
                    )
                })
                .await
                .map_err(|e| anyhow::anyhow!("BU Phase 1 panicked: {}", e))?
                .context("BU Phase 1 pipeline failed")?;

                // Phase 2: async LLM file analysis (overlaps with TD + inference)
                let mut llm_stats = LlmPhaseStats::default();
                let mut container_changes: Vec<(String, Vec<ContainerChange>)> = vec![];

                if !no_llm && !phase1.files_for_llm.is_empty() {
                    let (stats, comp) = Self::run_bu_phase2_llm(
                        phase1.llm_command.as_deref(),
                        &phase1.files_for_llm,
                        &shared_bu_phase2,
                        &llm_api_entries_bu,
                        &progress_bu_phase2,
                    )
                    .await;
                    llm_stats = stats;
                    container_changes = comp;
                }

                Ok::<_, anyhow::Error>((phase1.stats, llm_stats, container_changes))
            },
        );

        // Unwrap results from both branches
        let (
            td,
            old_surface,
            new_surface,
            inferred_rename_patterns,
        ) = td_inference_result?;

        let (phase1_stats, llm_stats, container_changes) = bu_result?;

        // Merge behavioral results from both pipelines
        let behavioral_changes = Self::merge_behavioral_breaks(self.lang.as_ref(), &shared);
        let llm_api_changes = match Arc::try_unwrap(llm_api_entries) {
            Ok(mutex) => mutex.into_inner().unwrap_or_default(),
            Err(arc) => arc.lock().unwrap().clone(),
        };

        let bu_stats = BuStats {
            changed_function_count: phase1_stats.changed_function_count,
            skipped_by_td: phase1_stats.skipped_by_td,
            test_behavioral_breaks: phase1_stats.test_behavioral_breaks,
            llm_behavioral_breaks: llm_stats.llm_behavioral_breaks,
            llm_calls: llm_stats.llm_calls,
            call_graph_propagated: phase1_stats.call_graph_propagated,
        };

        info!(
            skipped_by_td = bu_stats.skipped_by_td,
            test_breaks = bu_stats.test_behavioral_breaks,
            llm_breaks = bu_stats.llm_behavioral_breaks,
            llm_calls = bu_stats.llm_calls,
            llm_api = llm_api_changes.len(),
            propagated = bu_stats.call_graph_propagated,
            "BU pipeline summary"
        );
        progress.println(&format!(
        "  [BU] {} skipped (TD found), {} test-based breaks, {} LLM breaks ({} calls), {} LLM API, {} propagated up",
        bu_stats.skipped_by_td,
        bu_stats.test_behavioral_breaks,
        bu_stats.llm_behavioral_breaks,
        bu_stats.llm_calls,
        llm_api_changes.len(),
        bu_stats.call_graph_propagated,
    ));

        // TODO: hierarchy inference currently disabled in v1 pipeline.
        // It will be re-implemented inside Language::run_extended_analysis()
        // so the language owns the entire hierarchy lifecycle.
        let extensions = L::AnalysisExtensions::default();

        Ok(AnalysisResult {
            structural_changes: td.structural_changes,
            behavioral_changes,
            manifest_changes: td.manifest_changes,
            llm_api_changes,
            old_surface,
            new_surface,
            inferred_rename_patterns,
            container_changes,
            extensions,
        })
    }
    /// Run the v2 concurrent TD+SD analysis pipeline.
    ///
    /// Replaces the BU pipeline with the deterministic SD (Source-Level Diff)
    /// pipeline. TD runs for structural changes; SD runs for source-level
    /// change facts. Both run concurrently via `tokio::join!`.
    ///
    /// Optionally runs rename inference (LLM) after TD completes, but skips
    /// BU entirely — no test-delta analysis, no LLM file analysis.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_v2(
        &self,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
        no_llm: bool,
        llm_command: Option<&str>,
        build_command: Option<&str>,
        dep_css_dir: Option<&Path>,
        progress: &ProgressReporter,
    ) -> Result<AnalysisResult<L>> {
        let _span = info_span!("analyze_pipeline_v2", %from_ref, %to_ref).entered();
        let shared = Arc::new(SharedFindings::<L>::new());

        // ── Owned clones for TD's spawn_blocking ────────────────────
        let lang_td = self.lang.clone();
        let repo_td = repo.to_path_buf();
        let from_td = from_ref.to_string();
        let to_td = to_ref.to_string();
        let build_cmd = build_command.map(|s| s.to_string());
        let shared_td = shared.clone();
        let progress_td = progress.clone();

        // ── Owned clones for SD's spawn_blocking ────────────────────
        let lang_sd = self.lang.clone();
        let repo_sd = repo.to_path_buf();
        let from_sd = from_ref.to_string();
        let to_sd = to_ref.to_string();
        let dep_css_dir_sd = dep_css_dir.map(|p| p.to_path_buf());
        let progress_sd = progress.clone();

        // ── Owned clones for rename inference ────────────────────────
        // Note: hierarchy inference is skipped in v2 — the SD pipeline
        // derives composition trees deterministically from BEM, DOM
        // nesting, context, and name-prefix signals.
        let lang_rename = self.lang.clone();
        let from_rename_ref = from_ref.to_string();
        let to_rename_ref = to_ref.to_string();
        let llm_cmd_default = llm_command.unwrap_or("goose run --no-session -q -t");
        let llm_cmd_rename = llm_cmd_default.to_string();
        let progress_rename = progress.clone();
        let shared_inference = shared.clone();

        // ── Run TD + extended analysis concurrently ────────────────
        let no_llm_val = no_llm;
        let llm_cmd_sd = llm_command.map(|s| s.to_string());
        let (td_inference_result, ext_result) = tokio::join!(
            // TD branch: structural diff → rename inference → hierarchy inference
            async move {
                // TD: blocking (extract surfaces, structural diff)
                let td = tokio::task::spawn_blocking(move || {
                    Self::run_td(
                        lang_td.as_ref(),
                        &repo_td,
                        &from_td,
                        &to_td,
                        build_cmd.as_deref(),
                        &shared_td,
                        &progress_td,
                    )
                })
                .await
                .map_err(|e| anyhow::anyhow!("TD task panicked: {}", e))?
                .context("TD pipeline failed")?;

                // TD done — extract surfaces for inference phases
                let default_surface = Arc::new(ApiSurface::default());
                let old_surface = shared_inference
                    .try_get_old_surface()
                    .cloned()
                    .unwrap_or_else(|| default_surface.clone());
                let new_surface = shared_inference
                    .try_get_new_surface()
                    .cloned()
                    .unwrap_or_else(|| default_surface.clone());

                // Run rename inference (if LLM enabled).
                // Hierarchy inference is skipped in v2 — the SD pipeline
                // derives composition trees deterministically.
                let inferred_rename_patterns = if !no_llm {
                    let changes_rename = td.structural_changes.clone();
                    let old_surf_rename = old_surface.clone();
                    let new_surf_rename = new_surface.clone();
                    let from_rename = from_rename_ref.clone();
                    let to_rename = to_rename_ref.clone();

                    tokio::task::spawn_blocking(move || {
                        let _span = info_span!("rename_inference").entered();
                        let rename_phase =
                            progress_rename.start_phase("Inferring rename patterns");
                        let result = Self::infer_rename_patterns(
                            lang_rename.as_ref(),
                            &changes_rename,
                            &old_surf_rename,
                            &new_surf_rename,
                            &llm_cmd_rename,
                            &from_rename,
                            &to_rename,
                        );
                        rename_phase.finish("Rename inference complete");
                        result
                    })
                    .await
                    .unwrap_or(None)
                } else {
                    None
                };
                Ok::<_, anyhow::Error>((
                    td,
                    old_surface,
                    new_surface,
                    inferred_rename_patterns,
                ))
            },
            // Extended analysis branch: language-specific pipelines (independent of TD)
            async move {
                let ext_phase = progress_sd.start_phase("[EXT] Language-specific analysis ...");
                let no_llm_sd = no_llm_val;
                let result = tokio::task::spawn_blocking(move || {
                    lang_sd.run_extended_analysis(
                        &repo_sd,
                        &from_sd,
                        &to_sd,
                        &[], // structural_changes not yet available
                        &semver_analyzer_core::ApiSurface::default(),
                        &semver_analyzer_core::ApiSurface::default(),
                        llm_cmd_sd.as_deref(),
                        dep_css_dir_sd.as_deref(),
                        no_llm_sd,
                    )
                })
                .await
                .map_err(|e| anyhow::anyhow!("Extended analysis task panicked: {}", e))?;

                match &result {
                    Ok(_) => {
                        ext_phase.finish("[EXT] Language-specific analysis complete");
                    }
                    Err(e) => {
                        warn!(%e, "Extended analysis failed");
                        ext_phase.finish("[EXT] Language-specific analysis failed");
                    }
                }

                result
            },
        );

        // ── Unwrap results ──────────────────────────────────────────
        let (
            td,
            old_surface,
            new_surface,
            inferred_rename_patterns,
        ) = td_inference_result?;

        let extensions = match ext_result {
            Ok(ext) => ext,
            Err(e) => {
                warn!(%e, "Extended analysis failed, continuing with empty results");
                L::AnalysisExtensions::default()
            }
        };

        // ── Summary logging ─────────────────────────────────────────
        progress.println(&format!("  [EXT] {:?}", extensions));

        Ok(AnalysisResult {
            structural_changes: td.structural_changes,
            behavioral_changes: vec![], // No BU in v2
            manifest_changes: td.manifest_changes,
            llm_api_changes: vec![], // No LLM file analysis in v2
            old_surface,
            new_surface,
            inferred_rename_patterns,
            container_changes: vec![], // No BU container changes in v2
            extensions,
        })
    }
} // end impl Analyzer<L> (public API)

/// Re-export `AnalysisResult` from core as the orchestrator's output type.
pub use semver_analyzer_core::AnalysisResult;

/// Stats from the TD pipeline.
/// Fields are printed during analysis and retained for future structured output.
#[allow(dead_code)]
pub(crate) struct TdStats {
    pub old_symbol_count: usize,
    pub new_symbol_count: usize,
    pub structural_change_count: usize,
    pub structural_breaking_count: usize,
}

/// Stats from the BU pipeline.
pub(crate) struct BuStats {
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
    functions: Vec<ChangedFunction>,
    /// Git diff of the associated test file (if any test assertions changed).
    /// Included so the LLM can detect composition pattern changes from tests.
    test_diff: Option<String>,
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
}

// ── TD Pipeline ─────────────────────────────────────────────────────────

struct TdResult<L: Language> {
    structural_changes: Arc<Vec<StructuralChange>>,
    manifest_changes: Vec<ManifestChange<L>>,
    #[allow(dead_code)]
    stats: TdStats,
}

impl<L: Language> Analyzer<L> {
    fn run_td(
        lang: &L,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
        _build_command: Option<&str>,
        shared: &SharedFindings<L>,
        progress: &ProgressReporter,
    ) -> Result<TdResult<L>> {
        let _span = info_span!("td_pipeline", %from_ref, %to_ref).entered();

        // Step 1: Extract API surface at old ref
        let phase =
            progress.start_phase(&format!("[TD] Extracting API surface at {} ...", from_ref));
        let old_surface =
            {
                let _extract_span = info_span!("extract_surface", git_ref = %from_ref).entered();
                Arc::new(lang.extract(repo, from_ref).with_context(|| {
                    format!("Failed to extract API surface at ref {}", from_ref)
                })?)
            };
        let old_count = old_surface.symbols.len();
        phase.finish_with_detail(
            &format!("[TD] Extracted API surface at {}", from_ref),
            &format!("{} symbols", old_count),
        );
        info!(symbols = old_count, git_ref = %from_ref, "old surface extracted");

        // Store in shared state — Arc clone is a cheap refcount bump
        shared.set_old_surface(old_surface.clone());

        // Step 2: Extract API surface at new ref
        let phase = progress.start_phase(&format!("[TD] Extracting API surface at {} ...", to_ref));
        let new_surface = {
            let _extract_span = info_span!("extract_surface", git_ref = %to_ref).entered();
            Arc::new(
                lang.extract(repo, to_ref)
                    .with_context(|| format!("Failed to extract API surface at ref {}", to_ref))?,
            )
        };
        let new_count = new_surface.symbols.len();
        phase.finish_with_detail(
            &format!("[TD] Extracted API surface at {}", to_ref),
            &format!("{} symbols", new_count),
        );
        info!(symbols = new_count, git_ref = %to_ref, "new surface extracted");

        shared.set_new_surface(new_surface.clone());

        // Step 3: Structural diff (using language-specific semantics)
        let phase = progress.start_phase("[TD] Computing structural diff ...");
        let structural_changes = {
            let _diff_span = info_span!("structural_diff").entered();
            diff_surfaces_with_semantics(&old_surface, &new_surface, lang)
        };
        let breaking = structural_changes.iter().filter(|c| c.is_breaking).count();
        phase.finish_with_detail(
            "[TD] Structural diff complete",
            &format!(
                "{} changes ({} breaking)",
                structural_changes.len(),
                breaking
            ),
        );
        info!(
            total = structural_changes.len(),
            breaking, "structural diff complete"
        );

        // Insert all breaking changes into shared state (broadcasts to BU)
        let breaking_changes: Vec<_> = structural_changes
            .iter()
            .filter(|c| c.is_breaking)
            .cloned()
            .collect();
        shared.insert_structural_breaks(breaking_changes);

        // Step 4: Manifest diff (language-specific)
        let _manifest_span = info_span!("manifest_diff").entered();
        let mut manifest_changes = Vec::new();
        for manifest_file in L::MANIFEST_FILES {
            let old_content = read_git_file(repo, from_ref, manifest_file);
            let new_content = read_git_file(repo, to_ref, manifest_file);
            if let (Some(old_str), Some(new_str)) = (old_content, new_content) {
                let changes = L::diff_manifest_content(&old_str, &new_str);
                manifest_changes.extend(changes);
            }
        }

        let total_changes = structural_changes.len();

        Ok(TdResult {
            structural_changes: Arc::new(structural_changes),
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
    #[allow(clippy::too_many_arguments)]
    fn run_bu_phase1(
        lang: &L,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
        llm_command: Option<String>,
        llm_all_files: bool,
        shared: &SharedFindings<L>,
        progress: &ProgressReporter,
    ) -> Result<BuPhase1Result> {
        let _span = info_span!("bu_pipeline_phase1").entered();

        // Step 1: Parse git diff to find all changed functions
        let phase = progress.start_phase("[BU] Parsing changed functions ...");
        let changed_fns = {
            let _parse_span = info_span!("parse_changed_functions").entered();
            lang.parse_changed_functions(repo, from_ref, to_ref)
                .context("Failed to parse changed functions")?
        };
        phase.finish_with_detail(
            "[BU] Parsed changed functions",
            &format!("{} found", changed_fns.len()),
        );
        info!(count = changed_fns.len(), "changed functions parsed");

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
        let _test_span = info_span!("test_analysis").entered();
        for func in &changed_fns {
            if should_skip_for_bu(shared, &mut receiver, &func.qualified_name) {
                stats.skipped_by_td += 1;
                continue;
            }

            if func.old_body.is_empty() || func.new_body.is_empty() {
                continue;
            }

            let test_files = lang.find_tests(repo, &func.file).unwrap_or_default();

            let test_diff = test_files.iter().find_map(|tf| {
                lang.diff_test_assertions(repo, tf, from_ref, to_ref)
                    .ok()
                    .filter(|td| td.has_assertion_changes)
            });

            if let Some(td) = test_diff {
                let description = format!(
                    "Test assertions changed: {} removed, {} added",
                    td.removed_assertions.len(),
                    td.added_assertions.len()
                );
                let evidence_description = format!(
                    "Test assertion changes detected: {} removed, {} added in {}",
                    td.removed_assertions.len(),
                    td.added_assertions.len(),
                    td.full_diff.lines().count(),
                );
                let brk = BehavioralBreak::<L> {
                    symbol: func.qualified_name.clone(),
                    caused_by: func.qualified_name.clone(),
                    call_path: vec![func.name.clone()],
                    evidence_description,
                    confidence: 0.95,
                    description,
                    category: None, // Test-delta: category inferred later or by body analyzer
                    evidence_type: EvidenceType::TestDelta,
                    is_internal_only: None,
                };
                stats.test_behavioral_breaks += 1;

                if func.visibility == Visibility::Exported || func.visibility == Visibility::Public
                {
                    shared.insert_behavioral_break(brk);
                } else {
                    let source_file = repo.join(&func.file);
                    if source_file.exists() {
                        let propagated = Self::walk_up_call_graph(
                            lang,
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
        drop(_test_span);

        // ── Deterministic body analysis (per-function, no LLM) ──────────
        // Delegates to the language's body analyzer (e.g., JSX diff + CSS scan
        // for TypeScript) if available.
        let _body_span = info_span!("body_analysis").entered();
        let mut body_change_count = 0;
        if let Some(body_analyzer) = lang.body_analyzer() {
            for func in &changed_fns {
                if func.old_body.is_empty() || func.new_body.is_empty() {
                    continue;
                }
                if func.visibility != Visibility::Exported && func.visibility != Visibility::Public
                {
                    continue;
                }

                let results = body_analyzer.analyze_changed_body(
                    &func.old_body,
                    &func.new_body,
                    &func.name,
                    &func.file.to_string_lossy(),
                );
                for result in results {
                    // Check if TD already found this symbol (avoid duplicates)
                    if should_skip_for_bu(shared, &mut receiver, &func.qualified_name) {
                        continue;
                    }

                    let category: Option<L::Category> =
                        result.category_label.as_deref().and_then(|s| {
                            serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
                        });
                    let brk = BehavioralBreak::<L> {
                        symbol: func.qualified_name.clone(),
                        caused_by: func.qualified_name.clone(),
                        call_path: vec![func.name.clone()],
                        evidence_description: result.description.clone(),
                        confidence: result.confidence,
                        description: result.description,
                        category,
                        evidence_type: EvidenceType::BodyAnalysis,
                        is_internal_only: None,
                    };
                    shared.insert_behavioral_break(brk);
                    body_change_count += 1;
                }
            }
        }

        if body_change_count > 0 {
            info!(
                count = body_change_count,
                "body-level changes detected deterministically"
            );
        }
        drop(_body_span);

        // ── Prepare file list for LLM Phase 2 ───────────────────────────
        let mut files_for_llm = Vec::new();

        if llm_command.is_some() {
            // Group functions by file
            let mut by_file: BTreeMap<String, Vec<&ChangedFunction>> = BTreeMap::new();
            for func in &changed_fns {
                if func.old_body.is_empty() || func.new_body.is_empty() {
                    continue;
                }
                let file_key = func.file.to_string_lossy().to_string();
                by_file.entry(file_key).or_default().push(func);
            }

            let unfiltered_count = by_file.len();
            // filter_map keeps the test_diff from the filter pass so we don't
            // fetch it twice (each fetch_test_diff spawns git subprocesses).
            let filtered: Vec<_> = by_file
                .into_iter()
                .filter_map(|(path, funcs)| {
                    let has_exported = funcs.iter().any(|f| {
                        f.visibility == Visibility::Exported || f.visibility == Visibility::Public
                    });
                    if !has_exported {
                        return None;
                    }

                    // Use language-specific exclusion rules
                    if L::should_exclude_from_analysis(Path::new(&path)) {
                        return None;
                    }

                    let test_diff = if !llm_all_files {
                        let td = fetch_test_diff(lang, repo, Path::new(&path), from_ref, to_ref);
                        td.as_ref()?;
                        td
                    } else {
                        fetch_test_diff(lang, repo, Path::new(&path), from_ref, to_ref)
                    };

                    Some((path, funcs, test_diff))
                })
                .collect();

            if llm_all_files {
                info!(
                    files = filtered.len(),
                    "LLM file-level analysis (--llm-all-files)"
                );
            } else {
                info!(
                    files = filtered.len(),
                    total_with_exports = unfiltered_count,
                    "LLM file-level analysis (test-change filtered)"
                );
            }

            // Pre-fetch git diffs for each file
            for (file_path, funcs, test_diff) in filtered {
                let diff_content = match git_diff_file(repo, from_ref, to_ref, &file_path) {
                    Some(d) => d,
                    None => continue,
                };

                if diff_content.trim().is_empty() {
                    continue;
                }

                let owned_funcs: Vec<ChangedFunction> =
                    funcs.iter().map(|f| (*f).clone()).collect();

                files_for_llm.push(LlmFileTask {
                    file_path,
                    diff_content,
                    functions: owned_funcs,
                    test_diff,
                });
            }

            // ── Include changed files without function body changes ─────
            // Some files have behavioral/type changes (CSS module imports,
            // forwardRef wrappers with unchanged bodies, enum-only changes)
            // that the diff_parser doesn't detect as function body changes.
            // If the file changed AND has changed tests, include it for LLM
            // analysis with an empty functions list.
            if !llm_all_files {
                let already_included: HashSet<String> =
                    files_for_llm.iter().map(|t| t.file_path.clone()).collect();

                // Build git diff --name-only args with language-specific patterns
                let mut git_args = vec![
                    "-C".to_string(),
                    repo.to_string_lossy().to_string(),
                    "diff".to_string(),
                    "--name-only".to_string(),
                    format!("{}..{}", from_ref, to_ref),
                    "--".to_string(),
                ];
                for pattern in L::SOURCE_FILE_PATTERNS {
                    git_args.push(pattern.to_string());
                }

                let all_changed_output = Command::new("git").args(&git_args).output();

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

                        // Use language-specific exclusion rules
                        if L::should_exclude_from_analysis(Path::new(&file_path)) {
                            continue;
                        }

                        // Must have changed tests
                        let test_diff_content =
                            fetch_test_diff(lang, repo, Path::new(&file_path), from_ref, to_ref);
                        if test_diff_content.is_none() {
                            continue;
                        }

                        // Get the diff
                        let diff_content = match git_diff_file(repo, from_ref, to_ref, &file_path) {
                            Some(d) => d,
                            None => continue,
                        };

                        if diff_content.trim().is_empty() {
                            continue;
                        }

                        extra_count += 1;
                        files_for_llm.push(LlmFileTask {
                            file_path,
                            diff_content,
                            functions: vec![], // No function body changes detected
                            test_diff: test_diff_content,
                        });
                    }

                    if extra_count > 0 {
                        debug!(
                        count = extra_count,
                        "extra files included (changed + tests changed, no function body changes)"
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

    // ── Rename inference ──────────────────────────────────────────────────

    /// Infer rename patterns for constants and interfaces using LLM.
    ///
    /// Called between the TD and BU phases. Makes up to 2 LLM calls:
    /// 1. Constant rename patterns (when enough removed + added constants)
    /// 2. Interface rename mappings (when enough unmapped removed interfaces)
    fn infer_rename_patterns(
        lang: &L,
        structural_changes: &[StructuralChange],
        old_surface: &ApiSurface<L::SymbolData>,
        new_surface: &ApiSurface<L::SymbolData>,
        llm_command: &str,
        from_ref: &str,
        to_ref: &str,
    ) -> Option<InferredRenamePatterns> {
        let renames = lang.renames()?;

        let mut llm_calls = 0;
        let mut constant_patterns = Vec::new();
        let mut interface_mappings = Vec::new();
        let mut constant_hit_rate = 0.0;

        // ── Call 1: Constant rename patterns ──────────────────────────

        // Group removed/added constants by package directory
        let mut removed_constants: HashMap<String, Vec<&str>> = HashMap::new();
        let mut added_constants: HashMap<String, Vec<&str>> = HashMap::new();

        for change in structural_changes {
            let pkg = change.package.as_deref().unwrap_or("").to_string();

            match &change.change_type {
                StructuralChangeType::Removed(ChangeSubject::Symbol { .. }) => {
                    removed_constants
                        .entry(pkg)
                        .or_default()
                        .push(&change.symbol);
                }
                StructuralChangeType::Added(ChangeSubject::Symbol { .. }) => {
                    added_constants.entry(pkg).or_default().push(&change.symbol);
                }
                _ => {}
            }
        }

        let min_removed_constants = renames.min_removed_for_constant_inference();

        // Check each package for constant rename inference trigger
        for (pkg, removed) in &removed_constants {
            let added = match added_constants.get(pkg) {
                Some(a) if a.len() > min_removed_constants => a,
                _ => continue,
            };
            if removed.len() < min_removed_constants {
                continue;
            }

            info!(
                package = %pkg,
                removed = removed.len(),
                added = added.len(),
                "inferring constant rename patterns"
            );

            // Sample using language-specific strategy
            let removed_sample = renames.sample_removed_constants(removed, added);
            let added_sample = renames.sample_added_constants(removed, added);

            // Use the package name directly (already set by extractor)
            let pkg_name = pkg.to_string();

            let _llm_span = info_span!("llm_constant_renames", %pkg_name).entered();
            let analyzer = LlmBehaviorAnalyzer::new(llm_command);
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
                        let re = match Regex::new(&llm_pat.match_regex) {
                            Ok(r) => r,
                            Err(e) => {
                                warn!(
                                    regex = %llm_pat.match_regex,
                                    error = %e,
                                    "invalid regex from LLM"
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
                            debug!(
                                pattern = %llm_pat.match_regex,
                                replace = %llm_pat.replace,
                                hits,
                                "rename pattern matched"
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
                    info!(
                        mapped = total_hits,
                        total = total_removed,
                        hit_rate_pct = format_args!("{:.0}", constant_hit_rate * 100.0),
                        "constant rename inference complete"
                    );
                }
                Err(e) => {
                    warn!(%e, "constant rename inference failed");
                }
            }
        }

        // ── Call 2: Interface/component rename mappings ───────────────

        let min_removed_interfaces = renames.min_removed_for_interface_inference();

        // Build O(1) lookup indexes by qualified_name — avoids O(n) linear
        // scans per structural change when the symbol lists are large.
        let old_by_qname: HashMap<&str, &Symbol<L::SymbolData>> = old_surface
            .symbols
            .iter()
            .map(|s| (s.qualified_name.as_str(), s))
            .collect();
        let new_by_qname: HashMap<&str, &Symbol<L::SymbolData>> = new_surface
            .symbols
            .iter()
            .map(|s| (s.qualified_name.as_str(), s))
            .collect();

        // Find removed interfaces with no migration_target
        let removed_interfaces: Vec<(&str, Vec<String>)> = structural_changes
            .iter()
            .filter(|c| {
                matches!(
                    c.change_type,
                    StructuralChangeType::Removed(ChangeSubject::Symbol { .. })
                ) && c.migration_target.is_none()
                    && L::RENAMEABLE_SYMBOL_KINDS.contains(&c.kind)
            })
            .filter_map(|c| {
                let sym = old_by_qname.get(c.qualified_name.as_str())?;
                let members: Vec<String> = sym.members.iter().map(|m| m.name.clone()).collect();
                Some((c.symbol.as_str(), members))
            })
            .collect();

        // Find added interfaces
        let added_interfaces: Vec<(&str, Vec<String>)> = structural_changes
            .iter()
            .filter(|c| {
                matches!(
                    c.change_type,
                    StructuralChangeType::Added(ChangeSubject::Symbol { .. })
                ) && L::RENAMEABLE_SYMBOL_KINDS.contains(&c.kind)
            })
            .filter_map(|c| {
                let sym = new_by_qname.get(c.qualified_name.as_str())?;
                let members: Vec<String> = sym.members.iter().map(|m| m.name.clone()).collect();
                Some((c.symbol.as_str(), members))
            })
            .collect();

        if removed_interfaces.len() > min_removed_interfaces && !added_interfaces.is_empty() {
            info!(
                removed = removed_interfaces.len(),
                added = added_interfaces.len(),
                "inferring interface rename mappings"
            );

            // Cap to keep the prompt manageable
            let removed_capped: Vec<(&str, &[String])> = removed_interfaces
                .iter()
                .take(MAX_INTERFACES_FOR_INFERENCE)
                .map(|(n, m)| (*n, m.as_slice()))
                .collect();
            let added_capped: Vec<(&str, &[String])> = added_interfaces
                .iter()
                .take(MAX_INTERFACES_FOR_INFERENCE)
                .map(|(n, m)| (*n, m.as_slice()))
                .collect();

            // Use the first package's name for context (already a display name from extractor)
            let first_pkg = removed_constants
                .keys()
                .next()
                .or_else(|| added_constants.keys().next())
                .cloned()
                .unwrap_or_default();

            let _llm_span = info_span!("llm_interface_renames").entered();
            let analyzer = LlmBehaviorAnalyzer::new(llm_command);
            match analyzer.infer_interface_renames(
                &removed_capped,
                &added_capped,
                &first_pkg,
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
                            warn!(
                                old_name = %mapping.old_name,
                                "LLM mapping old_name not in removed list, skipping"
                            );
                            continue;
                        }
                        if !added_names.contains(mapping.new_name.as_str()) {
                            warn!(
                                new_name = %mapping.new_name,
                                "LLM mapping new_name not in added list, skipping"
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

                        trace!(
                            old = %mapping.old_name,
                            new = %mapping.new_name,
                            confidence = mapping.confidence,
                            overlap_pct = format_args!("{:.0}", overlap_ratio * 100.0),
                            reason = %mapping.reason,
                            "interface rename mapping"
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
                    warn!(%e, "interface rename inference failed");
                }
            }
        }

        if llm_calls == 0 {
            return None;
        }

        let interface_mappings_count = interface_mappings.len();
        Some(InferredRenamePatterns {
            constant_patterns,
            interface_mappings,
            metadata: InferenceMetadata {
                llm_calls,
                constant_hit_rate,
                interface_mappings_found: interface_mappings_count,
            },
        })
    }

    /// BU Phase 2: Concurrent LLM file analysis.
    ///
    /// Runs up to `concurrency` LLM calls in parallel using tokio tasks.
    /// Displays a progress bar showing overall completion.
    #[allow(clippy::type_complexity)]
    async fn run_bu_phase2_llm(
        llm_command: Option<&str>,
        files: &[LlmFileTask],
        shared: &Arc<SharedFindings<L>>,
        llm_api_entries: &Arc<Mutex<Vec<LlmApiChange>>>,
        progress: &ProgressReporter,
    ) -> (LlmPhaseStats, Vec<(String, Vec<ContainerChange>)>) {
        let _span = info_span!("bu_pipeline_phase2", file_count = files.len()).entered();
        let cmd = match llm_command {
            Some(c) => c.to_string(),
            None => return (LlmPhaseStats::default(), vec![]),
        };

        let total = files.len();
        let concurrency = LLM_CONCURRENCY;
        let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
        let llm_calls = Arc::new(AtomicUsize::new(0));
        let llm_breaks = Arc::new(AtomicUsize::new(0));
        let composition_entries: Arc<Mutex<Vec<(String, Vec<ContainerChange>)>>> =
            Arc::new(Mutex::new(Vec::new()));

        info!(total, concurrency, "starting LLM file analysis");
        let bar = progress.start_counted("[BU] LLM Analysis", total as u64);

        let mut handles = Vec::with_capacity(total);

        for task in files {
            let sem = semaphore.clone();
            let shared_ref = shared.clone();
            let api_entries = llm_api_entries.clone();
            let calls = llm_calls.clone();
            let breaks = llm_breaks.clone();
            let comp_entries = composition_entries.clone();
            let cmd = cmd.clone();
            let file_path = task.file_path.clone();
            let diff_content = task.diff_content.clone();
            let functions = task.functions.clone();
            let test_diff = task.test_diff.clone();

            let handle = tokio::spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore closed");

                debug!(file = %file_path, "LLM analysis started");

                // Run the LLM call in a blocking task since it spawns a child process
                // Run with retry: if the first attempt fails (e.g., truncated
                // JSON response), retry once before giving up.
                let result = tokio::task::spawn_blocking({
                    let file_path = file_path.clone();
                    move || {
                        let _llm_span = info_span!("llm_file_analysis", %file_path).entered();
                        let analyzer = LlmBehaviorAnalyzer::new(&cmd);
                        let first = analyzer.analyze_file_diff(
                            &file_path,
                            &diff_content,
                            &functions,
                            test_diff.as_deref(),
                        );
                        match first {
                            Ok(result) => Ok((file_path, result)),
                            Err(e) => {
                                warn!(
                                    file = %file_path,
                                    %e,
                                    "LLM analysis failed, retrying"
                                );
                                // Retry once
                                analyzer
                                    .analyze_file_diff(
                                        &file_path,
                                        &diff_content,
                                        &functions,
                                        test_diff.as_deref(),
                                    )
                                    .map(|result| (file_path, result))
                            }
                        }
                    }
                })
                .await;

                calls.fetch_add(1, Ordering::Relaxed);

                match result {
                    Ok(Ok((file_path, (beh_changes, api_changes, comp_changes)))) => {
                        let beh_count = beh_changes.len();
                        let api_cnt = api_changes.len();
                        let comp_cnt = comp_changes.len();

                        // Store composition pattern changes
                        if !comp_changes.is_empty() {
                            let mapped: Vec<ContainerChange> = comp_changes
                                .into_iter()
                                .map(|c| ContainerChange {
                                    symbol: c.component,
                                    old_container: c.old_parent,
                                    new_container: c.new_parent,
                                    description: c.description,
                                })
                                .collect();
                            if let Ok(mut entries) = comp_entries.lock() {
                                entries.push((file_path.clone(), mapped));
                            }
                        }

                        for change in beh_changes {
                            breaks.fetch_add(1, Ordering::Relaxed);
                            let category: Option<L::Category> =
                                change.category.as_deref().and_then(|s| {
                                    serde_json::from_value(serde_json::Value::String(s.to_string()))
                                        .ok()
                                });
                            let evidence_description =
                                format!("LLM behavioral analysis: {}", change.description);
                            let brk = BehavioralBreak::<L> {
                                symbol: format!("{}::{}", file_path, change.symbol),
                                caused_by: format!("{}::{}", file_path, change.symbol),
                                call_path: vec![change.symbol.clone()],
                                evidence_description,
                                confidence: 0.70,
                                description: change.description,
                                category,
                                evidence_type: EvidenceType::LlmAnalysis,
                                is_internal_only: change.is_internal_only,
                            };
                            shared_ref.insert_behavioral_break(brk);
                        }

                        for change in api_changes {
                            if let Ok(mut entries) = api_entries.lock() {
                                entries.push(LlmApiChange {
                                    file_path: file_path.clone(),
                                    symbol: change.symbol,
                                    change: change.change,
                                    description: change.description,
                                    removal_disposition: change.removal_disposition,
                                    renders_element: change.renders_element,
                                });
                            }
                        }

                        debug!(
                            file = %file_path,
                            behavioral = beh_count,
                            api = api_cnt,
                            composition = comp_cnt,
                            "LLM analysis complete"
                        );
                    }
                    Ok(Err(e)) => {
                        error!(file = %file_path, %e, "LLM analysis failed after retry");
                    }
                    Err(e) => {
                        error!(%e, "LLM analysis panicked");
                    }
                }
            });

            handles.push(handle);
        }

        // Wait for all tasks, incrementing the progress bar
        for handle in handles {
            let _ = handle.await;
            bar.inc();
        }
        bar.finish();

        let comp_results = match Arc::try_unwrap(composition_entries) {
            Ok(mutex) => mutex.into_inner().unwrap_or_default(),
            Err(arc) => arc.lock().unwrap().clone(),
        };

        (
            LlmPhaseStats {
                llm_calls: llm_calls.load(Ordering::Relaxed),
                llm_behavioral_breaks: llm_breaks.load(Ordering::Relaxed),
            },
            comp_results,
        )
    }

    /// Walk UP the call graph from a private function with a behavioral break.
    ///
    /// Finds all callers of the function within the same file. For each caller:
    /// - If exported/public → record a transitive behavioral break
    /// - If private → continue walking up
    ///
    /// Uses a visited set for cycle detection.
    fn walk_up_call_graph(
        lang: &L,
        source_file: &Path,
        symbol_name: &str,
        qualified_name: &str,
        original_break: &BehavioralBreak<L>,
        shared: &SharedFindings<L>,
    ) -> usize {
        let mut propagated = 0;
        let mut to_check = vec![(symbol_name.to_string(), qualified_name.to_string())];
        let mut visited = HashSet::new();

        while let Some((current_name, current_qname)) = to_check.pop() {
            if !visited.insert(current_qname.clone()) {
                continue; // Cycle detection
            }

            let callers = match lang.find_callers(source_file, &current_name) {
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

                    shared.insert_behavioral_break(BehavioralBreak::<L> {
                        symbol: caller.qualified_name.clone(),
                        caused_by: original_break.caused_by.clone(),
                        call_path,
                        evidence_description: original_break.evidence_description.clone(),
                        confidence: original_break.confidence * 0.9, // Slight confidence decay for transitive
                        description: format!(
                            "Behavioral change in {} propagated through call chain",
                            original_break.caused_by
                        ),
                        category: original_break.category.clone(), // Propagate parent's category
                        evidence_type: EvidenceType::CallGraphPropagation,
                        is_internal_only: original_break.is_internal_only,
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
    fn merge_behavioral_breaks(lang: &L, shared: &SharedFindings<L>) -> Vec<BehavioralChange<L>> {
        shared
            .behavioral_breaks()
            .iter()
            .map(|entry| {
                let brk = entry.value();

                let source_file = if brk.symbol.contains("::") {
                    let parts: Vec<&str> = brk.symbol.splitn(2, "::").collect();
                    Some(parts[0].to_string())
                } else {
                    None
                };

                let kind = lang.behavioral_change_kind(&brk.evidence_type);
                let evidence_type = Some(brk.evidence_type.clone());
                let referenced_symbols = lang.extract_referenced_symbols(&brk.description);
                let is_internal_only = brk.is_internal_only;

                BehavioralChange {
                    symbol: lang.display_name(&brk.symbol),
                    kind,
                    category: brk.category.clone(),
                    description: brk.description.clone(),
                    source_file,
                    confidence: Some(brk.confidence),
                    evidence_type,
                    referenced_symbols,
                    is_internal_only,
                }
            })
            .collect()
    }
} // end impl Analyzer<L> (private methods, part 1)

fn git_diff_file(repo: &Path, from_ref: &str, to_ref: &str, file_path: &str) -> Option<String> {
    let output = Command::new("git")
        .args([
            "-C",
            &repo.to_string_lossy(),
            "diff",
            &format!("{}..{}", from_ref, to_ref),
            "--",
            file_path,
        ])
        .output()
        .ok()?;
    if output.status.success() {
        let content = String::from_utf8_lossy(&output.stdout).to_string();
        if content.is_empty() {
            None
        } else {
            Some(content)
        }
    } else {
        None
    }
}

fn fetch_test_diff<L: Language>(
    lang: &L,
    repo: &Path,
    source_file: &Path,
    from_ref: &str,
    to_ref: &str,
) -> Option<String> {
    let test_files = lang.find_tests(repo, source_file).unwrap_or_default();
    test_files.iter().find_map(|tf| {
        let td = lang.diff_test_assertions(repo, tf, from_ref, to_ref).ok()?;
        if td.full_diff.is_empty() {
            None
        } else {
            Some(td.full_diff)
        }
    })
}

fn read_git_file(repo: &Path, git_ref: &str, file_path: &str) -> Option<String> {
    let output = Command::new("git")
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

// ── Component Hierarchy Inference ────────────────────────────────────────
//
// NOTE: Hierarchy inference has been moved to the Language implementation
// (e.g., TypeScript::run_extended_analysis). The language provides prompts
// and processes results; the orchestrator handles LLM invocation.
//
// The ~420 lines of hierarchy inference code that were here have been
// removed as part of the genericization effort. The hierarchy concept
// is framework-specific (React, Vue, etc.) and does not belong in the
// generic orchestrator.
//
// To re-enable hierarchy inference:
// 1. Language::run_extended_analysis() returns hierarchy data in AnalysisExtensions
// 2. The orchestrator calls run_extended_analysis() after TD completes
// 3. For LLM-based inference, the Language provides prompts via a trait method
//    and the orchestrator handles invocation/concurrency

// The hierarchy inference code has been removed. See git history for
// the original `infer_and_diff_hierarchies` implementation.

impl<L: Language> Analyzer<L> {
    // Placeholder — hierarchy methods removed during genericization.
}

// NOTE: ~420 lines of hierarchy inference code (infer_and_diff_hierarchies)
// were removed during genericization. See git history for the original
// implementation. The hierarchy lifecycle is now owned by the Language
// implementation via run_extended_analysis().
