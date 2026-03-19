# Structural Migration Detection

## Overview

The structural migration detection system identifies when a removed API symbol
(interface, class) has a plausible replacement in the same component directory.
It uses **same-directory member name overlap analysis** to detect three common
migration patterns automatically, without requiring library-specific knowledge.

This document describes the current implementation, its design decisions, and
potential future enhancements.

## Current Implementation

### Location

- `crates/core/src/diff/migration.rs` -- detection algorithm
- `crates/core/src/diff/mod.rs` -- Phase 5 integration into `diff_surfaces()`
- `crates/core/src/types/report.rs` -- `MigrationTarget`, `MemberMapping`,
  `MigrationSuggested` change type
- `src/konveyor/mod.rs` -- `StructuralMigration` fix strategy generation

### Algorithm

For each removed interface/class:

1. Compute its **canonical component directory** (stripping `/deprecated/` and
   `/next/` path segments).
2. Find all surviving or newly added interfaces in that directory.
3. For each candidate:
   - If it's a **same-name replacement** (e.g., deprecated `SelectProps` and
     main `SelectProps`), compare full member lists.
   - Otherwise, compare the removed interface's members against members that
     were **newly added** to the candidate (to avoid false matches on inherited
     base props like `children`, `className`).
4. If the overlap exceeds thresholds (currently >= 3 matching members AND
   >= 25% ratio), emit a `MigrationSuggested` change with the member mapping.

### Detected Patterns

| Pattern | Example | Signal |
|---|---|---|
| **Merge child into parent** | `EmptyStateHeaderProps` -> `EmptyStateProps` | Child removed, parent gained matching props |
| **Same-name replacement** | deprecated `SelectProps` -> main `SelectProps` | Same name at different path, overlapping members |
| **Decomposed into children** | (partial) `ModalProps` lost props, `ModalHeaderProps` gained them | Removed members match new sibling members |

### Output

Migration suggestions appear in three places:

1. **Analysis report** (`patternfly-report.json`): Each `ApiChange` with a
   migration target has a `migration_target` field with the full member mapping.
2. **Change description**: Enhanced from "was removed" to
   "was removed -- migrate to `X` (N matching members, M% overlap)".
3. **Fix strategies** (`fix-strategies.json`): `StructuralMigration` strategy
   with `member_mappings`, `removed_members`, `replacement`, and `overlap_ratio`.

## Future Enhancements

### 1. Fuzzy Member Name Matching

**Current limitation**: Members are matched by exact name only. `selections` in
the old interface and `selected` in the new interface are not recognized as
related.

**Enhancement**: Add Levenshtein distance or substring matching for member names
that don't have exact matches. A member pair with edit distance <= 2 and same
type signature could be flagged as a "likely rename" with lower confidence.

```
selections: string[]  -->  selected: string[]  (edit distance 2, same type)
onToggle: (isOpen) => void  -->  onOpenChange: (isOpen) => void  (different name, similar type)
```

This would enrich the `member_mappings` output with `{ old_name, new_name,
match_type: "exact" | "fuzzy", confidence: 0.85 }`.

### 2. Type Signature Overlap

**Current limitation**: Only member names are compared, not their types.

**Enhancement**: For candidates with moderate name overlap (20-30%), also
compare type signatures. If a removed member's type closely matches a candidate
member's type (ignoring minor differences like `string | SelectOptionObject` ->
`string | number`), boost the overlap score.

This would help detect migrations where prop names changed but types stayed
similar, reducing false negatives.

### 3. Cross-Directory Detection

**Current limitation**: Only symbols in the same canonical component directory
are considered.

**Enhancement**: Check symbols at the package barrel export level. If
`components/OldWidget/OldWidgetProps` is removed and
`components/NewWidget/NewWidgetProps` is added with 60% member overlap, flag it
even though they're in different directories.

**Risk**: Higher false positive rate. Would need stricter thresholds (e.g., 50%
overlap minimum) and possibly name similarity scoring.

### 4. Prop Removal -> New Child Component Correlation

**Current limitation**: The Modal pattern (props removed from parent, new child
components promoted) is only partially detected. The algorithm catches the case
where a *Props interface* is removed, but not where an existing interface *loses
members* that appear on new sibling components.

**Enhancement**: After detecting `PropertyRemoved` changes on a surviving
interface, check if new interfaces in the same directory have members matching
the removed properties.

