//! TypeScript-specific analysis extensions.
//!
//! `TsAnalysisExtensions` bundles all language-specific data that the
//! TypeScript implementation produces during analysis and passes through
//! to `build_report()`. Core never inspects this data — it just carries
//! it from the analysis phase to the report builder.

use crate::hierarchy_types::HierarchyDelta;
use crate::sd_types::SdPipelineResult;
use semver_analyzer_core::ExpectedChild;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// All TypeScript-specific data produced during analysis.
///
/// This is the concrete type for `Language::AnalysisExtensions`.
/// It bundles:
/// - SD pipeline results (source-level changes, composition trees, etc.)
/// - Hierarchy inference results (hierarchy deltas, inferred hierarchies)
///
/// Core's orchestrator receives this as an opaque `L::AnalysisExtensions`
/// and passes it to `build_report()` without inspecting it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TsAnalysisExtensions {
    /// SD pipeline results. None when running the v1 (BU) pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sd_result: Option<SdPipelineResult>,

    /// Hierarchy changes between versions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hierarchy_deltas: Vec<HierarchyDelta>,

    /// Inferred hierarchies for the new version.
    /// Family name → component name → expected children.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub new_hierarchies: HashMap<String, HashMap<String, Vec<ExpectedChild>>>,
}

impl TsAnalysisExtensions {
    /// Summary line for progress logging.
    pub fn summary_line(&self) -> String {
        let mut parts = Vec::new();

        if let Some(ref sd) = self.sd_result {
            parts.push(format!(
                "{} source-level changes, {} composition trees, {} conformance checks",
                sd.source_level_changes.len(),
                sd.composition_trees.len(),
                sd.conformance_checks.len(),
            ));
            if !sd.composition_changes.is_empty() {
                parts.push(format!(
                    "{} composition changes",
                    sd.composition_changes.len(),
                ));
            }
        }

        if !self.hierarchy_deltas.is_empty() {
            parts.push(format!("{} hierarchy deltas", self.hierarchy_deltas.len()));
        }

        if parts.is_empty() {
            "no language-specific extensions".to_string()
        } else {
            parts.join(", ")
        }
    }
}

impl fmt::Display for TsAnalysisExtensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary_line())
    }
}
