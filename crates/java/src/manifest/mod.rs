//! Java manifest parsing (pom.xml, build.gradle).
//!
//! Analyzes structural changes to build manifests between two versions.
//! Supports Maven (`pom.xml`) via quick-xml and Gradle (`build.gradle`,
//! `build.gradle.kts`) via regex-based extraction.

mod pom;

use crate::language::Java;
use crate::types::JavaManifestChangeType;
use semver_analyzer_core::ManifestChange;

/// Diff two manifest file contents (auto-detects format).
///
/// Called by `Language::diff_manifest_content` with the raw file content
/// at both refs. Detects the format from content structure.
pub fn diff_manifest_content(old: &str, new: &str) -> Vec<ManifestChange<Java>> {
    // Try pom.xml first (XML content)
    if old.trim_start().starts_with("<?xml") || old.trim_start().starts_with("<project") {
        return pom::diff_pom(old, new);
    }

    // Try build.gradle (Groovy/Kotlin DSL)
    if looks_like_gradle(old) || looks_like_gradle(new) {
        return diff_gradle(old, new);
    }

    Vec::new()
}

/// Basic check for Gradle file content.
fn looks_like_gradle(content: &str) -> bool {
    content.contains("dependencies {")
        || content.contains("plugins {")
        || content.contains("apply plugin:")
        || content.contains("implementation(")
        || content.contains("implementation '")
        || content.contains("implementation \"")
}

// ── Gradle diff (regex-based) ───────────────────────────────────────────

/// Simple regex-based extraction for Gradle dependency changes.
///
/// This is intentionally basic — Gradle files are full Groovy/Kotlin programs
/// and can't be fully parsed without a Groovy/Kotlin parser. We extract
/// the most common dependency declaration patterns.
fn diff_gradle(old: &str, new: &str) -> Vec<ManifestChange<Java>> {
    let old_deps = extract_gradle_dependencies(old);
    let new_deps = extract_gradle_dependencies(new);
    let mut changes = Vec::new();

    // Dependencies removed
    for (key, old_dep) in &old_deps {
        if !new_deps.contains_key(key) {
            changes.push(ManifestChange {
                field: format!("dependency:{}", key),
                change_type: JavaManifestChangeType::DependencyRemoved,
                before: Some(old_dep.clone()),
                after: None,
                description: format!("Dependency `{}` was removed", key),
                is_breaking: true,
            });
        }
    }

    // Dependencies added
    for (key, new_dep) in &new_deps {
        if !old_deps.contains_key(key) {
            changes.push(ManifestChange {
                field: format!("dependency:{}", key),
                change_type: JavaManifestChangeType::DependencyAdded,
                before: None,
                after: Some(new_dep.clone()),
                description: format!("Dependency `{}` was added", key),
                is_breaking: false,
            });
        }
    }

    // Dependencies with changed versions
    for (key, old_dep) in &old_deps {
        if let Some(new_dep) = new_deps.get(key) {
            if old_dep != new_dep {
                changes.push(ManifestChange {
                    field: format!("dependency:{}", key),
                    change_type: JavaManifestChangeType::DependencyVersionChanged,
                    before: Some(old_dep.clone()),
                    after: Some(new_dep.clone()),
                    description: format!(
                        "Dependency `{}` changed: `{}` → `{}`",
                        key, old_dep, new_dep
                    ),
                    is_breaking: false,
                });
            }
        }
    }

    // Group/version changes
    let old_group = extract_gradle_property(old, "group");
    let new_group = extract_gradle_property(new, "group");
    if old_group != new_group {
        if let (Some(o), Some(n)) = (&old_group, &new_group) {
            changes.push(ManifestChange {
                field: "group".into(),
                change_type: JavaManifestChangeType::ProjectIdentityChanged,
                before: Some(o.clone()),
                after: Some(n.clone()),
                description: format!("Project group changed from `{}` to `{}`", o, n),
                is_breaking: true,
            });
        }
    }

    let old_version = extract_gradle_property(old, "version");
    let new_version = extract_gradle_property(new, "version");
    if old_version != new_version {
        if let (Some(o), Some(n)) = (&old_version, &new_version) {
            changes.push(ManifestChange {
                field: "version".into(),
                change_type: JavaManifestChangeType::ProjectIdentityChanged,
                before: Some(o.clone()),
                after: Some(n.clone()),
                description: format!("Project version changed from `{}` to `{}`", o, n),
                is_breaking: false,
            });
        }
    }

    changes
}

