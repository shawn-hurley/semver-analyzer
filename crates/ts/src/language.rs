//! TypeScript `Language` trait implementation.
//!
//! Provides all TypeScript/React-specific semantic rules, message formatting,
//! and associated types for the multi-language architecture.
//!
//! This module extracts language-specific logic that currently lives in
//! `core/diff/compare.rs`, `core/diff/helpers.rs`, `core/diff/migration.rs`,
//! and `core/diff/mod.rs` into a trait implementation that the diff engine
//! can call through the `LanguageSemantics` and `MessageFormatter` traits.

use anyhow::Result;
use semver_analyzer_core::{
    AnalysisReport, AnalysisResult, ApiSurface, BehavioralChangeKind, BodyAnalysisResult,
    BodyAnalysisSemantics, Caller, ChangedFunction, EvidenceType, ExpectedChild,
    ExtendedAnalysisParams, HierarchySemantics, Language, LanguageSemantics, ManifestChange,
    MessageFormatter, Reference, RenameSemantics, StructuralChange, StructuralChangeType, Symbol,
    SymbolKind, TestDiff, TestFile, Visibility,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::extensions::TsAnalysisExtensions;
use crate::TsSymbolData;

// ── TypeScript language type ────────────────────────────────────────────

/// The TypeScript language implementation.
#[derive(Debug, Clone)]
pub struct TypeScript {
    build_command: Option<String>,
}

impl TypeScript {
    pub fn new(build_command: Option<String>) -> Self {
        Self { build_command }
    }
}

impl Default for TypeScript {
    fn default() -> Self {
        Self {
            build_command: Some("yarn build".to_string()),
        }
    }
}

// ── Associated types ────────────────────────────────────────────────────

/// Behavioral change categories for TypeScript/React analysis.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TsCategory {
    /// Changed element types, wrapper elements, component nesting.
    DomStructure,
    /// CSS class name renames, removals, changed application logic.
    CssClass,
    /// CSS custom property (variable) renames or removals.
    CssVariable,
    /// ARIA attribute changes, role changes, keyboard navigation.
    Accessibility,
    /// Changed default prop/parameter values.
    DefaultValue,
    /// Changed conditional logic, return values, event handling.
    LogicChange,
    /// Changed data-* attributes (data-testid, data-ouia-*, etc.).
    DataAttribute,
    /// General render output change.
    RenderOutput,
}

/// Manifest change types for npm/package.json.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TsManifestChangeType {
    EntryPointChanged,
    ExportsEntryRemoved,
    ExportsEntryAdded,
    ExportsConditionRemoved,
    ModuleSystemChanged,
    PeerDependencyAdded,
    PeerDependencyRemoved,
    PeerDependencyRangeChanged,
    EngineConstraintChanged,
    BinEntryRemoved,
}

/// Evidence data for TypeScript behavioral changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TsEvidence {
    /// Test assertions changed.
    TestDelta {
        removed_assertions: Vec<String>,
        added_assertions: Vec<String>,
    },
    /// Deterministic JSX AST diff.
    JsxDiff {
        element_before: Option<String>,
        element_after: Option<String>,
        change_description: String,
    },
    /// Deterministic CSS reference scan.
    CssScan { change_description: String },
    /// LLM-based analysis (with or without test context).
    LlmAnalysis {
        has_test_context: bool,
        spec_summary: String,
    },
}

/// TypeScript-specific report data carried on each `TypeSummary`.
///
/// Contains React/JSX-specific analysis results: discovered child
/// components with absorbed members, and expected composition
/// hierarchy children from LLM inference.
///
/// Flattened into the parent `TypeSummary` JSON via `#[serde(flatten)]`
/// for backward compatibility — fields appear at the top level.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TsReportData {
    /// Discovered child/sibling components (e.g., ModalHeader added
    /// alongside Modal being modified).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_components: Vec<ChildComponent>,

    /// Expected direct children of this component, derived from LLM
    /// hierarchy inference on the component family's source code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_children: Vec<ExpectedChild>,
}

/// A child or sibling component discovered during TypeScript analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildComponent {
    /// Component name (e.g., "ModalHeader").
    pub name: String,
    /// Whether this component was added or modified.
    pub status: ChildComponentStatus,
    /// Known members on this child component (from the new surface AST).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_members: Vec<String>,
    /// Members that were removed from the parent and match members on this
    /// child (by name). Populated from AST member comparison.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub absorbed_members: Vec<String>,
}

/// Status of a child/sibling component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildComponentStatus {
    /// Newly added in the new version.
    Added,
    /// Existed before but was modified.
    Modified,
}

// ── LanguageSemantics ───────────────────────────────────────────────────

impl LanguageSemantics<TsSymbolData> for TypeScript {
    fn is_member_addition_breaking(
        &self,
        container: &Symbol<TsSymbolData>,
        member: &Symbol<TsSymbolData>,
    ) -> bool {
        // TypeScript uses structural typing. Adding a required member to an
        // interface or type alias breaks consumers because they must now
        // provide it. Adding an optional member is non-breaking.
        //
        // For enums and classes, adding a member is never breaking.
        match container.kind {
            SymbolKind::Interface | SymbolKind::TypeAlias => {
                let is_optional = member
                    .signature
                    .as_ref()
                    .and_then(|s| s.parameters.first())
                    .map(|p| p.optional)
                    .unwrap_or(false);
                !is_optional
            }
            _ => false,
        }
    }

    fn same_family(&self, a: &Symbol<TsSymbolData>, b: &Symbol<TsSymbolData>) -> bool {
        // React convention: components in the same directory are a family.
        // E.g., components/Modal/Modal.tsx and components/Modal/ModalHeader.tsx
        //
        // We strip /deprecated/ and /next/ segments for canonical matching so
        // that a symbol moving between deprecated/ and main/ paths is still
        // considered the same family.
        canonical_component_dir(&a.file.to_string_lossy())
            == canonical_component_dir(&b.file.to_string_lossy())
    }

    fn same_identity(&self, a: &Symbol<TsSymbolData>, b: &Symbol<TsSymbolData>) -> bool {
        // React convention: ButtonProps and Button are the same concept.
        // Strip the "Props" suffix before comparing.
        strip_props_suffix(&a.name) == strip_props_suffix(&b.name)
    }

    fn visibility_rank(&self, v: Visibility) -> u8 {
        // TypeScript visibility ranking. Protected is treated the same as
        // Internal for semver purposes (both are non-exported).
        match v {
            Visibility::Private => 0,
            Visibility::Internal => 1,
            Visibility::Protected => 1, // TS protected ≈ internal for semver
            Visibility::Public => 2,
            Visibility::Exported => 3,
        }
    }

    fn parse_union_values(&self, type_str: &str) -> Option<BTreeSet<String>> {
        // TypeScript string literal unions: 'primary' | 'secondary' | 'danger'
        parse_ts_union_literals(type_str)
    }

    fn post_process(&self, changes: &mut Vec<StructuralChange>) {
        // Deduplicate changes for symbols exported both by name and as
        // `export default` (a TypeScript/JS-specific pattern).
        dedup_default_exports(changes);
    }

    fn hierarchy(&self) -> Option<&dyn HierarchySemantics<TsSymbolData>> {
        Some(self)
    }

    fn renames(&self) -> Option<&dyn RenameSemantics> {
        Some(self)
    }

    fn body_analyzer(&self) -> Option<&dyn BodyAnalysisSemantics> {
        Some(self)
    }

    fn primitive_type_names(&self) -> &[&str] {
        &[
            "string",
            "number",
            "boolean",
            "void",
            "null",
            "undefined",
            "never",
            "any",
            "unknown",
        ]
    }

    fn is_async_wrapper(&self, type_str: &str) -> bool {
        type_str.starts_with("Promise<")
    }

    fn format_import_change(&self, symbol: &str, old_path: &str, new_path: &str) -> String {
        format!(
            "replace `import {{ {} }} from '{}'` with `import {{ {} }} from '{}'`",
            symbol, old_path, symbol, new_path,
        )
    }

    fn should_skip_symbol(&self, sym: &Symbol<TsSymbolData>) -> bool {
        // Star re-exports (`export * from './module'`) are barrel-file
        // directives, not actual API symbols.
        sym.name == "*"
    }

