//! Konveyor rule generation from semver-analyzer breaking change reports.
//!
//! Transforms an `AnalysisReport` into a Konveyor-compatible ruleset directory
//! that can be consumed by `konveyor-analyzer --rules <dir>`.
//!
//! The mapping is deterministic: each breaking change type produces a specific
//! rule pattern using `builtin.filecontent` (regex) or `builtin.json` (xpath).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use semver_analyzer_core::{
    AnalysisReport, ApiChange, ApiChangeKind, ApiChangeType, BehavioralChange, FileChanges,
    ManifestChange, ManifestChangeType,
};

// ── Konveyor YAML types ─────────────────────────────────────────────────

/// Ruleset metadata (written to `ruleset.yaml`).
#[derive(Debug, Serialize)]
pub struct KonveyorRuleset {
    pub name: String,
    pub description: String,
    pub labels: Vec<String>,
}

/// A single Konveyor rule.
#[derive(Debug, Serialize)]
pub struct KonveyorRule {
    #[serde(rename = "ruleID")]
    pub rule_id: String,
    pub labels: Vec<String>,
    pub effort: u32,
    pub category: String,
    pub description: String,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<KonveyorLink>,
    pub when: KonveyorCondition,
}

/// A hyperlink attached to a rule.
#[derive(Debug, Serialize)]
pub struct KonveyorLink {
    pub url: String,
    pub title: String,
}

/// A Konveyor `when` condition.
///
/// Supports `builtin.filecontent` (regex), `builtin.json` (xpath),
/// `frontend.referenced` (AST-level, requires the frontend-analyzer-provider),
/// and `or` (disjunction of conditions).
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum KonveyorCondition {
    FileContent {
        #[serde(rename = "builtin.filecontent")]
        filecontent: FileContentFields,
    },
    Json {
        #[serde(rename = "builtin.json")]
        json: JsonFields,
    },
    FrontendReferenced {
        #[serde(rename = "frontend.referenced")]
        referenced: FrontendReferencedFields,
    },
    Or {
        or: Vec<KonveyorCondition>,
    },
}

/// Fields for a `builtin.filecontent` condition.
#[derive(Debug, Serialize)]
pub struct FileContentFields {
    pub pattern: String,
    #[serde(rename = "filePattern")]
    pub file_pattern: String,
}

/// Fields for a `builtin.json` condition.
#[derive(Debug, Serialize)]
pub struct JsonFields {
    pub xpath: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filepaths: Option<String>,
}

/// Fields for a `frontend.referenced` condition.
///
/// This condition requires the frontend-analyzer-provider gRPC server.
/// It performs AST-level symbol matching with location discriminators.
#[derive(Debug, Serialize)]
pub struct FrontendReferencedFields {
    /// Regex pattern for the symbol name.
    pub pattern: String,
    /// Where to look: IMPORT, JSX_COMPONENT, JSX_PROP, FUNCTION_CALL, TYPE_REFERENCE.
    pub location: String,
    /// Filter JSX props to only those on this component (regex).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    /// Filter JSX components to only those inside this parent (regex).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// Which Konveyor provider to target for rule generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleProvider {
    /// Use `builtin.filecontent` (regex) — works with vanilla Konveyor.
    Builtin,
    /// Use `frontend.referenced` (AST) — requires the frontend-analyzer-provider.
    Frontend,
}

// ── Fix guidance types ──────────────────────────────────────────────────

/// How to fix a detected issue.
///
/// Mirrors the frontend-analyzer-provider's fix engine: each rule is mapped
/// to a deterministic fix strategy with confidence level.
#[derive(Debug, Clone, Serialize)]
pub struct FixGuidanceEntry {
    /// The rule ID this fix corresponds to.
    #[serde(rename = "ruleID")]
    pub rule_id: String,

    /// The fix strategy to apply.
    pub strategy: FixStrategy,

    /// How confident we are this fix is correct.
    pub confidence: FixConfidence,

    /// Where this fix guidance came from.
    pub source: FixSource,

    /// The affected symbol.
    pub symbol: String,

    /// Source file where the breaking change originates.
    pub file: String,

    /// Concrete instructions for fixing the issue.
    pub fix_description: String,

    /// Example of the old code pattern (when available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,

    /// Example of the new code pattern (when available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,

    /// Search pattern to find code that needs fixing.
    pub search_pattern: String,

    /// Suggested replacement (for mechanical fixes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
}

/// What kind of fix to apply.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixStrategy {
    /// Find-and-replace: rename old symbol to new symbol.
    Rename,
    /// Update function call sites to match new signature.
    UpdateSignature,
    /// Update type annotations to match new types.
    UpdateType,
    /// Remove usages of a deleted symbol and find alternatives.
    FindAlternative,
    /// Remove a property/field that no longer exists.
    RemoveUsage,
    /// Update import paths or module system (require ↔ import).
    UpdateImport,
    /// Update package.json dependency configuration.
    UpdateDependency,
    /// Requires manual review — behavioral change or complex refactor.
    ManualReview,
}

/// How confident the fix guidance is.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixConfidence {
    /// Mechanical rename or direct replacement — safe to auto-apply.
    Exact,
    /// Pattern-based fix — likely correct but may need review.
    High,
    /// Inferred fix — needs human verification.
    Medium,
    /// Best-effort suggestion — may not be applicable.
    Low,
}

/// Where the fix guidance originates.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixSource {
    /// Deterministic — derived from structural analysis.
    Pattern,
    /// AI-generated — from LLM behavioral analysis.
    Llm,
    /// Flagged for manual intervention.
    Manual,
}

/// Top-level fix guidance document written to `fix-guidance.yaml`.
#[derive(Debug, Serialize)]
pub struct FixGuidanceDoc {
    /// Version range this guidance applies to.
    pub migration: MigrationInfo,
    /// Summary statistics.
    pub summary: FixSummary,
    /// Per-rule fix entries.
    pub fixes: Vec<FixGuidanceEntry>,
}

/// Migration metadata.
#[derive(Debug, Serialize)]
pub struct MigrationInfo {
    pub from_ref: String,
    pub to_ref: String,
    pub generated_by: String,
}

/// Summary of fix guidance.
#[derive(Debug, Serialize)]
pub struct FixSummary {
    pub total_fixes: usize,
    pub auto_fixable: usize,
    pub needs_review: usize,
    pub manual_only: usize,
}

// ── Public API ───────────────────────────────────────────────────────────

/// Generate Konveyor rules from an `AnalysisReport`.
///
/// Each breaking API change, behavioral change, and manifest change
/// produces one rule. The mapping is fully deterministic.
///
/// When `provider` is `Frontend`, API change rules use `frontend.referenced`
/// conditions with AST-level location discriminators (JSX_COMPONENT, JSX_PROP,
/// IMPORT, etc.). When `Builtin`, rules use `builtin.filecontent` regex patterns.
pub fn generate_rules(
    report: &AnalysisReport,
    file_pattern: &str,
    provider: RuleProvider,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();
    let mut id_counts: HashMap<String, usize> = HashMap::new();

    // API changes (per-file)
    for file_changes in &report.changes {
        for api_change in &file_changes.breaking_api_changes {
            let rule = api_change_to_rule(
                api_change,
                file_changes,
                file_pattern,
                provider,
                &mut id_counts,
            );
            rules.push(rule);
        }

        for behavioral in &file_changes.breaking_behavioral_changes {
            let rule =
                behavioral_change_to_rule(behavioral, file_changes, file_pattern, &mut id_counts);
            rules.push(rule);
        }
    }

    // Manifest changes
    for manifest in &report.manifest_changes {
        let rule = manifest_change_to_rule(manifest, file_pattern, &mut id_counts);
        rules.push(rule);
    }

    rules
}

