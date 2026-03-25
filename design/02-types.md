# Core Types

## API Surface Model

These types represent a language's public API surface. They are populated by
a language crate's `ApiExtractor` and consumed by the diff engine. The types
are language-agnostic -- each field either applies universally or is naturally
`None`/`false`/empty for languages that don't have the concept.

### `ApiSurface`

The top-level container. One per git ref.

```rust
pub struct ApiSurface {
    pub symbols: Vec<Symbol>,
}
```

### `Symbol`

A single exported symbol in the API surface. Symbols form a tree: a `Class`
symbol has `members` which are themselves `Symbol` values (methods, properties,
enum members, etc.).

```rust
pub struct Symbol {
    /// Simple name (e.g., "createUser", "Button", "ClientOptions").
    pub name: String,

    /// Fully qualified name including module path.
    /// TS: "src/api/users.createUser". Go: "pkg/client.Client". Java: "com.foo.UserService".
    pub qualified_name: String,

    /// What kind of symbol this is.
    pub kind: SymbolKind,

    /// Visibility level.
    pub visibility: Visibility,

    /// Source file containing this symbol.
    pub file: PathBuf,

    /// Line number in the source file (1-indexed).
    pub line: usize,

    /// Function/method signature (None for non-callable symbols like constants).
    pub signature: Option<Signature>,

    /// Parent class/struct (`extends` clause).
    /// TS: `class EmailValidator extends BaseValidator` -> "BaseValidator"
    /// Java: `class ArrayList extends AbstractList` -> "AbstractList"
    /// Go/Python: None (Go has no inheritance; Python uses `implements` for base classes)
    pub extends: Option<String>,

    /// Implemented interfaces.
    /// TS: `class Foo implements Bar, Baz` -> ["Bar", "Baz"]
    /// Java: `class Foo implements Serializable` -> ["Serializable"]
    /// Go: implicit, but extractable
    pub implements: Vec<String>,

    /// Whether this symbol is abstract.
    /// TS, Java, C#: `abstract class Foo` or `abstract method()`.
    /// Python: `@abstractmethod`.
    /// Go: always false (no abstract concept).
    pub is_abstract: bool,

    /// Types referenced in this symbol's signature.
    /// Used for transitive impact analysis.
    pub type_dependencies: Vec<String>,

    /// Whether this member is readonly.
    /// TS: `readonly`. Java: `final`. C#: `readonly`.
    /// Go/Python: always false.
    pub is_readonly: bool,

    /// Whether this member is static.
    /// TS, Java, C#: `static`. Python: `@staticmethod`.
    /// Go: always false.
    pub is_static: bool,

    /// Accessor kind (get/set properties).
    /// TS: `get foo()`, `set foo()`. C#: `get { }`, `set { }`. Python: `@property`.
    /// Go/Java: always None.
    pub accessor_kind: Option<AccessorKind>,

    /// Child members (methods, properties, enum variants, struct fields).
    pub members: Vec<Symbol>,
}
```

**Design decision -- flat struct with optional fields vs. kind-specific structs:**

We considered having separate struct types per kind (e.g., `ClassSymbol`,
`FunctionSymbol`, `InterfaceSymbol`) but rejected this because:

1. The diff engine operates generically over `Symbol` -- it compares members,
   checks modifiers, and diffs signatures regardless of kind
