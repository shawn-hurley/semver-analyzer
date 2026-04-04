//! TypeScript `Language` trait implementation.
//!
//! Provides all TypeScript/React-specific semantic rules, message formatting,
//! and associated types for the multi-language architecture.
//!
//! This module extracts language-specific logic that currently lives in
//! `core/diff/compare.rs`, `core/diff/helpers.rs`, `core/diff/migration.rs`,
//! and `core/diff/mod.rs` into a trait implementation that the diff engine
//! can call through the `LanguageSemantics` and `MessageFormatter` traits.

use crate::extensions::TsAnalysisExtensions;
use crate::symbol_data::TsSymbolData;
use anyhow::Result;
use semver_analyzer_core::{
    AnalysisReport, AnalysisResult, ApiSurface, BehavioralChangeKind, BodyAnalysisResult,
    BodyAnalysisSemantics, Caller, ChangedFunction, EvidenceType, HierarchySemantics, Language,
    LanguageSemantics, ManifestChange, MessageFormatter, Reference, RenameSemantics,
    StructuralChange, StructuralChangeType, Symbol, SymbolKind, TestDiff, TestFile, Visibility,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

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

/// TypeScript-specific report data (React component analysis).
///
/// These types will eventually absorb ComponentSummary, HierarchyDelta,
/// ContainerChange, and other React-specific types currently
/// in the core crate. For now this is a placeholder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TsReportData {
    /// Placeholder -- will hold ComponentSummary, ConstantGroup, etc.
    /// when they move from core in Phase 5.
    #[serde(default)]
    pub _placeholder: (),
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

    fn hierarchy(&self) -> Option<&dyn HierarchySemantics> {
        Some(self)
    }

    fn renames(&self) -> Option<&dyn RenameSemantics> {
        Some(self)
    }

    fn body_analyzer(&self) -> Option<&dyn BodyAnalysisSemantics> {
        Some(self)
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
        // Star re-export symbols (`export * from './module'`) are barrel-file
        // directives, not actual API symbols. They share qualified_names and
        // produce noise in the diff output.
        sym.name == "*"
    }

    fn member_label(&self) -> &str {
        "props"
    }

    fn extract_rename_fallback_key(&self, symbol: &Symbol<TsSymbolData>) -> Option<String> {
        // Extract the CSS value from a token symbol's `.d.ts` type annotation.
        // Token files have type annotations like:
        //   { ["name"]: "--pf-v5-global--Color--dark-100"; ["value"]: "#151515"; ["var"]: "var(...)" }
        // This extracts the `"value"` field (e.g., `"#151515"`).
        let return_type = symbol.signature.as_ref()?.return_type.as_deref()?;

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
        // TypeScript: strip /deprecated/ and /next/ path segments from
        // qualified names so relocated symbols match their original position.
        qualified_name
            .replace("/deprecated/", "/")
            .replace("/next/", "/")
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

    fn extract(&self, repo: &Path, git_ref: &str) -> Result<ApiSurface<TsSymbolData>> {
        let extractor = crate::extract::OxcExtractor::new();
        extractor.extract_at_ref(repo, git_ref, self.build_command.as_deref())
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
        repo: &Path,
        from_ref: &str,
        to_ref: &str,
        _structural_changes: &[StructuralChange],
        _old_surface: &ApiSurface<TsSymbolData>,
        _new_surface: &ApiSurface<TsSymbolData>,
        _llm_command: Option<&str>,
        dep_css_dir: Option<&Path>,
        _no_llm: bool,
    ) -> Result<TsAnalysisExtensions> {
        let css_profiles = dep_css_dir.and_then(|dir| {
            crate::css_profile::extract_css_profiles_from_dir(dir)
                .map_err(|e| {
                    tracing::warn!(%e, "failed to extract CSS profiles from dependency");
                    e
                })
                .ok()
        });

        let sd_result = crate::sd_pipeline::run_sd(repo, from_ref, to_ref, css_profiles.as_ref())?;

        Ok(TsAnalysisExtensions {
            sd_result: Some(sd_result),
            hierarchy_deltas: Vec::new(),
            new_hierarchies: std::collections::HashMap::new(),
        })
    }
}

// ── HierarchySemantics (React component hierarchy) ─────────────────────

impl HierarchySemantics for TypeScript {
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

    fn family_name_from_symbols(&self, symbols: &[&Symbol]) -> Option<String> {
        // Extract the component directory name from the first symbol's file path
        for sym in symbols {
            let path = sym.file.to_string_lossy();
            if let Some(name) = extract_family_from_path(&path) {
                return Some(name);
            }
        }
        None
    }

    fn is_hierarchy_candidate(&self, sym: &Symbol) -> bool {
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

/// Read a file from a git ref.
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

// ── Extracted helper functions ──────────────────────────────────────────

/// Extract the component directory from a file path, stripping /deprecated/
/// and /next/ segments for canonical matching.
///
/// `packages/react-core/dist/esm/deprecated/components/Select/Select.d.ts`
/// → `packages/react-core/dist/esm/components/Select`
///
/// `packages/react-core/dist/esm/components/EmptyState/EmptyStateHeader.d.ts`
/// → `packages/react-core/dist/esm/components/EmptyState`
fn canonical_component_dir(file_path: &str) -> String {
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
    use semver_analyzer_core::{Parameter, Signature};

    fn sym(name: &str, kind: SymbolKind) -> Symbol<TsSymbolData> {
        Symbol::new(name, name, kind, Visibility::Exported, "test.d.ts", 1)
    }

    fn make_interface(name: &str, file: &str, members: &[&str]) -> Symbol<TsSymbolData> {
        let mut s = Symbol::new(
            name,
            &format!("{}.{}", file, name),
            SymbolKind::Interface,
            Visibility::Exported,
            file,
            1,
        );
        for &member_name in members {
            s.members.push(Symbol::new(
                member_name,
                &format!("{}.{}.{}", file, name, member_name),
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
}