/// Generate fix guidance entries from an `AnalysisReport`.
///
/// Each rule gets a corresponding fix entry describing what to do about
/// the breaking change: strategy, confidence, concrete instructions, and
/// before/after examples where available.
pub fn generate_fix_guidance(
    report: &AnalysisReport,
    rules: &[KonveyorRule],
    file_pattern: &str,
) -> FixGuidanceDoc {
    let mut fixes = Vec::new();
    let mut rule_idx = 0;

    // API + behavioral changes (per-file, in same order as generate_rules)
    for file_changes in &report.changes {
        for api_change in &file_changes.breaking_api_changes {
            if rule_idx < rules.len() {
                let fix = api_change_to_fix(
                    api_change,
                    file_changes,
                    &rules[rule_idx].rule_id,
                    file_pattern,
                );
                fixes.push(fix);
                rule_idx += 1;
            }
        }
        for behavioral in &file_changes.breaking_behavioral_changes {
            if rule_idx < rules.len() {
                let fix =
                    behavioral_change_to_fix(behavioral, file_changes, &rules[rule_idx].rule_id);
                fixes.push(fix);
                rule_idx += 1;
            }
        }
    }

    // Manifest changes
    for manifest in &report.manifest_changes {
        if rule_idx < rules.len() {
            let fix = manifest_change_to_fix(manifest, &rules[rule_idx].rule_id);
            fixes.push(fix);
            rule_idx += 1;
        }
    }

    let auto_fixable = fixes
        .iter()
        .filter(|f| matches!(f.confidence, FixConfidence::Exact | FixConfidence::High))
        .count();
    let manual_only = fixes
        .iter()
        .filter(|f| matches!(f.source, FixSource::Manual))
        .count();
    let needs_review = fixes.len() - auto_fixable - manual_only;

    FixGuidanceDoc {
        migration: MigrationInfo {
            from_ref: report.comparison.from_ref.clone(),
            to_ref: report.comparison.to_ref.clone(),
            generated_by: format!("semver-analyzer v{}", report.metadata.tool_version),
        },
        summary: FixSummary {
            total_fixes: fixes.len(),
            auto_fixable,
            needs_review,
            manual_only,
        },
        fixes,
    }
}

/// Write a Konveyor ruleset directory.
///
/// Creates:
///   `<output_dir>/ruleset.yaml`         — ruleset metadata
///   `<output_dir>/breaking-changes.yaml` — all generated rules
pub fn write_ruleset_dir(
    output_dir: &Path,
    ruleset_name: &str,
    report: &AnalysisReport,
    rules: &[KonveyorRule],
) -> Result<()> {
    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create output directory {}", output_dir.display()))?;

    // Write ruleset.yaml
    let from_ref = &report.comparison.from_ref;
    let to_ref = &report.comparison.to_ref;
    let ruleset = KonveyorRuleset {
        name: ruleset_name.to_string(),
        description: format!(
            "Breaking changes detected between {} and {} by semver-analyzer v{}",
            from_ref, to_ref, report.metadata.tool_version
        ),
        labels: vec!["source=semver-analyzer".to_string()],
    };

    let ruleset_path = output_dir.join("ruleset.yaml");
    let ruleset_yaml = serde_yaml::to_string(&ruleset).context("Failed to serialize ruleset")?;
    std::fs::write(&ruleset_path, &ruleset_yaml)
        .with_context(|| format!("Failed to write {}", ruleset_path.display()))?;

    // Write rules file
    let rules_path = output_dir.join("breaking-changes.yaml");
    let rules_yaml = serde_yaml::to_string(&rules).context("Failed to serialize rules")?;
    std::fs::write(&rules_path, &rules_yaml)
        .with_context(|| format!("Failed to write {}", rules_path.display()))?;

    Ok(())
}

/// Write fix guidance to a separate sibling directory.
///
/// Given the ruleset `output_dir`, creates a `fix-guidance/` directory
/// next to it and writes `fix-guidance.yaml` there.
///
/// Example: if `output_dir` is `./rules`, writes to `./fix-guidance/fix-guidance.yaml`.
pub fn write_fix_guidance_dir(
    output_dir: &Path,
    fix_guidance: &FixGuidanceDoc,
) -> Result<std::path::PathBuf> {
    let fix_dir = fix_guidance_dir_for(output_dir);

    std::fs::create_dir_all(&fix_dir).with_context(|| {
        format!(
            "Failed to create fix guidance directory {}",
            fix_dir.display()
        )
    })?;

    let fix_path = fix_dir.join("fix-guidance.yaml");
    let fix_yaml =
        serde_yaml::to_string(fix_guidance).context("Failed to serialize fix guidance")?;
    std::fs::write(&fix_path, &fix_yaml)
        .with_context(|| format!("Failed to write {}", fix_path.display()))?;

    Ok(fix_dir)
}

/// Compute the fix-guidance sibling directory path for a given ruleset output dir.
///
/// `./my-rules` → `./fix-guidance`
/// `./output/rules` → `./output/fix-guidance`
pub fn fix_guidance_dir_for(output_dir: &Path) -> std::path::PathBuf {
    let parent = output_dir.parent().unwrap_or(Path::new("."));
    parent.join("fix-guidance")
}

// ── Rule generators ─────────────────────────────────────────────────────

fn api_change_to_rule(
    change: &ApiChange,
    file_changes: &FileChanges,
    file_pattern: &str,
    provider: RuleProvider,
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    let file_path = file_changes.file.display().to_string();
    let leaf_symbol = extract_leaf_symbol(&change.symbol);
    let effort = effort_for_api_change(&change.change);
    let change_type_label = api_change_type_label(&change.change);

    let base_id = format!(
        "semver-{}-{}-{}",
        sanitize_id(&file_path),
        sanitize_id(&change.symbol),
        change_type_label,
    );
    let rule_id = unique_id(base_id, id_counts);

    let message = build_api_message(change, &file_path);

    let mut labels = vec![
        "source=semver-analyzer".to_string(),
        format!("change-type={}", change_type_label),
        format!("kind={}", api_kind_label(&change.kind)),
    ];

    // Infer has-codemod from the change type
    let has_codemod = matches!(
        change.change,
        ApiChangeType::Renamed | ApiChangeType::SignatureChanged | ApiChangeType::TypeChanged
    );
    labels.push(format!("has-codemod={}", has_codemod));

    let condition = if provider == RuleProvider::Frontend {
        build_frontend_condition(change, leaf_symbol)
    } else {
        let pattern = build_pattern(&change.kind, &change.change, leaf_symbol, &change.before);
        KonveyorCondition::FileContent {
            filecontent: FileContentFields {
                pattern,
                file_pattern: file_pattern.to_string(),
            },
        }
    };

    KonveyorRule {
        rule_id,
        labels,
        effort,
        category: "mandatory".to_string(),
        description: change.description.clone(),
        message,
        links: Vec::new(),
        when: condition,
    }
}

fn behavioral_change_to_rule(
    change: &BehavioralChange,
    file_changes: &FileChanges,
    file_pattern: &str,
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    let file_path = file_changes.file.display().to_string();
    let leaf_symbol = extract_leaf_symbol(&change.symbol);
    let pattern = format!(r"\b{}\b", regex_escape(leaf_symbol));

    let base_id = format!(
        "semver-{}-{}-behavioral",
        sanitize_id(&file_path),
        sanitize_id(&change.symbol),
    );
    let rule_id = unique_id(base_id, id_counts);

    let message = format!(
        "Behavioral change in '{}': {}\n\nFile: {}\nReview all usages to ensure compatibility with the new behavior.",
        change.symbol, change.description, file_path,
    );

    let mut labels = vec![
        "source=semver-analyzer".to_string(),
        "ai-generated".to_string(),
    ];

    // Use the behavioral category for more precise change-type labels
    if let Some(ref cat) = change.category {
        labels.push(format!("change-type={}", behavioral_category_label(cat)));
        // DOM, CSS, a11y, and behavioral changes primarily impact frontend testing
        if matches!(
            cat,
            semver_analyzer_core::BehavioralCategory::DomStructure
                | semver_analyzer_core::BehavioralCategory::CssClass
                | semver_analyzer_core::BehavioralCategory::CssVariable
                | semver_analyzer_core::BehavioralCategory::Accessibility
                | semver_analyzer_core::BehavioralCategory::DataAttribute
        ) {
            labels.push("impact=frontend-testing".to_string());
        }
    } else {
        labels.push("change-type=behavioral".to_string());
    }

    KonveyorRule {
        rule_id,
        labels,
        effort: 3,
        category: "mandatory".to_string(),
        description: change.description.clone(),
        message,
        links: Vec::new(),
        when: KonveyorCondition::FileContent {
            filecontent: FileContentFields {
                pattern,
                file_pattern: file_pattern.to_string(),
            },
        },
    }
}

