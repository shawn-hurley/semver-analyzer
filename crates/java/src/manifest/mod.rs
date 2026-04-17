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
}

fn diff_gradle(old: &str, new: &str) -> Vec<ManifestChange<Java>> {
    let old_deps = extract_gradle_dependencies(old);
    let new_deps = extract_gradle_dependencies(new);
    let mut changes = Vec::new();

    for (key, old_dep) in &old_deps {
        if !new_deps.contains_key(key) {
            changes.push(ManifestChange {
                field: format!("dependency:{}", key),
                change_type: JavaManifestChangeType::DependencyRemoved,
                before: Some(old_dep.clone()),
                after: None,
                description: format!("Dependency `{}` was removed", key),
                is_breaking: true,
                source_package: None,
            });
        }
    }

    for (key, new_dep) in &new_deps {
        if !old_deps.contains_key(key) {
            changes.push(ManifestChange {
                field: format!("dependency:{}", key),
                change_type: JavaManifestChangeType::DependencyAdded,
                before: None,
                after: Some(new_dep.clone()),
                description: format!("Dependency `{}` was added", key),
                is_breaking: false,
                source_package: None,
            });
        }
    }

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

fn extract_gradle_dependencies(content: &str) -> std::collections::HashMap<String, String> {
    let mut deps = std::collections::HashMap::new();

    let dep_pattern = regex::Regex::new(
        r#"(?:implementation|api|compileOnly|runtimeOnly|testImplementation|annotationProcessor)\s*[\(]?\s*['"]([^'"]+)['"]"#
    ).unwrap();

    for cap in dep_pattern.captures_iter(content) {
        let dep_str = &cap[1];
        let parts: Vec<&str> = dep_str.split(':').collect();
        if parts.len() >= 2 {
            let key = format!("{}:{}", parts[0], parts[1]);
            deps.insert(key, dep_str.to_string());
        }
    }

    deps
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
    }

    #[test]
    fn test_manifest_format_detection() {
        let pom = r#"<?xml version="1.0"?><project></project>"#;
        assert!(diff_manifest_content(pom, pom).is_empty());
    }
}
