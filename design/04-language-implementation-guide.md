# Language Implementation Guide

This document shows how to implement the `Language` trait for a new language,
using TypeScript and Go as contrasting examples. It covers every associated
type and trait method, explaining the reasoning behind each language's choices.

## Implementing `Language` for TypeScript

### Type definitions

```rust
/// The TypeScript language implementation.
pub struct TypeScript;

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
#[serde(tag = "type")]
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
    CssScan {
        change_description: String,
    },
    /// LLM-based analysis (with or without test context).
    LlmAnalysis {
        has_test_context: bool,
        spec_summary: String,
    },
}

/// TypeScript-specific report data (React component analysis).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TsReportData {
    /// Per-component summaries with pre-aggregated change data.
    pub components: Vec<ComponentSummary>,
    /// Bulk constant/token change groups.
    pub constants: Vec<ConstantGroup>,
    /// Newly added components.
    pub added_components: Vec<AddedComponent>,
    /// Component hierarchy changes between versions.
    pub hierarchy_deltas: Vec<HierarchyDelta>,
    /// JSX composition pattern changes.
    pub composition_changes: Vec<CompositionPatternChange>,
}
```

### `Language` trait implementation

```rust
impl Language for TypeScript {
    type Category = TsCategory;
    type ManifestChangeType = TsManifestChangeType;
    type Evidence = TsEvidence;
    type ReportData = TsReportData;

    fn name() -> &'static str { "typescript" }
}
```

### `LanguageSemantics` implementation

```rust
impl LanguageSemantics for TypeScript {
    fn is_member_addition_breaking(&self, _container: &Symbol, member: &Symbol) -> bool {
        // TypeScript uses structural typing. Adding a required member to an
        // interface breaks consumers because they must now provide it.
        // Adding an optional member is non-breaking.
        let is_optional = member.signature.as_ref()
            .and_then(|s| s.parameters.first())
            .map(|p| p.optional)
            .unwrap_or(false);
        !is_optional
    }

    fn same_family(&self, a: &Symbol, b: &Symbol) -> bool {
        // React convention: components in the same directory are a family.
        // components/Modal/Modal.tsx and components/Modal/ModalHeader.tsx
        // are in the same family.
        canonical_component_dir(&a.file) == canonical_component_dir(&b.file)
    }

    fn same_identity(&self, a: &Symbol, b: &Symbol) -> bool {
        // React convention: ButtonProps and Button are the same concept.
        let a_base = a.name.strip_suffix("Props").unwrap_or(&a.name);
        let b_base = b.name.strip_suffix("Props").unwrap_or(&b.name);
        a_base == b_base
    }

    fn visibility_rank(&self, v: Visibility) -> u8 {
        match v {
            Visibility::Private => 0,
            Visibility::Internal => 1,
            Visibility::Protected => 1,  // TS protected ≈ internal for semver
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
```

### `MessageFormatter` implementation (excerpt)

```rust
impl MessageFormatter for TypeScript {
    fn describe(&self, change: &StructuralChange) -> String {
        match &change.change_type {
            StructuralChangeType::Removed(subject) => match subject {
                ChangeSubject::Symbol { kind } => {
                    format!("Exported {} `{}` was removed",
                        ts_kind_label(kind), change.symbol)
                }
                ChangeSubject::Member { name, kind } => {
                    let term = if change.kind == SymbolKind::Enum {
                        "Enum member"
                    } else {
                        "Prop"  // React terminology
                    };
                    format!("{} `{}` was removed from `{}`",
                        term, name, change.symbol)
                }
                ChangeSubject::Parameter { name } => {
                    format!("Parameter `{}` was removed from `{}`",
                        name, change.symbol)
                }
                // ... other subjects
            },

            StructuralChangeType::Changed(subject) => match subject {
                ChangeSubject::ReturnType => {
                    let before = change.before.as_deref().unwrap_or("void");
                    let after = change.after.as_deref().unwrap_or("void");

                    // TypeScript-specific: detect async transitions
                    if !before.starts_with("Promise<") && after.starts_with("Promise<") {
                        format!("`{}` was made async -- callers must now await the result",
                            change.symbol)
                    } else {
                        format!("Return type of `{}` changed from `{}` to `{}`",
                            change.symbol, before, after)
                    }
                }
                ChangeSubject::Visibility => {
                    format!("Visibility of `{}` changed from {} to {}",
                        change.symbol,
                        change.before.as_deref().unwrap_or("unknown"),
                        change.after.as_deref().unwrap_or("unknown"))
                }
                // ... other subjects
            },

            StructuralChangeType::Renamed { from, to } => {
                // Handle cross-type renames (prop -> children, etc.)
                format!("`{}` was renamed", change.symbol)
            }

            // ... Added, Relocated
        }
    }
}

fn ts_kind_label(kind: &SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Class => "class",
        SymbolKind::Interface => "interface",
        SymbolKind::TypeAlias => "type alias",
        SymbolKind::Enum => "enum",
        SymbolKind::Constant => "constant",
        SymbolKind::Variable => "variable",
        SymbolKind::Property => "prop",   // React terminology
        SymbolKind::Constructor => "constructor",
        SymbolKind::Namespace => "namespace",
        SymbolKind::Struct => "struct",
        SymbolKind::EnumMember => "enum member",
    }
}
```