    fn member_label(&self) -> &'static str {
        "props"
    }

    fn extract_rename_fallback_key(&self, sym: &Symbol<TsSymbolData>) -> Option<String> {
        // Token `.d.ts` files have type annotations like:
        //   { ["name"]: "--pf-v5-global--Color--dark-100"; ["value"]: "#151515"; ["var"]: "var(...)" }
        // Extract the "value" field for CSS-value-based rename matching.
        let return_type = sym.signature.as_ref()?.return_type.as_deref()?;
        let value_start = return_type
            .find("[\"value\"]")
            .or_else(|| return_type.find("\"value\""))?;
        let after_key = &return_type[value_start..];
        let colon_pos = after_key.find(':')?;
        let after_colon = &after_key[colon_pos + 1..];
        let open_quote = after_colon.find('"')?;
        let after_open = &after_colon[open_quote + 1..];
        let close_quote = after_open.find('"')?;
        let value = after_open[..close_quote].to_string();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }

    fn canonical_name_for_relocation(&self, qualified_name: &str) -> String {
        // Strip /deprecated/ and /next/ lifecycle segments so symbols
        // moving between these directories are matched as relocations.
        qualified_name
            .replace("/deprecated/", "/")
            .replace("/next/", "/")
    }

    fn classify_relocation(&self, old_qname: &str, new_qname: &str) -> Option<&'static str> {
        let old_deprecated = old_qname.contains("/deprecated/");
        let new_deprecated = new_qname.contains("/deprecated/");
        let old_next = old_qname.contains("/next/");
        let new_next = new_qname.contains("/next/");

        match (old_deprecated, new_deprecated, old_next, new_next) {
            (false, true, _, _) => Some("moved to deprecated"),
            (true, false, _, _) => Some("promoted from deprecated"),
            (_, _, true, false) => Some("promoted from next"),
            (_, _, false, true) => Some("moved to next"),
            _ => None,
        }
    }

    fn derive_import_subpath(&self, package: Option<&str>, qualified_name: &str) -> String {
        let base = package.unwrap_or("unknown");
        if qualified_name.contains("/deprecated/") {
            format!("{}/deprecated", base)
        } else if qualified_name.contains("/next/") {
            format!("{}/next", base)
        } else {
            base.to_string()
        }
    }
}

// ── MessageFormatter ────────────────────────────────────────────────────

impl MessageFormatter for TypeScript {
    fn describe(&self, change: &StructuralChange) -> String {
        // For Phase 2, this matches on the current 37-variant StructuralChangeType.
        // In Phase 4 when we collapse the enum, this will be updated to match
        // on the new 5-variant StructuralChangeTypeV2 + ChangeSubject.
        //
        // The descriptions must produce identical output to the current inline
        // description building in compare.rs and helpers.rs.
        //
        // For now, the descriptions are already built by the diff engine and
        // stored on the StructuralChange. This method returns them as-is.
        // In Phase 3, the diff engine will stop building descriptions and
        // call this method instead.
        change.description.clone()
    }
}

// ── Language ────────────────────────────────────────────────────────────

impl Language for TypeScript {
    type SymbolData = TsSymbolData;
    type Category = TsCategory;
    type ManifestChangeType = TsManifestChangeType;
    type Evidence = TsEvidence;
    type ReportData = TsReportData;
    type AnalysisExtensions = TsAnalysisExtensions;

