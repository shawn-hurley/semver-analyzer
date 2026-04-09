//! Type canonicalization for TypeScript type annotations.
//!
//! Applies 6 normalization rules on top of what `tsc --declaration` already
//! produces, so that string comparison of type annotations is reliable.
//!
//! The approach: parse the type string with OXC (by wrapping in `type T = ...;`),
//! then recursively walk the AST producing a canonical string in one pass.
//! This avoids fragile regex-based normalization.
//!
//! # Rules
//!
//! 1. **Union/Intersection Ordering**: Sort members alphabetically, flatten nested.
//! 2. **Array Syntax**: `Array<T>` → `T[]`, `ReadonlyArray<T>` → `readonly T[]`.
//! 3. **Parenthesization**: Remove unnecessary parens, keep required ones.
//! 4. **Whitespace**: Collapse to single spaces, remove trailing semicolons in object types.
//! 5. **never/unknown Absorption**: Apply TS type algebra identities.
//! 6. **Import Resolution**: Normalize namespace-qualified (`React.X`), direct imports
//!    (`X`), and import-type expressions (`import("react").X`) to the same canonical
//!    form using a per-file import map.

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use std::collections::HashMap;

/// Per-file import map for resolving type name aliases.
///
/// Built from the import declarations in a `.d.ts` file. Used during
/// canonicalization to normalize different representations of the same
/// type (e.g., `React.ReactNode` vs `ReactNode` vs `import("react").ReactNode`).
#[derive(Debug, Clone, Default)]
pub struct ImportMap {
    /// Maps local names to their resolved module and exported name.
    ///
    /// Examples:
    /// - `import React from 'react'`      → "React" → Default("react")
    /// - `import { ReactNode } from 'react'` → "ReactNode" → Named("react", "ReactNode")
    /// - `import * as React from 'react'` → "React" → Namespace("react")
    entries: HashMap<String, ImportEntry>,
}

/// A resolved import entry.
#[derive(Debug, Clone)]
pub enum ImportEntry {
    /// Default import: `import X from 'module'`
    Default(String),
    /// Named import: `import { X } from 'module'` or `import { Y as X } from 'module'`
    Named {
        module: String,
        original_name: String,
    },
    /// Namespace import: `import * as X from 'module'`
    Namespace(String),
}

