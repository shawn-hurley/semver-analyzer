//! Shared types and utilities for Konveyor rule generation.
//!
//! This crate provides both language-agnostic infrastructure and some
//! JS/TS-specific functions that are in the process of being migrated to
//! `crates/ts/`. The JS-specific functions are re-exported through
//! `semver_analyzer_ts::konveyor_frontend` for the correct dependency direction.
//!
//! ## Language-agnostic (stays here)
//!
//! Rule types, consolidation, regex builders, file I/O, fix strategy types,
//! rename pattern loading, rule merging/deduplication.
//!
//! ## JS/TS-specific (re-exported via `crates/ts/src/konveyor_frontend.rs`)
//!
//! ### Public functions
//! - `build_frontend_condition` — JSX/React component/prop condition builder
//! - `api_change_to_strategy` — fix strategy with npm/CSS awareness
//! - `suppress_redundant_prop_rules` — React prop rule deduplication
//! - `suppress_redundant_prop_value_rules` — JSX prop value rule dedup
//! - `resolve_npm_package` — npm monorepo package lookup
//! - `read_package_json_at_ref` / `read_package_json_from_file` — package.json parsing
//! - `extract_package_from_path` — npm path extraction
//!
//! ### Private helpers (called only by the above)
//! - `extract_package_path` — npm dist/src path parsing
//! - `detect_version_prefix` — CSS variable version prefix detection
//! - `parse_value_rename_field` — structured value rename parsing
//! - `looks_like_import_path` — npm import path heuristic
//! - `extract_value_filter` — union value extraction for conditions
//! - `is_single_quoted_value` — quoted string detection
//! - `extract_component_prop` — `Component.prop` splitting
//! - `default_ts_file_pattern` — TS/JS file extension regex
//!
//! ### Config types
//! - `PackageInfo`, `CssVarRenameEntry`, `CompositionRuleEntry`,
//!   `PropRenameEntry`, `ComponentWarningEntry`, `MissingImportEntry`,
//!   `ValueReviewEntry`

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use semver_analyzer_core::{ApiChange, ApiChangeKind, ApiChangeType, RemovalDisposition};

/// Default file pattern used as a fallback in `merge_rule_group` when no
/// condition has a file pattern set. Currently defaults to JS/TS files.
/// Language crates should set `file_pattern` on rule conditions before
/// consolidation to avoid this fallback.
pub const DEFAULT_FILE_PATTERN: &str = "*.{ts,tsx,js,jsx,mjs,cjs}";

// ── Re-exports from the shared konveyor-core crate ──────────────────────
//
// These types are the canonical definitions shared with
// frontend-analyzer-provider. Re-exported here so existing code that
// imports from `semver_analyzer_konveyor_core` continues to work.

// Rule definition types
pub use konveyor_core::rule::{
    dedup_conditions, extract_file_pattern_from_condition, extract_frontend_refs,
    FileContentFields, FrontendDependencyFields, FrontendPatternFields, FrontendReferencedFields,
    JavaAnnotatedFields, JavaAnnotationElement, JavaDependencyFields, JavaReferencedFields,
    JsonFields, KonveyorCondition, KonveyorLink, KonveyorRule, KonveyorRuleset,
};

// Fix strategy types
pub use konveyor_core::fix::{
    extract_fix_strategies, strategy_priority, write_fix_strategies, DeprecatedMigrationContext,
    FixConfidence, FixGuidanceDoc, FixGuidanceEntry, FixSource, FixStrategyEntry,
    FixStrategyKind as FixStrategy, FixSummary, MappingEntry, MemberMappingEntry, MigrationInfo,
    PropMigrationEntry,
};

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

/// An explicit CSS custom property rename mapping.
///
/// Maps a v5 CSS variable name to its v6 equivalent. Used for global tokens
/// where the entire variable name structure changed (not just the version prefix).
/// For example: `--pf-v5-global--BackgroundColor--100` → `--pf-t--global--background--color--100`.
///
/// These are applied as individual `FrontendCssVar` rules during Konveyor rule
/// generation, ensuring inline CSS variable string references (e.g., in style props)
/// are correctly migrated.
#[derive(Debug, Clone, Deserialize)]
pub struct CssVarRenameEntry {
    /// The old (v5) CSS custom property name.
    pub from: String,
    /// The new (v6) CSS custom property name.
    pub to: String,
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
    /// Explicit token/constant rename mappings (old_name → new_name).
    ///
    /// These override the algorithmic rename detection in the diff engine.
    /// Use this when the automated matching produces wrong pairings, or when
    /// upstream documentation provides authoritative mappings.
    ///
    /// Format: `{ "global_success_color_100": "t_global_color_status_success_100" }`
    #[serde(default)]
    pub token_mappings: HashMap<String, String>,
    /// Explicit CSS custom property renames (old CSS var → new CSS var).
    ///
    /// These generate individual `FrontendCssVar` rules that replace the full
    /// CSS variable name in inline style references, SCSS files, etc.
    /// Required because the broad `CssVariablePrefix` strategy only swaps
    /// the version prefix (e.g., `--pf-v5-` → `--pf-v6-`) but global tokens
    /// were entirely restructured in v6.
    #[serde(default)]
    pub css_var_renames: Vec<CssVarRenameEntry>,
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
    /// Explicit token/constant rename mappings (old_name → new_name).
    /// These override algorithmic rename detection.
    pub token_mappings: HashMap<String, String>,
    /// Explicit CSS custom property renames (old CSS var → new CSS var).
    pub css_var_renames: Vec<CssVarRenameEntry>,
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

        tracing::info!(
            count = patterns.len(),
            path = %path.display(),
            "Loaded rename patterns"
        );
        if !file.composition_rules.is_empty() {
            tracing::info!(
                count = file.composition_rules.len(),
                "Loaded composition rules"
            );
        }
        if !file.prop_renames.is_empty() {
            tracing::info!(count = file.prop_renames.len(), "Loaded prop renames");
        }
        if !file.value_reviews.is_empty() {
            tracing::info!(count = file.value_reviews.len(), "Loaded value reviews");
        }
        if !file.missing_imports.is_empty() {
            tracing::info!(
                count = file.missing_imports.len(),
                "Loaded missing import rules"
            );
        }
        if !file.component_warnings.is_empty() {
            tracing::info!(
                count = file.component_warnings.len(),
                "Loaded component warnings"
            );
        }
        if !file.token_mappings.is_empty() {
            tracing::info!(count = file.token_mappings.len(), "Loaded token mappings");
        }
        if !file.css_var_renames.is_empty() {
            tracing::info!(
                count = file.css_var_renames.len(),
                "Loaded CSS variable renames"
            );
        }
        Ok(Self {
            patterns,
            composition_rules: file.composition_rules,
            prop_renames: file.prop_renames,
            value_reviews: file.value_reviews,
            missing_imports: file.missing_imports,
            component_warnings: file.component_warnings,
            token_mappings: file.token_mappings,
            css_var_renames: file.css_var_renames,
        })
    }

    /// Try to find a replacement for a removed symbol name.
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
    pub fn add_pattern(&mut self, match_regex: &str, replace: &str) {
        match regex::Regex::new(match_regex) {
            Ok(re) => self.patterns.push((re, replace.to_string())),
            Err(e) => tracing::warn!(
                pattern = match_regex,
                error = %e,
                "Skipping invalid inferred pattern"
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
            token_mappings: HashMap::new(),
            css_var_renames: Vec::new(),
        }
    }

    /// Look up a user-provided token mapping (exact match).
    pub fn get_token_mapping(&self, old_name: &str) -> Option<&str> {
        self.token_mappings.get(old_name).map(|s| s.as_str())
    }
}

