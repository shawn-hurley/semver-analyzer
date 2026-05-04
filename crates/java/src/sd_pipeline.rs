//! Java source-level diff (SD) pipeline.
//!
//! Deterministic AST-based analysis that detects behavioral breaking
//! changes by extracting structured profiles from Java source files
//! at two git refs and diffing them.
//!
//! ## Pipeline phases
//!
//! - **Phase A**: Find changed `.java` files, extract profiles at both
//!   refs, diff to produce `JavaSourceChange` entries
//! - **Phase B**: Extract all profiles at the new version
//! - **Phase B.5**: Resolve inheritance chains
//! - **Phase B1**: Build inheritance trees, detect hierarchy breakages
//! - **Phase B3**: Module system diff

use crate::sd_types::*;
use anyhow::{Context, Result};
use semver_analyzer_core::git::read_git_file;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use tree_sitter::{Node, Parser};

/// Run the Java SD pipeline.
///
/// This is the main entry point called by `Java::run_extended_analysis`.
pub fn run_java_sd(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    _from_worktree: Option<&Path>,
    to_worktree: Option<&Path>,
) -> Result<JavaSdPipelineResult> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .context("Failed to set tree-sitter Java language")?;

    // ── Phase A: Diff changed files ─────────────────────────────────
    tracing::info!("SD Phase A: diffing changed Java files");

    let changed_files = get_changed_java_files(repo, from_ref, to_ref)?;
    let mut old_profiles: HashMap<String, JavaClassProfile> = HashMap::new();
    let mut source_changes: Vec<JavaSourceChange> = Vec::new();

    for file_path in &changed_files {
        let old_source = read_git_file(repo, from_ref, file_path).unwrap_or_default();
        let new_source = read_git_file(repo, to_ref, file_path).unwrap_or_default();

        if old_source.is_empty() && new_source.is_empty() {
            continue;
        }

        let old_file_profiles = extract_profiles(&mut parser, &old_source, file_path);
        let new_file_profiles = extract_profiles(&mut parser, &new_source, file_path);

        // Store old profiles
        for p in &old_file_profiles {
            old_profiles.insert(p.qualified_name.clone(), p.clone());
        }

        // Diff profiles
        let mut changes = diff_file_profiles(&old_file_profiles, &new_file_profiles);
        source_changes.append(&mut changes);
    }

    // ── Phase A.5: Mine migration examples from test files ────────
    let (migration_examples, migration_mappings) =
        crate::migration_examples::mine_migration_examples(repo, to_ref, None)
            .unwrap_or_else(|e| {
                tracing::warn!("Migration example mining failed: {}", e);
                (Vec::new(), Vec::new())
            });

    // ── Phase B: Full extraction at to-ref ──────────────────────────
    tracing::info!("SD Phase B: extracting all profiles at to-ref");

    let new_profiles = if let Some(worktree) = to_worktree {
        extract_all_profiles(&mut parser, worktree)?
    } else {
        extract_all_profiles_from_git(&mut parser, repo, to_ref)?
    };

    // ── Phase B.5: Resolve inheritance ──────────────────────────────
    tracing::info!("SD Phase B.5: resolving inheritance chains");

    let serializable_classes = resolve_serializable_classes(&new_profiles);

    // Detect serialization changes for changed classes that are serializable
    for change_class in changed_files.iter().filter_map(|f| {
        // Map file path to class names in new profiles
        new_profiles
            .values()
            .find(|p| p.file == *f)
            .map(|p| p.qualified_name.clone())
    }) {
        if serializable_classes.contains(&change_class) {
            if let (Some(old_p), Some(new_p)) =
                (old_profiles.get(&change_class), new_profiles.get(&change_class))
            {
                let mut ser_changes = diff_serialization(old_p, new_p);
                source_changes.append(&mut ser_changes);
            }
        }
    }

    // ── Phase B1: Build inheritance summary ─────────────────────────
    tracing::info!("SD Phase B1: building inheritance trees");

    let inheritance_summary = build_inheritance_summary(&new_profiles);

    // ── Phase B3: Module system diff ────────────────────────────────
    tracing::info!("SD Phase B3: diffing module system");

    let module_changes = diff_modules(repo, from_ref, to_ref, &mut parser);

    let total_changes = source_changes.len() + module_changes.len();
    let breaking = source_changes.iter().filter(|c| c.is_breaking).count()
        + module_changes.iter().filter(|c| c.is_breaking).count();
    tracing::info!(
        total = total_changes,
        breaking = breaking,
        "SD pipeline complete"
    );

    Ok(JavaSdPipelineResult {
        source_level_changes: source_changes,
        old_profiles,
        new_profiles,
        module_changes,
        inheritance_summary,
        migration_examples,
        migration_mappings,
    })
}

// ── Phase A: Changed file discovery ─────────────────────────────────────

fn get_changed_java_files(repo: &Path, from_ref: &str, to_ref: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args([
            "diff",
            "--name-only",
            "--diff-filter=AMRC",
            &format!("{}..{}", from_ref, to_ref),
            "--",
            "*.java",
        ])
        .current_dir(repo)
        .output()
        .context("Failed to run git diff")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter(|l| !l.is_empty())
        .filter(|l| !is_test_file(l))
        .filter(|l| !l.ends_with("package-info.java"))
        .map(|l| l.to_string())
        .collect())
}

