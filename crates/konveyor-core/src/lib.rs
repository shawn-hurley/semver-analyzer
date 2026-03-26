//! Shared types and utilities for Konveyor rule generation.
//!
//! This crate contains the language-independent types, structs, enums, and
//! helper functions used by the Konveyor rule generation pipeline.  All items
//! here are agnostic of any specific `Language` implementation (e.g., TypeScript)
//! and can be consumed by both the TS-specific crate and the binary crate.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use semver_analyzer_core::{ApiChange, ApiChangeKind, ApiChangeType, RemovalDisposition};

// ── User-supplied rename patterns ───────────────────────────────────────

/// A single regex-based rename pattern.
///
/// When a symbol is removed and its name matches `match_regex`, the
/// replacement is computed by applying the regex substitution `replace`.
/// Standard regex capture groups (`$1`, `${1}`) are supported.
#[derive(Debug, Clone, Deserialize)]
pub struct RenamePatternEntry {
    /// Regex to match against the removed symbol name.
    #[serde(rename = "match")]
    pub match_pattern: String,
    /// Replacement string (supports `$1`, `${1}` capture group references).
    pub replace: String,
}

/// A composition rule: detect a child component inside a parent component.
///
/// Generates rules with the `parent` constraint on `frontend.referenced`.
#[derive(Debug, Clone, Deserialize)]
pub struct CompositionRuleEntry {
    /// Regex pattern for the child component (e.g., `"Icon$"`).
    pub child_pattern: String,
    /// Regex for the required parent component (e.g., `"^Button$"`).
    pub parent: String,
    /// Rule category: `mandatory` or `potential`.
    #[serde(default = "default_mandatory")]
    pub category: String,
    /// Human-readable description.
    pub description: String,
    /// Effort estimate.
    #[serde(default = "default_effort_2")]
    pub effort: u32,
    /// Optional package scope (e.g., `@patternfly/react-core`).
    #[serde(default)]
    pub package: Option<String>,
}

/// A prop rename rule: detect usage of an old prop name on specific components.
#[derive(Debug, Clone, Deserialize)]
pub struct PropRenameEntry {
    /// Old prop name.
    pub old_prop: String,
    /// New prop name (for message/fix guidance).
    pub new_prop: String,
    /// Regex matching the components this rename applies to.
    pub components: String,
    /// Package scope.
    #[serde(default)]
    pub package: Option<String>,
    /// Human-readable description.
    #[serde(default)]
    pub description: Option<String>,
}

/// A component warning: emit a JSX_COMPONENT rule for a component whose internal
/// DOM/CSS rendering changed without an API surface change.
///
/// These are informational rules that alert consumers to review usages of a
/// component whose behavior changed internally.
#[derive(Debug, Clone, Deserialize)]
pub struct ComponentWarningEntry {
    /// Regex pattern matching the component name (e.g., `"^TextArea$"`).
    pub pattern: String,
    /// Package scope.
    #[serde(default)]
    pub package: Option<String>,
    /// Rule category: `mandatory` or `potential`.
    #[serde(default = "default_potential")]
    pub category: String,
    /// Human-readable description.
    pub description: String,
    /// Effort estimate.
    #[serde(default = "default_effort_1")]
    pub effort: u32,
}

/// A missing co-requisite import rule: flag when pattern A is present but pattern B is absent.
///
/// Uses `and` + `not` combinators with `builtin.filecontent` to detect cases
/// where a file has one import but is missing a newly required companion import.
#[derive(Debug, Clone, Deserialize)]
pub struct MissingImportEntry {
    /// Regex that must be present in the file (the existing import).
    pub has_pattern: String,
    /// Regex that must be absent from the file (the missing import).
    pub missing_pattern: String,
    /// File glob pattern (e.g., `"\\.(ts|tsx|js|jsx)$"`).
    #[serde(default = "default_ts_file_pattern")]
    pub file_pattern: String,
    /// Rule category: `mandatory` or `potential`.
    #[serde(default = "default_mandatory")]
    pub category: String,
    /// Human-readable description.
    pub description: String,
    /// Effort estimate.
    #[serde(default = "default_effort_1")]
    pub effort: u32,
}

pub fn default_ts_file_pattern() -> String {
    r"\.(ts|tsx|js|jsx)$".to_string()
}

/// A value review rule: detect a specific prop value that may need updating.
///
/// Used for cases where a prop value is technically still valid but may need
/// review (e.g., `variant="plain"` on MenuToggle).
#[derive(Debug, Clone, Deserialize)]
pub struct ValueReviewEntry {
    /// Prop name.
    pub prop: String,
    /// Regex matching the component.
    pub component: String,
    /// Regex matching the value to flag.
    pub value: String,
    /// Package scope.
    #[serde(default)]
    pub package: Option<String>,
    /// Rule category: `mandatory` or `potential`.
    #[serde(default = "default_potential")]
    pub category: String,
    /// Human-readable description.
    pub description: String,
    /// Effort estimate.
    #[serde(default = "default_effort_1")]
    pub effort: u32,
}

pub fn default_mandatory() -> String {
    "mandatory".to_string()
}
pub fn default_potential() -> String {
    "potential".to_string()
}
pub fn default_effort_1() -> u32 {
    1
}
pub fn default_effort_2() -> u32 {
    2
}

/// Parsed rename patterns file (extended with composition rules, prop renames,
/// value review rules, missing import rules, and component warnings).
#[derive(Debug, Clone, Deserialize)]
pub struct RenamePatternsFile {
    #[serde(default)]
    pub rename_patterns: Vec<RenamePatternEntry>,
    #[serde(default)]
    pub composition_rules: Vec<CompositionRuleEntry>,
    #[serde(default)]
    pub prop_renames: Vec<PropRenameEntry>,
    #[serde(default)]
    pub value_reviews: Vec<ValueReviewEntry>,
    #[serde(default)]
    pub missing_imports: Vec<MissingImportEntry>,
    #[serde(default)]
    pub component_warnings: Vec<ComponentWarningEntry>,
}

/// Compiled rename patterns ready for matching.
#[derive(Debug, Clone)]
pub struct RenamePatterns {
    patterns: Vec<(regex::Regex, String)>,
    pub composition_rules: Vec<CompositionRuleEntry>,
    pub prop_renames: Vec<PropRenameEntry>,
    pub value_reviews: Vec<ValueReviewEntry>,
    pub missing_imports: Vec<MissingImportEntry>,
    pub component_warnings: Vec<ComponentWarningEntry>,
}

impl RenamePatterns {
    /// Load and compile rename patterns from a YAML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read rename patterns from {}", path.display()))?;
        let file: RenamePatternsFile = serde_yaml::from_str(&content).with_context(|| {
            format!("Failed to parse {} as rename patterns YAML", path.display())
        })?;

        let mut patterns = Vec::new();
        for entry in &file.rename_patterns {
            let re = regex::Regex::new(&entry.match_pattern).with_context(|| {
                format!("Invalid regex in rename pattern: {}", entry.match_pattern)
            })?;
            patterns.push((re, entry.replace.clone()));
        }

