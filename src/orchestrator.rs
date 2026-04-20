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
    BehavioralChange, ChangeSubject, ChangedFunction, EvidenceType, ExpectedChild, HierarchyDelta,
    InferenceMetadata, InferredConstantPattern, InferredInterfaceMapping, InferredRenamePatterns,
    Language, LlmApiChange, ManifestChange, SharedFindings, StructuralChange, StructuralChangeType,
    Symbol, Visibility,
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
        llm_timeout: u64,
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

        // LLM behavioral categories from the Language impl
        let llm_categories = self.lang.llm_categories();

        // Additional clones for rename/hierarchy inference (run inside TD branch)
        let lang_rename = self.lang.clone();
        let lang_hierarchy = self.lang.clone();
        let repo_hierarchy = repo.to_path_buf();
        let from_hierarchy = from_ref.to_string();
        let to_hierarchy = to_ref.to_string();
        let llm_cmd_rename = llm_cmd_default.to_string();
        let llm_cmd_hierarchy = llm_cmd_default.to_string();
        let progress_rename = progress.clone();
        let progress_hierarchy = progress.clone();
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
                let default_surface: Arc<ApiSurface<L::SymbolData>> =
                    Arc::new(ApiSurface::default());
                let old_surface = shared_inference
                    .try_get_old_surface()
                    .cloned()
                    .unwrap_or_else(|| default_surface.clone());
                let new_surface = shared_inference
                    .try_get_new_surface()
                    .cloned()
                    .unwrap_or_else(|| default_surface.clone());

                // Run rename + hierarchy concurrently (overlaps with BU Phase 2)
                let (inferred_rename_patterns, (hierarchy_deltas, new_hierarchies)) = if !no_llm {
                    // Arc clones for rename inference's spawn_blocking
                    let changes_rename = td.structural_changes.clone();
                    let old_surf_rename = old_surface.clone();
                    let new_surf_rename = new_surface.clone();
                    let from_rename = from_hierarchy.clone();
                    let to_rename = to_hierarchy.clone();

                    tokio::join!(
                        // Rename inference: sync (1-2 LLM calls)
                        async {
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
                                    llm_timeout,
                                );
                                rename_phase.finish("Rename inference complete");
                                result
                            })
                            .await
                            .unwrap_or(None)
                        },
                        // Hierarchy inference: async (N concurrent LLM calls)
                        Self::infer_and_diff_hierarchies(
                            lang_hierarchy.as_ref(),
                            &repo_hierarchy,
                            &from_hierarchy,
                            &to_hierarchy,
                            &llm_cmd_hierarchy,
                            &td.structural_changes,
                            &old_surface,
                            &new_surface,
                            llm_timeout,
                            &progress_hierarchy,
                        ),
                    )
                } else {
                    (None, (Vec::new(), HashMap::new()))
                };

                Ok::<_, anyhow::Error>((
                    td,
                    old_surface,
                    new_surface,
                    inferred_rename_patterns,
                    hierarchy_deltas,
                    new_hierarchies,
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
                if !no_llm && !phase1.files_for_llm.is_empty() {
                    llm_stats = Self::run_bu_phase2_llm(
                        phase1.llm_command.as_deref(),
                        &phase1.files_for_llm,
                        &shared_bu_phase2,
                        &llm_api_entries_bu,
                        llm_timeout,
                        &progress_bu_phase2,
                        &llm_categories,
                    )
                    .await;
                }

                Ok::<_, anyhow::Error>((phase1.stats, llm_stats))
            },
        );

        // Unwrap results from both branches
        let (
            td,
            old_surface,
            new_surface,
            inferred_rename_patterns,
            _hierarchy_deltas,
            _new_hierarchies,
        ) = td_inference_result?;

        let (phase1_stats, llm_stats) = bu_result?;

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

        Ok(AnalysisResult {
            structural_changes: td.structural_changes,
            behavioral_changes,
            manifest_changes: td.manifest_changes,
            llm_api_changes,
            old_surface,
            new_surface,
            inferred_rename_patterns,
            container_changes: vec![],
            extensions: L::AnalysisExtensions::default(),
            degradation: shared.degradation_arc(),
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
        _build_command: Option<&str>,
        dep_dir: Option<&Path>,
        dep_from: Option<&str>,
        dep_to: Option<&str>,
        dep_build_command: Option<&str>,
        llm_timeout: u64,
        progress: &ProgressReporter,
    ) -> Result<AnalysisResult<L>> {
        let _span = info_span!("analyze_pipeline_v2", %from_ref, %to_ref).entered();
        let shared = Arc::new(SharedFindings::<L>::new());

        // ── Owned clones for TD's parallel extraction ─────────────────
        // Two spawn_blocking tasks run concurrently (from-ref + to-ref),
        // then a third runs the diff + manifest analysis.
        let lang_from = self.lang.clone();
        let lang_to = self.lang.clone();
        let lang_td = self.lang.clone();
        let repo_from = repo.to_path_buf();
        let repo_to = repo.to_path_buf();
        let repo_td = repo.to_path_buf();
        let from_td = from_ref.to_string();
        let to_td = to_ref.to_string();
        let from_extract = from_ref.to_string();
        let to_extract = to_ref.to_string();
        let shared_from = shared.clone();
        let shared_to = shared.clone();
        let shared_td = shared.clone();
        let progress_from = progress.clone();
        let progress_to = progress.clone();
        let progress_td = progress.clone();

        // ── Owned clones for SD's spawn_blocking ────────────────────
        let lang_sd = self.lang.clone();
        let repo_sd = repo.to_path_buf();
        let from_sd = from_ref.to_string();
        let to_sd = to_ref.to_string();
        let dep_dir_sd = dep_dir.map(|p| p.to_path_buf());
        let dep_from_sd = dep_from.map(|s| s.to_string());
        let dep_to_sd = dep_to.map(|s| s.to_string());
        let dep_build_cmd_sd = dep_build_command.map(|s| s.to_string());
        let progress_sd = progress.clone();
        let shared_sd = shared.clone();

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

        // ── Channels for worktree sharing (TD → SD) ────────────────
        // TD creates worktrees during extraction. It sends Arc clones
        // through these channels so SD can use the filesystem paths
        // for oxc_resolver-based import resolution.
        let (from_wt_tx, from_wt_rx) = std::sync::mpsc::channel::<
            Option<Arc<dyn semver_analyzer_core::traits::WorktreeAccess>>,
        >();
        let (to_wt_tx, to_wt_rx) = std::sync::mpsc::channel::<
            Option<Arc<dyn semver_analyzer_core::traits::WorktreeAccess>>,
        >();

        // ── Run TD + SD concurrently ────────────────────────────────
        let (td_inference_result, ext_result) = tokio::join!(
            // TD branch: parallel extraction → diff → rename inference
            async move {
                let shared_td_outer = shared_td.clone();

                // ── Parallel extraction: from-ref and to-ref concurrently ──
                // Each extraction creates a worktree (yarn install + tsc build)
                // and sends the worktree handle to SD immediately when ready.
                let (from_result, to_result) = tokio::join!(
                    tokio::task::spawn_blocking(move || {
                        let _span = info_span!("td_pipeline").entered();
                        let phase = progress_from.start_phase(&format!(
                            "[TD] Extracting API surface at {} ...",
                            from_extract
                        ));
                        let _extract_span =
                            info_span!("extract_surface", git_ref = %from_extract).entered();
                        let (surface, wt) = lang_from
                            .extract_keeping_worktree(
                                &repo_from,
                                &from_extract,
                                Some(shared_from.degradation()),
                            )
                            .with_context(|| {
                                format!("Failed to extract API surface at ref {}", from_extract)
                            })?;
                        let surface = Arc::new(surface);
                        let count = surface.symbols.len();
                        phase.finish_with_detail(
                            &format!("[TD] Extracted API surface at {}", from_extract),
                            &format!("{} symbols", count),
                        );
                        info!(symbols = count, git_ref = %from_extract, "old surface extracted");
                        shared_from.set_old_surface(surface.clone());
                        // Send worktree handle to SD immediately
                        let _ = from_wt_tx.send(wt.as_ref().map(Arc::clone));
                        Ok::<_, anyhow::Error>((surface, wt))
                    }),
                    tokio::task::spawn_blocking(move || {
                        let _span = info_span!("td_pipeline").entered();
                        let phase = progress_to.start_phase(&format!(
                            "[TD] Extracting API surface at {} ...",
                            to_extract
                        ));
                        let _extract_span =
                            info_span!("extract_surface", git_ref = %to_extract).entered();
                        let (surface, wt) = lang_to
                            .extract_keeping_worktree(
                                &repo_to,
                                &to_extract,
                                Some(shared_to.degradation()),
                            )
                            .with_context(|| {
                                format!("Failed to extract API surface at ref {}", to_extract)
                            })?;
                        let surface = Arc::new(surface);
                        let count = surface.symbols.len();
                        phase.finish_with_detail(
                            &format!("[TD] Extracted API surface at {}", to_extract),
                            &format!("{} symbols", count),
                        );
                        info!(symbols = count, git_ref = %to_extract, "new surface extracted");
                        shared_to.set_new_surface(surface.clone());
                        // Send worktree handle to SD immediately
                        let _ = to_wt_tx.send(wt.as_ref().map(Arc::clone));
                        Ok::<_, anyhow::Error>((surface, wt))
                    }),
                );

                let (old_surface, old_wt) = from_result
                    .map_err(|e| anyhow::anyhow!("TD from-ref extraction panicked: {}", e))?
                    .context("TD from-ref extraction failed")?;
                let (new_surface, new_wt) = to_result
                    .map_err(|e| anyhow::anyhow!("TD to-ref extraction panicked: {}", e))?
                    .context("TD to-ref extraction failed")?;

                // ── Diff + manifest analysis (needs both surfaces) ──
                let td = tokio::task::spawn_blocking(move || {
                    let _span =
                        info_span!("td_pipeline", from_ref = %from_td, to_ref = %to_td).entered();
                    Self::run_td_analyze(
                        lang_td.as_ref(),
                        &repo_td,
                        &from_td,
                        &to_td,
                        old_surface,
                        new_surface,
                        old_wt,
                        new_wt,
                        &shared_td,
                        &progress_td,
                    )
                })
                .await
                .map_err(|e| anyhow::anyhow!("TD analysis panicked: {}", e))?
                .context("TD analysis failed")?;

                // TD done — extract surfaces for inference phases
                let default_surface: Arc<ApiSurface<L::SymbolData>> =
                    Arc::new(ApiSurface::default());
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
                        let rename_phase = progress_rename.start_phase("Inferring rename patterns");
                        let result = Self::infer_rename_patterns(
                            lang_rename.as_ref(),
                            &changes_rename,
                            &old_surf_rename,
                            &new_surf_rename,
                            &llm_cmd_rename,
                            &from_rename,
                            &to_rename,
                            llm_timeout,
                        );
                        rename_phase.finish("Rename inference complete");
                        result
                    })
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(%e, "Rename inference task panicked");
                        shared_td_outer.degradation().record(
                            "LLM",
                            "Rename inference task panicked",
                            "Inferred rename patterns may be incomplete",
                        );
                        None
                    })
                } else {
                    None
                };
                Ok::<_, anyhow::Error>((td, old_surface, new_surface, inferred_rename_patterns))
            },
            // SD branch: source-level analysis (independent of TD)
            async move {
                let sd_phase = progress_sd.start_phase("[SD] Source-level analysis ...");
                // Clone shared for use after spawn_blocking
                let shared_sd_outer = shared_sd.clone();
                let result = tokio::task::spawn_blocking(move || {
                    // If a dep CSS repo is provided with a ref and build command,
                    // create a worktree, build it, and use the built path for CSS
                    // profile extraction. Otherwise fall back to the raw dir path.
                    // Create a worktree for the dep repo (e.g., CSS repo).
                     // Use `create_only` — the dep repo may not be the same
                     // language (no language-specific build tool detection).
                     // The caller-provided build command handles install + build.
                    let dep_worktree_guard = if let (Some(dep_dir), Some(dep_to)) =
                        (&dep_dir_sd, &dep_to_sd)
                    {
                        use semver_analyzer_ts::WorktreeGuard;
                        match WorktreeGuard::create_only(dep_dir, dep_to) {
                            Ok(guard) => {
                                // Run the user-provided build command in the worktree
                                if let Some(cmd) = &dep_build_cmd_sd {
                                    tracing::info!(
                                        command = %cmd,
                                        worktree = %guard.path().display(),
                                        "Running dep repo build command"
                                    );
                                    match std::process::Command::new("sh")
                                        .args(["-c", cmd])
                                        .current_dir(guard.path())
                                        .output()
                                    {
                                        Ok(output) if output.status.success() => {
                                            tracing::info!("Dep repo build succeeded");
                                        }
                                        Ok(output) => {
                                            let stderr = String::from_utf8_lossy(&output.stderr);
                                            let tail: String = stderr.lines().rev().take(10).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
                                            tracing::warn!(
                                                exit_code = ?output.status.code(),
                                                stderr = %tail,
                                                "Dep repo build failed"
                                            );
                                            shared_sd.degradation().record(
                                                "CSS",
                                                format!("Dep repo build command failed (exit {})", output.status.code().unwrap_or(-1)),
                                                "CSS class removal detection may be incomplete",
                                            );
                                        }
                                        Err(e) => {
                                            tracing::warn!(%e, "Failed to run dep repo build command");
                                            shared_sd.degradation().record(
                                                "CSS",
                                                format!("Failed to run dep repo build command: {}", e),
                                                "CSS class removal detection may be incomplete",
                                            );
                                        }
                                    }
                                }
                                tracing::info!(
                                    dep_repo = %dep_dir.display(),
                                    dep_ref = %dep_to,
                                    worktree = %guard.path().display(),
                                    "Created dep repo worktree for CSS extraction"
                                );
                                Some(guard)
                            }
                            Err(e) => {
                                tracing::warn!(%e, "Failed to create dep repo worktree, using raw dir");
                                shared_sd.degradation().record(
                                    "CSS",
                                    format!("Dep repo worktree creation failed: {}", e),
                                    "CSS profiles unavailable — some CSS-based rules may be missing",
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };

                    // Detect CSS component blocks removed between dep-repo versions.
                    // Compare component directory listings at dep_from vs dep_to.
                    // Must run before dep_dir_sd is consumed below.
                    let removed_css_blocks = if let (Some(dep_dir), Some(from), Some(to)) =
                        (&dep_dir_sd, &dep_from_sd, &dep_to_sd)
                    {
                        detect_removed_css_blocks(dep_dir, from, to)
                    } else {
                        Vec::new()
                    };

                    // Analyze CSS class inventories: extract full class lists from
                    // both versions and detect dead classes (prefix swap → non-existent).
                    // Builds a worktree for from_ref too so we get compiled CSS.
                    let css_inventory = if let (Some(dep_dir), Some(from), Some(to)) =
                        (&dep_dir_sd, &dep_from_sd, &dep_to_sd)
                    {
                        analyze_css_class_inventories(
                            dep_dir,
                            from,
                            to,
                            dep_worktree_guard.as_ref().map(|g| g.path()),
                            dep_build_cmd_sd.as_deref(),
                        )
                    } else {
                        CssInventoryResult {
                            dead_classes_after_swap: Vec::new(),
                            old_inventory: std::collections::HashSet::new(),
                            new_inventory: std::collections::HashSet::new(),
                        }
                    };

                    // Detect dep-repo packages (name → version) for dep-update
                    // rule generation. Reads the dep repo's package.json at to_ref.
                    // Must run before dep_dir_sd is consumed below.
                    let dep_repo_packages = if let (Some(dep_dir), Some(to_ref)) =
                        (&dep_dir_sd, &dep_to_sd)
                    {
                        detect_dep_repo_packages(dep_dir, to_ref)
                    } else {
                        std::collections::HashMap::new()
                    };

                    // Use worktree path if available, otherwise fall back to raw dir
                    let css_dir = dep_worktree_guard
                        .as_ref()
                        .map(|g| g.path().to_path_buf())
                        .or(dep_dir_sd);

                    // Receive worktree handles from TD pipeline.
                    // Blocks until TD creates each worktree (or TD drops
                    // senders on error). SD's early phases (A, A.5, B) run
                    // inside run_extended_analysis using read_git_file;
                    // only Phase B.5 (extends resolution) uses these paths.
                    let from_wt = from_wt_rx.recv().ok().flatten();
                    let to_wt = to_wt_rx.recv().ok().flatten();

                    let from_wt_path = from_wt.as_ref().map(|w| w.path().to_path_buf());
                    let to_wt_path = to_wt.as_ref().map(|w| w.path().to_path_buf());

                    if let (Some(from_path), Some(to_path)) =
                        (from_wt_path.as_ref(), to_wt_path.as_ref())
                    {
                        tracing::info!(
                            from = %from_path.display(),
                            to = %to_path.display(),
                            "Received worktree paths from TD for SD extends resolution"
                        );
                    }

                    let params = semver_analyzer_core::ExtendedAnalysisParams {
                        repo: repo_sd.clone(),
                        from_ref: from_sd.clone(),
                        to_ref: to_sd.clone(),
                        dep_dir: css_dir,
                        removed_dep_components: removed_css_blocks,
                        dep_repo_packages,
                        from_worktree_path: from_wt_path,
                        to_worktree_path: to_wt_path,
                        dead_css_classes_after_swap: css_inventory.dead_classes_after_swap,
                        old_css_class_inventory: css_inventory.old_inventory,
                        new_css_class_inventory: css_inventory.new_inventory,
                    };

                    // Keep worktree handles alive until analysis completes.
                    let result = lang_sd.run_extended_analysis(&params);
                    drop(from_wt);
                    drop(to_wt);
                    result
                })
                .await
                .map_err(|e| anyhow::anyhow!("SD task panicked: {}", e))?;

                match &result {
                    Ok(_) => {
                        sd_phase.finish("[SD] Source-level analysis complete");
                    }
                    Err(e) => {
                        warn!(%e, "SD pipeline failed");
                        shared_sd_outer.degradation().record(
                            "SD",
                            format!("Source-level analysis failed: {}", e),
                            "Composition trees and conformance rules are unavailable",
                        );
                        sd_phase.finish_failed("[SD] Source-level analysis failed");
                    }
                }

                result
            },
        );

        // ── Unwrap results ──────────────────────────────────────────
        let (td, old_surface, new_surface, inferred_rename_patterns) = td_inference_result?;

        let mut extensions = match ext_result {
            Ok(ext) => ext,
            Err(e) => {
                warn!(%e, "Extended analysis failed, continuing with empty results");
                shared.degradation().record(
                    "SD",
                    format!("Source-level analysis failed: {}", e),
                    "Composition trees and conformance rules are unavailable",
                );
                L::AnalysisExtensions::default()
            }
        };

        // ── Finalize extensions + summary logging ────────────────────
        // Delegate cross-pipeline processing (e.g., deprecated replacement
        // detection) to the Language impl, then log the summary.
        let structural_changes = self.lang.finalize_extensions(
            &mut extensions,
            td.structural_changes,
            repo,
            from_ref,
            to_ref,
        );

        for line in self.lang.extensions_log_summary(&extensions) {
            progress.println(&format!("  {}", line));
        }

        Ok(AnalysisResult {
            structural_changes,
            behavioral_changes: vec![], // No BU in v2
            manifest_changes: td.manifest_changes,
            llm_api_changes: vec![], // No LLM file analysis in v2
            old_surface,
            new_surface,
            inferred_rename_patterns,
            container_changes: vec![], // No BU container changes in v2
            extensions,
            degradation: shared.degradation_arc(),
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
    /// Worktree handles kept alive for sharing with SD pipeline.
    /// The BU pipeline ignores these (they drop after `run_td` returns).
    #[allow(dead_code)]
    old_worktree: Option<Arc<dyn semver_analyzer_core::traits::WorktreeAccess>>,
    #[allow(dead_code)]
    new_worktree: Option<Arc<dyn semver_analyzer_core::traits::WorktreeAccess>>,
}

impl<L: Language> Analyzer<L> {
    /// Run the full TD pipeline: extract both surfaces sequentially, diff,
    /// and analyze manifests. Used by the BU pipeline's `run()`.
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
        let (old_surface, old_worktree) = {
            let _extract_span = info_span!("extract_surface", git_ref = %from_ref).entered();
            let (surface, wt) = lang
                .extract_keeping_worktree(repo, from_ref, Some(shared.degradation()))
                .with_context(|| format!("Failed to extract API surface at ref {}", from_ref))?;
            (Arc::new(surface), wt)
        };
        let old_count = old_surface.symbols.len();
        phase.finish_with_detail(
            &format!("[TD] Extracted API surface at {}", from_ref),
            &format!("{} symbols", old_count),
        );
        info!(symbols = old_count, git_ref = %from_ref, "old surface extracted");
        shared.set_old_surface(old_surface.clone());

        // Step 2: Extract API surface at new ref
        let phase = progress.start_phase(&format!("[TD] Extracting API surface at {} ...", to_ref));
        let (new_surface, new_worktree) = {
            let _extract_span = info_span!("extract_surface", git_ref = %to_ref).entered();
            let (surface, wt) = lang
                .extract_keeping_worktree(repo, to_ref, Some(shared.degradation()))
                .with_context(|| format!("Failed to extract API surface at ref {}", to_ref))?;
            (Arc::new(surface), wt)
        };
        let new_count = new_surface.symbols.len();
        phase.finish_with_detail(
            &format!("[TD] Extracted API surface at {}", to_ref),
            &format!("{} symbols", new_count),
        );
        info!(symbols = new_count, git_ref = %to_ref, "new surface extracted");
        shared.set_new_surface(new_surface.clone());

        // Steps 3-4: diff + manifest
        Self::run_td_analyze(
            lang,
            repo,
            from_ref,
            to_ref,
            old_surface,
            new_surface,
            old_worktree,
            new_worktree,
            shared,
            progress,
        )
    }

    /// Post-extraction TD analysis: structural diff + manifest diff.
    ///
    /// Separated from `run_td` so that `run_v2` can extract both surfaces
    /// in parallel and then call this with the results.
    #[allow(clippy::too_many_arguments)]
    fn run_td_analyze(
        lang: &L,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
        old_surface: Arc<ApiSurface<L::SymbolData>>,
        new_surface: Arc<ApiSurface<L::SymbolData>>,
        old_worktree: Option<Arc<dyn semver_analyzer_core::traits::WorktreeAccess>>,
        new_worktree: Option<Arc<dyn semver_analyzer_core::traits::WorktreeAccess>>,
        shared: &SharedFindings<L>,
        progress: &ProgressReporter,
    ) -> Result<TdResult<L>> {
        let old_count = old_surface.symbols.len();
        let new_count = new_surface.symbols.len();

        // Structural diff (using language-specific semantics)
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

        // Manifest diff (language-specific)
        let manifest_phase = progress.start_phase("[TD] Diffing manifest files ...");
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

        let package_manifests = L::discover_package_manifests(repo, to_ref);
        if !package_manifests.is_empty() {
            tracing::info!(
                count = package_manifests.len(),
                "Diffing per-package manifest files"
            );
        }
        for (manifest_path, package_name) in &package_manifests {
            let old_content = read_git_file(repo, from_ref, manifest_path);
            let new_content = read_git_file(repo, to_ref, manifest_path);
            if let (Some(old_str), Some(new_str)) = (old_content, new_content) {
                let mut changes = L::diff_manifest_content(&old_str, &new_str);
                for change in &mut changes {
                    change.source_package = Some(package_name.clone());
                }
                manifest_changes.extend(changes);
            }
        }

        let manifest_breaking = manifest_changes.iter().filter(|c| c.is_breaking).count();
        if manifest_changes.is_empty() {
            manifest_phase.finish("[TD] No manifest changes detected");
        } else {
            manifest_phase.finish_with_detail(
                "[TD] Manifest diff complete",
                &format!(
                    "{} changes ({} breaking)",
                    manifest_changes.len(),
                    manifest_breaking
                ),
            );
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
            old_worktree,
            new_worktree,
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

            if func.old_body.is_none() || func.new_body.is_none() {
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
                if func.old_body.is_none() || func.new_body.is_none() {
                    continue;
                }
                if func.visibility != Visibility::Exported && func.visibility != Visibility::Public
                {
                    continue;
                }

                let results = body_analyzer.analyze_changed_body(
                    func.old_body.as_deref().unwrap_or(""),
                    func.new_body.as_deref().unwrap_or(""),
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
                if func.old_body.is_none() || func.new_body.is_none() {
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
    #[allow(clippy::too_many_arguments)]
    fn infer_rename_patterns(
        lang: &L,
        structural_changes: &[StructuralChange],
        old_surface: &ApiSurface<L::SymbolData>,
        new_surface: &ApiSurface<L::SymbolData>,
        llm_command: &str,
        from_ref: &str,
        to_ref: &str,
        llm_timeout: u64,
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
            let analyzer = LlmBehaviorAnalyzer::new(llm_command).with_timeout(llm_timeout);
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
            let analyzer = LlmBehaviorAnalyzer::new(llm_command).with_timeout(llm_timeout);
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
        llm_timeout: u64,
        progress: &ProgressReporter,
        categories: &[semver_analyzer_core::LlmCategoryDefinition],
    ) -> LlmPhaseStats {
        let _span = info_span!("bu_pipeline_phase2", file_count = files.len()).entered();
        let cmd = match llm_command {
            Some(c) => c.to_string(),
            None => return LlmPhaseStats::default(),
        };

        let total = files.len();
        let concurrency = LLM_CONCURRENCY;
        let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
        let llm_calls = Arc::new(AtomicUsize::new(0));
        let llm_breaks = Arc::new(AtomicUsize::new(0));
        let llm_failures = Arc::new(AtomicUsize::new(0));
        info!(total, concurrency, "starting LLM file analysis");
        let bar = progress.start_counted("[BU] LLM Analysis", total as u64);

        let mut handles = Vec::with_capacity(total);

        for task in files {
            let sem = semaphore.clone();
            let shared_ref = shared.clone();
            let api_entries = llm_api_entries.clone();
            let calls = llm_calls.clone();
            let breaks = llm_breaks.clone();
            let failures = llm_failures.clone();
            let cmd = cmd.clone();
            let file_path = task.file_path.clone();
            let diff_content = task.diff_content.clone();
            let functions = task.functions.clone();
            let test_diff = task.test_diff.clone();

            let cats = categories.to_vec();
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
                        let analyzer = LlmBehaviorAnalyzer::new(&cmd).with_timeout(llm_timeout);
                        let first = analyzer.analyze_file_diff(
                            &file_path,
                            &diff_content,
                            &functions,
                            test_diff.as_deref(),
                            &cats,
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
                                        &cats,
                                    )
                                    .map(|result| (file_path, result))
                            }
                        }
                    }
                })
                .await;

                calls.fetch_add(1, Ordering::Relaxed);

                match result {
                    Ok(Ok((file_path, (beh_changes, api_changes)))) => {
                        let beh_count = beh_changes.len();
                        let api_cnt = api_changes.len();

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
                                });
                            }
                        }

                        debug!(
                            file = %file_path,
                            behavioral = beh_count,
                            api = api_cnt,
                            "LLM analysis complete"
                        );
                    }
                    Ok(Err(e)) => {
                        error!(file = %file_path, %e, "LLM analysis failed after retry");
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        error!(%e, "LLM analysis panicked");
                        failures.fetch_add(1, Ordering::Relaxed);
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

        // Record LLM failure summary as degradation if any failed
        let failure_count = llm_failures.load(Ordering::Relaxed);
        if failure_count > 0 {
            shared.degradation().record(
                "LLM",
                format!("{} of {} file analyses failed", failure_count, total),
                "Some behavioral changes may be missing from the report",
            );
        }

        LlmPhaseStats {
            llm_calls: llm_calls.load(Ordering::Relaxed),
            llm_behavioral_breaks: llm_breaks.load(Ordering::Relaxed),
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
    semver_analyzer_core::git::git_diff_file(repo, from_ref, to_ref, file_path)
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
    semver_analyzer_core::git::read_git_file(repo, git_ref, file_path)
}

// ── Component Hierarchy Inference ────────────────────────────────────────
//
// Infers the component parent-child hierarchy for both versions by giving
// the LLM each component family's source code, then diffs the hierarchies
// to produce HierarchyDelta entries.

impl<L: Language> Analyzer<L> {
    /// Infer hierarchies for both versions and compute deltas.
    #[allow(clippy::too_many_arguments)]
    async fn infer_and_diff_hierarchies(
        lang: &L,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
        llm_command: &str,
        structural_changes: &[StructuralChange],
        old_surface: &ApiSurface<L::SymbolData>,
        new_surface: &ApiSurface<L::SymbolData>,
        llm_timeout: u64,
        progress: &ProgressReporter,
    ) -> (
        Vec<HierarchyDelta>,
        HashMap<String, HashMap<String, Vec<ExpectedChild>>>,
    ) {
        let _span = info_span!("hierarchy_inference").entered();

        let hierarchy = match lang.hierarchy() {
            Some(h) => h,
            None => return (Vec::new(), HashMap::new()),
        };

        // Find families to analyze: group symbols by family, filter to those with
        // breaking changes and enough components.
        let families = {
            // O(1) lookup indexes to avoid linear scans per structural change
            let new_by_qname: HashMap<&str, &Symbol<L::SymbolData>> = new_surface
                .symbols
                .iter()
                .map(|s| (s.qualified_name.as_str(), s))
                .collect();
            let old_by_qname: HashMap<&str, &Symbol<L::SymbolData>> = old_surface
                .symbols
                .iter()
                .map(|s| (s.qualified_name.as_str(), s))
                .collect();

            let changed_dirs: HashSet<String> = structural_changes
                .iter()
                .filter(|c| c.is_breaking)
                .filter_map(|c| {
                    let sym = new_by_qname
                        .get(c.qualified_name.as_str())
                        .or_else(|| old_by_qname.get(c.qualified_name.as_str()));
                    sym.and_then(|s| hierarchy.family_name_from_symbols(&[s]))
                })
                .collect();

            // Group new surface symbols by family
            let mut family_components: HashMap<String, HashSet<String>> = HashMap::new();
            for sym in &new_surface.symbols {
                if !hierarchy.is_hierarchy_candidate(sym) {
                    continue;
                }
                if let Some(family_name) = hierarchy.family_name_from_symbols(&[sym]) {
                    family_components
                        .entry(family_name)
                        .or_default()
                        .insert(sym.name.clone());
                }
            }

            let min_components = hierarchy.min_components_for_hierarchy();
            let result: Vec<String> = family_components
                .into_iter()
                .filter(|(dir, components)| {
                    components.len() >= min_components && changed_dirs.contains(dir)
                })
                .map(|(dir, _)| dir)
                .collect();

            info!(count = result.len(), "qualifying hierarchy families");
            result
        };

        if families.is_empty() {
            return (Vec::new(), HashMap::new());
        }

        // ── Phase 0: Deterministic hierarchy ─────────────────────────
        //
        // Compute hierarchy without LLM using three signals:
        // 1. Prop absorption: removed props that moved to new child components
        // 2. Cross-family extends: components whose props extend another family
        // 3. Internal rendering: what JSX components are rendered internally
        //
        // The old surface uses rendered_components only (no structural changes).
        // The new surface uses all three signals.
        let deterministic_old = hierarchy.compute_deterministic_hierarchy(old_surface, &[]);
        let deterministic_new =
            hierarchy.compute_deterministic_hierarchy(new_surface, structural_changes);

        let det_old_count = deterministic_old.len();
        let det_new_count = deterministic_new.len();
        if det_old_count > 0 || det_new_count > 0 {
            info!(
                old_families = det_old_count,
                new_families = det_new_count,
                "deterministic hierarchy computed from rendered_components"
            );
            for (family, components) in &deterministic_new {
                let children_count: usize = components.values().map(|v| v.len()).sum();
                debug!(
                    family = %family,
                    components = components.len(),
                    children = children_count,
                    "deterministic hierarchy"
                );
            }
        }

        // Families that have deterministic hierarchy data for the new version
        // can skip LLM inference entirely.
        let families_needing_llm: Vec<String> = families
            .iter()
            .filter(|f| !deterministic_new.contains_key(*f))
            .cloned()
            .collect();

        let families_with_det: usize = families.len() - families_needing_llm.len();
        if families_with_det > 0 {
            info!(
                deterministic = families_with_det,
                llm = families_needing_llm.len(),
                "hierarchy inference split"
            );
        }

        // Detect cross-family relationships and prepare related signatures
        let context_rels = hierarchy.cross_family_relationships(repo, to_ref);

        // Build lookup: consumer_family → [(provider_family, [relationship_names])]
        let mut context_providers: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
        for (consumer, provider, rel_name) in &context_rels {
            context_providers
                .entry(consumer.clone())
                .or_default()
                .entry(provider.clone())
                .or_default()
                .push(rel_name.clone());
        }

        // Pre-read related component signatures for each consumer family
        let mut related_signatures: HashMap<String, String> = HashMap::new();
        for (consumer, providers) in &context_providers {
            let mut combined = String::new();
            for (provider, rel_names) in providers {
                if let Some(sigs) =
                    hierarchy.related_family_content(repo, to_ref, provider, rel_names)
                {
                    combined.push_str(&sigs);
                }
            }
            if !combined.is_empty() {
                related_signatures.insert(consumer.clone(), combined);
            }
        }

        info!(
            families = families.len(),
            llm_families = families_needing_llm.len(),
            cross_family = related_signatures.len(),
            "analyzing component hierarchy for both versions"
        );

        let bar =
            progress.start_counted("[Hierarchy] Inference", families_needing_llm.len() as u64);

        // Run LLM calls concurrently — only for families without deterministic data
        let semaphore = Arc::new(tokio::sync::Semaphore::new(LLM_CONCURRENCY));

        // For each family needing LLM, infer hierarchy for BOTH old and new refs
        let mut handles = Vec::new();

        for family in &families_needing_llm {
            // Pre-compute file paths before spawning — hierarchy is a reference
            // that can't cross the 'static boundary of tokio::spawn.
            let old_paths = hierarchy.family_source_paths(repo, from_ref, family);
            let new_paths = hierarchy.family_source_paths(repo, to_ref, family);

            let sem = semaphore.clone();
            let repo = repo.to_path_buf();
            let from_ref = from_ref.to_string();
            let to_ref = to_ref.to_string();
            let llm_cmd = llm_command.to_string();
            let family = family.clone();
            let related = related_signatures.get(&family).cloned();
            let has_context = related.is_some();

            let handle = tokio::spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore closed");

                debug!(
                    family = %family,
                    cross_family = has_context,
                    "inferring hierarchy"
                );

                let old_content = {
                    if old_paths.is_empty() {
                        None
                    } else {
                        let mut content = String::new();
                        for file_path in &old_paths {
                            if let Some(file_content) = read_git_file(&repo, &from_ref, file_path) {
                                content.push_str(&format!("\n--- File: {} ---\n", file_path));
                                content.push_str(&file_content);
                                content.push('\n');
                            }
                        }
                        if content.is_empty() {
                            None
                        } else {
                            Some(content)
                        }
                    }
                };

                let new_content = {
                    if new_paths.is_empty() {
                        None
                    } else {
                        let mut content = String::new();
                        for file_path in &new_paths {
                            if let Some(file_content) = read_git_file(&repo, &to_ref, file_path) {
                                content.push_str(&format!("\n--- File: {} ---\n", file_path));
                                content.push_str(&file_content);
                                content.push('\n');
                            }
                        }
                        if content.is_empty() {
                            None
                        } else {
                            Some(content)
                        }
                    }
                };

                // Infer old hierarchy (if family existed in old version)
                // Old version doesn't get related signatures — conformance is
                // about the new version's expected structure.
                let old_hierarchy = if let Some(content) = old_content {
                    tokio::task::spawn_blocking({
                        let analyzer_cmd = llm_cmd.clone();
                        let family_name = family.clone();
                        move || {
                            let _span = info_span!("llm_hierarchy", %family_name, version = "old")
                                .entered();
                            let a =
                                LlmBehaviorAnalyzer::new(&analyzer_cmd).with_timeout(llm_timeout);
                            let prompt =
                                semver_analyzer_ts::llm_prompts::build_hierarchy_inference_prompt(
                                    &family_name,
                                    &content,
                                    None,
                                );
                            match a.infer_hierarchy_from_prompt(&prompt) {
                                Ok(h) => Some(h),
                                Err(e) => {
                                    warn!(
                                        family = %family_name,
                                        %e,
                                        "old hierarchy inference failed"
                                    );
                                    None
                                }
                            }
                        }
                    })
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default()
                } else {
                    HashMap::new()
                };

                // Infer new hierarchy — include related signatures if available.
                // Retry once on failure since the LLM sometimes returns prose
                // instead of the expected JSON block.
                let new_hierarchy = if let Some(content) = new_content {
                    tokio::task::spawn_blocking({
                        let family_name = family.clone();
                        let related_sigs = related;
                        move || {
                            let _span = info_span!("llm_hierarchy", %family_name, version = "new")
                                .entered();
                            let analyzer =
                                LlmBehaviorAnalyzer::new(&llm_cmd).with_timeout(llm_timeout);
                            let prompt =
                                semver_analyzer_ts::llm_prompts::build_hierarchy_inference_prompt(
                                    &family_name,
                                    &content,
                                    related_sigs.as_deref(),
                                );
                            match analyzer.infer_hierarchy_from_prompt(&prompt) {
                                Ok(h) => Some(h),
                                Err(e) => {
                                    warn!(
                                        family = %family_name,
                                        %e,
                                        "new hierarchy failed, retrying"
                                    );
                                    // Retry once
                                    match analyzer.infer_hierarchy_from_prompt(&prompt) {
                                        Ok(h) => Some(h),
                                        Err(e2) => {
                                            error!(
                                                family = %family_name,
                                                %e2,
                                                "new hierarchy retry also failed"
                                            );
                                            None
                                        }
                                    }
                                }
                            }
                        }
                    })
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default()
                } else {
                    HashMap::new()
                };

                (family, old_hierarchy, new_hierarchy)
            });

            handles.push(handle);
        }

        // Collect LLM results
        let mut all_old: HashMap<String, HashMap<String, Vec<ExpectedChild>>> = HashMap::new();
        let mut all_new: HashMap<String, HashMap<String, Vec<ExpectedChild>>> = HashMap::new();

        for handle in handles {
            if let Ok((family, old_h, new_h)) = handle.await {
                let old_count: usize = old_h.values().map(|v| v.len()).sum();
                let new_count: usize = new_h.values().map(|v| v.len()).sum();
                debug!(
                    family = %family,
                    old_children = old_count,
                    new_children = new_count,
                    source = "llm",
                    "hierarchy inferred"
                );
                all_old.insert(family.clone(), old_h);
                all_new.insert(family, new_h);
            }
            bar.inc();
        }
        bar.finish();

        // Merge deterministic hierarchy results (from rendered_components).
        // These cover families that were skipped for LLM inference.
        for (family, components) in deterministic_old {
            all_old.entry(family).or_insert(components);
        }
        for (family, components) in deterministic_new {
            all_new.entry(family).or_insert(components);
        }

        // Compute deltas
        let mut deltas = Vec::new();

        for (family, new_hierarchy) in &all_new {
            let old_hierarchy = all_old.get(family).cloned().unwrap_or_default();

            for (component, new_children) in new_hierarchy {
                let old_children = old_hierarchy.get(component).cloned().unwrap_or_default();

                let old_child_names: HashSet<String> =
                    old_children.iter().map(|c| c.name.clone()).collect();
                let new_child_names: HashSet<String> =
                    new_children.iter().map(|c| c.name.clone()).collect();

                let added: Vec<ExpectedChild> = new_children
                    .iter()
                    .filter(|c| !old_child_names.contains(&c.name))
                    .map(|c| ExpectedChild {
                        name: c.name.clone(),
                        required: c.required,
                        mechanism: c.mechanism.clone(),
                        prop_name: c.prop_name.clone(),
                    })
                    .collect();

                let removed: Vec<String> = old_children
                    .iter()
                    .filter(|c| !new_child_names.contains(&c.name))
                    .map(|c| c.name.clone())
                    .collect();

                if !added.is_empty() || !removed.is_empty() {
                    deltas.push(HierarchyDelta {
                        component: component.clone(),
                        added_children: added,
                        removed_children: removed,
                        migrated_members: Vec::new(), // Populated during report building
                        source_package: None,
                        migration_target: None,
                    });
                }
            }
        }

        info!(
            deltas = deltas.len(),
            families = all_new.len(),
            "hierarchy deltas detected"
        );

        (deltas, all_new)
    }
} // end impl Analyzer<L> (private methods, part 2)

/// Detect CSS component blocks removed between two refs of a CSS repo.
///
/// Lists the `src/patternfly/components/` directories at each ref using
/// `git ls-tree`, then returns the directory names that exist in `from_ref`
/// but not in `to_ref`. These map to CSS BEM block names (e.g., "Select" →
/// `pf-v5-c-select`).
fn detect_removed_css_blocks(dep_dir: &Path, from_ref: &str, to_ref: &str) -> Vec<String> {
    let list_dirs = |git_ref: &str| -> std::collections::HashSet<String> {
        let output = std::process::Command::new("git")
            .args([
                "ls-tree",
                "--name-only",
                git_ref,
                "src/patternfly/components/",
            ])
            .current_dir(dep_dir)
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                stdout
                    .lines()
                    .filter_map(|line| {
                        let name = line
                            .trim()
                            .strip_prefix("src/patternfly/components/")
                            .unwrap_or(line.trim());
                        // Skip SCSS index files
                        if name.ends_with(".scss") || name.is_empty() {
                            None
                        } else {
                            Some(name.to_string())
                        }
                    })
                    .collect()
            }
            _ => std::collections::HashSet::new(),
        }
    };

    let old_dirs = list_dirs(from_ref);
    let new_dirs = list_dirs(to_ref);

    let mut removed: Vec<String> = old_dirs.difference(&new_dirs).cloned().collect();
    removed.sort();

    if !removed.is_empty() {
        tracing::info!(
            count = removed.len(),
            blocks = ?removed,
            "Detected removed CSS component blocks between dep-repo versions"
        );
    }

    // Convert PascalCase directory names to kebab-case BEM block names
    // e.g., "Select" → "select", "AppLauncher" → "app-launcher"
    removed
        .into_iter()
        .map(|name| pascal_to_kebab(&name))
        .collect()
}