fn is_test_file(path: &str) -> bool {
    path.contains("/src/test/")
        || path.ends_with("Test.java")
        || path.ends_with("Tests.java")
        || path.ends_with("IT.java")
        || path.ends_with("ITCase.java")
}

// ── Profile extraction ──────────────────────────────────────────────────

fn extract_profiles(
    parser: &mut Parser,
    source: &str,
    file_path: &str,
) -> Vec<JavaClassProfile> {
    if source.is_empty() {
        return Vec::new();
    }

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let root = tree.root_node();
    let package = extract_package(root, source);
    let imports = extract_import_set(root, source);
    let mut profiles = Vec::new();

    extract_class_profiles(
        root,
        source,
        file_path,
        &package,
        &imports,
        &mut profiles,
    );

    profiles
}

fn extract_class_profiles(
    node: Node,
    source: &str,
    file_path: &str,
    package: &Option<String>,
    imports: &HashSet<String>,
    profiles: &mut Vec<JavaClassProfile>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration" => {
                if let Some(profile) =
                    extract_single_profile(child, source, file_path, package, imports)
                {
                    profiles.push(profile);
                }
                // Recurse into nested classes
                extract_class_profiles(child, source, file_path, package, imports, profiles);
            }
            _ => {
                extract_class_profiles(child, source, file_path, package, imports, profiles);
            }
        }
    }
}

fn extract_single_profile(
    node: Node,
    source: &str,
    file_path: &str,
    package: &Option<String>,
    imports: &HashSet<String>,
) -> Option<JavaClassProfile> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())?;

    let qualified_name = match package {
        Some(pkg) => format!("{}.{}", pkg, name),
        None => name.clone(),
    };

    let mut profile = JavaClassProfile {
        qualified_name,
        name,
        file: file_path.to_string(),
        ..Default::default()
    };

    // Extract modifiers
    if let Some(mods) = find_child_by_kind(node, "modifiers") {
        let mut mod_cursor = mods.walk();
        for mod_child in mods.children(&mut mod_cursor) {
            match mod_child.kind() {
                "final" => profile.is_final = true,
                "sealed" => profile.is_sealed = true,
                "abstract" => profile.is_abstract = true,
                "marker_annotation" | "annotation" => {
                    if let Some(ann) = parse_profile_annotation(mod_child, source) {
                        profile.annotations.push(ann);
                    }
                }
                _ => {}
            }
        }
    }

    // Extract extends
    if let Some(superclass) = node.child_by_field_name("superclass") {
        let type_node = superclass
            .child(superclass.child_count().saturating_sub(1))
            .unwrap_or(superclass);
        profile.extends = Some(node_text(type_node, source).to_string());
    }

    // Extract implements
    if let Some(interfaces) = node.child_by_field_name("interfaces") {
        profile.implements = extract_type_list(interfaces, source);
    }

    // Check for Serializable
    profile.is_serializable = profile.implements.iter().any(|i| i == "Serializable")
        || imports.contains("java.io.Serializable");

    // Extract permits
    if let Some(permits) = node.child_by_field_name("permits") {
        profile.permits = extract_type_list(permits, source);
    }

    // Extract body members (methods, fields, constructors)
    let body = find_child_by_kind(node, "class_body")
        .or_else(|| find_child_by_kind(node, "interface_body"))
        .or_else(|| find_child_by_kind(node, "enum_body"));

    if let Some(body) = body {
        let qname = profile.qualified_name.clone();
        extract_profile_members(body, source, &qname, &mut profile);
    }

    Some(profile)
}

fn extract_profile_members(
    body: Node,
    source: &str,
    parent_qname: &str,
    profile: &mut JavaClassProfile,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            "method_declaration" => {
                if let Some(method) = extract_method_profile(child, source, parent_qname) {
                    profile.methods.push(method);
                }
            }
            "constructor_declaration" | "compact_constructor_declaration" => {
                // Extract constructor parameter types
                if let Some(params) = child.child_by_field_name("parameters") {
                    profile.constructor_params = extract_param_types(params, source);
                }
            }
            "field_declaration" => {
                let mut fields = extract_field_profiles(child, source);
                profile.fields.append(&mut fields);
            }
            _ => {}
        }
    }
}

fn extract_method_profile(
    node: Node,
    source: &str,
    parent_qname: &str,
) -> Option<MethodProfile> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())?;

    let qualified_name = format!("{}.{}", parent_qname, name);

    let mut profile = MethodProfile {
        name,
        qualified_name,
        is_synchronized: false,
        is_native: false,
        is_override: false,
        is_default: false,
        is_abstract: false,
        thrown_exceptions: Vec::new(),
        annotations: Vec::new(),
        delegations: Vec::new(),
        return_type: node
            .child_by_field_name("type")
            .map(|n| node_text(n, source).to_string()),
        param_types: node
            .child_by_field_name("parameters")
            .map(|n| extract_param_types(n, source))
            .unwrap_or_default(),
    };

    // Extract modifiers and annotations
    if let Some(mods) = find_child_by_kind(node, "modifiers") {
        let mut mod_cursor = mods.walk();
        for mod_child in mods.children(&mut mod_cursor) {
            match mod_child.kind() {
                "synchronized" => profile.is_synchronized = true,
                "native" => profile.is_native = true,
                "default" => profile.is_default = true,
                "abstract" => profile.is_abstract = true,
                "marker_annotation" | "annotation" => {
                    if let Some(ann) = parse_profile_annotation(mod_child, source) {
                        if ann.name == "Override" {
                            profile.is_override = true;
                        }
                        profile.annotations.push(ann);
                    }
                }
                _ => {}
            }
        }
    }

    // Extract throws clause
    let mut throws_cursor = node.walk();
    for child in node.children(&mut throws_cursor) {
        if child.kind() == "throws" {
            let mut tc = child.walk();
            for throw_child in child.children(&mut tc) {
                if throw_child.kind() == "type_identifier"
                    || throw_child.kind() == "scoped_type_identifier"
                {
                    profile
                        .thrown_exceptions
                        .push(node_text(throw_child, source).to_string());
                }
            }
        }
    }

    // Extract method call delegations from body
    if let Some(body) = find_child_by_kind(node, "block") {
        extract_delegations(body, source, &mut profile.delegations);
    }

    Some(profile)
}