        eprintln!(
            "Loaded {} rename patterns from {}",
            patterns.len(),
            path.display()
        );
        if !file.composition_rules.is_empty() {
            eprintln!("Loaded {} composition rules", file.composition_rules.len());
        }
        if !file.prop_renames.is_empty() {
            eprintln!("Loaded {} prop renames", file.prop_renames.len());
        }
        if !file.value_reviews.is_empty() {
            eprintln!("Loaded {} value reviews", file.value_reviews.len());
        }
        if !file.missing_imports.is_empty() {
            eprintln!("Loaded {} missing import rules", file.missing_imports.len());
        }
        if !file.component_warnings.is_empty() {
            eprintln!(
                "Loaded {} component warnings",
                file.component_warnings.len()
            );
        }
        Ok(Self {
            patterns,
            composition_rules: file.composition_rules,
            prop_renames: file.prop_renames,
            value_reviews: file.value_reviews,
            missing_imports: file.missing_imports,
            component_warnings: file.component_warnings,
        })
    }

    /// Try to find a replacement for a removed symbol name.
    ///
    /// Returns `Some(new_name)` if any pattern matches, `None` otherwise.
    pub fn find_replacement(&self, symbol_name: &str) -> Option<String> {
        for (re, replace) in &self.patterns {
            if re.is_match(symbol_name) {
                let result = re.replace(symbol_name, replace.as_str()).to_string();
                if result != symbol_name {
                    return Some(result);
                }
            }
        }
        None
    }

    /// Add a single rename pattern (compiled regex + replacement string).
    /// Used to merge LLM-inferred patterns at runtime.
    pub fn add_pattern(&mut self, match_regex: &str, replace: &str) {
        match regex::Regex::new(match_regex) {
            Ok(re) => self.patterns.push((re, replace.to_string())),
            Err(e) => eprintln!(
                "[warn] Skipping invalid inferred pattern '{}': {}",
                match_regex, e
            ),
        }
    }

    /// Empty patterns (no-op).
    pub fn empty() -> Self {
        Self {
            patterns: Vec::new(),
            composition_rules: Vec::new(),
            prop_renames: Vec::new(),
            value_reviews: Vec::new(),
            missing_imports: Vec::new(),
            component_warnings: Vec::new(),
        }
    }
}

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
    /// Fix strategy for this rule. Not serialized to kantra YAML — written
    /// separately to fix-strategies.json after consolidation.
    #[serde(skip)]
    pub fix_strategy: Option<FixStrategyEntry>,
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
    FrontendCssClass {
        #[serde(rename = "frontend.cssclass")]
        cssclass: FrontendPatternFields,
    },
    FrontendCssVar {
        #[serde(rename = "frontend.cssvar")]
        cssvar: FrontendPatternFields,
    },
    Or {
        or: Vec<KonveyorCondition>,
    },
    And {
        and: Vec<KonveyorCondition>,
    },
    /// Negated `builtin.filecontent`: matches when the pattern is NOT found.
    /// Serializes as `{ "not": true, "builtin.filecontent": { ... } }`.
    FileContentNegated {
        #[serde(rename = "not")]
        negated: bool,
        #[serde(rename = "builtin.filecontent")]
        filecontent: FileContentFields,
    },
}

/// Fields for `frontend.cssclass` and `frontend.cssvar` conditions.
#[derive(Debug, Serialize)]
pub struct FrontendPatternFields {
    pub pattern: String,
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
    pub filepaths: Option<Vec<String>>,
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
    /// Filter by the parent component's import source (regex).
    /// Requires `parent` to be set. Ensures the parent is from a specific
    /// package (e.g., `@patternfly/react-core`), not a custom app component.
    #[serde(rename = "parentFrom", skip_serializing_if = "Option::is_none")]
    pub parent_from: Option<String>,
    /// Filter JSX prop values to only those matching this regex.
    /// Used for prop value changes (e.g., `variant="tertiary"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Scope to imports from a specific package (e.g., `@patternfly/react-tokens`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
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

/// Minimum number of constants from the same package with the same change type
/// before they are collapsed into a single combined rule.
pub const CONSTANT_COLLAPSE_THRESHOLD: usize = 10;

/// Grouping key for collapsible constant changes: package + change type + strategy.
/// This ensures constants with different fix strategies end up in separate rules.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConstantGroupKey {
    pub package: String,
    pub change_type: ApiChangeType,
    pub strategy: String,
}

/// A compound token with its removed and added member key suffixes.
///
/// Used to cache compound token data between suffix inventory extraction
/// and suffix mapping application.
pub struct CompoundToken {
    pub removed: BTreeSet<String>,
    pub added: BTreeSet<String>,
}

/// Information about a package discovered in the monorepo.
#[derive(Debug, Clone)]
pub struct PackageInfo {
    /// npm package name (e.g., "@patternfly/react-core").
    pub name: String,
    /// Package version at the new ref (read from disk).
    pub version: Option<String>,
}

/// A single from/to mapping within a consolidated fix strategy.
#[derive(Debug, Clone, Serialize)]
pub struct MappingEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prop: Option<String>,
}

/// A member-level mapping entry for structural migration strategies.
#[derive(Debug, Clone, Serialize)]
pub struct MemberMappingEntry {
    pub old_name: String,
    pub new_name: String,
}

/// A machine-readable fix strategy entry.
///
/// For non-consolidated rules, `from`/`to` hold the single mapping.
/// For consolidated rules, `mappings` holds all individual mappings from the
/// merged rules, allowing the fix engine to apply all renames/removals.
/// For structural migration rules, `member_mappings` and `removed_members`
/// describe the member-level overlap between removed and replacement interfaces.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FixStrategyEntry {
    pub strategy: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prop: Option<String>,
    /// All individual mappings when this strategy was merged from multiple rules.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub mappings: Vec<MappingEntry>,
    /// Structural migration: matching member mappings between removed and replacement.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub member_mappings: Vec<MemberMappingEntry>,
    /// Structural migration: member names only in the removed interface (no match).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub removed_members: Vec<String>,
    /// Structural migration: the replacement symbol name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
    /// Structural migration: overlap ratio between removed and replacement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlap_ratio: Option<f64>,
    /// Dependency update: npm package name (e.g., "@patternfly/react-core").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// Dependency update: new version range (e.g., "^6.1.0").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_version: Option<String>,
}

impl FixStrategyEntry {
    /// Create a new strategy entry with only the strategy type set.
    pub fn new(strategy: &str) -> Self {
        Self {
            strategy: strategy.into(),
            ..Default::default()
        }
    }

