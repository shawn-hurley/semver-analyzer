//! Core data structures for representing API surfaces and their components.
//!
//! These are language-agnostic: the per-language `ApiExtractor` implementations
//! populate them, and the language-agnostic `diff_surfaces()` engine consumes them.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Language-agnostic public API surface extracted from source code at a git ref.
/// Used by TD (Top-Down) pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiSurface {
    /// All exported symbols in the API surface.
    pub symbols: Vec<Symbol>,
}

/// A single exported symbol in the API surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Includes type-only imports which don't create runtime references
    /// but DO create API surface dependencies.
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeParameter {
    /// Name of the type parameter (e.g., "T").
    pub name: String,

    /// Constraint (e.g., "Serializable" from `T extends Serializable`).
    pub constraint: Option<String>,

    /// Default type (e.g., "unknown" from `T = unknown`).
    pub default: Option<String>,
}

/// A function/method parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// Whether this is a rest parameter (`...args`).
    pub is_rest: bool,
}
