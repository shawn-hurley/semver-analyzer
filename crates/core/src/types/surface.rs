//! Core data structures for representing API surfaces and their components.
//!
//! These are language-agnostic: the per-language `Language` implementations
//! populate them, and the language-agnostic `diff_surfaces_with_semantics()` engine consumes them.
//!
//! `Symbol<M>` and `ApiSurface<M>` are generic over a metadata type parameter `M`
//! that carries language-specific per-symbol data. The default `M = ()` keeps the
//! types backward-compatible for code that doesn't need language-specific metadata.
//! TypeScript uses `TsSymbolData` (rendered components, CSS tokens).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// Helper for `#[serde(skip_serializing_if)]` on the `language_data` field.
/// Skips serialization when the value equals its `Default`, avoiding noise
/// like `language_data: null` for `Symbol<()>` or empty `TsSymbolData`.
fn is_default<T: Default + PartialEq>(val: &T) -> bool {
    *val == T::default()
}

/// Language-agnostic public API surface extracted from source code at a git ref.
/// Used by TD (Top-Down) pipeline.
///
/// Generic over `M` for per-symbol language metadata. Defaults to `()`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(bound(serialize = "M: Serialize", deserialize = "M: Deserialize<'de>"))]
pub struct ApiSurface<M: Default + Clone + PartialEq = ()> {
    /// All exported symbols in the API surface.
    pub symbols: Vec<Symbol<M>>,
}

impl<M: Default + Clone + PartialEq> ApiSurface<M> {
    /// Returns true if the surface has no symbols.
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    /// Returns the number of symbols in the surface.
    pub fn len(&self) -> usize {
        self.symbols.len()
    }
}

/// A single exported symbol in the API surface.
///
/// Generic over `M` for language-specific per-symbol metadata. The default
/// `M = ()` keeps the type backward-compatible for code that doesn't need
/// language-specific data (e.g., core diff tests, MinimalSemantics).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "M: Serialize + PartialEq",
    deserialize = "M: Deserialize<'de>"
))]
pub struct Symbol<M: Default + Clone + PartialEq = ()> {
    /// Simple name (e.g., "createUser").
    pub name: String,

    /// Fully qualified name including module path (e.g., "src/api/users.createUser").
    pub qualified_name: String,

    /// What kind of symbol this is.
    pub kind: SymbolKind,

    /// Export visibility level.
    pub visibility: Visibility,

    /// Source file containing this symbol.
    pub file: PathBuf,

    /// Distribution/dependency identity for this symbol's package.
    ///
    /// This is the name that appears in dependency manifests:
    /// - TypeScript: npm package name (e.g., `"@patternfly/react-charts"`)
    /// - Go: module path from go.mod (e.g., `"github.com/org/repo"`)
    /// - Python: PyPI package name (e.g., `"requests"`)
    ///
    /// See also [`import_path`](Self::import_path) for the consumer-facing
    /// import specifier, which may include subpath information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,

    /// Consumer-facing import specifier through which this symbol is accessible.
    ///
    /// Distinct from [`package`](Self::package), which identifies the
    /// distribution/dependency unit (what appears in dependency manifests).
    /// `import_path` is what consumers write in their source code import
    /// statements.
    ///
    /// Examples:
    /// - TypeScript: `"@patternfly/react-charts/victory"` (subpath export)
    /// - Go: `"github.com/org/repo/pkg/auth"` (package import path)
    /// - Python: `"requests.auth"` (module import path)
    ///
    /// When `None`, the import path is assumed to be the same as `package`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_path: Option<String>,

    /// Line number in the source file (1-indexed).
    pub line: usize,

    /// Function/method signature (None for non-callable symbols like constants).
    pub signature: Option<Signature>,

    // -- Class hierarchy --
    /// Parent class (`extends` clause). e.g., "BaseValidator" for
    /// `class EmailValidator extends BaseValidator`.
    pub extends: Option<String>,

    /// Implemented interfaces. e.g., `["Serializable", "Comparable"]`.
    pub implements: Vec<String>,

    /// Whether this symbol is abstract (class or method).
    pub is_abstract: bool,

    // -- Type dependencies --
    /// Types referenced in this symbol's signature (parameter types,
    /// return types, generic constraints, property types). Used for
    /// transitive impact analysis: if a referenced type changes,
    /// this symbol is potentially affected.
    ///
    /// Example: `fn createUser(opts: UserOptions): Promise<User>`
    ///   -> type_dependencies: `["UserOptions", "User"]`
    ///
    /// Includes compile-time-only dependencies that affect the API surface
    /// but may not exist at runtime.
    pub type_dependencies: Vec<String>,

    // -- Member modifiers (for class/interface members) --
    /// Whether this member is readonly.
    pub is_readonly: bool,

    /// Whether this member is static.
    pub is_static: bool,

    /// Accessor kind (for properties that are get/set accessors).
    pub accessor_kind: Option<AccessorKind>,

    // -- Members (for classes, interfaces, enums) --
    /// Child members (methods, properties, enum variants).
    /// Only populated for Class, Interface, and Enum kinds.
    pub members: Vec<Symbol<M>>,

    // -- Language-specific metadata --
    /// Per-symbol data specific to the implementing language.
    ///
    /// - TypeScript: `TsSymbolData` (rendered components, CSS tokens)
    /// - Other languages: `()` (no additional data)
    ///
    /// Skipped in serialization when equal to Default (e.g., `()` or empty `TsSymbolData`).
    #[serde(default, skip_serializing_if = "is_default")]
    pub language_data: M,
}