    /// Create a Rename strategy with a single from/to pair.
    pub fn rename(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            strategy: "Rename".into(),
            from: Some(from.into()),
            to: Some(to.into()),
            ..Default::default()
        }
    }

    /// Create a strategy with from/to and a named strategy type.
    pub fn with_from_to(strategy: &str, from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            strategy: strategy.into(),
            from: Some(from.into()),
            to: Some(to.into()),
            ..Default::default()
        }
    }

    /// Create a RemoveProp strategy.
    pub fn remove_prop(component: impl Into<String>, prop: impl Into<String>) -> Self {
        Self {
            strategy: "RemoveProp".into(),
            component: Some(component.into()),
            prop: Some(prop.into()),
            ..Default::default()
        }
    }

    /// Create an LlmAssisted strategy enriched with structural migration data.
    pub fn structural_migration(
        removed_symbol: &str,
        replacement_symbol: &str,
        member_mappings: Vec<MemberMappingEntry>,
        removed_members: Vec<String>,
        overlap_ratio: f64,
    ) -> Self {
        Self {
            strategy: "LlmAssisted".into(),
            from: Some(removed_symbol.into()),
            to: Some(replacement_symbol.into()),
            member_mappings,
            removed_members,
            replacement: Some(replacement_symbol.into()),
            overlap_ratio: Some(overlap_ratio),
            ..Default::default()
        }
    }

    /// Create an UpdateDependency strategy for a package version bump.
    pub fn update_dependency(package: impl Into<String>, new_version: impl Into<String>) -> Self {
        Self {
            strategy: "UpdateDependency".into(),
            package: Some(package.into()),
            new_version: Some(new_version.into()),
            ..Default::default()
        }
    }

    /// Convert to a MappingEntry (extracting the single mapping).
    pub fn to_mapping(&self) -> MappingEntry {
        MappingEntry {
            from: self.from.clone(),
            to: self.to.clone(),
            component: self.component.clone(),
            prop: self.prop.clone(),
        }
    }
}

// ── Shared functions ────────────────────────────────────────────────────

/// Build a regex pattern that matches all symbol names in a group by
/// extracting common prefixes (up to the first `_` segment).
pub fn build_token_prefix_pattern(symbols: &[&str]) -> String {
    let mut prefixes: BTreeSet<String> = BTreeSet::new();
    for sym in symbols {
        if let Some(idx) = sym.find('_') {
            prefixes.insert(format!("{}_", &sym[..idx]));
        } else {
            prefixes.insert(sym.to_string());
        }
    }
    if prefixes.len() > 20 || prefixes.is_empty() {
        return ".*".to_string();
    }
    let alts: Vec<String> = prefixes.into_iter().map(|p| regex_escape(&p)).collect();
    format!("^({})", alts.join("|"))
}

/// Build a single combined rule for a group of collapsible constant changes.
pub fn build_combined_constant_rule(
    key: &ConstantGroupKey,
    changes: &[(&ApiChange, Option<String>, FixStrategyEntry)],
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    let symbol_names: Vec<&str> = changes.iter().map(|(c, _, _)| c.symbol.as_str()).collect();
    let pattern = build_token_prefix_pattern(&symbol_names);
    let from_pkg = changes[0].1.clone();
    // Strategy is uniform within the group — take the first one
    let strategy = Some(changes[0].2.clone());

    let change_type_str = api_change_type_label(&key.change_type);
    let kind_str = api_kind_label(&ApiChangeKind::Constant);
    let slug = key
        .package
        .replace('@', "")
        .replace('/', "-")
        .replace('.', "-");
    let strategy_slug = key.strategy.to_lowercase().replace(' ', "-");
    let base_id = format!(
        "semver-{}-constant-{}-{}-combined",
        slug, change_type_str, strategy_slug
    );
    let rule_id = unique_id(base_id, id_counts);

    // Build a summary message
    let mut message = format!(
        "{} constants from `{}` had breaking changes ({}).\n",
        changes.len(),
        key.package,
        change_type_str,
    );

    // If there's a CSS prefix change, include it in the message
    if let Some(ref strat) = strategy {
        if strat.strategy == "CssVariablePrefix" {
            if let (Some(ref from), Some(ref to)) = (&strat.from, &strat.to) {
                message.push_str(&format!(
                    "CSS variable prefix changed from `{}` to `{}`.\n",
                    from, to
                ));
            }
        }
    }

    // Add a sample of the first few symbol names
    let sample_count = 5.min(symbol_names.len());
    message.push_str(&format!(
        "Affected constants include: {}",
        symbol_names[..sample_count].join(", ")
    ));
    if symbol_names.len() > sample_count {
        message.push_str(&format!(" and {} more.", symbol_names.len() - sample_count));
    }

    KonveyorRule {
        rule_id,
        labels: vec![
            "source=semver-analyzer".to_string(),
            format!("change-type={}", change_type_str),
            format!("kind={}", kind_str),
            "has-codemod=true".to_string(),
            format!("package={}", key.package),
        ],
        effort: 3,
        category: "mandatory".to_string(),
        description: format!(
            "{} constants from {} have breaking changes",
            changes.len(),
            key.package
        ),
        message,
        links: Vec::new(),
        when: KonveyorCondition::FrontendReferenced {
            referenced: FrontendReferencedFields {
                pattern,
                location: "IMPORT".to_string(),
                component: None,
                parent: None,
                value: None,
                from: from_pkg,
                parent_from: None,
            },
        },
        fix_strategy: strategy,
    }
}

/// Suppress redundant individual token removal rules.
pub fn suppress_redundant_token_rules(
    rules: Vec<KonveyorRule>,
    covered_symbols: &BTreeSet<String>,
) -> Vec<KonveyorRule> {
    if covered_symbols.is_empty() {
        return rules;
    }

    let before_count = rules.len();
    let rules: Vec<KonveyorRule> = rules
        .into_iter()
        .filter(|rule| {
            let is_removal = rule.labels.iter().any(|l| l == "change-type=removed");
            let is_constant = rule.labels.iter().any(|l| l == "kind=constant");

            if !is_removal || !is_constant {
                return true;
            }

            let is_index = rule.message.lines().any(|l| l.contains("index.d.ts"));
            if is_index {
                return true;
            }

            let symbol = rule.description.split('`').nth(1).unwrap_or("");

            !covered_symbols.contains(symbol)
        })
        .collect();

    let suppressed = before_count - rules.len();
    if suppressed > 0 {
        eprintln!(
            "Suppressed {} redundant token removal rules (covered by parent type_changed)",
            suppressed
        );
    }

    rules
}

/// Suppress redundant prop-level removal rules when a component-level
/// `component-import-deprecated` rule already covers the same component.
pub fn suppress_redundant_prop_rules(rules: Vec<KonveyorRule>) -> Vec<KonveyorRule> {
    let covered: BTreeSet<String> = rules
        .iter()
        .filter(|r| r.rule_id.contains("component-import-deprecated"))
        .filter_map(|r| match &r.when {
            KonveyorCondition::FrontendReferenced { referenced } => {
                let pat = &referenced.pattern;
                Some(
                    pat.strip_prefix('^')
                        .unwrap_or(pat)
                        .strip_suffix('$')
                        .unwrap_or(pat)
                        .to_string(),
                )
            }
            _ => None,
        })
        .collect();

    if covered.is_empty() {
        return rules;
    }

    let before_count = rules.len();
    let rules: Vec<KonveyorRule> = rules
        .into_iter()
        .filter(|rule| {
            let strategy = rule.fix_strategy.as_ref();
            let is_remove_prop = strategy
                .map(|s| s.strategy == "RemoveProp")
                .unwrap_or(false);
            if !is_remove_prop {
                return true;
            }
            let target = strategy.and_then(|s| s.component.as_deref()).unwrap_or("");
            let target_base = target.strip_suffix("Props").unwrap_or(target);
            if covered.contains(target_base) {
                return false;
            }
            true
        })
        .collect();

    let suppressed = before_count - rules.len();
    if suppressed > 0 {
        eprintln!(
            "Suppressed {} redundant prop removal rules (covered by component-import-deprecated)",
            suppressed
        );
    }

    rules
}