fn manifest_change_to_rule(
    change: &ManifestChange,
    file_pattern: &str,
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    let change_type_label = manifest_change_type_label(&change.change_type);

    let base_id = format!(
        "semver-manifest-{}-{}",
        sanitize_id(&change.field),
        change_type_label,
    );
    let rule_id = unique_id(base_id, id_counts);

    let category = if change.is_breaking {
        "mandatory"
    } else {
        "optional"
    };

    let effort = manifest_effort(&change.change_type);

    let (condition, message) =
        build_manifest_condition_and_message(change, file_pattern, change_type_label);

    KonveyorRule {
        rule_id,
        labels: vec![
            "source=semver-analyzer".to_string(),
            "change-type=manifest".to_string(),
            format!("manifest-field={}", change.field),
        ],
        effort,
        category: category.to_string(),
        description: change.description.clone(),
        message,
        links: Vec::new(),
        when: condition,
    }
}

// ── Fix guidance generators ─────────────────────────────────────────────

fn api_change_to_fix(
    change: &ApiChange,
    file_changes: &FileChanges,
    rule_id: &str,
    file_pattern: &str,
) -> FixGuidanceEntry {
    let file_path = file_changes.file.display().to_string();
    let leaf_symbol = extract_leaf_symbol(&change.symbol);
    let search_pattern = build_pattern(&change.kind, &change.change, leaf_symbol, &change.before);

    let (strategy, confidence, source, fix_description, replacement) = match change.change {
        ApiChangeType::Renamed => {
            let old_name = change
                .before
                .as_deref()
                .map(|b| extract_leaf_symbol(b).to_string())
                .unwrap_or_else(|| change.symbol.clone());
            let new_name = change
                .after
                .as_deref()
                .map(|a| extract_leaf_symbol(a).to_string())
                .unwrap_or_else(|| change.symbol.clone());

            let desc = format!(
                "Rename all occurrences of '{}' to '{}'.\n\
                 This is a mechanical find-and-replace that can be auto-applied.\n\
                 Search pattern: {} (in {} files)",
                old_name, new_name, search_pattern, file_pattern,
            );
            (
                FixStrategy::Rename,
                FixConfidence::Exact,
                FixSource::Pattern,
                desc,
                Some(new_name),
            )
        }

        ApiChangeType::SignatureChanged => {
            let desc = if let (Some(ref before), Some(ref after)) = (&change.before, &change.after)
            {
                format!(
                    "Update all call sites of '{}' to match the new signature.\n\n\
                     Old signature: {}\n\
                     New signature: {}\n\n\
                     Review each call site and adjust arguments accordingly.\n\
                     {}",
                    change.symbol, before, after, change.description,
                )
            } else {
                format!(
                    "Update all call sites of '{}' to match the new signature.\n\
                     {}\n\n\
                     Review each usage and adjust arguments, type parameters, or \
                     modifiers as described above.",
                    change.symbol, change.description,
                )
            };

            (
                FixStrategy::UpdateSignature,
                FixConfidence::High,
                FixSource::Pattern,
                desc,
                None,
            )
        }

        ApiChangeType::TypeChanged => {
            let desc = if let (Some(ref before), Some(ref after)) = (&change.before, &change.after)
            {
                format!(
                    "Update type annotations from '{}' to '{}'.\n\n\
                     Old type: {}\n\
                     New type: {}\n\n\
                     Check all locations where this type is used in assignments, \
                     function parameters, return types, and generic type arguments.\n\
                     {}",
                    change.symbol, change.symbol, before, after, change.description,
                )
            } else {
                format!(
                    "Update type references for '{}'.\n\
                     {}\n\n\
                     Check all locations where this type is used and update accordingly.",
                    change.symbol, change.description,
                )
            };

            (
                FixStrategy::UpdateType,
                FixConfidence::High,
                FixSource::Pattern,
                desc,
                None,
            )
        }

        ApiChangeType::Removed => {
            let kind_label = api_kind_label(&change.kind);
            let desc = format!(
                "The {} '{}' has been removed.\n\n\
                 Action required:\n\
                 1. Find all usages of '{}' in your codebase\n\
                 2. Identify an appropriate replacement (check the library's \
                    migration guide or changelog)\n\
                 3. Update each usage to use the replacement\n\
                 4. Remove any imports of '{}'\n\n\
                 {}",
                kind_label, change.symbol, change.symbol, change.symbol, change.description,
            );

            (
                FixStrategy::FindAlternative,
                FixConfidence::Low,
                FixSource::Manual,
                desc,
                None,
            )
        }

        ApiChangeType::VisibilityChanged => {
            let desc = format!(
                "The visibility of '{}' has been reduced.\n\n\
                 If you are importing or using '{}' from outside its module, \
                 you need to find a public alternative.\n\
                 {}\n\n\
                 Check if there is a new public API that exposes the same functionality, \
                 or refactor your code to avoid depending on this internal symbol.",
                change.symbol, change.symbol, change.description,
            );

            (
                FixStrategy::FindAlternative,
                FixConfidence::Medium,
                FixSource::Pattern,
                desc,
                None,
            )
        }
    };

    FixGuidanceEntry {
        rule_id: rule_id.to_string(),
        strategy,
        confidence,
        source,
        symbol: change.symbol.clone(),
        file: file_path,
        fix_description,
        before: change.before.clone(),
        after: change.after.clone(),
        search_pattern,
        replacement,
    }
}

fn behavioral_change_to_fix(
    change: &BehavioralChange,
    file_changes: &FileChanges,
    rule_id: &str,
) -> FixGuidanceEntry {
    let file_path = file_changes.file.display().to_string();
    let leaf_symbol = extract_leaf_symbol(&change.symbol);
    let search_pattern = format!(r"\b{}\b", regex_escape(leaf_symbol));

    let fix_description = format!(
        "Behavioral change detected in '{}' (AI-generated finding).\n\n\
         What changed: {}\n\n\
         Action required:\n\
         1. Review all usages of '{}' in your codebase\n\
         2. Verify that your code handles the new behavior correctly\n\
         3. Update tests that depend on the old behavior\n\
         4. Pay special attention to edge cases and error handling\n\n\
         This finding was generated by LLM analysis and should be \
         verified by a developer.",
        change.symbol, change.description, change.symbol,
    );

    FixGuidanceEntry {
        rule_id: rule_id.to_string(),
        strategy: FixStrategy::ManualReview,
        confidence: FixConfidence::Medium,
        source: FixSource::Llm,
        symbol: change.symbol.clone(),
        file: file_path,
        fix_description,
        before: None,
        after: None,
        search_pattern,
        replacement: None,
    }
}