2. Most fields are applicable to multiple kinds across languages (e.g., `extends`
   applies to classes in TS/Java/C# and interfaces in TS)
3. Unused fields default to `None`/`false`/empty, which is clean and correct
4. Only the `ApiExtractor` (language-specific) creates these -- so the
   "invalid state" of e.g., `is_abstract = true` on an enum member is a bug in
   the extractor, not a type system problem worth solving with separate structs

### `SymbolKind`

```rust
pub enum SymbolKind {
    Function,       // TS: `function foo()`. Go: `func foo()`. Python: `def foo()`.
    Method,         // TS: class method. Go: `func (r Recv) Method()`. Java: `void method()`.
    Class,          // TS, Java, Python, C#. Go: no classes (use Struct).
    Struct,         // Go, C#. NEW -- not in current codebase.
    Interface,      // TS, Go, Java, C#. Python: Protocol/ABC.
    TypeAlias,      // TS: `type Foo = ...`. Go: `type Foo = ...`. C#: `using Foo = ...`.
    Enum,           // TS, Java, Python, C#. Go: pseudo-enums via const iota.
    EnumMember,     // Member of an enum.
    Constant,       // TS: `const X`. Go: `const X`. Java: `static final X`.
    Variable,       // TS: `let/var`. Go: `var`. Java/C#: field.
    Property,       // Interface/class member, struct field. Also covers accessors.
    Constructor,    // TS, Java, C#, Python (`__init__`). Go: no constructors.
    Namespace,      // TS, C#. Go/Java/Python: packages/modules (not symbols).
}
```

**Changes from current design:**

| Change | Reasoning |
|--------|-----------|
| Added `Struct` | Go and C# distinguish structs from classes. Go has no classes at all -- everything is a struct. |
| Removed `GetAccessor`, `SetAccessor` | These were redundant with `Property` + the `accessor_kind` field on `Symbol`. The diff engine never pattern-matched on `GetAccessor`/`SetAccessor` -- it only compared `accessor_kind` for equality. Removing them simplifies the enum without losing information. |

### `Visibility`

```rust
pub enum Visibility {
    /// Explicitly private. TS: `private`/`#field`. Java: `private`. C#: `private`.
    Private,
    /// Module/package-internal. TS: not exported. Go: lowercase. Java: package-private.
    Internal,
    /// Accessible to subclasses. Java: `protected`. C#: `protected`. Python: `_prefix`.
    /// NEW -- not in current codebase.
    Protected,
    /// Public within the package. TS: class member. Java: `public`. C#: `public`.
    Public,
    /// Exported from the package for external consumers. TS: `export`. Go: uppercase.
    Exported,
}
```

**Change from current design:** Added `Protected`. Java, C#, and Python all have
a visibility level between internal and public. The current code maps TypeScript
`protected` to `Internal`, losing the distinction.

**Important:** The numeric ranking of visibility levels is NOT hardcoded. It is
provided by `LanguageSemantics::visibility_rank()` because the ordering differs
by language. Java's `protected` is more visible than package-private. C# has
additional levels (`private protected`, `protected internal`).

### `AccessorKind`

```rust
pub enum AccessorKind {
    Get,
    Set,
    GetSet,
}
```

Unchanged. Applies to TS, C#, and Python (`@property`). Go and Java don't use
accessor properties (Java uses getter/setter methods, which are modeled as
`Method` symbols).

### `Signature`

```rust
pub struct Signature {
    /// Ordered list of parameters.
    pub parameters: Vec<Parameter>,

    /// Return type as a canonicalized string. None if not annotated.
    /// Go: multiple returns modeled as tuple string "(User, error)".
    pub return_type: Option<String>,

    /// Generic type parameters.
    pub type_parameters: Vec<TypeParameter>,

    /// Whether the function is async.
    /// TS, Python, C#: marked with `async` keyword.
    /// Go: always false (goroutines, no async marker).
    /// Java: always false (returns CompletableFuture but no keyword).
    pub is_async: bool,
}
```

**Note on `is_async`:** This field is currently **never read** by the diff engine.
The current code detects async transitions by parsing the return type string
(`starts_with("Promise<")`). With the new design, async detection moves entirely
to the `MessageFormatter`. The `is_async` field is kept as metadata so the
formatter can use it directly instead of parsing return type strings.

### `Parameter`

```rust
pub struct Parameter {
    /// Parameter name.
    pub name: String,

    /// Type annotation as a canonicalized string.
    pub type_annotation: Option<String>,

    /// Whether the parameter is optional.
    /// TS: `param?: Type`. Python: `param: Type = None`. C#: nullable.
    /// Go/Java: always false (no optional parameters).
    pub optional: bool,

    /// Whether the parameter has a default value.
    /// TS, Python, C#: yes. Go/Java: always false.
    pub has_default: bool,

    /// The actual default value expression as a string.
    pub default_value: Option<String>,

    /// Whether this is a rest/variadic parameter.
    /// TS: `...args`. Go: `...args`. Java: `Type...`. Python: `*args`. C#: `params`.
    pub is_rest: bool,
}
```

### `TypeParameter`

```rust
pub struct TypeParameter {
    /// Name of the type parameter (e.g., "T").
    pub name: String,

    /// Constraint. TS/Java: `extends Foo`. Go: implicit. C#: `where T : Foo`.
    pub constraint: Option<String>,