/// Suppress `prop-value-change` rules that overlap with `type-changed` rules.
pub fn suppress_redundant_prop_value_rules(rules: Vec<KonveyorRule>) -> Vec<KonveyorRule> {
    let mut type_changed_triggers: HashSet<(String, String, String)> = HashSet::new();

    for rule in &rules {
        let is_type_changed = rule.labels.iter().any(|l| l == "change-type=type-changed");
        if !is_type_changed {
            continue;
        }

        let refs = extract_frontend_refs(&rule.when);
        for cond in refs {
            if let (Some(component), Some(value)) = (&cond.component, &cond.value) {
                if cond.location == "JSX_PROP" {
                    type_changed_triggers.insert((
                        component.clone(),
                        cond.pattern.clone(),
                        value.clone(),
                    ));
                }
            }
        }
    }

    if type_changed_triggers.is_empty() {
        return rules;
    }

    let before_count = rules.len();
    let rules: Vec<KonveyorRule> = rules
        .into_iter()
        .filter(|rule| {
            let is_prop_value = rule
                .labels
                .iter()
                .any(|l| l == "change-type=prop-value-change");
            if !is_prop_value {
                return true;
            }

            let refs = extract_frontend_refs(&rule.when);
            if refs.is_empty() {
                return true;
            }

            let all_covered = refs.iter().all(|cond| {
                if let (Some(component), Some(value)) = (&cond.component, &cond.value) {
                    type_changed_triggers.contains(&(
                        component.clone(),
                        cond.pattern.clone(),
                        value.clone(),
                    ))
                } else {
                    false
                }
            });

            !all_covered
        })
        .collect();

    let suppressed = before_count - rules.len();
    if suppressed > 0 {
        eprintln!(
            "Suppressed {} redundant prop-value-change rules (covered by type-changed)",
            suppressed
        );
    }

    rules
}

/// Extract all `FrontendReferencedFields` from a `KonveyorCondition`,
/// recursing into `Or`/`And` combinators.
pub fn extract_frontend_refs(condition: &KonveyorCondition) -> Vec<&FrontendReferencedFields> {
    match condition {
        KonveyorCondition::FrontendReferenced { referenced } => vec![referenced],
        KonveyorCondition::Or { or } => or.iter().flat_map(extract_frontend_refs).collect(),
        KonveyorCondition::And { and } => and.iter().flat_map(extract_frontend_refs).collect(),
        _ => vec![],
    }
}

/// Consolidate rules by grouping related rules into single combined rules.
pub fn consolidate_rules(rules: Vec<KonveyorRule>) -> (Vec<KonveyorRule>, HashMap<String, String>) {
    let mut groups: BTreeMap<String, Vec<KonveyorRule>> = BTreeMap::new();
    for rule in rules {
        let key = consolidation_key(&rule);
        groups.entry(key).or_default().push(rule);
    }
    let mut consolidated = Vec::new();
    let mut id_mapping = HashMap::new();
    for (_key, group) in groups {
        if group.len() == 1 {
            let rule = group.into_iter().next().unwrap();
            id_mapping.insert(rule.rule_id.clone(), rule.rule_id.clone());
            consolidated.push(rule);
        } else {
            let old_ids: Vec<String> = group.iter().map(|r| r.rule_id.clone()).collect();
            let merged = merge_rule_group(group);
            let new_id = merged.rule_id.clone();
            for old_id in old_ids {
                id_mapping.insert(old_id, new_id.clone());
            }
            consolidated.push(merged);
        }
    }
    (consolidated, id_mapping)
}

pub fn consolidation_key(rule: &KonveyorRule) -> String {
    let change_type = rule
        .labels
        .iter()
        .find(|l| l.starts_with("change-type="))
        .map(|l| l.strip_prefix("change-type=").unwrap_or("unknown"))
        .unwrap_or("unknown");
    let kind = rule
        .labels
        .iter()
        .find(|l| l.starts_with("kind="))
        .map(|l| l.strip_prefix("kind=").unwrap_or(""))
        .unwrap_or("");
    let file_key = rule
        .message
        .lines()
        .find(|l| l.starts_with("File:"))
        .map(|l| l.trim_start_matches("File:").trim())
        .unwrap_or("unknown");

    if change_type == "manifest" {
        let field = rule
            .labels
            .iter()
            .find(|l| l.starts_with("manifest-field="))
            .map(|l| l.strip_prefix("manifest-field=").unwrap_or(""))
            .unwrap_or("");
        return format!("manifest-{}-{}", field, change_type);
    }

    if change_type == "removed" && kind == "constant" {
        let symbol = rule.description.split('`').nth(1).unwrap_or("");
        let is_component_constant = symbol
            .chars()
            .next()
            .map_or(false, |c| c.is_ascii_uppercase());
        if !is_component_constant {
            let package = extract_package_from_path(file_key);
            return format!("{}-constant-removed", package);
        }
    }

    if change_type == "type-changed" && kind == "constant" {
        let package = extract_package_from_path(file_key);
        return format!("{}-constant-type-changed", package);
    }

    match change_type {
        "css-variable"
        | "new-sibling-component"
        | "component-removal"
        | "dependency-update"
        | "composition"
        | "hierarchy-composition" => {
            return rule.rule_id.clone();
        }
        _ => {}
    }

    format!("{}-{}-{}", file_key, kind, change_type)
}