impl<M: Default + Clone + PartialEq> Symbol<M> {
    /// Create a new Symbol with required fields, defaulting optional fields.
    pub fn new(
        name: impl Into<String>,
        qualified_name: impl Into<String>,
        kind: SymbolKind,
        visibility: Visibility,
        file: impl Into<PathBuf>,
        line: usize,
    ) -> Self {
        Self {
            name: name.into(),
            qualified_name: qualified_name.into(),
            kind,
            visibility,
            file: file.into(),
            package: None,
            import_path: None,
            line,
            signature: None,
            extends: None,
            implements: Vec::new(),
            is_abstract: false,
            type_dependencies: Vec::new(),
            is_readonly: false,
            is_static: false,
            accessor_kind: None,
            members: Vec::new(),
            language_data: M::default(),
        }
    }

    /// Convert this symbol's metadata type to a different type.
    ///
    /// Useful for converting between `Symbol<()>` (core/test) and
    /// `Symbol<TsSymbolData>` (TypeScript extraction).
    pub fn with_metadata<N: Default + Clone + PartialEq>(self) -> Symbol<N> {
        Symbol {
            name: self.name,
            qualified_name: self.qualified_name,
            kind: self.kind,
            visibility: self.visibility,
            file: self.file,
            package: self.package,
            import_path: self.import_path,
            line: self.line,
            signature: self.signature,
            extends: self.extends,
            implements: self.implements,
            is_abstract: self.is_abstract,
            type_dependencies: self.type_dependencies,
            is_readonly: self.is_readonly,
            is_static: self.is_static,
            accessor_kind: self.accessor_kind,
            members: self
                .members
                .into_iter()
                .map(|m| m.with_metadata())
                .collect(),
            language_data: N::default(),
        }
    }
}

/// What kind of symbol this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    /// Value type (Go, C#). Distinct from Class in languages that differentiate.
    Struct,
    Interface,
    TypeAlias,
    Enum,
    EnumMember,
    Constant,
    Variable,
    Property,
    Constructor,
    GetAccessor,
    SetAccessor,
    Namespace,
}

/// Export visibility level.
///
/// Defaults to `Public` — the most common visibility for API symbols
/// across languages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Module-level export: the symbol is visible to external consumers.
    /// Relevant for languages with an explicit export mechanism
    /// (JS/TS: `export`, Python: `__all__`, Rust: `pub` at crate root).
    /// Languages without a distinction between public and exported (Java,
    /// C#, Go) should use `Public` for all externally-visible symbols.
    Exported,
    /// Public member: visible to all code within the same package/module.
    #[default]
    Public,
    /// Accessible to subclasses. Java: `protected`. C#: `protected`. Python: `_prefix`.
    Protected,
    /// Module-internal (not exported).
    Internal,
    /// Explicitly marked private (`private` keyword or `#field`).
    Private,
}

/// Accessor kind for class members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessorKind {
    Get,
    Set,
    GetSet,
}

/// Function or method signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Ordered list of parameters.
    pub parameters: Vec<Parameter>,

    /// Return type as a canonicalized string (e.g., "Promise<User>").
    /// None if not annotated.
    pub return_type: Option<String>,

    /// Generic type parameters (e.g., `<T extends Serializable = unknown>`).
    pub type_parameters: Vec<TypeParameter>,

    /// Whether the function is async.
    pub is_async: bool,
}

/// A generic type parameter declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeParameter {
    /// Name of the type parameter (e.g., "T").
    pub name: String,

    /// Constraint (e.g., "Serializable" from `T extends Serializable`).
    pub constraint: Option<String>,

    /// Default type (e.g., "unknown" from `T = unknown`).
    pub default: Option<String>,
}

/// A function/method parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parameter {
    /// Parameter name.
    pub name: String,

    /// Type annotation as a canonicalized string.
    pub type_annotation: Option<String>,

    /// Whether the parameter is optional (`param?: Type`).
    pub optional: bool,

    /// Whether the parameter has a default value.
    pub has_default: bool,

    /// The actual default value expression as a string, for static comparison.
    /// e.g., `"10"`, `"'hello'"`, `"[]"`.
    pub default_value: Option<String>,

    /// Whether this is a variadic/rest parameter (`...args`).
    pub is_variadic: bool,
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Function => write!(f, "function"),
            Self::Method => write!(f, "method"),
            Self::Class => write!(f, "class"),
            Self::Struct => write!(f, "struct"),
            Self::Interface => write!(f, "interface"),
            Self::TypeAlias => write!(f, "type alias"),
            Self::Enum => write!(f, "enum"),
            Self::EnumMember => write!(f, "enum member"),
            Self::Constant => write!(f, "constant"),
            Self::Variable => write!(f, "variable"),
            Self::Property => write!(f, "property"),
            Self::Constructor => write!(f, "constructor"),
            Self::GetAccessor => write!(f, "get accessor"),
            Self::SetAccessor => write!(f, "set accessor"),
            Self::Namespace => write!(f, "namespace"),
        }
    }
}

impl fmt::Display for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exported => write!(f, "exported"),
            Self::Public => write!(f, "public"),
            Self::Protected => write!(f, "protected"),
            Self::Internal => write!(f, "internal"),
            Self::Private => write!(f, "private"),
        }
    }
}
