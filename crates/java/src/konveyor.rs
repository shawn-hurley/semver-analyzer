//! Java Konveyor rule generator.
//!
//! Converts an `AnalysisReport<Java>` into Konveyor YAML rules using
//! `java.referenced` conditions for AST-level matching.
//!
//! Two generators:
//! - `generate_rules()` — TD rules from structural API diff
//! - `generate_sd_rules()` — SD rules from source-level behavioral analysis

use crate::language::Java;
use crate::sd_types::{JavaSdPipelineResult, JavaSourceCategory, JavaSourceChange, MigrationMapping};
use semver_analyzer_core::AnalysisReport;
use semver_analyzer_konveyor_core::{
    FixStrategyEntry, JavaDependencyFields, JavaReferencedFields, KonveyorCondition, KonveyorLink,
    KonveyorRule, KonveyorRuleset, MemberMappingEntry,
};
use std::collections::HashMap;

// ── Configuration ───────────────────────────────────────────────────────

/// Configuration for Java Konveyor rule generation.
///
/// Parameterizes the rule generator for different Java projects
/// (Spring Boot, Quarkus, Jakarta EE, etc.).
#[derive(Debug, Clone)]
pub struct JavaKonveyorConfig {
    /// Project name (e.g., "spring-boot"). Used in ruleset metadata.
    pub project_name: String,
    /// Rule ID prefix (e.g., "sb4"). Used in rule IDs.
    pub rule_id_prefix: String,
    /// Migration guide URL (optional).
    pub migration_guide_url: Option<String>,
    /// Migration guide title (optional).
    pub migration_guide_title: Option<String>,
}

impl Default for JavaKonveyorConfig {
    fn default() -> Self {
        Self {
            project_name: "java-library".into(),
            rule_id_prefix: "java".into(),
            migration_guide_url: None,
            migration_guide_title: None,
        }
    }
}

impl JavaKonveyorConfig {
    /// Create a config from CLI args.
    pub fn from_args(
        project_name: Option<&str>,
        rule_prefix: Option<&str>,
        migration_guide_url: Option<&str>,
    ) -> Self {
        let project = project_name.unwrap_or("java-library");
        let prefix = rule_prefix.unwrap_or_else(|| {
            // Derive prefix from project name: "spring-boot" → "sb"
            project
                .split('-')
                .filter_map(|w| w.chars().next())
                .collect::<String>()
                .as_str()
                .to_string()
                .leak() // Safe: called once per CLI invocation
        });
        Self {
            project_name: project.to_string(),
            rule_id_prefix: prefix.to_string(),
            migration_guide_url: migration_guide_url.map(|s| s.to_string()),
            migration_guide_title: migration_guide_url
                .map(|_| format!("{} Migration Guide", project)),
        }
    }
}

// ── Ruleset ─────────────────────────────────────────────────────────────

/// Generate a ruleset metadata file.
pub fn ruleset(from: &str, to: &str) -> KonveyorRuleset {
    ruleset_with_config(from, to, &JavaKonveyorConfig::default())
}

/// Generate a ruleset with custom config.
pub fn ruleset_with_config(from: &str, to: &str, config: &JavaKonveyorConfig) -> KonveyorRuleset {
    KonveyorRuleset {
        name: format!("{}-{}-to-{}", config.project_name, from, to),
        description: format!(
            "Auto-generated migration rules for {} {} to {}",
            config.project_name, from, to
        ),
        labels: vec!["source=semver-analyzer".into(), "language=java".into()],
    }
}

// ── TD rule generation ──────────────────────────────────────────────────

/// Generate TD rules from a Java analysis report.
pub fn generate_rules(report: &AnalysisReport<Java>) -> Vec<KonveyorRule> {
    generate_rules_with_config(report, &JavaKonveyorConfig::default())
}

/// Generate TD rules with custom config.
pub fn generate_rules_with_config(
    report: &AnalysisReport<Java>,
    config: &JavaKonveyorConfig,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();
    let mut id_counts: HashMap<String, usize> = HashMap::new();

    let mut relocations: Vec<(&str, &str, &str)> = Vec::new();

    for fc in &report.changes {
        for ac in &fc.breaking_api_changes {
            match ac.change {
                semver_analyzer_core::ApiChangeType::Renamed => {
                    if let (Some(before), Some(after)) = (&ac.before, &ac.after) {
                        let before_class = before.rsplit('.').next().unwrap_or(before);
                        let after_class = after.rsplit('.').next().unwrap_or(after);
                        if before_class == after_class && before != after {
                            // Package relocation -- verify packages are related
                            // to avoid false matches across unrelated subsystems
                            // (e.g., criterion.Property → spatial.sqlserver.Property)
                            if packages_are_related(before, after) {
                                relocations.push((&ac.symbol, before, after));
                            }
                        } else if before_class != after_class {
                            rules.push(make_rename_rule(
                                &ac.symbol,
                                before,
                                after,
                                &ac.description,
                                config,
                                &mut id_counts,
                            ));
                        }
                    }
                }
                semver_analyzer_core::ApiChangeType::Removed => {
                    if let Some(ref mt) = ac.migration_target {
                        // Reject migration targets with incompatible base types
                        // or unrelated packages -- treat as plain removal instead.
                        //
                        // Exception: zero-overlap targets from name-prefix fallback
                        // (e.g., MySQL5InnoDBDialect → MySQLDialect) are allowed
                        // despite base type differences, since the relationship is
                        // established by naming convention in a type consolidation
                        // pattern (many versioned subclasses → one base class).
                        let is_name_prefix_match = mt.overlap_ratio == 0.0
                            && mt.matching_members.is_empty();
                        let is_valid_target = packages_are_related(
                            &mt.removed_qualified_name,
                            &mt.replacement_qualified_name,
                        ) && (is_name_prefix_match || !has_incompatible_base_type(
                            mt.old_extends.as_deref(),
                            mt.new_extends.as_deref(),
                        ));

                        if !is_valid_target {
                            if !is_type_parameter_pattern(&mt.removed_qualified_name) {
                                rules.push(make_removal_rule(
                                    &ac.symbol,
                                    &mt.removed_qualified_name,
                                    &ac.description,
                                    config,
                                    &mut id_counts,
                                ));
                            }
                            continue;
                        }
                        rules.push(make_removal_with_target_rule(
                            &ac.symbol,
                            &mt.removed_qualified_name,
                            &mt.replacement_symbol,
                            &mt.replacement_qualified_name,
                            &ac.description,
                            config,
                            &mut id_counts,
                        ));
                    } else {
                        // Use the qualified_name for the scanner pattern, NOT the
                        // `before` field. `before` contains a descriptive format
                        // like "class: Restrictions" which doesn't match the scanner's
                        // regex-based FQN/simple-name matching. `qualified_name` has
                        // the proper FQN (e.g., "org.hibernate.criterion.Restrictions").
                        let qname = if ac.qualified_name.is_empty() {
                            ac.before.as_deref().unwrap_or(&ac.symbol)
                        } else {
                            &ac.qualified_name
                        };

                        // Skip type parameter removals -- patterns like "T", "E", "R"
                        // are too short and match nearly every Java file. Type parameter
                        // changes on methods are typically non-breaking for callers.
                        if is_type_parameter_pattern(qname) {
                            continue;
                        }

                        rules.push(make_removal_rule(
                            &ac.symbol,
                            qname,
                            &ac.description,
                            config,
                            &mut id_counts,
                        ));
                    }
                }
                semver_analyzer_core::ApiChangeType::TypeChanged => {
                    if let (Some(before), Some(after)) = (&ac.before, &ac.after) {
                        rules.push(make_type_changed_rule(
                            &ac.symbol,
                            &ac.qualified_name,
                            before,
                            after,
                            &ac.description,
                            config,
                            &mut id_counts,
                        ));
                    }
                }
                semver_analyzer_core::ApiChangeType::SignatureChanged => {
                    if let (Some(before), Some(after)) = (&ac.before, &ac.after) {
                        rules.push(make_signature_changed_rule(
                            &ac.symbol,
                            &ac.qualified_name,
                            before,
                            after,
                            &ac.description,
                            config,
                            &mut id_counts,
                        ));
                    }
                }
                semver_analyzer_core::ApiChangeType::VisibilityChanged => {
                    if let (Some(before), Some(after)) = (&ac.before, &ac.after) {
                        rules.push(make_visibility_changed_rule(
                            &ac.symbol,
                            &ac.qualified_name,
                            before,
                            after,
                            &ac.description,
                            config,
                            &mut id_counts,
                        ));
                    }
                }
            }
        }
    }

    for &(name, old_qname, new_qname) in &relocations {
        rules.push(make_import_relocation_rule(
            name,
            old_qname,
            new_qname,
            config,
            &mut id_counts,
        ));
    }

    for mc in &report.manifest_changes {
        if mc.is_breaking {
            if let Some(ref before) = mc.before {
                rules.push(make_dependency_rule(
                    &mc.field,
                    before,
                    mc.after.as_deref(),
                    &mc.description,
                    config,
                    &mut id_counts,
                ));
            }
        }
    }

    // Post-processing: consolidate "interface became generic" patterns.
    // When an interface gains a type parameter (e.g., UserType → UserType<J>),
    // the diff emits N individual type-changed rules for each method where
    // Object→J (the type param name). These are misleading individually — the
    // fix isn't to replace Object with "J" literally but to bind the type parameter.
    // Consolidate them into a single LlmAssisted rule.
    consolidate_generic_interface_rules(&mut rules, report, config, &mut id_counts);

    rules
}

