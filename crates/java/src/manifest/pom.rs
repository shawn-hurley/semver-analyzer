//! Maven `pom.xml` manifest diff engine.

use crate::language::Java;
use crate::types::JavaManifestChangeType;
use quick_xml::events::Event;
use quick_xml::Reader;
use semver_analyzer_core::ManifestChange;
use std::collections::HashMap;

pub fn diff_pom(old: &str, new: &str) -> Vec<ManifestChange<Java>> {
    let old_pom = match parse_pom(old) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let new_pom = match parse_pom(new) {
        Some(p) => p,
        None => return Vec::new(),
    };

    let mut changes = Vec::new();
    diff_project_identity(&old_pom, &new_pom, &mut changes);
    diff_parent(&old_pom, &new_pom, &mut changes);
    diff_dependencies(&old_pom.dependencies, &new_pom.dependencies, &mut changes);
    diff_properties(&old_pom.properties, &new_pom.properties, &mut changes);
    changes
}

#[derive(Debug, Default)]
struct PomData {
    group_id: Option<String>,
    artifact_id: Option<String>,
    version: Option<String>,
    parent_group_id: Option<String>,
    parent_artifact_id: Option<String>,
    parent_version: Option<String>,
    dependencies: Vec<PomDependency>,
    properties: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct PomDependency {
    group_id: String,
    artifact_id: String,
    version: Option<String>,
    scope: Option<String>,
}

impl PomDependency {
    fn key(&self) -> String {
        format!("{}:{}", self.group_id, self.artifact_id)
    }
}

fn parse_pom(content: &str) -> Option<PomData> {
    let mut reader = Reader::from_str(content);
    let mut buf = Vec::new();
    let mut pom = PomData::default();

    let mut path: Vec<String> = Vec::new();
    let mut current_dep: Option<PomDependencyBuilder> = None;
    let mut in_dependency_management = false;
    let mut text_buf = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                path.push(name.clone());

                if name == "dependencyManagement" {
                    in_dependency_management = true;
                }
                if name == "dependency" && !in_dependency_management {
                    current_dep = Some(PomDependencyBuilder::default());
                }
                text_buf.clear();
            }
            Ok(Event::End(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();

                if name == "dependencyManagement" {
                    in_dependency_management = false;
                }

                let text = text_buf.trim().to_string();
                let depth = path.len();

                if path_matches(&path, &["project", "groupId"]) && name == "groupId" && depth == 2 {
                    pom.group_id = some_if_nonempty(&text);
                }
                if path_matches(&path, &["project", "artifactId"])
                    && name == "artifactId"
                    && depth == 2
                {
                    pom.artifact_id = some_if_nonempty(&text);
                }
                if path_matches(&path, &["project", "version"]) && name == "version" && depth == 2 {
                    pom.version = some_if_nonempty(&text);
                }

                if path_matches(&path, &["project", "parent", "groupId"]) && name == "groupId" {
                    pom.parent_group_id = some_if_nonempty(&text);
                }
                if path_matches(&path, &["project", "parent", "artifactId"]) && name == "artifactId"
                {
                    pom.parent_artifact_id = some_if_nonempty(&text);
                }
                if path_matches(&path, &["project", "parent", "version"]) && name == "version" {
                    pom.parent_version = some_if_nonempty(&text);
                }

                if path.len() == 3
                    && path.first().map(|s| s.as_str()) == Some("project")
                    && path.get(1).map(|s| s.as_str()) == Some("properties")
                    && !text.is_empty()
                {
                    pom.properties.insert(name.clone(), text.clone());
                }

                if let Some(ref mut dep) = current_dep {
                    match name.as_str() {
                        "groupId" => dep.group_id = some_if_nonempty(&text),
                        "artifactId" => dep.artifact_id = some_if_nonempty(&text),
                        "version" => dep.version = some_if_nonempty(&text),
                        "scope" => dep.scope = some_if_nonempty(&text),
                        "dependency" => {
                            if let Some(finished) = dep.build() {
                                pom.dependencies.push(finished);
                            }
                            current_dep = None;
                        }
                        _ => {}
                    }
                }

                path.pop();
                text_buf.clear();
            }
            Ok(Event::Text(ref e)) => {
                if let Ok(t) = e.unescape() {
                    text_buf.push_str(&t);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                tracing::warn!(error = %e, "Error parsing pom.xml");
                return None;
            }
            _ => {}
        }
        buf.clear();
    }

    Some(pom)
}

#[derive(Debug, Default)]
struct PomDependencyBuilder {
    group_id: Option<String>,
    artifact_id: Option<String>,
    version: Option<String>,
    scope: Option<String>,
}

impl PomDependencyBuilder {
    fn build(&self) -> Option<PomDependency> {
        Some(PomDependency {
            group_id: self.group_id.clone()?,
            artifact_id: self.artifact_id.clone()?,
            version: self.version.clone(),
            scope: self.scope.clone(),
        })
    }
}

