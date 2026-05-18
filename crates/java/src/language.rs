//! Java `Language` trait implementation.
//!
//! Provides all Java-specific semantic rules, message formatting,
//! and associated types for the multi-language architecture.

use crate::extensions::JavaAnalysisExtensions;
use crate::types::{
    JavaAnnotation, JavaCategory, JavaEvidence, JavaManifestChangeType, JavaReportData,
    JavaSymbolData,
};
use anyhow::{Context, Result};
use semver_analyzer_core::{
    AnalysisReport, AnalysisResult, ApiSurface, Caller, ChangeSubject, ChangedFunction, Language,
    LanguageSemantics, ManifestChange, MessageFormatter, Reference, StructuralChange,
    StructuralChangeType, Symbol, SymbolKind, TestDiff, TestFile, Visibility,
};
use std::path::Path;

// ── Java language type ──────────────────────────────────────────────────

/// The Java language implementation.
///
/// Optionally carries per-ref build configuration for Maven/Gradle
/// builds. When no config is provided, source files are parsed
/// directly with tree-sitter (no build step).
///
/// The `index` field is lazily built on the first call to `find_callers`
/// or `find_references`, using the repo root stored during `extract`.
pub struct Java {
    /// Per-ref build configuration (optional).
    ref_config: Option<crate::worktree::JavaRefBuildConfig>,
    /// Lazily-built cross-file index for call graph walking.
    index: std::sync::Mutex<Option<crate::index::JavaIndex>>,
    /// Repo root path, stored after extraction for index building.
    repo_root: std::sync::Mutex<Option<std::path::PathBuf>>,
}

impl std::fmt::Debug for Java {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Java")
            .field("ref_config", &self.ref_config)
            .field("has_index", &self.index.lock().ok().map(|i| i.is_some()))
            .finish()
    }
}

impl Clone for Java {
    fn clone(&self) -> Self {
        Self {
            ref_config: self.ref_config.clone(),
            // Don't clone the lazily-built index; it will be rebuilt if needed
            index: std::sync::Mutex::new(None),
            repo_root: std::sync::Mutex::new(
                self.repo_root.lock().ok().and_then(|r| r.clone()),
            ),
        }
    }
}

impl Java {
    /// Create a new Java language instance with no build configuration.
    pub fn new() -> Self {
        Self {
            ref_config: None,
            index: std::sync::Mutex::new(None),
            repo_root: std::sync::Mutex::new(None),
        }
    }

    /// Create a Java language instance with per-ref build configuration.
    pub fn with_ref_config(config: crate::worktree::JavaRefBuildConfig) -> Self {
        Self {
            ref_config: Some(config),
            index: std::sync::Mutex::new(None),
            repo_root: std::sync::Mutex::new(None),
        }
    }

    /// Get or build the cross-file index for the repo.
    fn get_or_build_index(&self) -> Result<std::sync::MutexGuard<'_, Option<crate::index::JavaIndex>>> {
        let mut guard = self.index.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            let repo = self
                .repo_root
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(repo_path) = repo {
                match crate::index::JavaIndex::build(&repo_path) {
                    Ok(idx) => {
                        *guard = Some(idx);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to build Java index for call graph");
                    }
                }
            }
        }
        Ok(guard)
    }
}

impl Default for Java {
    fn default() -> Self {
        Self::new()
    }
}

// ── LanguageSemantics implementation ────────────────────────────────────