// ── Public API ───────────────────────────────────────────────────────────

/// Minimum number of constants from the same package with the same change type
/// before they are collapsed into a single combined rule.
pub const CONSTANT_COLLAPSE_THRESHOLD: usize = 10;

/// Grouping key for collapsible constant changes: package + change type + strategy.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConstantGroupKey {
    pub package: String,
    pub change_type: ApiChangeType,
    pub strategy: String,
}

/// A compound token with its removed and added member key suffixes.
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

// ── Shared functions ────────────────────────────────────────────────────

/// Build a regex pattern that exactly matches any of the given symbol names.
///
/// Uses a precise alternation `^(sym1|sym2|...)$` rather than prefix heuristics.
/// The `from` field on the condition already scopes to the package, so the pattern
/// only needs to discriminate which specific exports are affected.
pub fn build_token_prefix_pattern(symbols: &[&str]) -> String {
    if symbols.is_empty() {
        return ".*".to_string();
    }
    let alts: Vec<String> = symbols
        .iter()
        .map(|s| regex_escape(s))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    format!("^({})$", alts.join("|"))
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

    // For Rename strategies, collect per-token mappings from ALL changes
    // rather than just using the first change's strategy.
    let strategy = if key.strategy == "Rename" {
        let mut combined = FixStrategyEntry::new("Rename");
        for (_, _, strat) in changes {
            if strat.strategy == "Rename" {
                combined.mappings.push(MappingEntry {
                    from: strat.from.clone(),
                    to: strat.to.clone(),
                    component: None,
                    prop: None,
                });
            }
        }
        Some(combined)
    } else {
        Some(changes[0].2.clone())
    };

    let change_type_str = api_change_type_label(&key.change_type);
    let kind_str = api_kind_label(&ApiChangeKind::Constant);
    let slug = key.package.replace('@', "").replace(['/', '.'], "-");
    let strategy_slug = key.strategy.to_lowercase().replace(' ', "-");
    let base_id = format!(
        "semver-{}-constant-{}-{}-combined",
        slug, change_type_str, strategy_slug
    );
    let rule_id = unique_id(base_id, id_counts);

    let mut message = format!(
        "{} constants from `{}` had breaking changes ({}).\n",
        changes.len(),
        key.package,
        change_type_str,
    );

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
                file_pattern: None,
                parent_from: None,
                not_parent: None,
                child: None,
                not_child: None,
                requires_child: None,
            },
        },
        fix_strategy: strategy,
    }
}

// suppress_redundant_token_rules has been removed — the V2 constantgroup
// path and consolidation handle deduplication of token rules.

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
        tracing::debug!(
            count = suppressed,
            "Suppressed redundant prop removal rules (covered by component-import-deprecated)"
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
        tracing::debug!(
            count = suppressed,
            "Suppressed redundant prop-value-change rules (covered by type-changed)"
        );
    }

    rules
}

/// Merge rules that have identical detection conditions into a single rule.
///
/// Multiple rules can target the same `(component, prop, from, location)` tuple
/// when different change types (type-changed, removed, value-removed, etc.)
/// produce separate rules for the same detection pattern. Since the provider
/// evaluates each rule independently, duplicates cause the same file+line to
/// be reported multiple times.
///
/// This function merges such duplicates by keeping the first rule's condition
/// and combining the messages and labels from all duplicates.
pub fn merge_duplicate_conditions(rules: Vec<KonveyorRule>) -> Vec<KonveyorRule> {
    // Build a key from the serialized `when` clause. Rules with identical
    // conditions will produce identical keys. Use a HashMap for grouping
    // and a Vec to preserve insertion order.
    //
    // Rules with has-codemod=true are never merged — they carry specific
    // fix strategy data (Rename mappings, etc.) that would be lost if
    // merged with other rules sharing the same detection condition.
    let mut group_index: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<Vec<KonveyorRule>> = Vec::new();
    for rule in rules {
        let has_codemod = rule.labels.iter().any(|l| l == "has-codemod=true");
        let key = if has_codemod {
            // Unique key — ensures this rule is always a singleton group
            rule.rule_id.clone()
        } else {
            serde_json::to_string(&rule.when).unwrap_or_default()
        };
        if let Some(&idx) = group_index.get(&key) {
            groups[idx].push(rule);
        } else {
            let idx = groups.len();
            group_index.insert(key, idx);
            groups.push(vec![rule]);
        }
    }

    let mut merged = Vec::new();
    let mut total_merged = 0usize;
    for group in groups {
        if group.len() == 1 {
            merged.push(group.into_iter().next().unwrap());
            continue;
        }

        total_merged += group.len() - 1;

        // Take the first rule as the base, merge labels from all duplicates.
        let mut iter = group.into_iter();
        let mut base = iter.next().unwrap();

        // Collect labels from all duplicate rules (deduplicated, sorted)
        let mut all_labels: BTreeSet<String> = BTreeSet::new();
        for l in &base.labels {
            all_labels.insert(l.clone());
        }
        for dup in iter {
            for l in &dup.labels {
                all_labels.insert(l.clone());
            }
        }
        base.labels = all_labels.into_iter().collect();

        merged.push(base);
    }

    if total_merged > 0 {
        tracing::debug!(
            count = total_merged,
            "Merged rules with duplicate detection conditions"
        );
    }

    merged
}

/// Consolidate rules by grouping related rules into single combined rules.
///
/// Uses a default package extractor that treats the file path as the package
/// name. For language-specific package extraction (e.g., npm monorepos),
/// use `consolidate_rules_with` and provide a custom extractor.
pub fn consolidate_rules(rules: Vec<KonveyorRule>) -> (Vec<KonveyorRule>, HashMap<String, String>) {
    consolidate_rules_with(rules, |path| path.to_string())
}

