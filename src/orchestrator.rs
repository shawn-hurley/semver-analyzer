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
    BehavioralChange, ChangeSubject, ChangedFunction, ContainerChange, DeprecatedReplacement,
    EvidenceType, ExpectedChild, HierarchyDelta, InferenceMetadata, InferredConstantPattern,
    InferredInterfaceMapping, InferredRenamePatterns, Language, LlmApiChange, ManifestChange,
    SdPipelineResult, SharedFindings, SourceLevelCategory, StructuralChange, StructuralChangeType,
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
                let default_surface = Arc::new(ApiSurface::default());
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
                let mut container_changes: Vec<(String, Vec<ContainerChange>)> = vec![];

                if !no_llm && !phase1.files_for_llm.is_empty() {
                    let (stats, comp) = Self::run_bu_phase2_llm(
                        phase1.llm_command.as_deref(),
                        &phase1.files_for_llm,
                        &shared_bu_phase2,
                        &llm_api_entries_bu,
                        llm_timeout,
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
            hierarchy_deltas,
            new_hierarchies,
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

        Ok(AnalysisResult {
            structural_changes: td.structural_changes,
            behavioral_changes,
            manifest_changes: td.manifest_changes,
            llm_api_changes,
            old_surface,
            new_surface,
            inferred_rename_patterns,
            container_changes,
            hierarchy_deltas,
            new_hierarchies,
            sd_result: None,
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
        dep_from: Option<&str>,
        dep_to: Option<&str>,
        dep_build_command: Option<&str>,
        llm_timeout: u64,
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
        let dep_from_sd = dep_from.map(|s| s.to_string());
        let dep_to_sd = dep_to.map(|s| s.to_string());
        let dep_build_cmd_sd = dep_build_command.map(|s| s.to_string());
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

        // ── Run TD + SD concurrently ────────────────────────────────
        let (td_inference_result, sd_result) = tokio::join!(
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
                    .unwrap_or(None)
                } else {
                    None
                };
                let hierarchy_deltas = Vec::new();
                let new_hierarchies = HashMap::new();

                Ok::<_, anyhow::Error>((
                    td,
                    old_surface,
                    new_surface,
                    inferred_rename_patterns,
                    hierarchy_deltas,
                    new_hierarchies,
                ))
            },
            // SD branch: source-level analysis (independent of TD)
            async move {
                let sd_phase = progress_sd.start_phase("[SD] Source-level analysis ...");
                let result = tokio::task::spawn_blocking(move || {
                    // If a dep CSS repo is provided with a ref and build command,
                    // create a worktree, build it, and use the built path for CSS
                    // profile extraction. Otherwise fall back to the raw dir path.
                    // Create a worktree for the dep repo (e.g., CSS repo).
                    // Use `create_only` — the dep repo may not be a TypeScript
                    // project (no tsconfig.json, no package manager detection).
                    // The caller-provided build command handles install + build.
                    let dep_worktree_guard = if let (Some(dep_dir), Some(dep_to)) =
                        (&dep_css_dir_sd, &dep_to_sd)
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
                                        }
                                        Err(e) => {
                                            tracing::warn!(%e, "Failed to run dep repo build command");
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
                                None
                            }
                        }
                    } else {
                        None
                    };

                    // Detect CSS component blocks removed between dep-repo versions.
                    // Compare component directory listings at dep_from vs dep_to.
                    // Must run before dep_css_dir_sd is consumed below.
                    let removed_css_blocks = if let (Some(dep_dir), Some(from), Some(to)) =
                        (&dep_css_dir_sd, &dep_from_sd, &dep_to_sd)
                    {
                        detect_removed_css_blocks(dep_dir, from, to)
                    } else {
                        Vec::new()
                    };

                    // Use worktree path if available, otherwise fall back to raw dir
                    let css_dir = dep_worktree_guard
                        .as_ref()
                        .map(|g| g.path().to_path_buf())
                        .or(dep_css_dir_sd);

                    let mut sd_result = lang_sd.run_source_diff(
                        &repo_sd,
                        &from_sd,
                        &to_sd,
                        css_dir.as_deref(),
                    );

                    // Attach the removed CSS blocks to the SD result
                    if let Ok(ref mut r) = sd_result {
                        r.removed_css_blocks = removed_css_blocks;
                    }

                    sd_result
                })
                .await
                .map_err(|e| anyhow::anyhow!("SD task panicked: {}", e))?;

                match &result {
                    Ok(r) => {
                        sd_phase.finish_with_detail(
                            "[SD] Source-level analysis complete",
                            &format!(
                                "{} changes, {} trees, {} conformance",
                                r.source_level_changes.len(),
                                r.composition_trees.len(),
                                r.conformance_checks.len(),
                            ),
                        );
                    }
                    Err(e) => {
                        warn!(%e, "SD pipeline failed");
                        sd_phase.finish("[SD] Source-level analysis failed");
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
            hierarchy_deltas,
            new_hierarchies,
        ) = td_inference_result?;

        let mut sd = match sd_result {
            Ok(sd) => sd,
            Err(e) => {
                warn!(%e, "SD pipeline failed, continuing with empty results");
                semver_analyzer_core::SdPipelineResult::default()
            }
        };

        // Capture dependency repo package info (e.g., @patternfly/patternfly CSS package)
        // so rule generation can create dep-update rules for packages outside the main monorepo.
        if let Some(dep_dir) = dep_css_dir {
            let dep_pkg_json = dep_dir.join("package.json");
            if dep_pkg_json.exists() {
                if let Ok(content) = std::fs::read_to_string(&dep_pkg_json) {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let (Some(name), Some(version)) = (
                            parsed.get("name").and_then(|v| v.as_str()),
                            parsed.get("version").and_then(|v| v.as_str()),
                        ) {
                            // Some repos use "0.0.0-development" in source and set
                            // the real version during CI. Fall back to the git tag
                            // (e.g., "v6.4.0" → "6.4.0") if the version looks like
                            // a placeholder.
                            let effective_version = if version.starts_with("0.0.0")
                                || version == "0.0.0-development"
                            {
                                // Use `git tag -l` with version sort to find the
                                // latest release tag (e.g., "v6.4.0"), skipping
                                // prerelease tags like "prerelease-v6.4.0-...".
                                let tag_version = std::process::Command::new("git")
                                    .args(["tag", "-l", "v[0-9]*", "--sort=-v:refname"])
                                    .current_dir(dep_dir)
                                    .output()
                                    .ok()
                                    .and_then(|o| {
                                        if o.status.success() {
                                            let output = String::from_utf8_lossy(&o.stdout);
                                            output
                                                .lines()
                                                .next()
                                                .map(|tag| {
                                                    tag.trim().trim_start_matches('v').to_string()
                                                })
                                                .filter(|t| !t.is_empty())
                                        } else {
                                            None
                                        }
                                    });
                                tag_version.unwrap_or_else(|| version.to_string())
                            } else {
                                version.to_string()
                            };
                            info!(name, version = %effective_version, "Captured dep-repo package info");
                            sd.dep_repo_packages
                                .insert(name.to_string(), effective_version);
                        }
                    }
                }
            }
        }

        // ── Summary logging ─────────────────────────────────────────
        progress.println(&format!(
            "  [SD] {} source-level changes, {} composition trees, {} conformance checks",
            sd.source_level_changes.len(),
            sd.composition_trees.len(),
            sd.conformance_checks.len(),
        ));
        if !sd.composition_changes.is_empty() {
            progress.println(&format!(
                "  [SD] {} composition changes detected",
                sd.composition_changes.len(),
            ));
        }

        info!(
            source_level_changes = sd.source_level_changes.len(),
            composition_trees = sd.composition_trees.len(),
            composition_changes = sd.composition_changes.len(),
            conformance_checks = sd.conformance_checks.len(),
            "SD pipeline summary"
        );

        // ── Deprecated replacement detection via rendering swaps ────
        // For each component relocated to /deprecated/, check if other
        // components in the codebase switched from rendering the old
        // component to rendering a new one (e.g., ToolbarFilter stopped
        // rendering Chip and started rendering Label). If so, record
        // the replacement relationship for downstream report/rule generation.
        let deprecated_replacements = detect_deprecated_replacements(&td.structural_changes, &sd);
        if !deprecated_replacements.is_empty() {
            for dr in &deprecated_replacements {
                info!(
                    old = %dr.old_component,
                    new = %dr.new_component,
                    evidence = ?dr.evidence_hosts,
                    "Deprecated replacement detected via rendering swap"
                );
            }
            progress.println(&format!(
                "  [SD] {} deprecated replacements detected via rendering swaps",
                deprecated_replacements.len(),
            ));
            sd.deprecated_replacements = deprecated_replacements;
        }

        // Transform structural changes: for components with a deprecated
        // replacement, convert the relocation entry into a TypeChanged
        // entry pointing to the replacement, and suppress the redundant
        // signature-changed entry (base class change).
        let structural_changes =
            apply_deprecated_replacements(td.structural_changes, &sd.deprecated_replacements);

        Ok(AnalysisResult {
            structural_changes,
            behavioral_changes: vec![], // No BU in v2
            manifest_changes: td.manifest_changes,
            llm_api_changes: vec![], // No LLM file analysis in v2
            old_surface,
            new_surface,
            inferred_rename_patterns,
            container_changes: vec![], // No BU container changes in v2
            hierarchy_deltas,
            new_hierarchies,
            sd_result: Some(sd),
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
    #[allow(clippy::too_many_arguments)]
    fn infer_rename_patterns(
        lang: &L,
        structural_changes: &[StructuralChange],
        old_surface: &ApiSurface,
        new_surface: &ApiSurface,
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
        let old_by_qname: HashMap<&str, &Symbol> = old_surface
            .symbols
            .iter()
            .map(|s| (s.qualified_name.as_str(), s))
            .collect();
        let new_by_qname: HashMap<&str, &Symbol> = new_surface
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
                        let analyzer = LlmBehaviorAnalyzer::new(&cmd).with_timeout(llm_timeout);
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
        old_surface: &ApiSurface,
        new_surface: &ApiSurface,
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
            let new_by_qname: HashMap<&str, &Symbol> = new_surface
                .symbols
                .iter()
                .map(|s| (s.qualified_name.as_str(), s))
                .collect();
            let old_by_qname: HashMap<&str, &Symbol> = old_surface
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
                            match a.infer_component_hierarchy(&family_name, &content, None) {
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
                            match analyzer.infer_component_hierarchy(
                                &family_name,
                                &content,
                                related_sigs.as_deref(),
                            ) {
                                Ok(h) => Some(h),
                                Err(e) => {
                                    warn!(
                                        family = %family_name,
                                        %e,
                                        "new hierarchy failed, retrying"
                                    );
                                    // Retry once
                                    match analyzer.infer_component_hierarchy(
                                        &family_name,
                                        &content,
                                        related_sigs.as_deref(),
                                    ) {
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

// ── Deprecated replacement detection ────────────────────────────────────

/// Detect deprecated components that have differently-named replacements
/// by analyzing rendering swaps in the SD source-level changes.
///
/// For each component relocated to `/deprecated/`, check if other host
/// components stopped rendering it and started rendering something new.
/// When multiple hosts agree on the replacement (or a single host shows
/// a clear 1:1 swap), record the replacement relationship.
///
/// Example: ToolbarFilter and MultiTypeaheadSelect both stopped rendering
/// `Chip` and started rendering `Label` → `Chip` is replaced by `Label`.
fn detect_deprecated_replacements(
    structural_changes: &[StructuralChange],
    sd: &SdPipelineResult,
) -> Vec<DeprecatedReplacement> {
    // Step 1: Collect component names that were relocated to deprecated.
    // Only look at variable/constant kinds (the component itself, not its Props interface).
    let relocated_components: HashSet<String> = structural_changes
        .iter()
        .filter(|sc| matches!(sc.change_type, StructuralChangeType::Relocated { .. }))
        .filter(|sc| sc.description.contains("moved to deprecated"))
        .filter(|sc| {
            matches!(
                sc.kind,
                semver_analyzer_core::SymbolKind::Variable
                    | semver_analyzer_core::SymbolKind::Constant
            )
        })
        .map(|sc| sc.symbol.clone())
        .collect();

    if relocated_components.is_empty() {
        return vec![];
    }

    // Step 2: Build per-host rendering swap maps from SD source-level changes.
    // For each host component, track what it stopped and started rendering.
    let mut stopped_by_host: HashMap<String, HashSet<String>> = HashMap::new();
    let mut started_by_host: HashMap<String, HashSet<String>> = HashMap::new();

    for slc in &sd.source_level_changes {
        if slc.category != SourceLevelCategory::RenderedComponent {
            continue;
        }
        if let Some(ref old_val) = slc.old_value {
            if slc.new_value.is_none() {
                // "X no longer internally renders Y" — old_value = Y
                stopped_by_host
                    .entry(slc.component.clone())
                    .or_default()
                    .insert(old_val.clone());
            }
        }
        if let Some(ref new_val) = slc.new_value {
            if slc.old_value.is_none() {
                // "X now internally renders Y" — new_value = Y
                started_by_host
                    .entry(slc.component.clone())
                    .or_default()
                    .insert(new_val.clone());
            }
        }
    }

    // Step 3: For each relocated component, find hosts that stopped rendering
    // it and started rendering something new. The intersection of "started"
    // sets across hosts is the candidate replacement.
    let mut replacements = Vec::new();

    for old_comp in &relocated_components {
        // Find all hosts that stopped rendering this component
        let mut candidate_counts: HashMap<String, Vec<String>> = HashMap::new();

        for (host, stopped) in &stopped_by_host {
            if !stopped.contains(old_comp) {
                continue;
            }
            // This host stopped rendering old_comp — what did it start rendering?
            if let Some(started) = started_by_host.get(host) {
                for new_comp in started {
                    // Skip generic wrappers (Fragment, etc.) and the relocated
                    // component itself, and other relocated components.
                    if new_comp == "Fragment"
                        || new_comp == "React.Fragment"
                        || relocated_components.contains(new_comp)
                        || new_comp == old_comp
                    {
                        continue;
                    }
                    candidate_counts
                        .entry(new_comp.clone())
                        .or_default()
                        .push(host.clone());
                }
            }
        }

        // Pick the candidate with the most host evidence.
        // Tiebreaker: prefer candidates whose structural shape matches
        // (e.g., Chip → Label not LabelGroup; ChipGroup → LabelGroup not Label).
        let old_is_group = old_comp.ends_with("Group");
        if let Some((best_replacement, hosts)) =
            candidate_counts
                .into_iter()
                .max_by(|(name_a, hosts_a), (name_b, hosts_b)| {
                    hosts_a.len().cmp(&hosts_b.len()).then_with(|| {
                        // Prefer matching "Group" shape
                        let a_matches = name_a.ends_with("Group") == old_is_group;
                        let b_matches = name_b.ends_with("Group") == old_is_group;
                        a_matches.cmp(&b_matches)
                    })
                })
        {
            replacements.push(DeprecatedReplacement {
                old_component: old_comp.clone(),
                new_component: best_replacement,
                evidence_hosts: hosts,
            });
        }
    }

    replacements
}

/// Transform structural changes based on detected deprecated replacements.
///
/// For each component with a deprecated replacement:
/// 1. Convert the `Relocated` entry into a `Changed` entry with the
///    replacement component name in `after` and a descriptive message.
/// 2. Suppress the `signature_changed` entry for the Props interface
///    (base class change is a consequence of the replacement, not an
///    independent migration action).
fn apply_deprecated_replacements(
    structural_changes: Arc<Vec<StructuralChange>>,
    replacements: &[DeprecatedReplacement],
) -> Arc<Vec<StructuralChange>> {
    if replacements.is_empty() {
        return structural_changes;
    }

    // Build lookup: old component name → replacement info
    let replacement_map: HashMap<&str, &DeprecatedReplacement> = replacements
        .iter()
        .map(|r| (r.old_component.as_str(), r))
        .collect();

    // Also build a set of Props interface names to suppress signature-changed
    // entries for (e.g., "ChipProps" when "Chip" has a replacement).
    let suppressed_signature_changes: HashSet<String> = replacements
        .iter()
        .map(|r| format!("{}Props", r.old_component))
        .collect();

    let original = Arc::try_unwrap(structural_changes).unwrap_or_else(|arc| (*arc).clone());

    let mut result = Vec::with_capacity(original.len());

    for sc in original {
        // Check if this is a relocation for a replaced component
        if matches!(sc.change_type, StructuralChangeType::Relocated { .. }) {
            if let Some(repl) = replacement_map.get(sc.symbol.as_str()) {
                // Transform: Relocated → Changed (component replacement)
                result.push(StructuralChange {
                    change_type: StructuralChangeType::Changed(ChangeSubject::Symbol {
                        kind: sc.kind,
                    }),
                    before: Some(repl.old_component.clone()),
                    after: Some(repl.new_component.clone()),
                    description: format!(
                        "Component `{}` was deprecated and replaced by `{}`. \
                         Migrate from `<{}>` to `<{}>`.",
                        repl.old_component,
                        repl.new_component,
                        repl.old_component,
                        repl.new_component,
                    ),
                    ..sc
                });
                continue;
            }
            // Also check Props interfaces (e.g., ChipProps → LabelProps)
            let props_base = sc.symbol.strip_suffix("Props");
            if let Some(base) = props_base {
                if let Some(repl) = replacement_map.get(base) {
                    // Transform: Relocated ChipProps → Changed pointing to LabelProps
                    result.push(StructuralChange {
                        change_type: StructuralChangeType::Changed(ChangeSubject::Symbol {
                            kind: sc.kind,
                        }),
                        before: Some(format!("{}Props", repl.old_component)),
                        after: Some(format!("{}Props", repl.new_component)),
                        description: format!(
                            "Interface `{}Props` was deprecated and replaced by `{}Props`. \
                             Migrate from `{}Props` to `{}Props`.",
                            repl.old_component,
                            repl.new_component,
                            repl.old_component,
                            repl.new_component,
                        ),
                        ..sc
                    });
                    continue;
                }
            }
        }

        // Suppress signature-changed entries for Props of replaced components.
        // e.g., "ChipProps base class changed from X to LabelProps" is redundant
        // once we know Chip → Label.
        if matches!(sc.change_type, StructuralChangeType::Changed(_))
            && suppressed_signature_changes.contains(&sc.symbol)
            && sc.description.contains("base class changed")
        {
            debug!(
                symbol = %sc.symbol,
                "Suppressing signature-changed entry for replaced component Props"
            );
            continue;
        }

        result.push(sc);
    }

    Arc::new(result)
}

#[cfg(test)]
mod deprecated_replacement_tests {
    use super::*;
    use semver_analyzer_core::{
        ChangeSubject, SourceLevelCategory, SourceLevelChange, StructuralChange,
        StructuralChangeType, SymbolKind,
    };

    /// Helper: build a Relocated structural change for a component variable.
    fn relocated_component(name: &str) -> StructuralChange {
        StructuralChange {
            symbol: name.to_string(),
            qualified_name: format!("pkg/src/components/{name}/{name}.{name}"),
            kind: SymbolKind::Variable,
            package: Some("@patternfly/react-core".to_string()),
            change_type: StructuralChangeType::Relocated {
                from: ChangeSubject::Symbol {
                    kind: SymbolKind::Variable,
                },
                to: ChangeSubject::Symbol {
                    kind: SymbolKind::Variable,
                },
            },
            before: Some(format!("pkg/src/components/{name}/{name}.{name}")),
            after: Some(format!(
                "pkg/src/deprecated/components/{name}/{name}.{name}"
            )),
            description: format!("variable `{name}` moved to deprecated exports"),
            is_breaking: true,
            impact: None,
            migration_target: None,
        }
    }

    /// Helper: build a Relocated structural change for a Props interface.
    fn relocated_props(name: &str) -> StructuralChange {
        let props_name = format!("{name}Props");
        StructuralChange {
            symbol: props_name.clone(),
            qualified_name: format!("pkg/src/components/{name}/{name}.{props_name}"),
            kind: SymbolKind::Interface,
            package: Some("@patternfly/react-core".to_string()),
            change_type: StructuralChangeType::Relocated {
                from: ChangeSubject::Symbol {
                    kind: SymbolKind::Interface,
                },
                to: ChangeSubject::Symbol {
                    kind: SymbolKind::Interface,
                },
            },
            before: Some(format!("pkg/src/components/{name}/{name}.{props_name}")),
            after: Some(format!(
                "pkg/src/deprecated/components/{name}/{name}.{props_name}"
            )),
            description: format!("interface `{props_name}` moved to deprecated exports"),
            is_breaking: true,
            impact: None,
            migration_target: None,
        }
    }

    /// Helper: build a signature-changed structural change for Props base class.
    fn signature_changed_props(name: &str, old_base: &str, new_base: &str) -> StructuralChange {
        let props_name = format!("{name}Props");
        StructuralChange {
            symbol: props_name.clone(),
            qualified_name: format!("pkg/src/components/{name}/{name}.{props_name}"),
            kind: SymbolKind::Interface,
            package: Some("@patternfly/react-core".to_string()),
            change_type: StructuralChangeType::Changed(ChangeSubject::Symbol {
                kind: SymbolKind::Interface,
            }),
            before: Some(old_base.to_string()),
            after: Some(new_base.to_string()),
            description: format!("`{props_name}` base class changed from {old_base} to {new_base}"),
            is_breaking: true,
            impact: None,
            migration_target: None,
        }
    }

    /// Helper: build a RenderedComponent source-level change for "stopped rendering".
    fn stopped_rendering(host: &str, component: &str) -> SourceLevelChange {
        SourceLevelChange {
            component: host.to_string(),
            category: SourceLevelCategory::RenderedComponent,
            description: format!("{host} no longer internally renders {component}"),
            old_value: Some(component.to_string()),
            new_value: None,
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
        }
    }

    /// Helper: build a RenderedComponent source-level change for "started rendering".
    fn started_rendering(host: &str, component: &str) -> SourceLevelChange {
        SourceLevelChange {
            component: host.to_string(),
            category: SourceLevelCategory::RenderedComponent,
            description: format!("{host} now internally renders {component}"),
            old_value: None,
            new_value: Some(component.to_string()),
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
        }
    }

    /// Helper: build a non-RenderedComponent source-level change (e.g., CssToken).
    fn css_token_change(host: &str, desc: &str) -> SourceLevelChange {
        SourceLevelChange {
            component: host.to_string(),
            category: SourceLevelCategory::CssToken,
            description: desc.to_string(),
            old_value: None,
            new_value: None,
            has_test_implications: false,
            test_description: None,
            element: None,
            migration_from: None,
        }
    }

    fn make_sd(source_level_changes: Vec<SourceLevelChange>) -> SdPipelineResult {
        SdPipelineResult {
            source_level_changes,
            ..Default::default()
        }
    }

    // ── Detection tests ─────────────────────────────────────────────

    #[test]
    fn test_chip_to_label_detected_via_rendering_swap() {
        // Chip and ChipGroup relocated to deprecated.
        // ToolbarFilter and MultiTypeaheadSelect both stopped rendering
        // Chip/ChipGroup and started rendering Label/LabelGroup.
        let structural_changes = vec![
            relocated_component("Chip"),
            relocated_props("Chip"),
            relocated_component("ChipGroup"),
            relocated_props("ChipGroup"),
        ];

        let sd = make_sd(vec![
            stopped_rendering("ToolbarFilter", "Chip"),
            stopped_rendering("ToolbarFilter", "ChipGroup"),
            started_rendering("ToolbarFilter", "Label"),
            started_rendering("ToolbarFilter", "LabelGroup"),
            started_rendering("ToolbarFilter", "Fragment"),
            stopped_rendering("MultiTypeaheadSelect", "Chip"),
            stopped_rendering("MultiTypeaheadSelect", "ChipGroup"),
            started_rendering("MultiTypeaheadSelect", "Label"),
            started_rendering("MultiTypeaheadSelect", "LabelGroup"),
        ]);

        let result = detect_deprecated_replacements(&structural_changes, &sd);

        assert_eq!(
            result.len(),
            2,
            "Should detect Chip→Label and ChipGroup→LabelGroup"
        );

        let chip_repl = result.iter().find(|r| r.old_component == "Chip");
        assert!(chip_repl.is_some(), "Should find Chip replacement");
        let chip_repl = chip_repl.unwrap();
        assert_eq!(chip_repl.new_component, "Label");
        assert_eq!(chip_repl.evidence_hosts.len(), 2);
        assert!(chip_repl
            .evidence_hosts
            .contains(&"ToolbarFilter".to_string()));
        assert!(chip_repl
            .evidence_hosts
            .contains(&"MultiTypeaheadSelect".to_string()));

        let group_repl = result.iter().find(|r| r.old_component == "ChipGroup");
        assert!(group_repl.is_some(), "Should find ChipGroup replacement");
        let group_repl = group_repl.unwrap();
        assert_eq!(group_repl.new_component, "LabelGroup");
        assert_eq!(group_repl.evidence_hosts.len(), 2);
    }

    #[test]
    fn test_modal_not_detected_no_rendering_swap() {
        // Modal relocated to deprecated, but ModalContent stopped rendering
        // Modal without starting to render any differently-named replacement.
        let structural_changes = vec![
            relocated_component("Modal"),
            relocated_props("Modal"),
            relocated_component("ModalBox"),
            relocated_props("ModalBox"),
            relocated_component("ModalBoxBody"),
            relocated_props("ModalBoxBody"),
        ];

        let sd = make_sd(vec![
            stopped_rendering("ModalContent", "Modal"),
            stopped_rendering("ModalContent", "ModalBox"),
            stopped_rendering("ModalContent", "ModalBoxBody"),
            // ModalContent didn't start rendering anything new
        ]);

        let result = detect_deprecated_replacements(&structural_changes, &sd);
        assert!(
            result.is_empty(),
            "Modal should not be detected — no rendering swap"
        );
    }

    #[test]
    fn test_dual_list_selector_not_detected_no_external_swap() {
        // DualListSelector sub-components are relocated, but they only
        // stopped rendering within their own family (which is also relocated).
        let structural_changes = vec![
            relocated_component("DualListSelector"),
            relocated_component("DualListSelectorPane"),
            relocated_component("DualListSelectorList"),
            relocated_component("DualListSelectorControl"),
        ];

        let sd = make_sd(vec![
            stopped_rendering("DualListSelectorPane", "DualListSelector"),
            stopped_rendering("DualListSelectorPane", "DualListSelectorList"),
            stopped_rendering("DualListSelector", "DualListSelectorControl"),
            stopped_rendering("DualListSelector", "DualListSelectorPane"),
            // The hosts stopped rendering other relocated components,
            // not differently-named replacements.
        ]);

        let result = detect_deprecated_replacements(&structural_changes, &sd);
        assert!(
            result.is_empty(),
            "DualListSelector should not be detected — sub-components are also relocated"
        );
    }

    #[test]
    fn test_tile_not_detected_no_swap() {
        // Tile relocated to deprecated with no rendering swap at all.
        let structural_changes = vec![relocated_component("Tile"), relocated_props("Tile")];

        let sd = make_sd(vec![
            // No rendering changes involving Tile
            css_token_change("Tile", "Tile no longer uses CSS token foo"),
        ]);

        let result = detect_deprecated_replacements(&structural_changes, &sd);
        assert!(
            result.is_empty(),
            "Tile should not be detected — no rendering swap"
        );
    }

    #[test]
    fn test_fragment_only_swap_not_detected() {
        // A relocated component where the host only started rendering Fragment.
        // Fragment is a generic wrapper and should be filtered out.
        let structural_changes = vec![relocated_component("SomeComponent")];

        let sd = make_sd(vec![
            stopped_rendering("HostComponent", "SomeComponent"),
            started_rendering("HostComponent", "Fragment"),
            started_rendering("HostComponent", "React.Fragment"),
        ]);

        let result = detect_deprecated_replacements(&structural_changes, &sd);
        assert!(
            result.is_empty(),
            "Fragment-only swaps should not produce a replacement"
        );
    }

    #[test]
    fn test_no_relocations_returns_empty() {
        // No relocated components at all — should short-circuit.
        let structural_changes = vec![StructuralChange {
            symbol: "SomeProps".to_string(),
            qualified_name: "pkg/SomeProps".to_string(),
            kind: SymbolKind::Interface,
            package: None,
            change_type: StructuralChangeType::Changed(ChangeSubject::Symbol {
                kind: SymbolKind::Interface,
            }),
            before: Some("OldType".to_string()),
            after: Some("NewType".to_string()),
            description: "type changed".to_string(),
            is_breaking: true,
            impact: None,
            migration_target: None,
        }];

        let sd = make_sd(vec![
            stopped_rendering("Host", "Foo"),
            started_rendering("Host", "Bar"),
        ]);

        let result = detect_deprecated_replacements(&structural_changes, &sd);
        assert!(result.is_empty());
    }

    #[test]
    fn test_single_host_swap_detected() {
        // Only one host shows the swap, but it's still valid evidence.
        let structural_changes = vec![relocated_component("OldWidget")];

        let sd = make_sd(vec![
            stopped_rendering("Dashboard", "OldWidget"),
            started_rendering("Dashboard", "NewWidget"),
        ]);

        let result = detect_deprecated_replacements(&structural_changes, &sd);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].old_component, "OldWidget");
        assert_eq!(result[0].new_component, "NewWidget");
        assert_eq!(result[0].evidence_hosts, vec!["Dashboard".to_string()]);
    }

    #[test]
    fn test_props_interface_relocation_not_counted_as_component() {
        // Only Props interface is relocated (no component variable).
        // Should not be detected — we only look at Variable/Constant kinds.
        let structural_changes = vec![relocated_props("SomeWidget")];

        let sd = make_sd(vec![
            stopped_rendering("Host", "SomeWidget"),
            started_rendering("Host", "NewWidget"),
        ]);

        let result = detect_deprecated_replacements(&structural_changes, &sd);
        assert!(
            result.is_empty(),
            "Props-only relocations should not trigger detection"
        );
    }

    #[test]
    fn test_relocated_component_swapped_for_another_relocated_component_ignored() {
        // Both OldA and OldB are relocated. Host stops rendering OldA
        // and starts rendering OldB. Since OldB is also relocated, it
        // should not be treated as a replacement.
        let structural_changes = vec![relocated_component("OldA"), relocated_component("OldB")];

        let sd = make_sd(vec![
            stopped_rendering("Host", "OldA"),
            started_rendering("Host", "OldB"),
        ]);

        let result = detect_deprecated_replacements(&structural_changes, &sd);
        assert!(
            result.is_empty(),
            "Swapping one relocated component for another should not count"
        );
    }

    #[test]
    fn test_best_candidate_wins_with_most_hosts() {
        // Two possible replacements, but one has more host evidence.
        let structural_changes = vec![relocated_component("OldComp")];

        let sd = make_sd(vec![
            stopped_rendering("Host1", "OldComp"),
            started_rendering("Host1", "BetterReplacement"),
            started_rendering("Host1", "WeakerCandidate"),
            stopped_rendering("Host2", "OldComp"),
            started_rendering("Host2", "BetterReplacement"),
            // Host2 does NOT start rendering WeakerCandidate
        ]);

        let result = detect_deprecated_replacements(&structural_changes, &sd);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].new_component, "BetterReplacement");
        assert_eq!(result[0].evidence_hosts.len(), 2);
    }

    // ── Transformation tests ────────────────────────────────────────

    #[test]
    fn test_apply_transforms_relocation_to_changed() {
        let changes = Arc::new(vec![
            relocated_component("Chip"),
            relocated_props("Chip"),
            signature_changed_props("Chip", "React.HTMLProps<HTMLDivElement>", "LabelProps"),
        ]);

        let replacements = vec![DeprecatedReplacement {
            old_component: "Chip".to_string(),
            new_component: "Label".to_string(),
            evidence_hosts: vec!["ToolbarFilter".to_string()],
        }];

        let result = apply_deprecated_replacements(changes, &replacements);

        // Should have 2 entries: transformed component + transformed props.
        // The signature-changed entry should be suppressed.
        assert_eq!(
            result.len(),
            2,
            "Expected 2 entries (component + props), got {}",
            result.len()
        );

        // Check the component entry was transformed
        let comp = &result[0];
        assert_eq!(comp.symbol, "Chip");
        assert!(
            matches!(comp.change_type, StructuralChangeType::Changed(_)),
            "Should be Changed, got {:?}",
            comp.change_type
        );
        assert_eq!(comp.before.as_deref(), Some("Chip"));
        assert_eq!(comp.after.as_deref(), Some("Label"));
        assert!(comp.description.contains("replaced by `Label`"));

        // Check the props entry was transformed
        let props = &result[1];
        assert_eq!(props.symbol, "ChipProps");
        assert!(matches!(
            props.change_type,
            StructuralChangeType::Changed(_)
        ));
        assert_eq!(props.before.as_deref(), Some("ChipProps"));
        assert_eq!(props.after.as_deref(), Some("LabelProps"));
        assert!(props.description.contains("replaced by `LabelProps`"));
    }

    #[test]
    fn test_apply_suppresses_signature_changed_for_replaced_props() {
        let changes = Arc::new(vec![
            // Unrelated change should pass through
            StructuralChange {
                symbol: "OtherProps".to_string(),
                qualified_name: "pkg/OtherProps".to_string(),
                kind: SymbolKind::Interface,
                package: None,
                change_type: StructuralChangeType::Changed(ChangeSubject::Symbol {
                    kind: SymbolKind::Interface,
                }),
                before: Some("OldBase".to_string()),
                after: Some("NewBase".to_string()),
                description: "`OtherProps` base class changed from OldBase to NewBase".to_string(),
                is_breaking: true,
                impact: None,
                migration_target: None,
            },
            // This should be suppressed
            signature_changed_props("Chip", "React.HTMLProps<HTMLDivElement>", "LabelProps"),
        ]);

        let replacements = vec![DeprecatedReplacement {
            old_component: "Chip".to_string(),
            new_component: "Label".to_string(),
            evidence_hosts: vec!["ToolbarFilter".to_string()],
        }];

        let result = apply_deprecated_replacements(changes, &replacements);

        // OtherProps should pass through, ChipProps signature-changed should be suppressed
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].symbol, "OtherProps");
    }

    #[test]
    fn test_apply_no_replacements_returns_unchanged() {
        let original = vec![relocated_component("Modal"), relocated_props("Modal")];
        let changes = Arc::new(original.clone());

        let result = apply_deprecated_replacements(changes, &[]);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].symbol, "Modal");
        assert!(matches!(
            result[0].change_type,
            StructuralChangeType::Relocated { .. }
        ));
    }

    #[test]
    fn test_apply_preserves_non_replaced_relocations() {
        // Mix of replaced (Chip) and non-replaced (Modal) relocations.
        let changes = Arc::new(vec![
            relocated_component("Chip"),
            relocated_component("Modal"),
            relocated_props("Chip"),
            relocated_props("Modal"),
        ]);

        let replacements = vec![DeprecatedReplacement {
            old_component: "Chip".to_string(),
            new_component: "Label".to_string(),
            evidence_hosts: vec!["ToolbarFilter".to_string()],
        }];

        let result = apply_deprecated_replacements(changes, &replacements);

        // Chip + ChipProps → transformed (2 Changed entries)
        // Modal + ModalProps → preserved (2 Relocated entries)
        assert_eq!(result.len(), 4);

        let chip = result.iter().find(|s| s.symbol == "Chip").unwrap();
        assert!(matches!(chip.change_type, StructuralChangeType::Changed(_)));

        let modal = result.iter().find(|s| s.symbol == "Modal").unwrap();
        assert!(matches!(
            modal.change_type,
            StructuralChangeType::Relocated { .. }
        ));
    }

    #[test]
    fn test_full_patternfly_scenario() {
        // Simulate the real PatternFly v5→v6 scenario with Chip, ChipGroup,
        // Modal, DualListSelector, and Tile — all relocated to deprecated.
        // Only Chip and ChipGroup should be detected as having replacements.
        let structural_changes = vec![
            relocated_component("Chip"),
            relocated_props("Chip"),
            relocated_component("ChipGroup"),
            relocated_props("ChipGroup"),
            signature_changed_props("Chip", "React.HTMLProps<HTMLDivElement>", "LabelProps"),
            signature_changed_props(
                "ChipGroup",
                "React.HTMLProps<HTMLUListElement>",
                "Omit<LabelGroupProps, 'ref'>",
            ),
            relocated_component("Modal"),
            relocated_props("Modal"),
            relocated_component("ModalBox"),
            relocated_component("Tile"),
            relocated_props("Tile"),
            relocated_component("DualListSelector"),
            relocated_props("DualListSelector"),
        ];

        let sd = make_sd(vec![
            // Chip → Label swaps
            stopped_rendering("ToolbarFilter", "Chip"),
            stopped_rendering("ToolbarFilter", "ChipGroup"),
            started_rendering("ToolbarFilter", "Label"),
            started_rendering("ToolbarFilter", "LabelGroup"),
            started_rendering("ToolbarFilter", "Fragment"),
            stopped_rendering("MultiTypeaheadSelect", "Chip"),
            stopped_rendering("MultiTypeaheadSelect", "ChipGroup"),
            started_rendering("MultiTypeaheadSelect", "Label"),
            started_rendering("MultiTypeaheadSelect", "LabelGroup"),
            // Modal — no differently-named replacement
            stopped_rendering("ModalContent", "Modal"),
            stopped_rendering("ModalContent", "ModalBox"),
            // DualListSelector — internal sub-component swaps (both relocated)
            stopped_rendering("DualListSelectorPane", "DualListSelector"),
            // Tile — nothing
        ]);

        // Detection
        let replacements = detect_deprecated_replacements(&structural_changes, &sd);
        assert_eq!(
            replacements.len(),
            2,
            "Only Chip and ChipGroup should be detected"
        );

        let chip = replacements
            .iter()
            .find(|r| r.old_component == "Chip")
            .unwrap();
        assert_eq!(chip.new_component, "Label");

        let group = replacements
            .iter()
            .find(|r| r.old_component == "ChipGroup")
            .unwrap();
        assert_eq!(group.new_component, "LabelGroup");

        // Transformation
        let changes = Arc::new(structural_changes);
        let result = apply_deprecated_replacements(changes, &replacements);

        // Chip (component + props) → 2 Changed entries
        // ChipGroup (component + props) → 2 Changed entries
        // ChipProps signature-changed → suppressed
        // ChipGroupProps signature-changed → suppressed
        // Modal (component + props) → 2 Relocated entries (preserved)
        // ModalBox → 1 Relocated entry (preserved)
        // Tile (component + props) → 2 Relocated entries (preserved)
        // DualListSelector (component + props) → 2 Relocated entries (preserved)
        // Total: 4 Changed + 7 Relocated = 11
        assert_eq!(
            result.len(),
            11,
            "Expected 11 entries (4 Changed + 7 Relocated), got {}",
            result.len()
        );

        // Verify Chip entries are Changed
        let chip_entries: Vec<_> = result
            .iter()
            .filter(|s| s.symbol == "Chip" || s.symbol == "ChipProps")
            .collect();
        assert_eq!(chip_entries.len(), 2);
        for entry in &chip_entries {
            assert!(
                matches!(entry.change_type, StructuralChangeType::Changed(_)),
                "{} should be Changed",
                entry.symbol
            );
        }

        // Verify Modal entries are still Relocated
        let modal_entries: Vec<_> = result
            .iter()
            .filter(|s| s.symbol == "Modal" || s.symbol == "ModalProps")
            .collect();
        assert_eq!(modal_entries.len(), 2);
        for entry in &modal_entries {
            assert!(
                matches!(entry.change_type, StructuralChangeType::Relocated { .. }),
                "{} should remain Relocated",
                entry.symbol
            );
        }

        // Verify no signature-changed entries remain for Chip/ChipGroup
        let sig_changed: Vec<_> = result
            .iter()
            .filter(|s| {
                (s.symbol == "ChipProps" || s.symbol == "ChipGroupProps")
                    && s.description.contains("base class changed")
            })
            .collect();
        assert!(
            sig_changed.is_empty(),
            "Signature-changed entries for replaced Props should be suppressed"
        );
    }
}
