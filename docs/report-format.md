# Report Format Reference

The analysis report is a JSON document produced by the `analyze` command. This reference documents every field and its serialization behavior.

## Overview

- The report is generic over language (`AnalysisReport<L: Language>`). Currently, only TypeScript is implemented.
- Many fields use `skip_serializing_if` -- they are **absent** from JSON when empty/null. Treat missing fields as empty collections or null.
- Enum serialization varies: some are plain strings, some are internally tagged objects, some are externally tagged. See [Enum Reference](#enum-reference).

## Top-Level Fields

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `repository` | string (path) | Always | Path to the analyzed repository |
| `comparison` | [Comparison](#comparison) | Always | Git comparison metadata |
| `summary` | [Summary](#summary) | Always | Aggregate change counts |
| `changes` | [FileChanges](#filechanges)[] | Always | Per-file breaking changes (may be `[]`) |
| `manifest_changes` | [ManifestChange](#manifestchange)[] | If non-empty | Package manifest changes |
| `added_files` | string[] | If non-empty | Files added between refs |
| `packages` | [PackageChanges](#packagechanges)[] | If non-empty | Pre-aggregated per-package view |
| `member_renames` | object | If non-empty | Map of old member name -> new name |
| `inferred_rename_patterns` | [InferredRenamePatterns](#inferredrenamepatterns) | If present | LLM-inferred rename patterns |
| `hierarchy_deltas` | [HierarchyDelta](#hierarchydelta)[] | If non-empty | Component hierarchy changes |
| `sd_result` | [SdPipelineResult](#sdpipelineresult) | If SD ran | Source-level diff results (default pipeline) |
| `metadata` | [AnalysisMetadata](#analysismetadata) | Always | Tool version, LLM usage stats |

## Core Structures

### Comparison

| Field | Type | Description |
|-------|------|-------------|
| `from_ref` | string | Old git ref (tag/branch) |
| `to_ref` | string | New git ref |
| `from_sha` | string | Full commit SHA |
| `to_sha` | string | Full commit SHA |
| `commit_count` | number | Commits between refs |
| `analysis_timestamp` | string | ISO 8601 timestamp |

### Summary

| Field | Type | Description |
|-------|------|-------------|
| `total_breaking_changes` | number | Total count |
| `breaking_api_changes` | number | Structural (TD) changes |
| `breaking_behavioral_changes` | number | Behavioral (BU) changes |
| `files_with_breaking_changes` | number | Files affected |

### FileChanges

One entry per file with breaking changes. Array is sorted alphabetically by file path.

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `file` | string | Always | File path |
| `status` | string | Always | `"added"`, `"modified"`, `"deleted"`, or `"renamed"` |
| `renamed_from` | string | If renamed | Original file path |
| `breaking_api_changes` | [ApiChange](#apichange)[] | Always | Structural changes |
| `breaking_behavioral_changes` | [BehavioralChange](#behavioralchange)[] | Always | Behavioral changes |
| `container_changes` | [ContainerChange](#containerchange)[] | If non-empty | Container/wrapper changes |

### ApiChange

Individual structural breaking change from the TD pipeline.

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `symbol` | string | Always | Symbol name (e.g., `"CardProps.isFlat"`) |
| `qualified_name` | string | If non-empty | Fully qualified name. Note: this is a `String`, not nullable -- absent means `""` |
| `kind` | string | Always | See [ApiChangeKind](#apichangekind) |
| `change` | string | Always | See [ApiChangeType](#apichangetype) |
| `before` | string | If present | Old signature/type |
| `after` | string | If present | New signature/type |
| `description` | string | Always | Human-readable description |
| `migration_target` | [MigrationTarget](#migrationtarget) | If present | Replacement mapping |
| `removal_disposition` | [RemovalDisposition](#removaldisposition) | If present | What happened to the removed symbol |
| `renders_element` | string | If present | HTML element this component renders |

### BehavioralChange

Behavioral change from the BU pipeline.

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `symbol` | string | Always | Function/method name |
| `kind` | string | Always | `"function"`, `"method"`, `"class"`, or `"module"` |
| `category` | string | If present | TS categories: `"dom_structure"`, `"css_class"`, `"css_variable"`, `"accessibility"`, `"default_value"`, `"logic_change"`, `"data_attribute"`, `"render_output"` |
| `description` | string | Always | What changed |
| `confidence` | number | If present | 0.0 to 1.0 |
| `evidence_type` | string | If present | `"test_delta"`, `"llm_analysis"`, `"body_analysis"`, `"call_graph_propagation"` |
| `referenced_symbols` | string[] | If non-empty | Related symbols |
| `is_internal_only` | boolean | If present | Whether change is internal |

Note: The `source_file` field exists in the Rust struct but is **never serialized** to JSON.

### ContainerChange

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `symbol` | string | Always | Symbol name |
| `old_container` | string | If present | Previous container |
| `new_container` | string | If present | New container |
| `description` | string | Always | What changed |

### ManifestChange

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `field` | string | Always | package.json field (e.g., `"main"`, `"exports"`) |
| `change_type` | string | Always | See [TsManifestChangeType](#tsmanifestchangetype) |
| `before` | string | If present | Old value |
| `after` | string | If present | New value |
| `description` | string | Always | What changed |
| `is_breaking` | boolean | Always | Whether this is a breaking change |

## Package View

### PackageChanges

Pre-aggregated per-package view. Primary data source for rule generation.

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `name` | string | Always | Package name (e.g., `"@patternfly/react-core"`) |
| `old_version` | string | If present | Version at old ref |
| `new_version` | string | If present | Version at new ref |
| `type_summaries` | [ComponentSummary](#componentsummary)[] | If non-empty | Per-component change summaries |
| `constants` | [ConstantGroup](#constantgroup)[] | If non-empty | Grouped constant changes |
| `added_exports` | [AddedExport](#addedexport)[] | If non-empty | New exports in new version |

### ComponentSummary

Pre-aggregated summary of all changes to a single type/interface/component.

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `name` | string | Always | Component name (e.g., `"Modal"`) |
| `definition_name` | string | Always | Props interface name (e.g., `"ModalProps"`) |
| `status` | string | Always | `"modified"`, `"removed"`, or `"added"` |
| `member_summary` | [MemberSummary](#membersummary) | Always | Aggregate member counts |
| `removed_members` | [RemovedMember](#removedmember)[] | If non-empty | Members removed |
| `type_changes` | [TypeChange](#typechange)[] | If non-empty | Members with type changes |
| `migration_target` | [MigrationTarget](#migrationtarget) | If present | Replacement component mapping |
| `behavioral_changes` | [BehavioralChange](#behavioralchange)[] | If non-empty | Behavioral changes for this component |
| `child_components` | [ChildComponent](#childcomponent)[] | If non-empty | New/modified child components |
| `expected_children` | [ExpectedChild](#expectedchild)[] | If non-empty | Expected child components |
| `source_files` | string[] | If non-empty | Source file paths |

### MemberSummary

| Field | Type | Description |
|-------|------|-------------|
| `total` | number | Members in old version |
| `removed` | number | Members removed |
| `renamed` | number | Members renamed |
| `type_changed` | number | Members with type changes |
| `added` | number | Members added in new version |
| `removal_ratio` | number | 0.0 to 1.0 |

### RemovedMember

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `name` | string | Always | Member name |
| `old_type` | string | If present | Type before removal |
| `removal_disposition` | [RemovalDisposition](#removaldisposition) | If present | What happened to it |

### TypeChange

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `property` | string | Always | Property name |
| `before` | string | If present | Old type |
| `after` | string | If present | New type |

### ChildComponent

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `name` | string | Always | Child component name |
| `status` | string | Always | `"added"` or `"modified"` |
| `known_members` | string[] | If non-empty | Known props on the child |
| `absorbed_members` | string[] | If non-empty | Props moved from parent to this child |

### ExpectedChild

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | -- | Child component name |
| `required` | boolean | `false` | Whether the child is mandatory |
| `mechanism` | string | `"child"` | `"child"` (JSX child) or `"prop"` (passed as prop) |
| `prop_name` | string | null | Prop name when mechanism is `"prop"` |

### MigrationTarget

Describes the replacement mapping between a removed and surviving symbol.

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `removed_symbol` | string | Always | Old symbol name |
| `removed_qualified_name` | string | Always | Old qualified name |
| `removed_package` | string | If present | Old package |
| `replacement_symbol` | string | Always | New symbol name |
| `replacement_qualified_name` | string | Always | New qualified name |
| `replacement_package` | string | If present | New package |
| `matching_members` | [MemberMapping](#membermapping)[] | Always | Matching member pairs |
| `removed_only_members` | string[] | Always | Members with no match |
| `overlap_ratio` | number | Always | 0.0 to 1.0 |
| `old_extends` | string | If present | Old `extends` clause |
| `new_extends` | string | If present | New `extends` clause |

### MemberMapping

| Field | Type | Description |
|-------|------|-------------|
| `old_name` | string | Old member name |
| `new_name` | string | New member name |

### RemovalDisposition

Internally tagged enum (`"type"` discriminator field):

```json
{ "type": "moved_to_related_type", "target_type": "ModalHeader", "mechanism": "children" }
{ "type": "replaced_by_member", "new_member": "severity" }
{ "type": "made_automatic" }
{ "type": "truly_removed" }
```

| Variant | Fields | Description |
|---------|--------|-------------|
| `moved_to_related_type` | `target_type`, `mechanism` (`"prop"` or `"children"`) | Member moved to another component |
| `replaced_by_member` | `new_member` | Replaced by a differently-named member |
| `made_automatic` | -- | Behavior is now automatic (no prop needed) |
| `truly_removed` | -- | Genuinely removed with no replacement |

### ConstantGroup

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `change_type` | string | Always | `"removed"`, `"renamed"`, etc. |
| `count` | number | Always | Number of constants in group |
| `symbols` | string[] | If non-empty | Constant names |
| `common_prefix_pattern` | string | Always | Shared prefix (may be `""`) |
| `strategy_hint` | string | Always | Fix strategy hint (may be `""`) |
| `suffix_renames` | [SuffixRename](#suffixrename)[] | If non-empty | Suffix-level rename mappings |

### SuffixRename

| Field | Type | Description |
|-------|------|-------------|
| `from` | string | Old suffix |
| `to` | string | New suffix |

### AddedExport

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Export name |
| `qualified_name` | string | Qualified name |
| `package` | string | Package name |

### HierarchyDelta

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `component` | string | Always | Component name |
| `added_children` | [ExpectedChild](#expectedchild)[] | If non-empty | New child components |
| `removed_children` | string[] | If non-empty | Removed child components |
| `migrated_members` | [MigratedMember](#migratedmember)[] | If non-empty | Props migrated to children |
| `source_package` | string | If present | Package name |
| `migration_target` | [MigrationTarget](#migrationtarget) | If present | Replacement mapping |

### MigratedMember

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `member_name` | string | Always | Prop name |
| `target_child` | string | Always | Child component that received the prop |
| `target_member_name` | string | If present | New prop name on the child |

### AnalysisMetadata

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `call_graph_analysis` | string | Always | Whether call graph was used |
| `tool_version` | string | Always | semver-analyzer version |
| `llm_usage` | [LlmUsage](#llmusage) | If present | LLM cost/call statistics |

### LlmUsage

| Field | Type | Description |
|-------|------|-------------|
| `total_calls` | number | Total LLM invocations |
| `spec_inference_calls` | number | Spec inference calls |
| `comparison_calls` | number | Comparison calls |
| `propagation_calls` | number | Propagation calls |
| `total_input_tokens` | number | Input tokens consumed |
| `total_output_tokens` | number | Output tokens generated |
| `estimated_cost_usd` | number | Estimated cost |
| `circuit_breaker_triggered` | boolean | Whether cost limit was hit |

### InferredRenamePatterns

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `constant_patterns` | object[] | If non-empty | `{match_regex, replace, hit_count, total_removed}` |
| `interface_mappings` | object[] | If non-empty | `{old_name, new_name, confidence, reason, member_overlap_ratio}` |
| `metadata` | object | Always | `{llm_calls, constant_hit_rate, interface_mappings_found}` |

## SD Pipeline Result

Present as `sd_result` when the default SD pipeline runs (absent with `--behavioral`).

### SdPipelineResult

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `source_level_changes` | [SourceLevelChange](#sourcelevelchange)[] | Always | Per-component source changes |
| `composition_trees` | [CompositionTree](#compositiontree)[] | Always | New-version composition trees |
| `old_composition_trees` | [CompositionTree](#compositiontree)[] | If non-empty | Old-version composition trees |
| `composition_changes` | [CompositionChange](#compositionchange)[] | Always | Delta between old and new trees |
| `conformance_checks` | [ConformanceCheck](#conformancecheck)[] | Always | Structural validity rules |
| `component_packages` | object | If non-empty | Map: component name -> package name (new) |
| `old_component_packages` | object | If non-empty | Map: component name -> package name (old) |
| `old_component_props` | object | If non-empty | Map: component -> prop names (old) |
| `new_component_props` | object | If non-empty | Map: component -> prop names (new) |
| `old_component_prop_types` | object | If non-empty | Map: component -> {prop: type} (old) |
| `new_component_prop_types` | object | If non-empty | Map: component -> {prop: type} (new) |
| `new_required_props` | object | If non-empty | Map: component -> required prop names |
| `dep_repo_packages` | object | If non-empty | Map: package name -> version from dep repo |
| `removed_css_blocks` | string[] | If non-empty | CSS component blocks removed |
| `deprecated_replacements` | [DeprecatedReplacement](#deprecatedreplacement)[] | If non-empty | Deprecated component replacements |
| `old_profiles` | object | If non-empty | Map: key -> [ComponentSourceProfile](#componentsourceprofile) |
| `new_profiles` | object | If non-empty | Map: key -> [ComponentSourceProfile](#componentsourceprofile) |

### SourceLevelChange

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `component` | string | Always | Component name |
| `category` | string | Always | See [SourceLevelCategory](#sourcelevelcategory) |
| `description` | string | Always | What changed |
| `old_value` | string | If present | Previous value |
| `new_value` | string | If present | New value |
| `has_test_implications` | boolean | Always | Whether tests are likely affected |
| `test_description` | string | If present | How tests are affected |
| `element` | string | If present | DOM element involved |
| `migration_from` | string | If present | Original component (for deprecated migrations) |

### CompositionTree

| Field | Type | Description |
|-------|------|-------------|
| `root` | string | Family name (e.g., `"Table"`, `"deprecated/DualListSelector"`) |
| `family_members` | string[] | All components in the family |
| `edges` | [CompositionEdge](#compositionedge)[] | Parent-child relationships |

### CompositionEdge

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `parent` | string | -- | Parent component |
| `child` | string | -- | Child component |
| `relationship` | string | -- | `"bem_element"`, `"independent_block"`, `"internal"`, `"direct_child"`, `"unknown"` |
| `required` | boolean | -- | Whether this nesting is required |
| `bem_evidence` | string | null | BEM evidence description |
| `strength` | string | `"allowed"` | `"required"` or `"allowed"` |

### CompositionChange

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `family` | string | Always | Family name |
| `change_type` | object | Always | Externally tagged enum (see below) |
| `description` | string | Always | What changed |
| `before_pattern` | string | If present | Old composition pattern |
| `after_pattern` | string | If present | New composition pattern |

**`change_type` variants** (externally tagged: `{ "variant_name": { ...fields } }`):

| Variant | Fields | Description |
|---------|--------|-------------|
| `new_required_child` | `parent`, `new_child`, `wraps: []` | New intermediate wrapper |
| `prop_to_child` | `parent`, `child`, `props: []` | Props moved to child |
| `child_to_prop` | `parent`, `child`, `props: []` | Child absorbed into parent prop |
| `family_member_removed` | `member` | Component removed from family |
| `family_member_added` | `member` | Component added to family |
| `prop_driven_to_composition` | `parent` | API changed to composition-based |
| `composition_to_prop_driven` | `parent` | API changed to prop-driven |

### ConformanceCheck

| Field | Type | Present | Description |
|-------|------|---------|-------------|
| `family` | string | Always | Family name |
| `check_type` | object | Always | Externally tagged enum (see below) |
| `description` | string | Always | What the rule enforces |
| `correct_example` | string | If present | Correct usage example |

**`check_type` variants:**

| Variant | Fields | Description |
|---------|--------|-------------|
| `missing_intermediate` | `parent`, `child`, `required_intermediate` | Child needs a wrapper |
| `missing_child` | `parent`, `expected_child` | Parent must contain child |
| `invalid_direct_child` | `parent`, `child`, `expected_parent` | Child in wrong parent |
| `exclusive_wrapper` | `parent`, `allowed_children: []` | All children must be specific types |

### DeprecatedReplacement

| Field | Type | Description |
|-------|------|-------------|
| `old_component` | string | Deprecated component name |
| `new_component` | string | Replacement component name |
| `evidence_hosts` | string[] | Components that showed the rendering swap |

### ComponentSourceProfile

Large struct containing the full source-level profile of a component. Key fields:

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Component name |
| `file` | string | Source file path |
| `rendered_elements` | object | Map: HTML element -> count |
| `rendered_components` | string[] | React components rendered |
| `aria_attributes` | object | Map: `"element::attr"` -> value (double-colon key format) |
| `role_attributes` | object | Map: element -> role value |
| `data_attributes` | object | Map: `"element::attr"` -> value |
| `prop_defaults` | object | Map: prop name -> default value |
| `uses_portal` | boolean | Whether createPortal is used |
| `consumed_contexts` | string[] | useContext dependencies |
| `provided_contexts` | string[] | Context providers |
| `is_forward_ref` | boolean | Wrapped in forwardRef |
| `is_memo` | boolean | Wrapped in memo |
| `css_tokens_used` | string[] | CSS token references (e.g., `"styles.button"`) |
| `bem_block` | string | BEM block name |
| `extends_props` | string[] | Props interfaces this component extends |
| `children_slot_path` | string[] | DOM path to where `{children}` renders |
| `has_children_prop` | boolean | Whether component accepts children |
| `all_props` | string[] | All prop names |
| `required_props` | string[] | Required prop names |
| `prop_types` | object | Map: prop name -> type string |

Note: `aria_attributes` and `data_attributes` use a `"element::attribute"` key format (double-colon separated) because the underlying data is keyed by `(element_tag, attribute_name)` tuples.

## Enum Reference

### ApiChangeKind

Plain string: `"function"`, `"method"`, `"class"`, `"interface"`, `"type_alias"`, `"constant"`, `"property"`, `"field"`, `"module_export"`

### ApiChangeType

Plain string: `"removed"`, `"signature_changed"`, `"type_changed"`, `"visibility_changed"`, `"renamed"`

### SourceLevelCategory

Plain string: `"dom_structure"`, `"aria_change"`, `"role_change"`, `"data_attribute"`, `"css_token"`, `"prop_default"`, `"portal_usage"`, `"context_dependency"`, `"composition"`, `"forward_ref"`, `"memo"`, `"rendered_component"`, `"prop_attribute_override"`

### TsManifestChangeType

Plain string: `"entry_point_changed"`, `"exports_entry_removed"`, `"exports_entry_added"`, `"exports_condition_removed"`, `"module_system_changed"`, `"peer_dependency_added"`, `"peer_dependency_removed"`, `"peer_dependency_range_changed"`, `"engine_constraint_changed"`, `"bin_entry_removed"`

### ChildRelationship

Plain string: `"bem_element"`, `"independent_block"`, `"internal"`, `"direct_child"`, `"unknown"`

### EdgeStrength

Plain string: `"required"`, `"allowed"` (default: `"allowed"`)

### ComponentStatus

Plain string: `"modified"`, `"removed"`, `"added"`

### FileStatus

Plain string: `"added"`, `"modified"`, `"deleted"`, `"renamed"`

### EvidenceType

Plain string: `"test_delta"`, `"llm_analysis"`, `"body_analysis"`, `"call_graph_propagation"`

## Serialization Notes

1. **Missing fields = empty/null**: Fields with `skip_serializing_if` are omitted when empty. Treat absent fields as empty arrays, empty objects, or null.

2. **`qualified_name` on ApiChange**: This is a `String`, not nullable. It is **absent** when empty (not `null`). On input, treat missing as `""`.

3. **`source_file` on BehavioralChange**: Exists in the Rust struct but is **never** serialized. You will never see this field in JSON.

4. **Tuple-keyed maps**: `aria_attributes` and `data_attributes` in ComponentSourceProfile use `"element::attribute"` string keys (double-colon separator) because the underlying type is `BTreeMap<(String, String), String>`.

5. **Enum formats**:
   - Most enums: plain snake_case strings (`"removed"`, `"dom_structure"`)
   - `RemovalDisposition`: Internally tagged with `"type"` discriminator (`{ "type": "moved_to_related_type", ... }`)
   - `CompositionChangeType`, `ConformanceCheckType`: Externally tagged (`{ "variant_name": { ...fields } }`)

6. **Default values**: `EdgeStrength` defaults to `"allowed"`, `ExpectedChild.required` defaults to `false`, `ExpectedChild.mechanism` defaults to `"child"`. Missing fields deserialize to these defaults.

7. **`ConstantGroup` string fields**: `common_prefix_pattern` and `strategy_hint` always appear in JSON (even as `""`). They are not skipped when empty.