/// Read package.json at a specific git ref using `git show`.
/// Returns (name, version) tuple.
pub fn read_package_json_at_ref(
    repo_path: &std::path::Path,
    git_ref: &str,
    pkg_json_path: &str,
) -> Option<(Option<String>, Option<String>)> {
    let output = std::process::Command::new("git")
        .args(["show", &format!("{}:{}", git_ref, pkg_json_path)])
        .current_dir(repo_path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let content = String::from_utf8(output.stdout).ok()?;
    let parsed = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    let name = parsed.get("name")?.as_str().map(|s| s.to_string());
    let version = parsed.get("version")?.as_str().map(|s| s.to_string());
    Some((name, version))
}

/// Read package.json from a file path on disk.
/// Returns (name, version) tuple.
pub fn read_package_json_from_file(
    path: &std::path::Path,
) -> Option<(Option<String>, Option<String>)> {
    let content = std::fs::read_to_string(path).ok()?;
    let parsed = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    let name = parsed.get("name")?.as_str().map(|s| s.to_string());
    let version = parsed.get("version")?.as_str().map(|s| s.to_string());
    Some((name, version))
}

/// Look up the npm package name for a file path using the cache.
pub fn resolve_npm_package(file_path: &str, cache: &HashMap<String, String>) -> Option<String> {
    let parts: Vec<&str> = file_path.split('/').collect();
    let pkg_idx = parts.iter().position(|&p| p == "packages")?;
    let pkg_dir_name = parts.get(pkg_idx + 1)?;

    let base_name = cache.get(*pkg_dir_name)?;

    let has_deprecated = parts.iter().any(|&p| p == "deprecated");
    let has_next = parts.iter().any(|&p| p == "next");

    if has_deprecated {
        Some(format!("^{}/deprecated$", regex_escape(base_name)))
    } else if has_next {
        Some(format!("^{}/next$", regex_escape(base_name)))
    } else {
        Some(base_name.clone())
    }
}

/// Extract a package name from a file path for consolidation grouping.
pub fn extract_package_from_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if let Some(pkg_idx) = parts.iter().position(|&p| p == "packages") {
        if let Some(pkg_name) = parts.get(pkg_idx + 1) {
            let has_deprecated = parts.iter().any(|&p| p == "deprecated");
            if has_deprecated {
                return format!("{}-deprecated", pkg_name);
            }
            return pkg_name.to_string();
        }
    }
    path.split('/')
        .find(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

pub fn merge_rule_group(group: Vec<KonveyorRule>) -> KonveyorRule {
    let count = group.len();
    let first_rule_id = group[0].rule_id.clone();
    let first_category = group[0].category.clone();
    let effort = group.iter().map(|r| r.effort).max().unwrap_or(1);
    let mut all_labels: BTreeSet<String> = BTreeSet::new();
    for rule in &group {
        for label in &rule.labels {
            all_labels.insert(label.clone());
        }
    }
    let labels: Vec<String> = all_labels.into_iter().collect();
    let descriptions: Vec<&str> = group.iter().map(|r| r.description.as_str()).collect();

    let unique_messages: Vec<&str> = {
        let mut seen = BTreeSet::new();
        group
            .iter()
            .map(|r| r.message.as_str())
            .filter(|m| seen.insert(*m))
            .collect()
    };
    let message = if unique_messages.len() == 1 {
        unique_messages[0].to_string()
    } else {
        let total = unique_messages.len();
        let mut parts = Vec::new();
        parts.push(format!(
            "This rule contains {} migration steps. Apply each one independently:\n",
            total
        ));
        for (i, msg) in unique_messages.iter().enumerate() {
            parts.push(format!("Step {} of {}:\n{}\n", i + 1, total, msg));
        }
        parts.join("\n")
    };
    let description = format!("{} related changes", count);
    let rule_id = format!("{}-group-{}", first_rule_id, count);

    let is_large_removed_constant = count > 20
        && labels.iter().any(|l| l == "change-type=removed")
        && labels.iter().any(|l| l == "kind=constant");

    let all_strategies: Vec<Option<FixStrategyEntry>> =
        group.iter().map(|r| r.fix_strategy.clone()).collect();

    let when = if is_large_removed_constant {
        let symbols: Vec<&str> = descriptions
            .iter()
            .filter_map(|d| d.split('`').nth(1))
            .collect();
        let pattern = build_common_prefix_pattern(&symbols);

        let from_pkg: Option<String> = labels
            .iter()
            .find(|l| l.starts_with("package="))
            .map(|l| l.strip_prefix("package=").unwrap_or("").to_string());

        if from_pkg.is_some() {
            KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern,
                    location: "IMPORT".to_string(),
                    component: None,
                    parent: None,
                    value: None,
                    from: from_pkg,
                    parent_from: None,
                },
            }
        } else {
            let file_pattern = extract_file_pattern_from_condition(&group[0].when)
                .unwrap_or_else(|| "*.{ts,tsx,js,jsx,mjs,cjs}".to_string());
            KonveyorCondition::FileContent {
                filecontent: FileContentFields {
                    pattern,
                    file_pattern,
                },
            }
        }
    } else {
        let conditions: Vec<KonveyorCondition> = group.into_iter().map(|r| r.when).collect();
        if conditions.len() == 1 {
            conditions.into_iter().next().unwrap()
        } else {
            let unique = dedup_conditions(conditions);
            if unique.len() == 1 {
                unique.into_iter().next().unwrap()
            } else {
                KonveyorCondition::Or { or: unique }
            }
        }
    };

    let fix_strategy = {
        let strats: Vec<FixStrategyEntry> = all_strategies.into_iter().filter_map(|s| s).collect();
        if strats.is_empty() {
            None
        } else if strats.len() == 1 {
            Some(strats.into_iter().next().unwrap())
        } else {
            let mut best = strats[0].strategy.clone();
            let mut best_prio = strategy_priority(&best);
            for s in &strats {
                let p = strategy_priority(&s.strategy);
                if p > best_prio {
                    best_prio = p;
                    best = s.strategy.clone();
                }
            }
            let has_structural_migration = strats
                .iter()
                .any(|s| s.strategy == "LlmAssisted" && !s.member_mappings.is_empty());
            if has_structural_migration {
                best = "LlmAssisted".to_string();
            }
            let matching: Vec<&FixStrategyEntry> =
                strats.iter().filter(|s| s.strategy == best).collect();
            let mappings: Vec<MappingEntry> = matching.iter().map(|s| s.to_mapping()).collect();
            let primary = matching
                .iter()
                .find(|s| !s.member_mappings.is_empty())
                .copied()
                .unwrap_or(matching[0]);
            Some(FixStrategyEntry {
                strategy: best,
                from: primary.from.clone(),
                to: primary.to.clone(),
                component: primary.component.clone(),
                prop: primary.prop.clone(),
                mappings,
                member_mappings: primary.member_mappings.clone(),
                removed_members: primary.removed_members.clone(),
                replacement: primary.replacement.clone(),
                overlap_ratio: primary.overlap_ratio,
                package: primary.package.clone(),
                new_version: primary.new_version.clone(),
            })
        }
    };

    KonveyorRule {
        rule_id,
        labels,
        effort,
        category: first_category,
        description,
        message,
        links: Vec::new(),
        when,
        fix_strategy,
    }
}

/// Priority for fix strategy type. Higher = more actionable.
pub fn strategy_priority(strategy: &str) -> u8 {
    match strategy {
        "Rename" => 5,
        "RemoveProp" => 4,
        "CssVariablePrefix" => 4,
        "ImportPathChange" => 3,
        "PropValueChange" => 2,
        "PropTypeChange" => 2,
        "LlmAssisted" => 1,
        _ => 0,
    }
}

/// Build a regex pattern from the common prefix of a list of symbol names.
pub fn build_common_prefix_pattern(symbols: &[&str]) -> String {
    if symbols.is_empty() {
        return ".*".to_string();
    }

    let mut prefix_groups: BTreeMap<String, usize> = BTreeMap::new();
    for sym in symbols {
        let parts: Vec<&str> = sym.splitn(3, '_').collect();
        let prefix = if parts.len() >= 2 {
            format!("{}_{}", parts[0], parts[1])
        } else {
            sym.to_string()
        };
        *prefix_groups.entry(prefix).or_insert(0) += 1;
    }

    let top_prefixes: Vec<&str> = symbols
        .iter()
        .filter_map(|s| s.split('_').next())
        .collect::<BTreeSet<&str>>()
        .into_iter()
        .collect();

    if top_prefixes.len() <= 5 {
        let alts: Vec<String> = top_prefixes.iter().map(|p| format!("{}_", p)).collect();
        format!(r"\b({})", alts.join("|"))
    } else {
        r"\b[a-z][a-z0-9_]+_(Color|BackgroundColor|FontSize|BorderWidth|BoxShadow|FontWeight|Width|Height|ZIndex)\b".to_string()
    }
}