fn some_if_nonempty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn path_matches(path: &[String], expected: &[&str]) -> bool {
    if path.len() < expected.len() {
        return false;
    }
    let start = path.len() - expected.len();
    path[start..]
        .iter()
        .zip(expected.iter())
        .all(|(a, b)| a == b)
}

fn diff_project_identity(old: &PomData, new: &PomData, changes: &mut Vec<ManifestChange<Java>>) {
    if old.group_id != new.group_id {
        changes.push(ManifestChange {
            field: "groupId".into(),
            change_type: JavaManifestChangeType::ProjectIdentityChanged,
            before: old.group_id.clone(),
            after: new.group_id.clone(),
            description: format!(
                "groupId changed from `{}` to `{}`",
                old.group_id.as_deref().unwrap_or("(none)"),
                new.group_id.as_deref().unwrap_or("(none)")
            ),
            is_breaking: true,
            source_package: None,
        });
    }

    if old.artifact_id != new.artifact_id {
        changes.push(ManifestChange {
            field: "artifactId".into(),
            change_type: JavaManifestChangeType::ProjectIdentityChanged,
            before: old.artifact_id.clone(),
            after: new.artifact_id.clone(),
            description: format!(
                "artifactId changed from `{}` to `{}`",
                old.artifact_id.as_deref().unwrap_or("(none)"),
                new.artifact_id.as_deref().unwrap_or("(none)")
            ),
            is_breaking: true,
            source_package: None,
        });
    }

    if old.version != new.version {
        changes.push(ManifestChange {
            field: "version".into(),
            change_type: JavaManifestChangeType::ProjectIdentityChanged,
            before: old.version.clone(),
            after: new.version.clone(),
            description: format!(
                "Project version changed from `{}` to `{}`",
                old.version.as_deref().unwrap_or("(none)"),
                new.version.as_deref().unwrap_or("(none)")
            ),
            is_breaking: false,
            source_package: None,
        });
    }
}

fn diff_parent(old: &PomData, new: &PomData, changes: &mut Vec<ManifestChange<Java>>) {
    let old_parent = match (&old.parent_group_id, &old.parent_artifact_id) {
        (Some(g), Some(a)) => Some(format!("{}:{}", g, a)),
        _ => None,
    };
    let new_parent = match (&new.parent_group_id, &new.parent_artifact_id) {
        (Some(g), Some(a)) => Some(format!("{}:{}", g, a)),
        _ => None,
    };

    if old.parent_version != new.parent_version {
        let parent_desc = old_parent
            .as_deref()
            .or(new_parent.as_deref())
            .unwrap_or("parent");
        changes.push(ManifestChange {
            field: "parent.version".into(),
            change_type: JavaManifestChangeType::ParentVersionChanged,
            before: old.parent_version.clone(),
            after: new.parent_version.clone(),
            description: format!(
                "Parent `{}` version changed from `{}` to `{}`",
                parent_desc,
                old.parent_version.as_deref().unwrap_or("(none)"),
                new.parent_version.as_deref().unwrap_or("(none)")
            ),
            is_breaking: false,
            source_package: None,
        });
    }

    if old_parent != new_parent && old_parent.is_some() && new_parent.is_some() {
        changes.push(ManifestChange {
            field: "parent".into(),
            change_type: JavaManifestChangeType::ParentVersionChanged,
            before: old_parent,
            after: new_parent,
            description: "Parent POM changed to a different artifact".into(),
            is_breaking: true,
            source_package: None,
        });
    }
}

