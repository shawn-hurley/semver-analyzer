//! TypeScript `Language` trait implementation.
//!
//! Provides all TypeScript/React-specific semantic rules, message formatting,
//! and associated types for the multi-language architecture.
//!
//! This module extracts language-specific logic that currently lives in
//! `core/diff/compare.rs`, `core/diff/helpers.rs`, `core/diff/migration.rs`,
//! and `core/diff/mod.rs` into a trait implementation that the diff engine
//! can call through the `LanguageSemantics` and `MessageFormatter` traits.

use semver_analyzer_core::{
    Language, LanguageSemantics, MessageFormatter, StructuralChange, StructuralChangeType, Symbol,
    SymbolKind, Visibility,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

// ── TypeScript language type ────────────────────────────────────────────

/// The TypeScript language implementation.
#[derive(Debug)]
pub struct TypeScript;

// ── Associated types ────────────────────────────────────────────────────

/// Behavioral change categories for TypeScript/React analysis.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TsCategory {
    /// Changed element types, wrapper elements, component nesting.
    DomStructure,
    /// CSS class name renames, removals, changed application logic.
    CssClass,
    /// CSS custom property (variable) renames or removals.
    CssVariable,
    /// ARIA attribute changes, role changes, keyboard navigation.
    Accessibility,
    /// Changed default prop/parameter values.
    DefaultValue,
    /// Changed conditional logic, return values, event handling.
    LogicChange,
    /// Changed data-* attributes (data-testid, data-ouia-*, etc.).
    DataAttribute,
    /// General render output change.
    RenderOutput,
}

/// Manifest change types for npm/package.json.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TsManifestChangeType {
    EntryPointChanged,
    ExportsEntryRemoved,
    ExportsEntryAdded,
    ExportsConditionRemoved,
    ModuleSystemChanged,
    PeerDependencyAdded,
    PeerDependencyRemoved,
    PeerDependencyRangeChanged,
    EngineConstraintChanged,
    BinEntryRemoved,
}

/// Evidence data for TypeScript behavioral changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TsEvidence {
    /// Test assertions changed.
    TestDelta {
        removed_assertions: Vec<String>,
        added_assertions: Vec<String>,
    },
    /// Deterministic JSX AST diff.
    JsxDiff {
        element_before: Option<String>,
        element_after: Option<String>,
        change_description: String,
    },
    /// Deterministic CSS reference scan.
    CssScan { change_description: String },
    /// LLM-based analysis (with or without test context).
    LlmAnalysis {
        has_test_context: bool,
        spec_summary: String,
    },
}

/// TypeScript-specific report data (React component analysis).
///
/// These types will eventually absorb ComponentSummary, HierarchyDelta,
/// CompositionPatternChange, and other React-specific types currently
/// in the core crate. For now this is a placeholder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TsReportData {
    /// Placeholder -- will hold ComponentSummary, ConstantGroup, etc.
    /// when they move from core in Phase 5.
    #[serde(default)]
    pub _placeholder: (),
}

// ── LanguageSemantics ───────────────────────────────────────────────────

impl LanguageSemantics for TypeScript {
    fn is_member_addition_breaking(&self, container: &Symbol, member: &Symbol) -> bool {
        // TypeScript uses structural typing. Adding a required member to an
        // interface or type alias breaks consumers because they must now
        // provide it. Adding an optional member is non-breaking.
        //
        // For enums and classes, adding a member is never breaking.
        match container.kind {
            SymbolKind::Interface | SymbolKind::TypeAlias => {
                let is_optional = member
                    .signature
                    .as_ref()
                    .and_then(|s| s.parameters.first())
                    .map(|p| p.optional)
                    .unwrap_or(false);
                !is_optional
            }
            _ => false,
        }
    }

    fn same_family(&self, a: &Symbol, b: &Symbol) -> bool {
        // React convention: components in the same directory are a family.
        // E.g., components/Modal/Modal.tsx and components/Modal/ModalHeader.tsx
        //
        // We strip /deprecated/ and /next/ segments for canonical matching so
        // that a symbol moving between deprecated/ and main/ paths is still
        // considered the same family.
        canonical_component_dir(&a.file.to_string_lossy())
            == canonical_component_dir(&b.file.to_string_lossy())
    }

    fn same_identity(&self, a: &Symbol, b: &Symbol) -> bool {
        // React convention: ButtonProps and Button are the same concept.
        // Strip the "Props" suffix before comparing.
        strip_props_suffix(&a.name) == strip_props_suffix(&b.name)
    }