    /// Default type. TS: `T = unknown`. C#: possible. Go/Java: always None.
    pub default: Option<String>,
}
```

---

## Diff Output Types

These types represent the output of the diff engine. They are language-agnostic
in structure -- the language-specific semantics are already applied during
diff computation (via `LanguageSemantics`) and description formatting
(via `MessageFormatter`).

### `StructuralChange`

The result of comparing a symbol between two API surface versions.

```rust
pub struct StructuralChange {
    /// The affected symbol name.
    pub symbol: String,

    /// Fully qualified symbol name.
    pub qualified_name: String,

    /// Symbol kind.
    pub kind: SymbolKind,

    /// What happened and to what.
    pub change_type: StructuralChangeType,

    /// Value before the change (type string, visibility level, etc.).
    pub before: Option<String>,

    /// Value after the change.
    pub after: Option<String>,

    /// Whether this change is breaking.
    pub is_breaking: bool,

    /// Human-readable description, populated by MessageFormatter.
    pub description: String,

    /// Migration target if a replacement was detected.
    pub migration_target: Option<MigrationTarget>,
}
```

### `StructuralChangeType`

Collapsed from 37 variants to 5. The change type says **what happened**. The
`ChangeSubject` says **what it happened to**. The `before`/`after` fields say
**what the values were**.

```rust
pub enum StructuralChangeType {
    Added(ChangeSubject),
    Removed(ChangeSubject),
    Changed(ChangeSubject),
    Renamed { from: ChangeSubject, to: ChangeSubject },
    Relocated { from: ChangeSubject, to: ChangeSubject },
}
```

**Why collapse from 37 to 5?**

The old 37-variant enum encoded the **what happened**, **what it happened to**,
and sometimes **presentation hints** (e.g., `MadeAsync` vs `ReturnTypeChanged`)
all in one variant name. With the `MessageFormatter` owning all descriptions,
the presentation hints are no longer needed in the enum. And with `ChangeSubject`
carrying the "what it happened to," the change type only needs to express the
lifecycle event.

**`Renamed` has `from` and `to` as separate `ChangeSubject` values** to support
cross-type renames. In React, a prop can become children (`Member { name: "title" }`
renamed to `Member { name: "children" }`). In Go, an options struct field could
become a functional option function. Keeping `from` and `to` as independent
subjects preserves this expressiveness.

### `ChangeSubject`

What aspect of a symbol was affected. The `symbol`/`qualified_name` on the
parent `StructuralChange` identifies the top-level symbol; the `ChangeSubject`
adds the specific sub-element context.

```rust
pub enum ChangeSubject {
    /// The symbol itself (added, removed, renamed, relocated).
    Symbol { kind: SymbolKind },

    /// A member of a container (property on interface, method on class,
    /// field on struct, variant on enum).
    Member { name: String, kind: SymbolKind },

    /// A parameter on a function/method.
    Parameter { name: String },

    /// The return type of a function/method.
    ReturnType,

    /// The visibility of a symbol.
    Visibility,

    /// A modifier on a symbol (readonly, abstract, static, accessor kind).
    Modifier { modifier: String },

    /// A generic type parameter.
    TypeParameter { name: String },

    /// The base class (`extends` clause).
    BaseClass,

    /// An interface implementation.
    InterfaceImpl { interface_name: String },

    /// A value in a union/constrained type.
    UnionValue { value: String },
}
```

**How the old 37 variants map:**

| Old variant | New representation |
|---|---|
| `SymbolRemoved` | `Removed(Symbol { kind })` |
| `SymbolRenamed` | `Renamed { from: Symbol { .. }, to: Symbol { .. } }` |
| `PropertyAdded` | `Added(Member { name, kind: Property })` |
| `EnumMemberRemoved` | `Removed(Member { name, kind: EnumMember })` |
| `ParameterTypeChanged` | `Changed(Parameter { name })` |
| `ReturnTypeChanged` | `Changed(ReturnType)` |
| `MadeAsync` | `Changed(ReturnType)` -- formatter decides description |
| `VisibilityReduced` | `Changed(Visibility)` |
| `ReadonlyAdded` | `Added(Modifier { modifier: "readonly" })` |
| `AbstractRemoved` | `Removed(Modifier { modifier: "abstract" })` |
| `BaseClassChanged` | `Changed(BaseClass)` |
| `InterfaceImplementationAdded` | `Added(InterfaceImpl { interface_name })` |
| `UnionMemberRemoved` | `Removed(UnionValue { value })` |
| `MigrationSuggested` | Metadata on `Removed(Symbol { .. })` via `migration_target` field |

### `MigrationTarget`

Detected when a removed symbol has a likely replacement in the same family,
based on member overlap analysis.

```rust
pub struct MigrationTarget {
    /// The symbol that was removed.
    pub removed_symbol: String,
    pub removed_qualified_name: String,

