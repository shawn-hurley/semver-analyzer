//! Core types, traits, and diff engine for the semver-analyzer.
//!
//! This crate contains language-agnostic components:
//! - API surface types (`ApiSurface`, `Symbol`, etc.)
//! - Report types (`AnalysisReport`, `StructuralChange`, etc.)
//! - Traits for language-pluggable analysis (`Language`, `LanguageSemantics`, etc.)
//! - The structural diff engine (`diff_surfaces_with_semantics`)

pub mod cli;
pub mod diagnostics;
pub mod diff;
pub mod error;
pub mod git;
pub mod shared;
pub mod traits;
pub mod types;

pub use shared::*;
pub use traits::*;
pub use types::*;

/// Shared test utilities for the core crate.
///
/// Provides a minimal `TestLang` implementation of the `Language` trait
/// for use in unit tests across core modules.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use crate::traits::{Language, LanguageSemantics, MessageFormatter};
    use crate::types::{
        AnalysisMetadata, AnalysisReport, AnalysisResult, ApiSurface, Caller, ChangedFunction,
        Comparison, ManifestChange, Reference, StructuralChange, Summary, Symbol, TestDiff,
        TestFile, Visibility,
    };

    /// Minimal Language implementation for tests.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct TestLang;

    impl<M: Default + Clone + PartialEq> LanguageSemantics<M> for TestLang {
        fn is_member_addition_breaking(&self, _c: &Symbol<M>, _m: &Symbol<M>) -> bool {
            false
        }
        fn same_family(&self, _a: &Symbol<M>, _b: &Symbol<M>) -> bool {
            false
        }
        fn same_identity(&self, _a: &Symbol<M>, _b: &Symbol<M>) -> bool {
            false
        }
        fn visibility_rank(&self, _v: Visibility) -> u8 {
            0
        }
    }

    impl MessageFormatter for TestLang {
        fn describe(&self, _c: &StructuralChange) -> String {
            String::new()
        }
    }

    impl Language for TestLang {
        type SymbolData = ();
        type Category = String;
        type ManifestChangeType = String;
        type Evidence = String;
        type ReportData = String;
        type AnalysisExtensions = crate::types::EmptyExtensions;
        const RENAMEABLE_SYMBOL_KINDS: &'static [crate::types::SymbolKind] = &[];
        const NAME: &'static str = "test";
        const MANIFEST_FILES: &'static [&'static str] = &[];
        const SOURCE_FILE_PATTERNS: &'static [&'static str] = &[];

        fn extract(
            &self,
            _repo: &Path,
            _git_ref: &str,
            _degradation: Option<&crate::diagnostics::DegradationTracker>,
        ) -> anyhow::Result<ApiSurface> {
            Ok(ApiSurface::default())
        }
        fn parse_changed_functions(
            &self,
            _repo: &Path,
            _from_ref: &str,
            _to_ref: &str,
        ) -> anyhow::Result<Vec<ChangedFunction>> {
            Ok(vec![])
        }
        fn find_callers(&self, _file: &Path, _symbol_name: &str) -> anyhow::Result<Vec<Caller>> {
            Ok(vec![])
        }
        fn find_references(
            &self,
            _file: &Path,
            _symbol_name: &str,
        ) -> anyhow::Result<Vec<Reference>> {
            Ok(vec![])
        }
        fn find_tests(&self, _repo: &Path, _source_file: &Path) -> anyhow::Result<Vec<TestFile>> {
            Ok(vec![])
        }
        fn diff_test_assertions(
            &self,
            _repo: &Path,
            _test_file: &TestFile,
            _from_ref: &str,
            _to_ref: &str,
        ) -> anyhow::Result<TestDiff> {
            Ok(TestDiff {
                test_file: PathBuf::new(),
                removed_assertions: vec![],
                added_assertions: vec![],
                has_assertion_changes: false,
                full_diff: String::new(),
            })
        }

        fn diff_manifest_content(_old: &str, _new: &str) -> Vec<ManifestChange<Self>> {
            vec![]
        }
        fn should_exclude_from_analysis(_path: &Path) -> bool {
            false
        }
        fn build_report(
            &self,
            _results: &AnalysisResult<Self>,
            _repo: &Path,
            _from_ref: &str,
            _to_ref: &str,
        ) -> AnalysisReport<Self> {
            AnalysisReport {
                repository: PathBuf::new(),
                comparison: Comparison {
                    from_ref: String::new(),
                    to_ref: String::new(),
                    from_sha: String::new(),
                    to_sha: String::new(),
                    commit_count: 0,
                    analysis_timestamp: String::new(),
                },
                summary: Summary {
                    total_breaking_changes: 0,
                    breaking_api_changes: 0,
                    breaking_behavioral_changes: 0,
                    files_with_breaking_changes: 0,
                },
                changes: vec![],
                manifest_changes: vec![],
                added_files: vec![],
                packages: vec![],
                member_renames: HashMap::new(),
                inferred_rename_patterns: None,
                extensions: crate::types::EmptyExtensions {},
                metadata: AnalysisMetadata {
                    call_graph_analysis: String::new(),
                    tool_version: String::new(),
                    llm_usage: None,
                },
            }
        }
    }
}
