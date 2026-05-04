//! Java manifest parsing (pom.xml, build.gradle).

mod pom;

use crate::language::Java;
use crate::types::JavaManifestChangeType;
use semver_analyzer_core::ManifestChange;

/// Diff two manifest file contents (auto-detects format).
pub fn diff_manifest_content(old: &str, new: &str) -> Vec<ManifestChange<Java>> {
    if old.trim_start().starts_with("<?xml") || old.trim_start().starts_with("<project") {
        return pom::diff_pom(old, new);
    }

    if looks_like_gradle(old) || looks_like_gradle(new) {
        return diff_gradle(old, new);
    }

    Vec::new()
}

fn looks_like_gradle(content: &str) -> bool {
    content.contains("dependencies {")
        || content.contains("plugins {")
        || content.contains("apply plugin:")
        || content.contains("implementation(")
        || content.contains("implementation '")
        || content.contains("implementation \"")
        || content.contains("libraries")
        || content.contains("ext {")
        || content.contains("ext[")
        || content.contains("group = ")
        || content.contains("group=")
}

fn diff_gradle(old: &str, new: &str) -> Vec<ManifestChange<Java>> {
    let old_deps = extract_gradle_dependencies(old);
    let new_deps = extract_gradle_dependencies(new);
    let mut changes = Vec::new();

    // Detect group ID changes (e.g., org.hibernate → org.hibernate.orm)
    let old_group = extract_gradle_group(old);
    let new_group = extract_gradle_group(new);
    if let (Some(ref old_g), Some(ref new_g)) = (&old_group, &new_group) {
        if old_g != new_g {
            changes.push(ManifestChange {
                field: "project:group".into(),
                change_type: JavaManifestChangeType::ProjectIdentityChanged,
                before: Some(old_g.clone()),
                after: Some(new_g.clone()),
                description: format!(
                    "Project group ID changed: `{}` → `{}`",
                    old_g, new_g
                ),
                is_breaking: true,
                source_package: None,
            });
        }
    }

    let mut removed: Vec<(&String, &String)> = Vec::new();
    let mut added: Vec<(&String, &String)> = Vec::new();

    for (key, old_dep) in &old_deps {
        if !new_deps.contains_key(key) {
            removed.push((key, old_dep));
        }
    }

    for (key, new_dep) in &new_deps {
        if !old_deps.contains_key(key) {
            added.push((key, new_dep));
        }
    }

    // Detect coordinate renames: match removed deps to added deps by artifact
    // name similarity. This catches javax→jakarta migrations where both the
    // groupId and artifactId change.
    let mut renamed_old: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut renamed_new: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (old_key, old_dep) in &removed {
        let old_artifact = old_key.rsplit(':').next().unwrap_or(old_key);
        for (new_key, new_dep) in &added {
            if renamed_new.contains(*new_key) {
                continue;
            }
            let new_artifact = new_key.rsplit(':').next().unwrap_or(new_key);
            if artifacts_are_related(old_artifact, new_artifact) {
                changes.push(ManifestChange {
                    field: format!("dependency:{}", old_key),
                    change_type: JavaManifestChangeType::DependencyCoordinateChanged,
                    before: Some((*old_dep).clone()),
                    after: Some((*new_dep).clone()),
                    description: format!(
                        "Dependency coordinate changed: `{}` → `{}`",
                        old_key, new_key
                    ),
                    is_breaking: true,
                    source_package: None,
                });
                renamed_old.insert((*old_key).clone());
                renamed_new.insert((*new_key).clone());
                break;
            }
        }
    }

    // Remaining removals (not matched to a rename)
    for (key, old_dep) in &removed {
        if !renamed_old.contains(*key) {
            changes.push(ManifestChange {
                field: format!("dependency:{}", key),
                change_type: JavaManifestChangeType::DependencyRemoved,
                before: Some((*old_dep).clone()),
                after: None,
                description: format!("Dependency `{}` was removed", key),
                is_breaking: true,
                source_package: None,
            });
        }
    }

    // Remaining additions (not matched to a rename)
    for (key, new_dep) in &added {
        if !renamed_new.contains(*key) {
            changes.push(ManifestChange {
                field: format!("dependency:{}", key),
                change_type: JavaManifestChangeType::DependencyAdded,
                before: None,
                after: Some((*new_dep).clone()),
                description: format!("Dependency `{}` was added", key),
                is_breaking: false,
                source_package: None,
            });
        }
    }

    // Version changes (same key, different value)
    for (key, old_dep) in &old_deps {
        if let Some(new_dep) = new_deps.get(key) {
            if old_dep != new_dep {
                changes.push(ManifestChange {
                    field: format!("dependency:{}", key),
                    change_type: JavaManifestChangeType::DependencyVersionChanged,
                    before: Some(old_dep.clone()),
                    after: Some(new_dep.clone()),
                    description: format!(
                        "Dependency `{}` changed: `{}` -> `{}`",
                        key, old_dep, new_dep
                    ),
                    is_breaking: false,
                    source_package: None,
                });
            }
        }
    }

    changes
}

