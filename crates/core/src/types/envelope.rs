//! Report envelope types for the two-tier report architecture.
//!
//! The `ReportEnvelope` is a self-describing container that separates
//! language-agnostic data (always readable) from language-specific data
//! (requires knowing the `Language` type to deserialize).
//!
//! See `design/03-report-envelope.md` for the full design.

use super::report::StructuralChange;
use crate::traits::Language;
use serde::{Deserialize, Serialize};

// ── Report envelope ─────────────────────────────────────────────────

/// Self-describing container for an analysis report.
///
/// The language-agnostic fields (`summary`, `structural_changes`) are always
/// accessible. The `language_report` field requires knowing the concrete
/// `Language` implementation to deserialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportEnvelope {
    /// Which language produced this report. Matches `L::NAME`.
    pub language: String,

    /// Tool version that produced this report.
    pub version: String,

    /// Aggregate statistics. Readable without language knowledge.
    pub summary: AnalysisSummary,

    /// All structural changes detected by the diff engine.
    /// Descriptions are already formatted by the MessageFormatter.
    pub structural_changes: Vec<StructuralChange>,

    /// Language-specific report data, serialized as JSON.
    /// Consumers call `envelope.language_report::<L>()` to deserialize.
    pub language_report: serde_json::Value,
}

impl ReportEnvelope {
    /// Deserialize the language-specific report section.
    ///
    /// Returns an error if `L::NAME` doesn't match `self.language`
    /// or if deserialization fails.
    pub fn language_report<L: Language>(&self) -> anyhow::Result<LanguageReport<L>> {
        if L::NAME != self.language {
            anyhow::bail!(
                "Report was produced by '{}' but requested as '{}'",
                self.language,
                L::NAME
            );
        }
        Ok(serde_json::from_value(self.language_report.clone())?)
    }
}

/// Aggregate statistics readable without language knowledge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisSummary {
    /// Total structural breaking changes.
    pub total_structural_breaking: usize,
    /// Total structural non-breaking changes.
    pub total_structural_non_breaking: usize,
    /// Total behavioral changes (from the language-specific BU pipeline).
    pub total_behavioral_changes: usize,
    /// Total manifest changes.
    pub total_manifest_changes: usize,
    /// Number of packages analyzed.
    pub packages_analyzed: usize,
    /// Number of files changed.
    pub files_changed: usize,
    /// Breakdown of structural changes by lifecycle type.
    pub by_change_type: ChangeTypeCounts,
}

/// Breakdown of structural changes by lifecycle type.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChangeTypeCounts {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub renamed: usize,
    pub relocated: usize,
}

/// Language-specific section of the report.
///
/// Deserialized only by consumers that know the language.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct LanguageReport<L: Language> {
    /// Behavioral changes with language-specific categories and evidence.
    pub behavioral_changes: Vec<LanguageBehavioralChange<L>>,

    /// Manifest changes with language-specific change types.
    pub manifest_changes: Vec<LanguageManifestChange<L>>,

    /// Framework-specific analysis data.
    pub data: L::ReportData,
}

/// A behavioral change with language-specific types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct LanguageBehavioralChange<L: Language> {
    pub symbol: String,
    pub category: Option<L::Category>,
    pub description: String,
    pub confidence: f64,
    pub evidence: L::Evidence,
    pub is_internal_only: bool,
}

