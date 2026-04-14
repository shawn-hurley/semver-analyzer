//! Java report builder.

use crate::language::Java;
use semver_analyzer_core::{
    AnalysisMetadata, AnalysisReport, AnalysisResult, ApiChange, Comparison, FileChanges,
    FileStatus, Summary,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Build the Java analysis report from raw results.
pub fn build_report(
    results: &AnalysisResult<Java>,
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
) -> AnalysisReport<Java> {
    let mut file_map: HashMap<PathBuf, Vec<ApiChange>> = HashMap::new();

    for change in results.structural_changes.iter() {
        if !change.is_breaking {
            continue;
        }

        let file = find_file_for_symbol(
            &change.qualified_name,
            &results.old_surface,
            &results.new_surface,
        )
        .unwrap_or_else(|| PathBuf::from("unknown"));

        let api_change = ApiChange {
            symbol: change.symbol.clone(),
            qualified_name: change.qualified_name.clone(),
            kind: change.kind.into(),
            change: change.change_type.to_api_change_type(),
            before: change.before.clone(),
            after: change.after.clone(),
            description: change.description.clone(),
            migration_target: change.migration_target.clone(),
            removal_disposition: None,
        };

        file_map.entry(file).or_default().push(api_change);
    }

    let mut changes: Vec<FileChanges<Java>> = file_map
        .into_iter()
        .map(|(file, api_changes)| FileChanges {
            file,
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: api_changes,
            breaking_behavioral_changes: Vec::new(),
            container_changes: Vec::new(),
        })
        .collect();

    changes.sort_by(|a, b| a.file.cmp(&b.file));

    let breaking_api = results
        .structural_changes
        .iter()
        .filter(|c| c.is_breaking)
        .count();

    let breaking_behavioral = results
        .behavioral_changes
        .iter()
        .filter(|c| !c.is_internal_only.unwrap_or(false))
        .count();

    let files_with_breaking = changes.len();

    AnalysisReport {
        repository: repo.to_path_buf(),
        comparison: Comparison {
            from_ref: from_ref.to_string(),
            to_ref: to_ref.to_string(),
            from_sha: String::new(),
            to_sha: String::new(),
            commit_count: 0,
            analysis_timestamp: String::new(),
        },
        summary: Summary {
            total_breaking_changes: breaking_api + breaking_behavioral,
            breaking_api_changes: breaking_api,
            breaking_behavioral_changes: breaking_behavioral,
            files_with_breaking_changes: files_with_breaking,
        },
        changes,
        manifest_changes: results.manifest_changes.clone(),
        added_files: Vec::new(),
        packages: Vec::new(),
        member_renames: HashMap::new(),
        inferred_rename_patterns: results.inferred_rename_patterns.clone(),
        extensions: (),
        metadata: AnalysisMetadata {
            call_graph_analysis: String::new(),
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            llm_usage: None,
        },
    }
}

fn find_file_for_symbol(
    qualified_name: &str,
    old_surface: &semver_analyzer_core::ApiSurface<crate::types::JavaSymbolData>,
    new_surface: &semver_analyzer_core::ApiSurface<crate::types::JavaSymbolData>,
) -> Option<PathBuf> {
    for surface in [new_surface, old_surface] {
        for sym in &surface.symbols {
            if sym.qualified_name == qualified_name {
                return Some(sym.file.clone());
            }
            for member in &sym.members {
                if member.qualified_name == qualified_name {
                    return Some(sym.file.clone());
                }
            }
        }
    }
    None
}