/// Consolidate rules with a custom package extraction function.
///
/// The `package_extractor` maps a file path to a package name for grouping
/// constant-level rules. For npm: `extract_package_from_path`. For Maven:
/// extract groupId from directory structure. For generic: identity function.
pub fn consolidate_rules_with(
    rules: Vec<KonveyorRule>,
    package_extractor: impl Fn(&str) -> String,
) -> (Vec<KonveyorRule>, HashMap<String, String>) {
    let mut groups: BTreeMap<String, Vec<KonveyorRule>> = BTreeMap::new();
    for rule in rules {
        let key = consolidation_key(&rule, &package_extractor);
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

/// Compute the consolidation group key for a rule.
///
/// Uses the provided `package_extractor` to map file paths to package
/// names when grouping constant-level rules.
pub fn consolidation_key(
    rule: &KonveyorRule,
    package_extractor: &dyn Fn(&str) -> String,
) -> String {
    // Combined constant rules are already collapsed — never re-merge them.
    if rule.rule_id.contains("-combined") {
        return rule.rule_id.clone();
    }

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
            .is_some_and(|c| c.is_ascii_uppercase());
        if !is_component_constant {
            let package = package_extractor(file_key);
            return format!("{}-constant-removed", package);
        }
    }

    // type-changed constants: fall through to the default file-based key
    // so that unrelated components (e.g., Banner vs Card vs Truncate) are
    // not grouped together.  Previously this grouped by package only.

    // Renamed properties with codemod data: keep as singleton.
    // These rules carry per-prop Rename mappings in their fix_strategy
    // that would be lost or mis-classified if merged.
    if change_type == "renamed" && (kind == "property" || kind == "constant") {
        let has_codemod = rule.labels.iter().any(|l| l == "has-codemod=true");
        if has_codemod {
            return rule.rule_id.clone();
        }
        // Non-codemod renamed constants: group by package
        let symbol = rule.description.split('`').nth(1).unwrap_or("");
        let is_component_constant = symbol
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase());
        if !is_component_constant {
            let package = package_extractor(file_key);
            return format!("{}-constant-renamed-has-codemod=false", package);
        }
    }

    match change_type {
        "css-variable"
        | "new-sibling-component"
        | "component-removal"
        | "dependency-update"
        | "composition"
        | "hierarchy-composition"
        | "deprecated-migration" => {
            return rule.rule_id.clone();
        }
        _ => {}
    }

    // Include has-codemod in the key so rules with different codemod
    // flags don't merge. The fix engine can't partially handle a group
    // where some entries need LLM and others need codemod.
    let codemod = rule
        .labels
        .iter()
        .find(|l| l.starts_with("has-codemod="))
        .map(|l| l.as_str())
        .unwrap_or("has-codemod=false");

    format!("{}-{}-{}-{}", file_key, kind, change_type, codemod)
}

/// Read package.json at a specific git ref using `git show`.
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

    let has_deprecated = parts.contains(&"deprecated");
    let has_next = parts.contains(&"next");

    if has_deprecated {
        Some(format!("{}/deprecated", base_name))
    } else if has_next {
        Some(format!("{}/next", base_name))
    } else {
        Some(base_name.clone())
    }
}