/// A manifest change with language-specific types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct LanguageManifestChange<L: Language> {
    pub field: String,
    pub change_type: L::ManifestChangeType,
    pub before: Option<String>,
    pub after: Option<String>,
    pub description: String,
    pub is_breaking: bool,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    // ── Dummy Language impl for testing ──────────────────────────

    #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
    enum TestCategory {
        Alpha,
        Beta,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    enum TestManifest {
        DepAdded,
        DepRemoved,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct TestEvidence {
        detail: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct TestReportData {
        count: usize,
    }

    #[derive(Debug)]
    struct TestLang;

    impl crate::traits::LanguageSemantics for TestLang {
        fn is_member_addition_breaking(
            &self,
            _container: &crate::types::Symbol,
            _member: &crate::types::Symbol,
        ) -> bool {
            false
        }
        fn same_family(&self, _a: &crate::types::Symbol, _b: &crate::types::Symbol) -> bool {
            false
        }
        fn same_identity(&self, _a: &crate::types::Symbol, _b: &crate::types::Symbol) -> bool {
            false
        }
        fn visibility_rank(&self, _v: crate::types::Visibility) -> u8 {
            0
        }
    }

    impl crate::traits::MessageFormatter for TestLang {
        fn describe(&self, _change: &StructuralChange) -> String {
            String::new()
        }
    }

    impl Language for TestLang {
        type Category = TestCategory;
        type ManifestChangeType = TestManifest;
        type Evidence = TestEvidence;
        type ReportData = TestReportData;
        type AnalysisExtensions = ();
        const RENAMEABLE_SYMBOL_KINDS: &'static [crate::types::SymbolKind] = &[];
        const NAME: &'static str = "test";
        const MANIFEST_FILES: &'static [&'static str] = &[];
        const SOURCE_FILE_PATTERNS: &'static [&'static str] = &[];

        fn extract(
            &self,
            _repo: &Path,
            _git_ref: &str,
        ) -> anyhow::Result<crate::types::ApiSurface> {
            Ok(crate::types::ApiSurface::default())
        }
        fn parse_changed_functions(
            &self,
            _repo: &Path,
            _from_ref: &str,
            _to_ref: &str,
        ) -> anyhow::Result<Vec<crate::types::ChangedFunction>> {
            Ok(vec![])
        }
        fn find_callers(
            &self,
            _file: &Path,
            _symbol_name: &str,
        ) -> anyhow::Result<Vec<crate::types::Caller>> {
            Ok(vec![])
        }
        fn find_references(
            &self,
            _file: &Path,
            _symbol_name: &str,
        ) -> anyhow::Result<Vec<crate::types::Reference>> {
            Ok(vec![])
        }
        fn find_tests(
            &self,
            _repo: &Path,
            _source_file: &Path,
        ) -> anyhow::Result<Vec<crate::types::TestFile>> {
            Ok(vec![])
        }
        fn diff_test_assertions(
            &self,
            _repo: &Path,
            _test_file: &crate::types::TestFile,
            _from_ref: &str,
            _to_ref: &str,
        ) -> anyhow::Result<crate::types::TestDiff> {
            Ok(crate::types::TestDiff {
                test_file: std::path::PathBuf::new(),
                removed_assertions: vec![],
                added_assertions: vec![],
                has_assertion_changes: false,
                full_diff: String::new(),
            })
        }

        fn diff_manifest_content(
            _old: &str,
            _new: &str,
        ) -> Vec<crate::types::ManifestChange<Self>> {
            vec![]
        }
        fn should_exclude_from_analysis(_path: &Path) -> bool {
            false
        }
        fn build_report(
            _results: &crate::types::AnalysisResult<Self>,
            _repo: &Path,
            _from_ref: &str,
            _to_ref: &str,
        ) -> crate::types::AnalysisReport<Self> {
            crate::types::AnalysisReport {
                repository: std::path::PathBuf::new(),
                comparison: crate::types::Comparison {
                    from_ref: String::new(),
                    to_ref: String::new(),
                    from_sha: String::new(),
                    to_sha: String::new(),
                    commit_count: 0,
                    analysis_timestamp: String::new(),
                },
                summary: crate::types::Summary {
                    total_breaking_changes: 0,
                    breaking_api_changes: 0,
                    breaking_behavioral_changes: 0,
                    files_with_breaking_changes: 0,
                },
                changes: vec![],
                manifest_changes: vec![],
                added_files: vec![],
                packages: vec![],
                member_renames: std::collections::HashMap::new(),
                inferred_rename_patterns: None,
                extensions: (),
                metadata: crate::types::AnalysisMetadata {
                    call_graph_analysis: String::new(),
                    tool_version: String::new(),
                    llm_usage: None,
                },
            }
        }
    }

    // ── Tests ───────────────────────────────────────────────────

    #[test]
    fn analysis_summary_serialization_round_trip() {
        let summary = AnalysisSummary {
            total_structural_breaking: 10,
            total_structural_non_breaking: 5,
            total_behavioral_changes: 3,
            total_manifest_changes: 2,
            packages_analyzed: 1,
            files_changed: 15,
            by_change_type: ChangeTypeCounts {
                added: 2,
                removed: 5,
                changed: 4,
                renamed: 3,
                relocated: 1,
            },
        };

        let json = serde_json::to_string(&summary).unwrap();
        let roundtrip: AnalysisSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip.total_structural_breaking, 10);
        assert_eq!(roundtrip.by_change_type.removed, 5);
    }

    #[test]
    fn language_report_serialization_round_trip() {
        let report = LanguageReport::<TestLang> {
            behavioral_changes: vec![LanguageBehavioralChange {
                symbol: "foo".into(),
                category: Some(TestCategory::Alpha),
                description: "something changed".into(),
                confidence: 0.9,
                evidence: TestEvidence {
                    detail: "test delta".into(),
                },
                is_internal_only: false,
            }],
            manifest_changes: vec![LanguageManifestChange {
                field: "dependencies.bar".into(),
                change_type: TestManifest::DepAdded,
                before: None,
                after: Some("^1.0.0".into()),
                description: "dependency added".into(),
                is_breaking: false,
            }],
            data: TestReportData { count: 42 },
        };

        let json = serde_json::to_string(&report).unwrap();
        let roundtrip: LanguageReport<TestLang> = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip.behavioral_changes.len(), 1);
        assert_eq!(
            roundtrip.behavioral_changes[0].category,
            Some(TestCategory::Alpha)
        );
        assert_eq!(
            roundtrip.manifest_changes[0].change_type,
            TestManifest::DepAdded
        );
        assert_eq!(roundtrip.data.count, 42);
    }

    #[test]
    fn report_envelope_language_mismatch_returns_error() {
        let envelope = ReportEnvelope {
            language: "go".into(),
            version: "0.1.0".into(),
            summary: AnalysisSummary {
                total_structural_breaking: 0,
                total_structural_non_breaking: 0,
                total_behavioral_changes: 0,
                total_manifest_changes: 0,
                packages_analyzed: 0,
                files_changed: 0,
                by_change_type: ChangeTypeCounts::default(),
            },
            structural_changes: vec![],
            language_report: serde_json::Value::Null,
        };

        let result = envelope.language_report::<TestLang>();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("go"),
            "Error should mention the actual language: {}",
            err
        );
        assert!(
            err.contains("test"),
            "Error should mention the requested language: {}",
            err
        );
    }

    #[test]
    fn report_envelope_correct_language_deserializes() {
        let report = LanguageReport::<TestLang> {
            behavioral_changes: vec![],
            manifest_changes: vec![],
            data: TestReportData { count: 7 },
        };

        let envelope = ReportEnvelope {
            language: "test".into(),
            version: "0.1.0".into(),
            summary: AnalysisSummary {
                total_structural_breaking: 0,
                total_structural_non_breaking: 0,
                total_behavioral_changes: 0,
                total_manifest_changes: 0,
                packages_analyzed: 0,
                files_changed: 0,
                by_change_type: ChangeTypeCounts::default(),
            },
            structural_changes: vec![],
            language_report: serde_json::to_value(&report).unwrap(),
        };

        let deserialized = envelope.language_report::<TestLang>().unwrap();
        assert_eq!(deserialized.data.count, 7);
    }
}