fn extract_delegations(node: Node, source: &str, delegations: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "method_invocation" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, source).to_string();
                // Include receiver if present
                if let Some(obj) = child.child_by_field_name("object") {
                    let receiver = node_text(obj, source);
                    delegations.push(format!("{}.{}", receiver, name));
                } else {
                    delegations.push(name);
                }
            }
        }
        // Recurse into child nodes
        extract_delegations(child, source, delegations);
    }
}

fn extract_field_profiles(node: Node, source: &str) -> Vec<FieldProfile> {
    let mut fields = Vec::new();

    let mut is_transient = false;
    let mut is_volatile = false;
    let mut is_static = false;
    let mut is_final = false;

    if let Some(mods) = find_child_by_kind(node, "modifiers") {
        let mut mc = mods.walk();
        for m in mods.children(&mut mc) {
            match m.kind() {
                "transient" => is_transient = true,
                "volatile" => is_volatile = true,
                "static" => is_static = true,
                "final" => is_final = true,
                _ => {}
            }
        }
    }

    let field_type = node
        .child_by_field_name("type")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            if let Some(name_node) = child.child_by_field_name("name") {
                fields.push(FieldProfile {
                    name: node_text(name_node, source).to_string(),
                    field_type: field_type.clone(),
                    is_transient,
                    is_volatile,
                    is_static,
                    is_final,
                });
            }
        }
    }

    fields
}

fn extract_param_types(params_node: Node, source: &str) -> Vec<String> {
    let mut types = Vec::new();
    let mut cursor = params_node.walk();
    for child in params_node.children(&mut cursor) {
        if child.kind() == "formal_parameter" || child.kind() == "spread_parameter" {
            if let Some(type_node) = child.child_by_field_name("type") {
                types.push(node_text(type_node, source).to_string());
            }
        }
    }
    types
}

// ── Profile diffing ─────────────────────────────────────────────────────

fn diff_file_profiles(
    old_profiles: &[JavaClassProfile],
    new_profiles: &[JavaClassProfile],
) -> Vec<JavaSourceChange> {
    let mut changes = Vec::new();

    let old_map: HashMap<&str, &JavaClassProfile> = old_profiles
        .iter()
        .map(|p| (p.qualified_name.as_str(), p))
        .collect();
    let new_map: HashMap<&str, &JavaClassProfile> = new_profiles
        .iter()
        .map(|p| (p.qualified_name.as_str(), p))
        .collect();

    // Diff matching classes
    for (qname, old_p) in &old_map {
        if let Some(new_p) = new_map.get(qname) {
            diff_class_profiles(old_p, new_p, &mut changes);
        }
    }

    changes
}