/// Extract the file pattern from an existing condition (for reuse in consolidated rules).
pub fn extract_file_pattern_from_condition(condition: &KonveyorCondition) -> Option<String> {
    match condition {
        KonveyorCondition::FileContent { filecontent } => Some(filecontent.file_pattern.clone()),
        KonveyorCondition::Or { or } => or.first().and_then(extract_file_pattern_from_condition),
        _ => None,
    }
}

/// Increment the version number in a CSS prefix string.
pub fn increment_version_prefix(prefix: &str) -> String {
    let re = regex::Regex::new(r"v(\d+)").unwrap();
    re.replace(prefix, |caps: &regex::Captures| {
        let ver: u32 = caps[1].parse().unwrap_or(0);
        format!("v{}", ver + 1)
    })
    .to_string()
}

pub fn dedup_conditions(conditions: Vec<KonveyorCondition>) -> Vec<KonveyorCondition> {
    let mut seen = BTreeSet::new();
    let mut unique = Vec::new();
    for cond in conditions {
        let key = serde_json::to_string(&cond).unwrap_or_default();
        if seen.insert(key) {
            unique.push(cond);
        }
    }
    unique
}

/// Extract fix strategies from the final (post-consolidation) rules.
pub fn extract_fix_strategies(rules: &[KonveyorRule]) -> HashMap<String, FixStrategyEntry> {
    rules
        .iter()
        .filter_map(|r| {
            r.fix_strategy
                .as_ref()
                .map(|s| (r.rule_id.clone(), s.clone()))
        })
        .collect()
}

/// Write fix strategies JSON to the fix-guidance directory.
pub fn write_fix_strategies(
    fix_dir: &Path,
    strategies: &HashMap<String, FixStrategyEntry>,
) -> Result<()> {
    let path = fix_dir.join("fix-strategies.json");
    let json =
        serde_json::to_string_pretty(strategies).context("Failed to serialize fix strategies")?;
    std::fs::write(&path, &json).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

/// Write conformance rules to a separate file in the output directory.
pub fn write_conformance_rules(output_dir: &Path, rules: &[KonveyorRule]) -> Result<()> {
    let ruleset = KonveyorRuleset {
        name: "semver-conformance".to_string(),
        description: "Component usage conformance checks — verifies child component composition matches expected patterns".to_string(),
        labels: vec!["source=semver-analyzer".to_string()],
    };

    let ruleset_path = output_dir.join("conformance-ruleset.yaml");
    let ruleset_yaml =
        serde_yaml::to_string(&ruleset).context("Failed to serialize conformance ruleset")?;
    std::fs::write(&ruleset_path, &ruleset_yaml)
        .with_context(|| format!("Failed to write {}", ruleset_path.display()))?;

    let rules_path = output_dir.join("conformance-rules.yaml");
    let rules_yaml =
        serde_yaml::to_string(&rules).context("Failed to serialize conformance rules")?;
    std::fs::write(&rules_path, &rules_yaml)
        .with_context(|| format!("Failed to write {}", rules_path.display()))?;

    Ok(())
}

/// Write fix guidance to a separate sibling directory.
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
pub fn fix_guidance_dir_for(output_dir: &Path) -> std::path::PathBuf {
    let parent = output_dir.parent().unwrap_or(Path::new("."));
    parent.join("fix-guidance")
}

/// Regex for extracting member keys from token type strings.
pub fn member_key_re() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"\["([a-zA-Z_][a-zA-Z_0-9]*)"\]"#).unwrap()
    });
    &RE
}

pub fn api_change_to_strategy(
    change: &ApiChange,
    rename_patterns: &RenamePatterns,
    member_renames: &HashMap<String, String>,
    file_path: &str,
) -> Option<FixStrategyEntry> {
    match change.change {
        ApiChangeType::Renamed => {
            let before = change.before.as_deref().unwrap_or("");
            let after = change.after.as_deref().unwrap_or("");
            if after.contains("/deprecated/") && !before.contains("/deprecated/") {
                let mut e = FixStrategyEntry::new("ImportPathChange");
                e.from = extract_package_path(before);
                e.to = extract_package_path(after);
                return Some(e);
            }
            if before == after || extract_leaf_symbol(before) == extract_leaf_symbol(after) {
                let fp = extract_package_path(before);
                let tp = extract_package_path(after);
                if fp.is_some() && tp.is_some() && fp != tp {
                    let mut e = FixStrategyEntry::new("ImportPathChange");
                    e.from = fp;
                    e.to = tp;
                    return Some(e);
                }
            }
            Some(FixStrategyEntry::rename(
                extract_leaf_symbol(before),
                extract_leaf_symbol(after),
            ))
        }
        ApiChangeType::TypeChanged | ApiChangeType::SignatureChanged => {
            if let Some(ref before) = change.before {
                if is_single_quoted_value(before) {
                    let value = &before[1..before.len() - 1];
                    let (component, prop) = extract_component_prop(&change.symbol);
                    let mut e = FixStrategyEntry::new("PropValueChange");
                    e.from = Some(value.into());
                    e.component = component;
                    e.prop = prop;
                    return Some(e);
                }
            }
            if let Some((fp, tp)) = detect_version_prefix(&change.description) {
                return Some(FixStrategyEntry::with_from_to("CssVariablePrefix", fp, tp));
            }
            let (component, prop) = extract_component_prop(&change.symbol);
            let mut e = FixStrategyEntry::new("PropTypeChange");
            e.from = change.before.clone();
            e.to = change.after.clone();
            e.component = component;
            e.prop = prop;
            Some(e)
        }
        ApiChangeType::Removed => {
            if let Some(ref target) = change.migration_target {
                let member_mappings = target
                    .matching_members
                    .iter()
                    .map(|m| MemberMappingEntry {
                        old_name: m.old_name.clone(),
                        new_name: m.new_name.clone(),
                    })
                    .collect();
                return Some(FixStrategyEntry::structural_migration(
                    &target.removed_symbol,
                    &target.replacement_symbol,
                    member_mappings,
                    target.removed_only_members.clone(),
                    target.overlap_ratio,
                ));
            }

            if matches!(change.kind, ApiChangeKind::Property | ApiChangeKind::Field) {
                if let Some(RemovalDisposition::ReplacedByProp { ref new_prop }) =
                    change.removal_disposition
                {
                    let old_name = change
                        .symbol
                        .rsplit_once('.')
                        .map(|(_, p)| p)
                        .unwrap_or(&change.symbol);
                    return Some(FixStrategyEntry::rename(old_name, new_prop));
                }

                let (component, prop) = extract_component_prop(&change.symbol);
                let mut e = FixStrategyEntry::new("RemoveProp");
                e.component = component;
                e.prop = prop.or_else(|| Some(change.symbol.clone()));
                Some(e)
            } else if let Some(new_name) = member_renames.get(&change.symbol) {
                Some(FixStrategyEntry::rename(&change.symbol, new_name))
            } else if let Some(replacement) = rename_patterns.find_replacement(&change.symbol) {
                Some(FixStrategyEntry::rename(&change.symbol, &replacement))
            } else if file_path.contains("/deprecated/") {
                let mut e = FixStrategyEntry::new("LlmAssisted");
                e.from = Some(change.symbol.clone());
                Some(e)
            } else {
                Some(FixStrategyEntry::new("Manual"))
            }
        }
        ApiChangeType::VisibilityChanged => Some(FixStrategyEntry::new("Manual")),
    }
}