fn diff_dependencies(
    old_deps: &[PomDependency],
    new_deps: &[PomDependency],
    changes: &mut Vec<ManifestChange<Java>>,
) {
    let old_map: HashMap<String, &PomDependency> = old_deps.iter().map(|d| (d.key(), d)).collect();
    let new_map: HashMap<String, &PomDependency> = new_deps.iter().map(|d| (d.key(), d)).collect();

    for (key, old_dep) in &old_map {
        if !new_map.contains_key(key) {
            changes.push(ManifestChange {
                field: format!("dependency:{}", key),
                change_type: JavaManifestChangeType::DependencyRemoved,
                before: Some(format_dep(old_dep)),
                after: None,
                description: format!("Dependency `{}` was removed", key),
                is_breaking: true,
                source_package: None,
            });
        }
    }

    for (key, new_dep) in &new_map {
        if !old_map.contains_key(key) {
            changes.push(ManifestChange {
                field: format!("dependency:{}", key),
                change_type: JavaManifestChangeType::DependencyAdded,
                before: None,
                after: Some(format_dep(new_dep)),
                description: format!("Dependency `{}` was added", key),
                is_breaking: false,
                source_package: None,
            });
        }
    }

    for (key, old_dep) in &old_map {
        if let Some(new_dep) = new_map.get(key) {
            if old_dep.version != new_dep.version {
                changes.push(ManifestChange {
                    field: format!("dependency:{}", key),
                    change_type: JavaManifestChangeType::DependencyVersionChanged,
                    before: old_dep.version.clone(),
                    after: new_dep.version.clone(),
                    description: format!(
                        "Dependency `{}` version changed from `{}` to `{}`",
                        key,
                        old_dep.version.as_deref().unwrap_or("(managed)"),
                        new_dep.version.as_deref().unwrap_or("(managed)")
                    ),
                    is_breaking: false,
                    source_package: None,
                });
            }
            if old_dep.scope != new_dep.scope {
                changes.push(ManifestChange {
                    field: format!("dependency:{}", key),
                    change_type: JavaManifestChangeType::DependencyScopeChanged,
                    before: old_dep.scope.clone(),
                    after: new_dep.scope.clone(),
                    description: format!(
                        "Dependency `{}` scope changed from `{}` to `{}`",
                        key,
                        old_dep.scope.as_deref().unwrap_or("compile"),
                        new_dep.scope.as_deref().unwrap_or("compile")
                    ),
                    is_breaking: old_dep.scope.as_deref().unwrap_or("compile") == "compile"
                        && new_dep.scope.as_deref().unwrap_or("compile") != "compile",
                    source_package: None,
                });
            }
        }
    }
}

fn diff_properties(
    old_props: &HashMap<String, String>,
    new_props: &HashMap<String, String>,
    changes: &mut Vec<ManifestChange<Java>>,
) {
    for (key, old_val) in old_props {
        match new_props.get(key) {
            Some(new_val) if old_val != new_val => {
                changes.push(ManifestChange {
                    field: format!("property:{}", key),
                    change_type: JavaManifestChangeType::PropertyChanged,
                    before: Some(old_val.clone()),
                    after: Some(new_val.clone()),
                    description: format!(
                        "Property `{}` changed from `{}` to `{}`",
                        key, old_val, new_val
                    ),
                    is_breaking: false,
                    source_package: None,
                });
            }
            None => {
                changes.push(ManifestChange {
                    field: format!("property:{}", key),
                    change_type: JavaManifestChangeType::PropertyChanged,
                    before: Some(old_val.clone()),
                    after: None,
                    description: format!("Property `{}` was removed (was `{}`)", key, old_val),
                    is_breaking: false,
                    source_package: None,
                });
            }
            _ => {}
        }
    }

    for (key, new_val) in new_props {
        if !old_props.contains_key(key) {
            changes.push(ManifestChange {
                field: format!("property:{}", key),
                change_type: JavaManifestChangeType::PropertyChanged,
                before: None,
                after: Some(new_val.clone()),
                description: format!("Property `{}` was added with value `{}`", key, new_val),
                is_breaking: false,
                source_package: None,
            });
        }
    }
}

fn format_dep(dep: &PomDependency) -> String {
    let mut s = dep.key();
    if let Some(ref v) = dep.version {
        s.push(':');
        s.push_str(v);
    }
    if let Some(ref scope) = dep.scope {
        s.push_str(&format!(" ({})", scope));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_pom() {
        let pom = r#"<?xml version="1.0" encoding="UTF-8"?>
<project>
    <groupId>org.springframework.boot</groupId>
    <artifactId>spring-boot-starter-web</artifactId>
    <version>3.2.0</version>
    <dependencies>
        <dependency>
            <groupId>org.springframework.boot</groupId>
            <artifactId>spring-boot-starter-web</artifactId>
        </dependency>
    </dependencies>
</project>"#;

        let data = parse_pom(pom).unwrap();
        assert_eq!(data.group_id.as_deref(), Some("org.springframework.boot"));
        assert_eq!(data.artifact_id.as_deref(), Some("spring-boot-starter-web"));
        assert_eq!(data.dependencies.len(), 1);
    }

    #[test]
    fn test_diff_pom_parent_version() {
        let old = r#"<?xml version="1.0"?>
<project>
    <parent>
        <groupId>org.springframework.boot</groupId>
        <artifactId>spring-boot-starter-parent</artifactId>
        <version>3.2.0</version>
    </parent>
</project>"#;

        let new = r#"<?xml version="1.0"?>
<project>
    <parent>
        <groupId>org.springframework.boot</groupId>
        <artifactId>spring-boot-starter-parent</artifactId>
        <version>4.0.0</version>
    </parent>
</project>"#;

        let changes = diff_pom(old, new);
        assert_eq!(changes.len(), 1);
        assert!(matches!(
            changes[0].change_type,
            JavaManifestChangeType::ParentVersionChanged
        ));
    }
}