pub fn diff_class_profiles(
    old: &JavaClassProfile,
    new: &JavaClassProfile,
    changes: &mut Vec<JavaSourceChange>,
) {
    let class = &old.qualified_name;

    // ── Annotation changes ──────────────────────────────────────────
    diff_annotations(class, &old.annotations, &new.annotations, None, changes);

    // ── Final/sealed/abstract changes ───────────────────────────────
    if !old.is_final && new.is_final {
        changes.push(JavaSourceChange {
            class_name: class.clone(),
            category: JavaSourceCategory::FinalAdded,
            description: format!("Class `{}` is now final — cannot be extended", old.name),
            old_value: None,
            new_value: Some("final".into()),
            is_breaking: true,
            method: None,
            dependency_chain: None,
        });
    }
    if old.is_final && !new.is_final {
        changes.push(JavaSourceChange {
            class_name: class.clone(),
            category: JavaSourceCategory::FinalRemoved,
            description: format!("Class `{}` is no longer final", old.name),
            old_value: Some("final".into()),
            new_value: None,
            is_breaking: false,
            method: None,
            dependency_chain: None,
        });
    }

    if old.is_sealed != new.is_sealed || old.permits != new.permits {
        changes.push(JavaSourceChange {
            class_name: class.clone(),
            category: JavaSourceCategory::SealedChanged,
            description: if new.is_sealed && !old.is_sealed {
                format!(
                    "Class `{}` is now sealed (permits: {})",
                    old.name,
                    new.permits.join(", ")
                )
            } else if !new.is_sealed && old.is_sealed {
                format!("Class `{}` is no longer sealed", old.name)
            } else {
                format!(
                    "Sealed class `{}` permits changed: [{}] → [{}]",
                    old.name,
                    old.permits.join(", "),
                    new.permits.join(", ")
                )
            },
            old_value: if old.is_sealed {
                Some(old.permits.join(", "))
            } else {
                None
            },
            new_value: if new.is_sealed {
                Some(new.permits.join(", "))
            } else {
                None
            },
            is_breaking: new.is_sealed
                && (!old.is_sealed
                    || old.permits.iter().any(|p| !new.permits.contains(p))),
            method: None,
            dependency_chain: None,
        });
    }

    // ── Inheritance changes ─────────────────────────────────────────
    if old.extends != new.extends {
        changes.push(JavaSourceChange {
            class_name: class.clone(),
            category: JavaSourceCategory::InheritanceChanged,
            description: format!(
                "Class `{}` extends changed: {:?} → {:?}",
                old.name, old.extends, new.extends
            ),
            old_value: old.extends.clone(),
            new_value: new.extends.clone(),
            is_breaking: true,
            method: None,
            dependency_chain: None,
        });
    }

    let old_impls: HashSet<&str> = old.implements.iter().map(|s| s.as_str()).collect();
    let new_impls: HashSet<&str> = new.implements.iter().map(|s| s.as_str()).collect();
    let removed_impls: Vec<&&str> = old_impls.difference(&new_impls).collect();
    if !removed_impls.is_empty() {
        changes.push(JavaSourceChange {
            class_name: class.clone(),
            category: JavaSourceCategory::InheritanceChanged,
            description: format!(
                "Class `{}` no longer implements: {}",
                old.name,
                removed_impls.iter().map(|s| **s).collect::<Vec<_>>().join(", ")
            ),
            old_value: Some(old.implements.join(", ")),
            new_value: Some(new.implements.join(", ")),
            is_breaking: true,
            method: None,
            dependency_chain: None,
        });
    }

    // ── Constructor dependency changes ──────────────────────────────
    if old.constructor_params != new.constructor_params
        && !old.constructor_params.is_empty()
    {
        changes.push(JavaSourceChange {
            class_name: class.clone(),
            category: JavaSourceCategory::ConstructorDependencyChanged,
            description: format!(
                "Constructor of `{}` changed: ({}) → ({})",
                old.name,
                old.constructor_params.join(", "),
                new.constructor_params.join(", "),
            ),
            old_value: Some(old.constructor_params.join(", ")),
            new_value: Some(new.constructor_params.join(", ")),
            is_breaking: true,
            method: None,
            dependency_chain: None,
        });
    }

    // ── Method-level changes ────────────────────────────────────────
    let old_methods: HashMap<&str, &MethodProfile> = old
        .methods
        .iter()
        .map(|m| (m.name.as_str(), m))
        .collect();
    let new_methods: HashMap<&str, &MethodProfile> = new
        .methods
        .iter()
        .map(|m| (m.name.as_str(), m))
        .collect();

    for (name, old_m) in &old_methods {
        if let Some(new_m) = new_methods.get(name) {
            diff_method_profiles(class, old_m, new_m, changes);
        }
    }
}