impl ImportMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a default import: `import X from 'module'`
    pub fn add_default(&mut self, local_name: &str, module: &str) {
        self.entries.insert(
            local_name.to_string(),
            ImportEntry::Default(module.to_string()),
        );
    }

    /// Register a named import: `import { X } from 'module'` or `import { Y as X }`
    pub fn add_named(&mut self, local_name: &str, original_name: &str, module: &str) {
        self.entries.insert(
            local_name.to_string(),
            ImportEntry::Named {
                module: module.to_string(),
                original_name: original_name.to_string(),
            },
        );
    }

    /// Register a namespace import: `import * as X from 'module'`
    pub fn add_namespace(&mut self, local_name: &str, module: &str) {
        self.entries.insert(
            local_name.to_string(),
            ImportEntry::Namespace(module.to_string()),
        );
    }

    /// Check if a name is a namespace or default import (i.e., used as a qualifier).
    ///
    /// When we see `React.ReactNode`, if `React` is a namespace/default import,
    /// we know `.ReactNode` is the actual type name.
    pub fn is_namespace_or_default(&self, name: &str) -> bool {
        matches!(
            self.entries.get(name),
            Some(ImportEntry::Default(_) | ImportEntry::Namespace(_))
        )
    }

    /// Get the module path for a namespace or default import.
    pub fn module_for(&self, name: &str) -> Option<&str> {
        match self.entries.get(name) {
            Some(ImportEntry::Default(m) | ImportEntry::Namespace(m)) => Some(m),
            Some(ImportEntry::Named { module, .. }) => Some(module),
            None => None,
        }
    }

    /// Check if a direct name was imported from a module (named import).
    ///
    /// When we see bare `ReactNode`, and it was imported via
    /// `import { ReactNode } from 'react'`, this returns the module.
    pub fn named_import_module(&self, name: &str) -> Option<&str> {
        match self.entries.get(name) {
            Some(ImportEntry::Named { module, .. }) => Some(module),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Merge namespace and default import entries from another ImportMap.
    ///
    /// Only copies `Namespace` and `Default` entries (not `Named`, since named
    /// imports are file-specific). Does not overwrite existing entries — local
    /// imports take priority over global/merged entries.
    ///
    /// This is used for building a project-wide import map from the union of
    /// all per-file imports. If *any* file has `import * as React from 'react'`,
    /// we know `React.X` means `X` from `react` everywhere.
    pub fn merge_namespaces_from(&mut self, other: &ImportMap) {
        for (name, entry) in &other.entries {
            if matches!(entry, ImportEntry::Default(_) | ImportEntry::Namespace(_)) {
                self.entries
                    .entry(name.clone())
                    .or_insert_with(|| entry.clone());
            }
        }
    }

    /// Merge all entries from another ImportMap (fallback/base map).
    ///
    /// Does not overwrite existing entries — local imports take priority.
    pub fn merge_all_from(&mut self, other: &ImportMap) {
        for (name, entry) in &other.entries {
            self.entries
                .entry(name.clone())
                .or_insert_with(|| entry.clone());
        }
    }

    /// Iterate over all entries (for testing/inspection).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ImportEntry)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Number of entries in the map.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Canonicalize a TypeScript type annotation string.
///
/// Returns `None` if the type string cannot be parsed (malformed input).
/// In that case the caller should use the original string as-is.
pub fn canonicalize_type(type_str: &str) -> Option<String> {
    canonicalize_type_with_imports(type_str, None)
}

/// Canonicalize a TypeScript type annotation string with import resolution.
///
/// When an `ImportMap` is provided, namespace-qualified references (e.g.,
/// `React.ReactNode`), direct imports (`ReactNode`), and import-type
/// expressions (`import("react").ReactNode`) are all normalized to the
/// same canonical form.
pub fn canonicalize_type_with_imports(
    type_str: &str,
    imports: Option<&ImportMap>,
) -> Option<String> {
    let trimmed = type_str.trim();
    if trimmed.is_empty() {
        return Some(String::new());
    }

    let wrapped = format!("type T = {};", trimmed);
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, &wrapped, SourceType::d_ts()).parse();

    if !ret.errors.is_empty() {
        return None; // Parse failed — return None to signal "use original"
    }

    // Extract the TSType from `type T = <type>;`
    for stmt in &ret.program.body {
        if let Statement::TSTypeAliasDeclaration(alias) = stmt {
            return Some(emit_type(
                &wrapped,
                imports,
                &alias.type_annotation,
                Context::TopLevel,
            ));
        }
    }

    None
}

/// Context for serialization — controls whether parentheses are needed.
#[derive(Clone, Copy, PartialEq)]
enum Context {
    /// Top-level or inside parens/type args — no extra parens needed.
    TopLevel,
    /// Inside an array bracket — unions/intersections need parens.
    ArrayElement,
    /// Inside an intersection — unions need parens.
    IntersectionMember,
}

/// Emit a canonical string for a TSType node.
///
/// This is the core recursive function that applies all 6 rules in one pass.
fn emit_type(source: &str, imports: Option<&ImportMap>, ts_type: &TSType, ctx: Context) -> String {
    match ts_type {
        // ── Rule 1 + Rule 5: Union types ─────────────────────────────
        TSType::TSUnionType(u) => {
            // Flatten nested unions (Rule 1) and collect members
            let mut members = Vec::new();
            flatten_union(source, imports, u, &mut members);

            // Rule 5: never absorption in unions
            // `T | never` → `T` (never is identity for |)
            members.retain(|m| m != "never");

            // Rule 5: unknown absorption in unions
            // `T | unknown` → `unknown` (unknown absorbs in |)
            if members.iter().any(|m| m == "unknown") {
                return "unknown".to_string();
            }

            // If nothing left after filtering, result is `never`
            if members.is_empty() {
                return "never".to_string();
            }
            if members.len() == 1 {
                return members.into_iter().next().unwrap();
            }

            // Rule 1: Sort alphabetically
            members.sort();

            let result = members.join(" | ");

            // Rule 3: Add parens if needed in array element or intersection context
            if ctx == Context::ArrayElement || ctx == Context::IntersectionMember {
                format!("({})", result)
            } else {
                result
            }
        }

        // ── Rule 1 + Rule 5: Intersection types ─────────────────────
        TSType::TSIntersectionType(i) => {
            let mut members = Vec::new();
            flatten_intersection(source, imports, i, &mut members);

            // Rule 5: unknown absorption in intersections
            // `T & unknown` → `T` (unknown is identity for &)
            members.retain(|m| m != "unknown");

            // Rule 5: never absorption in intersections
            // `T & never` → `never` (never absorbs in &)
            if members.iter().any(|m| m == "never") {
                return "never".to_string();
            }

            if members.is_empty() {
                return "unknown".to_string();
            }
            if members.len() == 1 {
                return members.into_iter().next().unwrap();
            }

            // Rule 1: Sort alphabetically
            members.sort();

            let result = members.join(" & ");

            // Rule 3: Add parens if in array context
            if ctx == Context::ArrayElement {
                format!("({})", result)
            } else {
                result
            }
        }

        // ── Rule 2 + Rule 6: Type references ─────────────────────────
        TSType::TSTypeReference(r) => {
            let type_name = ts_type_name_string(source, imports, &r.type_name);

            // Rule 2: Array<T> → T[]
            if type_name == "Array" {
                if let Some(type_args) = &r.type_arguments {
                    if type_args.params.len() == 1 {
                        let inner =
                            emit_type(source, imports, &type_args.params[0], Context::ArrayElement);
                        return format!("{}[]", inner);
                    }
                }
                // Array without type arg — keep as-is
                return "Array".to_string();
            }

            // Rule 2: ReadonlyArray<T> → readonly T[]
            if type_name == "ReadonlyArray" {
                if let Some(type_args) = &r.type_arguments {
                    if type_args.params.len() == 1 {
                        let inner =
                            emit_type(source, imports, &type_args.params[0], Context::ArrayElement);
                        return format!("readonly {}[]", inner);
                    }
                }
                return "ReadonlyArray".to_string();
            }

            // Rule 7: Strip default generic parameters.
            // When ALL type arguments are `any`, the generic parameters are
            // TypeScript defaults and can be omitted.  For example,
            // `ReactElement<any>` is identical to `ReactElement` because
            // `any` is the default type parameter.  Stripping these avoids
            // false positive type-change detections when a `.d.ts` file
            // makes the default explicit.
            if let Some(type_args) = r.type_arguments.as_ref().filter(|ta| {
                !ta.params
                    .iter()
                    .all(|a| matches!(a, TSType::TSAnyKeyword(_)))
            }) {
                let args: Vec<String> = type_args
                    .params
                    .iter()
                    .map(|a| emit_type(source, imports, a, Context::TopLevel))
                    .collect();
                format!("{}<{}>", type_name, args.join(", "))
            } else {
                type_name
            }
        }

        // ── Array type (already shorthand): T[] ──────────────────────
        TSType::TSArrayType(a) => {
            let inner = emit_type(source, imports, &a.element_type, Context::ArrayElement);
            format!("{}[]", inner)
        }

        // ── Rule 3: Parenthesized types ──────────────────────────────
        TSType::TSParenthesizedType(p) => {
            // Remove unnecessary parentheses by just emitting the inner type
            // The inner type will add parens if needed based on context
            emit_type(source, imports, &p.type_annotation, ctx)
        }

        // ── Tuple types ──────────────────────────────────────────────
        TSType::TSTupleType(t) => {
            let elements: Vec<String> = t
                .element_types
                .iter()
                .map(|e| emit_tuple_element(source, imports, e))
                .collect();
            format!("[{}]", elements.join(", "))
        }

        // ── Object type literals (Rule 4: whitespace) ────────────────
        TSType::TSTypeLiteral(lit) => {
            if lit.members.is_empty() {
                return "{}".to_string();
            }
            let members: Vec<String> = lit
                .members
                .iter()
                .map(|m| emit_ts_signature(source, imports, m))
                .collect();
            // Rule 4: normalize whitespace, no trailing semicolons
            format!("{{ {} }}", members.join("; "))
        }

        // ── Function types ───────────────────────────────────────────
        TSType::TSFunctionType(f) => {
            let type_params = f
                .type_parameters
                .as_ref()
                .map(|tp| emit_type_params(source, imports, tp))
                .unwrap_or_default();
            let params = emit_formal_params(source, imports, &f.params);
            let ret = emit_type(
                source,
                imports,
                &f.return_type.type_annotation,
                Context::TopLevel,
            );
            format!("{}({}) => {}", type_params, params, ret)
        }

        // ── Constructor types ────────────────────────────────────────
        TSType::TSConstructorType(c) => {
            let type_params = c
                .type_parameters
                .as_ref()
                .map(|tp| emit_type_params(source, imports, tp))
                .unwrap_or_default();
            let params = emit_formal_params(source, imports, &c.params);
            let ret = emit_type(
                source,
                imports,
                &c.return_type.type_annotation,
                Context::TopLevel,
            );
            format!("new {}({}) => {}", type_params, params, ret)
        }

        // ── Conditional types ────────────────────────────────────────
        TSType::TSConditionalType(c) => {
            let check = emit_type(source, imports, &c.check_type, Context::TopLevel);
            let extends = emit_type(source, imports, &c.extends_type, Context::TopLevel);
            let true_type = emit_type(source, imports, &c.true_type, Context::TopLevel);
            let false_type = emit_type(source, imports, &c.false_type, Context::TopLevel);
            format!(
                "{} extends {} ? {} : {}",
                check, extends, true_type, false_type
            )
        }

        // ── Mapped types ─────────────────────────────────────────────
        TSType::TSMappedType(m) => {
            let readonly_prefix = match m.readonly {
                Some(TSMappedTypeModifierOperator::Plus)
                | Some(TSMappedTypeModifierOperator::True) => "readonly ",
                Some(TSMappedTypeModifierOperator::Minus) => "-readonly ",
                _ => "",
            };

            let key = span_text(source, m.key.span());
            let constraint = emit_type(source, imports, &m.constraint, Context::TopLevel);

            let name_type = m
                .name_type
                .as_ref()
                .map(|nt| format!(" as {}", emit_type(source, imports, nt, Context::TopLevel)))
                .unwrap_or_default();

            let optional_suffix = match m.optional {
                Some(TSMappedTypeModifierOperator::Plus)
                | Some(TSMappedTypeModifierOperator::True) => "?",
                Some(TSMappedTypeModifierOperator::Minus) => "-?",
                _ => "",
            };

            let value_type = m
                .type_annotation
                .as_ref()
                .map(|ta| format!(": {}", emit_type(source, imports, ta, Context::TopLevel)))
                .unwrap_or_default();

            format!(
                "{{ {}[{} in {}{}]{}{} }}",
                readonly_prefix, key, constraint, name_type, optional_suffix, value_type
            )
        }

        // ── Indexed access types ─────────────────────────────────────
        TSType::TSIndexedAccessType(idx) => {
            let obj = emit_type(source, imports, &idx.object_type, Context::TopLevel);
            let index = emit_type(source, imports, &idx.index_type, Context::TopLevel);
            format!("{}[{}]", obj, index)
        }

        // ── Type operator (keyof, unique, readonly) ──────────────────
        TSType::TSTypeOperatorType(op) => {
            let operator = match op.operator {
                TSTypeOperatorOperator::Keyof => "keyof",
                TSTypeOperatorOperator::Unique => "unique",
                TSTypeOperatorOperator::Readonly => "readonly",
            };
            let inner = emit_type(source, imports, &op.type_annotation, Context::TopLevel);
            format!("{} {}", operator, inner)
        }

        // ── Infer types ──────────────────────────────────────────────
        TSType::TSInferType(infer) => {
            let name = &infer.type_parameter.name;
            if let Some(constraint) = &infer.type_parameter.constraint {
                let c = emit_type(source, imports, constraint, Context::TopLevel);
                format!("infer {} extends {}", name, c)
            } else {
                format!("infer {}", name)
            }
        }

        // ── Template literal types ───────────────────────────────────
        TSType::TSTemplateLiteralType(tpl) => {
            // Reconstruct template literal from quasis and types
            let mut result = String::from("`");
            for (i, quasi) in tpl.quasis.iter().enumerate() {
                result.push_str(&quasi.value.raw);
                if i < tpl.types.len() {
                    result.push_str("${");
                    result.push_str(&emit_type(
                        source,
                        imports,
                        &tpl.types[i],
                        Context::TopLevel,
                    ));
                    result.push('}');
                }
            }
            result.push('`');
            result
        }

        // ── Type query (typeof) ──────────────────────────────────────
        TSType::TSTypeQuery(q) => {
            let name = type_query_expr_name_string(source, imports, &q.expr_name);
            if let Some(type_args) = &q.type_arguments {
                let args: Vec<String> = type_args
                    .params
                    .iter()
                    .map(|a| emit_type(source, imports, a, Context::TopLevel))
                    .collect();
                format!("typeof {}<{}>", name, args.join(", "))
            } else {
                format!("typeof {}", name)
            }
        }

        // ── Rule 6: Import type expressions ──────────────────────────
        TSType::TSImportType(import) => {
            // import("react").Context<T> → Context<T>
            // The qualifier after import() is the actual type name.
            // We strip the import() wrapper to normalize with namespace imports.
            if let Some(qualifier) = &import.qualifier {
                let qual_str = import_qualifier_to_string(qualifier);
                if let Some(type_args) = &import.type_arguments {
                    let args: Vec<String> = type_args
                        .params
                        .iter()
                        .map(|a| emit_type(source, imports, a, Context::TopLevel))
                        .collect();
                    format!("{}<{}>", qual_str, args.join(", "))
                } else {
                    qual_str
                }
            } else {
                // No qualifier: `import("module")` alone — keep as source text
                span_text(source, import.span()).to_string()
            }
        }

        // ── Type predicate ───────────────────────────────────────────
        TSType::TSTypePredicate(pred) => {
            let param_name = match &pred.parameter_name {
                TSTypePredicateName::Identifier(id) => id.name.to_string(),
                TSTypePredicateName::This(_) => "this".to_string(),
            };
            if let Some(ta) = &pred.type_annotation {
                let type_str = emit_type(source, imports, &ta.type_annotation, Context::TopLevel);
                if pred.asserts {
                    format!("asserts {} is {}", param_name, type_str)
                } else {
                    format!("{} is {}", param_name, type_str)
                }
            } else if pred.asserts {
                format!("asserts {}", param_name)
            } else {
                // Bare type predicate without annotation — unusual
                param_name
            }
        }

        // ── Named tuple member ───────────────────────────────────────
        TSType::TSNamedTupleMember(named) => emit_tuple_element_inner(source, imports, named),

        // ── Literal types ────────────────────────────────────────────
        TSType::TSLiteralType(lit) => {
            // Use source text for literals (strings, numbers, booleans, etc.)
            span_text(source, lit.span()).to_string()
        }

        // ── Keyword types — canonical forms ──────────────────────────
        TSType::TSAnyKeyword(_) => "any".to_string(),
        TSType::TSBigIntKeyword(_) => "bigint".to_string(),
        TSType::TSBooleanKeyword(_) => "boolean".to_string(),
        TSType::TSNeverKeyword(_) => "never".to_string(),
        TSType::TSNullKeyword(_) => "null".to_string(),
        TSType::TSNumberKeyword(_) => "number".to_string(),
        TSType::TSObjectKeyword(_) => "object".to_string(),
        TSType::TSStringKeyword(_) => "string".to_string(),
        TSType::TSSymbolKeyword(_) => "symbol".to_string(),
        TSType::TSUndefinedKeyword(_) => "undefined".to_string(),
        TSType::TSUnknownKeyword(_) => "unknown".to_string(),
        TSType::TSVoidKeyword(_) => "void".to_string(),
        TSType::TSIntrinsicKeyword(_) => "intrinsic".to_string(),
        TSType::TSThisType(_) => "this".to_string(),

        // ── JSDoc types (rare in .d.ts) ──────────────────────────────
        TSType::JSDocNullableType(jsdoc) => {
            let inner = emit_type(source, imports, &jsdoc.type_annotation, Context::TopLevel);
            format!("{}?", inner)
        }
        TSType::JSDocNonNullableType(jsdoc) => {
            let inner = emit_type(source, imports, &jsdoc.type_annotation, Context::TopLevel);
            format!("{}!", inner)
        }
        TSType::JSDocUnknownType(_) => "?".to_string(),
    }
}

// ─── Flatten helpers (Rule 1) ────────────────────────────────────────────

/// Flatten nested union types and collect canonical string for each member.
fn flatten_union(
    source: &str,
    imports: Option<&ImportMap>,
    union: &TSUnionType,
    out: &mut Vec<String>,
) {
    for member in &union.types {
        match member {
            TSType::TSUnionType(inner) => {
                flatten_union(source, imports, inner, out);
            }
            TSType::TSParenthesizedType(p) => {
                // Unwrap parens and check if the inner type is also a union
                if let TSType::TSUnionType(inner) = &p.type_annotation {
                    flatten_union(source, imports, inner, out);
                } else {
                    out.push(emit_type(
                        source,
                        imports,
                        &p.type_annotation,
                        Context::TopLevel,
                    ));
                }
            }
            _ => {
                // Each member is emitted at TopLevel context (no extra parens)
                // because the union itself handles grouping
                out.push(emit_type(source, imports, member, Context::TopLevel));
            }
        }
    }
}

/// Flatten nested intersection types and collect canonical string for each member.
fn flatten_intersection(
    source: &str,
    imports: Option<&ImportMap>,
    inter: &TSIntersectionType,
    out: &mut Vec<String>,
) {
    for member in &inter.types {
        match member {
            TSType::TSIntersectionType(inner) => {
                flatten_intersection(source, imports, inner, out);
            }
            TSType::TSParenthesizedType(p) => {
                if let TSType::TSIntersectionType(inner) = &p.type_annotation {
                    flatten_intersection(source, imports, inner, out);
                } else {
                    out.push(emit_type(
                        source,
                        imports,
                        &p.type_annotation,
                        Context::IntersectionMember,
                    ));
                }
            }
            _ => {
                out.push(emit_type(
                    source,
                    imports,
                    member,
                    Context::IntersectionMember,
                ));
            }
        }
    }
}

// ─── Tuple element emission ──────────────────────────────────────────────

fn emit_tuple_element(source: &str, imports: Option<&ImportMap>, elem: &TSTupleElement) -> String {
    match elem {
        TSTupleElement::TSOptionalType(opt) => {
            format!(
                "{}?",
                emit_type(source, imports, &opt.type_annotation, Context::TopLevel)
            )
        }
        TSTupleElement::TSRestType(rest) => {
            format!(
                "...{}",
                emit_type(source, imports, &rest.type_annotation, Context::TopLevel)
            )
        }
        TSTupleElement::TSNamedTupleMember(named) => {
            emit_tuple_element_inner(source, imports, named)
        }
        // Most other variants correspond to TSType variants
        _ => {
            // For unmapped variants, use the span text and try to canonicalize it
            let raw = span_text(source, elem.span());
            canonicalize_type(raw).unwrap_or_else(|| raw.to_string())
        }
    }
}

fn emit_tuple_element_inner(
    source: &str,
    imports: Option<&ImportMap>,
    named: &TSNamedTupleMember,
) -> String {
    let name = &named.label.name;
    let type_str = emit_tuple_element(source, imports, &named.element_type);
    if named.optional {
        format!("{}?: {}", name, type_str)
    } else {
        format!("{}: {}", name, type_str)
    }
}

// ─── Signature member emission (for object type literals) ────────────────

fn emit_ts_signature(source: &str, imports: Option<&ImportMap>, sig: &TSSignature) -> String {
    match sig {
        TSSignature::TSPropertySignature(prop) => {
            let key = emit_property_key(source, &prop.key);
            let optional = if prop.optional { "?" } else { "" };
            let readonly = if prop.readonly { "readonly " } else { "" };
            if let Some(ta) = &prop.type_annotation {
                let type_str = emit_type(source, imports, &ta.type_annotation, Context::TopLevel);
                format!("{}{}{}: {}", readonly, key, optional, type_str)
            } else {
                format!("{}{}{}", readonly, key, optional)
            }
        }
        TSSignature::TSMethodSignature(method) => {
            let key = emit_property_key(source, &method.key);
            let optional = if method.optional { "?" } else { "" };
            let type_params = method
                .type_parameters
                .as_ref()
                .map(|tp| emit_type_params(source, imports, tp))
                .unwrap_or_default();
            let params = emit_formal_params(source, imports, &method.params);
            let ret = method
                .return_type
                .as_ref()
                .map(|ta| {
                    format!(
                        ": {}",
                        emit_type(source, imports, &ta.type_annotation, Context::TopLevel)
                    )
                })
                .unwrap_or_default();
            format!("{}{}{}({}){}", key, optional, type_params, params, ret)
        }
        TSSignature::TSIndexSignature(idx) => {
            let params: Vec<String> = idx
                .parameters
                .iter()
                .map(|p| {
                    let name = &p.name;
                    let type_str = emit_type(
                        source,
                        imports,
                        &p.type_annotation.type_annotation,
                        Context::TopLevel,
                    );
                    format!("{}: {}", name, type_str)
                })
                .collect();
            let readonly = if idx.readonly { "readonly " } else { "" };
            let type_str = emit_type(
                source,
                imports,
                &idx.type_annotation.type_annotation,
                Context::TopLevel,
            );
            format!("{}[{}]: {}", readonly, params.join(", "), type_str)
        }
        TSSignature::TSCallSignatureDeclaration(call) => {
            let type_params = call
                .type_parameters
                .as_ref()
                .map(|tp| emit_type_params(source, imports, tp))
                .unwrap_or_default();
            let params = emit_formal_params(source, imports, &call.params);
            let ret = call
                .return_type
                .as_ref()
                .map(|ta| {
                    format!(
                        ": {}",
                        emit_type(source, imports, &ta.type_annotation, Context::TopLevel)
                    )
                })
                .unwrap_or_default();
            format!("{}({}){}", type_params, params, ret)
        }
        TSSignature::TSConstructSignatureDeclaration(ctor) => {
            let type_params = ctor
                .type_parameters
                .as_ref()
                .map(|tp| emit_type_params(source, imports, tp))
                .unwrap_or_default();
            let params = emit_formal_params(source, imports, &ctor.params);
            let ret = ctor
                .return_type
                .as_ref()
                .map(|ta| {
                    format!(
                        ": {}",
                        emit_type(source, imports, &ta.type_annotation, Context::TopLevel)
                    )
                })
                .unwrap_or_default();
            format!("new {}({}){}", type_params, params, ret)
        }
    }
}

// ─── Helper emission functions ───────────────────────────────────────────

fn emit_property_key(source: &str, key: &PropertyKey) -> String {
    match key {
        PropertyKey::StaticIdentifier(id) => id.name.to_string(),
        PropertyKey::PrivateIdentifier(id) => format!("#{}", id.name),
        _ => {
            // Computed property key — use source text
            format!("[{}]", span_text(source, key.span()))
        }
    }
}

fn emit_type_params(
    source: &str,
    imports: Option<&ImportMap>,
    tp: &TSTypeParameterDeclaration,
) -> String {
    let params: Vec<String> = tp
        .params
        .iter()
        .map(|p| {
            let mut s = p.name.to_string();
            if let Some(constraint) = &p.constraint {
                s.push_str(" extends ");
                s.push_str(&emit_type(source, imports, constraint, Context::TopLevel));
            }
            if let Some(default) = &p.default {
                s.push_str(" = ");
                s.push_str(&emit_type(source, imports, default, Context::TopLevel));
            }
            s
        })
        .collect();
    format!("<{}>", params.join(", "))
}

fn emit_formal_params(
    source: &str,
    imports: Option<&ImportMap>,
    params: &FormalParameters,
) -> String {
    let mut parts: Vec<String> = params
        .items
        .iter()
        .map(|p| emit_formal_param(source, imports, p))
        .collect();

    if let Some(rest) = &params.rest {
        let name = match &rest.rest.argument {
            BindingPattern::BindingIdentifier(id) => id.name.to_string(),
            _ => "<destructured>".to_string(),
        };
        let type_str = rest
            .type_annotation
            .as_ref()
            .map(|ta| {
                format!(
                    ": {}",
                    emit_type(source, imports, &ta.type_annotation, Context::TopLevel)
                )
            })
            .unwrap_or_default();
        parts.push(format!("...{}{}", name, type_str));
    }

    parts.join(", ")
}

fn emit_formal_param(source: &str, imports: Option<&ImportMap>, param: &FormalParameter) -> String {
    let name = match &param.pattern {
        BindingPattern::BindingIdentifier(id) => id.name.to_string(),
        _ => "<destructured>".to_string(),
    };
    let optional = if param.optional { "?" } else { "" };
    let type_str = param
        .type_annotation
        .as_ref()
        .map(|ta| {
            format!(
                ": {}",
                emit_type(source, imports, &ta.type_annotation, Context::TopLevel)
            )
        })
        .unwrap_or_default();
    format!("{}{}{}", name, optional, type_str)
}

/// Resolve a TSTypeName to a canonical string.
///
/// Rule 6 (import resolution): When an import map is provided:
/// - `React.ReactNode` where `React` is imported from `'react'`
///   → strips the namespace qualifier, producing just `ReactNode`
/// - Deeper qualified names like `React.JSX.Element` → `JSX.Element`
/// - Direct imports like `ReactNode` are kept as-is (they're already canonical)
fn ts_type_name_string(source: &str, imports: Option<&ImportMap>, name: &TSTypeName) -> String {
    match name {
        TSTypeName::IdentifierReference(id) => id.name.to_string(),
        TSTypeName::QualifiedName(qn) => {
            // Check if the leftmost part is a namespace/default import
            if let Some(imports) = imports {
                if let Some(root_id) = leftmost_identifier(&qn.left) {
                    if imports.is_namespace_or_default(&root_id) {
                        // Strip the import qualifier: React.ReactNode → ReactNode
                        // For deeper paths: React.JSX.Element → JSX.Element
                        return strip_import_prefix(
                            source,
                            Some(imports),
                            &qn.left,
                            &qn.right.name,
                        );
                    }
                }
            }
            let left = ts_type_name_string(source, imports, &qn.left);
            format!("{}.{}", left, qn.right.name)
        }
        TSTypeName::ThisExpression(_) => "this".to_string(),
    }
}

/// Resolve a `TSTypeQueryExprName` (the expression inside `typeof`).
///
/// `TSTypeQueryExprName` inherits `TSTypeName` variants (IdentifierReference,
/// QualifiedName, ThisExpression) plus `TSImportType`. We delegate to the
/// same import-stripping logic used for type references.
///
/// Examples:
/// - `typeof React.useEffect` → `typeof useEffect` (when React is a namespace import)
/// - `typeof import("react").useEffect` → `typeof useEffect`
fn type_query_expr_name_string(
    source: &str,
    imports: Option<&ImportMap>,
    expr: &TSTypeQueryExprName,
) -> String {
    match expr {
        TSTypeQueryExprName::IdentifierReference(id) => id.name.to_string(),
        TSTypeQueryExprName::QualifiedName(qn) => {
            // Same logic as ts_type_name_string for qualified names
            if let Some(imports) = imports {
                if let Some(root_id) = leftmost_identifier(&qn.left) {
                    if imports.is_namespace_or_default(&root_id) {
                        return strip_import_prefix(
                            source,
                            Some(imports),
                            &qn.left,
                            &qn.right.name,
                        );
                    }
                }
            }
            let left = ts_type_name_string(source, imports, &qn.left);
            format!("{}.{}", left, qn.right.name)
        }
        TSTypeQueryExprName::TSImportType(import) => {
            // typeof import("react").useEffect → useEffect
            if let Some(qualifier) = &import.qualifier {
                import_qualifier_to_string(qualifier)
            } else {
                span_text(source, import.span()).to_string()
            }
        }
        TSTypeQueryExprName::ThisExpression(_) => "this".to_string(),
    }
}

/// Get the leftmost identifier in a TSTypeName chain.
/// `React.JSX.Element` → `React`
fn leftmost_identifier(name: &TSTypeName) -> Option<String> {
    match name {
        TSTypeName::IdentifierReference(id) => Some(id.name.to_string()),
        TSTypeName::QualifiedName(qn) => leftmost_identifier(&qn.left),
        TSTypeName::ThisExpression(_) => None,
    }
}

/// Strip the import namespace prefix from a qualified name.
/// Given `React.JSX.Element` where `React` is a namespace import:
/// - If left is just `React` (the import), return `right` = "Element" → wait, need the middle
/// - Actually: left = QualifiedName(React, JSX), right = Element
///   → leftmost of left is React (namespace import)
///   → strip React, keep JSX.Element
fn strip_import_prefix(
    _source: &str,
    _imports: Option<&ImportMap>,
    left: &TSTypeName,
    right: &str,
) -> String {
    match left {
        TSTypeName::IdentifierReference(_) => {
            // Left is just the import name (e.g., `React`), right is the type
            // React.ReactNode → ReactNode
            right.to_string()
        }
        TSTypeName::QualifiedName(qn) => {
            // Left is deeper (e.g., `React.JSX`), right is `Element`
            // We need to strip just the leftmost import name
            let remaining = strip_qualified_left(&qn.left, &qn.right.name);
            format!("{}.{}", remaining, right)
        }
        TSTypeName::ThisExpression(_) => right.to_string(),
    }
}

/// Recursively strip the leftmost segment from a qualified name.
fn strip_qualified_left(left: &TSTypeName, right: &str) -> String {
    match left {
        TSTypeName::IdentifierReference(_) => {
            // This is the import name — return just the right part
            right.to_string()
        }
        TSTypeName::QualifiedName(qn) => {
            let remaining = strip_qualified_left(&qn.left, &qn.right.name);
            format!("{}.{}", remaining, right)
        }
        TSTypeName::ThisExpression(_) => right.to_string(),
    }
}

/// Convert a TSImportTypeQualifier to a string.
///
/// Handles `import("react").Context` → "Context"
/// and `import("react").JSX.Element` → "JSX.Element"
fn import_qualifier_to_string(qualifier: &TSImportTypeQualifier) -> String {
    match qualifier {
        TSImportTypeQualifier::Identifier(id) => id.name.to_string(),
        TSImportTypeQualifier::QualifiedName(qn) => {
            let left = import_qualifier_to_string(&qn.left);
            format!("{}.{}", left, qn.right.name)
        }
    }
}

fn span_text(source: &str, span: oxc_span::Span) -> &str {
    &source[span.start as usize..span.end as usize]
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(input: &str) -> String {
        canonicalize_type(input).unwrap_or_else(|| panic!("Failed to parse: {}", input))
    }

    // ── Rule 1: Union/Intersection ordering ──────────────────────────

    #[test]
    fn union_sorted_alphabetically() {
        assert_eq!(canon("string | number"), "number | string");
    }

    #[test]
    fn union_already_sorted() {
        assert_eq!(canon("number | string"), "number | string");
    }

    #[test]
    fn union_three_members() {
        assert_eq!(
            canon("string | number | boolean"),
            "boolean | number | string"
        );
    }

    #[test]
    fn union_flattened() {
        assert_eq!(canon("(C | A) | B"), "A | B | C");
    }

    #[test]
    fn intersection_sorted() {
        assert_eq!(canon("B & A"), "A & B");
    }

    #[test]
    fn intersection_flattened() {
        assert_eq!(canon("(C & A) & B"), "A & B & C");
    }

    #[test]
    fn union_of_intersections() {
        // Each intersection member is sorted, then union members are sorted.
        // In TS, & has higher precedence than |, so A & B | C & D is unambiguous.
        assert_eq!(canon("(B & A) | (D & C)"), "A & B | C & D");
    }

    #[test]
    fn intersection_of_unions() {
        // Unions within intersections need parens because | has lower precedence.
        assert_eq!(canon("(B | A) & (D | C)"), "(A | B) & (C | D)");
    }

    // ── Rule 2: Array syntax ─────────────────────────────────────────

    #[test]
    fn array_generic_to_shorthand() {
        assert_eq!(canon("Array<string>"), "string[]");
    }

    #[test]
    fn readonly_array_to_shorthand() {
        assert_eq!(canon("ReadonlyArray<Item>"), "readonly Item[]");
    }

    #[test]
    fn array_of_union_gets_parens() {
        // Array<string | number> → (number | string)[] (sorted union, parenthesized)
        assert_eq!(canon("Array<string | number>"), "(number | string)[]");
    }

    #[test]
    fn array_shorthand_preserved() {
        assert_eq!(canon("string[]"), "string[]");
    }

    #[test]
    fn nested_array() {
        assert_eq!(canon("Array<Array<string>>"), "string[][]");
    }

    // ── Rule 3: Parenthesization ─────────────────────────────────────

    #[test]
    fn unnecessary_parens_removed() {
        assert_eq!(canon("(string)"), "string");
    }

    #[test]
    fn double_parens_removed() {
        assert_eq!(canon("((string))"), "string");
    }

    #[test]
    fn union_in_array_keeps_parens() {
        assert_eq!(canon("(string | number)[]"), "(number | string)[]");
    }

    // ── Rule 4: Whitespace normalization ─────────────────────────────

    #[test]
    fn object_type_whitespace_normalized() {
        assert_eq!(
            canon("{  a :  string ;  b :  number ;  }"),
            "{ a: string; b: number }"
        );
    }

    #[test]
    fn empty_object_type() {
        assert_eq!(canon("{}"), "{}");
    }

    // ── Rule 5: never/unknown absorption ─────────────────────────────

    #[test]
    fn union_never_absorbed() {
        assert_eq!(canon("string | never"), "string");
    }

    #[test]
    fn union_unknown_absorbs() {
        assert_eq!(canon("string | unknown"), "unknown");
    }

    #[test]
    fn intersection_unknown_absorbed() {
        assert_eq!(canon("string & unknown"), "string");
    }

    #[test]
    fn intersection_never_absorbs() {
        assert_eq!(canon("string & never"), "never");
    }

    #[test]
    fn union_all_never() {
        assert_eq!(canon("never | never"), "never");
    }

    #[test]
    fn intersection_all_unknown() {
        assert_eq!(canon("unknown & unknown"), "unknown");
    }

    // ── Complex types ────────────────────────────────────────────────

    #[test]
    fn generic_type_reference() {
        assert_eq!(canon("Promise<string>"), "Promise<string>");
    }

    #[test]
    fn map_type() {
        assert_eq!(canon("Map<string, number>"), "Map<string, number>");
    }

    #[test]
    fn tuple_type() {
        assert_eq!(canon("[string, number]"), "[string, number]");
    }

    #[test]
    fn function_type() {
        assert_eq!(
            canon("(x: string, y: number) => boolean"),
            "(x: string, y: number) => boolean"
        );
    }

    #[test]
    fn conditional_type() {
        assert_eq!(
            canon("T extends string ? true : false"),
            "T extends string ? true : false"
        );
    }

    #[test]
    fn indexed_access_type() {
        assert_eq!(canon("T[K]"), "T[K]");
    }

    #[test]
    fn typeof_query() {
        assert_eq!(canon("typeof window"), "typeof window");
    }

    #[test]
    fn keyof_operator() {
        assert_eq!(canon("keyof T"), "keyof T");
    }

    #[test]
    fn template_literal_type() {
        assert_eq!(canon("`hello-${string}`"), "`hello-${string}`");
    }

    #[test]
    fn literal_types() {
        assert_eq!(canon("42"), "42");
        assert_eq!(canon("\"hello\""), "\"hello\"");
        assert_eq!(canon("true"), "true");
    }

    #[test]
    fn keyword_types_idempotent() {
        for kw in &[
            "any",
            "bigint",
            "boolean",
            "never",
            "null",
            "number",
            "object",
            "string",
            "symbol",
            "undefined",
            "unknown",
            "void",
        ] {
            assert_eq!(canon(kw), *kw, "keyword '{}' should be idempotent", kw);
        }
    }

    #[test]
    fn infer_type() {
        assert_eq!(canon("infer T"), "infer T");
    }

    #[test]
    fn infer_type_with_constraint() {
        assert_eq!(canon("infer T extends string"), "infer T extends string");
    }

    // ── Idempotency ──────────────────────────────────────────────────

    #[test]
    fn canonicalization_is_idempotent() {
        let cases = [
            "number | string",
            "string[]",
            "readonly string[]",
            "(number | string)[]",
            "{ a: string; b: number }",
            "Map<string, number[]>",
        ];
        for case in &cases {
            let first = canon(case);
            let second = canon(&first);
            assert_eq!(first, second, "Not idempotent for: {}", case);
        }
    }

    // ── Parse failure returns None ───────────────────────────────────

    #[test]
    fn malformed_input_returns_none() {
        assert!(canonicalize_type(">>>invalid<<<").is_none());
    }

    #[test]
    fn empty_input() {
        assert_eq!(canonicalize_type(""), Some(String::new()));
    }

    // ── Combined rules ───────────────────────────────────────────────

    #[test]
    fn array_of_union_with_never() {
        // Array<string | never | number> → (number | string)[]
        assert_eq!(
            canon("Array<string | never | number>"),
            "(number | string)[]"
        );
    }

    #[test]
    fn nested_generics_with_sorting() {
        assert_eq!(
            canon("Promise<string | number>"),
            "Promise<number | string>"
        );
    }

    #[test]
    fn readonly_array_of_sorted_union() {
        assert_eq!(
            canon("ReadonlyArray<string | number>"),
            "readonly (number | string)[]"
        );
    }

    #[test]
    fn type_predicate() {
        // Type predicates are only valid in return position, not as type aliases.
        // They can't be parsed via `type T = x is string;`.
        // We test them via the extract module which handles return types.
        // Here we just verify that malformed input returns None gracefully.
        assert!(canonicalize_type("x is string").is_none());
    }

    #[test]
    fn mapped_type() {
        // Mapped types should preserve structure
        let result = canon("{ [K in keyof T]: T[K] }");
        assert!(result.contains("[K in keyof T]"));
        assert!(result.contains("T[K]"));
    }

    // ── Rule 6: Import resolution tests ─────────────────────────────

    fn canon_with_imports(input: &str, imports: &ImportMap) -> String {
        canonicalize_type_with_imports(input, Some(imports)).unwrap()
    }

    fn react_imports() -> ImportMap {
        let mut m = ImportMap::new();
        m.add_default("React", "react");
        m
    }

    #[test]
    fn import_resolution_namespace_qualified() {
        // React.ReactNode → ReactNode (strip namespace qualifier)
        let imports = react_imports();
        assert_eq!(canon_with_imports("React.ReactNode", &imports), "ReactNode");
    }

    #[test]
    fn import_resolution_deep_qualified() {
        // React.JSX.Element → JSX.Element (strip only the import prefix)
        let imports = react_imports();
        assert_eq!(
            canon_with_imports("React.JSX.Element", &imports),
            "JSX.Element"
        );
    }

    #[test]
    fn import_resolution_import_type_expression() {
        // import("react").Context<T> → Context<T>
        assert_eq!(canon(r#"import("react").Context<T>"#), "Context<T>");
    }

    #[test]
    fn import_resolution_import_type_qualified() {
        // import("react/jsx-runtime").JSX.Element → JSX.Element
        assert_eq!(
            canon(r#"import("react/jsx-runtime").JSX.Element"#),
            "JSX.Element"
        );
    }

    #[test]
    fn import_resolution_namespace_generic() {
        // React.Context<Partial<FooProps>> → Context<Partial<FooProps>>
        let imports = react_imports();
        assert_eq!(
            canon_with_imports("React.Context<Partial<FooProps>>", &imports),
            "Context<Partial<FooProps>>"
        );
    }

    #[test]
    fn import_resolution_namespace_in_union() {
        // React.ReactNode | undefined → ReactNode | undefined
        let imports = react_imports();
        assert_eq!(
            canon_with_imports("React.ReactNode | undefined", &imports),
            "ReactNode | undefined"
        );
    }

    #[test]
    fn import_resolution_no_imports_passthrough() {
        // Without imports, qualified names pass through unchanged
        assert_eq!(canon("React.ReactNode"), "React.ReactNode");
    }

    #[test]
    fn import_resolution_non_import_qualified_preserved() {
        // If 'Foo' is not in the import map, Foo.Bar stays as-is
        let imports = react_imports();
        assert_eq!(canon_with_imports("Foo.Bar", &imports), "Foo.Bar");
    }

    #[test]
    fn import_resolution_import_type_no_qualifier() {
        // import("module") alone without qualifier — kept as source text
        let result = canon(r#"import("module")"#);
        assert!(result.contains("import"));
    }

    #[test]
    fn import_resolution_complex_return_type() {
        // Simulates: React.ForwardRefExoticComponent<Omit<FooProps, "ref"> & React.RefAttributes<any>>
        let imports = react_imports();
        let result = canon_with_imports(
            r#"React.ForwardRefExoticComponent<Omit<FooProps, "ref"> & React.RefAttributes<any>>"#,
            &imports,
        );
        // Rule 7: RefAttributes<any> is stripped to RefAttributes since
        // <any> is the default generic parameter.
        assert_eq!(
            result,
            r#"ForwardRefExoticComponent<Omit<FooProps, "ref"> & RefAttributes>"#
        );
    }

    #[test]
    fn import_resolution_both_forms_equal() {
        // The key test: React.Context<T> (namespace) and import("react").Context<T>
        // should produce the same canonical form
        let imports = react_imports();
        let from_namespace = canon_with_imports("React.Context<T>", &imports);
        let from_import_type = canon(r#"import("react").Context<T>"#);
        assert_eq!(from_namespace, from_import_type);
        assert_eq!(from_namespace, "Context<T>");
    }

    #[test]
    fn import_resolution_typeof_namespace_stripped() {
        // typeof React.useEffect → typeof useEffect
        let imports = react_imports();
        assert_eq!(
            canon_with_imports("typeof React.useEffect", &imports),
            "typeof useEffect"
        );
    }

    #[test]
    fn import_resolution_typeof_deep_namespace_stripped() {
        // typeof React.JSX.IntrinsicElements → typeof JSX.IntrinsicElements
        let imports = react_imports();
        assert_eq!(
            canon_with_imports("typeof React.JSX.IntrinsicElements", &imports),
            "typeof JSX.IntrinsicElements"
        );
    }

    #[test]
    fn import_resolution_typeof_no_import_passthrough() {
        // typeof Foo.bar without import map entry → unchanged
        let imports = react_imports();
        assert_eq!(
            canon_with_imports("typeof Foo.bar", &imports),
            "typeof Foo.bar"
        );
    }

    #[test]
    fn import_resolution_typeof_bare_identifier() {
        // typeof useEffect → typeof useEffect (no change)
        let imports = react_imports();
        assert_eq!(
            canon_with_imports("typeof useEffect", &imports),
            "typeof useEffect"
        );
    }

    // ── ImportMap merge tests ────────────────────────────────────────

    #[test]
    fn import_map_merge_namespaces_only() {
        let mut base = ImportMap::new();
        base.add_namespace("React", "react");

        let mut other = ImportMap::new();
        other.add_namespace("Lodash", "lodash");
        other.add_named("useState", "useState", "react");

        base.merge_namespaces_from(&other);

        assert!(base.is_namespace_or_default("Lodash"));
        assert!(base.named_import_module("useState").is_none());
    }

    #[test]
    fn import_map_merge_all_no_overwrite() {
        let mut base = ImportMap::new();
        base.add_namespace("React", "custom-react");

        let mut other = ImportMap::new();
        other.add_namespace("React", "react");

        base.merge_all_from(&other);

        // Original should win
        assert_eq!(base.module_for("React"), Some("custom-react"));
    }

    #[test]
    fn import_map_len_and_iter() {
        let mut m = ImportMap::new();
        assert_eq!(m.len(), 0);
        m.add_namespace("React", "react");
        m.add_named("FC", "FunctionComponent", "react");
        assert_eq!(m.len(), 2);
        assert_eq!(m.iter().count(), 2);
    }

    // ── Rule 7: Default generic parameter stripping ──────────────────

    #[test]
    fn strip_all_any_generic_params() {
        assert_eq!(canon("ReactElement<any>"), "ReactElement");
    }

    #[test]
    fn strip_multiple_any_generic_params() {
        assert_eq!(canon("Map<any, any>"), "Map");
    }

    #[test]
    fn preserve_non_any_generic_params() {
        assert_eq!(canon("ReactElement<string>"), "ReactElement<string>");
    }

    #[test]
    fn preserve_mixed_generic_params() {
        assert_eq!(canon("Map<string, any>"), "Map<string, any>");
    }

    #[test]
    fn no_type_args_unchanged() {
        assert_eq!(canon("ReactElement"), "ReactElement");
    }

    #[test]
    fn strip_any_in_union_member() {
        assert_eq!(canon("ReactElement<any> | string"), "ReactElement | string");
    }

    #[test]
    fn strip_any_in_array() {
        assert_eq!(canon("ReactElement<any>[]"), "ReactElement[]");
    }

    #[test]
    fn strip_any_in_function_return() {
        assert_eq!(
            canon("(props: Foo) => ReactElement<any>"),
            "(props: Foo) => ReactElement"
        );
    }
}