/// Check if two artifact IDs are likely the same dependency under different
/// coordinates (e.g., javax→jakarta migration).
/// Used by both Gradle and POM differs.
///
/// Matching heuristics:
/// - Strip `javax.`/`jakarta.` prefix and compare
/// - Both end with `-api` and share a common root
pub(crate) fn artifacts_are_related(old_artifact: &str, new_artifact: &str) -> bool {
    if old_artifact == new_artifact {
        return true;
    }

    // Strip javax./jakarta. prefixes and compare
    let old_stripped = old_artifact
        .strip_prefix("javax.")
        .or_else(|| old_artifact.strip_prefix("jakarta."))
        .unwrap_or(old_artifact);
    let new_stripped = new_artifact
        .strip_prefix("javax.")
        .or_else(|| new_artifact.strip_prefix("jakarta."))
        .unwrap_or(new_artifact);

    if old_stripped == new_stripped && !old_stripped.is_empty() {
        return true;
    }

    // Both end with -api and share a root after stripping common suffixes
    if old_artifact.ends_with("-api") && new_artifact.ends_with("-api") {
        let old_root = old_artifact.trim_end_matches("-api");
        let new_root = new_artifact.trim_end_matches("-api");
        let old_root_stripped = old_root
            .strip_prefix("javax.")
            .or_else(|| old_root.strip_prefix("jakarta."))
            .unwrap_or(old_root);
        let new_root_stripped = new_root
            .strip_prefix("javax.")
            .or_else(|| new_root.strip_prefix("jakarta."))
            .unwrap_or(new_root);
        if old_root_stripped == new_root_stripped && !old_root_stripped.is_empty() {
            return true;
        }
    }

    false
}

/// Extract the `group = '...'` property from a Gradle file.
///
/// Searches for patterns like:
/// - `group = 'org.hibernate'`
/// - `group = "org.hibernate.orm"`
/// - `allprojects { group = 'org.hibernate' }`
fn extract_gradle_group(content: &str) -> Option<String> {
    let re = regex::Regex::new(r#"group\s*=\s*['"]([^'"]+)['"]"#).ok()?;
    // Return the last match (innermost scope wins — allprojects overrides root)
    let mut group = None;
    for cap in re.captures_iter(content) {
        group = Some(cap[1].to_string());
    }
    group
}

fn extract_gradle_dependencies(content: &str) -> std::collections::HashMap<String, String> {
    let mut deps = std::collections::HashMap::new();

    // Pattern 1: Direct dependency declarations
    // e.g., implementation 'org.foo:bar:1.0'
    let dep_pattern = regex::Regex::new(
        r#"(?:implementation|api|compileOnly|runtimeOnly|testImplementation|annotationProcessor|compile|providedCompile)\s*[\(]?\s*['"]([^'"]+)['"]"#
    ).unwrap();

    for cap in dep_pattern.captures_iter(content) {
        let dep_str = &cap[1];
        // Skip property references that aren't resolved
        if dep_str.contains("${") {
            continue;
        }
        let parts: Vec<&str> = dep_str.split(':').collect();
        if parts.len() >= 2 {
            let key = format!("{}:{}", parts[0], parts[1]);
            deps.insert(key, dep_str.to_string());
        }
    }

    // Pattern 2: Library map declarations
    // e.g., jpa: "javax.persistence:javax.persistence-api:2.2",
    // e.g., libraries["hibernate"] = "org.hibernate:hibernate-core:5.6.15"
    let lib_pattern = regex::Regex::new(
        r#"['"]([^'"]+:[^'"]+:[^'"]+)['"]"#
    ).unwrap();

    for cap in lib_pattern.captures_iter(content) {
        let dep_str = &cap[1];
        if dep_str.contains("${") {
            continue;
        }
        let parts: Vec<&str> = dep_str.split(':').collect();
        if parts.len() >= 3 {
            let key = format!("{}:{}", parts[0], parts[1]);
            // Don't overwrite direct dependency declarations
            deps.entry(key).or_insert_with(|| dep_str.to_string());
        }
    }

    deps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gradle_group_extraction() {
        let content = r#"
            allprojects {
                group = 'org.hibernate'
                version = project.ormVersion.fullName
            }
        "#;
        assert_eq!(extract_gradle_group(content), Some("org.hibernate".into()));

        let content2 = r#"
            group = 'org.hibernate.orm'
            version = project.ormVersion.fullName
        "#;
        assert_eq!(extract_gradle_group(content2), Some("org.hibernate.orm".into()));
    }

    #[test]
    fn test_gradle_group_change_detection() {
        let old = r#"
            allprojects {
                group = 'org.hibernate'
            }
        "#;
        let new = r#"
            group = 'org.hibernate.orm'
        "#;
        let changes = diff_gradle(old, new);
        let group_change = changes.iter().find(|c| c.field == "project:group");
        assert!(group_change.is_some(), "Should detect group ID change");
        let gc = group_change.unwrap();
        assert_eq!(gc.before.as_deref(), Some("org.hibernate"));
        assert_eq!(gc.after.as_deref(), Some("org.hibernate.orm"));
        assert!(matches!(gc.change_type, JavaManifestChangeType::ProjectIdentityChanged));
    }

    #[test]
    fn test_gradle_dependency_extraction() {
        let content = r#"
            dependencies {
                implementation 'org.springframework.boot:spring-boot-starter-web:3.2.0'
                api "com.google.guava:guava:32.1.2-jre"
                testImplementation("org.junit.jupiter:junit-jupiter:5.10.0")
            }
        "#;
        let deps = extract_gradle_dependencies(content);
        assert_eq!(deps.len(), 3);
        assert!(deps.contains_key("org.springframework.boot:spring-boot-starter-web"));
    }

    #[test]
    fn test_manifest_format_detection() {
        let pom = r#"<?xml version="1.0"?><project></project>"#;
        assert!(diff_manifest_content(pom, pom).is_empty());
    }
}