fn diff_method_profiles(
    class: &str,
    old: &MethodProfile,
    new: &MethodProfile,
    changes: &mut Vec<JavaSourceChange>,
) {
    // Annotation changes on methods
    diff_annotations(
        class,
        &old.annotations,
        &new.annotations,
        Some(&old.name),
        changes,
    );

    // Synchronized removed
    if old.is_synchronized && !new.is_synchronized {
        changes.push(JavaSourceChange {
            class_name: class.to_string(),
            category: JavaSourceCategory::SynchronizationRemoved,
            description: format!(
                "Method `{}` is no longer synchronized — thread safety may be affected",
                old.name
            ),
            old_value: Some("synchronized".into()),
            new_value: None,
            is_breaking: true,
            method: Some(old.name.clone()),
            dependency_chain: None,
        });
    }
    if !old.is_synchronized && new.is_synchronized {
        changes.push(JavaSourceChange {
            class_name: class.to_string(),
            category: JavaSourceCategory::SynchronizationAdded,
            description: format!("Method `{}` is now synchronized", old.name),
            old_value: None,
            new_value: Some("synchronized".into()),
            is_breaking: false,
            method: Some(old.name.clone()),
            dependency_chain: None,
        });
    }

    // Native removed
    if old.is_native && !new.is_native {
        changes.push(JavaSourceChange {
            class_name: class.to_string(),
            category: JavaSourceCategory::NativeRemoved,
            description: format!(
                "Method `{}` is no longer native — JNI consumers will break",
                old.name
            ),
            old_value: Some("native".into()),
            new_value: None,
            is_breaking: true,
            method: Some(old.name.clone()),
            dependency_chain: None,
        });
    }

    // Override removed
    if old.is_override && !new.is_override {
        changes.push(JavaSourceChange {
            class_name: class.to_string(),
            category: JavaSourceCategory::OverrideRemoved,
            description: format!(
                "Method `{}` no longer overrides parent — behavior may revert to parent impl",
                old.name
            ),
            old_value: Some("@Override".into()),
            new_value: None,
            is_breaking: true,
            method: Some(old.name.clone()),
            dependency_chain: None,
        });
    }
    if !old.is_override && new.is_override {
        changes.push(JavaSourceChange {
            class_name: class.to_string(),
            category: JavaSourceCategory::OverrideAdded,
            description: format!("Method `{}` now overrides parent", old.name),
            old_value: None,
            new_value: Some("@Override".into()),
            is_breaking: false,
            method: Some(old.name.clone()),
            dependency_chain: None,
        });
    }

    // Exception changes
    let old_exc: HashSet<&str> = old.thrown_exceptions.iter().map(|s| s.as_str()).collect();
    let new_exc: HashSet<&str> = new.thrown_exceptions.iter().map(|s| s.as_str()).collect();

    for added in new_exc.difference(&old_exc) {
        changes.push(JavaSourceChange {
            class_name: class.to_string(),
            category: JavaSourceCategory::ExceptionAdded,
            description: format!(
                "Method `{}` now throws `{}` — callers must handle this",
                old.name, added
            ),
            old_value: None,
            new_value: Some(added.to_string()),
            is_breaking: true,
            method: Some(old.name.clone()),
            dependency_chain: None,
        });
    }

    for removed in old_exc.difference(&new_exc) {
        changes.push(JavaSourceChange {
            class_name: class.to_string(),
            category: JavaSourceCategory::ExceptionRemoved,
            description: format!(
                "Method `{}` no longer throws `{}`",
                old.name, removed
            ),
            old_value: Some(removed.to_string()),
            new_value: None,
            is_breaking: false,
            method: Some(old.name.clone()),
            dependency_chain: None,
        });
    }

    // Delegation changes (behavioral change detection)
    let old_dels: HashSet<&str> = old.delegations.iter().map(|s| s.as_str()).collect();
    let new_dels: HashSet<&str> = new.delegations.iter().map(|s| s.as_str()).collect();

    if old_dels != new_dels && !old_dels.is_empty() {
        let removed_dels: Vec<&&str> = old_dels.difference(&new_dels).collect();
        let added_dels: Vec<&&str> = new_dels.difference(&old_dels).collect();

        if !removed_dels.is_empty() || !added_dels.is_empty() {
            changes.push(JavaSourceChange {
                class_name: class.to_string(),
                category: JavaSourceCategory::DelegationChanged,
                description: format!(
                    "Method `{}` delegation changed: removed [{}], added [{}]",
                    old.name,
                    removed_dels.iter().map(|s| **s).collect::<Vec<_>>().join(", "),
                    added_dels.iter().map(|s| **s).collect::<Vec<_>>().join(", "),
                ),
                old_value: Some(old.delegations.join(", ")),
                new_value: Some(new.delegations.join(", ")),
                is_breaking: true,
                method: Some(old.name.clone()),
                dependency_chain: None,
            });
        }
    }
}

fn diff_annotations(
    class: &str,
    old_anns: &[ProfileAnnotation],
    new_anns: &[ProfileAnnotation],
    method: Option<&str>,
    changes: &mut Vec<JavaSourceChange>,
) {
    let old_by_name: HashMap<&str, &ProfileAnnotation> = old_anns
        .iter()
        .map(|a| (a.name.as_str(), a))
        .collect();
    let new_by_name: HashMap<&str, &ProfileAnnotation> = new_anns
        .iter()
        .map(|a| (a.name.as_str(), a))
        .collect();

    let target = method.unwrap_or(class.rsplit('.').next().unwrap_or(class));

    for name in old_by_name.keys() {
        if !new_by_name.contains_key(name) {
            changes.push(JavaSourceChange {
                class_name: class.to_string(),
                category: JavaSourceCategory::AnnotationRemoved,
                description: format!("Annotation `@{}` removed from `{}`", name, target),
                old_value: Some(format!("@{}", name)),
                new_value: None,
                is_breaking: true, // Conservative: annotation removal is often breaking
                method: method.map(|m| m.to_string()),
                dependency_chain: None,
            });
        }
    }

    for name in new_by_name.keys() {
        if !old_by_name.contains_key(name) {
            changes.push(JavaSourceChange {
                class_name: class.to_string(),
                category: JavaSourceCategory::AnnotationAdded,
                description: format!("Annotation `@{}` added to `{}`", name, target),
                old_value: None,
                new_value: Some(format!("@{}", name)),
                is_breaking: false,
                method: method.map(|m| m.to_string()),
                dependency_chain: None,
            });
        }
    }

    for (name, old_ann) in &old_by_name {
        if let Some(new_ann) = new_by_name.get(name) {
            if old_ann.attributes != new_ann.attributes {
                changes.push(JavaSourceChange {
                    class_name: class.to_string(),
                    category: JavaSourceCategory::AnnotationChanged,
                    description: format!(
                        "Annotation `@{}` on `{}` changed attributes",
                        name, target
                    ),
                    old_value: Some(format_annotation(old_ann)),
                    new_value: Some(format_annotation(new_ann)),
                    is_breaking: true,
                    method: method.map(|m| m.to_string()),
                    dependency_chain: None,
                });
            }
        }
    }
}

// ── Serialization analysis ──────────────────────────────────────────────

fn resolve_serializable_classes(
    profiles: &HashMap<String, JavaClassProfile>,
) -> HashSet<String> {
    let mut serializable = HashSet::new();
    for p in profiles.values() {
        if p.is_serializable {
            serializable.insert(p.qualified_name.clone());
        }
    }
    // Transitive: if a superclass is Serializable, subclasses are too
    let mut changed = true;
    while changed {
        changed = false;
        for p in profiles.values() {
            if serializable.contains(&p.qualified_name) {
                continue;
            }
            if let Some(ref ext) = p.extends {
                if serializable.contains(ext) {
                    serializable.insert(p.qualified_name.clone());
                    changed = true;
                }
            }
        }
    }
    serializable
}