/// Consolidate per-method `Object→<type_param>` type-changed rules into a single
/// composite "interface became generic" rule when the pattern is detected.
///
/// Detection: multiple type-changed rules for methods of the same interface where
/// `before` = a common type (Object, Serializable) and `after` = a single uppercase
/// letter or short name (J, T, E) that matches a type parameter.
fn consolidate_generic_interface_rules(
    rules: &mut Vec<KonveyorRule>,
    report: &AnalysisReport<Java>,
    config: &JavaKonveyorConfig,
    id_counts: &mut HashMap<String, usize>,
) {
    // Group type-changed rules by declaring interface
    let mut interface_rules: HashMap<String, Vec<usize>> = HashMap::new();
    let mut type_param_names: HashMap<String, String> = HashMap::new();

    for (idx, rule) in rules.iter().enumerate() {
        if !rule.labels.iter().any(|l| l == "change-type=type-changed") {
            continue;
        }
        // Extract declaring class from the rule's fix strategy
        if let Some(ref strategy) = rule.fix_strategy {
            if let (Some(ref from), Some(ref to)) = (&strategy.from, &strategy.to) {
                // Pattern: from is a common base type, to is a short type parameter name
                let to_trimmed = to.trim();
                let is_type_param = to_trimmed.len() <= 3
                    && to_trimmed.chars().next().is_some_and(|c| c.is_uppercase());

                if is_type_param
                    && (from == "Object"
                        || from == "Serializable"
                        || from == "Comparable")
                {
                    // Extract the declaring interface from the rule description
                    // by looking at the original API changes
                    for ac in report.changes.iter().flat_map(|fc| &fc.breaking_api_changes) {
                        if let Some(rule_qn) = extract_qualified_name_from_rule(rule) {
                            if ac.qualified_name == rule_qn {
                                let (class_opt, _) =
                                    extract_class_and_member(&ac.qualified_name);
                                if let Some(class) = class_opt {
                                    interface_rules
                                        .entry(class.clone())
                                        .or_default()
                                        .push(idx);
                                    type_param_names
                                        .entry(class)
                                        .or_insert_with(|| to_trimmed.to_string());
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    // For interfaces with 3+ consolidated type-changed rules, replace with a single composite rule
    let mut indices_to_remove = Vec::new();

    for (interface_name, rule_indices) in &interface_rules {
        if rule_indices.len() < 3 {
            continue; // Not enough to be a generic interface pattern
        }

        let type_param = type_param_names
            .get(interface_name)
            .cloned()
            .unwrap_or_else(|| "T".to_string());

        // Collect affected method names
        let mut affected_methods = Vec::new();
        for &idx in rule_indices {
            let method_part = rules[idx]
                .description
                .strip_prefix("Type of `")
                .and_then(|s| s.split('`').next())
                .unwrap_or("unknown");
            affected_methods.push(method_part.to_string());
        }

        // Find the interface FQN from the API changes
        let interface_fqn = report
            .changes
            .iter()
            .flat_map(|fc| &fc.breaking_api_changes)
            .find(|ac| {
                let (class_opt, _) = extract_class_and_member(&ac.qualified_name);
                class_opt.as_deref() == Some(interface_name.as_str())
            })
            .map(|ac| {
                ac.qualified_name
                    .rsplit_once('.')
                    .map(|(pkg, _)| pkg.to_string())
                    .unwrap_or_else(|| interface_name.clone())
            })
            .unwrap_or_else(|| interface_name.clone());

        let rule_id = unique_id(
            &format!(
                "{}-generic-interface-{}",
                config.rule_id_prefix,
                slugify(interface_name)
            ),
            id_counts,
        );

        let composite_rule = KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".into(),
                "change-type=generic-interface".into(),
                "language=java".into(),
            ],
            effort: 5,
            category: "mandatory".into(),
            description: format!(
                "Interface `{}` is now generic `{}<{}>`",
                interface_name, interface_name, type_param
            ),
            message: format!(
                "Interface `{}` gained type parameter `<{}>`. All implementing classes must:\n\n\
                 1. Update their `implements` clause: `implements {}<YourConcreteType>` \
                 (e.g., `implements {}<String>`)\n\
                 2. Replace `Object` with your concrete type in all overridden method \
                 signatures ({} methods affected: {})\n\
                 3. Remove unnecessary casts in method bodies (parameters are now typed)\n\n\
                 The concrete type should match your `returnedClass()` return value.\n\n\
                 Do NOT use `{}` as a literal type — it is a type parameter that must be \
                 bound to a concrete type in each implementing class.",
                interface_name,
                type_param,
                interface_name,
                interface_name,
                affected_methods.len(),
                affected_methods.join(", "),
                type_param,
            ),
            links: vec![],
            when: KonveyorCondition::JavaReferenced {
                referenced: JavaReferencedFields {
                    pattern: regex_escape(&interface_fqn),
                    scope: Some("IMPORT".into()),
                    ..Default::default()
                },
            },
            fix_strategy: Some(FixStrategyEntry {
                strategy: "LlmAssisted".into(),
                from: Some(format!("implements {}", interface_name)),
                to: Some(format!("implements {}<ConcreteType>", interface_name)),
                replacement: Some(format!(
                    "Interface `{}` is now generic. Add the type parameter to \
                     your `implements` clause and replace `Object` with your \
                     concrete type in all {} overridden methods. The concrete type \
                     should match what `returnedClass()` returns. Do NOT use `{}` \
                     as a literal type name.",
                    interface_name,
                    affected_methods.len(),
                    type_param,
                )),
                ..Default::default()
            }),
        };

        // Mark individual rules for removal and add composite
        indices_to_remove.extend(rule_indices.iter().copied());
        rules.push(composite_rule);
    }

    // Remove the individual rules (in reverse order to preserve indices)
    indices_to_remove.sort_unstable();
    indices_to_remove.dedup();
    for idx in indices_to_remove.into_iter().rev() {
        rules.remove(idx);
    }
}

/// Try to extract the qualified name from a rule's scanner condition.
fn extract_qualified_name_from_rule(rule: &KonveyorRule) -> Option<String> {
    match &rule.when {
        KonveyorCondition::JavaReferenced { referenced } => {
            // Unescape the regex pattern to get the original FQN
            Some(referenced.pattern.replace(r"\.", "."))
        }
        KonveyorCondition::Or { or } => {
            // Find the first condition with a full FQN pattern (contains dots)
            for cond in or {
                if let KonveyorCondition::JavaReferenced { referenced } = cond {
                    let pattern = referenced.pattern.replace(r"\.", ".");
                    if pattern.contains('.') {
                        return Some(pattern);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

// ── SD rule generation ──────────────────────────────────────────────────

/// Generate SD rules from source-level diff results.
pub fn generate_sd_rules(
    sd: &JavaSdPipelineResult,
    config: &JavaKonveyorConfig,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();
    let mut id_counts: HashMap<String, usize> = HashMap::new();

    // Generate rules from source-level changes
    for change in &sd.source_level_changes {
        if !change.is_breaking {
            continue;
        }
        if let Some(rule) = make_sd_rule(change, config, &mut id_counts) {
            rules.push(rule);
        }
    }

    // Generate rules from module changes
    for change in &sd.module_changes {
        if !change.is_breaking {
            continue;
        }
        if let Some(rule) = make_sd_rule(change, config, &mut id_counts) {
            rules.push(rule);
        }
    }

    rules
}

// ── Migration mapping enrichment ────────────────────────────────────────

/// Enrich removal rules with migration context from mined migration examples.
///
/// For each removal rule (rules with `change-type=removed` and no fix strategy),
/// checks if a migration mapping exists that maps the removed class to a
/// replacement. If found, attaches an `LlmAssisted` fix strategy with:
/// - `from`/`to`: old → new FQN
/// - `member_mappings`: method-level old → new name pairs
/// - `replacement`: the new class name
/// - Context string with mapping table and representative code examples
pub fn enrich_rules_with_migration_mappings(
    rules: &mut [KonveyorRule],
    mappings: &[MigrationMapping],
) -> usize {
    if mappings.is_empty() {
        return 0;
    }

    // Build lookup: old_class_simple_name → best mapping (most examples).
    // Mappings are pre-sorted by example_count descending, so use
    // entry().or_insert() to keep the first (strongest) match.
    let mut by_name: HashMap<&str, &MigrationMapping> = HashMap::new();
    let mut by_fqn: HashMap<&str, &MigrationMapping> = HashMap::new();
    for m in mappings {
        by_name.entry(m.old_class.as_str()).or_insert(m);
        by_fqn.entry(m.old_fqn.as_str()).or_insert(m);
    }

    let mut enriched = 0;

    for rule in rules.iter_mut() {
        // Only enrich removal rules that lack a fix strategy
        if rule.fix_strategy.is_some() {
            continue;
        }
        if !rule
            .labels
            .iter()
            .any(|l| l == "change-type=removed")
        {
            continue;
        }

        // Extract the removed class name from the rule's when condition
        let pattern = match &rule.when {
            KonveyorCondition::JavaReferenced { referenced } => &referenced.pattern,
            _ => continue,
        };

        // Try to find a mapping by FQN match first, then by simple name.
        // Patterns can be:
        //   - FQN: "org.hibernate.criterion.Restrictions"
        //   - Kind-prefixed: "class: Restrictions" or "interface: Criterion"
        let mapping = by_fqn
            .get(pattern.as_str())
            .or_else(|| {
                // Extract simple name: strip "class: ", "interface: ", or take last dotted segment
                let simple = if let Some(after_colon) = pattern.split(": ").nth(1) {
                    after_colon.trim()
                } else {
                    pattern.rsplit('.').next().unwrap_or(pattern)
                };
                by_name.get(simple)
            });

        let mapping = match mapping {
            Some(m) => *m,
            None => continue,
        };

        // Build context string for the LLM
        let context = format_migration_context(mapping);

        // Build member mappings
        let member_mappings: Vec<MemberMappingEntry> = mapping
            .method_mappings
            .iter()
            .map(|mm| MemberMappingEntry {
                old_name: mm.old_method.clone(),
                new_name: mm.new_method.clone(),
            })
            .collect();

        // Update the rule
        rule.fix_strategy = Some(FixStrategyEntry {
            strategy: "LlmAssisted".into(),
            from: Some(mapping.old_fqn.clone()),
            to: Some(mapping.new_fqn.clone()),
            replacement: Some(mapping.new_class.clone()),
            member_mappings,
            removed_members: Vec::new(),
            overlap_ratio: Some(mapping.example_count as f64),
            ..Default::default()
        });

        // Update the rule message to include migration guidance
        rule.message = format!(
            "{}\n\n{}", rule.message.trim_end_matches("\n\nThis class has been removed with no direct replacement."), context
        );
        rule.effort = 7; // Higher effort since it's a paradigm shift

        enriched += 1;
    }

    enriched
}

/// Format migration mapping into a human-readable context string for the LLM.
fn format_migration_context(mapping: &MigrationMapping) -> String {
    let mut ctx = String::new();

    ctx.push_str(&format!(
        "Migration: `{}` → `{}`\n",
        mapping.old_fqn, mapping.new_fqn,
    ));

    if !mapping.method_mappings.is_empty() {
        ctx.push_str("\nMethod mappings (old → new):\n");
        for mm in &mapping.method_mappings {
            ctx.push_str(&format!(
                "  {}.{}() → {}.{}()",
                mapping.old_class, mm.old_method, mapping.new_class, mm.new_method
            ));
            if mm.confidence > 1 {
                ctx.push_str(&format!("  [{} examples]", mm.confidence));
            }
            ctx.push('\n');
        }
    }

    // Add representative code examples (max 2)
    let examples_to_show = mapping.pattern_examples.len().min(2);
    if examples_to_show > 0 {
        ctx.push_str("\nMigration examples from library tests:\n");
        for ex in mapping.pattern_examples.iter().take(examples_to_show) {
            ctx.push_str("\n  Before (old API):\n");
            for line in ex.old_code.lines() {
                ctx.push_str(&format!("    {}\n", line.trim()));
            }
            ctx.push_str("  After (new API):\n");
            for line in ex.new_code.lines() {
                ctx.push_str(&format!("    {}\n", line.trim()));
            }
        }
    }

    // Add common pitfalls for Criteria API migration
    if mapping.old_class == "Restrictions"
        || mapping.old_class == "DetachedCriteria"
        || mapping.old_class == "Criteria"
        || mapping.old_class == "Order"
        || mapping.old_class == "Projections"
    {
        ctx.push_str("\n## IMPORTANT: Common Pitfalls\n\n");
        ctx.push_str(
            "1. CriteriaQuery vs TypedQuery: These are DIFFERENT types.\n\
             - CriteriaQuery<T> = the query definition (from criteriaBuilder.createQuery())\n\
             - TypedQuery<T> = the executable query (from entityManager.createQuery(criteriaQuery))\n\
             Do NOT convert CriteriaQuery to TypedQuery before passing to factory/wrapper methods \
             that expect CriteriaQuery.\n\n\
             2. Do NOT introduce JPA static metamodel classes (e.g., Entity_, Consumer_) unless \
             they already exist in the project. Use string-based property access: root.get(\"fieldName\")\n\n\
             3. If the existing code passes DetachedCriteria to a wrapper/factory method (e.g., \
             buildQuery(session, criteria)), pass the new CriteriaQuery directly to that same method.\n\
             Do NOT call entityManager.createQuery() first and do NOT materialize results to a List \
             before passing.\n\n\
             4. When removing an import for a removed class, DELETE the import line entirely. \
             Do NOT comment it out or add TODO markers.\n\n\
             5. If a type from the removed import is used in method signatures inherited from an \
             interface, change the method signature to use the replacement type (e.g., Criterion → \
             Predicate, DetachedCriteria → CriteriaQuery).\n\n\
             6. If you add a new method that implements a replacement interface contract (e.g., \
             getQueryRestriction replacing getCriteriaRestrictions), DELETE the old method entirely \
             along with its import. The old interface no longer declares it — it is dead code. Do NOT \
             keep both the old and new methods in the same class.\n",
        );
    }

    ctx
}

fn make_sd_rule(
    change: &JavaSourceChange,
    config: &JavaKonveyorConfig,
    id_counts: &mut HashMap<String, usize>,
) -> Option<KonveyorRule> {
    let (change_type_label, scope, effort) = match change.category {
        JavaSourceCategory::AnnotationRemoved => ("annotation-removed", "ANNOTATION", 3),
        JavaSourceCategory::AnnotationChanged => ("annotation-changed", "ANNOTATION", 2),
        JavaSourceCategory::SynchronizationRemoved => ("synchronization-removed", "METHOD_CALL", 3),
        JavaSourceCategory::ExceptionAdded => ("exception-added", "METHOD_CALL", 3),
        JavaSourceCategory::SerializationFieldRemoved => ("serialization-break", "TYPE", 5),
        JavaSourceCategory::SerializationFieldTypeChanged => ("serialization-break", "TYPE", 5),
        JavaSourceCategory::TransientChanged => ("serialization-break", "TYPE", 3),
        JavaSourceCategory::OverrideRemoved => ("override-removed", "TYPE", 3),
        JavaSourceCategory::ConstructorDependencyChanged => {
            ("constructor-changed", "CONSTRUCTOR_CALL", 3)
        }
        JavaSourceCategory::FinalAdded => ("final-added", "TYPE", 3),
        JavaSourceCategory::SealedChanged => ("sealed-changed", "TYPE", 3),
        JavaSourceCategory::InheritanceChanged => ("inheritance-changed", "TYPE", 5),
        JavaSourceCategory::NativeRemoved => ("native-removed", "METHOD_CALL", 5),
        JavaSourceCategory::DelegationChanged => ("delegation-changed", "METHOD_CALL", 3),
        JavaSourceCategory::ModuleExportRemoved => ("module-export-removed", "IMPORT", 5),
        // Non-breaking categories don't generate rules
        _ => return None,
    };

    let class_pattern = regex_escape(&change.class_name);
    let rule_id = unique_id(
        &format!(
            "{}-sd-{}-{}",
            config.rule_id_prefix,
            change_type_label,
            slugify(&change.class_name)
        ),
        id_counts,
    );

    let mut labels = vec![
        "source=semver-analyzer".into(),
        format!("change-type={}", change_type_label),
        "language=java".into(),
        "pipeline=sd".into(),
    ];

    if change.method.is_some() {
        labels.push("scope=method".into());
    }

    let links = config
        .migration_guide_url
        .as_ref()
        .map(|url| {
            vec![KonveyorLink {
                url: url.clone(),
                title: config
                    .migration_guide_title
                    .clone()
                    .unwrap_or_else(|| "Migration Guide".into()),
            }]
        })
        .unwrap_or_default();

    Some(KonveyorRule {
        rule_id,
        labels,
        effort,
        category: "mandatory".into(),
        description: change.description.clone(),
        message: build_sd_message(change),
        links,
        when: KonveyorCondition::JavaReferenced {
            referenced: JavaReferencedFields {
                pattern: class_pattern,
                scope: Some(scope.to_string()),
                ..Default::default()
            },
        },
        fix_strategy: build_sd_fix_strategy(change),
    })
}

fn build_sd_message(change: &JavaSourceChange) -> String {
    let mut msg = change.description.clone();

    if let (Some(old), Some(new)) = (&change.old_value, &change.new_value) {
        msg.push_str(&format!("\n\nBefore: `{}`\nAfter: `{}`", old, new));
    } else if let Some(old) = &change.old_value {
        msg.push_str(&format!("\n\nRemoved: `{}`", old));
    } else if let Some(new) = &change.new_value {
        msg.push_str(&format!("\n\nAdded: `{}`", new));
    }

    msg
}

fn build_sd_fix_strategy(change: &JavaSourceChange) -> Option<FixStrategyEntry> {
    match change.category {
        JavaSourceCategory::AnnotationRemoved | JavaSourceCategory::AnnotationChanged => {
            Some(FixStrategyEntry::new("ManualReview"))
        }
        JavaSourceCategory::FinalAdded | JavaSourceCategory::SealedChanged => {
            Some(FixStrategyEntry::new("ManualReview"))
        }
        JavaSourceCategory::InheritanceChanged => {
            if let (Some(old), Some(new)) = (&change.old_value, &change.new_value) {
                Some(FixStrategyEntry::with_from_to(
                    "UpdateSignature",
                    old,
                    new,
                ))
            } else {
                Some(FixStrategyEntry::new("ManualReview"))
            }
        }
        _ => None,
    }
}

// ── TD rule helpers ─────────────────────────────────────────────────────

fn make_import_relocation_rule(
    name: &str,
    old_qname: &str,
    new_qname: &str,
    config: &JavaKonveyorConfig,
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    let rule_id = unique_id(
        &format!("{}-import-{}", config.rule_id_prefix, slugify(name)),
        id_counts,
    );

    let old_pkg = old_qname
        .rsplit_once('.')
        .map(|(p, _)| p)
        .unwrap_or(old_qname);
    let new_pkg = new_qname
        .rsplit_once('.')
        .map(|(p, _)| p)
        .unwrap_or(new_qname);

    let links = config
        .migration_guide_url
        .as_ref()
        .map(|url| {
            vec![KonveyorLink {
                url: url.clone(),
                title: config
                    .migration_guide_title
                    .clone()
                    .unwrap_or_else(|| "Migration Guide".into()),
            }]
        })
        .unwrap_or_default();

    KonveyorRule {
        rule_id,
        labels: vec![
            "source=semver-analyzer".into(),
            "change-type=import-path-change".into(),
            "language=java".into(),
            "has-codemod=true".into(),
        ],
        effort: 1,
        category: "mandatory".into(),
        description: format!("`{}` moved from `{}` to `{}`", name, old_pkg, new_pkg),
        message: format!(
            "`{}` has been relocated.\n\n\
             Replace:\n  `import {}`\n\
             With:\n  `import {}`",
            name, old_qname, new_qname,
        ),
        links,
        when: KonveyorCondition::JavaReferenced {
            referenced: JavaReferencedFields {
                pattern: old_qname.to_string(),
                scope: Some("IMPORT".into()),
                ..Default::default()
            },
        },
        fix_strategy: Some(FixStrategyEntry::with_from_to(
            "JavaImportRename",
            old_qname,
            new_qname,
        )),
    }
}

fn make_rename_rule(
    symbol: &str,
    old_name: &str,
    new_name: &str,
    description: &str,
    config: &JavaKonveyorConfig,
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    let is_fqn = old_name.contains('.');

    let rule_id = unique_id(
        &format!(
            "{}-{}-{}",
            config.rule_id_prefix,
            if is_fqn { "migrate" } else { "rename" },
            slugify(symbol)
        ),
        id_counts,
    );

    // FQN renames (class/type renames): use JavaImportRename strategy for
    // safe, import-aware, word-boundary-aware replacement.
    // Bare method renames: use LlmAssisted because short method names like
    // "read" or "connection" cause false positives with text replacement.
    let fix_strategy = if is_fqn {
        Some(FixStrategyEntry::with_from_to(
            "JavaImportRename",
            old_name,
            new_name,
        ))
    } else {
        Some(FixStrategyEntry::with_from_to(
            "LlmAssisted",
            old_name,
            new_name,
        ))
    };

    // For bare method names, add word boundary anchors to the scanner
    // pattern to prevent matching inside unrelated identifiers
    // (e.g., "read" matching inside "Thread").
    let scanner_pattern = if is_fqn {
        old_name.to_string()
    } else {
        format!("\\b{}\\b", old_name)
    };

    KonveyorRule {
        rule_id,
        labels: vec![
            "source=semver-analyzer".into(),
            "change-type=renamed".into(),
            "language=java".into(),
        ],
        effort: 3,
        category: "mandatory".into(),
        description: format!("`{}` renamed to `{}`", old_name, new_name),
        message: format!(
            "{}\n\nReplace `{}` with `{}`.",
            description, old_name, new_name,
        ),
        links: vec![],
        when: KonveyorCondition::Or {
            or: vec![
                KonveyorCondition::JavaReferenced {
                    referenced: JavaReferencedFields {
                        pattern: scanner_pattern.clone(),
                        scope: Some("IMPORT".into()),
                        ..Default::default()
                    },
                },
                KonveyorCondition::JavaReferenced {
                    referenced: JavaReferencedFields {
                        pattern: scanner_pattern,
                        scope: Some("TYPE".into()),
                        ..Default::default()
                    },
                },
            ],
        },
        fix_strategy,
    }
}

fn make_removal_rule(
    symbol: &str,
    qualified_name: &str,
    description: &str,
    config: &JavaKonveyorConfig,
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    let rule_id = unique_id(
        &format!("{}-removed-{}", config.rule_id_prefix, slugify(symbol)),
        id_counts,
    );

    KonveyorRule {
        rule_id,
        labels: vec![
            "source=semver-analyzer".into(),
            "change-type=removed".into(),
            "language=java".into(),
        ],
        effort: 5,
        category: "mandatory".into(),
        description: format!("`{}` has been removed", symbol),
        message: format!(
            "{}\n\nThis class has been removed with no direct replacement.",
            description
        ),
        links: vec![],
        when: KonveyorCondition::JavaReferenced {
            referenced: JavaReferencedFields {
                pattern: qualified_name.to_string(),
                scope: Some("IMPORT".into()),
                ..Default::default()
            },
        },
        fix_strategy: None,
    }
}

fn make_removal_with_target_rule(
    symbol: &str,
    old_qname: &str,
    new_symbol: &str,
    new_qname: &str,
    description: &str,
    config: &JavaKonveyorConfig,
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    let rule_id = unique_id(
        &format!("{}-migrate-{}", config.rule_id_prefix, slugify(symbol)),
        id_counts,
    );

    KonveyorRule {
        rule_id,
        labels: vec![
            "source=semver-analyzer".into(),
            "change-type=removed".into(),
            "has-codemod=true".into(),
            "language=java".into(),
        ],
        effort: 3,
        category: "mandatory".into(),
        description: format!("`{}` removed -- migrate to `{}`", symbol, new_symbol),
        message: format!(
            "{}\n\nReplace:\n  `import {}`\nWith:\n  `import {}`",
            description, old_qname, new_qname,
        ),
        links: vec![],
        when: KonveyorCondition::JavaReferenced {
            referenced: JavaReferencedFields {
                pattern: old_qname.to_string(),
                scope: Some("IMPORT".into()),
                ..Default::default()
            },
        },
        fix_strategy: Some(FixStrategyEntry::with_from_to(
            "JavaImportRename",
            old_qname,
            new_qname,
        )),
    }
}

fn make_type_changed_rule(
    symbol: &str,
    qualified_name: &str,
    before: &str,
    after: &str,
    description: &str,
    config: &JavaKonveyorConfig,
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    let rule_id = unique_id(
        &format!(
            "{}-type-changed-{}",
            config.rule_id_prefix,
            slugify(symbol)
        ),
        id_counts,
    );

    // Extract method name and declaring class from qualified name
    // e.g., "org.hibernate.Interceptor.onFlushDirty" → class="Interceptor", method="onFlushDirty"
    let (declaring_class, method_name) = extract_class_and_member(qualified_name);

    // Build a condition that matches both:
    // 1. Direct type references (TYPE scope) — for code that uses the type directly
    // 2. Method definitions in classes extending/implementing the declaring class
    //    (DEFINITION scope + extends/implements filter) — for consumer overrides
    let when = if let (Some(class), Some(method)) = (&declaring_class, &method_name) {
        KonveyorCondition::Or {
            or: vec![
                KonveyorCondition::JavaReferenced {
                    referenced: JavaReferencedFields {
                        pattern: regex_escape(qualified_name),
                        scope: Some("TYPE".into()),
                        ..Default::default()
                    },
                },
                KonveyorCondition::JavaReferenced {
                    referenced: JavaReferencedFields {
                        pattern: method.clone(),
                        scope: Some("DEFINITION".into()),
                        kind: Some("method".into()),
                        extends: Some(class.clone()),
                        ..Default::default()
                    },
                },
                KonveyorCondition::JavaReferenced {
                    referenced: JavaReferencedFields {
                        pattern: method.clone(),
                        scope: Some("DEFINITION".into()),
                        kind: Some("method".into()),
                        implements: Some(class.clone()),
                        ..Default::default()
                    },
                },
            ],
        }
    } else {
        KonveyorCondition::JavaReferenced {
            referenced: JavaReferencedFields {
                pattern: regex_escape(qualified_name),
                scope: Some("TYPE".into()),
                ..Default::default()
            },
        }
    };

    KonveyorRule {
        rule_id,
        labels: vec![
            "source=semver-analyzer".into(),
            "change-type=type-changed".into(),
            "language=java".into(),
        ],
        effort: 3,
        category: "mandatory".into(),
        description: format!("Type of `{}` changed: `{}` → `{}`", symbol, before, after),
        message: format!(
            "{}\n\nType changed from `{}` to `{}`.",
            description, before, after
        ),
        links: vec![],
        when,
        fix_strategy: Some(FixStrategyEntry::with_from_to("UpdateType", before, after)),
    }
}

fn make_signature_changed_rule(
    symbol: &str,
    qualified_name: &str,
    before: &str,
    after: &str,
    description: &str,
    config: &JavaKonveyorConfig,
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    // Detect annotation element changes: when qualified_name contains
    // ".annotations." and before/after are "method: <name>: <type>",
    // this is an annotation parameter rename (e.g., @Type(type=...) → @Type(value=...)).
    // Use AnnotationParamRewrite strategy with IMPORT scope instead of UpdateSignature.
    if let Some(annotation_rule) = try_make_annotation_param_rule(
        symbol,
        qualified_name,
        before,
        after,
        description,
        config,
        id_counts,
    ) {
        return annotation_rule;
    }

    let rule_id = unique_id(
        &format!(
            "{}-sig-changed-{}",
            config.rule_id_prefix,
            slugify(symbol)
        ),
        id_counts,
    );

    // Extract method name and declaring class from qualified name
    let (declaring_class, method_name) = extract_class_and_member(qualified_name);

    // Build conditions matching both callers and overriders
    let when = if let (Some(class), Some(method)) = (&declaring_class, &method_name) {
        KonveyorCondition::Or {
            or: vec![
                // Match direct method calls
                KonveyorCondition::JavaReferenced {
                    referenced: JavaReferencedFields {
                        pattern: regex_escape(qualified_name),
                        scope: Some("METHOD_CALL".into()),
                        ..Default::default()
                    },
                },
                // Match method definitions in subclasses (overrides)
                KonveyorCondition::JavaReferenced {
                    referenced: JavaReferencedFields {
                        pattern: method.clone(),
                        scope: Some("DEFINITION".into()),
                        kind: Some("method".into()),
                        extends: Some(class.clone()),
                        ..Default::default()
                    },
                },
                KonveyorCondition::JavaReferenced {
                    referenced: JavaReferencedFields {
                        pattern: method.clone(),
                        scope: Some("DEFINITION".into()),
                        kind: Some("method".into()),
                        implements: Some(class.clone()),
                        ..Default::default()
                    },
                },
            ],
        }
    } else {
        KonveyorCondition::JavaReferenced {
            referenced: JavaReferencedFields {
                pattern: regex_escape(qualified_name),
                scope: Some("METHOD_CALL".into()),
                ..Default::default()
            },
        }
    };

    KonveyorRule {
        rule_id,
        labels: vec![
            "source=semver-analyzer".into(),
            "change-type=signature-changed".into(),
            "language=java".into(),
        ],
        effort: 3,
        category: "mandatory".into(),
        description: format!("Signature of `{}` changed", symbol),
        message: format!(
            "{}\n\nBefore: `{}`\nAfter: `{}`",
            description, before, after
        ),
        links: vec![],
        when,
        fix_strategy: Some(FixStrategyEntry::with_from_to(
            "UpdateSignature",
            before,
            after,
        )),
    }
}

/// Detect annotation element changes and generate an AnnotationParamRewrite rule.
///
/// Annotation elements appear as signature changes where:
/// - `qualified_name` contains `.annotations.` (e.g., `org.hibernate.annotations.Type.type`)
/// - `before`/`after` follow the pattern `method: <name>: <type>`
///
/// For these, we generate a rule with IMPORT scope (to match files importing the annotation)
/// and an `AnnotationParamRewrite` fix strategy that handles the param rename + value transform.
fn try_make_annotation_param_rule(
    symbol: &str,
    qualified_name: &str,
    before: &str,
    after: &str,
    description: &str,
    config: &JavaKonveyorConfig,
    id_counts: &mut HashMap<String, usize>,
) -> Option<KonveyorRule> {
    // Only handle annotation elements
    if !qualified_name.contains(".annotations.") {
        return None;
    }

    // Parse before/after to extract param names: "method: <name>: <type>"
    let parse_method_name = |s: &str| -> Option<String> {
        let s = s.strip_prefix("method: ")?;
        let colon_pos = s.find(':')?;
        Some(s[..colon_pos].trim().to_string())
    };

    let old_param = parse_method_name(before)?;
    let new_param = parse_method_name(after)?;

    // Only generate AnnotationParamRewrite when the element was actually renamed
    if old_param == new_param {
        return None;
    }

    // Extract the annotation class FQN: e.g., org.hibernate.annotations.Type from
    // org.hibernate.annotations.Type.type
    let annotation_fqn = qualified_name.rsplit_once('.')?.0;

    let rule_id = unique_id(
        &format!(
            "{}-annotation-param-{}",
            config.rule_id_prefix,
            slugify(symbol)
        ),
        id_counts,
    );

    // Determine value transform based on old/new types
    // "method: type: String" → String value (FQN) → Class literal
    // For other type changes, use Identity
    let old_type = before.rsplit(": ").next().unwrap_or("");
    let new_type = after.rsplit(": ").next().unwrap_or("");
    let value_transform = if old_type == "String" && new_type.contains("Class") {
        "StringFqnToClassLiteral"
    } else {
        "Identity"
    };

    // Use IMPORT scope to match files that import this annotation
    let when = KonveyorCondition::JavaReferenced {
        referenced: JavaReferencedFields {
            pattern: regex_escape(annotation_fqn),
            scope: Some("IMPORT".into()),
            ..Default::default()
        },
    };

    let mut fix_entry = FixStrategyEntry::with_from_to(
        "AnnotationParamRewrite",
        &old_param,
        &new_param,
    );
    fix_entry.replacement = Some(value_transform.to_string());

    Some(KonveyorRule {
        rule_id,
        labels: vec![
            "source=semver-analyzer".into(),
            "change-type=annotation-param-changed".into(),
            "language=java".into(),
        ],
        effort: 2,
        category: "mandatory".into(),
        description: format!(
            "Annotation `{}` parameter `{}` renamed to `{}`",
            annotation_fqn.rsplit('.').next().unwrap_or(annotation_fqn),
            old_param,
            new_param,
        ),
        message: format!(
            "{}\n\nRewrite `@{}({} = ...)` to `@{}({} = ...)`.\n\nBefore: `{}`\nAfter: `{}`",
            description,
            annotation_fqn.rsplit('.').next().unwrap_or(annotation_fqn),
            old_param,
            annotation_fqn.rsplit('.').next().unwrap_or(annotation_fqn),
            new_param,
            before,
            after,
        ),
        links: vec![],
        when,
        fix_strategy: Some(fix_entry),
    })
}

fn make_visibility_changed_rule(
    symbol: &str,
    qualified_name: &str,
    before: &str,
    after: &str,
    description: &str,
    config: &JavaKonveyorConfig,
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    let rule_id = unique_id(
        &format!(
            "{}-visibility-{}",
            config.rule_id_prefix,
            slugify(symbol)
        ),
        id_counts,
    );

    KonveyorRule {
        rule_id,
        labels: vec![
            "source=semver-analyzer".into(),
            "change-type=visibility-changed".into(),
            "language=java".into(),
        ],
        effort: 3,
        category: "mandatory".into(),
        description: format!(
            "Visibility of `{}` changed: {} → {}",
            symbol, before, after
        ),
        message: format!(
            "{}\n\nVisibility narrowed from `{}` to `{}`.",
            description, before, after
        ),
        links: vec![],
        when: KonveyorCondition::JavaReferenced {
            referenced: JavaReferencedFields {
                pattern: regex_escape(qualified_name),
                scope: Some("TYPE".into()),
                ..Default::default()
            },
        },
        fix_strategy: Some(FixStrategyEntry::new("ManualReview")),
    }
}

fn make_dependency_rule(
    field: &str,
    before: &str,
    after: Option<&str>,
    description: &str,
    config: &JavaKonveyorConfig,
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    let dep_name = field.strip_prefix("dependency:").unwrap_or(field);
    let rule_id = unique_id(
        &format!("{}-dep-{}", config.rule_id_prefix, slugify(dep_name)),
        id_counts,
    );

    let message = if let Some(new) = after {
        format!("{}\n\nReplace `{}` with `{}`.", description, before, new)
    } else {
        format!("{}\n\nThis dependency has been removed.", description)
    };

    KonveyorRule {
        rule_id,
        labels: vec![
            "source=semver-analyzer".into(),
            "change-type=dependency-update".into(),
            "language=java".into(),
        ],
        effort: 1,
        category: "mandatory".into(),
        description: description.to_string(),
        message,
        links: vec![],
        when: KonveyorCondition::JavaDependency {
            dependency: JavaDependencyFields {
                name: Some(dep_name.to_string()),
                nameregex: None,
                // Match any version (kantra requires at least one bound)
                upperbound: Some("99.99.99".into()),
                lowerbound: Some("0".into()),
            },
        },
        fix_strategy: None,
    }
}

// ── Class migration rules (mostly-emptied base classes) ─────────────────

/// Generate migration rules for classes that had most of their methods removed.
///
/// These are typically abstract base classes or convenience implementations
/// (e.g., `EmptyInterceptor`) that consumers extend. When most methods are
/// removed, consumers need to switch to implementing the interface directly.
///
/// Each generated rule:
/// - Matches at IMPORT scope (consumer imports the emptied class)
/// - Uses DEFINITION scope + extends filter (consumer extends the class)
/// - Carries rich LLM context with the list of removed methods and their new signatures
pub fn generate_class_migration_rules(
    report: &AnalysisReport<Java>,
    config: &JavaKonveyorConfig,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();
    let mut id_counts: HashMap<String, usize> = HashMap::new();

    // Collect per-class removed methods from the report
    let mut class_removed_methods: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut class_packages: HashMap<String, String> = HashMap::new();

    for fc in &report.changes {
        if !matches!(fc.status, semver_analyzer_core::FileStatus::Modified) {
            continue;
        }
        for ac in &fc.breaking_api_changes {
            if ac.change != semver_analyzer_core::ApiChangeType::Removed {
                continue;
            }
            // Only method-level removals (member of a class)
            let (class_opt, method_opt) = extract_class_and_member(&ac.qualified_name);
            if let (Some(_class_name), Some(method_name)) = (class_opt, method_opt) {
                let class_qn = ac
                    .qualified_name
                    .rsplit_once('.')
                    .map(|(prefix, _)| prefix.to_string())
                    .unwrap_or_default();

                let pkg = class_qn
                    .rsplit_once('.')
                    .map(|(p, _)| p.to_string())
                    .unwrap_or_default();

                class_removed_methods
                    .entry(class_qn.clone())
                    .or_default()
                    .push((method_name, ac.description.clone()));
                class_packages.insert(class_qn, pkg);
            }
        }
    }

    // Generate rules for classes with 5+ removed methods
    for (class_qn, removed_methods) in &class_removed_methods {
        if removed_methods.len() < 5 {
            continue;
        }

        let class_name = class_qn.rsplit('.').next().unwrap_or(class_qn);
        let pkg = class_packages
            .get(class_qn.as_str())
            .map(|s| s.as_str())
            .unwrap_or("");

        // Build a rich migration context listing all removed methods
        let method_list: Vec<String> = removed_methods
            .iter()
            .map(|(name, _desc)| format!("  - {name}"))
            .collect();

        let message = format!(
            "`{}` has had {} methods removed, indicating a major API redesign.\n\n\
             Consumers extending this class should migrate to implementing the \
             corresponding interface directly.\n\n\
             Removed methods:\n{}\n\n\
             ## Migration instructions\n\n\
             1. If this file wraps APIs that were completely removed (e.g., `DetachedCriteria`, \
             `CriteriaImpl`, `Criteria`) and has NO external callers (no other file references \
             this class by name), **delete this file entirely** — it is dead code.\n\n\
             2. If a factory or caller already references a replacement class that doesn't exist \
             yet (e.g., `CriteriaCandlepinQuery`), **create that replacement class** as a new file \
             in the same package. The replacement should implement the same interface using the \
             new JPA Criteria API (`jakarta.persistence.criteria.*`). Look for other implementations \
             of the same interface (e.g., `EmptyCandlepinQuery`) as a reference for the method \
             contract.\n\n\
             3. Otherwise, update your class to implement the interface instead of extending \
             this base class. Add `@Override` implementations for the methods you need.",
            class_name,
            removed_methods.len(),
            method_list.join("\n"),
        );

        let rule_id = unique_id(
            &format!(
                "{}-class-migrate-{}",
                config.rule_id_prefix,
                slugify(class_name)
            ),
            &mut id_counts,
        );

        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".into(),
                "change-type=class-migration".into(),
                "language=java".into(),
            ],
            effort: 5,
            category: "mandatory".into(),
            description: format!(
                "`{}` base class emptied — migrate to interface",
                class_name
            ),
            message,
            links: vec![],
            when: KonveyorCondition::Or {
                or: vec![
                    // Match files importing the emptied class
                    KonveyorCondition::JavaReferenced {
                        referenced: JavaReferencedFields {
                            pattern: regex_escape(class_qn),
                            scope: Some("IMPORT".into()),
                            ..Default::default()
                        },
                    },
                    // Match classes extending the emptied class
                    KonveyorCondition::JavaReferenced {
                        referenced: JavaReferencedFields {
                            pattern: class_name.to_string(),
                            scope: Some("DEFINITION".into()),
                            kind: Some("class".into()),
                            extends: Some(class_name.to_string()),
                            ..Default::default()
                        },
                    },
                ],
            },
            fix_strategy: {
                let interface_name = class_name
                    .strip_prefix("Empty")
                    .or_else(|| class_name.strip_prefix("Abstract"))
                    .unwrap_or(class_name);
                Some(FixStrategyEntry::with_from_to(
                    "LlmAssisted",
                    format!("extends {class_name}"),
                    format!("implements {interface_name} (from package {pkg})"),
                ))
            },
        });
    }

    rules
}

// ── Namespace migration rules ───────────────────────────────────────────

/// A parsed namespace migration with optional dependency coordinate.
#[derive(Debug, Clone)]
pub struct NamespaceMigration {
    pub old_ns: String,
    pub new_ns: String,
    /// Optional Maven/Gradle coordinate: `"group:artifact:version"`
    pub dependency: Option<(String, String)>, // (group:artifact, version)
}

/// Parse a namespace migration string.
///
/// Formats:
/// - `"javax.persistence=jakarta.persistence"` — import rename only
/// - `"javax.persistence=jakarta.persistence@jakarta.persistence:jakarta.persistence-api:3.1.0"` — import rename + dependency update
pub fn parse_namespace_migration(s: &str) -> Option<NamespaceMigration> {
    let (old, rest) = s.split_once('=')?;
    let old = old.trim();
    if old.is_empty() {
        return None;
    }

    // Check for @dependency suffix
    let (new_ns, dependency) = if let Some((ns, dep_str)) = rest.trim().split_once('@') {
        let dep = parse_dependency_coordinate(dep_str.trim());
        (ns.trim(), dep)
    } else {
        (rest.trim(), None)
    };

    if new_ns.is_empty() {
        return None;
    }

    Some(NamespaceMigration {
        old_ns: old.to_string(),
        new_ns: new_ns.to_string(),
        dependency,
    })
}

/// Parse a Maven/Gradle dependency coordinate `"group:artifact:version"`.
/// Returns `(group:artifact, version)`.
fn parse_dependency_coordinate(s: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() >= 3 {
        let group_artifact = format!("{}:{}", parts[0], parts[1]);
        let version = parts[2].to_string();
        Some((group_artifact, version))
    } else {
        None
    }
}

/// Generate namespace migration rules from parsed migrations.
///
/// Each migration produces:
/// 1. An import-scoped rule with `JavaImportRename` strategy
/// 2. If a dependency coordinate is provided, a companion `EnsureDependency` rule
pub fn generate_namespace_migration_rules(
    migrations: &[NamespaceMigration],
    config: &JavaKonveyorConfig,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();
    let mut id_counts: HashMap<String, usize> = HashMap::new();

    for mig in migrations {
        rules.push(make_namespace_migration_rule(
            &mig.old_ns,
            &mig.new_ns,
            config,
            &mut id_counts,
        ));

        // Emit companion dependency update rule if coordinates provided
        if let Some((ref coord, ref version)) = mig.dependency {
            rules.push(make_namespace_dependency_rule(
                &mig.old_ns,
                coord,
                version,
                config,
                &mut id_counts,
            ));
        }
    }

    rules
}

fn make_namespace_migration_rule(
    old_ns: &str,
    new_ns: &str,
    config: &JavaKonveyorConfig,
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    let rule_id = unique_id(
        &format!("{}-ns-migrate-{}", config.rule_id_prefix, slugify(old_ns)),
        id_counts,
    );

    let links = config
        .migration_guide_url
        .as_ref()
        .map(|url| {
            vec![KonveyorLink {
                url: url.clone(),
                title: config
                    .migration_guide_title
                    .clone()
                    .unwrap_or_else(|| "Migration Guide".into()),
            }]
        })
        .unwrap_or_default();

    KonveyorRule {
        rule_id,
        labels: vec![
            "source=semver-analyzer".into(),
            "change-type=import-path-change".into(),
            "has-codemod=true".into(),
            "language=java".into(),
        ],
        effort: 1,
        category: "mandatory".into(),
        description: format!("Migrate `{}` imports to `{}`", old_ns, new_ns),
        message: format!(
            "The `{}` namespace has been replaced by `{}`.\n\n\
             Replace all `import {}.*` with `import {}.*`.",
            old_ns, new_ns, old_ns, new_ns
        ),
        links,
        when: KonveyorCondition::JavaReferenced {
            referenced: JavaReferencedFields {
                pattern: format!("{}\\.", regex_escape(old_ns)),
                scope: Some("IMPORT".into()),
                ..Default::default()
            },
        },
        fix_strategy: Some(FixStrategyEntry::with_from_to(
            "JavaImportRename",
            old_ns,
            new_ns,
        )),
    }
}

fn make_namespace_dependency_rule(
    old_ns: &str,
    new_coordinate: &str, // "group:artifact"
    new_version: &str,
    config: &JavaKonveyorConfig,
    id_counts: &mut HashMap<String, usize>,
) -> KonveyorRule {
    let rule_id = unique_id(
        &format!("{}-ns-dep-{}", config.rule_id_prefix, slugify(old_ns)),
        id_counts,
    );

    // Extract the old artifact name from the namespace for dependency matching.
    // e.g., "javax.persistence" → match any dependency containing "javax.persistence"
    let old_artifact_pattern = old_ns.replace('.', ".*");

    KonveyorRule {
        rule_id,
        labels: vec![
            "source=semver-analyzer".into(),
            "change-type=dependency-update".into(),
            "has-codemod=true".into(),
            "language=java".into(),
        ],
        effort: 1,
        category: "mandatory".into(),
        description: format!(
            "Update `{}` dependency to `{}:{}`",
            old_ns, new_coordinate, new_version
        ),
        message: format!(
            "The `{}` namespace requires updating the build dependency.\n\n\
             Update your build file to use `{}:{}`.",
            old_ns, new_coordinate, new_version
        ),
        links: vec![],
        when: KonveyorCondition::JavaDependency {
            dependency: JavaDependencyFields {
                name: None,
                nameregex: Some(old_artifact_pattern),
                // Match any version (kantra requires at least one bound)
                upperbound: Some("99.99.99".into()),
                lowerbound: Some("0".into()),
            },
        },
        fix_strategy: Some(FixStrategyEntry::ensure_dependency_with_old(
            new_coordinate,
            new_version,
            old_ns,
        )),
    }
}

/// Generate `EnsureDependency` rules for a library group ID change.
///
/// When a library changes its Maven group ID (e.g., `org.hibernate` → `org.hibernate.orm`),
/// consumers need to update their dependency coordinates. This generates one rule per
/// published submodule so the fix engine can find and replace each dependency in the
/// consumer's build files.
///
/// `to_ref` is the library's target version (used as the new dependency version).
/// `submodules` contains the artifact names (e.g., `["hibernate-core", "hibernate-c3p0"]`).
pub fn generate_group_id_migration_rules(
    old_group: &str,
    new_group: &str,
    to_ref: &str,
    submodules: &[String],
    config: &JavaKonveyorConfig,
) -> Vec<KonveyorRule> {
    let mut rules = Vec::new();
    let mut id_counts: HashMap<String, usize> = HashMap::new();

    // For each submodule, generate an EnsureDependency rule
    let artifacts: Vec<&str> = if submodules.is_empty() {
        // If no submodules found, generate a generic rule for the root artifact
        vec![]
    } else {
        submodules.iter().map(|s| s.as_str()).collect()
    };

    for artifact in &artifacts {
        let old_coordinate = format!("{}:{}", old_group, artifact);
        let new_coordinate = format!("{}:{}", new_group, artifact);

        let rule_id = unique_id(
            &format!(
                "{}-dep-group-{}",
                config.rule_id_prefix,
                slugify(artifact)
            ),
            &mut id_counts,
        );

        // Clean up the version: strip "Final" suffix variations, use just major.minor.patch
        let version = to_ref
            .trim_end_matches(".Final")
            .trim_end_matches("-SNAPSHOT");

        rules.push(KonveyorRule {
            rule_id,
            labels: vec![
                "source=semver-analyzer".into(),
                "change-type=dependency-update".into(),
                "has-codemod=true".into(),
                "language=java".into(),
            ],
            effort: 1,
            category: "mandatory".into(),
            description: format!(
                "Update dependency: `{}` → `{}`",
                old_coordinate, new_coordinate
            ),
            message: format!(
                "The library's Maven group ID changed from `{}` to `{}`.\n\n\
                 Update your build file: `{}:*` → `{}:{}`.",
                old_group, new_group, old_coordinate, new_coordinate, version
            ),
            links: vec![],
            when: KonveyorCondition::JavaDependency {
                dependency: JavaDependencyFields {
                    name: Some(old_coordinate.clone()),
                    nameregex: None,
                    upperbound: Some("99.99.99".into()),
                    lowerbound: Some("0".into()),
                },
            },
            fix_strategy: Some(FixStrategyEntry::ensure_dependency_with_old(
                &new_coordinate,
                version,
                &old_coordinate,
            )),
        });
    }

    rules
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn unique_id(base: &str, counts: &mut HashMap<String, usize>) -> String {
    let count = counts.entry(base.to_string()).or_insert(0);
    *count += 1;
    if *count == 1 {
        base.to_string()
    } else {
        format!("{}-{}", base, count)
    }
}

fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '.' | '/' | ':' | '@' | ' ' => '-',
            c if c.is_alphanumeric() || c == '-' || c == '_' => c,
            _ => '-',
        })
        .collect::<String>()
        .to_lowercase()
}

/// Check if old and new extends clauses indicate an incompatible base type change.
///
/// Returns `true` (incompatible) when:
/// - Both have extends, the base class name is the same, but type parameters differ
///   (e.g., `AbstractSingleColumnStandardBasicType<String>` vs `<Object>`)
/// - Both have extends but the base class names are completely different
///   (e.g., `AbstractTypeDescriptor<X>` vs `AbstractClassJavaType<Y>`)
///
/// Returns `false` (compatible or unknown) when:
/// - Either extends is None (no data to compare)
/// - Both are exactly the same
fn has_incompatible_base_type(old_extends: Option<&str>, new_extends: Option<&str>) -> bool {
    let (old_ext, new_ext) = match (old_extends, new_extends) {
        (Some(o), Some(n)) => (o, n),
        _ => return false,
    };

    if old_ext == new_ext {
        return false;
    }

    // Extract base class name (before '<') and type parameters (inside '<>')
    let (old_base, old_params) = split_generic(old_ext);
    let (new_base, new_params) = split_generic(new_ext);

    if old_base != new_base {
        // Completely different base classes -- incompatible
        return true;
    }

    // Same base class but different type parameters
    if old_params != new_params {
        return true;
    }

    false
}

/// Split a type expression into base name and generic parameters.
/// e.g., `"AbstractSingleColumnStandardBasicType<String>"` → `("AbstractSingleColumnStandardBasicType", "<String>")`
/// e.g., `"Foo"` → `("Foo", "")`
fn split_generic(s: &str) -> (&str, &str) {
    if let Some(pos) = s.find('<') {
        (&s[..pos], &s[pos..])
    } else {
        (s, "")
    }
}

/// Check if two fully-qualified names are in related packages.
///
/// Returns `true` if the packages share at least 3 segments (e.g.,
/// `org.hibernate.dialect` and `org.hibernate.community.dialect` share
/// `org.hibernate` = 2 common segments, plus the fact that the target
/// contains a suffix of the source path like `dialect`).
///
/// This filters out false renames where a class was removed in one subsystem
/// and a same-named class exists in an unrelated subsystem (e.g.,
/// `criterion.Property` → `spatial.dialect.sqlserver.Property`).
fn packages_are_related(old_fqn: &str, new_fqn: &str) -> bool {
    let old_pkg = old_fqn.rsplit_once('.').map(|(p, _)| p).unwrap_or(old_fqn);
    let new_pkg = new_fqn.rsplit_once('.').map(|(p, _)| p).unwrap_or(new_fqn);

    let old_parts: Vec<&str> = old_pkg.split('.').collect();
    let new_parts: Vec<&str> = new_pkg.split('.').collect();

    // Count common prefix segments
    let common = old_parts
        .iter()
        .zip(new_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();

    // Require at least 3 common segments for large libraries
    // (org.hibernate.X must share at least org.hibernate.X)
    if common >= 3 {
        return true;
    }

    // Special case: dialect → community.dialect is a known reorganization pattern
    // Check if the last segment of the old package appears in the new package
    if common >= 2 {
        if let Some(old_subsystem) = old_parts.get(common) {
            if new_parts[common..].contains(old_subsystem) {
                return true;
            }
        }
    }

    false
}

/// Check if a pattern looks like a Java type parameter (e.g., "T", "E",
/// "R", "S extends Foo") rather than a real class/method name. These
/// generate overly broad scanner rules that match nearly every file.
fn is_type_parameter_pattern(pattern: &str) -> bool {
    let trimmed = pattern.trim();
    // Single uppercase letter: T, E, R, S, N, Y, P, etc.
    if trimmed.len() == 1 && trimmed.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
        return true;
    }
    // Bounded type parameter: "T extends Foo", "E extends Enum<E>"
    if trimmed.contains(" extends ") {
        let first = trimmed.split_whitespace().next().unwrap_or("");
        if first.len() <= 2 && first.chars().all(|c| c.is_ascii_uppercase()) {
            return true;
        }
    }
    // "Serializable" used as a type parameter pattern is also too broad
    // (matches java.io.Serializable imports in most files)
    if trimmed == "Serializable" {
        return true;
    }
    false
}

fn regex_escape(s: &str) -> String {
    s.replace('.', "\\.")
}

/// Extract the declaring class simple name and member name from a qualified name.
///
/// e.g., `"org.hibernate.Interceptor.onFlushDirty"` → `(Some("Interceptor"), Some("onFlushDirty"))`
/// e.g., `"org.hibernate.Session"` → `(None, None)` — no member
///
/// Uses the Java convention that class names start with an uppercase letter
/// and member names start with a lowercase letter.
fn extract_class_and_member(qualified_name: &str) -> (Option<String>, Option<String>) {
    let parts: Vec<&str> = qualified_name.split('.').collect();
    if parts.len() < 2 {
        return (None, None);
    }

    // Walk from the end to find the member (lowercase start) and class (uppercase start)
    let last = parts[parts.len() - 1];
    let second_last = parts[parts.len() - 2];

    // If second_last starts uppercase and last starts lowercase → class.method
    if second_last
        .chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
        && last
            .chars()
            .next()
            .map(|c| c.is_lowercase())
            .unwrap_or(false)
    {
        return (
            Some(second_last.to_string()),
            Some(last.to_string()),
        );
    }

    (None, None)
}
