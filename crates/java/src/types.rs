//! Java-specific types for the semver-analyzer.
//!
//! Defines the associated types required by the `Language` trait:
//! `JavaSymbolData`, `JavaCategory`, `JavaManifestChangeType`,
//! `JavaEvidence`, and `JavaReportData`.

use serde::{Deserialize, Serialize};

// ── Per-symbol metadata ─────────────────────────────────────────────────

/// Per-symbol metadata for Java declarations.
///
/// This is the concrete type for `Language::SymbolData`.
/// Carried on `Symbol<JavaSymbolData>` throughout the Java pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaSymbolData {
    /// Annotations on this declaration (e.g., `@Deprecated`, `@Bean`,
    /// `@ConfigurationProperties(prefix = "spring.data.mongodb")`).
    ///
    /// Used by `diff_language_data` to detect annotation changes that
    /// the core diff engine can't see (it treats `language_data` as opaque).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<JavaAnnotation>,

    /// Checked exception types in the `throws` clause.
    ///
    /// Adding a checked exception is breaking (callers must handle it).
    /// Removing one is not (catch blocks become dead code).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub throws: Vec<String>,

    /// Whether this is a Java record (`record Foo(int x, String y)`).
    ///
    /// Records have different semver semantics: adding/removing/reordering
    /// components is always breaking because it changes the canonical
    /// constructor signature.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_record: bool,

    /// Whether this is a Java annotation type (`@interface Foo`).
    ///
    /// Annotation types have different semantics from regular interfaces:
    /// elements have types and optional defaults, they can't be implemented,
    /// and adding a required element (no default) is breaking.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_annotation_type: bool,

    /// Whether this is an interface `default` method.
    ///
    /// Critical for `is_member_addition_breaking`: adding a non-default
    /// abstract method to an interface breaks all implementors, but adding
    /// a default method does NOT.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_default: bool,

    /// Whether this class/interface is `sealed`.
    ///
    /// Sealed types restrict which classes can extend/implement them.
    /// Making a non-sealed type sealed is breaking.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_sealed: bool,

    /// Permitted subtypes for sealed classes/interfaces.
    ///
    /// `sealed class Shape permits Circle, Rectangle`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permits: Vec<String>,

    /// Whether this class is `final` (cannot be extended).
    ///
    /// Making a non-final class final is a breaking change because
    /// existing subclasses will fail to compile.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_final: bool,
}

fn is_false(b: &bool) -> bool {
    !b
}

/// A Java annotation on a declaration.
///
/// Captures the annotation name, optional fully-qualified name, and
/// key-value attributes. Used for detecting framework-level semantic
/// changes (e.g., Spring `@ConfigurationProperties` prefix changes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaAnnotation {
    /// Simple annotation name (e.g., `"Deprecated"`, `"Bean"`).
    pub name: String,

    /// Fully qualified annotation name, if resolvable from imports.
    ///
    /// e.g., `"java.lang.Deprecated"`,
    /// `"org.springframework.context.annotation.Bean"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,

    /// Annotation attributes as key-value pairs.
    ///
    /// Single-value annotations use `"value"` as the key:
    /// `@RequestMapping("/api")` → `[("value", "\"/api\"")]`
    ///
    /// Named attributes: `@Deprecated(since = "3.2", forRemoval = true)`
    /// → `[("since", "\"3.2\""), ("forRemoval", "true")]`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<(String, String)>,
}

// ── Behavioral change categories ────────────────────────────────────────

/// Behavioral change categories for Java analysis.
///
/// These categorize the nature of behavioral changes detected by the BU
/// pipeline (changed function bodies, test deltas, LLM analysis).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JavaCategory {
    /// Changed method logic, control flow, return values.
    LogicChange,
    /// Changed exception handling (try/catch/throw).
    ExceptionHandling,
    /// Changed annotations or configuration.
    Configuration,
    /// Changed concurrency behavior (synchronized, locks, futures).
    Concurrency,
    /// Changed data access patterns (SQL, JPA, repository calls).
    DataAccess,
    /// Changed security-related behavior (auth, permissions).
    Security,
    /// Changed serialization/deserialization behavior (Jackson, etc.).
    Serialization,
    /// General behavioral change.
    Other,
}

// ── Manifest change types ───────────────────────────────────────────────

/// Manifest change types for Java build systems (pom.xml, build.gradle).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JavaManifestChangeType {
    /// A dependency was added.
    DependencyAdded,
    /// A dependency was removed.
    DependencyRemoved,
    /// A dependency version changed.
    DependencyVersionChanged,
    /// The parent POM version changed (e.g., spring-boot-starter-parent).
    ParentVersionChanged,
    /// A Maven/Gradle property changed (often version properties).
    PropertyChanged,
    /// A build plugin was added, removed, or changed.
    PluginChanged,
    /// The project's own groupId, artifactId, or version changed.
    ProjectIdentityChanged,
    /// A dependency scope changed (compile → runtime, etc.).
    DependencyScopeChanged,
}

// ── Evidence types ──────────────────────────────────────────────────────

/// Evidence data for Java behavioral changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JavaEvidence {
    /// JUnit/TestNG test assertions changed.
    TestDelta {
        removed_assertions: Vec<String>,
        added_assertions: Vec<String>,
    },
    /// LLM-based analysis of method body changes.
    LlmAnalysis {
        has_test_context: bool,
        spec_summary: String,
    },
}

// ── Report data ─────────────────────────────────────────────────────────

/// Java-specific report data.
///
/// Placeholder for future Java-specific report enrichment (e.g.,
/// Spring annotation analysis, module system changes).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JavaReportData {
    #[serde(default)]
    pub _placeholder: (),
}