/// Extract a package name from a file path for consolidation grouping.
pub fn extract_package_from_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if let Some(pkg_idx) = parts.iter().position(|&p| p == "packages") {
        if let Some(pkg_name) = parts.get(pkg_idx + 1) {
            let has_deprecated = parts.contains(&"deprecated");
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
    // Track has-codemod conservatively: if ANY entry is has-codemod=false,
    // the entire group should be has-codemod=false because the fix engine
    // can't partially handle a grouped rule.
    let mut any_no_codemod = false;
    for rule in &group {
        for label in &rule.labels {
            if label == "has-codemod=false" {
                any_no_codemod = true;
            }
            all_labels.insert(label.clone());
        }
    }
    // Resolve conflicting has-codemod labels
    if any_no_codemod {
        all_labels.remove("has-codemod=true");
        all_labels.insert("has-codemod=false".to_string());
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
    /// Maximum number of steps before switching to a compact summary message.
    /// Beyond this threshold, verbose "Step X of N" formatting becomes impractical.
    const MAX_VERBOSE_STEPS: usize = 50;

    let message = if unique_messages.len() == 1 {
        unique_messages[0].to_string()
    } else if unique_messages.len() > MAX_VERBOSE_STEPS {
        // Use compact summary for very large groups instead of listing every step.
        let sample_count = 5.min(unique_messages.len());
        let mut msg = format!(
            "{} related changes detected. Showing first {} of {}:\n\n",
            unique_messages.len(),
            sample_count,
            unique_messages.len()
        );
        for (i, m) in unique_messages.iter().take(sample_count).enumerate() {
            msg.push_str(&format!("{}. {}\n\n", i + 1, m));
        }
        if unique_messages.len() > sample_count {
            msg.push_str(&format!(
                "... and {} more changes.",
                unique_messages.len() - sample_count
            ));
        }
        msg
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
                    file_pattern: None,
                    parent_from: None,
                    not_parent: None,
                    child: None,
                    not_child: None,
                    requires_child: None,
                },
            }
        } else {
            // Fallback file pattern when no condition has one.
            // This default covers JS/TS files. Language crates that
            // generate rules for other file types should set file_pattern
            // on each rule's condition before consolidation.
            let file_pattern = extract_file_pattern_from_condition(&group[0].when)
                .unwrap_or_else(|| DEFAULT_FILE_PATTERN.to_string());
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
        let strats: Vec<FixStrategyEntry> = all_strategies.into_iter().flatten().collect();
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
            // Preserve nested mappings: if a sub-strategy already has a
            // `mappings` array (e.g., constantgroup rules with per-token
            // Rename entries), flatten those into the merged rule instead
            // of discarding them via to_mapping() (which only reads
            // top-level from/to).
            let mappings: Vec<MappingEntry> = matching
                .iter()
                .flat_map(|s| {
                    if s.mappings.is_empty() {
                        vec![s.to_mapping()]
                    } else {
                        s.mappings.clone()
                    }
                })
                .collect();
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
                ..Default::default()
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

/// Build a regex pattern that exactly matches any of the given symbol names.
///
/// Uses a precise alternation `^(sym1|sym2|...)$` rather than prefix heuristics.
/// This replaces the previous approach that fell back to broad patterns when
/// there were too many unique prefixes.
pub fn build_common_prefix_pattern(symbols: &[&str]) -> String {
    if symbols.is_empty() {
        return ".*".to_string();
    }
    let alts: Vec<String> = symbols
        .iter()
        .map(|s| regex_escape(s))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    format!("^({})$", alts.join("|"))
}

/// Write conformance rules to a separate file in the output directory.
pub fn write_conformance_rules(output_dir: &Path, rules: &[KonveyorRule]) -> Result<()> {
    let ruleset = KonveyorRuleset {
        name: "semver-conformance".to_string(),
        description: "Component usage conformance checks -- verifies child component composition matches expected patterns".to_string(),
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

            // Structured value rename: "Component.prop = value"
            // Extract just the value part for the Rename strategy.
            if let (Some((_, _, old_val)), Some((_, _, new_val))) = (
                parse_value_rename_field(before),
                parse_value_rename_field(after),
            ) {
                return Some(FixStrategyEntry::rename(old_val, new_val));
            }

            // Handle import path relocations where before/after are direct import
            // specifiers (e.g., "@patternfly/react-charts" → "@patternfly/react-charts/victory").
            // These come from Relocated changes detected by the diff engine when a symbol's
            // import_path changed between versions.
            // Guard: symbol_summary strings contain ": " (e.g., "variable: foo")
            // and must not be treated as import paths.
            //
            // IMPORTANT: This check must come BEFORE the Constant/design-token
            // branch below.  Relocated chart components (Variable → Constant kind)
            // have import-path strings in before/after, not symbol summaries.
            // If the Constant branch fires first it calls
            // `extract_name_from_summary(after)` on the import path, producing a
            // nonsensical `Rename { from: "Chart", to: "@patternfly/react-charts/victory" }`
            // that corrupts every file containing "Chart".
            if !before.is_empty()
                && !after.is_empty()
                && before != after
                && !before.contains("packages/")
                && !after.contains("packages/")
                && looks_like_import_path(before)
                && looks_like_import_path(after)
            {
                let mut e = FixStrategyEntry::new("ImportPathChange");
                e.from = Some(before.to_string());
                e.to = Some(after.to_string());
                return Some(e);
            }

            // Constants / design tokens: generate Rename directly from the symbol
            // name and the new name extracted from the after summary.
            // The before/after fields are symbol_summary strings (e.g.,
            // "variable: global_success_color_100: { ... }") which must NOT be
            // treated as import paths or fed raw into Rename strategies.
            //
            // User-provided token_mappings override the algorithmic pairing.
            if matches!(change.kind, ApiChangeKind::Constant) {
                let new_name =
                    if let Some(user_mapping) = rename_patterns.get_token_mapping(&change.symbol) {
                        user_mapping
                    } else {
                        extract_name_from_summary(after)
                    };
                return Some(FixStrategyEntry::rename(&change.symbol, new_name));
            }

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
            // Restructured prop: type-incompatible rename (e.g.,
            // splitButtonOptions: SplitButtonOptions → splitButtonItems: ReactNode[]).
            // Use LLM-assisted fixing since the value needs restructuring.
            if let Some(RemovalDisposition::ReplacedByMember { ref new_member }) =
                change.removal_disposition
            {
                let (component, prop) = extract_component_prop(&change.symbol);
                let mut e = FixStrategyEntry::new("LlmAssisted");
                e.component = component;
                e.prop = prop;
                e.replacement = Some(new_member.clone());
                return Some(e);
            }

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
            // Deprecated component replacements (e.g., Chip → Label) need
            // LLM-assisted fixing — the replacement component has a different
            // prop surface, so this is not a mechanical type change.
            if change
                .description
                .contains("was deprecated and replaced by")
            {
                let mut e = FixStrategyEntry::new("LlmAssisted");
                e.from = change.before.clone();
                e.to = change.after.clone();
                return Some(e);
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
                if let Some(RemovalDisposition::ReplacedByMember { ref new_member }) =
                    change.removal_disposition
                {
                    // Enum value replacement: before is a quoted value like 'light'.
                    // Rename the VALUE, not the prop name.
                    if let Some(ref before) = change.before {
                        if is_single_quoted_value(before) {
                            let old_val = &before[1..before.len() - 1];
                            return Some(FixStrategyEntry::rename(old_val, new_member));
                        }
                    }
                    // Prop rename: before is not a quoted value.
                    let old_name = change
                        .symbol
                        .rsplit_once('.')
                        .map(|(_, p)| p)
                        .unwrap_or(&change.symbol);
                    return Some(FixStrategyEntry::rename(old_name, new_member));
                }

                // Check if this is a removed enum VALUE (e.g., variant='light')
                // rather than a removed prop. A quoted `before` value indicates
                // an enum member removal. Use PropValueChange instead of
                // RemoveProp so the SD pipeline's sd-prop-value-* rule (which
                // knows the replacement value) takes precedence.
                if let Some(ref before) = change.before {
                    if is_single_quoted_value(before) {
                        let value = &before[1..before.len() - 1];
                        let (component, prop) = extract_component_prop(&change.symbol);
                        let mut e = FixStrategyEntry::new("PropValueChange");
                        e.component = component;
                        e.prop = prop;
                        e.from = Some(value.to_string());
                        return Some(e);
                    }
                }

                let (component, prop) = extract_component_prop(&change.symbol);
                let mut e = FixStrategyEntry::new("RemoveProp");
                e.component = component;
                e.prop = prop.or_else(|| Some(change.symbol.clone()));
                Some(e)
            } else if matches!(change.kind, ApiChangeKind::Constant)
                && rename_patterns.get_token_mapping(&change.symbol).is_some()
            {
                // User-provided token_mappings override for removed constants
                // that actually have a v6 replacement (e.g., global_Color_dark_100
                // → t_global_text_color_regular).
                let new_name = rename_patterns.get_token_mapping(&change.symbol).unwrap();
                Some(FixStrategyEntry::rename(&change.symbol, new_name))
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

    // Find the start of internal paths — after dist/<variant>/ or src/
    let internal_start = parts.iter().position(|&p| p == "dist" || p == "src")?;

    // Skip the "dist" or "src" segment, and if "dist", also skip the variant (esm, js, etc.)
    let content_start = if parts.get(internal_start) == Some(&"dist") {
        // dist/<variant>/... — skip both "dist" and the variant
        internal_start + 2
    } else {
        // src/... — skip just "src"
        internal_start + 1
    };

    let internal_parts = &parts[content_start..];

    // Check for known subpath patterns (deprecated, next, and other subpath exports)
    let has_deprecated = internal_parts.contains(&"deprecated");
    let has_next = internal_parts.contains(&"next");

    let mut path = pkg_name.to_string();
    if has_deprecated {
        path.push_str("/deprecated");
    } else if has_next {
        path.push_str("/next");
    } else if let Some(&first_segment) = internal_parts.first() {
        // Check if the first segment after src/ or dist/<variant>/ is a
        // subpath export entry point (e.g., "victory" in src/victory/components/...).
        // Heuristic: if it's not "components", "helpers", "utils", or other standard
        // internal directory names, treat it as a subpath export.
        let standard_dirs = [
            "components",
            "helpers",
            "utils",
            "hooks",
            "types",
            "styles",
            "layouts",
            "lib",
        ];
        if !standard_dirs.contains(&first_segment)
            && !first_segment.contains('.')
            && first_segment != "index"
        {
            path.push('/');
            path.push_str(first_segment);
        }
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
/// Parse a structured value rename `before`/`after` field.
///
/// Format: `"Component.propName = value"`
/// Returns `(component, prop_name, value)`.
fn parse_value_rename_field(s: &str) -> Option<(&str, &str, &str)> {
    let (component_prop, value) = s.split_once(" = ")?;
    let (component, prop) = component_prop.rsplit_once('.')?;
    if component.is_empty() || prop.is_empty() || value.is_empty() {
        return None;
    }
    Some((component, prop, value))
}

pub fn build_frontend_condition(
    change: &ApiChange,
    leaf_symbol: &str,
    from_pkg: Option<&str>,
) -> KonveyorCondition {
    // Detect structured value rename entries (before: "Component.prop = value")
    // and build a condition matching the prop name with a value filter.
    if let Some(before) = change.before.as_deref() {
        if let Some((component, prop_name, value)) = parse_value_rename_field(before) {
            let from = from_pkg.map(|s| s.to_string());
            // Match both old and new prop name so the rule fires whether
            // the prop has been renamed already or not.
            let new_prop = change
                .after
                .as_deref()
                .and_then(parse_value_rename_field)
                .map(|(_, p, _)| p);

            let mut conditions = vec![KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: format!("^{}$", regex_escape(prop_name)),
                    location: "JSX_PROP".to_string(),
                    component: Some(format!("^{}$", regex_escape(component))),
                    parent: None,
                    value: Some(format!("^{}$", regex_escape(value))),
                    from: from.clone(),
                    file_pattern: None,
                    parent_from: None,
                    not_parent: None,
                    child: None,
                    not_child: None,
                    requires_child: None,
                },
            }];

            // Also match the new prop name if it differs
            if let Some(new_p) = new_prop {
                if new_p != prop_name {
                    conditions.push(KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: format!("^{}$", regex_escape(new_p)),
                            location: "JSX_PROP".to_string(),
                            component: Some(format!("^{}$", regex_escape(component))),
                            parent: None,
                            value: Some(format!("^{}$", regex_escape(value))),
                            from: from.clone(),
                            file_pattern: None,
                            parent_from: None,
                            not_parent: None,
                            child: None,
                            not_child: None,
                            requires_child: None,
                        },
                    });
                }
            }

            return if conditions.len() == 1 {
                conditions.into_iter().next().unwrap()
            } else {
                KonveyorCondition::Or { or: conditions }
            };
        }
    }

    let match_name = if change.change == ApiChangeType::Renamed {
        change
            .before
            .as_deref()
            .filter(|b| !looks_like_import_path(b))
            .map(extract_leaf_symbol)
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
        .is_some_and(|f| f.starts_with('^') && f.ends_with('$'));

    match change.kind {
        ApiChangeKind::Class | ApiChangeKind::Interface
            if change.change == ApiChangeType::Renamed =>
        {
            // Classes are JSX components; interfaces/types use TYPE_REFERENCE.
            let primary_location = if change.kind == ApiChangeKind::Interface {
                "TYPE_REFERENCE"
            } else {
                "JSX_COMPONENT"
            };

            let mut conditions = vec![KonveyorCondition::FrontendReferenced {
                referenced: FrontendReferencedFields {
                    pattern: pattern.clone(),
                    location: "IMPORT".to_string(),
                    component: None,
                    parent: None,
                    value: None,
                    from: from.clone(),
                    file_pattern: None,
                    parent_from: None,
                    not_parent: None,
                    child: None,
                    not_child: None,
                    requires_child: None,
                },
            }];
            if !is_subpath_scoped {
                conditions.insert(
                    0,
                    KonveyorCondition::FrontendReferenced {
                        referenced: FrontendReferencedFields {
                            pattern: pattern.clone(),
                            location: primary_location.to_string(),
                            component: None,
                            parent: None,
                            value: None,
                            from: from.clone(),
                            file_pattern: None,
                            parent_from: None,
                            not_parent: None,
                            child: None,
                            not_child: None,
                            requires_child: None,
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
                        file_pattern: None,
                        parent_from: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
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
                    file_pattern: None,
                    parent_from: None,
                    not_parent: None,
                    child: None,
                    not_child: None,
                    requires_child: None,
                },
            });
            if let Some(component_name) = match_name.strip_suffix("Props") {
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
                            file_pattern: None,
                            parent_from: None,
                            not_parent: None,
                            child: None,
                            not_child: None,
                            requires_child: None,
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
                        file_pattern: None,
                        parent_from: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
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
                        file_pattern: None,
                        parent_from: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
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
                not_parent: None,
                child: None,
                not_child: None,
                requires_child: None,
                value: None,
                from,
                file_pattern: None,
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
                        file_pattern: None,
                        parent_from: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
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
                    file_pattern: None,
                    parent_from: None,
                    not_parent: None,
                    child: None,
                    not_child: None,
                    requires_child: None,
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
                .is_some_and(|c| c.is_ascii_uppercase());
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
                                file_pattern: None,
                                parent_from: None,
                                not_parent: None,
                                child: None,
                                not_child: None,
                                requires_child: None,
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
                                file_pattern: None,
                                parent_from: None,
                                not_parent: None,
                                child: None,
                                not_child: None,
                                requires_child: None,
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
                        file_pattern: None,
                        parent_from: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
                    },
                }
            }
        }
    }
}

/// Extract a value filter from an ApiChange if it represents a single union member removal.
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

/// Compute the removed union member values between before and after type expressions.
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

/// Compute the added union member values between before and after type expressions.
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

/// Determine whether an API change is purely additive (non-breaking for consumers).
///
/// Additive changes introduce new capabilities without removing or modifying
/// existing ones. Existing consumer code continues to work without modification.
///
/// This function checks two categories:
///
/// 1. **TypeChanged** (union types): If the `before` union values are a subset of
///    the `after` union values (i.e., no values were removed, only added), the
///    change is additive. Example: `'primary' | 'secondary'` → `'primary' |
///    'secondary' | 'stateful'`.
///
/// 2. **SignatureChanged**: Property additions, readonly additions, and base class
///    widenings where no existing interface is removed. Detected by checking the
///    description string for known additive patterns.
///
/// Returns `true` if the change is additive and should be labeled
/// `change-scope=additive` so that analysis runs can filter these out.
///
/// NOTE: This function is intentionally called at rule creation time (not as a
/// post-processing filter) because the underlying `ApiChange` data -- including
/// `before`, `after`, and `description` -- is needed for the detection. Downstream
/// consumers like `extract_compound_tokens()`, `extract_suffix_inventory()`, and
/// `detect_css_prefix_changes()` still see the full set of `ApiChange` entries
/// because they read from `report.breaking_api_changes`, not from generated rules.
pub fn is_additive_change(change: &ApiChange) -> bool {
    match change.change {
        ApiChangeType::TypeChanged => {
            // For union type changes, check if all before values still exist in after.
            // If nothing was removed, the change only adds new options.
            let removed = extract_removed_union_values(change);
            let added = extract_added_union_values(change);

            // Only consider it additive if we can actually parse union values
            // from both sides and nothing was removed.
            if removed.is_empty() && !added.is_empty() {
                return true;
            }

            // TODO: Non-union type widening (e.g., `RefObject<HTMLUListElement>` →
            // `RefObject<HTMLUListElement | null>`) cannot be reliably detected from
            // string comparison alone. The report should carry structured type
            // information (e.g., a `TypeChange` enum with `Widened`/`Narrowed`/`Replaced`
            // variants) set by the diff engine which has the full AST context.
            // See: semver-analyzer/crates/core/src/diff/compare.rs

            false
        }
        ApiChangeType::SignatureChanged => {
            // Check the description for known additive patterns.
            let desc = change.description.to_lowercase();

            // "property X was added to Y" -- new optional property
            if desc.contains("was added to") || desc.contains("was added") {
                return true;
            }
            // "X was made readonly" -- tightening mutability is non-breaking for consumers
            if desc.contains("was made readonly") {
                return true;
            }
            // "base class changed from none to X" -- adding a base class
            if desc.contains("base class changed from none to") {
                return true;
            }

            false
        }
        // Removed, Renamed, VisibilityChanged are never additive
        _ => false,
    }
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

    // When a SignatureChanged prop still carries a ReplacedByMember
    // disposition (type-incompatible rename), include migration guidance.
    if change.change == ApiChangeType::SignatureChanged {
        if let Some(RemovalDisposition::ReplacedByMember { ref new_member }) =
            change.removal_disposition
        {
            msg.push_str(&format!(
                "\n\nMigration: This property was replaced by '{}'. \
                 The type has changed, so the value may need restructuring \
                 when moving to the new property.",
                new_member
            ));
        }
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
        ApiChangeKind::Enum => "enum",
        ApiChangeKind::Constructor => "constructor",
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

/// Extract the symbol name from a `symbol_summary` string.
///
/// `symbol_summary` format: `"{kind}: {name}"` or `"{kind}: {name}: {type}"`.
/// Returns just the `{name}` portion.  Falls back to the full string if the
/// format is not recognised.
pub fn extract_name_from_summary(summary: &str) -> &str {
    // Split on ": " to strip the kind prefix ("variable", "constant", etc.)
    if let Some(rest) = summary.split_once(": ").map(|(_, r)| r) {
        // If a type annotation follows (another ": "), take only the name part
        rest.split_once(": ").map(|(name, _)| name).unwrap_or(rest)
    } else {
        summary
    }
}

/// Returns `true` if `s` looks like an npm import path rather than a
/// `symbol_summary` string.  Import paths start with `@` or are simple
/// identifiers (no `": "` separator that symbol summaries always contain).
fn looks_like_import_path(s: &str) -> bool {
    // Reject symbol summaries like "variable: foo" immediately.
    if s.contains(": ") || s.is_empty() {
        return false;
    }
    // Scoped packages (@scope/name) or paths containing /
    if s.starts_with('@') || s.contains('/') {
        return true;
    }
    // Bare npm package names are all-lowercase with only [a-z0-9._-].
    // This rejects camelCase prop names like "deleteChip".
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
}

/// Extract the trailing PascalCase suffix from a snake_case token constant name.
pub fn extract_trailing_suffix(name: &str) -> Option<&str> {
    let last_underscore = name.rfind('_')?;
    let suffix = &name[last_underscore + 1..];
    if !suffix.is_empty()
        && suffix.chars().next().is_some_and(|c| c.is_uppercase())
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
            s.chars().next().is_some_and(|c| c.is_uppercase())
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── build_token_prefix_pattern tests ─────────────────────────────

    #[test]
    fn test_token_prefix_pattern_empty() {
        assert_eq!(build_token_prefix_pattern(&[]), ".*");
    }

    #[test]
    fn test_token_prefix_pattern_single_symbol() {
        let result = build_token_prefix_pattern(&["EmptyStateHeader"]);
        assert_eq!(result, "^(EmptyStateHeader)$");
    }

    #[test]
    fn test_token_prefix_pattern_multiple_symbols() {
        let result = build_token_prefix_pattern(&["Chip", "ChipGroup", "Text"]);
        // BTreeSet deduplicates and sorts
        assert_eq!(result, "^(Chip|ChipGroup|Text)$");
    }

    #[test]
    fn test_token_prefix_pattern_with_underscores() {
        // Symbols with underscores should be exact-matched, not prefix-collapsed
        let result =
            build_token_prefix_pattern(&["c_about_modal_box", "c_button", "global_font_size"]);
        assert_eq!(result, "^(c_about_modal_box|c_button|global_font_size)$");
    }

    #[test]
    fn test_token_prefix_pattern_many_symbols_no_wildcard() {
        // Even with >20 symbols, should NOT fall back to .*
        let symbols: Vec<String> = (0..30).map(|i| format!("Symbol{}", i)).collect();
        let refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
        let result = build_token_prefix_pattern(&refs);
        assert!(result.starts_with("^("));
        assert!(result.ends_with(")$"));
        assert!(!result.contains(".*"));
        // All 30 symbols should be present
        for sym in &symbols {
            assert!(result.contains(sym.as_str()), "Missing symbol: {}", sym);
        }
    }

    #[test]
    fn test_token_prefix_pattern_deduplication() {
        let result = build_token_prefix_pattern(&["Foo", "Bar", "Foo", "Bar", "Baz"]);
        assert_eq!(result, "^(Bar|Baz|Foo)$");
    }

    #[test]
    fn test_token_prefix_pattern_regex_special_chars() {
        // Symbols with regex-special characters should be escaped
        let result = build_token_prefix_pattern(&["Foo.Bar", "Baz(Qux)"]);
        assert!(result.contains(r"Foo\.Bar"));
        assert!(result.contains(r"Baz\(Qux\)"));
    }

    // ── build_common_prefix_pattern tests ────────────────────────────

    #[test]
    fn test_common_prefix_pattern_empty() {
        assert_eq!(build_common_prefix_pattern(&[]), ".*");
    }

    #[test]
    fn test_common_prefix_pattern_exact_alternation() {
        let result = build_common_prefix_pattern(&[
            "c_about_modal_box_brand_PaddingBottom",
            "global_font_size_100",
        ]);
        // Should be exact alternation, not prefix-based
        assert!(result.starts_with("^("));
        assert!(result.ends_with(")$"));
        assert!(result.contains("c_about_modal_box_brand_PaddingBottom"));
        assert!(result.contains("global_font_size_100"));
    }

    #[test]
    fn test_common_prefix_pattern_many_prefixes_no_fallback() {
        // Even with many unique prefixes, should NOT fall back to a broad CSS suffix pattern
        let symbols: Vec<String> = (0..10)
            .map(|i| format!("prefix{}_some_suffix", i))
            .collect();
        let refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
        let result = build_common_prefix_pattern(&refs);
        assert!(!result.contains("Color|BackgroundColor|FontSize"));
        assert!(result.starts_with("^("));
    }

    // ── is_additive_change tests ─────────────────────────────────────

    fn make_api_change(
        change: ApiChangeType,
        before: Option<&str>,
        after: Option<&str>,
        description: &str,
    ) -> ApiChange {
        ApiChange {
            symbol: "Test.prop".into(),
            qualified_name: String::new(),
            kind: ApiChangeKind::Property,
            change,
            before: before.map(|s| s.into()),
            after: after.map(|s| s.into()),
            description: description.into(),
            migration_target: None,
            removal_disposition: None,
        }
    }

    #[test]
    fn test_additive_union_type_change() {
        // Adding 'stateful' to ButtonVariant is additive
        let change = make_api_change(
            ApiChangeType::TypeChanged,
            Some("'primary' | 'secondary' | 'tertiary'"),
            Some("'primary' | 'secondary' | 'stateful' | 'tertiary'"),
            "variant type changed",
        );
        assert!(
            is_additive_change(&change),
            "Adding union members should be additive"
        );
    }

    #[test]
    fn test_subtractive_union_type_change() {
        // Removing 'tertiary' from ButtonVariant is NOT additive
        let change = make_api_change(
            ApiChangeType::TypeChanged,
            Some("'primary' | 'secondary' | 'tertiary'"),
            Some("'primary' | 'secondary'"),
            "variant type changed",
        );
        assert!(
            !is_additive_change(&change),
            "Removing union members should NOT be additive"
        );
    }

    #[test]
    fn test_mixed_union_type_change() {
        // Removing 'light-200' and adding 'secondary' is NOT additive
        let change = make_api_change(
            ApiChangeType::TypeChanged,
            Some("'default' | 'light-200' | 'no-background'"),
            Some("'default' | 'no-background' | 'secondary'"),
            "colorVariant type changed",
        );
        assert!(
            !is_additive_change(&change),
            "Mixed add+remove should NOT be additive"
        );
    }

    #[test]
    fn test_non_union_type_widening_not_yet_detected() {
        // RefObject<HTMLUListElement> → RefObject<HTMLUListElement | null> is additive
        // but we can't reliably detect non-union type widening from strings alone.
        // TODO: The report should carry structured type change info from the diff
        // engine (e.g., TypeChange::Widened) to enable this detection.
        let change = make_api_change(
            ApiChangeType::TypeChanged,
            Some("RefObject<HTMLUListElement>"),
            Some("RefObject<HTMLUListElement | null>"),
            "innerRef type changed",
        );
        assert!(
            !is_additive_change(&change),
            "Non-union type widening is not yet detected as additive (needs structured type info in report)"
        );
    }

    #[test]
    fn test_signature_property_added() {
        let change = make_api_change(
            ApiChangeType::SignatureChanged,
            None,
            None,
            "property `isFullHeight` was added to `CodeEditorProps`",
        );
        assert!(
            is_additive_change(&change),
            "Property addition should be additive"
        );
    }

    #[test]
    fn test_signature_made_readonly() {
        let change = make_api_change(
            ApiChangeType::SignatureChanged,
            Some("mutable"),
            Some("readonly"),
            "`AlertGroup` was made readonly",
        );
        assert!(
            is_additive_change(&change),
            "Making readonly should be additive"
        );
    }

    #[test]
    fn test_signature_base_class_added() {
        let change = make_api_change(
            ApiChangeType::SignatureChanged,
            None,
            None,
            "`AccordionItemProps` base class changed from none to React.HTMLProps",
        );
        assert!(
            is_additive_change(&change),
            "Adding base class from none should be additive"
        );
    }

    #[test]
    fn test_signature_base_class_changed_not_additive() {
        // Changing from one base class to another is NOT necessarily additive
        let change = make_api_change(
            ApiChangeType::SignatureChanged,
            None,
            None,
            "`ChipProps` base class changed from React.HTMLProps to LabelProps",
        );
        assert!(
            !is_additive_change(&change),
            "Changing base class should NOT be additive"
        );
    }

    #[test]
    fn test_removed_is_never_additive() {
        let change = make_api_change(
            ApiChangeType::Removed,
            Some("'primary' | 'secondary'"),
            None,
            "variant was removed",
        );
        assert!(
            !is_additive_change(&change),
            "Removed should never be additive"
        );
    }

    #[test]
    fn test_renamed_is_never_additive() {
        let change = make_api_change(
            ApiChangeType::Renamed,
            None,
            None,
            "isOpen was renamed to isExpanded",
        );
        assert!(
            !is_additive_change(&change),
            "Renamed should never be additive"
        );
    }

    // ── merge_duplicate_conditions tests ─────────────────────────────

    #[test]
    fn test_merge_duplicate_conditions_no_dupes() {
        let rules = vec![
            KonveyorRule {
                rule_id: "rule-1".into(),
                labels: vec!["change-type=removed".into()],
                effort: 1,
                category: "mandatory".into(),
                description: "Prop removed".into(),
                message: "variant removed from Button".into(),
                links: vec![],
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: "^variant$".into(),
                        location: "JSX_PROP".into(),
                        component: Some("^Button$".into()),
                        parent: None,
                        value: None,
                        from: Some("@patternfly/react-core".into()),
                        parent_from: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
                        file_pattern: None,
                    },
                },
                fix_strategy: None,
            },
            KonveyorRule {
                rule_id: "rule-2".into(),
                labels: vec!["change-type=removed".into()],
                effort: 1,
                category: "mandatory".into(),
                description: "Prop removed".into(),
                message: "variant removed from Banner".into(),
                links: vec![],
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: "^variant$".into(),
                        location: "JSX_PROP".into(),
                        component: Some("^Banner$".into()),
                        parent: None,
                        value: None,
                        from: Some("@patternfly/react-core".into()),
                        parent_from: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
                        file_pattern: None,
                    },
                },
                fix_strategy: None,
            },
        ];
        let result = merge_duplicate_conditions(rules);
        assert_eq!(result.len(), 2, "Different conditions should not merge");
    }

    #[test]
    fn test_merge_duplicate_conditions_with_dupes() {
        let rules = vec![
            KonveyorRule {
                rule_id: "rule-dts".into(),
                labels: vec!["change-type=removed".into()],
                effort: 1,
                category: "mandatory".into(),
                description: "From .d.ts".into(),
                message: "variant removed (d.ts)".into(),
                links: vec![],
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: "^variant$".into(),
                        location: "JSX_PROP".into(),
                        component: Some("^Button$".into()),
                        parent: None,
                        value: None,
                        from: Some("@patternfly/react-core".into()),
                        parent_from: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
                        file_pattern: None,
                    },
                },
                fix_strategy: None,
            },
            KonveyorRule {
                rule_id: "rule-tsx".into(),
                labels: vec!["change-type=type-changed".into()],
                effort: 1,
                category: "mandatory".into(),
                description: "From .tsx".into(),
                message: "variant type changed (tsx)".into(),
                links: vec![],
                when: KonveyorCondition::FrontendReferenced {
                    referenced: FrontendReferencedFields {
                        pattern: "^variant$".into(),
                        location: "JSX_PROP".into(),
                        component: Some("^Button$".into()),
                        parent: None,
                        value: None,
                        from: Some("@patternfly/react-core".into()),
                        parent_from: None,
                        not_parent: None,
                        child: None,
                        not_child: None,
                        requires_child: None,
                        file_pattern: None,
                    },
                },
                fix_strategy: None,
            },
        ];
        let result = merge_duplicate_conditions(rules);
        assert_eq!(result.len(), 1, "Same condition should merge into one rule");
    }

    // ── extract_name_from_summary tests ──────────────────────────────

    #[test]
    fn test_extract_name_from_summary_with_type() {
        let summary = r##"constant: global_success_color_100: { ["name"]: "--pf-v5-global--success-color--100"; ["value"]: "#3e8635" }"##;
        assert_eq!(
            extract_name_from_summary(summary),
            "global_success_color_100"
        );
    }

    #[test]
    fn test_extract_name_from_summary_without_type() {
        assert_eq!(
            extract_name_from_summary("variable: t_global_color_status_success_100"),
            "t_global_color_status_success_100"
        );
    }

    #[test]
    fn test_extract_name_from_summary_plain_name() {
        // No kind prefix — returns as-is
        assert_eq!(
            extract_name_from_summary("global_success_color_100"),
            "global_success_color_100"
        );
    }

    // ── looks_like_import_path tests ─────────────────────────────────

    #[test]
    fn test_looks_like_import_path_scoped_package() {
        assert!(looks_like_import_path("@patternfly/react-core"));
        assert!(looks_like_import_path("@patternfly/react-charts/victory"));
    }

    #[test]
    fn test_looks_like_import_path_simple_package() {
        assert!(looks_like_import_path("react"));
        assert!(looks_like_import_path("lodash/merge"));
    }

    #[test]
    fn test_looks_like_import_path_rejects_symbol_summary() {
        assert!(!looks_like_import_path(
            "variable: global_success_color_100"
        ));
        assert!(!looks_like_import_path(
            r##"constant: foo: { ["name"]: "bar" }"##
        ));
    }

    // ── api_change_to_strategy for token renames ─────────────────────

    #[test]
    fn test_token_rename_generates_rename_strategy() {
        let change = ApiChange {
            symbol: "global_success_color_100".into(),
            qualified_name: String::new(),
            kind: ApiChangeKind::Constant,
            change: ApiChangeType::Renamed,
            before: Some(
                r##"constant: global_success_color_100: { ["name"]: "--pf-v5-global--success-color--100"; ["value"]: "#3e8635"; ["var"]: "var(--pf-v5-global--success-color--100)" }"##
                    .into(),
            ),
            after: Some("variable: t_global_color_status_success_100".into()),
            description: "Exported constant `global_success_color_100` was renamed to `t_global_color_status_success_100`".into(),
            migration_target: None,
            removal_disposition: None,
        };
        let patterns = RenamePatterns::empty();
        let member_renames = HashMap::new();
        let strategy =
            api_change_to_strategy(&change, &patterns, &member_renames, "some/file.d.ts");
        let s = strategy.expect("should produce a strategy");
        assert_eq!(s.strategy, "Rename");
        assert_eq!(s.from.as_deref(), Some("global_success_color_100"));
        assert_eq!(s.to.as_deref(), Some("t_global_color_status_success_100"));
    }

    #[test]
    fn test_token_rename_no_type_annotation() {
        // Some token renames have no type in the after field
        let change = ApiChange {
            symbol: "global_warning_color_100".into(),
            qualified_name: String::new(),
            kind: ApiChangeKind::Constant,
            change: ApiChangeType::Renamed,
            before: Some("variable: global_warning_color_100".into()),
            after: Some("variable: t_chart_global_warning_color_100".into()),
            description: "renamed".into(),
            migration_target: None,
            removal_disposition: None,
        };
        let patterns = RenamePatterns::empty();
        let member_renames = HashMap::new();
        let strategy =
            api_change_to_strategy(&change, &patterns, &member_renames, "some/file.d.ts");
        let s = strategy.expect("should produce a strategy");
        assert_eq!(s.strategy, "Rename");
        assert_eq!(s.from.as_deref(), Some("global_warning_color_100"));
        assert_eq!(s.to.as_deref(), Some("t_chart_global_warning_color_100"));
    }

    // ── token_mappings override tests ────────────────────────────────

    #[test]
    fn test_token_mapping_overrides_algorithmic_rename() {
        // The algorithm would produce t_global_color_status_danger_100 from the
        // after field, but the user-provided token_mapping should win.
        let change = ApiChange {
            symbol: "global_danger_color_100".into(),
            qualified_name: String::new(),
            kind: ApiChangeKind::Constant,
            change: ApiChangeType::Renamed,
            before: Some("constant: global_danger_color_100".into()),
            after: Some("variable: t_global_color_status_danger_100".into()),
            description: "renamed".into(),
            migration_target: None,
            removal_disposition: None,
        };

        let mut patterns = RenamePatterns::empty();
        patterns.token_mappings.insert(
            "global_danger_color_100".into(),
            "chart_global_danger_Color_100".into(),
        );
        let member_renames = HashMap::new();
        let strategy =
            api_change_to_strategy(&change, &patterns, &member_renames, "some/file.d.ts");
        let s = strategy.expect("should produce a strategy");
        assert_eq!(s.strategy, "Rename");
        assert_eq!(s.from.as_deref(), Some("global_danger_color_100"));
        // User mapping wins over algorithmic extract_name_from_summary
        assert_eq!(s.to.as_deref(), Some("chart_global_danger_Color_100"));
    }

    #[test]
    fn test_token_mapping_not_present_falls_through_to_algorithm() {
        // When no user mapping exists, the algorithm's result is used
        let change = ApiChange {
            symbol: "global_spacer_sm".into(),
            qualified_name: String::new(),
            kind: ApiChangeKind::Constant,
            change: ApiChangeType::Renamed,
            before: Some("constant: global_spacer_sm".into()),
            after: Some("variable: t_global_spacer_sm".into()),
            description: "renamed".into(),
            migration_target: None,
            removal_disposition: None,
        };

        // Patterns with some mappings, but not for this symbol
        let mut patterns = RenamePatterns::empty();
        patterns.token_mappings.insert(
            "global_danger_color_100".into(),
            "chart_global_danger_Color_100".into(),
        );
        let member_renames = HashMap::new();
        let strategy =
            api_change_to_strategy(&change, &patterns, &member_renames, "some/file.d.ts");
        let s = strategy.expect("should produce a strategy");
        assert_eq!(s.strategy, "Rename");
        assert_eq!(s.from.as_deref(), Some("global_spacer_sm"));
        // Falls through to extract_name_from_summary
        assert_eq!(s.to.as_deref(), Some("t_global_spacer_sm"));
    }

    #[test]
    fn test_token_mapping_yaml_deserialization() {
        let yaml = r#"
rename_patterns: []
token_mappings:
  global_success_color_100: t_global_color_status_success_100
  global_danger_color_100: chart_global_danger_Color_100
  global_Color_dark_100: t_global_icon_color_regular
"#;
        let file: RenamePatternsFile = serde_yaml::from_str(yaml).expect("should parse YAML");
        assert_eq!(file.token_mappings.len(), 3);
        assert_eq!(
            file.token_mappings.get("global_success_color_100").unwrap(),
            "t_global_color_status_success_100"
        );
        assert_eq!(
            file.token_mappings.get("global_danger_color_100").unwrap(),
            "chart_global_danger_Color_100"
        );
        assert_eq!(
            file.token_mappings.get("global_Color_dark_100").unwrap(),
            "t_global_icon_color_regular"
        );
    }

    #[test]
    fn test_token_mapping_empty_when_not_in_yaml() {
        // Existing YAML without token_mappings should still work
        let yaml = r#"
rename_patterns:
  - match: "^foo$"
    replace: "bar"
"#;
        let file: RenamePatternsFile = serde_yaml::from_str(yaml).expect("should parse YAML");
        assert!(file.token_mappings.is_empty());
        assert_eq!(file.rename_patterns.len(), 1);
    }

    #[test]
    fn test_get_token_mapping() {
        let mut patterns = RenamePatterns::empty();
        patterns.token_mappings.insert(
            "global_Color_dark_100".into(),
            "t_global_icon_color_regular".into(),
        );

        assert_eq!(
            patterns.get_token_mapping("global_Color_dark_100"),
            Some("t_global_icon_color_regular")
        );
        assert_eq!(patterns.get_token_mapping("nonexistent_token"), None);
    }
}