pub fn extract_component_prop(symbol: &str) -> (Option<String>, Option<String>) {
    if symbol.contains('.') {
        let parts: Vec<&str> = symbol.splitn(2, '.').collect();
        (Some(parts[0].to_string()), Some(parts[1].to_string()))
    } else {
        (None, None)
    }
}

pub fn extract_package_path(qualified_name: &str) -> Option<String> {
    let parts: Vec<&str> = qualified_name.split('/').collect();
    let pkg_idx = parts.iter().position(|&p| p == "packages")?;
    let pkg_name = parts.get(pkg_idx + 1)?;
    let internal_parts: Vec<&str> = parts[parts.iter().position(|&p| p == "dist")?..].to_vec();
    let has_deprecated = internal_parts.iter().any(|&p| p == "deprecated");
    let has_next = internal_parts.iter().any(|&p| p == "next");
    let mut path = pkg_name.to_string();
    if has_deprecated {
        path.push_str("/deprecated");
    } else if has_next {
        path.push_str("/next");
    }
    Some(path)
}

pub fn detect_version_prefix(description: &str) -> Option<(String, String)> {
    static PREFIX_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"--([a-zA-Z]+-v)(\d+)-").unwrap());
    let mut prefixes: Vec<String> = Vec::new();
    for cap in PREFIX_RE.captures_iter(description) {
        let prefix = format!("--{}{}-", &cap[1], &cap[2]);
        if !prefixes.contains(&prefix) {
            prefixes.push(prefix);
        }
        if prefixes.len() == 2 {
            break;
        }
    }
    if prefixes.len() == 2 {
        let base1: String = prefixes[0]
            .chars()
            .take_while(|c| !c.is_ascii_digit())
            .collect();
        let base2: String = prefixes[1]
            .chars()
            .take_while(|c| !c.is_ascii_digit())
            .collect();
        if base1 == base2 {
            return Some((prefixes[0].clone(), prefixes[1].clone()));
        }
    }
    None
}

/// Build a regex pattern for detecting usage of a changed symbol.
pub fn build_pattern(
    kind: &ApiChangeKind,
    change: &ApiChangeType,
    leaf_symbol: &str,
    before: &Option<String>,
) -> String {
    let name = if *change == ApiChangeType::Renamed {
        if let Some(ref before_val) = before {
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
            format!(r"\b{}\b", escaped)
        }
    }
}

/// Build a `frontend.referenced` condition for an API change.
pub fn build_frontend_condition(
    change: &ApiChange,
    leaf_symbol: &str,
    from_pkg: Option<&str>,
) -> KonveyorCondition {
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
    let from = from_pkg.map(|s| s.to_string());

    let parent_component = if change.symbol.contains('.') {
        let parts: Vec<&str> = change.symbol.splitn(2, '.').collect();
        Some(format!("^{}$", regex_escape(parts[0])))
    } else {
        None
    };

    let is_subpath_scoped = from
        .as_ref()
        .map_or(false, |f| f.starts_with('^') && f.ends_with('$'));

    match change.kind {
        ApiChangeKind::Class | ApiChangeKind::Interface
            if change.change == ApiChangeType::Renamed =>
        {
            let mut conditions = vec![KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: pattern.clone(),
                    location: "IMPORT".to_string(),
                    component: None,
                    parent: None,
                    value: None,
                    from: from.clone(),
                    parent_from: None,
                },
            }];
            if !is_subpath_scoped {
                conditions.insert(
                    0,
                    KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: pattern.clone(),
                            location: "JSX_COMPONENT".to_string(),
                            component: None,
                            parent: None,
                            value: None,
                            from: from.clone(),
                            parent_from: None,
                        },
                    },
                );
            }
            KonveyorCondition::Or { or: conditions }
        }

        ApiChangeKind::Class | ApiChangeKind::Interface => {
            let mut conditions = Vec::new();
            if !is_subpath_scoped {
                conditions.push(KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: pattern.clone(),
                        location: "JSX_COMPONENT".to_string(),
                        component: None,
                        parent: None,
                        value: None,
                        from: from.clone(),
                        parent_from: None,
                    },
                });
            }
            conditions.push(KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: pattern.clone(),
                    location: "IMPORT".to_string(),
                    component: None,
                    parent: None,
                    value: None,
                    from: from.clone(),
                    parent_from: None,
                },
            });
            if match_name.ends_with("Props") {
                let component_name = &match_name[..match_name.len() - 5];
                if !component_name.is_empty() {
                    let comp_pattern = format!("^{}$", regex_escape(component_name));
                    conditions.push(KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: comp_pattern,
                            location: "IMPORT".to_string(),
                            component: None,
                            parent: None,
                            value: None,
                            from: from.clone(),
                            parent_from: None,
                        },
                    });
                }
            }
            KonveyorCondition::Or { or: conditions }
        }

        ApiChangeKind::Property | ApiChangeKind::Field => {
            let value_filter = extract_value_filter(change);
            if is_subpath_scoped {
                KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern,
                        location: "IMPORT".to_string(),
                        component: None,
                        parent: None,
                        value: None,
                        from,
                        parent_from: None,
                    },
                }
            } else {
                KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern,
                        location: "JSX_PROP".to_string(),
                        component: parent_component,
                        parent: None,
                        value: value_filter,
                        from,
                        parent_from: None,
                    },
                }
            }
        }

        ApiChangeKind::Function | ApiChangeKind::Method => KonveyorCondition::FrontendReferenced {
            referenced: FrontendReferencedFields {
                pattern,
                location: if is_subpath_scoped {
                    "IMPORT".to_string()
                } else {
                    "FUNCTION_CALL".to_string()
                },
                component: None,
                parent: None,
                parent_from: None,
                value: None,
                from,
            },
        },

        ApiChangeKind::TypeAlias => {
            let mut conditions = Vec::new();
            if !is_subpath_scoped {
                conditions.push(KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: pattern.clone(),
                        location: "TYPE_REFERENCE".to_string(),
                        component: None,
                        parent: None,
                        value: None,
                        from: from.clone(),
                        parent_from: None,
                    },
                });
            }
            conditions.push(KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern,
                    location: "IMPORT".to_string(),
                    component: None,
                    parent: None,
                    value: None,
                    from,
                    parent_from: None,
                },
            });
            if conditions.len() == 1 {
                conditions.into_iter().next().unwrap()
            } else {
                KonveyorCondition::Or { or: conditions }
            }
        }

        _ => {
            let is_component = match_name
                .chars()
                .next()
                .map_or(false, |c| c.is_ascii_uppercase());
            if is_component && !is_subpath_scoped {
                KonveyorCondition::Or {
                    or: vec![
                        KonveyorCondition::FrontendReferenced {
                            referenced: FrontendReferencedFields {
                                pattern: pattern.clone(),
                                location: "JSX_COMPONENT".to_string(),
                                component: None,
                                parent: None,
                                value: None,
                                from: from.clone(),
                                parent_from: None,
                            },
                        },
                        KonveyorCondition::FrontendReferenced {
                            referenced: FrontendReferencedFields {
                                pattern,
                                location: "IMPORT".to_string(),
                                component: None,
                                parent: None,
                                value: None,
                                from,
                                parent_from: None,
                            },
                        },
                    ],
                }
            } else {
                KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern,
                        location: "IMPORT".to_string(),
                        component: None,
                        parent: None,
                        value: None,
                        from,
                        parent_from: None,
                    },
                }
            }
        }
    }
}

