//! Hierarchy types that are TypeScript/React-specific.
//!
//! These types describe component parent-child composition relationships
//! and are only meaningful for UI component frameworks (React, Vue, etc.).
//! They were moved out of core because the hierarchy concept is
//! framework-specific, not language-agnostic.

use semver_analyzer_core::{ExpectedChild, MigrationTarget};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A change in the component hierarchy between versions, computed by diffing
/// the old and new hierarchy inference results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HierarchyDelta {
    /// The parent component whose children changed.
    pub component: String,
    /// Children added in the new version.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_children: Vec<ExpectedChild>,
    /// Children removed in the new version (no longer direct children).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_children: Vec<String>,
    /// Members removed from this type that now exist on a child type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub migrated_members: Vec<MigratedMember>,
    /// The import path this delta applies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_package: Option<String>,
    /// Migration target data for deprecated→main transitions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_target: Option<MigrationTarget>,
}

/// A member that migrated from a parent type to a child type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigratedMember {
    /// The member name on the old parent type.
    pub member_name: String,
    /// The child type the member moved to.
    pub target_child: String,
    /// The member name on the child, if different from the parent member name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_member_name: Option<String>,
}

/// The hierarchy of a single component family, as inferred by the LLM.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FamilyHierarchy {
    /// Component name → expected children.
    pub components: HashMap<String, Vec<ExpectedChild>>,
}