fn manifest_change_to_fix(change: &ManifestChange, rule_id: &str) -> FixGuidanceEntry {
    let (strategy, confidence, source, fix_description, search, replacement) =
        match change.change_type {
            ManifestChangeType::ModuleSystemChanged => {
                let is_cjs_to_esm = change
                    .after
                    .as_deref()
                    .map(|a| a == "module")
                    .unwrap_or(false);

                if is_cjs_to_esm {
                    (
                        FixStrategy::UpdateImport,
                        FixConfidence::High,
                        FixSource::Pattern,
                        format!(
                            "The package has changed from CommonJS to ESM.\n\n\
                             Action required:\n\
                             1. Convert all require() calls to import statements:\n\
                             \n\
                             Before: const {{ foo }} = require('package')\n\
                             After:  import {{ foo }} from 'package'\n\
                             \n\
                             2. Convert module.exports to export statements:\n\
                             \n\
                             Before: module.exports = {{ foo }}\n\
                             After:  export {{ foo }}\n\
                             \n\
                             3. Update your package.json \"type\" field if needed\n\
                             4. Rename .js files to .mjs if mixing module systems\n\n\
                             {}",
                            change.description,
                        ),
                        r"\brequire\s*\(".to_string(),
                        Some("import".to_string()),
                    )
                } else {
                    (
                        FixStrategy::UpdateImport,
                        FixConfidence::High,
                        FixSource::Pattern,
                        format!(
                            "The package has changed from ESM to CommonJS.\n\n\
                             Action required:\n\
                             1. Convert all import statements to require() calls:\n\
                             \n\
                             Before: import {{ foo }} from 'package'\n\
                             After:  const {{ foo }} = require('package')\n\
                             \n\
                             2. Convert export statements to module.exports\n\
                             3. Update your package.json \"type\" field if needed\n\n\
                             {}",
                            change.description,
                        ),
                        r"\bimport\s+".to_string(),
                        Some("require".to_string()),
                    )
                }
            }

            ManifestChangeType::PeerDependencyAdded => (
                FixStrategy::UpdateDependency,
                FixConfidence::Exact,
                FixSource::Pattern,
                format!(
                    "A new peer dependency has been added: '{}'\n\n\
                     Action required:\n\
                     1. Install the peer dependency: npm install {}\n\
                     2. Verify version compatibility with your existing dependencies\n\n\
                     {}",
                    change.field, change.field, change.description,
                ),
                change.field.clone(),
                change.after.clone(),
            ),

            ManifestChangeType::PeerDependencyRemoved => (
                FixStrategy::UpdateDependency,
                FixConfidence::High,
                FixSource::Pattern,
                format!(
                    "Peer dependency '{}' has been removed.\n\n\
                     Action required:\n\
                     1. Check if you still need '{}' as a direct dependency\n\
                     2. If it was only required by this package, you may be able \
                        to remove it\n\
                     3. Verify that removing it doesn't break other dependencies\n\n\
                     {}",
                    change.field, change.field, change.description,
                ),
                change.field.clone(),
                None,
            ),

            ManifestChangeType::PeerDependencyRangeChanged => (
                FixStrategy::UpdateDependency,
                FixConfidence::High,
                FixSource::Pattern,
                format!(
                    "Peer dependency '{}' version range changed.\n\n\
                     Before: {}\n\
                     After:  {}\n\n\
                     Action required:\n\
                     1. Update '{}' to a version that satisfies the new range\n\
                     2. Test for compatibility with the new version\n\n\
                     {}",
                    change.field,
                    change.before.as_deref().unwrap_or("(none)"),
                    change.after.as_deref().unwrap_or("(none)"),
                    change.field,
                    change.description,
                ),
                change.field.clone(),
                change.after.clone(),
            ),

            ManifestChangeType::EntryPointChanged | ManifestChangeType::ExportsEntryRemoved => (
                FixStrategy::UpdateImport,
                FixConfidence::Medium,
                FixSource::Pattern,
                format!(
                    "Package entry point or export map changed for '{}'.\n\n\
                     Before: {}\n\
                     After:  {}\n\n\
                     Action required:\n\
                     1. Update all import paths that reference the old entry point\n\
                     2. Check the package's export map for the new path\n\n\
                     {}",
                    change.field,
                    change.before.as_deref().unwrap_or("(none)"),
                    change.after.as_deref().unwrap_or("(none)"),
                    change.description,
                ),
                change.field.clone(),
                change.after.clone(),
            ),

            _ => (
                FixStrategy::ManualReview,
                FixConfidence::Medium,
                FixSource::Pattern,
                format!(
                    "Package manifest field '{}' changed.\n\n\
                     Before: {}\n\
                     After:  {}\n\n\
                     Review the change and update your configuration accordingly.\n\n\
                     {}",
                    change.field,
                    change.before.as_deref().unwrap_or("(none)"),
                    change.after.as_deref().unwrap_or("(none)"),
                    change.description,
                ),
                change.field.clone(),
                None,
            ),
        };

    FixGuidanceEntry {
        rule_id: rule_id.to_string(),
        strategy,
        confidence,
        source,
        symbol: change.field.clone(),
        file: "package.json".to_string(),
        fix_description,
        before: change.before.clone(),
        after: change.after.clone(),
        search_pattern: search,
        replacement,
    }
}

// ── Pattern building ────────────────────────────────────────────────────

/// Build a regex pattern for detecting usage of a changed symbol.
///
/// The pattern varies by the kind of symbol and the type of change:
/// - functions/methods: `\bname\s*\(` to match call sites
/// - properties/fields: `\.name\b` to match property access
/// - classes/interfaces/types: `\bname\b` to match any reference
/// - renamed symbols: match the OLD name from `before`
fn build_pattern(
    kind: &ApiChangeKind,
    change: &ApiChangeType,
    leaf_symbol: &str,
    before: &Option<String>,
) -> String {
    // For renames, match the old name
    let name = if *change == ApiChangeType::Renamed {
        if let Some(ref before_val) = before {
            // before might be a full signature; extract just the symbol name
            extract_leaf_symbol(before_val)
        } else {
            leaf_symbol
        }
    } else {
        leaf_symbol
    };

    let escaped = regex_escape(name);

    match kind {
        ApiChangeKind::Function | ApiChangeKind::Method => {
            format!(r"\b{}\s*\(", escaped)
        }
        ApiChangeKind::Property | ApiChangeKind::Field => {
            format!(r"\.{}\b", escaped)
        }
        _ => {
            // class, interface, type_alias, constant, struct, trait, module_export
            format!(r"\b{}\b", escaped)
        }
    }
}

/// Build a `frontend.referenced` condition for an API change.
///
/// Maps `ApiChangeKind` to the appropriate `location` discriminator
/// and extracts `component` filter for property-level changes.
///
/// For renames, generates an `or:` condition matching both JSX_COMPONENT
/// and IMPORT locations (same pattern as hand-crafted rules).
fn build_frontend_condition(change: &ApiChange, leaf_symbol: &str) -> KonveyorCondition {
    // For renames, match the OLD name
    let match_name = if change.change == ApiChangeType::Renamed {
        change
            .before
            .as_deref()
            .map(|b| extract_leaf_symbol(b))
            .unwrap_or(leaf_symbol)
    } else {
        leaf_symbol
    };

    let pattern = format!("^{}$", regex_escape(match_name));

    // Extract parent component for property/field changes
    // e.g., "Card.isFlat" → component="Card", prop="isFlat"
    let parent_component = if change.symbol.contains('.') {
        let parts: Vec<&str> = change.symbol.splitn(2, '.').collect();
        Some(format!("^{}$", regex_escape(parts[0])))
    } else {
        None
    };

    match change.kind {
        // Class/Interface used as JSX component → match both JSX and IMPORT
        ApiChangeKind::Class | ApiChangeKind::Interface
            if change.change == ApiChangeType::Renamed =>
        {
            KonveyorCondition::Or {
                or: vec![
                    KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: pattern.clone(),
                            location: "JSX_COMPONENT".to_string(),
                            component: None,
                            parent: None,
                        },
                    },
                    KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern,
                            location: "IMPORT".to_string(),
                            component: None,
                            parent: None,
                        },
                    },
                ],
            }
        }

        // Class/Interface used as JSX component
        ApiChangeKind::Class | ApiChangeKind::Interface => KonveyorCondition::FrontendReferenced {
            referenced: FrontendReferencedFields {
                pattern,
                location: "JSX_COMPONENT".to_string(),
                component: None,
                parent: None,
            },
        },

        // Property/Field → match as JSX prop, scoped to parent component
        ApiChangeKind::Property | ApiChangeKind::Field => KonveyorCondition::FrontendReferenced {
            referenced: FrontendReferencedFields {
                pattern,
                location: "JSX_PROP".to_string(),
                component: parent_component,
                parent: None,
            },
        },

        // Function/Method → match as function call
        ApiChangeKind::Function | ApiChangeKind::Method => KonveyorCondition::FrontendReferenced {
            referenced: FrontendReferencedFields {
                pattern,
                location: "FUNCTION_CALL".to_string(),
                component: None,
                parent: None,
            },
        },

        // TypeAlias/Interface (non-rename) → match as type reference
        ApiChangeKind::TypeAlias => KonveyorCondition::FrontendReferenced {
            referenced: FrontendReferencedFields {
                pattern,
                location: "TYPE_REFERENCE".to_string(),
                component: None,
                parent: None,
            },
        },

        // Constants, module exports, structs, traits → match as import
        _ => KonveyorCondition::FrontendReferenced {
            referenced: FrontendReferencedFields {
                pattern,
                location: "IMPORT".to_string(),
                component: None,
                parent: None,
            },
        },
    }
}