pub fn diff_serialization(
    old: &JavaClassProfile,
    new: &JavaClassProfile,
) -> Vec<JavaSourceChange> {
    let mut changes = Vec::new();
    let class = &old.qualified_name;

    let old_fields: HashMap<&str, &FieldProfile> = old
        .fields
        .iter()
        .filter(|f| !f.is_static && !f.is_transient)
        .map(|f| (f.name.as_str(), f))
        .collect();
    let new_fields: HashMap<&str, &FieldProfile> = new
        .fields
        .iter()
        .filter(|f| !f.is_static && !f.is_transient)
        .map(|f| (f.name.as_str(), f))
        .collect();

    for name in old_fields.keys() {
        if !new_fields.contains_key(name) {
            changes.push(JavaSourceChange {
                class_name: class.clone(),
                category: JavaSourceCategory::SerializationFieldRemoved,
                description: format!(
                    "Serializable field `{}` removed from `{}` — deserialization may break",
                    name, old.name
                ),
                old_value: Some(name.to_string()),
                new_value: None,
                is_breaking: true,
                method: None,
                dependency_chain: None,
            });
        }
    }

    for name in new_fields.keys() {
        if !old_fields.contains_key(name) {
            changes.push(JavaSourceChange {
                class_name: class.clone(),
                category: JavaSourceCategory::SerializationFieldAdded,
                description: format!(
                    "New serializable field `{}` added to `{}` — old serialized data won't have it",
                    name, old.name
                ),
                old_value: None,
                new_value: Some(name.to_string()),
                is_breaking: true,
                method: None,
                dependency_chain: None,
            });
        }
    }

    for (name, old_f) in &old_fields {
        if let Some(new_f) = new_fields.get(name) {
            if old_f.field_type != new_f.field_type {
                changes.push(JavaSourceChange {
                    class_name: class.clone(),
                    category: JavaSourceCategory::SerializationFieldTypeChanged,
                    description: format!(
                        "Serializable field `{}` type changed: `{}` → `{}`",
                        name, old_f.field_type, new_f.field_type
                    ),
                    old_value: Some(old_f.field_type.clone()),
                    new_value: Some(new_f.field_type.clone()),
                    is_breaking: true,
                    method: None,
                    dependency_chain: None,
                });
            }
        }
    }

    // Transient changes
    let old_all: HashMap<&str, &FieldProfile> = old
        .fields
        .iter()
        .filter(|f| !f.is_static)
        .map(|f| (f.name.as_str(), f))
        .collect();
    let new_all: HashMap<&str, &FieldProfile> = new
        .fields
        .iter()
        .filter(|f| !f.is_static)
        .map(|f| (f.name.as_str(), f))
        .collect();

    for (name, old_f) in &old_all {
        if let Some(new_f) = new_all.get(name) {
            if old_f.is_transient != new_f.is_transient {
                changes.push(JavaSourceChange {
                    class_name: class.clone(),
                    category: JavaSourceCategory::TransientChanged,
                    description: if new_f.is_transient {
                        format!(
                            "Field `{}` is now transient — excluded from serialization",
                            name
                        )
                    } else {
                        format!(
                            "Field `{}` is no longer transient — now included in serialization",
                            name
                        )
                    },
                    old_value: Some(if old_f.is_transient {
                        "transient".into()
                    } else {
                        "non-transient".into()
                    }),
                    new_value: Some(if new_f.is_transient {
                        "transient".into()
                    } else {
                        "non-transient".into()
                    }),
                    is_breaking: true,
                    method: None,
                    dependency_chain: None,
                });
            }
        }
    }

    changes
}

// ── Inheritance summary ─────────────────────────────────────────────────

fn build_inheritance_summary(
    profiles: &HashMap<String, JavaClassProfile>,
) -> Vec<InheritanceEntry> {
    let mut entries = Vec::new();

    for p in profiles.values() {
        let subclasses: Vec<String> = profiles
            .values()
            .filter(|other| other.extends.as_deref() == Some(&p.qualified_name))
            .map(|other| other.qualified_name.clone())
            .collect();

        entries.push(InheritanceEntry {
            class_name: p.qualified_name.clone(),
            extends: p.extends.clone(),
            implements: p.implements.clone(),
            is_final: p.is_final,
            is_sealed: p.is_sealed,
            subclasses,
        });
    }

    entries
}

// ── Module system diff ──────────────────────────────────────────────────