/// Result of CSS class inventory analysis between two dep-repo versions.
struct CssInventoryResult {
    /// CSS classes where a prefix swap produces a non-existent class.
    dead_classes_after_swap: Vec<(String, String)>,
    /// Full CSS class inventory from the old version (compiled CSS).
    old_inventory: std::collections::HashSet<String>,
    /// Full CSS class inventory from the new version (compiled CSS).
    new_inventory: std::collections::HashSet<String>,
}

/// Extract CSS class inventories from both dep-repo versions and detect dead classes.
///
/// Builds a worktree for `from_ref` (if a build command is provided) to get
/// compiled CSS — the dep repo typically has no pre-built CSS checked into git.
/// For `to_ref`, uses the already-built worktree if available.
///
/// Returns the full inventories for both versions plus the dead class list.
/// The inventories are used to generate enumerated per-class rules instead
/// of a single catch-all prefix swap rule.
fn analyze_css_class_inventories(
    dep_dir: &Path,
    from_ref: &str,
    to_ref: &str,
    to_worktree: Option<&Path>,
    build_command: Option<&str>,
) -> CssInventoryResult {
    use semver_analyzer_ts::css_profile::{
        extract_css_class_inventory, extract_css_class_inventory_from_dir,
    };

    let empty_result = CssInventoryResult {
        dead_classes_after_swap: Vec::new(),
        old_inventory: std::collections::HashSet::new(),
        new_inventory: std::collections::HashSet::new(),
    };

    // Extract old class inventory.
    // Build a worktree for from_ref so we get compiled CSS (SCSS source
    // in git doesn't contain usable class selectors).
    let from_worktree = {
        use semver_analyzer_ts::WorktreeGuard;
        match WorktreeGuard::create_only(dep_dir, from_ref) {
            Ok(guard) => {
                // Run build command to compile SCSS → CSS
                if let Some(cmd) = build_command {
                    tracing::info!(
                        command = %cmd,
                        worktree = %guard.path().display(),
                        ref_name = %from_ref,
                        "Building dep repo worktree for old CSS class inventory"
                    );
                    match std::process::Command::new("sh")
                        .args(["-c", cmd])
                        .current_dir(guard.path())
                        .output()
                    {
                        Ok(output) if output.status.success() => {
                            tracing::info!(ref_name = %from_ref, "Dep repo build succeeded for old ref");
                        }
                        Ok(output) => {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            let tail: String = stderr
                                .lines()
                                .rev()
                                .take(10)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect::<Vec<_>>()
                                .join("\n");
                            tracing::warn!(
                                ref_name = %from_ref,
                                exit_code = ?output.status.code(),
                                stderr = %tail,
                                "Dep repo build failed for old ref"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(%e, ref_name = %from_ref, "Failed to run dep repo build for old ref");
                        }
                    }
                }
                Some(guard)
            }
            Err(e) => {
                tracing::warn!(%e, "Failed to create dep repo worktree for old ref");
                None
            }
        }
    };

    // Extract old classes — prefer built worktree, fall back to git
    let old_classes = if let Some(ref guard) = from_worktree {
        match extract_css_class_inventory_from_dir(guard.path()) {
            Ok(classes) if !classes.is_empty() => {
                tracing::info!(
                    count = classes.len(),
                    "Old CSS class inventory extracted from built worktree"
                );
                classes
            }
            Ok(_) => {
                tracing::warn!("Old CSS worktree produced empty inventory, falling back to git");
                extract_css_class_inventory(dep_dir, from_ref).unwrap_or_default()
            }
            Err(e) => {
                tracing::warn!(%e, "Failed to extract old CSS from worktree, falling back to git");
                extract_css_class_inventory(dep_dir, from_ref).unwrap_or_default()
            }
        }
    } else {
        match extract_css_class_inventory(dep_dir, from_ref) {
            Ok(classes) => {
                tracing::info!(
                    count = classes.len(),
                    "Old CSS class inventory extracted via git"
                );
                classes
            }
            Err(e) => {
                tracing::warn!(%e, "Failed to extract old CSS class inventory");
                return empty_result;
            }
        }
    };

    // Extract new class inventory from the built worktree (preferred) or git
    let new_classes = if let Some(wt_path) = to_worktree {
        match extract_css_class_inventory_from_dir(wt_path) {
            Ok(classes) => {
                tracing::info!(
                    count = classes.len(),
                    "New CSS class inventory extracted from worktree"
                );
                classes
            }
            Err(e) => {
                tracing::warn!(%e, "Failed to extract new CSS from worktree, falling back to git");
                match extract_css_class_inventory(dep_dir, to_ref) {
                    Ok(classes) => classes,
                    Err(e2) => {
                        tracing::warn!(%e2, "Failed to extract new CSS class inventory");
                        return CssInventoryResult {
                            dead_classes_after_swap: Vec::new(),
                            old_inventory: old_classes,
                            new_inventory: std::collections::HashSet::new(),
                        };
                    }
                }
            }
        }
    } else {
        match extract_css_class_inventory(dep_dir, to_ref) {
            Ok(classes) => classes,
            Err(e) => {
                tracing::warn!(%e, "Failed to extract new CSS class inventory");
                return CssInventoryResult {
                    dead_classes_after_swap: Vec::new(),
                    old_inventory: old_classes,
                    new_inventory: std::collections::HashSet::new(),
                };
            }
        }
    };

    // Auto-detect the prefix swap from the class inventories.
    let old_prefix = detect_version_prefix(&old_classes);
    let new_prefix = detect_version_prefix(&new_classes);

    let dead_classes_after_swap = match (old_prefix, new_prefix) {
        (Some(ref old_pfx), Some(ref new_pfx)) if old_pfx != new_pfx => {
            tracing::info!(
                old_prefix = %old_pfx,
                new_prefix = %new_pfx,
                old_count = old_classes.len(),
                new_count = new_classes.len(),
                "Detected CSS class prefix change for dead-class analysis"
            );

            let mut dead: Vec<(String, String)> = old_classes
                .iter()
                .filter(|cls| cls.starts_with(old_pfx))
                .filter_map(|old_class| {
                    let swapped = format!("{}{}", new_pfx, &old_class[old_pfx.len()..]);
                    if !new_classes.contains(&swapped) {
                        Some((old_class.clone(), swapped))
                    } else {
                        None
                    }
                })
                .collect();
            dead.sort();

            if !dead.is_empty() {
                tracing::info!(
                    count = dead.len(),
                    "Detected CSS classes where prefix swap produces non-existent class"
                );
            }
            dead
        }
        _ => {
            tracing::debug!("No version prefix change detected, skipping dead-class detection");
            Vec::new()
        }
    };

    // Drop the from_worktree guard before returning (cleanup)
    drop(from_worktree);

    CssInventoryResult {
        dead_classes_after_swap,
        old_inventory: old_classes,
        new_inventory: new_classes,
    }
}

/// Detect the most common version prefix from a set of CSS class names.
///
/// Looks for patterns like `pf-v5-` or `pf-v6-` and returns the most
/// frequent one.
fn detect_version_prefix(classes: &std::collections::HashSet<String>) -> Option<String> {
    static VER_PREFIX_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^(pf-v\d+-)").unwrap());

    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for cls in classes {
        if let Some(caps) = VER_PREFIX_RE.captures(cls) {
            *counts.entry(caps[1].to_string()).or_default() += 1;
        }
    }

    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(prefix, _)| prefix)
}