/// Build the condition and message for a manifest change.
fn build_manifest_condition_and_message(
    change: &ManifestChange,
    file_pattern: &str,
    change_type_label: &str,
) -> (KonveyorCondition, String) {
    match change.change_type {
        ManifestChangeType::ModuleSystemChanged => {
            let is_cjs_to_esm = change
                .after
                .as_deref()
                .map(|a| a == "module")
                .unwrap_or(false);

            let (pattern, hint) = if is_cjs_to_esm {
                (
                    r"\brequire\s*\(".to_string(),
                    "Convert require() calls to ESM import statements.",
                )
            } else {
                (
                    r"\bimport\s+".to_string(),
                    "Convert ESM import statements to require() calls.",
                )
            };

            let message = format!(
                "Module system changed: {}\n\nBefore: {}\nAfter: {}\n{}",
                change.description,
                change.before.as_deref().unwrap_or("(none)"),
                change.after.as_deref().unwrap_or("(none)"),
                hint,
            );

            (
                KonveyorCondition::FileContent {
                    filecontent: FileContentFields {
                        pattern,
                        file_pattern: file_pattern.to_string(),
                    },
                },
                message,
            )
        }
        ManifestChangeType::PeerDependencyAdded
        | ManifestChangeType::PeerDependencyRemoved
        | ManifestChangeType::PeerDependencyRangeChanged => {
            let message = format!(
                "Peer dependency change ({}): {}\n\nField: {}\nBefore: {}\nAfter: {}",
                change_type_label,
                change.description,
                change.field,
                change.before.as_deref().unwrap_or("(none)"),
                change.after.as_deref().unwrap_or("(none)"),
            );

            (
                KonveyorCondition::Json {
                    json: JsonFields {
                        xpath: format!("//peerDependencies/{}", change.field),
                        filepaths: Some("package.json".to_string()),
                    },
                },
                message,
            )
        }
        _ => {
            // Generic manifest change: use filecontent to match the field name
            let message = format!(
                "Package manifest change ({}): {}\n\nField: {}\nBefore: {}\nAfter: {}",
                change_type_label,
                change.description,
                change.field,
                change.before.as_deref().unwrap_or("(none)"),
                change.after.as_deref().unwrap_or("(none)"),
            );

            (
                KonveyorCondition::Json {
                    json: JsonFields {
                        xpath: format!("//{}", change.field),
                        filepaths: Some("package.json".to_string()),
                    },
                },
                message,
            )
        }
    }
}

// ── Message building ────────────────────────────────────────────────────

fn build_api_message(change: &ApiChange, file_path: &str) -> String {
    let change_verb = match change.change {
        ApiChangeType::Removed => "was removed",
        ApiChangeType::SignatureChanged => "had its signature changed",
        ApiChangeType::TypeChanged => "had its type changed",
        ApiChangeType::VisibilityChanged => "had its visibility changed",
        ApiChangeType::Renamed => "was renamed",
    };

    let kind_label = api_kind_label(&change.kind);

    let mut msg = format!(
        "{} '{}' {} ({}): {}\n\nFile: {}",
        capitalize(kind_label),
        change.symbol,
        change_verb,
        kind_label,
        change.description,
        file_path,
    );

    if let Some(ref before) = change.before {
        msg.push_str(&format!("\nBefore: {}", before));
    }
    if let Some(ref after) = change.after {
        msg.push_str(&format!("\nAfter: {}", after));
    }

    msg
}

// ── Effort mapping ──────────────────────────────────────────────────────

fn effort_for_api_change(change: &ApiChangeType) -> u32 {
    match change {
        ApiChangeType::Removed => 5,
        ApiChangeType::SignatureChanged => 3,
        ApiChangeType::TypeChanged => 3,
        ApiChangeType::VisibilityChanged => 3,
        ApiChangeType::Renamed => 1,
    }
}

fn manifest_effort(change_type: &ManifestChangeType) -> u32 {
    match change_type {
        ManifestChangeType::ModuleSystemChanged => 7,
        ManifestChangeType::EntryPointChanged => 5,
        ManifestChangeType::ExportsEntryRemoved => 5,
        ManifestChangeType::ExportsConditionRemoved => 3,
        ManifestChangeType::BinEntryRemoved => 3,
        _ => 3,
    }
}

// ── Label helpers ───────────────────────────────────────────────────────

fn api_change_type_label(change: &ApiChangeType) -> &'static str {
    match change {
        ApiChangeType::Removed => "removed",
        ApiChangeType::SignatureChanged => "signature-changed",
        ApiChangeType::TypeChanged => "type-changed",
        ApiChangeType::VisibilityChanged => "visibility-changed",
        ApiChangeType::Renamed => "renamed",
    }
}

fn api_kind_label(kind: &ApiChangeKind) -> &'static str {
    match kind {
        ApiChangeKind::Function => "function",
        ApiChangeKind::Method => "method",
        ApiChangeKind::Class => "class",
        ApiChangeKind::Struct => "struct",
        ApiChangeKind::Interface => "interface",
        ApiChangeKind::Trait => "trait",
        ApiChangeKind::TypeAlias => "type-alias",
        ApiChangeKind::Constant => "constant",
        ApiChangeKind::Field => "field",
        ApiChangeKind::Property => "property",
        ApiChangeKind::ModuleExport => "module-export",
    }
}

fn behavioral_category_label(cat: &semver_analyzer_core::BehavioralCategory) -> &'static str {
    use semver_analyzer_core::BehavioralCategory;
    match cat {
        BehavioralCategory::DomStructure => "dom-structure",
        BehavioralCategory::CssClass => "css-class",
        BehavioralCategory::CssVariable => "css-variable",
        BehavioralCategory::Accessibility => "accessibility",
        BehavioralCategory::DefaultValue => "default-value",
        BehavioralCategory::LogicChange => "logic-change",
        BehavioralCategory::DataAttribute => "data-attribute",
        BehavioralCategory::RenderOutput => "render-output",
    }
}

fn manifest_change_type_label(change_type: &ManifestChangeType) -> &'static str {
    match change_type {
        ManifestChangeType::EntryPointChanged => "entry-point-changed",
        ManifestChangeType::ExportsEntryRemoved => "exports-entry-removed",
        ManifestChangeType::ExportsEntryAdded => "exports-entry-added",
        ManifestChangeType::ExportsConditionRemoved => "exports-condition-removed",
        ManifestChangeType::ModuleSystemChanged => "module-system-changed",
        ManifestChangeType::PeerDependencyAdded => "peer-dependency-added",
        ManifestChangeType::PeerDependencyRemoved => "peer-dependency-removed",
        ManifestChangeType::PeerDependencyRangeChanged => "peer-dependency-range-changed",
        ManifestChangeType::EngineConstraintChanged => "engine-constraint-changed",
        ManifestChangeType::BinEntryRemoved => "bin-entry-removed",
    }
}

// ── Utility helpers ─────────────────────────────────────────────────────

/// Extract the leaf symbol name from a potentially dotted path.
/// e.g. "Card.isFlat" → "isFlat", "createUser" → "createUser"
fn extract_leaf_symbol(symbol: &str) -> &str {
    symbol.rsplit('.').next().unwrap_or(symbol)
}