fn diff_modules(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    parser: &mut Parser,
) -> Vec<JavaSourceChange> {
    let mut changes = Vec::new();

    // Find module-info.java files
    let old_module = read_git_file(repo, from_ref, "src/main/java/module-info.java")
        .or_else(|| read_git_file(repo, from_ref, "module-info.java"));
    let new_module = read_git_file(repo, to_ref, "src/main/java/module-info.java")
        .or_else(|| read_git_file(repo, to_ref, "module-info.java"));

    let old_directives = old_module
        .as_deref()
        .map(|s| parse_module_directives(parser, s))
        .unwrap_or_default();
    let new_directives = new_module
        .as_deref()
        .map(|s| parse_module_directives(parser, s))
        .unwrap_or_default();

    let old_set: HashSet<&str> = old_directives.iter().map(|d| d.as_str()).collect();
    let new_set: HashSet<&str> = new_directives.iter().map(|d| d.as_str()).collect();

    for removed in old_set.difference(&new_set) {
        let is_export = removed.starts_with("exports ");
        changes.push(JavaSourceChange {
            class_name: "module-info".to_string(),
            category: if is_export {
                JavaSourceCategory::ModuleExportRemoved
            } else {
                JavaSourceCategory::ModuleRequiresChanged
            },
            description: format!("Module directive removed: `{}`", removed),
            old_value: Some(removed.to_string()),
            new_value: None,
            is_breaking: is_export, // Removing exports is breaking
            method: None,
            dependency_chain: None,
        });
    }

    for added in new_set.difference(&old_set) {
        let is_export = added.starts_with("exports ");
        changes.push(JavaSourceChange {
            class_name: "module-info".to_string(),
            category: if is_export {
                JavaSourceCategory::ModuleExportAdded
            } else {
                JavaSourceCategory::ModuleRequiresChanged
            },
            description: format!("Module directive added: `{}`", added),
            old_value: None,
            new_value: Some(added.to_string()),
            is_breaking: false,
            method: None,
            dependency_chain: None,
        });
    }

    changes
}

fn parse_module_directives(parser: &mut Parser, source: &str) -> Vec<String> {
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let mut directives = Vec::new();
    collect_directives(tree.root_node(), source, &mut directives);
    directives
}

fn collect_directives(node: Node, source: &str, directives: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "requires_module_directive"
            | "exports_module_directive"
            | "opens_module_directive"
            | "provides_module_directive"
            | "uses_module_directive" => {
                let text = node_text(child, source)
                    .trim()
                    .trim_end_matches(';')
                    .trim()
                    .to_string();
                directives.push(text);
            }
            _ => {
                collect_directives(child, source, directives);
            }
        }
    }
}

// ── Full extraction helpers ─────────────────────────────────────────────

fn extract_all_profiles(
    parser: &mut Parser,
    worktree: &Path,
) -> Result<HashMap<String, JavaClassProfile>> {
    let mut profiles = HashMap::new();
    extract_profiles_recursive(parser, worktree, worktree, &mut profiles)?;
    Ok(profiles)
}

fn extract_profiles_recursive(
    parser: &mut Parser,
    root: &Path,
    dir: &Path,
    profiles: &mut HashMap<String, JavaClassProfile>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            if name_str == "target"
                || name_str == "build"
                || name_str == "node_modules"
                || name_str == "test"
                || name_str == "tests"
            {
                continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let rel_str = rel.to_string_lossy();
            if rel_str.contains("/src/test/") || rel_str.starts_with("src/test/") {
                continue;
            }
            extract_profiles_recursive(parser, root, &path, profiles)?;
        } else if name_str.ends_with(".java")
            && name_str != "package-info.java"
            && name_str != "module-info.java"
        {
            let source = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let file_profiles = extract_profiles(parser, &source, &rel);
            for p in file_profiles {
                profiles.insert(p.qualified_name.clone(), p);
            }
        }
    }

    Ok(())
}

fn extract_all_profiles_from_git(
    parser: &mut Parser,
    repo: &Path,
    git_ref: &str,
) -> Result<HashMap<String, JavaClassProfile>> {
    // List all Java files at the ref
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", git_ref, "--", "*.java"])
        .current_dir(repo)
        .output()
        .context("Failed to run git ls-tree")?;

    if !output.status.success() {
        return Ok(HashMap::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut profiles = HashMap::new();

    for file_path in stdout.lines() {
        if file_path.is_empty()
            || is_test_file(file_path)
            || file_path.ends_with("package-info.java")
            || file_path.ends_with("module-info.java")
        {
            continue;
        }

        if let Some(source) = read_git_file(repo, git_ref, file_path) {
            let file_profiles = extract_profiles(parser, &source, file_path);
            for p in file_profiles {
                profiles.insert(p.qualified_name.clone(), p);
            }
        }
    }

    Ok(profiles)
}

// ── Tree-sitter helpers ─────────────────────────────────────────────────

fn node_text<'a>(node: Node, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

#[allow(clippy::manual_find)]
fn find_child_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

fn extract_package(root: Node, source: &str) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "package_declaration" {
            let mut inner = child.walk();
            for pkg_child in child.children(&mut inner) {
                if pkg_child.kind() == "scoped_identifier" || pkg_child.kind() == "identifier" {
                    return Some(node_text(pkg_child, source).to_string());
                }
            }
        }
    }
    None
}

fn extract_import_set(root: Node, source: &str) -> HashSet<String> {
    let mut imports = HashSet::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "import_declaration" {
            let text = node_text(child, source);
            let trimmed = text
                .trim_start_matches("import ")
                .trim_start_matches("static ")
                .trim_end_matches(';')
                .trim();
            imports.insert(trimmed.to_string());
        }
    }
    imports
}

fn extract_type_list(node: Node, source: &str) -> Vec<String> {
    let mut types = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "type_identifier" | "scoped_type_identifier" | "generic_type" => {
                types.push(node_text(child, source).to_string());
            }
            "type_list" => {
                types.append(&mut extract_type_list(child, source));
            }
            _ => {}
        }
    }
    types
}