impl LanguageSemantics<JavaSymbolData> for Java {
    fn is_member_addition_breaking(
        &self,
        container: &Symbol<JavaSymbolData>,
        member: &Symbol<JavaSymbolData>,
    ) -> bool {
        match container.kind {
            SymbolKind::Interface => {
                // Adding a default method to an interface is NOT breaking.
                if member.language_data.is_default {
                    return false;
                }
                // Adding an abstract method to an interface IS breaking
                // (all implementors must now provide it).
                if matches!(member.kind, SymbolKind::Method) {
                    return true;
                }
                // Annotation types: adding a required element (no default value)
                // is breaking. Elements with defaults are not.
                if container.language_data.is_annotation_type
                    && matches!(member.kind, SymbolKind::Method)
                {
                    if let Some(sig) = &member.signature {
                        return sig
                            .parameters
                            .first()
                            .map(|p| !p.has_default)
                            .unwrap_or(true);
                    }
                    return true;
                }
                false
            }
            SymbolKind::Class => {
                // Adding an abstract method to an abstract class IS breaking
                // (all concrete subclasses must implement it).
                if container.is_abstract
                    && member.is_abstract
                    && matches!(member.kind, SymbolKind::Method)
                {
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    fn same_family(&self, a: &Symbol<JavaSymbolData>, b: &Symbol<JavaSymbolData>) -> bool {
        // Same package = same family in Java.
        let pkg_a = java_package(&a.qualified_name);
        let pkg_b = java_package(&b.qualified_name);
        !pkg_a.is_empty() && pkg_a == pkg_b
    }

    fn same_identity(&self, a: &Symbol<JavaSymbolData>, b: &Symbol<JavaSymbolData>) -> bool {
        a.qualified_name == b.qualified_name
    }

    fn visibility_rank(&self, v: Visibility) -> u8 {
        match v {
            Visibility::Private => 0,
            Visibility::Internal => 1, // package-private
            Visibility::Protected => 2,
            Visibility::Public => 3,
            Visibility::Exported => 3,
        }
    }

    fn is_async_wrapper(&self, type_str: &str) -> bool {
        let trimmed = type_str.trim();
        trimmed.starts_with("CompletableFuture<")
            || trimmed.starts_with("CompletionStage<")
            || trimmed.starts_with("Future<")
            || trimmed == "CompletableFuture"
            || trimmed == "CompletionStage"
            || trimmed == "Future"
    }

    fn format_import_change(&self, symbol: &str, old_path: &str, new_path: &str) -> String {
        format!(
            "replace `import {}.{}` with `import {}.{}`",
            old_path, symbol, new_path, symbol,
        )
    }

    fn should_skip_symbol(&self, sym: &Symbol<JavaSymbolData>) -> bool {
        sym.name == "package-info"
    }

    fn member_label(&self) -> &'static str {
        "methods"
    }

    fn canonical_name_for_relocation(&self, qualified_name: &str) -> String {
        if let Some(dot_pos) = qualified_name.rfind('.') {
            qualified_name[dot_pos + 1..].to_string()
        } else {
            qualified_name.to_string()
        }
    }

    fn diff_language_data(
        &self,
        old: &Symbol<JavaSymbolData>,
        new: &Symbol<JavaSymbolData>,
    ) -> Vec<StructuralChange> {
        let mut changes = Vec::new();
        let old_data = &old.language_data;
        let new_data = &new.language_data;

        // ── Annotation changes ──────────────────────────────────────────
        diff_annotations(old, old_data, new_data, &mut changes);

        // ── Throws clause changes ───────────────────────────────────────
        diff_throws(old, old_data, new_data, &mut changes);

        // ── Final modifier changes ──────────────────────────────────────
        if old_data.is_final != new_data.is_final {
            if new_data.is_final {
                changes.push(lang_change(
                    old,
                    StructuralChangeType::Changed(ChangeSubject::Modifier {
                        modifier: "final".into(),
                    }),
                    Some("non-final".into()),
                    Some("final".into()),
                    format!("Class `{}` is now final and cannot be extended", old.name),
                    true,
                ));
            } else {
                changes.push(lang_change(
                    old,
                    StructuralChangeType::Changed(ChangeSubject::Modifier {
                        modifier: "final".into(),
                    }),
                    Some("final".into()),
                    Some("non-final".into()),
                    format!(
                        "Class `{}` is no longer final and can now be extended",
                        old.name
                    ),
                    false,
                ));
            }
        }

        // ── Sealed modifier changes ─────────────────────────────────────
        if old_data.is_sealed != new_data.is_sealed {
            if new_data.is_sealed {
                changes.push(lang_change(
                    old,
                    StructuralChangeType::Changed(ChangeSubject::Modifier {
                        modifier: "sealed".into(),
                    }),
                    Some("non-sealed".into()),
                    Some("sealed".into()),
                    format!(
                        "Class `{}` is now sealed — only permitted subtypes can extend it",
                        old.name
                    ),
                    true,
                ));
            } else {
                changes.push(lang_change(
                    old,
                    StructuralChangeType::Changed(ChangeSubject::Modifier {
                        modifier: "sealed".into(),
                    }),
                    Some("sealed".into()),
                    Some("non-sealed".into()),
                    format!("Class `{}` is no longer sealed", old.name),
                    false,
                ));
            }
        }

        // ── Non-sealed modifier changes ────────────────────────────────
        if old_data.is_non_sealed != new_data.is_non_sealed {
            if !new_data.is_non_sealed && new_data.is_sealed {
                // non-sealed → sealed is handled by the sealed change above
            } else if new_data.is_non_sealed {
                changes.push(lang_change(
                    old,
                    StructuralChangeType::Changed(ChangeSubject::Modifier {
                        modifier: "non-sealed".into(),
                    }),
                    Some("sealed/final".into()),
                    Some("non-sealed".into()),
                    format!(
                        "Class `{}` is now non-sealed — unrestricted extension is allowed",
                        old.name
                    ),
                    false,
                ));
            }
        }

        // ── Synchronized modifier changes ───────────────────────────────
        if old_data.is_synchronized != new_data.is_synchronized {
            if !new_data.is_synchronized {
                changes.push(lang_change(
                    old,
                    StructuralChangeType::Removed(ChangeSubject::Modifier {
                        modifier: "synchronized".into(),
                    }),
                    Some("synchronized".into()),
                    None,
                    format!(
                        "Method `{}` is no longer synchronized — callers relying on thread safety may break",
                        old.name
                    ),
                    true,
                ));
            } else {
                changes.push(lang_change(
                    old,
                    StructuralChangeType::Added(ChangeSubject::Modifier {
                        modifier: "synchronized".into(),
                    }),
                    None,
                    Some("synchronized".into()),
                    format!(
                        "Method `{}` is now synchronized",
                        old.name
                    ),
                    false,
                ));
            }
        }

        // ── Transient modifier changes ──────────────────────────────────
        if old_data.is_transient != new_data.is_transient {
            let (before, after, desc, breaking) = if new_data.is_transient {
                (None, Some("transient".into()),
                 format!("Field `{}` is now transient — excluded from serialization", old.name),
                 true)
            } else {
                (Some("transient".into()), None,
                 format!("Field `{}` is no longer transient — now included in serialization", old.name),
                 true)
            };
            changes.push(lang_change(
                old,
                StructuralChangeType::Changed(ChangeSubject::Modifier {
                    modifier: "transient".into(),
                }),
                before, after, desc, breaking,
            ));
        }

        // ── Volatile modifier changes ───────────────────────────────────
        if old_data.is_volatile != new_data.is_volatile {
            if !new_data.is_volatile {
                changes.push(lang_change(
                    old,
                    StructuralChangeType::Removed(ChangeSubject::Modifier {
                        modifier: "volatile".into(),
                    }),
                    Some("volatile".into()),
                    None,
                    format!(
                        "Field `{}` is no longer volatile — memory visibility guarantee removed",
                        old.name
                    ),
                    true,
                ));
            } else {
                changes.push(lang_change(
                    old,
                    StructuralChangeType::Added(ChangeSubject::Modifier {
                        modifier: "volatile".into(),
                    }),
                    None,
                    Some("volatile".into()),
                    format!("Field `{}` is now volatile", old.name),
                    false,
                ));
            }
        }

        // ── Native modifier changes ─────────────────────────────────────
        if old_data.is_native != new_data.is_native {
            if !new_data.is_native {
                changes.push(lang_change(
                    old,
                    StructuralChangeType::Removed(ChangeSubject::Modifier {
                        modifier: "native".into(),
                    }),
                    Some("native".into()),
                    None,
                    format!(
                        "Method `{}` is no longer native — JNI consumers will break",
                        old.name
                    ),
                    true,
                ));
            } else {
                changes.push(lang_change(
                    old,
                    StructuralChangeType::Added(ChangeSubject::Modifier {
                        modifier: "native".into(),
                    }),
                    None,
                    Some("native".into()),
                    format!("Method `{}` is now native", old.name),
                    false,
                ));
            }
        }

        // ── Permits list changes (for sealed classes) ───────────────────
        if old_data.is_sealed && new_data.is_sealed && old_data.permits != new_data.permits {
            let removed: Vec<&str> = old_data
                .permits
                .iter()
                .filter(|p| !new_data.permits.contains(p))
                .map(|s| s.as_str())
                .collect();
            let added: Vec<&str> = new_data
                .permits
                .iter()
                .filter(|p| !old_data.permits.contains(p))
                .map(|s| s.as_str())
                .collect();

            if !removed.is_empty() {
                changes.push(lang_change(
                    old,
                    StructuralChangeType::Changed(ChangeSubject::Modifier {
                        modifier: "permits".into(),
                    }),
                    Some(old_data.permits.join(", ")),
                    Some(new_data.permits.join(", ")),
                    format!(
                        "Sealed class `{}` no longer permits: {}",
                        old.name,
                        removed.join(", ")
                    ),
                    true,
                ));
            }
            if !added.is_empty() {
                changes.push(lang_change(
                    old,
                    StructuralChangeType::Changed(ChangeSubject::Modifier {
                        modifier: "permits".into(),
                    }),
                    Some(old_data.permits.join(", ")),
                    Some(new_data.permits.join(", ")),
                    format!(
                        "Sealed class `{}` now additionally permits: {}",
                        old.name,
                        added.join(", ")
                    ),
                    false,
                ));
            }
        }

        changes
    }
}

// ── diff_language_data helpers ──────────────────────────────────────────

fn diff_annotations(
    sym: &Symbol<JavaSymbolData>,
    old_data: &JavaSymbolData,
    new_data: &JavaSymbolData,
    changes: &mut Vec<StructuralChange>,
) {
    use std::collections::HashMap;

    let old_by_name: HashMap<&str, &JavaAnnotation> = old_data
        .annotations
        .iter()
        .map(|a| (a.name.as_str(), a))
        .collect();
    let new_by_name: HashMap<&str, &JavaAnnotation> = new_data
        .annotations
        .iter()
        .map(|a| (a.name.as_str(), a))
        .collect();

    for (name, old_ann) in &old_by_name {
        if !new_by_name.contains_key(name) {
            let is_breaking = is_annotation_removal_breaking(name);
            changes.push(lang_change(
                sym,
                StructuralChangeType::Removed(ChangeSubject::Modifier {
                    modifier: format!("@{}", name),
                }),
                Some(format_annotation(old_ann)),
                None,
                format!("Annotation `@{}` removed from `{}`", name, sym.name),
                is_breaking,
            ));
        }
    }

    for (name, new_ann) in &new_by_name {
        if !old_by_name.contains_key(name) {
            changes.push(lang_change(
                sym,
                StructuralChangeType::Added(ChangeSubject::Modifier {
                    modifier: format!("@{}", name),
                }),
                None,
                Some(format_annotation(new_ann)),
                format!("Annotation `@{}` added to `{}`", name, sym.name),
                false,
            ));
        }
    }

    for (name, old_ann) in &old_by_name {
        if let Some(new_ann) = new_by_name.get(name) {
            if old_ann.attributes != new_ann.attributes {
                changes.push(lang_change(
                    sym,
                    StructuralChangeType::Changed(ChangeSubject::Modifier {
                        modifier: format!("@{}", name),
                    }),
                    Some(format_annotation(old_ann)),
                    Some(format_annotation(new_ann)),
                    format!(
                        "Annotation `@{}` on `{}` changed attributes",
                        name, sym.name
                    ),
                    is_annotation_change_breaking(name),
                ));
            }
        }
    }
}

fn diff_throws(
    sym: &Symbol<JavaSymbolData>,
    old_data: &JavaSymbolData,
    new_data: &JavaSymbolData,
    changes: &mut Vec<StructuralChange>,
) {
    if old_data.throws == new_data.throws {
        return;
    }

    let added: Vec<&str> = new_data
        .throws
        .iter()
        .filter(|t| !old_data.throws.contains(t))
        .map(|s| s.as_str())
        .collect();

    let removed: Vec<&str> = old_data
        .throws
        .iter()
        .filter(|t| !new_data.throws.contains(t))
        .map(|s| s.as_str())
        .collect();

    if !added.is_empty() {
        changes.push(lang_change(
            sym,
            StructuralChangeType::Added(ChangeSubject::Modifier {
                modifier: "throws".into(),
            }),
            if old_data.throws.is_empty() {
                None
            } else {
                Some(old_data.throws.join(", "))
            },
            Some(new_data.throws.join(", ")),
            format!("Method `{}` now throws: {}", sym.name, added.join(", ")),
            true,
        ));
    }

    if !removed.is_empty() {
        changes.push(lang_change(
            sym,
            StructuralChangeType::Removed(ChangeSubject::Modifier {
                modifier: "throws".into(),
            }),
            Some(old_data.throws.join(", ")),
            if new_data.throws.is_empty() {
                None
            } else {
                Some(new_data.throws.join(", "))
            },
            format!(
                "Method `{}` no longer throws: {}",
                sym.name,
                removed.join(", ")
            ),
            false,
        ));
    }
}

fn is_annotation_removal_breaking(name: &str) -> bool {
    matches!(
        name,
        "Bean"
            | "Component"
            | "Service"
            | "Repository"
            | "Controller"
            | "RestController"
            | "Configuration"
            | "ConfigurationProperties"
            | "Autowired"
            | "ConditionalOnClass"
            | "ConditionalOnMissingBean"
            | "ConditionalOnProperty"
    )
}

fn is_annotation_change_breaking(name: &str) -> bool {
    matches!(
        name,
        "ConfigurationProperties"
            | "RequestMapping"
            | "GetMapping"
            | "PostMapping"
            | "PutMapping"
            | "DeleteMapping"
            | "PatchMapping"
            | "ConditionalOnProperty"
            | "ConditionalOnClass"
    )
}

fn format_annotation(ann: &JavaAnnotation) -> String {
    if ann.attributes.is_empty() {
        format!("@{}", ann.name)
    } else {
        let attrs: Vec<String> = ann
            .attributes
            .iter()
            .map(|(k, v)| {
                if k == "value" {
                    v.clone()
                } else {
                    format!("{} = {}", k, v)
                }
            })
            .collect();
        format!("@{}({})", ann.name, attrs.join(", "))
    }
}

fn lang_change(
    sym: &Symbol<JavaSymbolData>,
    change_type: StructuralChangeType,
    before: Option<String>,
    after: Option<String>,
    description: String,
    is_breaking: bool,
) -> StructuralChange {
    StructuralChange {
        symbol: sym.name.clone(),
        qualified_name: sym.qualified_name.clone(),
        kind: sym.kind,
        package: sym.package.clone(),
        change_type,
        before,
        after,
        description,
        is_breaking,
        impact: None,
        migration_target: None,
    }
}

// ── MessageFormatter implementation ─────────────────────────────────────

impl MessageFormatter for Java {
    fn describe(&self, change: &StructuralChange) -> String {
        change.description.clone()
    }
}

// ── Language implementation ─────────────────────────────────────────────

impl Language for Java {
    type SymbolData = JavaSymbolData;
    type Category = JavaCategory;
    type ManifestChangeType = JavaManifestChangeType;
    type Evidence = JavaEvidence;
    type ReportData = JavaReportData;
    type AnalysisExtensions = JavaAnalysisExtensions;

    const RENAMEABLE_SYMBOL_KINDS: &'static [SymbolKind] =
        &[SymbolKind::Interface, SymbolKind::Class, SymbolKind::Enum];
    const NAME: &'static str = "java";
    const MANIFEST_FILES: &'static [&'static str] = &[
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "gradle/libraries.gradle",
        "gradle/base-information.gradle",
    ];
    const SOURCE_FILE_PATTERNS: &'static [&'static str] = &["*.java"];

    fn discover_package_manifests(repo: &Path, git_ref: &str) -> Vec<(String, String)> {
        discover_submodule_manifests(repo, git_ref)
    }

    fn extract(
        &self,
        repo: &Path,
        git_ref: &str,
        degradation: Option<&semver_analyzer_core::diagnostics::DegradationTracker>,
    ) -> Result<ApiSurface<JavaSymbolData>> {
        use semver_analyzer_core::error::DiagnoseExt;

        // Store repo root for later index building (find_callers/find_references)
        if let Ok(mut root) = self.repo_root.lock() {
            *root = Some(repo.to_path_buf());
        }

        let config = self
            .ref_config
            .clone()
            .unwrap_or_default();

        let guard = crate::worktree::JavaWorktreeGuard::new(repo, git_ref, &config)
            .with_diagnosis(
                "Check that the repository path is correct and the git ref exists. \
                 Run 'git tag -l' or 'git branch -a' to verify."
            )?;

        // Record build warnings as degradation
        for warning in guard.warnings() {
            if let Some(tracker) = degradation {
                match warning {
                    crate::worktree::ExtractionWarning::BuildFailedSourceOnly { build_error } => {
                        tracker.record(
                            "TD",
                            format!("Java build failed at ref '{}': {}", git_ref, build_error),
                            "Extraction continues with source files only. \
                             Generated sources and resolved dependencies are unavailable.",
                        );
                    }
                    crate::worktree::ExtractionWarning::PartialBuild { succeeded, failed } => {
                        tracker.record(
                            "TD",
                            format!(
                                "Partial build at ref '{}': {} modules succeeded, {} failed",
                                git_ref, succeeded, failed
                            ),
                            "Some modules may have incomplete API surfaces.",
                        );
                    }
                }
            }
        }

        let mut extractor =
            crate::extract::JavaExtractor::new().context("Failed to create Java extractor")?;
        let surface = extractor.extract_from_dir(guard.path())?;

        // Record degradation if extraction produced zero symbols
        if surface.symbols.is_empty() {
            if let Some(tracker) = degradation {
                tracker.record(
                    "TD",
                    format!("Java extraction at ref '{}' produced 0 symbols", git_ref),
                    "API surface may be incomplete. Verify the git ref contains Java source files.",
                );
            }
        }

        Ok(surface)
    }

    fn parse_changed_functions(
        &self,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<Vec<ChangedFunction>> {
        let parser = crate::diff_parser::JavaDiffParser::new();
        parser.parse_changed_functions(repo, from_ref, to_ref)
    }

    /// Find all callers of a method across the project.
    ///
    /// Uses a lazily-built cross-file index to scan all `.java` files
    /// for method invocations matching `symbol_name`. The index is built
    /// on the first call using the repo root stored during `extract`.
    fn find_callers(&self, file: &Path, symbol_name: &str) -> Result<Vec<Caller>> {
        let guard = self.get_or_build_index()?;
        match guard.as_ref() {
            Some(index) => index.find_callers(file, symbol_name),
            None => Ok(Vec::new()),
        }
    }

    /// Find all references to a symbol across the project.
    ///
    /// Scans all indexed files for imports, type references, and method
    /// invocations matching `symbol_name`.
    fn find_references(&self, file: &Path, symbol_name: &str) -> Result<Vec<Reference>> {
        let guard = self.get_or_build_index()?;
        match guard.as_ref() {
            Some(index) => index.find_references(file, symbol_name),
            None => Ok(Vec::new()),
        }
    }

    fn find_tests(&self, repo: &Path, source_file: &Path) -> Result<Vec<TestFile>> {
        let analyzer = crate::test_analyzer::JavaTestAnalyzer::new();
        analyzer.find_tests(repo, source_file)
    }

    fn diff_test_assertions(
        &self,
        repo: &Path,
        test_file: &TestFile,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<TestDiff> {
        let analyzer = crate::test_analyzer::JavaTestAnalyzer::new();
        analyzer.diff_test_assertions(repo, test_file, from_ref, to_ref)
    }

    fn diff_manifest_content(old: &str, new: &str) -> Vec<ManifestChange<Self>> {
        crate::manifest::diff_manifest_content(old, new)
    }

    fn should_exclude_from_analysis(path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        let basename = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();

        path_str.contains("/src/test/")
            || path_str.contains("/test/")
            || path_str.contains("/target/")
            || path_str.starts_with("target/")
            || path_str.contains("/build/")
            || path_str.starts_with("build/")
            || path_str.contains("/generated/")
            || path_str.contains("/generated-sources/")
            || basename.ends_with("Test.java")
            || basename.ends_with("Tests.java")
            || basename.ends_with("IT.java")
            || basename.ends_with("ITCase.java")
            || basename == "package-info.java"
    }

    fn build_report(
        &self,
        results: &AnalysisResult<Self>,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
    ) -> AnalysisReport<Self> {
        crate::report::build_report(results, repo, from_ref, to_ref)
    }

    fn display_name(&self, qualified_name: &str) -> String {
        let parts: Vec<&str> = qualified_name.split('.').collect();
        if parts.len() <= 2 {
            return qualified_name.to_string();
        }
        for (i, part) in parts.iter().enumerate() {
            if part.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                return parts[i..].join(".");
            }
        }
        qualified_name.to_string()
    }

    fn llm_categories(&self) -> Vec<semver_analyzer_core::LlmCategoryDefinition> {
        use semver_analyzer_core::LlmCategoryDefinition;
        vec![
            LlmCategoryDefinition {
                id: "annotation_change".into(),
                label: "Annotation changes".into(),
                description: "Changed annotations (@Deprecated, @Override, @Nullable, \
                    custom annotations), added/removed annotation elements, changed \
                    retention or target"
                    .into(),
            },
            LlmCategoryDefinition {
                id: "exception_handling".into(),
                label: "Exception changes".into(),
                description: "Changed throws clauses, different exception types thrown, \
                    removed or added checked exceptions, changed error handling behavior"
                    .into(),
            },
            LlmCategoryDefinition {
                id: "method_signature".into(),
                label: "Method signature changes".into(),
                description: "Return type changes, parameter type/count changes, \
                    generic type parameter changes, varargs changes"
                    .into(),
            },
            LlmCategoryDefinition {
                id: "access_control".into(),
                label: "Access control changes".into(),
                description: "Visibility modifier changes (public → protected, \
                    protected → package-private), added/removed final on methods"
                    .into(),
            },
            LlmCategoryDefinition {
                id: "type_hierarchy".into(),
                label: "Type hierarchy changes".into(),
                description: "Changed extends/implements, sealed/permits changes, \
                    added/removed final on classes, interface to abstract class or \
                    vice versa"
                    .into(),
            },
            LlmCategoryDefinition {
                id: "default_impl".into(),
                label: "Default implementation changes".into(),
                description: "Added/removed default methods on interfaces, changed \
                    default method behavior, abstract method additions"
                    .into(),
            },
            LlmCategoryDefinition {
                id: "behavioral".into(),
                label: "Behavioral changes".into(),
                description: "Changed method body logic, different return values for \
                    same inputs, changed side effects, altered state transitions, \
                    threading/synchronization changes"
                    .into(),
            },
        ]
    }

    fn run_extended_analysis(
        &self,
        params: &semver_analyzer_core::ExtendedAnalysisParams,
    ) -> Result<JavaAnalysisExtensions> {
        let sd_result = crate::sd_pipeline::run_java_sd(
            &params.repo,
            &params.from_ref,
            &params.to_ref,
            params.from_worktree_path.as_deref(),
            params.to_worktree_path.as_deref(),
        )?;

        Ok(JavaAnalysisExtensions {
            sd_result: Some(sd_result),
        })
    }

    fn extensions_log_summary(&self, extensions: &JavaAnalysisExtensions) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(ref sd) = extensions.sd_result {
            let total = sd.source_level_changes.len() + sd.module_changes.len();
            let breaking = sd.source_level_changes.iter().filter(|c| c.is_breaking).count()
                + sd.module_changes.iter().filter(|c| c.is_breaking).count();
            lines.push(format!(
                "SD: {} source-level changes ({} breaking)",
                total, breaking
            ));
            lines.push(format!(
                "SD: {} class profiles, {} inheritance entries",
                sd.new_profiles.len(),
                sd.inheritance_summary.len()
            ));
            if !sd.migration_mappings.is_empty() {
                let total_methods: usize =
                    sd.migration_mappings.iter().map(|m| m.method_mappings.len()).sum();
                lines.push(format!(
                    "SD: {} migration mappings ({} method pairs) from {} examples",
                    sd.migration_mappings.len(),
                    total_methods,
                    sd.migration_examples.len()
                ));
            }
        }
        lines
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Extract the package portion of a Java qualified name.
fn java_package(qualified_name: &str) -> &str {
    let parts: Vec<&str> = qualified_name.split('.').collect();
    for (i, part) in parts.iter().enumerate() {
        if part.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            if i == 0 {
                return "";
            }
            let end = parts[..i].iter().map(|p| p.len()).sum::<usize>() + (i - 1);
            return &qualified_name[..end];
        }
    }
    qualified_name
}

/// Discover submodule manifest files (build.gradle / pom.xml) across the repo tree.
///
/// Uses `git ls-tree` to find all build files without requiring a worktree checkout.
/// Returns `(path, module_name)` pairs where module_name is extracted from the
/// parent directory name.
fn discover_submodule_manifests(repo: &Path, git_ref: &str) -> Vec<(String, String)> {
    let output = std::process::Command::new("git")
        .args(["ls-tree", "-r", "--name-only", git_ref])
        .current_dir(repo)
        .output();

    let output = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return Vec::new(),
    };

    let manifest_names = ["build.gradle", "build.gradle.kts", "pom.xml"];
    let mut results = Vec::new();

    for line in output.lines() {
        let path = line.trim();
        // Skip root-level files (already in MANIFEST_FILES)
        if !path.contains('/') {
            continue;
        }
        let file_name = path.rsplit('/').next().unwrap_or("");
        if !manifest_names.contains(&file_name) {
            continue;
        }
        // Skip buildSrc, test fixtures, documentation examples
        if path.contains("buildSrc")
            || path.contains("test-fixtures")
            || path.contains("documentation/")
            || path.contains("userguide/")
        {
            continue;
        }
        // Extract module name from path (parent directory)
        let module = path
            .rsplit_once('/')
            .map(|(parent, _)| parent.rsplit('/').next().unwrap_or(parent))
            .unwrap_or(path)
            .to_string();

        results.push((path.to_string(), module));
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_java_package() {
        assert_eq!(
            java_package("org.springframework.boot.WebApp"),
            "org.springframework.boot"
        );
        assert_eq!(
            java_package("org.springframework.boot.WebApp.doThing"),
            "org.springframework.boot"
        );
        assert_eq!(java_package("WebApp"), "");
        assert_eq!(java_package("com.example.Main"), "com.example");
    }

    #[test]
    fn test_display_name() {
        let java = Java::new();
        assert_eq!(
            java.display_name("org.springframework.boot.WebApp.doThing"),
            "WebApp.doThing"
        );
        assert_eq!(
            java.display_name("org.springframework.boot.WebApp"),
            "WebApp"
        );
        assert_eq!(java.display_name("WebApp"), "WebApp");
        assert_eq!(java.display_name("com.example.Main"), "Main");
    }

    #[test]
    fn test_visibility_rank() {
        let java = Java::new();
        assert!(
            java.visibility_rank(Visibility::Public) > java.visibility_rank(Visibility::Protected)
        );
        assert!(
            java.visibility_rank(Visibility::Protected)
                > java.visibility_rank(Visibility::Internal)
        );
        assert!(
            java.visibility_rank(Visibility::Internal) > java.visibility_rank(Visibility::Private)
        );
    }

    #[test]
    fn test_is_async_wrapper() {
        let java = Java::new();
        assert!(java.is_async_wrapper("CompletableFuture<String>"));
        assert!(java.is_async_wrapper("CompletionStage<Void>"));
        assert!(java.is_async_wrapper("Future<Integer>"));
        assert!(!java.is_async_wrapper("String"));
        assert!(!java.is_async_wrapper("List<CompletableFuture<String>>"));
    }

    #[test]
    fn test_should_skip_symbol() {
        let java = Java::new();
        let mut sym = Symbol::new(
            "package-info",
            "com.example.package-info",
            SymbolKind::Class,
            Visibility::Public,
            "package-info.java",
            1,
        );
        sym.language_data = JavaSymbolData::default();
        assert!(java.should_skip_symbol(&sym));

        let mut sym2 = Symbol::new(
            "Main",
            "com.example.Main",
            SymbolKind::Class,
            Visibility::Public,
            "Main.java",
            1,
        );
        sym2.language_data = JavaSymbolData::default();
        assert!(!java.should_skip_symbol(&sym2));
    }

    #[test]
    fn test_is_member_addition_breaking_interface_abstract() {
        let java = Java::new();
        let mut iface = Symbol::new(
            "Runnable",
            "java.lang.Runnable",
            SymbolKind::Interface,
            Visibility::Public,
            "Runnable.java",
            1,
        );
        iface.language_data = JavaSymbolData::default();

        let mut method = Symbol::new(
            "run",
            "java.lang.Runnable.run",
            SymbolKind::Method,
            Visibility::Public,
            "Runnable.java",
            5,
        );
        method.language_data = JavaSymbolData::default();
        assert!(java.is_member_addition_breaking(&iface, &method));
    }

    #[test]
    fn test_is_member_addition_breaking_interface_default() {
        let java = Java::new();
        let mut iface = Symbol::new(
            "Collection",
            "java.util.Collection",
            SymbolKind::Interface,
            Visibility::Public,
            "Collection.java",
            1,
        );
        iface.language_data = JavaSymbolData::default();

        let mut method = Symbol::new(
            "stream",
            "java.util.Collection.stream",
            SymbolKind::Method,
            Visibility::Public,
            "Collection.java",
            10,
        );
        method.language_data = JavaSymbolData {
            is_default: true,
            ..Default::default()
        };
        assert!(!java.is_member_addition_breaking(&iface, &method));
    }

    #[test]
    fn test_is_member_addition_breaking_abstract_class() {
        let java = Java::new();
        let mut abs_class = Symbol::new(
            "AbstractList",
            "java.util.AbstractList",
            SymbolKind::Class,
            Visibility::Public,
            "AbstractList.java",
            1,
        );
        abs_class.is_abstract = true;
        abs_class.language_data = JavaSymbolData::default();

        let mut method = Symbol::new(
            "size",
            "java.util.AbstractList.size",
            SymbolKind::Method,
            Visibility::Public,
            "AbstractList.java",
            10,
        );
        method.is_abstract = true;
        method.language_data = JavaSymbolData::default();
        assert!(java.is_member_addition_breaking(&abs_class, &method));

        let mut concrete = Symbol::new(
            "isEmpty",
            "java.util.AbstractList.isEmpty",
            SymbolKind::Method,
            Visibility::Public,
            "AbstractList.java",
            15,
        );
        concrete.language_data = JavaSymbolData::default();
        assert!(!java.is_member_addition_breaking(&abs_class, &concrete));
    }

    #[test]
    fn test_same_family() {
        let java = Java::new();
        let mut a = Symbol::new(
            "Foo",
            "com.example.service.Foo",
            SymbolKind::Class,
            Visibility::Public,
            "Foo.java",
            1,
        );
        a.language_data = JavaSymbolData::default();
        let mut b = Symbol::new(
            "Bar",
            "com.example.service.Bar",
            SymbolKind::Class,
            Visibility::Public,
            "Bar.java",
            1,
        );
        b.language_data = JavaSymbolData::default();
        let mut c = Symbol::new(
            "Baz",
            "com.example.other.Baz",
            SymbolKind::Class,
            Visibility::Public,
            "Baz.java",
            1,
        );
        c.language_data = JavaSymbolData::default();

        assert!(java.same_family(&a, &b));
        assert!(!java.same_family(&a, &c));
    }

    #[test]
    fn test_should_exclude_from_analysis() {
        assert!(Java::should_exclude_from_analysis(Path::new(
            "src/test/java/com/example/FooTest.java"
        )));
        assert!(Java::should_exclude_from_analysis(Path::new(
            "target/classes/Foo.class"
        )));
        assert!(Java::should_exclude_from_analysis(Path::new(
            "build/generated/Foo.java"
        )));
        // module-info.java is now included for module system analysis
        assert!(!Java::should_exclude_from_analysis(Path::new(
            "src/main/java/module-info.java"
        )));
        assert!(!Java::should_exclude_from_analysis(Path::new(
            "src/main/java/com/example/Foo.java"
        )));
    }

    // ── diff_language_data tests ─────────────────────────────────────

    fn make_sym(
        name: &str,
        qname: &str,
        kind: SymbolKind,
        data: JavaSymbolData,
    ) -> Symbol<JavaSymbolData> {
        let mut s = Symbol::new(name, qname, kind, Visibility::Public, "Test.java", 1);
        s.language_data = data;
        s
    }

    #[test]
    fn test_diff_language_data_annotation_added() {
        let java = Java::new();
        let old = make_sym(
            "Foo",
            "com.example.Foo",
            SymbolKind::Class,
            JavaSymbolData::default(),
        );
        let new = make_sym(
            "Foo",
            "com.example.Foo",
            SymbolKind::Class,
            JavaSymbolData {
                annotations: vec![JavaAnnotation {
                    name: "Deprecated".into(),
                    qualified_name: Some("java.lang.Deprecated".into()),
                    attributes: vec![],
                }],
                ..Default::default()
            },
        );

        let changes = java.diff_language_data(&old, &new);
        assert_eq!(changes.len(), 1);
        assert!(!changes[0].is_breaking);
        assert!(changes[0].description.contains("@Deprecated"));
        assert!(changes[0].description.contains("added"));
    }

    #[test]
    fn test_diff_language_data_annotation_removed_breaking() {
        let java = Java::new();
        let old = make_sym(
            "dataSource",
            "com.example.Config.dataSource",
            SymbolKind::Method,
            JavaSymbolData {
                annotations: vec![JavaAnnotation {
                    name: "Bean".into(),
                    qualified_name: None,
                    attributes: vec![],
                }],
                ..Default::default()
            },
        );
        let new = make_sym(
            "dataSource",
            "com.example.Config.dataSource",
            SymbolKind::Method,
            JavaSymbolData::default(),
        );

        let changes = java.diff_language_data(&old, &new);
        assert_eq!(changes.len(), 1);
        assert!(changes[0].is_breaking);
    }

    #[test]
    fn test_diff_language_data_throws_added() {
        let java = Java::new();
        let old = make_sym(
            "read",
            "com.example.Reader.read",
            SymbolKind::Method,
            JavaSymbolData::default(),
        );
        let new = make_sym(
            "read",
            "com.example.Reader.read",
            SymbolKind::Method,
            JavaSymbolData {
                throws: vec!["IOException".into()],
                ..Default::default()
            },
        );

        let changes = java.diff_language_data(&old, &new);
        assert_eq!(changes.len(), 1);
        assert!(changes[0].is_breaking);
        assert!(changes[0].description.contains("IOException"));
    }

    #[test]
    fn test_diff_language_data_class_became_final() {
        let java = Java::new();
        let old = make_sym(
            "Foo",
            "com.example.Foo",
            SymbolKind::Class,
            JavaSymbolData::default(),
        );
        let new = make_sym(
            "Foo",
            "com.example.Foo",
            SymbolKind::Class,
            JavaSymbolData {
                is_final: true,
                ..Default::default()
            },
        );

        let changes = java.diff_language_data(&old, &new);
        assert_eq!(changes.len(), 1);
        assert!(changes[0].is_breaking);
        assert!(changes[0].description.contains("final"));
    }

    #[test]
    fn test_diff_language_data_class_became_sealed() {
        let java = Java::new();
        let old = make_sym(
            "Shape",
            "com.example.Shape",
            SymbolKind::Class,
            JavaSymbolData::default(),
        );
        let new = make_sym(
            "Shape",
            "com.example.Shape",
            SymbolKind::Class,
            JavaSymbolData {
                is_sealed: true,
                permits: vec!["Circle".into(), "Rectangle".into()],
                ..Default::default()
            },
        );

        let changes = java.diff_language_data(&old, &new);
        assert_eq!(changes.len(), 1);
        assert!(changes[0].is_breaking);
        assert!(changes[0].description.contains("sealed"));
    }

    #[test]
    fn test_diff_language_data_no_changes() {
        let java = Java::new();
        let data = JavaSymbolData {
            annotations: vec![JavaAnnotation {
                name: "Override".into(),
                qualified_name: None,
                attributes: vec![],
            }],
            ..Default::default()
        };
        let old = make_sym(
            "foo",
            "com.example.Foo.foo",
            SymbolKind::Method,
            data.clone(),
        );
        let new = make_sym("foo", "com.example.Foo.foo", SymbolKind::Method, data);

        let changes = java.diff_language_data(&old, &new);
        assert!(changes.is_empty());
    }

    #[test]
    fn test_canonical_name_strips_package() {
        let java = Java::new();
        assert_eq!(
            java.canonical_name_for_relocation(
                "org.springframework.boot.autoconfigure.cache.JCacheManagerCustomizer"
            ),
            "JCacheManagerCustomizer"
        );
    }

    #[test]
    fn test_relocation_detection_java() {
        use semver_analyzer_core::diff::diff_surfaces_with_semantics;

        let old_sym = {
            let mut s = Symbol::new(
                "JCacheManagerCustomizer",
                "org.springframework.boot.autoconfigure.cache.JCacheManagerCustomizer",
                SymbolKind::Interface,
                Visibility::Public,
                "old/JCacheManagerCustomizer.java",
                1,
            );
            s.package = Some("org.springframework.boot.autoconfigure.cache".into());
            s.import_path =
                Some("org.springframework.boot.autoconfigure.cache.JCacheManagerCustomizer".into());
            s.language_data = JavaSymbolData::default();
            s
        };

        let new_sym = {
            let mut s = Symbol::new(
                "JCacheManagerCustomizer",
                "org.springframework.boot.cache.autoconfigure.JCacheManagerCustomizer",
                SymbolKind::Interface,
                Visibility::Public,
                "new/JCacheManagerCustomizer.java",
                1,
            );
            s.package = Some("org.springframework.boot.cache.autoconfigure".into());
            s.import_path =
                Some("org.springframework.boot.cache.autoconfigure.JCacheManagerCustomizer".into());
            s.language_data = JavaSymbolData::default();
            s
        };

        let old_surface = semver_analyzer_core::ApiSurface {
            symbols: vec![old_sym],
        };
        let new_surface = semver_analyzer_core::ApiSurface {
            symbols: vec![new_sym],
        };

        let java = Java::new();
        let changes = diff_surfaces_with_semantics(&old_surface, &new_surface, &java);

        assert!(!changes.is_empty());
        let has_relocation = changes.iter().any(|c| {
            matches!(
                c.change_type,
                semver_analyzer_core::StructuralChangeType::Relocated { .. }
            )
        });
        assert!(has_relocation, "Expected a Relocated change");
    }
}