/// Sanitize a string for use in a Konveyor rule ID.
/// Replaces non-alphanumeric characters with hyphens, lowercases, and deduplicates.
fn sanitize_id(s: &str) -> String {
    let sanitized: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();

    // Collapse consecutive hyphens and trim
    let mut result = String::with_capacity(sanitized.len());
    let mut prev_hyphen = false;
    for ch in sanitized.chars() {
        if ch == '-' {
            if !prev_hyphen && !result.is_empty() {
                result.push('-');
            }
            prev_hyphen = true;
        } else {
            result.push(ch);
            prev_hyphen = false;
        }
    }
    // Trim trailing hyphen
    if result.ends_with('-') {
        result.pop();
    }

    result
}

/// Generate a unique rule ID by appending a counter for duplicates.
fn unique_id(base: String, counts: &mut HashMap<String, usize>) -> String {
    let count = counts.entry(base.clone()).or_insert(0);
    *count += 1;
    if *count == 1 {
        base
    } else {
        format!("{}-{}", base, count)
    }
}

/// Escape special regex characters in a symbol name.
fn regex_escape(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    escaped
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => {
            let upper: String = first.to_uppercase().collect();
            upper + chars.as_str()
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use semver_analyzer_core::*;
    use std::path::PathBuf;

    fn make_report(
        changes: Vec<FileChanges>,
        manifest_changes: Vec<ManifestChange>,
    ) -> AnalysisReport {
        AnalysisReport {
            repository: PathBuf::from("/tmp/test-repo"),
            comparison: Comparison {
                from_ref: "v1.0.0".to_string(),
                to_ref: "v2.0.0".to_string(),
                from_sha: "abc123".to_string(),
                to_sha: "def456".to_string(),
                commit_count: 10,
                analysis_timestamp: "2026-03-16T00:00:00Z".to_string(),
            },
            summary: Summary {
                total_breaking_changes: 0,
                breaking_api_changes: 0,
                breaking_behavioral_changes: 0,
                files_with_breaking_changes: 0,
            },
            changes,
            manifest_changes,
            metadata: AnalysisMetadata {
                call_graph_analysis: "none".to_string(),
                tool_version: "0.1.0".to_string(),
                llm_usage: None,
            },
        }
    }

    #[test]
    fn test_extract_leaf_symbol() {
        assert_eq!(extract_leaf_symbol("Card.isFlat"), "isFlat");
        assert_eq!(extract_leaf_symbol("createUser"), "createUser");
        assert_eq!(extract_leaf_symbol("a.b.c"), "c");
    }

    #[test]
    fn test_sanitize_id() {
        assert_eq!(sanitize_id("src/api/users.d.ts"), "src-api-users-d-ts");
        assert_eq!(sanitize_id("Card.isFlat"), "card-isflat");
        assert_eq!(sanitize_id("foo///bar"), "foo-bar");
    }

    #[test]
    fn test_unique_id() {
        let mut counts = HashMap::new();
        assert_eq!(unique_id("foo".to_string(), &mut counts), "foo");
        assert_eq!(unique_id("foo".to_string(), &mut counts), "foo-2");
        assert_eq!(unique_id("foo".to_string(), &mut counts), "foo-3");
        assert_eq!(unique_id("bar".to_string(), &mut counts), "bar");
    }

    #[test]
    fn test_regex_escape() {
        assert_eq!(regex_escape("foo"), "foo");
        assert_eq!(regex_escape("foo.bar"), "foo\\.bar");
        assert_eq!(regex_escape("a*b+c?"), "a\\*b\\+c\\?");
    }

    #[test]
    fn test_build_pattern_function_removed() {
        let pattern = build_pattern(
            &ApiChangeKind::Function,
            &ApiChangeType::Removed,
            "createUser",
            &None,
        );
        assert_eq!(pattern, r"\bcreateUser\s*\(");
    }

    #[test]
    fn test_build_pattern_property_removed() {
        let pattern = build_pattern(
            &ApiChangeKind::Property,
            &ApiChangeType::Removed,
            "isFlat",
            &None,
        );
        assert_eq!(pattern, r"\.isFlat\b");
    }

    #[test]
    fn test_build_pattern_class_removed() {
        let pattern = build_pattern(
            &ApiChangeKind::Class,
            &ApiChangeType::Removed,
            "Card",
            &None,
        );
        assert_eq!(pattern, r"\bCard\b");
    }

    #[test]
    fn test_build_pattern_renamed_uses_before() {
        let pattern = build_pattern(
            &ApiChangeKind::Function,
            &ApiChangeType::Renamed,
            "newName",
            &Some("oldName".to_string()),
        );
        // Should match the OLD name, not the new one
        assert_eq!(pattern, r"\boldName\s*\(");
    }

    #[test]
    fn test_generate_rules_api_change() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/api/users.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "createUser".to_string(),
                kind: ApiChangeKind::Function,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: "Exported function 'createUser' was removed".to_string(),
            }],
            breaking_behavioral_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.{ts,tsx,js,jsx}", RuleProvider::Builtin);

        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].rule_id,
            "semver-src-api-users-d-ts-createuser-removed"
        );
        assert_eq!(rules[0].category, "mandatory");
        assert_eq!(rules[0].effort, 5);
        assert!(rules[0]
            .labels
            .contains(&"source=semver-analyzer".to_string()));
        assert!(rules[0].labels.contains(&"change-type=removed".to_string()));
        assert!(rules[0].labels.contains(&"kind=function".to_string()));
    }

    #[test]
    fn test_generate_rules_behavioral_change() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/api/users.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![],
            breaking_behavioral_changes: vec![BehavioralChange {
                symbol: "validateEmail".to_string(),
                kind: BehavioralChangeKind::Function,
                category: None,
                description: "Now rejects emails with '+' aliases".to_string(),
                source_file: Some("src/api/users.ts".to_string()),
            }],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.{ts,tsx}", RuleProvider::Builtin);

        assert_eq!(rules.len(), 1);
        assert!(rules[0].rule_id.contains("behavioral"));
        assert_eq!(rules[0].category, "mandatory");
        assert!(rules[0].labels.contains(&"ai-generated".to_string()));
        assert!(rules[0]
            .labels
            .contains(&"change-type=behavioral".to_string()));
    }

    #[test]
    fn test_generate_rules_manifest_module_system() {
        let manifest = vec![ManifestChange {
            field: "type".to_string(),
            change_type: ManifestChangeType::ModuleSystemChanged,
            before: Some("commonjs".to_string()),
            after: Some("module".to_string()),
            description: "CJS to ESM".to_string(),
            is_breaking: true,
        }];

        let report = make_report(vec![], manifest);
        let rules = generate_rules(&report, "*.{ts,tsx,js,jsx}", RuleProvider::Builtin);

        assert_eq!(rules.len(), 1);
        assert!(rules[0].rule_id.contains("manifest"));
        assert!(rules[0].rule_id.contains("module-system-changed"));
        assert_eq!(rules[0].category, "mandatory");
        assert_eq!(rules[0].effort, 7);

        // Should use filecontent to match require() calls
        match &rules[0].when {
            KonveyorCondition::FileContent { filecontent } => {
                assert!(filecontent.pattern.contains("require"));
            }
            _ => panic!("Expected FileContent condition for module system change"),
        }
    }

    #[test]
    fn test_generate_rules_manifest_peer_dep() {
        let manifest = vec![ManifestChange {
            field: "react".to_string(),
            change_type: ManifestChangeType::PeerDependencyRemoved,
            before: Some("^17.0.0".to_string()),
            after: None,
            description: "Peer dependency 'react' was removed".to_string(),
            is_breaking: true,
        }];

        let report = make_report(vec![], manifest);
        let rules = generate_rules(&report, "*.{ts,tsx,js,jsx}", RuleProvider::Builtin);

        assert_eq!(rules.len(), 1);
        // Should use builtin.json condition
        match &rules[0].when {
            KonveyorCondition::Json { json } => {
                assert!(json.xpath.contains("peerDependencies"));
            }
            _ => panic!("Expected Json condition for peer dependency change"),
        }
    }

    #[test]
    fn test_duplicate_rule_ids_get_suffix() {
        let changes = vec![FileChanges {
            file: PathBuf::from("test.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![
                ApiChange {
                    symbol: "foo".to_string(),
                    kind: ApiChangeKind::Function,
                    change: ApiChangeType::Removed,
                    before: None,
                    after: None,
                    description: "Removed foo".to_string(),
                },
                ApiChange {
                    symbol: "foo".to_string(),
                    kind: ApiChangeKind::Function,
                    change: ApiChangeType::Removed,
                    before: None,
                    after: None,
                    description: "Removed foo overload".to_string(),
                },
            ],
            breaking_behavioral_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.ts", RuleProvider::Builtin);

        assert_eq!(rules.len(), 2);
        assert_ne!(rules[0].rule_id, rules[1].rule_id);
        assert!(rules[1].rule_id.ends_with("-2"));
    }

    #[test]
    fn test_write_ruleset_dir() {
        let base = std::env::temp_dir().join("semver-konveyor-test-out");
        let dir = base.join("rules");
        let _ = std::fs::remove_dir_all(&base);

        let report = make_report(vec![], vec![]);
        let rules = generate_rules(&report, "*.ts", RuleProvider::Builtin);
        let fix_guidance = generate_fix_guidance(&report, &rules, "*.ts");

        write_ruleset_dir(&dir, "test-ruleset", &report, &rules).unwrap();
        let fix_dir = write_fix_guidance_dir(&dir, &fix_guidance).unwrap();

        // Ruleset dir contains rules only
        assert!(dir.join("ruleset.yaml").exists());
        assert!(dir.join("breaking-changes.yaml").exists());
        assert!(!dir.join("fix-guidance.yaml").exists()); // NOT in rules dir

        // Fix guidance is in sibling directory
        assert_eq!(fix_dir, base.join("fix-guidance"));
        assert!(fix_dir.join("fix-guidance.yaml").exists());

        let ruleset_content = std::fs::read_to_string(dir.join("ruleset.yaml")).unwrap();
        assert!(ruleset_content.contains("test-ruleset"));
        assert!(ruleset_content.contains("source=semver-analyzer"));

        let fix_content = std::fs::read_to_string(fix_dir.join("fix-guidance.yaml")).unwrap();
        assert!(fix_content.contains("migration"));
        assert!(fix_content.contains("total_fixes"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_full_roundtrip_yaml_output() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/components/Button.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Button.variant".to_string(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::TypeChanged,
                before: Some("'primary' | 'secondary'".to_string()),
                after: Some("'primary' | 'danger'".to_string()),
                description: "Removed 'secondary' variant, added 'danger'".to_string(),
            }],
            breaking_behavioral_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.{ts,tsx}", RuleProvider::Builtin);

        // Verify YAML serialization succeeds
        let yaml = serde_yaml::to_string(&rules).unwrap();
        assert!(yaml.contains("ruleID"));
        assert!(yaml.contains("builtin.filecontent"));
        assert!(yaml.contains("variant"));
    }

    // ── Fix guidance tests ──────────────────────────────────────────────

    #[test]
    fn test_fix_guidance_renamed_is_exact() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/lib.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Chip".to_string(),
                kind: ApiChangeKind::Class,
                change: ApiChangeType::Renamed,
                before: Some("Chip".to_string()),
                after: Some("Label".to_string()),
                description: "Chip renamed to Label".to_string(),
            }],
            breaking_behavioral_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.{ts,tsx}", RuleProvider::Builtin);
        let guidance = generate_fix_guidance(&report, &rules, "*.{ts,tsx}");

        assert_eq!(guidance.fixes.len(), 1);
        let fix = &guidance.fixes[0];
        assert!(matches!(fix.strategy, FixStrategy::Rename));
        assert!(matches!(fix.confidence, FixConfidence::Exact));
        assert!(matches!(fix.source, FixSource::Pattern));
        assert_eq!(fix.replacement.as_deref(), Some("Label"));
        assert!(fix.fix_description.contains("Rename all occurrences"));
        assert!(fix.fix_description.contains("'Chip'"));
        assert!(fix.fix_description.contains("'Label'"));
    }

    #[test]
    fn test_fix_guidance_removed_is_manual() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/api.d.ts"),
            status: FileStatus::Deleted,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "createUser".to_string(),
                kind: ApiChangeKind::Function,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: "Function createUser was removed".to_string(),
            }],
            breaking_behavioral_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.ts", RuleProvider::Builtin);
        let guidance = generate_fix_guidance(&report, &rules, "*.ts");

        assert_eq!(guidance.fixes.len(), 1);
        let fix = &guidance.fixes[0];
        assert!(matches!(fix.strategy, FixStrategy::FindAlternative));
        assert!(matches!(fix.confidence, FixConfidence::Low));
        assert!(matches!(fix.source, FixSource::Manual));
        assert!(fix.replacement.is_none());
        assert!(fix.fix_description.contains("has been removed"));
    }

    #[test]
    fn test_fix_guidance_signature_changed() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/utils.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "formatDate".to_string(),
                kind: ApiChangeKind::Function,
                change: ApiChangeType::SignatureChanged,
                before: Some("formatDate(d: Date): string".to_string()),
                after: Some("formatDate(d: Date, locale: string): string".to_string()),
                description: "Added required 'locale' parameter".to_string(),
            }],
            breaking_behavioral_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.ts", RuleProvider::Builtin);
        let guidance = generate_fix_guidance(&report, &rules, "*.ts");

        assert_eq!(guidance.fixes.len(), 1);
        let fix = &guidance.fixes[0];
        assert!(matches!(fix.strategy, FixStrategy::UpdateSignature));
        assert!(matches!(fix.confidence, FixConfidence::High));
        assert!(fix.fix_description.contains("Old signature:"));
        assert!(fix.fix_description.contains("New signature:"));
        assert_eq!(fix.before.as_deref(), Some("formatDate(d: Date): string"));
        assert_eq!(
            fix.after.as_deref(),
            Some("formatDate(d: Date, locale: string): string")
        );
    }

    #[test]
    fn test_fix_guidance_behavioral_is_llm_source() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/auth.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![],
            breaking_behavioral_changes: vec![BehavioralChange {
                symbol: "validateToken".to_string(),
                kind: BehavioralChangeKind::Function,
                category: None,
                description: "Now throws on expired tokens instead of returning null".to_string(),
                source_file: Some("src/auth.ts".to_string()),
            }],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.ts", RuleProvider::Builtin);
        let guidance = generate_fix_guidance(&report, &rules, "*.ts");

        assert_eq!(guidance.fixes.len(), 1);
        let fix = &guidance.fixes[0];
        assert!(matches!(fix.strategy, FixStrategy::ManualReview));
        assert!(matches!(fix.confidence, FixConfidence::Medium));
        assert!(matches!(fix.source, FixSource::Llm));
        assert!(fix.fix_description.contains("AI-generated"));
        assert!(fix.fix_description.contains("throws on expired tokens"));
    }

    #[test]
    fn test_fix_guidance_manifest_cjs_to_esm() {
        let manifest = vec![ManifestChange {
            field: "type".to_string(),
            change_type: ManifestChangeType::ModuleSystemChanged,
            before: Some("commonjs".to_string()),
            after: Some("module".to_string()),
            description: "CJS to ESM migration".to_string(),
            is_breaking: true,
        }];

        let report = make_report(vec![], manifest);
        let rules = generate_rules(&report, "*.ts", RuleProvider::Builtin);
        let guidance = generate_fix_guidance(&report, &rules, "*.ts");

        assert_eq!(guidance.fixes.len(), 1);
        let fix = &guidance.fixes[0];
        assert!(matches!(fix.strategy, FixStrategy::UpdateImport));
        assert!(matches!(fix.confidence, FixConfidence::High));
        assert!(fix.fix_description.contains("require()"));
        assert!(fix.fix_description.contains("import"));
        assert_eq!(fix.replacement.as_deref(), Some("import"));
    }

    #[test]
    fn test_fix_guidance_summary_counts() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/lib.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![
                ApiChange {
                    symbol: "Chip".to_string(),
                    kind: ApiChangeKind::Class,
                    change: ApiChangeType::Renamed,
                    before: Some("Chip".to_string()),
                    after: Some("Label".to_string()),
                    description: "Renamed".to_string(),
                },
                ApiChange {
                    symbol: "oldFn".to_string(),
                    kind: ApiChangeKind::Function,
                    change: ApiChangeType::Removed,
                    before: None,
                    after: None,
                    description: "Removed".to_string(),
                },
            ],
            breaking_behavioral_changes: vec![BehavioralChange {
                symbol: "process".to_string(),
                kind: BehavioralChangeKind::Function,
                category: None,
                description: "Changed behavior".to_string(),
                source_file: Some("src/lib.ts".to_string()),
            }],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.ts", RuleProvider::Builtin);
        let guidance = generate_fix_guidance(&report, &rules, "*.ts");

        assert_eq!(guidance.summary.total_fixes, 3);
        // Rename=Exact (auto), Removed=Low/Manual, Behavioral=Medium/LLM
        assert_eq!(guidance.summary.auto_fixable, 1); // only Rename
        assert_eq!(guidance.summary.manual_only, 1); // Removed
        assert_eq!(guidance.summary.needs_review, 1); // Behavioral
    }

    #[test]
    fn test_fix_guidance_yaml_roundtrip() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/index.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![
                ApiChange {
                    symbol: "Foo".to_string(),
                    kind: ApiChangeKind::Class,
                    change: ApiChangeType::Renamed,
                    before: Some("Foo".to_string()),
                    after: Some("Bar".to_string()),
                    description: "Renamed Foo to Bar".to_string(),
                },
                ApiChange {
                    symbol: "baz".to_string(),
                    kind: ApiChangeKind::Function,
                    change: ApiChangeType::SignatureChanged,
                    before: Some("baz(): void".to_string()),
                    after: Some("baz(x: number): void".to_string()),
                    description: "Added required param".to_string(),
                },
            ],
            breaking_behavioral_changes: vec![],
        }];

        let manifest = vec![ManifestChange {
            field: "type".to_string(),
            change_type: ManifestChangeType::ModuleSystemChanged,
            before: Some("commonjs".to_string()),
            after: Some("module".to_string()),
            description: "CJS to ESM".to_string(),
            is_breaking: true,
        }];

        let report = make_report(changes, manifest);
        let rules = generate_rules(&report, "*.{ts,tsx}", RuleProvider::Builtin);
        let guidance = generate_fix_guidance(&report, &rules, "*.{ts,tsx}");

        let yaml = serde_yaml::to_string(&guidance).unwrap();
        assert!(yaml.contains("strategy"));
        assert!(yaml.contains("confidence"));
        assert!(yaml.contains("fix_description"));
        assert!(yaml.contains("search_pattern"));
        assert!(yaml.contains("replacement"));
        assert!(yaml.contains("rename"));
        assert!(yaml.contains("update_signature"));
        assert!(yaml.contains("update_import"));
        assert!(yaml.contains("auto_fixable"));
        assert!(yaml.contains("needs_review"));
        assert!(yaml.contains("manual_only"));
    }

    // ── Frontend provider tests ─────────────────────────────────────

    #[test]
    fn test_frontend_provider_class_rename_generates_or_condition() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/components/Chip.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Chip".to_string(),
                kind: ApiChangeKind::Class,
                change: ApiChangeType::Renamed,
                before: Some("Chip".to_string()),
                after: Some("Label".to_string()),
                description: "Chip renamed to Label".to_string(),
            }],
            breaking_behavioral_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.ts", RuleProvider::Frontend);

        assert_eq!(rules.len(), 1);
        let yaml = serde_yaml::to_string(&rules[0]).unwrap();
        // Should have an or: condition with JSX_COMPONENT and IMPORT
        assert!(yaml.contains("frontend.referenced"));
        assert!(yaml.contains("JSX_COMPONENT"));
        assert!(yaml.contains("IMPORT"));
        assert!(yaml.contains("^Chip$")); // matches old name
        assert!(yaml.contains("has-codemod=true"));
    }

    #[test]
    fn test_frontend_provider_prop_removed_scoped_to_component() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/components/Card.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "Card.isFlat".to_string(),
                kind: ApiChangeKind::Property,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: "Card.isFlat prop removed".to_string(),
            }],
            breaking_behavioral_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.ts", RuleProvider::Frontend);

        assert_eq!(rules.len(), 1);
        let yaml = serde_yaml::to_string(&rules[0]).unwrap();
        // Should use JSX_PROP location with component filter
        assert!(yaml.contains("JSX_PROP"));
        assert!(yaml.contains("^isFlat$"));
        assert!(yaml.contains("^Card$")); // component filter
    }

    #[test]
    fn test_frontend_provider_function_uses_function_call() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/utils.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "createUser".to_string(),
                kind: ApiChangeKind::Function,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: "createUser removed".to_string(),
            }],
            breaking_behavioral_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.ts", RuleProvider::Frontend);

        assert_eq!(rules.len(), 1);
        let yaml = serde_yaml::to_string(&rules[0]).unwrap();
        assert!(yaml.contains("FUNCTION_CALL"));
        assert!(yaml.contains("^createUser$"));
    }

    #[test]
    fn test_frontend_provider_type_alias_uses_type_reference() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/types.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "UserRole".to_string(),
                kind: ApiChangeKind::TypeAlias,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: "UserRole type removed".to_string(),
            }],
            breaking_behavioral_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.ts", RuleProvider::Frontend);

        assert_eq!(rules.len(), 1);
        let yaml = serde_yaml::to_string(&rules[0]).unwrap();
        assert!(yaml.contains("TYPE_REFERENCE"));
        assert!(yaml.contains("^UserRole$"));
    }

    #[test]
    fn test_frontend_provider_constant_uses_import() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/config.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![ApiChange {
                symbol: "DEFAULT_TIMEOUT".to_string(),
                kind: ApiChangeKind::Constant,
                change: ApiChangeType::Removed,
                before: None,
                after: None,
                description: "DEFAULT_TIMEOUT removed".to_string(),
            }],
            breaking_behavioral_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let rules = generate_rules(&report, "*.ts", RuleProvider::Frontend);

        assert_eq!(rules.len(), 1);
        let yaml = serde_yaml::to_string(&rules[0]).unwrap();
        assert!(yaml.contains("IMPORT"));
        assert!(yaml.contains("^DEFAULT_TIMEOUT$"));
    }

    #[test]
    fn test_builtin_vs_frontend_same_rule_count() {
        let changes = vec![FileChanges {
            file: PathBuf::from("src/lib.d.ts"),
            status: FileStatus::Modified,
            renamed_from: None,
            breaking_api_changes: vec![
                ApiChange {
                    symbol: "Foo".to_string(),
                    kind: ApiChangeKind::Class,
                    change: ApiChangeType::Renamed,
                    before: Some("Foo".to_string()),
                    after: Some("Bar".to_string()),
                    description: "Renamed".to_string(),
                },
                ApiChange {
                    symbol: "Foo.baz".to_string(),
                    kind: ApiChangeKind::Property,
                    change: ApiChangeType::Removed,
                    before: None,
                    after: None,
                    description: "Removed prop".to_string(),
                },
            ],
            breaking_behavioral_changes: vec![],
        }];

        let report = make_report(changes, vec![]);
        let builtin_rules = generate_rules(&report, "*.ts", RuleProvider::Builtin);
        let frontend_rules = generate_rules(&report, "*.ts", RuleProvider::Frontend);

        // Same number of rules regardless of provider
        assert_eq!(builtin_rules.len(), frontend_rules.len());
        // Same rule IDs
        assert_eq!(builtin_rules[0].rule_id, frontend_rules[0].rule_id);
        assert_eq!(builtin_rules[1].rule_id, frontend_rules[1].rule_id);
    }
}