    fn visibility_rank(&self, v: Visibility) -> u8 {
        // TypeScript visibility ranking. Protected is treated the same as
        // Internal for semver purposes (both are non-exported).
        match v {
            Visibility::Private => 0,
            Visibility::Internal => 1,
            Visibility::Protected => 1, // TS protected ≈ internal for semver
            Visibility::Public => 2,
            Visibility::Exported => 3,
        }
    }

    fn parse_union_values(&self, type_str: &str) -> Option<BTreeSet<String>> {
        // TypeScript string literal unions: 'primary' | 'secondary' | 'danger'
        parse_ts_union_literals(type_str)
    }

    fn post_process(&self, changes: &mut Vec<StructuralChange>) {
        // Deduplicate changes for symbols exported both by name and as
        // `export default` (a TypeScript/JS-specific pattern).
        dedup_default_exports(changes);
    }
}

// ── MessageFormatter ────────────────────────────────────────────────────

impl MessageFormatter for TypeScript {
    fn describe(&self, change: &StructuralChange) -> String {
        // For Phase 2, this matches on the current 37-variant StructuralChangeType.
        // In Phase 4 when we collapse the enum, this will be updated to match
        // on the new 5-variant StructuralChangeTypeV2 + ChangeSubject.
        //
        // The descriptions must produce identical output to the current inline
        // description building in compare.rs and helpers.rs.
        //
        // For now, the descriptions are already built by the diff engine and
        // stored on the StructuralChange. This method returns them as-is.
        // In Phase 3, the diff engine will stop building descriptions and
        // call this method instead.
        change.description.clone()
    }
}

// ── Language ────────────────────────────────────────────────────────────

impl Language for TypeScript {
    type Category = TsCategory;
    type ManifestChangeType = TsManifestChangeType;
    type Evidence = TsEvidence;
    type ReportData = TsReportData;

    fn name() -> &'static str {
        "typescript"
    }
}

// ── Extracted helper functions ──────────────────────────────────────────

/// Extract the component directory from a file path, stripping /deprecated/
/// and /next/ segments for canonical matching.
///
/// `packages/react-core/dist/esm/deprecated/components/Select/Select.d.ts`
/// → `packages/react-core/dist/esm/components/Select`
///
/// `packages/react-core/dist/esm/components/EmptyState/EmptyStateHeader.d.ts`
/// → `packages/react-core/dist/esm/components/EmptyState`
fn canonical_component_dir(file_path: &str) -> String {
    let canonical = file_path
        .replace("/deprecated/", "/")
        .replace("/next/", "/");
    let canonical = if canonical.starts_with("deprecated/") {
        canonical.strip_prefix("deprecated/").unwrap().to_string()
    } else {
        canonical
    };
    let canonical = if canonical.starts_with("next/") {
        canonical.strip_prefix("next/").unwrap().to_string()
    } else {
        canonical
    };

    match canonical.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => canonical,
    }
}

/// Strip a "Props" suffix from a symbol name for comparison.
///
/// `EmptyStateHeaderProps` → `EmptyStateHeader`
/// `SelectProps` → `Select`
/// `Modal` → `Modal`
fn strip_props_suffix(name: &str) -> &str {
    name.strip_suffix("Props").unwrap_or(name)
}

/// Parse a TypeScript string literal union type into its individual members.
///
/// `'primary' | 'secondary' | 'tertiary'` → `{"primary", "secondary", "tertiary"}`
///
/// Also handles mixed unions like `'primary' | ButtonVariant | undefined` by
/// extracting only the string literal members (quoted with single or double quotes).
fn parse_ts_union_literals(type_str: &str) -> Option<BTreeSet<String>> {
    if !type_str.contains('\'') && !type_str.contains('"') {
        return None;
    }
    if !type_str.contains('|') {
        return None;
    }

    let mut literals = BTreeSet::new();
    for part in type_str.split('|') {
        let trimmed = part.trim();
        if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
            || (trimmed.starts_with('"') && trimmed.ends_with('"'))
        {
            let value = &trimmed[1..trimmed.len() - 1];
            if !value.is_empty() {
                literals.insert(value.to_string());
            }
        }
    }

    if literals.len() >= 2 {
        Some(literals)
    } else {
        None
    }
}