    /// The symbol that replaces it.
    pub replacement_symbol: String,
    pub replacement_qualified_name: String,

    /// Members that match between old and new.
    pub matching_members: Vec<MemberMapping>,

    /// Members from the removed symbol that have no match in the replacement.
    pub removed_only_members: Vec<String>,

    /// Ratio of matching members to total removed members.
    pub overlap_ratio: f64,
}

pub struct MemberMapping {
    pub old_name: String,
    pub new_name: String,
}
```

---

## Report Types (Generic over Language)

These types carry language-specific data through the analysis pipeline.
They are parameterized by `L: Language` and use its associated types.

### `BehavioralChange<L>`

A behavioral change detected by the BU (bottom-up) pipeline.

```rust
pub struct BehavioralChange<L: Language> {
    /// The function/method/class where the change occurs.
    pub symbol: String,

    /// The kind of symbol.
    pub kind: BehavioralChangeKind,

    /// Sub-category of the change. Language-defined.
    /// TS: DomStructure, CssClass, etc. Go: ErrorHandling, Concurrency, etc.
    pub category: Option<L::Category>,

    /// What changed and why it breaks consumers.
    pub description: String,

    /// Confidence score (0.0 to 1.0).
    pub confidence: f64,

    /// How the change was detected. Language-defined.
    /// TS: JsxDiff data, CSS scan, LLM analysis.
    /// Go: interface satisfaction, test delta, LLM analysis.
    pub evidence: L::Evidence,

    /// Whether this change only affects internal rendering.
    pub is_internal_only: bool,
}

pub enum BehavioralChangeKind {
    Function,
    Method,
    Class,
    Module,
}
```

### `ManifestChange<L>`

A change in the package manifest file.

```rust
pub struct ManifestChange<L: Language> {
    /// What field changed (e.g., "peerDependencies.react", "go 1.21").
    pub field: String,

    /// What kind of manifest change. Language-defined.
    /// TS: PeerDependencyAdded, ModuleSystemChanged, etc.
    /// Go: GoVersionChanged, RequireAdded, etc.
    pub change_type: L::ManifestChangeType,

    /// Value before the change.
    pub before: Option<String>,

    /// Value after the change.
    pub after: Option<String>,

    /// Human-readable description.
    pub description: String,

    /// Whether this change is breaking.
    pub is_breaking: bool,
}
```

### `LanguageReport<L>`

The language-specific section of the report, deserialized only by consumers
that know the language.

```rust
pub struct LanguageReport<L: Language> {
    /// Behavioral changes with language-specific categories and evidence.
    pub behavioral_changes: Vec<BehavioralChange<L>>,

    /// Manifest changes with language-specific change types.
    pub manifest_changes: Vec<ManifestChange<L>>,

    /// Framework-specific analysis data.
    /// TS: ComponentSummary, HierarchyDelta, CompositionPatternChange, etc.
    /// Go: PackageSummary, InterfaceSatisfactionReport, etc.
    pub data: L::ReportData,
}
```

---

## BU Pipeline Types (Unchanged)

These types are used internally by the bottom-up analysis pipeline. They are
language-agnostic (the `BehaviorAnalyzer` trait works with function bodies as
strings and produces generic specs).

```rust
pub struct ChangedFunction {
    pub qualified_name: String,
    pub name: String,
    pub file: PathBuf,
    pub line: usize,
    pub kind: SymbolKind,
    pub visibility: Visibility,
    pub old_body: String,
    pub new_body: String,
    pub old_signature: String,
    pub new_signature: String,
}

pub struct FunctionSpec {
    pub preconditions: Vec<Precondition>,
    pub postconditions: Vec<Postcondition>,
    pub error_behavior: Vec<ErrorBehavior>,
    pub side_effects: Vec<SideEffect>,
    pub notes: Vec<String>,
}

pub struct BreakingVerdict {
    pub is_breaking: bool,
    pub reasons: Vec<String>,
    pub confidence: f64,
}

pub struct TestDiff {
    pub test_file: PathBuf,
    pub removed_assertions: Vec<String>,
    pub added_assertions: Vec<String>,
    pub has_assertion_changes: bool,
    pub full_diff: String,
}
```