---

## Implementing `Language` for Go

Go provides a strong contrast because its type system and conventions differ
fundamentally from TypeScript.

### Type definitions

```rust
pub struct Go;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoCategory {
    DefaultValue,
    LogicChange,
    ErrorHandling,
    Concurrency,
    IoBehavior,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoManifestChangeType {
    GoVersionChanged,
    RequireAdded,
    RequireRemoved,
    RequireVersionChanged,
    ReplaceDirectiveChanged,
    RetractAdded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GoEvidence {
    TestDelta {
        removed_assertions: Vec<String>,
        added_assertions: Vec<String>,
    },
    InterfaceSatisfaction {
        interface_name: String,
        missing_methods: Vec<String>,
    },
    LlmAnalysis {
        has_test_context: bool,
        spec_summary: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoReportData {
    /// Per-package summaries.
    pub packages: Vec<GoPackageSummary>,
}
```

### `Language` trait implementation

```rust
impl Language for Go {
    type Category = GoCategory;
    type ManifestChangeType = GoManifestChangeType;
    type Evidence = GoEvidence;
    type ReportData = GoReportData;

    fn name() -> &'static str { "go" }
}
```

### `LanguageSemantics` implementation

```rust
impl LanguageSemantics for Go {
    fn is_member_addition_breaking(&self, container: &Symbol, _member: &Symbol) -> bool {
        // Go: adding ANY method to an interface is always breaking.
        // Every type that implements the interface must add the new method.
        // Go has no optional interface members.
        //
        // For structs, adding a field is non-breaking (consumers access
        // fields by name, not position).
        container.kind == SymbolKind::Interface
    }

    fn same_family(&self, a: &Symbol, b: &Symbol) -> bool {
        // Go convention: symbols in the same package are a family.
        // The package is determined by the directory containing the file.
        a.file.parent() == b.file.parent()
    }

    fn same_identity(&self, a: &Symbol, b: &Symbol) -> bool {
        // Go convention: Client and ClientOptions/ClientConfig are the
        // same concept. Also Client and ClientError.
        let a_base = strip_go_suffixes(&a.name);
        let b_base = strip_go_suffixes(&b.name);
        a_base == b_base
    }

    fn visibility_rank(&self, v: Visibility) -> u8 {
        // Go only has two visibility levels: exported (uppercase) and
        // unexported (lowercase). Protected and Public don't apply.
        match v {
            Visibility::Private => 0,
            Visibility::Internal => 0,    // unexported
            Visibility::Protected => 0,   // doesn't exist in Go
            Visibility::Public => 1,      // exported
            Visibility::Exported => 1,    // exported
        }
    }

    // parse_union_values: default None (Go has no union types)
    // post_process: default no-op
}

fn strip_go_suffixes(name: &str) -> &str {
    for suffix in &["Options", "Config", "Params", "Error", "Func"] {
        if let Some(base) = name.strip_suffix(suffix) {
            if !base.is_empty() {
                return base;
            }
        }
    }
    name
}
```

### `MessageFormatter` implementation (excerpt)

```rust
impl MessageFormatter for Go {
    fn describe(&self, change: &StructuralChange) -> String {
        match &change.change_type {
            StructuralChangeType::Removed(subject) => match subject {
                ChangeSubject::Symbol { kind } => {
                    format!("Exported {} `{}` was removed",
                        go_kind_label(kind), change.symbol)
                }
                ChangeSubject::Member { name, kind } => {
                    let term = match kind {
                        SymbolKind::Method => "method",
                        SymbolKind::Property => "field",  // Go uses "field"
                        _ => "member",
                    };
                    format!("{} `{}` was removed from `{}`",
                        term, name, change.symbol)
                }
                // ...
            },

            StructuralChangeType::Added(subject) => match subject {
                ChangeSubject::Member { name, kind: SymbolKind::Method } => {
                    if change.kind == SymbolKind::Interface {
                        // Go-specific: adding a method to an interface is
                        // always breaking because all implementors must add it
                        format!(
                            "Method `{}` was added to interface `{}` -- \
                             all implementors must add this method",
                            name, change.symbol
                        )
                    } else {
                        format!("Method `{}` was added to `{}`",
                            name, change.symbol)
                    }
                }
                // ...
            },

            StructuralChangeType::Changed(subject) => match subject {
                ChangeSubject::ReturnType => {
                    // Go: no async, no "void" default, multiple returns are normal
                    let before = change.before.as_deref().unwrap_or("(no return)");
                    let after = change.after.as_deref().unwrap_or("(no return)");
                    format!("Return type of `{}` changed from `{}` to `{}`",
                        change.symbol, before, after)
                }
                // ...
            },

            // ...
        }
    }
}

fn go_kind_label(kind: &SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Interface => "interface",
        SymbolKind::TypeAlias => "type",
        SymbolKind::Constant => "constant",
        SymbolKind::Variable => "variable",
        SymbolKind::Property => "field",     // Go uses "field" for struct members
        SymbolKind::Enum => "const group",   // Go pseudo-enums
        SymbolKind::EnumMember => "const",   // Go iota constants
        SymbolKind::Class => "type",         // Go doesn't have classes
        SymbolKind::Constructor => "constructor function",
        SymbolKind::Namespace => "package",
    }
}
```

