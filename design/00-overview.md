# Multi-Language Architecture Design

## Status

**Proposed** -- This document captures the target architecture for making
the semver-analyzer language-agnostic, supporting TypeScript, Go, Java,
Python, and C#.

## Problem

The current codebase has three logical layers:

- **`core` crate** -- intended to be language-agnostic types and diff engine
- **`ts` crate** -- TypeScript/React-specific extraction and analysis
- **Orchestrator** (`src/orchestrator.rs`) -- coordinates the analysis pipeline

In practice, TypeScript-specific concepts have bled into all layers:

- `core` defines `BehavioralCategory` with web-specific variants (`DomStructure`,
  `CssClass`, `CssVariable`)
- `core` defines `ManifestChangeType` with npm-specific variants (`PeerDependencyAdded`,
  `ExportsEntryRemoved`)
- `core` defines `EvidenceSource::JsxDiff` -- a React-specific variant
- The diff engine hardcodes TypeScript rules (`Promise<>` detection, `"void"` defaults,
  `Props` suffix stripping, union literal parsing, default export deduplication)
- The orchestrator is ~90% TypeScript/React-specific code

## Design Principles

1. **Core is truly language-agnostic.** It defines structures and traits. It never
   references TypeScript, React, npm, JSX, or any language-specific concept.

2. **Language-specific knowledge lives in language crates.** Each language crate
   (ts, go, java, python, csharp) implements the `Language` trait and provides
   all language-specific rules, types, and message formatting.

3. **The `Language` trait is the single integration point.** It bundles semantic
   rules, message formatting, and associated types. The diff engine and report
   types are parameterized by `Language`.

4. **The report is a two-tier artifact.** Structural changes are language-agnostic
   and always readable. Language-specific data (behavioral categories, manifest
   changes, framework-specific report data) lives in a typed section that
   requires language knowledge to deserialize.

5. **Consumers choose their depth.** A CI summary tool reads the language-agnostic
   tier. A Konveyor rule generator deserializes the language-specific tier.

## Documents

| Document | Description |
|----------|-------------|
| [01-traits.md](./01-traits.md) | Core traits: `Language`, `LanguageSemantics`, `MessageFormatter`, and the existing extraction/analysis traits |
| [02-types.md](./02-types.md) | Core types: `Symbol`, `SymbolKind`, `Signature`, `StructuralChangeType`, `ChangeSubject`, and report types |
| [03-report-envelope.md](./03-report-envelope.md) | The `ReportEnvelope` architecture: two-tier report with language-agnostic and language-specific sections |
| [04-language-implementation-guide.md](./04-language-implementation-guide.md) | How to implement `Language` for a new language, with TypeScript and Go as examples |
| [05-composition-tree-v2.md](./05-composition-tree-v2.md) | V2 composition tree builder: evidence-based signals, EdgeStrength, verification scorecard |

## Crate Structure (Target)

```
semver-analyzer-v2/
  crates/
    core/           # Language-agnostic types, traits, diff engine
    llm/            # LLM invocation (language-agnostic BehaviorAnalyzer)
    konveyor-core/  # Konveyor YAML types, shared rule utilities
    ts/             # TypeScript/React: Language impl, extraction, rule generation
    go/             # Go: Language impl, extraction, rule generation
    java/           # Java: Language impl, extraction, rule generation
    python/         # Python: Language impl, extraction, rule generation
    csharp/         # C#: Language impl, extraction, rule generation
  src/
    main.rs         # CLI entry point, language dispatch
    orchestrator.rs # Generic orchestrator parameterized by Language
```