/// Extract a value filter from an ApiChange if it represents a single union
/// member removal.
pub fn extract_value_filter(change: &ApiChange) -> Option<String> {
    let before = change.before.as_deref()?;
    if is_single_quoted_value(before) {
        let value = &before[1..before.len() - 1];
        if !value.is_empty() {
            return Some(format!("^{}$", regex_escape(value)));
        }
    }
    None
}

/// Check if a string is a single quoted value (not a union).
pub fn is_single_quoted_value(s: &str) -> bool {
    let s = s.trim();
    if s.len() < 2 {
        return false;
    }
    let quote = s.as_bytes()[0];
    if quote != b'\'' && quote != b'"' {
        return false;
    }
    if s.as_bytes()[s.len() - 1] != quote {
        return false;
    }
    let inner = &s[1..s.len() - 1];
    !inner.contains(" | ") && !inner.contains('|')
}

/// Parse string literal union members from a type expression.
pub fn parse_union_string_values(type_expr: &str) -> BTreeSet<String> {
    let mut values = BTreeSet::new();
    let re = regex::Regex::new(r"'([^']+)'").unwrap();
    for cap in re.captures_iter(type_expr) {
        values.insert(cap[1].to_string());
    }
    values
}

/// Compute the removed union member values between before and after type
/// expressions.
pub fn extract_removed_union_values(change: &ApiChange) -> Vec<String> {
    let before = match change.before.as_deref() {
        Some(b) => b,
        None => return Vec::new(),
    };
    let after = match change.after.as_deref() {
        Some(a) => a,
        None => return Vec::new(),
    };
    if change.change != ApiChangeType::TypeChanged {
        return Vec::new();
    }
    let before_vals = parse_union_string_values(before);
    let after_vals = parse_union_string_values(after);
    if before_vals.len() < 2 {
        return Vec::new();
    }
    before_vals.difference(&after_vals).cloned().collect()
}

/// Compute the added union member values between before and after type
/// expressions.
pub fn extract_added_union_values(change: &ApiChange) -> Vec<String> {
    let before = match change.before.as_deref() {
        Some(b) => b,
        None => return Vec::new(),
    };
    let after = match change.after.as_deref() {
        Some(a) => a,
        None => return Vec::new(),
    };
    if change.change != ApiChangeType::TypeChanged {
        return Vec::new();
    }
    let before_vals = parse_union_string_values(before);
    let after_vals = parse_union_string_values(after);
    after_vals.difference(&before_vals).cloned().collect()
}

// ── Message building ────────────────────────────────────────────────────

pub fn build_api_message(change: &ApiChange, file_path: &str) -> String {
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

pub fn effort_for_api_change(change: &ApiChangeType) -> u32 {
    match change {
        ApiChangeType::Removed => 5,
        ApiChangeType::SignatureChanged => 3,
        ApiChangeType::TypeChanged => 3,
        ApiChangeType::VisibilityChanged => 3,
        ApiChangeType::Renamed => 1,
    }
}

// ── Label helpers ───────────────────────────────────────────────────────

pub fn api_change_type_label(change: &ApiChangeType) -> &'static str {
    match change {
        ApiChangeType::Removed => "removed",
        ApiChangeType::SignatureChanged => "signature-changed",
        ApiChangeType::TypeChanged => "type-changed",
        ApiChangeType::VisibilityChanged => "visibility-changed",
        ApiChangeType::Renamed => "renamed",
    }
}

pub fn api_kind_label(kind: &ApiChangeKind) -> &'static str {
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

// ── Utility helpers ─────────────────────────────────────────────────────

/// Extract the leaf symbol name from a potentially dotted path.
pub fn extract_leaf_symbol(symbol: &str) -> &str {
    symbol.rsplit('.').next().unwrap_or(symbol)
}

/// Extract the trailing PascalCase suffix from a snake_case token constant name.
pub fn extract_trailing_suffix(name: &str) -> Option<&str> {
    let last_underscore = name.rfind('_')?;
    let suffix = &name[last_underscore + 1..];
    if !suffix.is_empty()
        && suffix.chars().next().map_or(false, |c| c.is_uppercase())
        && suffix.chars().any(|c| c.is_lowercase())
        && !suffix.contains('_')
    {
        Some(suffix)
    } else {
        None
    }
}

/// Derive the longest common suffix from a list of component names.
pub fn derive_common_suffix(names: &[String]) -> Option<String> {
    let valid: Vec<&str> = names
        .iter()
        .map(|s| s.as_str())
        .filter(|s| {
            s.chars().next().map_or(false, |c| c.is_uppercase())
                && !s.contains(' ')
                && !s.contains('(')
                && !s.contains('/')
        })
        .collect();

    if valid.len() < 2 {
        return None;
    }

    let min_len = valid.iter().map(|s| s.len()).min().unwrap_or(0);
    let first = valid[0];
    let first_bytes = first.as_bytes();

    let mut suffix_len = 0;
    for i in 1..=min_len {
        let idx = first.len() - i;
        let ch = first_bytes[idx];
        if valid[1..].iter().all(|s| {
            let sidx = s.len() - i;
            s.as_bytes()[sidx] == ch
        }) {
            suffix_len = i;
        } else {
            break;
        }
    }

    if suffix_len >= 3 {
        Some(first[first.len() - suffix_len..].to_string())
    } else {
        None
    }
}

/// Extract the target prop name from a composition change's `new_parent` field.
pub fn extract_target_prop(new_parent: &str) -> Option<&str> {
    let ctx = new_parent.split('(').nth(1)?;
    let ctx = ctx.trim_end_matches(')').trim();
    if !ctx.contains(" prop") {
        return None;
    }
    let prop_part = ctx.split(" prop").next()?;
    prop_part.split_whitespace().last()
}

/// Sanitize a string for use in a Konveyor rule ID.
pub fn sanitize_id(s: &str) -> String {
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
    if result.ends_with('-') {
        result.pop();
    }

    result
}

/// Generate a unique rule ID by appending a counter for duplicates.
pub fn unique_id(base: String, counts: &mut HashMap<String, usize>) -> String {
    let count = counts.entry(base.clone()).or_insert(0);
    *count += 1;
    if *count == 1 {
        base
    } else {
        format!("{}-{}", base, count)
    }
}

/// Escape special regex characters in a symbol name.
pub fn regex_escape(s: &str) -> String {
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

pub fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => {
            let upper: String = first.to_uppercase().collect();
            upper + chars.as_str()
        }
    }
}
