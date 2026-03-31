//! Core data structures for representing API surfaces and their components.
//!
//! These are language-agnostic: the per-language `Language` implementations
//! populate them, and the language-agnostic `diff_surfaces_with_semantics()` engine consumes them.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// Language-agnostic public API surface extracted from source code at a git ref.
/// Used by TD (Top-Down) pipeline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApiSurface {
    /// All exported symbols in the API surface.
    pub symbols: Vec<Symbol>,
}

impl ApiSurface {
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
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
    pub members: Vec<Symbol>,

    // -- JSX render tree (for React components) --
    /// Components from the same package that this component renders internally
    /// in its JSX return tree. Determined by parsing the `.tsx` source file.
    ///
    /// Used for hierarchy inference: components in the same family that do NOT
    /// appear in this list are likely consumer-provided children.
    ///
    /// Only populated for Function/Variable/Constant symbols that represent
    /// React components with JSX render functions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rendered_components: Vec<String>,
}

impl Symbol {
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
            rendered_components: Vec::new(),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Directly exported (`export function ...` or `export { ... }`).
    Exported,
    /// Public class member (not `private` or `protected`).
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