```
ModalProps lost: title, actions, description, footer
ModalHeaderProps (new) has: title, description, help, titleIconVariant
ModalFooterProps (new) has: (actions would map here)
```

This would emit a `DecomposedIntoChildren` change type linking the removed props
to their new home components.

### 5. Codemod Recipe Generation

**Current limitation**: The `StructuralMigration` strategy includes member
mappings but not a transformation recipe. The fix engine treats it as
`LlmAssisted`.

**Enhancement**: For patterns with high confidence (>70% overlap), generate a
machine-readable transformation recipe:

```json
{
  "strategy": "StructuralMigration",
  "recipe": {
    "type": "MergeChildIntoParent",
    "remove_child_tags": ["EmptyStateHeader", "EmptyStateIcon"],
    "move_props_to_parent": [
      {"from_child": "EmptyStateHeader", "prop": "titleText", "to_parent_prop": "titleText"},
      {"from_child": "EmptyStateIcon", "prop": "icon", "to_parent_prop": "icon"}
    ],
    "remove_imports": ["EmptyStateHeader", "EmptyStateIcon"]
  }
}
```

The fix engine could then apply this recipe via AST transformation rather than
falling back to LLM.

### 6. Multi-Surface Correlation for Re-exports

**Current limitation**: Detection works within a single package's API surface.

**Enhancement**: When a symbol is removed from one package and a similar symbol
appears in another package (e.g., `@patternfly/react-core/deprecated` ->
`@patternfly/react-core`), correlate across package boundaries using the barrel
export index.

### 7. Confidence Scoring and Thresholds

**Current limitation**: Fixed thresholds (25% overlap, 3 minimum members).

**Enhancement**: Compute a multi-factor confidence score:

- Member name overlap ratio (current)
- Type signature similarity
- Name similarity (Levenshtein on the interface/class name itself)
- Path proximity (same directory > same package > different package)
- Component naming convention (e.g., `FooProps` and `FooBarProps` share prefix)

Emit the score with each suggestion so downstream tools can filter by confidence.

### 8. Reverse Migration Detection (Additions)

**Current limitation**: Only detects removed -> replacement patterns.

**Enhancement**: Also detect when a new interface is added that appears to
*replace* an existing one. For example, if `FooV2Props` is added with 80%
overlap with `FooProps`, flag `FooProps` as potentially deprecated even if it
wasn't explicitly removed yet.

### 9. Behavioral Change Correlation

**Current limitation**: Migration suggestions are based solely on structural
API changes (the TD pipeline). Behavioral changes (the BU pipeline) are not
correlated.

**Enhancement**: When a behavioral change is detected on a component that also
has a structural migration suggestion, enrich the migration target with
behavioral context. For example, if `EmptyState`'s render output changed and
`EmptyStateHeaderProps` was merged into it, note that consumers may also need
to update tests.

### 10. Integration with Official Codemods

**Current limitation**: Migration suggestions are independent of any official
codemod tooling the library may provide.

**Enhancement**: When the library (e.g., PatternFly) publishes official codemods
(like `@patternfly/react-codemod`), cross-reference the detected migration
patterns with the available codemods. If a codemod exists for a detected
pattern, include the codemod command in the fix strategy output.

## Design Decisions

### Why same-directory only?

Cross-directory matching would increase false positives significantly. Most
component libraries organize related components in the same directory
(`components/Select/`, `components/Modal/`). The canonical path stripping
(`/deprecated/`, `/next/`) handles the most common organizational patterns.

### Why member names, not types?

Member names are the strongest signal for API continuity. Types change
frequently between versions (e.g., `string | SelectOptionObject` ->
`string | number`), but prop names tend to be more stable. Starting with
exact name matching keeps precision high while still catching the most
important patterns.

### Why not use the rename detector?

The rename detector (`diff/rename.rs`) operates on top-level symbols and uses
type fingerprinting. It works well for `Chip` -> `Label` (different names,
similar types). But for structural migrations, the symbols often share the same
name (`SelectProps` -> `SelectProps`) or have very different overall fingerprints
(86 props vs 28 props). The member-overlap approach is complementary -- it looks
*inside* the interfaces rather than at their external shape.

### Why annotate existing changes rather than emit new ones?

The migration detection annotates existing `SymbolRemoved` changes (converting
them to `MigrationSuggested`) rather than emitting separate changes. This
ensures downstream tools see exactly one change per symbol, with the migration
target as optional enrichment. It also avoids inflating the breaking change
count.