    const RENAMEABLE_SYMBOL_KINDS: &'static [SymbolKind] =
        &[SymbolKind::Interface, SymbolKind::Class];
    const NAME: &'static str = "typescript";
    const MANIFEST_FILES: &'static [&'static str] = &["package.json"];
    const SOURCE_FILE_PATTERNS: &'static [&'static str] = &["*.ts", "*.tsx"];

    fn extract(
        &self,
        repo: &Path,
        git_ref: &str,
        degradation: Option<&semver_analyzer_core::diagnostics::DegradationTracker>,
    ) -> Result<ApiSurface<TsSymbolData>> {
        let extractor = crate::extract::OxcExtractor::new();
        extractor.extract_at_ref(repo, git_ref, self.build_command.as_deref(), degradation)
    }

    fn parse_changed_functions(
        &self,
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<Vec<ChangedFunction>> {
        let parser = crate::diff_parser::TsDiffParser::new();
        parser.parse_changed_functions(repo, from_ref, to_ref)
    }

    fn find_callers(&self, file: &Path, symbol_name: &str) -> Result<Vec<Caller>> {
        let cg = crate::call_graph::TsCallGraphBuilder::new();
        cg.find_callers(file, symbol_name)
    }

    fn find_references(&self, file: &Path, symbol_name: &str) -> Result<Vec<Reference>> {
        let cg = crate::call_graph::TsCallGraphBuilder::new();
        cg.find_references(file, symbol_name)
    }

    fn find_tests(&self, repo: &Path, source_file: &Path) -> Result<Vec<TestFile>> {
        let ta = crate::test_analyzer::TsTestAnalyzer::new();
        ta.find_tests(repo, source_file)
    }

    fn diff_test_assertions(
        &self,
        repo: &Path,
        test_file: &TestFile,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<TestDiff> {
        let ta = crate::test_analyzer::TsTestAnalyzer::new();
        ta.diff_test_assertions(repo, test_file, from_ref, to_ref)
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

    fn behavioral_change_kind(&self, evidence_type: &EvidenceType) -> BehavioralChangeKind {
        match evidence_type {
            EvidenceType::TestDelta => BehavioralChangeKind::Function,
            _ => BehavioralChangeKind::Class, // component-level for React
        }
    }

    fn extract_referenced_symbols(&self, description: &str) -> Vec<String> {
        let mut refs = Vec::new();
        let mut seen = HashSet::new();

        // Pattern 1: JSX-style <ComponentName> or <ComponentName ...>
        let mut remaining = description;
        while let Some(start) = remaining.find('<') {
            let after_lt = &remaining[start + 1..];
            let end = after_lt.find(['>', ' ', '/']).unwrap_or(after_lt.len());
            let name = &after_lt[..end];
            if !name.is_empty()
                && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && name.chars().all(|c| c.is_ascii_alphanumeric())
                && name.chars().any(|c| c.is_ascii_lowercase())
                && seen.insert(name.to_string())
            {
                refs.push(name.to_string());
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
                    && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                    && name.chars().all(|c| c.is_ascii_alphanumeric())
                    && name.chars().any(|c| c.is_ascii_lowercase())
                    && !name.contains(' ')
                    && seen.insert(name.to_string())
                {
                    refs.push(name.to_string());
                }
                remaining = &after_tick[end + 1..];
            } else {
                break;
            }
        }

        refs
    }

    fn display_name(&self, qualified_name: &str) -> String {
        // Split on :: to get file prefix and symbol parts
        let parts: Vec<&str> = qualified_name.split("::").collect();
        match parts.len() {
            0 | 1 => qualified_name.to_string(),
            2 => parts[1].to_string(),
            _ => parts[1..].join("."),
        }
    }

    fn llm_categories(&self) -> Vec<semver_analyzer_core::LlmCategoryDefinition> {
        use semver_analyzer_core::LlmCategoryDefinition;
        vec![
            LlmCategoryDefinition {
                id: "dom_structure".into(),
                label: "DOM/render changes".into(),
                description: "Changed element types (e.g., `<header>` → `<div>`), \
                    added/removed wrapper elements, altered component nesting structure, \
                    children wrapping changes"
                    .into(),
            },
            LlmCategoryDefinition {
                id: "css_class".into(),
                label: "CSS changes".into(),
                description: "Class name renames (e.g., pf-v5-* → pf-v6-*), removed \
                    CSS classes, changed class application logic, modifier classes \
                    no longer applied"
                    .into(),
            },
            LlmCategoryDefinition {
                id: "css_variable".into(),
                label: "CSS variable changes".into(),
                description: "Renamed or removed CSS custom properties \
                    (e.g., --pf-v5-* → --pf-v6-*)"
                    .into(),
            },
            LlmCategoryDefinition {
                id: "accessibility".into(),
                label: "Accessibility changes".into(),
                description: "Added/removed/changed ARIA attributes (aria-label, \
                    aria-labelledby, aria-describedby, aria-hidden), changed `role` \
                    attributes, keyboard navigation changes, focus management changes, \
                    tab order changes (tabIndex additions/removals)"
                    .into(),
            },
            LlmCategoryDefinition {
                id: "default_value".into(),
                label: "Default value changes".into(),
                description: "Changed default prop values that alter behavior".into(),
            },
            LlmCategoryDefinition {
                id: "logic_change".into(),
                label: "Logic changes".into(),
                description: "Changed conditional logic, removed code paths, altered \
                    return values for same inputs, changed event handler types, removed \
                    or changed event emissions"
                    .into(),
            },
            LlmCategoryDefinition {
                id: "data_attribute".into(),
                label: "Data attribute changes".into(),
                description: "Changed data-ouia-component-type, data-testid, or other \
                    data-* attributes"
                    .into(),
            },
            LlmCategoryDefinition {
                id: "render_output".into(),
                label: "Other render output".into(),
                description: "Any other change to what is visually rendered that \
                    doesn't fit above"
                    .into(),
            },
        ]
    }

    fn diff_manifest_content(old: &str, new: &str) -> Vec<ManifestChange<Self>> {
        let old_json: serde_json::Value = match serde_json::from_str(old) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let new_json: serde_json::Value = match serde_json::from_str(new) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        crate::manifest::diff_manifests(&old_json, &new_json)
    }

    fn should_exclude_from_analysis(path: &Path) -> bool {
        let basename = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        let path_str = path.to_string_lossy();

        // Barrel/index files
        basename == "index.ts" || basename == "index.tsx" || basename == "index.js"
        // Declaration files
        || basename.ends_with(".d.ts")
        // Test files
        || basename.contains(".test.") || basename.contains(".spec.")
        // Test directories and build output
        || path_str.contains("__tests__")
        || path_str.contains("/dist/")
        || path_str.starts_with("dist/")
    }

    fn run_extended_analysis(
        &self,
        params: &ExtendedAnalysisParams,
    ) -> Result<TsAnalysisExtensions> {
        let css_profiles = params.dep_dir.as_deref().and_then(|dir| {
            crate::css_profile::extract_css_profiles_from_dir(dir)
                .map_err(|e| {
                    tracing::warn!(%e, "failed to extract CSS profiles from dependency");
                    e
                })
                .ok()
        });

        let mut sd_result = crate::sd_pipeline::run_sd(
            &params.repo,
            &params.from_ref,
            &params.to_ref,
            css_profiles.as_ref(),
        )?;

        // Wire orchestrator-computed data into the SD result
        sd_result.removed_css_blocks = params.removed_dep_components.clone();
        sd_result.dep_repo_packages = params.dep_repo_packages.clone();

        Ok(TsAnalysisExtensions {
            sd_result: Some(sd_result),
            hierarchy_deltas: Vec::new(),
            new_hierarchies: std::collections::HashMap::new(),
        })
    }

    fn finalize_extensions(
        &self,
        extensions: &mut Self::AnalysisExtensions,
        structural_changes: Arc<Vec<StructuralChange>>,
    ) -> Arc<Vec<StructuralChange>> {
        let sd = match extensions.sd_result.as_mut() {
            Some(sd) => sd,
            None => return structural_changes,
        };

        // Deprecated replacement detection via rendering swaps
        let deprecated_replacements =
            crate::deprecated_replacements::detect_deprecated_replacements(&structural_changes, sd);
        if !deprecated_replacements.is_empty() {
            for dr in &deprecated_replacements {
                tracing::info!(
                    old = %dr.old_component,
                    new = %dr.new_component,
                    evidence = ?dr.evidence_hosts,
                    "Deprecated replacement detected via rendering swap"
                );
            }
            sd.deprecated_replacements = deprecated_replacements;
        }

        // Transform structural changes
        crate::deprecated_replacements::apply_deprecated_replacements(
            structural_changes,
            &sd.deprecated_replacements,
        )
    }

    fn extensions_log_summary(&self, extensions: &Self::AnalysisExtensions) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(ref sd) = extensions.sd_result {
            lines.push(format!(
                "[SD] {} source-level changes, {} composition trees, {} conformance checks",
                sd.source_level_changes.len(),
                sd.composition_trees.len(),
                sd.conformance_checks.len(),
            ));
            if !sd.composition_changes.is_empty() {
                lines.push(format!(
                    "[SD] {} composition changes detected",
                    sd.composition_changes.len(),
                ));
            }
            if !sd.deprecated_replacements.is_empty() {
                lines.push(format!(
                    "[SD] {} deprecated replacements detected via rendering swaps",
                    sd.deprecated_replacements.len(),
                ));
            }
        }
        lines
    }
}

// ── HierarchySemantics (React component hierarchy) ─────────────────────

impl HierarchySemantics<TsSymbolData> for TypeScript {
    fn family_source_paths(&self, repo: &Path, git_ref: &str, family_name: &str) -> Vec<String> {
        let output = std::process::Command::new("git")
            .args(["ls-tree", "-r", "--name-only", git_ref])
            .current_dir(repo)
            .output();

        let all_files = match output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return Vec::new(),
        };

        let mut source_files = Vec::new();
        for line in all_files.lines() {
            if !line.ends_with(".tsx") && !line.ends_with(".ts") {
                continue;
            }
            if line.contains("__tests__")
                || line.contains("__mocks__")
                || line.contains("__snapshots__")
                || line.contains("/stories/")
            {
                continue;
            }
            // Check if this file is in the family directory.
            // Exclude `next/` and `deprecated/` staging directories — these
            // contain preview or compat copies of components that would confuse
            // hierarchy inference by showing two versions of the same component.
            if line.contains("/next/components/") || line.contains("/deprecated/components/") {
                continue;
            }
            let parts: Vec<&str> = line.rsplitn(2, '/').collect();
            if parts.len() < 2 {
                continue;
            }
            let dir = parts[1];
            let is_family_dir = dir.ends_with(&format!("/{}", family_name))
                || dir.ends_with(&format!("/components/{}", family_name));
            if is_family_dir {
                source_files.push(line.to_string());
            }
        }

        source_files
    }

    fn family_name_from_symbols(&self, symbols: &[&Symbol<TsSymbolData>]) -> Option<String> {
        // Extract the component directory name from the first symbol's file path
        for sym in symbols {
            let path = sym.file.to_string_lossy();
            if let Some(name) = extract_family_from_path(&path) {
                return Some(name);
            }
        }
        None
    }

    fn is_hierarchy_candidate(&self, sym: &Symbol<TsSymbolData>) -> bool {
        // React components are PascalCase functions, classes, variables, or constants
        matches!(
            sym.kind,
            SymbolKind::Variable | SymbolKind::Class | SymbolKind::Function | SymbolKind::Constant
        ) && sym
            .name
            .chars()
            .next()
            .map(|c| c.is_uppercase())
            .unwrap_or(false)
    }

    fn cross_family_relationships(
        &self,
        repo: &Path,
        git_ref: &str,
    ) -> Vec<(String, String, String)> {
        use regex::Regex;

        let output = match std::process::Command::new("git")
            .args(["ls-tree", "-r", "--name-only", git_ref])
            .current_dir(repo)
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return Vec::new(),
        };

        let re =
            Regex::new(r"import\s+\{[^}]*?(\w*Context\w*)[^}]*\}\s+from\s+'\.\./([\w]+)/").unwrap();

        let mut relationships = Vec::new();
        let mut seen = HashSet::new();

        for file_path in output.lines() {
            if (!file_path.ends_with(".tsx") && !file_path.ends_with(".ts"))
                || file_path.contains("__tests__")
                || file_path.contains("/examples/")
                || file_path.contains("/deprecated/")
                || file_path.contains("/stories/")
            {
                continue;
            }
            if !file_path.contains("/components/") {
                continue;
            }
            let consumer_family = match extract_family_from_path(file_path) {
                Some(f) => f,
                None => continue,
            };

            let content = match read_git_file(repo, git_ref, file_path) {
                Some(c) => c,
                None => continue,
            };

            for cap in re.captures_iter(&content) {
                let context_name = cap[1].to_string();
                let provider_family = cap[2].to_string();
                if provider_family == consumer_family {
                    continue;
                }
                let key = (
                    consumer_family.clone(),
                    provider_family.clone(),
                    context_name.clone(),
                );
                if seen.insert(key) {
                    relationships.push((
                        consumer_family.clone(),
                        provider_family.clone(),
                        context_name,
                    ));
                }
            }
        }

        relationships
    }

    fn compute_deterministic_hierarchy(
        &self,
        new_surface: &ApiSurface<TsSymbolData>,
        structural_changes: &[StructuralChange],
    ) -> std::collections::HashMap<String, std::collections::HashMap<String, Vec<ExpectedChild>>>
    {
        use semver_analyzer_core::ChangeSubject;
        use std::collections::{BTreeMap, HashMap};

        // ── Index: group hierarchy candidates by family ──────────────
        let mut families: HashMap<String, Vec<&Symbol<TsSymbolData>>> = HashMap::new();
        for sym in &new_surface.symbols {
            if !self.is_hierarchy_candidate(sym) {
                continue;
            }
            if let Some(family) = self.family_name_from_symbols(&[sym]) {
                families.entry(family).or_default().push(sym);
            }
        }

        // ── Index: interface extends map ─────────────────────────────
        //
        // Maps interface name → what it extends.
        // e.g., "DropdownProps" → "MenuProps"
        let mut iface_extends: HashMap<&str, &str> = HashMap::new();
        for sym in &new_surface.symbols {
            if sym.kind == SymbolKind::Interface {
                if let Some(ext) = &sym.extends {
                    iface_extends.insert(&sym.name, ext.as_str());
                }
            }
        }

        // ── Index: component → props interface name ──────────────────
        //
        // Convention: component "Dropdown" → props interface "DropdownProps".
        // We verify the interface actually exists in the surface.
        let iface_names: HashSet<&str> = new_surface
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .map(|s| s.name.as_str())
            .collect();

        // ── Index: props interface → component name ──────────────────
        //
        // Reverse mapping: "MenuProps" → "Menu", "MenuListProps" → "MenuList"
        // Used for cross-family extends resolution.
        let mut props_to_component: HashMap<String, &str> = HashMap::new();
        for sym in &new_surface.symbols {
            if !self.is_hierarchy_candidate(sym) {
                continue;
            }
            let props_name = format!("{}Props", sym.name);
            if iface_names.contains(props_name.as_str()) {
                props_to_component.insert(props_name, &sym.name);
            }
        }

        // ── Signal 1: Prop absorption ────────────────────────────────
        //
        // For each parent interface with removed members, find new family
        // members whose props interface has matching member names.
        let mut removed_props_by_parent: HashMap<String, HashSet<String>> = HashMap::new();
        for change in structural_changes {
            if let StructuralChangeType::Removed(ChangeSubject::Member { name, .. }) =
                &change.change_type
            {
                let parent = if let Some((p, _)) = change.symbol.rsplit_once('.') {
                    p.strip_suffix("Props").unwrap_or(p).to_string()
                } else {
                    change
                        .symbol
                        .strip_suffix("Props")
                        .unwrap_or(&change.symbol)
                        .to_string()
                };
                removed_props_by_parent
                    .entry(parent)
                    .or_default()
                    .insert(name.clone());
            }
        }

        // For each family, check which new members absorbed removed props.
        let mut absorption_children: HashMap<String, BTreeMap<String, Vec<String>>> =
            HashMap::new();

        for members in families.values() {
            for parent in members.iter() {
                let removed = match removed_props_by_parent.get(&parent.name) {
                    Some(r) if !r.is_empty() => r,
                    _ => continue,
                };

                for candidate in members.iter() {
                    if candidate.name == parent.name {
                        continue;
                    }

                    let candidate_props: HashSet<&str> =
                        candidate.members.iter().map(|m| m.name.as_str()).collect();

                    let props_iface_name = format!("{}Props", candidate.name);
                    let iface_props: HashSet<&str> = new_surface
                        .symbols
                        .iter()
                        .find(|s| s.name == props_iface_name && s.kind == SymbolKind::Interface)
                        .map(|s| s.members.iter().map(|m| m.name.as_str()).collect())
                        .unwrap_or_default();

                    let all_candidate_props: HashSet<&str> =
                        candidate_props.union(&iface_props).copied().collect();

                    let absorbed: Vec<String> = removed
                        .iter()
                        .filter(|prop| all_candidate_props.contains(prop.as_str()))
                        .cloned()
                        .collect();

                    if !absorbed.is_empty() {
                        absorption_children
                            .entry(parent.name.clone())
                            .or_default()
                            .insert(candidate.name.clone(), absorbed);
                    }
                }
            }
        }

        // ── Signal 2: Cross-family extends mapping ───────────────────
        //
        // If Dropdown renders Menu (from Menu family), and DropdownList's
        // props extend MenuListProps → DropdownList maps to MenuList.
        let mut extends_map: HashMap<&str, &str> = HashMap::new();
        for members in families.values() {
            for sym in members {
                let props_name = format!("{}Props", sym.name);
                if let Some(ext_iface) = iface_extends.get(props_name.as_str()) {
                    // Strip Omit<...> wrapper if present
                    let ext_clean = ext_iface
                        .strip_prefix("Omit<")
                        .and_then(|s| s.split(',').next())
                        .unwrap_or(ext_iface);
                    if let Some(ext_component) = props_to_component.get(ext_clean) {
                        // Only cross-family: the extended component should NOT be
                        // in the same family.
                        let ext_family = self.family_name_from_symbols(&[new_surface
                            .symbols
                            .iter()
                            .find(|s| s.name.as_str() == *ext_component)
                            .unwrap_or(sym)]);
                        let own_family = self.family_name_from_symbols(&[sym]);
                        if ext_family != own_family {
                            extends_map.insert(&sym.name, ext_component);
                        }
                    }
                }
            }
        }

        // ── Combine signals into hierarchy ───────────────────────────
        let mut result: HashMap<String, HashMap<String, Vec<ExpectedChild>>> = HashMap::new();

        for (family_name, members) in &families {
            let member_names: HashSet<&str> = members.iter().map(|s| s.name.as_str()).collect();
            let mut family_hierarchy: HashMap<String, Vec<ExpectedChild>> = HashMap::new();

            // Signal 3: internal rendering (from TsSymbolData.rendered_components)
            let mut renders_family: HashMap<&str, HashSet<&str>> = HashMap::new();
            for sym in members {
                let family_renders: HashSet<&str> = sym
                    .language_data
                    .rendered_components
                    .iter()
                    .filter(|r| {
                        member_names.contains(r.as_str()) && r.as_str() != sym.name.as_str()
                    })
                    .map(|r| r.as_str())
                    .collect();
                if !family_renders.is_empty() {
                    renders_family.insert(&sym.name, family_renders);
                }
            }

            for parent in members.iter() {
                let mut children: BTreeMap<&str, ExpectedChild> = BTreeMap::new();

                // ── Signal 1: absorption ─────────────────────────────
                if let Some(absorbed) = absorption_children.get(&parent.name) {
                    for child_name in absorbed.keys() {
                        if !member_names.contains(child_name.as_str()) {
                            continue;
                        }
                        let parent_renders = renders_family.get(parent.name.as_str());
                        let is_rendered = parent_renders
                            .map(|r| r.contains(child_name.as_str()))
                            .unwrap_or(false);

                        let child = if is_rendered {
                            ExpectedChild {
                                name: child_name.clone(),
                                required: false,
                                mechanism: "prop".to_string(),
                                prop_name: None,
                            }
                        } else {
                            ExpectedChild::new(child_name, false)
                        };
                        children.insert(child_name.as_str(), child);
                    }
                }

                // ── Signal 2: cross-family extends mapping ───────────
                if let Some(ext_parent) = extends_map.get(parent.name.as_str()) {
                    let renders_ext_parent = parent
                        .language_data
                        .rendered_components
                        .iter()
                        .any(|r| r.as_str() == *ext_parent);

                    let ext_parent_sym = new_surface
                        .symbols
                        .iter()
                        .find(|s| s.name.as_str() == *ext_parent);
                    let ext_parent_is_container = ext_parent_sym
                        .map(|ep| {
                            let ep_family = self.family_name_from_symbols(&[ep]);
                            ep.language_data.rendered_components.iter().any(|rc| {
                                new_surface
                                    .symbols
                                    .iter()
                                    .filter(|s| self.is_hierarchy_candidate(s))
                                    .any(|s| {
                                        s.name.as_str() == rc.as_str()
                                            && self.family_name_from_symbols(&[s]) == ep_family
                                    })
                            })
                        })
                        .unwrap_or(false);

                    if renders_ext_parent && ext_parent_is_container {
                        if let Some(ext_sym) = ext_parent_sym {
                            for candidate in members.iter() {
                                if candidate.name == parent.name {
                                    continue;
                                }
                                if children.contains_key(candidate.name.as_str()) {
                                    continue;
                                }

                                if let Some(ext_child) = extends_map.get(candidate.name.as_str()) {
                                    let ext_renders_child = ext_sym
                                        .language_data
                                        .rendered_components
                                        .contains(&ext_child.to_string());

                                    if !ext_renders_child {
                                        let ext_child_sym = new_surface
                                            .symbols
                                            .iter()
                                            .find(|s| s.name.as_str() == *ext_child);
                                        let ext_child_is_container = ext_child_sym
                                            .map(|ec| {
                                                let ec_family =
                                                    self.family_name_from_symbols(&[ec]);
                                                ec.language_data.rendered_components.iter().any(
                                                    |rc| {
                                                        new_surface
                                                            .symbols
                                                            .iter()
                                                            .filter(|s| {
                                                                self.is_hierarchy_candidate(s)
                                                            })
                                                            .any(|s| {
                                                                s.name.as_str() == rc.as_str()
                                                                    && self
                                                                        .family_name_from_symbols(
                                                                            &[s],
                                                                        )
                                                                        == ec_family
                                                            })
                                                    },
                                                )
                                            })
                                            .unwrap_or(false);

                                        if !ext_child_is_container {
                                            children.insert(
                                                &candidate.name,
                                                ExpectedChild::new(&candidate.name, false),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                if !children.is_empty() {
                    family_hierarchy.insert(parent.name.clone(), children.into_values().collect());
                }
            }

            if !family_hierarchy.is_empty() {
                result.insert(family_name.clone(), family_hierarchy);
            }
        }

        result
    }

    fn related_family_content(
        &self,
        repo: &Path,
        git_ref: &str,
        family_name: &str,
        relationship_names: &[String],
    ) -> Option<String> {
        let output = std::process::Command::new("git")
            .args(["ls-tree", "-r", "--name-only", git_ref])
            .current_dir(repo)
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let all_files = String::from_utf8_lossy(&output.stdout);
        let mut content = String::new();

        for line in all_files.lines() {
            if !line.ends_with(".tsx") && !line.ends_with(".ts") {
                continue;
            }
            if line.contains("__tests__")
                || line.contains("/examples/")
                || line.contains("/deprecated/")
                || line.contains("/stories/")
                || line.contains("index.ts")
            {
                continue;
            }
            let file_family = match extract_family_from_path(line) {
                Some(f) => f,
                None => continue,
            };
            if file_family != family_name {
                continue;
            }
            let file_content = match read_git_file(repo, git_ref, line) {
                Some(c) => c,
                None => continue,
            };
            let uses_context = relationship_names
                .iter()
                .any(|ctx| file_content.contains(ctx));
            if !uses_context {
                continue;
            }
            content.push_str(&format!(
                "\n--- Related: {} (uses {}) ---\n",
                line,
                relationship_names.join(", "),
            ));
            content.push_str(&file_content);
            content.push('\n');
        }

        if content.is_empty() {
            None
        } else {
            Some(content)
        }
    }
}

// ── RenameSemantics (PatternFly-specific rename patterns) ──────────────

impl RenameSemantics for TypeScript {
    fn sample_removed_constants<'a>(
        &self,
        removed: &[&'a str],
        _added: &[&'a str],
    ) -> Vec<&'a str> {
        let directional_suffixes = [
            "Top",
            "Bottom",
            "Left",
            "Right",
            "Width",
            "Height",
            "MaxWidth",
            "MaxHeight",
            "MinWidth",
            "MinHeight",
        ];
        let mut sample: Vec<&'a str> = removed
            .iter()
            .filter(|s| directional_suffixes.iter().any(|d| s.ends_with(d)))
            .take(20)
            .copied()
            .collect();
        for s in removed.iter() {
            if sample.len() >= 30 {
                break;
            }
            if !sample.contains(s) {
                sample.push(s);
            }
        }
        sample
    }

    fn sample_added_constants<'a>(&self, _removed: &[&'a str], added: &[&'a str]) -> Vec<&'a str> {
        let logical_suffixes = [
            "BlockStart",
            "BlockEnd",
            "InlineStart",
            "InlineEnd",
            "InlineSize",
            "BlockSize",
        ];
        let mut sample: Vec<&'a str> = added
            .iter()
            .filter(|s| logical_suffixes.iter().any(|d| s.contains(d)))
            .take(20)
            .copied()
            .collect();
        for s in added.iter() {
            if sample.len() >= 30 {
                break;
            }
            if !sample.contains(s) {
                sample.push(s);
            }
        }
        sample
    }
}

// ── BodyAnalysisSemantics (JSX diff + CSS scan) ────────────────────────

impl BodyAnalysisSemantics for TypeScript {
    fn analyze_changed_body(
        &self,
        old_body: &str,
        new_body: &str,
        func_name: &str,
        file_path: &str,
    ) -> Vec<BodyAnalysisResult> {
        let mut results = Vec::new();

        let file = Path::new(file_path);

        // JSX diff analysis
        if crate::jsx_diff::body_contains_jsx(old_body)
            && crate::jsx_diff::body_contains_jsx(new_body)
        {
            let jsx_changes = crate::jsx_diff::diff_jsx_bodies(old_body, new_body, func_name, file);
            for jsx_change in jsx_changes {
                results.push(BodyAnalysisResult {
                    description: jsx_change.description,
                    category_label: Some(ts_category_label(&jsx_change.category).to_string()),
                    confidence: 0.90,
                });
            }
        }

        // CSS variable/class scanning
        if crate::css_scan::body_contains_css_refs(old_body)
            || crate::css_scan::body_contains_css_refs(new_body)
        {
            let css_changes =
                crate::css_scan::diff_css_references(old_body, new_body, func_name, file);
            for css_change in css_changes {
                results.push(BodyAnalysisResult {
                    description: css_change.description,
                    category_label: Some(ts_category_label(&css_change.category).to_string()),
                    confidence: 0.90,
                });
            }
        }

        results
    }
}

/// Convert a TsCategory to a snake_case string label.
pub fn ts_category_label(cat: &TsCategory) -> &'static str {
    match cat {
        TsCategory::DomStructure => "dom_structure",
        TsCategory::CssClass => "css_class",
        TsCategory::CssVariable => "css_variable",
        TsCategory::Accessibility => "accessibility",
        TsCategory::DefaultValue => "default_value",
        TsCategory::LogicChange => "logic_change",
        TsCategory::DataAttribute => "data_attribute",
        TsCategory::RenderOutput => "render_output",
    }
}

/// Extract the component family directory name from a file path.
/// e.g., "packages/react-core/src/components/Masthead/Masthead.tsx" → "Masthead"
fn extract_family_from_path(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == "components" && i + 1 < parts.len() && i + 2 < parts.len() {
            return Some(parts[i + 1].to_string());
        }
    }
    None
}

use crate::git_utils::read_git_file;

// ── Extracted helper functions ──────────────────────────────────────────

/// Extract the component directory from a file path, stripping /deprecated/
/// and /next/ segments for canonical matching.
///
/// `packages/react-core/dist/esm/deprecated/components/Select/Select.d.ts`
/// → `packages/react-core/dist/esm/components/Select`
///
/// `packages/react-core/dist/esm/components/EmptyState/EmptyStateHeader.d.ts`
/// → `packages/react-core/dist/esm/components/EmptyState`
pub(crate) fn canonical_component_dir(file_path: &str) -> String {
    let canonical = file_path
        .replace("/deprecated/", "/")
        .replace("/next/", "/");
    let canonical = if canonical.starts_with("deprecated/") {
        canonical.strip_prefix("deprecated/").unwrap().to_string()
    } else {
        canonical
    };
    let canonical = if canonical.starts_with("next/") {
        canonical.strip_prefix("next/").unwrap().to_string()
    } else {
        canonical
    };

    match canonical.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => canonical,
    }
}

/// Strip a "Props" suffix from a symbol name for comparison.
///
/// `EmptyStateHeaderProps` → `EmptyStateHeader`
/// `SelectProps` → `Select`
/// `Modal` → `Modal`
fn strip_props_suffix(name: &str) -> &str {
    name.strip_suffix("Props").unwrap_or(name)
}

/// Parse a TypeScript string literal union type into its individual members.
///
/// `'primary' | 'secondary' | 'tertiary'` → `{"primary", "secondary", "tertiary"}`
///
/// Also handles mixed unions like `'primary' | ButtonVariant | undefined` by
/// extracting only the string literal members (quoted with single or double quotes).
fn parse_ts_union_literals(type_str: &str) -> Option<BTreeSet<String>> {
    if !type_str.contains('\'') && !type_str.contains('"') {
        return None;
    }
    if !type_str.contains('|') {
        return None;
    }

    let mut literals = BTreeSet::new();
    for part in type_str.split('|') {
        let trimmed = part.trim();
        if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
            || (trimmed.starts_with('"') && trimmed.ends_with('"'))
        {
            let value = &trimmed[1..trimmed.len() - 1];
            if !value.is_empty() {
                literals.insert(value.to_string());
            }
        }
    }

    if literals.len() >= 2 {
        Some(literals)
    } else {
        None
    }
}

/// Remove redundant `default` export changes when a named sibling from the
/// same file has the same change type.
fn dedup_default_exports(changes: &mut Vec<StructuralChange>) {
    let named_changes: HashSet<(String, StructuralChangeType)> = changes
        .iter()
        .filter(|c| c.symbol != "default")
        .filter_map(|c| {
            file_prefix(&c.qualified_name).map(|prefix| (prefix.to_string(), c.change_type.clone()))
        })
        .collect();

    changes.retain(|c| {
        if c.symbol != "default" {
            return true;
        }
        if let Some(prefix) = file_prefix(&c.qualified_name) {
            !named_changes.contains(&(prefix.to_string(), c.change_type.clone()))
        } else {
            true
        }
    });
}

/// Extract the file prefix from a qualified_name (everything before the last `.`).
fn file_prefix(qualified_name: &str) -> Option<&str> {
    qualified_name.rsplit_once('.').map(|(prefix, _)| prefix)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use semver_analyzer_core::Symbol as CoreSymbol;
    use semver_analyzer_core::{Parameter, Signature};

    /// In tests, `Symbol` means `Symbol<TsSymbolData>` to match trait impls.
    type Symbol = CoreSymbol<TsSymbolData>;

    fn sym(name: &str, kind: SymbolKind) -> Symbol {
        Symbol::new(name, name, kind, Visibility::Exported, "test.d.ts", 1)
    }

    fn make_interface(name: &str, file: &str, members: &[&str]) -> Symbol {
        let mut s = Symbol::new(
            name,
            format!("{}.{}", file, name),
            SymbolKind::Interface,
            Visibility::Exported,
            file,
            1,
        );
        for &member_name in members {
            s.members.push(Symbol::new(
                member_name,
                format!("{}.{}.{}", file, name, member_name),
                SymbolKind::Property,
                Visibility::Public,
                file,
                1,
            ));
        }
        s
    }

    // ── is_member_addition_breaking ──────────────────────────────

    #[test]
    fn required_member_on_interface_is_breaking() {
        let ts = TypeScript::default();
        let container = sym("ButtonProps", SymbolKind::Interface);
        let member = sym("onClick", SymbolKind::Property);
        assert!(ts.is_member_addition_breaking(&container, &member));
    }

    #[test]
    fn optional_member_on_interface_is_not_breaking() {
        let ts = TypeScript::default();
        let container = sym("ButtonProps", SymbolKind::Interface);
        let mut member = sym("onClick", SymbolKind::Property);
        member.signature = Some(Signature {
            parameters: vec![Parameter {
                name: "onClick".into(),
                type_annotation: Some("() => void".into()),
                optional: true,
                has_default: false,
                default_value: None,
                is_variadic: false,
            }],
            return_type: None,
            type_parameters: vec![],
            is_async: false,
        });
        assert!(!ts.is_member_addition_breaking(&container, &member));
    }

    #[test]
    fn member_on_enum_is_not_breaking() {
        let ts = TypeScript::default();
        let container = sym("Color", SymbolKind::Enum);
        let member = sym("Green", SymbolKind::EnumMember);
        assert!(!ts.is_member_addition_breaking(&container, &member));
    }

    #[test]
    fn member_on_class_is_not_breaking() {
        let ts = TypeScript::default();
        let container = sym("UserService", SymbolKind::Class);
        let member = sym("getUser", SymbolKind::Method);
        assert!(!ts.is_member_addition_breaking(&container, &member));
    }

    // ── same_family ─────────────────────────────────────────────

    #[test]
    fn same_directory_is_same_family() {
        let ts = TypeScript::default();
        let a = make_interface("Modal", "components/Modal/Modal.d.ts", &[]);
        let b = make_interface("ModalHeader", "components/Modal/ModalHeader.d.ts", &[]);
        assert!(ts.same_family(&a, &b));
    }

    #[test]
    fn different_directory_is_not_same_family() {
        let ts = TypeScript::default();
        let a = make_interface("Modal", "components/Modal/Modal.d.ts", &[]);
        let b = make_interface("Button", "components/Button/Button.d.ts", &[]);
        assert!(!ts.same_family(&a, &b));
    }

    #[test]
    fn deprecated_and_main_are_same_family() {
        let ts = TypeScript::default();
        let a = make_interface("Select", "deprecated/components/Select/Select.d.ts", &[]);
        let b = make_interface("Select", "components/Select/Select.d.ts", &[]);
        assert!(ts.same_family(&a, &b));
    }

    // ── same_identity ───────────────────────────────────────────

    #[test]
    fn button_and_button_props_are_same_identity() {
        let ts = TypeScript::default();
        let a = sym("Button", SymbolKind::Function);
        let b = sym("ButtonProps", SymbolKind::Interface);
        assert!(ts.same_identity(&a, &b));
    }

    #[test]
    fn same_name_is_same_identity() {
        let ts = TypeScript::default();
        let a = sym("Select", SymbolKind::Interface);
        let b = sym("Select", SymbolKind::Interface);
        assert!(ts.same_identity(&a, &b));
    }

    #[test]
    fn different_names_are_not_same_identity() {
        let ts = TypeScript::default();
        let a = sym("Button", SymbolKind::Function);
        let b = sym("Select", SymbolKind::Function);
        assert!(!ts.same_identity(&a, &b));
    }

    // ── visibility_rank ─────────────────────────────────────────

    #[test]
    fn ts_visibility_ranking() {
        let ts = TypeScript::default();
        assert!(ts.visibility_rank(Visibility::Private) < ts.visibility_rank(Visibility::Internal));
        assert_eq!(
            ts.visibility_rank(Visibility::Internal),
            ts.visibility_rank(Visibility::Protected)
        );
        assert!(ts.visibility_rank(Visibility::Protected) < ts.visibility_rank(Visibility::Public));
        assert!(ts.visibility_rank(Visibility::Public) < ts.visibility_rank(Visibility::Exported));
    }

    // ── parse_union_values ──────────────────────────────────────

    #[test]
    fn parses_string_literal_union() {
        let ts = TypeScript::default();
        let result = ts
            .parse_union_values("'primary' | 'secondary' | 'danger'")
            .unwrap();
        assert_eq!(result.len(), 3);
        assert!(result.contains("primary"));
        assert!(result.contains("secondary"));
        assert!(result.contains("danger"));
    }

    #[test]
    fn returns_none_for_non_union() {
        let ts = TypeScript::default();
        assert!(ts.parse_union_values("string").is_none());
    }

    #[test]
    fn returns_none_for_single_literal() {
        let ts = TypeScript::default();
        assert!(ts.parse_union_values("'primary'").is_none());
    }

    #[test]
    fn handles_mixed_union_with_type_refs() {
        let ts = TypeScript::default();
        let result = ts
            .parse_union_values("'primary' | 'secondary' | ButtonVariant | undefined")
            .unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains("primary"));
        assert!(result.contains("secondary"));
    }

    // ── post_process (dedup default exports) ────────────────────

    #[test]
    fn dedup_default_keeps_named_removes_default() {
        use semver_analyzer_core::ChangeSubject;
        let ts = TypeScript::default();
        let mut changes = vec![
            StructuralChange {
                symbol: "c_button".into(),
                qualified_name: "pkg/dist/c_button.c_button".into(),
                kind: SymbolKind::Constant,
                package: None,
                change_type: StructuralChangeType::Removed(ChangeSubject::Symbol {
                    kind: SymbolKind::Constant,
                }),
                before: None,
                after: None,
                description: "removed".into(),
                is_breaking: true,
                impact: None,
                migration_target: None,
            },
            StructuralChange {
                symbol: "default".into(),
                qualified_name: "pkg/dist/c_button.default".into(),
                kind: SymbolKind::Constant,
                package: None,
                change_type: StructuralChangeType::Removed(ChangeSubject::Symbol {
                    kind: SymbolKind::Constant,
                }),
                before: None,
                after: None,
                description: "removed".into(),
                is_breaking: true,
                impact: None,
                migration_target: None,
            },
        ];
        ts.post_process(&mut changes);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].symbol, "c_button");
    }

    // ── canonical_component_dir ─────────────────────────────────

    #[test]
    fn strips_deprecated_segment() {
        assert_eq!(
            canonical_component_dir(
                "packages/react-core/dist/esm/deprecated/components/Select/Select.d.ts"
            ),
            "packages/react-core/dist/esm/components/Select"
        );
    }

    #[test]
    fn strips_next_segment() {
        assert_eq!(
            canonical_component_dir(
                "packages/react-core/dist/esm/next/components/Modal/ModalHeader.d.ts"
            ),
            "packages/react-core/dist/esm/components/Modal"
        );
    }

    #[test]
    fn normal_path_returns_directory() {
        assert_eq!(
            canonical_component_dir(
                "packages/react-core/dist/esm/components/EmptyState/EmptyStateHeader.d.ts"
            ),
            "packages/react-core/dist/esm/components/EmptyState"
        );
    }

    // ── should_skip_symbol ──────────────────────────────────────

    #[test]
    fn star_reexport_skipped() {
        let ts = TypeScript::default();
        let sym = Symbol::new(
            "*",
            "pkg/index.*",
            SymbolKind::Variable,
            Visibility::Exported,
            std::path::PathBuf::from("pkg/index.d.ts"),
            1,
        );
        assert!(ts.should_skip_symbol(&sym));
    }

    #[test]
    fn normal_symbol_not_skipped() {
        let ts = TypeScript::default();
        let sym = Symbol::new(
            "Button",
            "pkg/Button.Button",
            SymbolKind::Variable,
            Visibility::Exported,
            std::path::PathBuf::from("pkg/Button.d.ts"),
            1,
        );
        assert!(!ts.should_skip_symbol(&sym));
    }

    // ── extract_rename_fallback_key ─────────────────────────────

    #[test]
    fn extract_css_token_value_basic() {
        let ts = TypeScript::default();
        let mut sym = Symbol::new(
            "global_Color_dark_100",
            "pkg/global_Color_dark_100",
            SymbolKind::Constant,
            Visibility::Public,
            std::path::PathBuf::from("pkg/global_Color_dark_100.d.ts"),
            1,
        );
        sym.signature = Some(semver_analyzer_core::Signature {
            parameters: Vec::new(),
            return_type: Some(
                "{ [\"name\"]: \"--pf-v5-global--Color--dark-100\"; [\"value\"]: \"#151515\"; [\"var\"]: \"var(--pf-v5-global--Color--dark-100)\" }"
                .to_string(),
            ),
            type_parameters: Vec::new(),
            is_async: false,
        });
        assert_eq!(
            ts.extract_rename_fallback_key(&sym),
            Some("#151515".to_string())
        );
    }

    #[test]
    fn extract_css_token_value_no_signature() {
        let ts = TypeScript::default();
        let sym = Symbol::new(
            "global_Color_dark_100",
            "pkg/global_Color_dark_100",
            SymbolKind::Constant,
            Visibility::Public,
            std::path::PathBuf::from("pkg/global_Color_dark_100.d.ts"),
            1,
        );
        assert_eq!(ts.extract_rename_fallback_key(&sym), None);
    }

    #[test]
    fn extract_css_token_value_no_value_field() {
        let ts = TypeScript::default();
        let mut sym = Symbol::new(
            "foo",
            "pkg/foo",
            SymbolKind::Constant,
            Visibility::Public,
            std::path::PathBuf::from("pkg/foo.d.ts"),
            1,
        );
        sym.signature = Some(semver_analyzer_core::Signature {
            parameters: Vec::new(),
            return_type: Some("string".to_string()),
            type_parameters: Vec::new(),
            is_async: false,
        });
        assert_eq!(ts.extract_rename_fallback_key(&sym), None);
    }

    #[test]
    fn extract_css_token_value_calc() {
        let ts = TypeScript::default();
        let mut sym = Symbol::new(
            "c_button_Width",
            "pkg/c_button_Width",
            SymbolKind::Constant,
            Visibility::Public,
            std::path::PathBuf::from("pkg/c_button_Width.d.ts"),
            1,
        );
        sym.signature = Some(semver_analyzer_core::Signature {
            parameters: Vec::new(),
            return_type: Some(
                "{ [\"name\"]: \"--pf-v5-c-button--Width\"; [\"value\"]: \"calc(1.25rem * 2)\"; [\"var\"]: \"var(--pf-v5-c-button--Width)\" }"
                .to_string(),
            ),
            type_parameters: Vec::new(),
            is_async: false,
        });
        assert_eq!(
            ts.extract_rename_fallback_key(&sym),
            Some("calc(1.25rem * 2)".to_string())
        );
    }

    // ── canonical_name_for_relocation ────────────────────────────

    #[test]
    fn canonical_strips_deprecated() {
        let ts = TypeScript::default();
        assert_eq!(
            ts.canonical_name_for_relocation("pkg/dist/esm/deprecated/components/Chip/Chip.Chip"),
            "pkg/dist/esm/components/Chip/Chip.Chip"
        );
    }

    #[test]
    fn canonical_strips_next() {
        let ts = TypeScript::default();
        assert_eq!(
            ts.canonical_name_for_relocation("pkg/dist/esm/next/components/Modal/Modal.Modal"),
            "pkg/dist/esm/components/Modal/Modal.Modal"
        );
    }

    #[test]
    fn canonical_preserves_normal_path() {
        let ts = TypeScript::default();
        let path = "pkg/dist/esm/components/Button/Button.Button";
        assert_eq!(ts.canonical_name_for_relocation(path), path);
    }

    // ── classify_relocation ─────────────────────────────────────

    #[test]
    fn classify_moved_to_deprecated() {
        let ts = TypeScript::default();
        assert_eq!(
            ts.classify_relocation(
                "pkg/dist/esm/components/Chip/Chip.Chip",
                "pkg/dist/esm/deprecated/components/Chip/Chip.Chip"
            ),
            Some("moved to deprecated")
        );
    }

    #[test]
    fn classify_promoted_from_deprecated() {
        let ts = TypeScript::default();
        assert_eq!(
            ts.classify_relocation(
                "pkg/dist/esm/deprecated/components/Modal/Modal.Modal",
                "pkg/dist/esm/components/Modal/Modal.Modal"
            ),
            Some("promoted from deprecated")
        );
    }

    #[test]
    fn classify_relocated_generic() {
        let ts = TypeScript::default();
        assert_eq!(
            ts.classify_relocation(
                "pkg/dist/esm/components/Chip/Chip.Chip",
                "pkg/dist/esm/components/Label/Chip.Chip"
            ),
            None
        );
    }

    #[test]
    fn classify_promoted_from_next() {
        let ts = TypeScript::default();
        assert_eq!(
            ts.classify_relocation(
                "pkg/dist/esm/next/components/Modal/ModalBody.ModalBody",
                "pkg/dist/esm/components/Modal/ModalBody.ModalBody"
            ),
            Some("promoted from next")
        );
    }

    #[test]
    fn classify_moved_to_next() {
        let ts = TypeScript::default();
        assert_eq!(
            ts.classify_relocation(
                "pkg/dist/esm/components/Foo/Foo.Foo",
                "pkg/dist/esm/next/components/Foo/Foo.Foo"
            ),
            Some("moved to next")
        );
    }

    // ── Deterministic hierarchy tests ───────────────────────────────

    fn make_component(name: &str, family: &str, rendered: Vec<&str>) -> Symbol {
        let mut sym = Symbol::new(
            name,
            format!("src/components/{}/{}.{}", family, name, name),
            SymbolKind::Variable,
            Visibility::Exported,
            format!("src/components/{}/{}.d.ts", family, name),
            1,
        );
        sym.language_data.rendered_components = rendered.into_iter().map(String::from).collect();
        sym
    }

    fn make_props_interface(
        name: &str,
        family: &str,
        extends: Option<&str>,
        members: &[&str],
    ) -> Symbol {
        let mut s = Symbol::new(
            name,
            format!("src/components/{}/{}.{}", family, name, name),
            SymbolKind::Interface,
            Visibility::Exported,
            format!("src/components/{}/{}.d.ts", family, name),
            1,
        );
        s.extends = extends.map(|e| e.to_string());
        for &member_name in members {
            s.members.push(Symbol::new(
                member_name,
                format!("{}.{}", name, member_name),
                SymbolKind::Variable,
                Visibility::Exported,
                format!("src/components/{}/{}.d.ts", family, name),
                1,
            ));
        }
        s
    }

    fn removed_member(parent: &str, member: &str) -> StructuralChange {
        use semver_analyzer_core::ChangeSubject;
        StructuralChange {
            symbol: format!("{}.{}", parent, member),
            qualified_name: format!("src/components/X/{}.{}", parent, member),
            kind: SymbolKind::Interface,
            package: None,
            change_type: StructuralChangeType::Removed(ChangeSubject::Member {
                name: member.to_string(),
                kind: SymbolKind::Variable,
            }),
            before: None,
            after: None,
            description: format!("property `{}` was removed", member),
            is_breaking: true,
            impact: None,
            migration_target: None,
        }
    }

    fn child_names(
        result: &std::collections::HashMap<
            String,
            std::collections::HashMap<String, Vec<ExpectedChild>>,
        >,
        family: &str,
        component: &str,
    ) -> Vec<String> {
        result
            .get(family)
            .and_then(|f| f.get(component))
            .map(|children| children.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default()
    }

    fn child_mechanism(
        result: &std::collections::HashMap<
            String,
            std::collections::HashMap<String, Vec<ExpectedChild>>,
        >,
        family: &str,
        parent: &str,
        child: &str,
    ) -> Option<String> {
        result
            .get(family)
            .and_then(|f| f.get(parent))
            .and_then(|children| children.iter().find(|c| c.name == child))
            .map(|c| c.mechanism.clone())
    }

    #[test]
    fn hierarchy_all_leaves_empty() {
        let ts = TypeScript::default();
        let surface = ApiSurface {
            symbols: vec![
                make_component("Masthead", "Masthead", vec![]),
                make_component("MastheadBrand", "Masthead", vec![]),
                make_component("MastheadContent", "Masthead", vec![]),
                make_component("MastheadLogo", "Masthead", vec![]),
                make_component("MastheadMain", "Masthead", vec![]),
                make_component("MastheadToggle", "Masthead", vec![]),
            ],
        };
        let result = ts.compute_deterministic_hierarchy(&surface, &[]);
        assert!(
            !result.contains_key("Masthead"),
            "All leaves → no hierarchy entry"
        );
    }

    #[test]
    fn hierarchy_no_signals_empty() {
        let ts = TypeScript::default();
        let surface = ApiSurface {
            symbols: vec![
                make_component("Modal", "Modal", vec![]),
                make_component("ModalHeader", "Modal", vec![]),
            ],
        };
        let result = ts.compute_deterministic_hierarchy(&surface, &[]);
        assert!(result.is_empty(), "No signals → empty hierarchy");
    }

    #[test]
    fn hierarchy_interfaces_excluded() {
        let ts = TypeScript::default();
        let surface = ApiSurface {
            symbols: vec![
                make_component("Modal", "Modal", vec![]),
                make_component("ModalBody", "Modal", vec![]),
                make_props_interface("ModalProps", "Modal", None, &["children"]),
            ],
        };
        let changes = vec![removed_member("ModalProps", "title")];
        let result = ts.compute_deterministic_hierarchy(&surface, &changes);

        for family in result.values() {
            for children in family.values() {
                for child in children {
                    assert_ne!(
                        child.name, "ModalProps",
                        "Interfaces should not be hierarchy candidates"
                    );
                }
            }
        }
    }

    // ── Signal 1: Prop absorption ────────────────────────────────

    #[test]
    fn hierarchy_signal1_prop_absorption() {
        let ts = TypeScript::default();
        // Parent had "header" prop removed, child ModalHeader has "header" member
        let surface = ApiSurface {
            symbols: vec![
                make_component("Modal", "Modal", vec![]),
                make_component("ModalHeader", "Modal", vec![]),
                make_props_interface("ModalProps", "Modal", None, &["children"]),
                make_props_interface("ModalHeaderProps", "Modal", None, &["header", "title"]),
            ],
        };
        let changes = vec![
            removed_member("ModalProps", "header"),
            removed_member("ModalProps", "title"),
        ];
        let result = ts.compute_deterministic_hierarchy(&surface, &changes);
        let children = child_names(&result, "Modal", "Modal");
        assert!(
            children.contains(&"ModalHeader".to_string()),
            "ModalHeader absorbed removed props from Modal"
        );
    }

    #[test]
    fn hierarchy_signal1_internally_rendered_is_prop_passed() {
        let ts = TypeScript::default();
        // Modal renders ModalHeader internally → mechanism should be "prop"
        let surface = ApiSurface {
            symbols: vec![
                make_component("Modal", "Modal", vec!["ModalHeader"]),
                make_component("ModalHeader", "Modal", vec![]),
                make_props_interface("ModalProps", "Modal", None, &["children"]),
                make_props_interface("ModalHeaderProps", "Modal", None, &["header"]),
            ],
        };
        let changes = vec![removed_member("ModalProps", "header")];
        let result = ts.compute_deterministic_hierarchy(&surface, &changes);
        assert_eq!(
            child_mechanism(&result, "Modal", "Modal", "ModalHeader"),
            Some("prop".to_string()),
            "Internally rendered child uses prop mechanism"
        );
    }

    #[test]
    fn hierarchy_signal1_not_rendered_is_child() {
        let ts = TypeScript::default();
        // Modal does NOT render ModalBody → mechanism should be "child"
        let surface = ApiSurface {
            symbols: vec![
                make_component("Modal", "Modal", vec![]),
                make_component("ModalBody", "Modal", vec![]),
                make_props_interface("ModalProps", "Modal", None, &["children"]),
                make_props_interface("ModalBodyProps", "Modal", None, &["bodyContent"]),
            ],
        };
        let changes = vec![removed_member("ModalProps", "bodyContent")];
        let result = ts.compute_deterministic_hierarchy(&surface, &changes);
        assert_eq!(
            child_mechanism(&result, "Modal", "Modal", "ModalBody"),
            Some("child".to_string()),
            "Non-rendered child uses child mechanism"
        );
    }

    // ── Signal 2: Cross-family extends ──────────────────────────

    #[test]
    fn hierarchy_signal2_cross_family_extends() {
        let ts = TypeScript::default();
        // Dropdown renders Menu (cross-family). DropdownList extends MenuListProps.
        // Menu is a container (renders MenuItem) but does NOT render MenuList
        // internally — MenuList is a consumer-placed child. So DropdownList
        // should also be a consumer-placed child of Dropdown.
        let surface = ApiSurface {
            symbols: vec![
                // Menu family: Menu renders MenuItem (making it a container),
                // but NOT MenuList (consumer places MenuList)
                make_component("Menu", "Menu", vec!["MenuItem"]),
                make_component("MenuList", "Menu", vec![]),
                make_component("MenuItem", "Menu", vec![]),
                make_props_interface("MenuProps", "Menu", None, &["children"]),
                make_props_interface("MenuListProps", "Menu", None, &["items"]),
                make_props_interface("MenuItemProps", "Menu", None, &["label"]),
                // Dropdown family
                make_component("Dropdown", "Dropdown", vec!["Menu"]),
                make_component("DropdownList", "Dropdown", vec![]),
                make_props_interface(
                    "DropdownProps",
                    "Dropdown",
                    Some("MenuProps"),
                    &["children"],
                ),
                make_props_interface(
                    "DropdownListProps",
                    "Dropdown",
                    Some("MenuListProps"),
                    &["items"],
                ),
            ],
        };
        let result = ts.compute_deterministic_hierarchy(&surface, &[]);
        let children = child_names(&result, "Dropdown", "Dropdown");
        assert!(
            children.contains(&"DropdownList".to_string()),
            "Cross-family extends: DropdownList should be child of Dropdown"
        );
    }

    #[test]
    fn hierarchy_signal2_leaf_wrapper_no_false_children() {
        let ts = TypeScript::default();
        // DropdownList extends MenuListProps but does NOT render Menu.
        // DropdownItem extends MenuItemProps.
        // DropdownList should NOT claim DropdownItem as its child
        // (only the root Dropdown that renders Menu should map children).
        let surface = ApiSurface {
            symbols: vec![
                make_component("Menu", "Menu", vec!["MenuList", "MenuItem"]),
                make_component("MenuList", "Menu", vec![]),
                make_component("MenuItem", "Menu", vec![]),
                make_props_interface("MenuProps", "Menu", None, &["children"]),
                make_props_interface("MenuListProps", "Menu", None, &["items"]),
                make_props_interface("MenuItemProps", "Menu", None, &["label"]),
                make_component("Dropdown", "Dropdown", vec!["Menu"]),
                make_component("DropdownList", "Dropdown", vec!["MenuList"]),
                make_component("DropdownItem", "Dropdown", vec![]),
                make_props_interface(
                    "DropdownProps",
                    "Dropdown",
                    Some("MenuProps"),
                    &["children"],
                ),
                make_props_interface(
                    "DropdownListProps",
                    "Dropdown",
                    Some("MenuListProps"),
                    &["items"],
                ),
                make_props_interface(
                    "DropdownItemProps",
                    "Dropdown",
                    Some("MenuItemProps"),
                    &["label"],
                ),
            ],
        };
        let result = ts.compute_deterministic_hierarchy(&surface, &[]);
        // DropdownList is a leaf wrapper — it should NOT have children
        let dl_children = child_names(&result, "Dropdown", "DropdownList");
        assert!(
            dl_children.is_empty(),
            "Leaf wrapper DropdownList should not have children"
        );
    }

    // ── Signal 3: Internal rendering ────────────────────────────

    #[test]
    fn hierarchy_signal3_internal_render_with_absorption() {
        let ts = TypeScript::default();
        // Alert renders AlertIcon internally. AlertIcon absorbed "icon" prop.
        let surface = ApiSurface {
            symbols: vec![
                make_component("Alert", "Alert", vec!["AlertIcon"]),
                make_component("AlertIcon", "Alert", vec![]),
                make_props_interface("AlertProps", "Alert", None, &["children"]),
                make_props_interface("AlertIconProps", "Alert", None, &["icon"]),
            ],
        };
        let changes = vec![removed_member("AlertProps", "icon")];
        let result = ts.compute_deterministic_hierarchy(&surface, &changes);
        assert_eq!(
            child_mechanism(&result, "Alert", "Alert", "AlertIcon"),
            Some("prop".to_string()),
            "Internally rendered child with absorption → prop mechanism"
        );
    }
}