/// Convert PascalCase to kebab-case.
/// e.g., "AppLauncher" → "app-launcher", "Select" → "select"
fn pascal_to_kebab(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('-');
            }
            result.push(c.to_lowercase().next().unwrap());
        } else {
            result.push(c);
        }
    }
    result
}

/// Detect dependency repo packages (name → version) at a given git ref.
///
/// Reads `package.json` from the dep repo at `to_ref` using `git show`.
/// Returns the package name and version as a single-entry map, or empty
/// if the file doesn't exist or can't be parsed.
fn detect_dep_repo_packages(
    dep_dir: &Path,
    to_ref: &str,
) -> std::collections::HashMap<String, String> {
    let output = std::process::Command::new("git")
        .args(["show", &format!("{}:package.json", to_ref)])
        .current_dir(dep_dir)
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let content = String::from_utf8_lossy(&out.stdout);
            // Parse name and version from package.json
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                let name = json["name"].as_str().unwrap_or_default();
                let mut version = json["version"].as_str().unwrap_or_default().to_string();

                // Many repos use placeholder versions like "0.0.0-development"
                // in source; the real version is set during npm publish. Fall
                // back to deriving the version from the git tag (e.g.,
                // "v6.4.0" → "6.4.0").
                if version.starts_with("0.0.0") {
                    let tag_version = to_ref.trim_start_matches('v');
                    if !tag_version.is_empty()
                        && tag_version
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_digit())
                    {
                        tracing::info!(
                            package = %name,
                            placeholder = %version,
                            derived = %tag_version,
                            "Package.json has placeholder version, using git tag"
                        );
                        version = tag_version.to_string();
                    }
                }

                if !name.is_empty() && !version.is_empty() {
                    tracing::info!(
                        package = %name,
                        version = %version,
                        "Detected dep-repo package"
                    );
                    let mut map = std::collections::HashMap::new();
                    map.insert(name.to_string(), version);
                    return map;
                }
            }
            std::collections::HashMap::new()
        }
        _ => {
            tracing::trace!("No package.json found in dep repo at {}", to_ref);
            std::collections::HashMap::new()
        }
    }
}