/// Remove redundant `default` export changes when a named sibling from the
/// same file has the same change type.
fn dedup_default_exports(changes: &mut Vec<StructuralChange>) {
    use std::collections::HashSet;

    let named_changes: HashSet<(String, StructuralChangeType)> = changes
        .iter()
        .filter(|c| c.symbol != "default")
        .filter_map(|c| {
            file_prefix(&c.qualified_name).map(|prefix| (prefix.to_string(), c.change_type.clone()))
        })
        .collect();

    changes.retain(|c| {
        if c.symbol != "default" {
            return true;
        }
        if let Some(prefix) = file_prefix(&c.qualified_name) {
            !named_changes.contains(&(prefix.to_string(), c.change_type.clone()))
        } else {
            true
        }
    });
}

/// Extract the file prefix from a qualified_name (everything before the last `.`).
fn file_prefix(qualified_name: &str) -> Option<&str> {
    qualified_name.rsplit_once('.').map(|(prefix, _)| prefix)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use semver_analyzer_core::{ApiSurface, Parameter, Signature};

    fn sym(name: &str, kind: SymbolKind) -> Symbol {
        Symbol::new(name, name, kind, Visibility::Exported, "test.d.ts", 1)
    }

    fn make_interface(name: &str, file: &str, members: &[&str]) -> Symbol {
        let mut s = Symbol::new(
            name,
            &format!("{}.{}", file, name),
            SymbolKind::Interface,
            Visibility::Exported,
            file,
            1,
        );
        for &member_name in members {
            s.members.push(Symbol::new(
                member_name,
                &format!("{}.{}.{}", file, name, member_name),
                SymbolKind::Property,
                Visibility::Public,
                file,
                1,
            ));
        }
        s
    }

    // ── is_member_addition_breaking ──────────────────────────────

    #[test]
    fn required_member_on_interface_is_breaking() {
        let ts = TypeScript;
        let container = sym("ButtonProps", SymbolKind::Interface);
        let member = sym("onClick", SymbolKind::Property);
        assert!(ts.is_member_addition_breaking(&container, &member));
    }

    #[test]
    fn optional_member_on_interface_is_not_breaking() {
        let ts = TypeScript;
        let container = sym("ButtonProps", SymbolKind::Interface);
        let mut member = sym("onClick", SymbolKind::Property);
        member.signature = Some(Signature {
            parameters: vec![Parameter {
                name: "onClick".into(),
                type_annotation: Some("() => void".into()),
                optional: true,
                has_default: false,
                default_value: None,
                is_rest: false,
            }],
            return_type: None,
            type_parameters: vec![],
            is_async: false,
        });
        assert!(!ts.is_member_addition_breaking(&container, &member));
    }

    #[test]
    fn member_on_enum_is_not_breaking() {
        let ts = TypeScript;
        let container = sym("Color", SymbolKind::Enum);
        let member = sym("Green", SymbolKind::EnumMember);
        assert!(!ts.is_member_addition_breaking(&container, &member));
    }

    #[test]
    fn member_on_class_is_not_breaking() {
        let ts = TypeScript;
        let container = sym("UserService", SymbolKind::Class);
        let member = sym("getUser", SymbolKind::Method);
        assert!(!ts.is_member_addition_breaking(&container, &member));
    }

    // ── same_family ─────────────────────────────────────────────

    #[test]
    fn same_directory_is_same_family() {
        let ts = TypeScript;
        let a = make_interface("Modal", "components/Modal/Modal.d.ts", &[]);
        let b = make_interface("ModalHeader", "components/Modal/ModalHeader.d.ts", &[]);
        assert!(ts.same_family(&a, &b));
    }

    #[test]
    fn different_directory_is_not_same_family() {
        let ts = TypeScript;
        let a = make_interface("Modal", "components/Modal/Modal.d.ts", &[]);
        let b = make_interface("Button", "components/Button/Button.d.ts", &[]);
        assert!(!ts.same_family(&a, &b));
    }

    #[test]
    fn deprecated_and_main_are_same_family() {
        let ts = TypeScript;
        let a = make_interface("Select", "deprecated/components/Select/Select.d.ts", &[]);
        let b = make_interface("Select", "components/Select/Select.d.ts", &[]);
        assert!(ts.same_family(&a, &b));
    }

    // ── same_identity ───────────────────────────────────────────

    #[test]
    fn button_and_button_props_are_same_identity() {
        let ts = TypeScript;
        let a = sym("Button", SymbolKind::Function);
        let b = sym("ButtonProps", SymbolKind::Interface);
        assert!(ts.same_identity(&a, &b));
    }

    #[test]
    fn same_name_is_same_identity() {
        let ts = TypeScript;
        let a = sym("Select", SymbolKind::Interface);
        let b = sym("Select", SymbolKind::Interface);
        assert!(ts.same_identity(&a, &b));
    }

    #[test]
    fn different_names_are_not_same_identity() {
        let ts = TypeScript;
        let a = sym("Button", SymbolKind::Function);
        let b = sym("Select", SymbolKind::Function);
        assert!(!ts.same_identity(&a, &b));
    }

    // ── visibility_rank ─────────────────────────────────────────

    #[test]
    fn ts_visibility_ranking() {
        let ts = TypeScript;
        assert!(ts.visibility_rank(Visibility::Private) < ts.visibility_rank(Visibility::Internal));
        assert_eq!(
            ts.visibility_rank(Visibility::Internal),
            ts.visibility_rank(Visibility::Protected)
        );
        assert!(ts.visibility_rank(Visibility::Protected) < ts.visibility_rank(Visibility::Public));
        assert!(ts.visibility_rank(Visibility::Public) < ts.visibility_rank(Visibility::Exported));
    }

    // ── parse_union_values ──────────────────────────────────────

    #[test]
    fn parses_string_literal_union() {
        let ts = TypeScript;
        let result = ts
            .parse_union_values("'primary' | 'secondary' | 'danger'")
            .unwrap();
        assert_eq!(result.len(), 3);
        assert!(result.contains("primary"));
        assert!(result.contains("secondary"));
        assert!(result.contains("danger"));
    }

    #[test]
    fn returns_none_for_non_union() {
        let ts = TypeScript;
        assert!(ts.parse_union_values("string").is_none());
    }

    #[test]
    fn returns_none_for_single_literal() {
        let ts = TypeScript;
        assert!(ts.parse_union_values("'primary'").is_none());
    }

    #[test]
    fn handles_mixed_union_with_type_refs() {
        let ts = TypeScript;
        let result = ts
            .parse_union_values("'primary' | 'secondary' | ButtonVariant | undefined")
            .unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains("primary"));
        assert!(result.contains("secondary"));
    }

    // ── post_process (dedup default exports) ────────────────────

    #[test]
    fn dedup_default_keeps_named_removes_default() {
        use semver_analyzer_core::ChangeSubject;
        let ts = TypeScript;
        let mut changes = vec![
            StructuralChange {
                symbol: "c_button".into(),
                qualified_name: "pkg/dist/c_button.c_button".into(),
                kind: SymbolKind::Constant,
                change_type: StructuralChangeType::Removed(ChangeSubject::Symbol {
                    kind: SymbolKind::Constant,
                }),
                before: None,
                after: None,
                description: "removed".into(),
                is_breaking: true,
                impact: None,
                migration_target: None,
            },
            StructuralChange {
                symbol: "default".into(),
                qualified_name: "pkg/dist/c_button.default".into(),
                kind: SymbolKind::Constant,
                change_type: StructuralChangeType::Removed(ChangeSubject::Symbol {
                    kind: SymbolKind::Constant,
                }),
                before: None,
                after: None,
                description: "removed".into(),
                is_breaking: true,
                impact: None,
                migration_target: None,
            },
        ];
        ts.post_process(&mut changes);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].symbol, "c_button");
    }

    // ── canonical_component_dir ─────────────────────────────────

    #[test]
    fn strips_deprecated_segment() {
        assert_eq!(
            canonical_component_dir(
                "packages/react-core/dist/esm/deprecated/components/Select/Select.d.ts"
            ),
            "packages/react-core/dist/esm/components/Select"
        );
    }

    #[test]
    fn strips_next_segment() {
        assert_eq!(
            canonical_component_dir(
                "packages/react-core/dist/esm/next/components/Modal/ModalHeader.d.ts"
            ),
            "packages/react-core/dist/esm/components/Modal"
        );
    }

    #[test]
    fn normal_path_returns_directory() {
        assert_eq!(
            canonical_component_dir(
                "packages/react-core/dist/esm/components/EmptyState/EmptyStateHeader.d.ts"
            ),
            "packages/react-core/dist/esm/components/EmptyState"
        );
    }
}