/// Extract dependencies from Gradle file content.
///
/// Matches patterns like:
/// - `implementation 'group:artifact:version'`
/// - `implementation "group:artifact:version"`
/// - `implementation("group:artifact:version")`
/// - `api 'group:artifact:version'`
/// - `compileOnly 'group:artifact:version'`
fn extract_gradle_dependencies(content: &str) -> std::collections::HashMap<String, String> {
    let mut deps = std::collections::HashMap::new();

    let dep_pattern = regex::Regex::new(
        r#"(?:implementation|api|compileOnly|runtimeOnly|testImplementation|annotationProcessor)\s*[\(]?\s*['"]([^'"]+)['"]"#
    ).unwrap();

    for cap in dep_pattern.captures_iter(content) {
        let dep_str = &cap[1];
        // Parse group:artifact:version
        let parts: Vec<&str> = dep_str.split(':').collect();
        if parts.len() >= 2 {
            let key = format!("{}:{}", parts[0], parts[1]);
            deps.insert(key, dep_str.to_string());
        }
    }

    deps
}

/// Extract a top-level property assignment from Gradle content.
fn extract_gradle_property(content: &str, property: &str) -> Option<String> {
    // Match: group = 'value' or group = "value" or group 'value'
    let pattern = regex::Regex::new(&format!(
        r#"{}[\s=]+['"]([^'"]+)['"]"#,
        regex::escape(property)
    ))
    .ok()?;

    pattern.captures(content).map(|cap| cap[1].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(deps.contains_key("com.google.guava:guava"));
        assert!(deps.contains_key("org.junit.jupiter:junit-jupiter"));
    }

    #[test]
    fn test_gradle_dependency_diff() {
        let old = r#"
            dependencies {
                implementation 'org.springframework.boot:spring-boot-starter-web:3.2.0'
                implementation 'com.fasterxml.jackson.core:jackson-databind:2.15.0'
            }
        "#;
        let new = r#"
            dependencies {
                implementation 'org.springframework.boot:spring-boot-starter-webmvc:4.0.0'
                implementation 'tools.jackson:jackson-databind:3.0.0'
            }
        "#;
        let changes = diff_gradle(old, new);
        // spring-boot-starter-web removed, spring-boot-starter-webmvc added
        // jackson group changed
        assert!(!changes.is_empty());
        assert!(changes
            .iter()
            .any(|c| matches!(c.change_type, JavaManifestChangeType::DependencyRemoved)));
        assert!(changes
            .iter()
            .any(|c| matches!(c.change_type, JavaManifestChangeType::DependencyAdded)));
    }

    #[test]
    fn test_gradle_group_version_extraction() {
        let content = r#"
            group = 'org.springframework.boot'
            version = '3.2.0'
        "#;
        assert_eq!(
            extract_gradle_property(content, "group"),
            Some("org.springframework.boot".into())
        );
        assert_eq!(
            extract_gradle_property(content, "version"),
            Some("3.2.0".into())
        );
    }

    #[test]
    fn test_manifest_format_detection() {
        let pom = r#"<?xml version="1.0"?><project></project>"#;
        let gradle = r#"dependencies { implementation 'foo:bar:1.0' }"#;
        let unknown = "some random text";

        // Should not panic on any format
        assert!(diff_manifest_content(pom, pom).is_empty());
        let _ = diff_manifest_content(gradle, gradle);
        assert!(diff_manifest_content(unknown, unknown).is_empty());
    }
}