---

## Key Differences By Language

### What constitutes a breaking change

| Scenario | TypeScript | Go | Java | Python | C# |
|----------|-----------|-----|------|--------|-----|
| Add method to interface | Breaking if required | **Always breaking** | Breaking (abstract) | Breaking (ABC) | Breaking (abstract) |
| Add field to struct/class | Non-breaking | Non-breaking | Non-breaking | Non-breaking | Non-breaking |
| Remove exported function | Breaking | Breaking | Breaking | Breaking | Breaking |
| Change return type | Breaking | Breaking | Breaking | Usually breaking | Breaking |
| Add optional parameter | Non-breaking | N/A (no optional) | N/A (overloads) | Non-breaking | Non-breaking |
| Add required parameter | Breaking | Breaking | Breaking | Breaking | Breaking |

### Visibility models

| Level | TypeScript | Go | Java | Python | C# |
|-------|-----------|-----|------|--------|-----|
| Private | `private`/`#` | N/A | `private` | `__name` | `private` |
| Internal | not exported | lowercase | package-private | N/A | `internal` |
| Protected | N/A | N/A | `protected` | `_name` | `protected` |
| Public | class member | N/A | `public` | default | `public` |
| Exported | `export` | Uppercase | public (from jar) | `__all__` | public (from assembly) |

### Companion type conventions

| Language | Convention | Example |
|----------|-----------|---------|
| TypeScript | `Props` suffix | `Button` + `ButtonProps` |
| Go | `Options`/`Config` suffix | `Client` + `ClientOptions` |
| Java | `Impl` suffix | `UserService` + `UserServiceImpl` |
| Python | `Mixin`/`Base` suffix | `UserView` + `UserViewMixin` |
| C# | `Options`/`Settings` suffix | `DbConnection` + `DbConnectionOptions` |

### Manifest files

| Language | File | Key concepts |
|----------|------|-------------|
| TypeScript | `package.json` | exports map, peerDependencies, CJS/ESM, engines |
| Go | `go.mod` | go version, require, replace, retract |
| Java | `pom.xml` / `build.gradle` | groupId:artifactId, dependencies, plugins |
| Python | `pyproject.toml` | dependencies, requires-python, scripts |
| C# | `*.csproj` | TargetFramework, PackageReference |

---

## Checklist for Adding a New Language

1. **Create the language crate** (e.g., `crates/go/`)

2. **Define the associated types:**
   - `Category` enum -- what kinds of behavioral changes exist
   - `ManifestChangeType` enum -- what manifest changes your package system has
   - `Evidence` enum -- how behavioral changes are detected
   - `ReportData` struct -- framework-specific analysis groupings

3. **Implement `LanguageSemantics`:**
   - `is_member_addition_breaking` -- the most important rule
   - `same_family` -- how to group related symbols
   - `same_identity` -- how to recognize companion types
   - `visibility_rank` -- your language's visibility ordering
   - `parse_union_values` -- if your language has union/literal types
   - `post_process` -- if you need to clean up change lists

4. **Implement `MessageFormatter`:**
   - One `match` over `StructuralChangeType` x `ChangeSubject`
   - Use language-appropriate terminology
   - Handle language-specific edge cases (async detection, etc.)

5. **Implement `ApiExtractor`:**
   - Parse your language's source at a git ref
   - Populate `Symbol`, `Signature`, `Parameter`, `TypeParameter`

6. **Implement `DiffParser`:**
   - Parse changed functions from git diff output

7. **Implement `CallGraphBuilder`:**
   - Find callers of a given function for propagation analysis

8. **Implement `TestAnalyzer`:**
   - Find test files and diff assertions between versions

9. **Implement manifest differ:**
   - Parse your manifest file format
   - Produce `ManifestChange<YourLanguage>` values

10. **Add language dispatch** to the CLI and rule generator binaries
