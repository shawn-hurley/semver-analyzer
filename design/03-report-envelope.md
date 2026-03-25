# Report Envelope Architecture

## Problem

The analysis report contains both language-agnostic data (structural changes
like "function removed," "parameter type changed") and language-specific data
(TypeScript behavioral categories, React component hierarchies, npm manifest
changes). Different consumers need different levels of access:

- A **CI summary tool** just needs counts: "47 breaking changes across 3 packages"
- An **LLM agent** needs change descriptions (already formatted in language-appropriate terms)
- A **Konveyor rule generator** needs the full language-specific data to build
  detection conditions and migration messages in framework-specific terms

We need a report format that lets simple consumers read it without any language
knowledge, while deep consumers can access the full typed data.

## Design

The `ReportEnvelope` is a two-tier JSON artifact:

1. **Language-agnostic tier** -- always readable, contains structural changes and
   summary statistics
2. **Language-specific tier** -- requires knowing the `Language` type to
   deserialize, contains behavioral categories, manifest changes, and
   framework-specific report data

```rust
/// Self-describing container for an analysis report.
///
/// The language-agnostic fields are always accessible. The `language_report`
/// field is a serialized `LanguageReport<L>` that requires the concrete
/// `Language` implementation to deserialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportEnvelope {
    /// Which language produced this report.
    /// Matches `L::name()` from the Language trait.
    /// e.g., "typescript", "go", "java", "python", "csharp".
    pub language: String,

    /// Tool version that produced this report.
    pub version: String,

    // ── Language-agnostic tier ──────────────────────────────────

    /// Aggregate statistics. Readable without language knowledge.
    pub summary: AnalysisSummary,

    /// All structural changes detected by the diff engine.
    /// These use the collapsed StructuralChangeType with ChangeSubject.
    /// Descriptions are already formatted by the MessageFormatter.
    pub structural_changes: Vec<StructuralChange>,

    // ── Language-specific tier ──────────────────────────────────

    /// Language-specific report data, serialized as JSON.
    /// Consumers call `envelope.language_report::<TypeScript>()?` to
    /// deserialize into the concrete `LanguageReport<TypeScript>`.
    pub language_report: serde_json::Value,
}
```

### `AnalysisSummary`

Aggregate statistics that simple consumers can read without deserializing
the language-specific section. Includes counts derived from both tiers.

```rust
pub struct AnalysisSummary {
    /// Total structural breaking changes.
    pub total_structural_breaking: usize,

    /// Total structural non-breaking changes.
    pub total_structural_non_breaking: usize,

    /// Total behavioral changes (from the language-specific BU pipeline).
    pub total_behavioral_changes: usize,

    /// Total manifest changes.
    pub total_manifest_changes: usize,

    /// Number of packages analyzed.
    pub packages_analyzed: usize,

    /// Number of files changed.
    pub files_changed: usize,

    /// Breakdown of structural changes by lifecycle type.
    pub by_change_type: ChangeTypeCounts,
}

pub struct ChangeTypeCounts {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub renamed: usize,
    pub relocated: usize,
}
```

### Typed access method

The envelope provides a safe accessor that validates the language tag before
deserializing:

```rust
impl ReportEnvelope {
    /// Deserialize the language-specific report section.
    ///
    /// Returns an error if:
    /// - `L::name()` doesn't match `self.language`
    /// - The JSON fails to deserialize into `LanguageReport<L>`
    pub fn language_report<L: Language>(&self) -> Result<LanguageReport<L>> {
        if L::name() != self.language {
            anyhow::bail!(
                "Report was produced by '{}' but requested as '{}'",
                self.language,
                L::name()
            );
        }
        Ok(serde_json::from_value(self.language_report.clone())?)
    }

    /// Construct an envelope from a typed analysis report.
    pub fn from_report<L: Language>(report: &AnalysisReport<L>) -> Result<Self> {
        Ok(Self {
            language: L::name().to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            summary: report.summary(),
            structural_changes: report.all_structural_changes(),
            language_report: serde_json::to_value(&report.language_report())?,
        })
    }
}
```

---

## Consumer Patterns

### Simple consumer -- no language knowledge

A CI tool, dashboard, or notification system that just needs aggregate data:

```rust
let envelope: ReportEnvelope = serde_json::from_reader(file)?;

println!("Language: {}", envelope.language);
println!("Version: {}", envelope.version);
println!("Breaking changes: {}", envelope.summary.total_structural_breaking);
println!("Behavioral changes: {}", envelope.summary.total_behavioral_changes);
println!("Manifest changes: {}", envelope.summary.total_manifest_changes);
println!("");
println!("Breakdown:");
println!("  Added: {}", envelope.summary.by_change_type.added);
println!("  Removed: {}", envelope.summary.by_change_type.removed);
println!("  Changed: {}", envelope.summary.by_change_type.changed);
println!("  Renamed: {}", envelope.summary.by_change_type.renamed);
println!("  Relocated: {}", envelope.summary.by_change_type.relocated);

// Can also iterate structural changes for descriptions
for change in &envelope.structural_changes {
    if change.is_breaking {
        println!("BREAKING: {}", change.description);
    }
}

// Never touches envelope.language_report
```

The `description` field on each `StructuralChange` was already populated by the
`MessageFormatter` at analysis time. So even without language knowledge, the
consumer gets human-readable, language-appropriate descriptions like:

- TypeScript: "Required prop `onClick` was added to interface `ButtonProps`"
- Go: "Method `Close` was added to interface `Reader` -- all implementors must add it"

### LLM agent -- reads descriptions, ignores typed data

An LLM agent generating migration instructions from the report:

```rust
let envelope: ReportEnvelope = serde_json::from_reader(file)?;

let mut prompt = String::from("The following breaking changes were detected:\n\n");
for change in &envelope.structural_changes {
    if change.is_breaking {
        prompt.push_str(&format!("- {}\n", change.description));
    }
}
prompt.push_str("\nGenerate migration instructions for each change.");

let response = llm.invoke(&prompt)?;
```

### Deep consumer -- needs language-specific types

A Konveyor rule generator that needs TypeScript-specific data:

```rust
let envelope: ReportEnvelope = serde_json::from_reader(file)?;

match envelope.language.as_str() {
    "typescript" => {
        let lang_report = envelope.language_report::<TypeScript>()?;

        // Access TS-specific behavioral categories
        for change in &lang_report.behavioral_changes {
            match change.category {
                Some(TsCategory::DomStructure) => { /* build JSX detection rule */ }
                Some(TsCategory::CssClass) => { /* build CSS class rule */ }
                _ => {}
            }
        }

        // Access TS-specific report data
        for component in &lang_report.data.components {
            // ComponentSummary with interface_name, child_components,
            // expected_children, etc.
        }

        // Also has access to structural changes from the envelope
        generate_ts_rules(&envelope.structural_changes, &lang_report)?;
    }
    "go" => {
        let lang_report = envelope.language_report::<Go>()?;
        generate_go_rules(&envelope.structural_changes, &lang_report)?;
    }
    other => anyhow::bail!("Unsupported language: {}", other),
}
```

---

## Serialized JSON Structure

An example of what the report envelope looks like as JSON:

```json
{
  "language": "typescript",
  "version": "0.5.0",
  "summary": {
    "total_structural_breaking": 47,
    "total_structural_non_breaking": 12,
    "total_behavioral_changes": 23,
    "total_manifest_changes": 3,
    "packages_analyzed": 5,
    "files_changed": 89,
    "by_change_type": {
      "added": 8,
      "removed": 31,
      "changed": 14,
      "renamed": 4,
      "relocated": 2
    }
  },
  "structural_changes": [
    {
      "symbol": "Button",
      "qualified_name": "@patternfly/react-core::Button",
      "kind": "interface",
      "change_type": {
        "Removed": {
          "Member": { "name": "variant", "kind": "property" }
        }
      },
      "before": "'primary' | 'secondary' | 'danger'",
      "after": null,
      "is_breaking": true,
      "description": "Prop `variant` was removed from `ButtonProps`",
      "migration_target": null
    }
  ],
  "language_report": {
    "behavioral_changes": [
      {
        "symbol": "Modal",
        "kind": "class",
        "category": "dom_structure",
        "description": "Modal wrapper element changed from <div> to <dialog>",
        "confidence": 0.9,
        "evidence": {
          "type": "JsxDiff",
          "element_before": "div",
          "element_after": "dialog"
        },
        "is_internal_only": false
      }
    ],
    "manifest_changes": [
      {
        "field": "peerDependencies.react",
        "change_type": "peer_dependency_range_changed",
        "before": "^17.0.0 || ^18.0.0",
        "after": "^18.0.0",
        "description": "Peer dependency `react` range narrowed",
        "is_breaking": true
      }
    ],
    "data": {
      "components": [ "..." ],
      "constants": [ "..." ],
      "hierarchy_deltas": [ "..." ]
    }
  }
}
```

Note how `language_report.behavioral_changes[0].category` is `"dom_structure"`
(a `TsCategory` variant) and `language_report.behavioral_changes[0].evidence`
has TypeScript-specific fields (`element_before`, `element_after`). A Go report
would have different category values and different evidence structure, but the
outer envelope shape is identical.

---

## Decoupled Architecture

The envelope enables a decoupled pipeline:

```
                    ┌─────────────────────┐
                    │  Analyzer Binary     │
                    │  (language-specific) │
                    └──────────┬──────────┘
                               │
                        writes JSON file
                               │
                               ▼
                    ┌─────────────────────┐
                    │   ReportEnvelope     │
                    │      (JSON)         │
                    └──────────┬──────────┘
                               │
              ┌────────────────┼────────────────┐
              │                │                │
              ▼                ▼                ▼
    ┌──────────────┐  ┌──────────────┐  ┌──────────────┐
    │ CI Summary   │  │  LLM Agent   │  │ Konveyor     │
    │              │  │              │  │ Rule Gen     │
    │ Reads:       │  │ Reads:       │  │ Reads:       │
    │  summary     │  │  structural  │  │  structural  │
    │              │  │  changes     │  │  changes     │
    │ Ignores:     │  │  (descript.) │  │  + language  │
    │  language_   │  │              │  │  _report     │
    │  report      │  │ Ignores:     │  │  (typed)     │
    │              │  │  language_   │  │              │
    │              │  │  report      │  │              │
    └──────────────┘  └──────────────┘  └──────────────┘
```

The analyzer binary runs once and produces the envelope. Multiple consumers
can read the same file at their chosen level of depth. New consumers can be
added without modifying the analyzer. New languages require implementing the
`Language` trait and rebuilding only the consumers that need language-specific
access.