fn parse_profile_annotation(node: Node, source: &str) -> Option<ProfileAnnotation> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source).to_string();

    let mut attributes = Vec::new();
    if let Some(args) = node.child_by_field_name("arguments") {
        let mut cursor = args.walk();
        for child in args.children(&mut cursor) {
            match child.kind() {
                "element_value_pair" => {
                    let key = child
                        .child_by_field_name("key")
                        .map(|n| node_text(n, source).to_string())
                        .unwrap_or_else(|| "value".into());
                    let value = child
                        .child_by_field_name("value")
                        .map(|n| node_text(n, source).to_string())
                        .unwrap_or_default();
                    attributes.push((key, value));
                }
                _ if child.kind() != "(" && child.kind() != ")" => {
                    let value = node_text(child, source).to_string();
                    if !value.is_empty() && value != "(" && value != ")" {
                        attributes.push(("value".into(), value));
                    }
                }
                _ => {}
            }
        }
    }

    Some(ProfileAnnotation {
        name,
        qualified_name: None,
        attributes,
    })
}

fn format_annotation(ann: &ProfileAnnotation) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_profiles_basic() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();

        let source = r#"
            package com.example;
            import java.io.Serializable;
            public class Foo implements Serializable {
                private String name;
                private transient int count;
                @Override
                public String toString() { return name; }
                public synchronized void update(String name) {
                    this.name = name;
                }
            }
        "#;

        let profiles = extract_profiles(&mut parser, source, "Foo.java");
        assert_eq!(profiles.len(), 1);

        let p = &profiles[0];
        assert_eq!(p.qualified_name, "com.example.Foo");
        assert!(p.is_serializable);
        assert_eq!(p.fields.len(), 2);
        assert_eq!(p.methods.len(), 2);

        let update = p.methods.iter().find(|m| m.name == "update").unwrap();
        assert!(update.is_synchronized);

        let to_string = p.methods.iter().find(|m| m.name == "toString").unwrap();
        assert!(to_string.is_override);

        let transient_field = p.fields.iter().find(|f| f.name == "count").unwrap();
        assert!(transient_field.is_transient);
    }

    #[test]
    fn test_diff_profiles_annotation_removed() {
        let old = JavaClassProfile {
            qualified_name: "com.example.Foo".into(),
            name: "Foo".into(),
            annotations: vec![ProfileAnnotation {
                name: "Deprecated".into(),
                qualified_name: None,
                attributes: vec![],
            }],
            ..Default::default()
        };
        let new = JavaClassProfile {
            qualified_name: "com.example.Foo".into(),
            name: "Foo".into(),
            ..Default::default()
        };

        let mut changes = Vec::new();
        diff_class_profiles(&old, &new, &mut changes);

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].category, JavaSourceCategory::AnnotationRemoved);
        assert!(changes[0].is_breaking);
    }

    #[test]
    fn test_diff_profiles_synchronized_removed() {
        let old = JavaClassProfile {
            qualified_name: "com.example.Foo".into(),
            name: "Foo".into(),
            methods: vec![MethodProfile {
                name: "update".into(),
                qualified_name: "com.example.Foo.update".into(),
                is_synchronized: true,
                ..default_method()
            }],
            ..Default::default()
        };
        let new = JavaClassProfile {
            qualified_name: "com.example.Foo".into(),
            name: "Foo".into(),
            methods: vec![MethodProfile {
                name: "update".into(),
                qualified_name: "com.example.Foo.update".into(),
                is_synchronized: false,
                ..default_method()
            }],
            ..Default::default()
        };

        let mut changes = Vec::new();
        diff_class_profiles(&old, &new, &mut changes);

        assert!(changes
            .iter()
            .any(|c| c.category == JavaSourceCategory::SynchronizationRemoved));
    }

    #[test]
    fn test_diff_serialization_field_removed() {
        let old = JavaClassProfile {
            qualified_name: "com.example.Data".into(),
            name: "Data".into(),
            is_serializable: true,
            fields: vec![
                FieldProfile {
                    name: "name".into(),
                    field_type: "String".into(),
                    is_transient: false,
                    is_volatile: false,
                    is_static: false,
                    is_final: false,
                },
                FieldProfile {
                    name: "age".into(),
                    field_type: "int".into(),
                    is_transient: false,
                    is_volatile: false,
                    is_static: false,
                    is_final: false,
                },
            ],
            ..Default::default()
        };
        let new = JavaClassProfile {
            qualified_name: "com.example.Data".into(),
            name: "Data".into(),
            is_serializable: true,
            fields: vec![FieldProfile {
                name: "name".into(),
                field_type: "String".into(),
                is_transient: false,
                is_volatile: false,
                is_static: false,
                is_final: false,
            }],
            ..Default::default()
        };

        let changes = diff_serialization(&old, &new);
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].category,
            JavaSourceCategory::SerializationFieldRemoved
        );
        assert!(changes[0].is_breaking);
    }

    fn default_method() -> MethodProfile {
        MethodProfile {
            name: String::new(),
            qualified_name: String::new(),
            is_synchronized: false,
            is_native: false,
            is_override: false,
            is_default: false,
            is_abstract: false,
            thrown_exceptions: Vec::new(),
            annotations: Vec::new(),
            delegations: Vec::new(),
            return_type: None,
            param_types: Vec::new(),
        }
    }
}
