# Semver Analyzer — Agent Guide

## Project Overview

A multi-language semver analysis tool built in Rust. Compares two versions of a
library (via git refs), detects breaking changes, and generates Konveyor
migration rules with fix strategies.

### Architecture

- `crates/core/` — Language-agnostic diff engine, types, traits
- `crates/ts/` — TypeScript/React-specific: source profiles, JSX analysis, CSS
  profiles, konveyor rule generation
- `crates/konveyor-core/` — Konveyor rule types and fix strategy framework
- `crates/llm/` — LLM integration for behavioral analysis
- `src/orchestrator.rs` — Pipeline orchestrator (TD+BU or TD+SD)
- `src/main.rs` — CLI entry point

### Three Pipelines

The analyzer has three pipelines. The `--behavioral` flag controls which
combination runs. By default, the SD pipeline runs; `--behavioral` switches
to the BU pipeline instead.

#### TD (Top-Down) — Structural API Diff

**Always runs.** Extracts `.d.ts` API surfaces at both git refs, then diffs
them to detect:

- Renamed, removed, added symbols (constants, interfaces, type-aliases)
- Type changes on properties
- Signature changes (base class, return type)
- Relocations (moved to deprecated/, next/ promoted)
- Member-level renames within interfaces

Key files: `crates/core/src/diff/` (mod.rs, rename.rs, compare.rs, relocate.rs,
migration.rs)

#### SD (Source-Level Diff) — Deterministic Source Analysis (default)

**Runs by default.** Replaces BU with deterministic AST-based analysis:

1. Extract `ComponentSourceProfile` for each component at both refs
2. Diff profiles to produce `SourceLevelChange` entries:
   - DOM structure, ARIA, role, data attribute changes
   - CSS token usage, prop-style bindings
   - Portal usage, context dependencies
   - Forward ref, memo, composition
   - Prop defaults, children slot path
   - Managed attribute overrides (prop-overrides-attribute)
3. Build composition trees and conformance checks
4. Extract CSS profiles for class/variable removal detection

Key files: `crates/ts/src/source_profile/`, `crates/ts/src/sd_pipeline.rs`,
`crates/ts/src/composition/`

#### BU (Bottom-Up) — Behavioral Analysis (opt-in)

**Runs when `--behavioral` is set.** Walks the git diff bottom-up:

1. Parse changed functions from git diff
2. For each changed function, find associated test files
3. Check test assertion changes → behavioral break
4. If private function has behavioral break → walk UP call graph to public API
5. Optionally runs LLM analysis on changed files for deeper behavioral insights

Key files: `src/orchestrator.rs` (run() method, lines 56–305)

### Pipeline Selection

```sh
# Default: TD + SD (structural + source-level)
semver-analyzer analyze typescript --repo ... --from v5 --to v6

# Opt-in: TD + BU (structural + behavioral)
semver-analyzer analyze typescript --repo ... --from v5 --to v6 --behavioral
```

Both produce an `AnalysisReport` with the same top-level structure. The default
pipeline populates `sd_result` (source_level_changes, composition_trees,
conformance_checks, etc.). The `--behavioral` pipeline populates
`breaking_behavioral_changes` instead.

Rule generation (`konveyor` subcommand) generates SD-specific rules
(composition, conformance, prop-to-child migration, test impact, CSS removal,
prop-attribute-override) by default. With `--behavioral`, only TD-based rules
are generated.

## Key Rules for Agents

### Rename Detection (CRITICAL)

**Before modifying `crates/core/src/diff/rename.rs`**, read:

- `design/rename-detector-verification.md` — Contains the verification dataset
  (15 known-true renames, 28 known-false renames with similarity scores and root
  causes), the verification procedure, and threshold boundaries.
- Run the verification procedure after any change to confirm no regressions.

#### Generic Type Parameter Normalization

The `normalize_type_structure()` function strips generic type parameters from
normalized `_T_` placeholders. This ensures that types like `ReactElement` and
`ReactElement<any>` produce identical normalized fingerprints (`_T_`), enabling
rename detection when the only type difference is a default generic parameter.

Without this, renamed props with trivially-different generic parameters (e.g.,
`labelIcon: ReactElement` → `labelHelp: ReactElement<any>`) fail all four
rename passes and are emitted as separate Removed + Added entries instead of
a single Renamed.

### Source Profile Extraction

Source profiles are extracted in `crates/ts/src/source_profile/`. Submodules:

- `mod.rs` — Main extraction, JSX walking (also detects cloneElement inline
  and JSX elements in parameter/variable destructuring defaults)
- `prop_defaults.rs` — Default value extraction from destructuring
- `prop_style.rs` — Prop-to-CSS-class binding detection
- `managed_attrs.rs` — Prop-overrides-attribute dataflow tracing
- `diff.rs` — Profile diffing to produce SourceLevelChange entries
- `bem.rs` — BEM CSS structure parsing
- `children_slot.rs` — Children wrapper path tracing + CSS token detail
- `clone_element.rs` — cloneElement prop injection detection
- `react_api.rs` — React API usage detection (portal, memo, forwardRef)

### Composition Tree V2 Architecture (CRITICAL)

The v2 composition tree builder (`build_composition_tree_v2` in
`composition/mod.rs`) replaces BEM-based edge creation with evidence-based
signals. **BEM determines family membership only. All parent-child edges come
from structural evidence.**

#### Signal Steps (in order)

| Step | Signal | Strength | Rationale |
|------|--------|----------|-----------|
| 1 | Internal rendering | Required | Component renders the child (JSX body + prop-default JSX) |
| 1.5 | Delegate tree projection | Allowed | Wrapper family inherits edges from delegate family tree |
| 2 | CSS direct-child `>` | Required* | Styles require exact parent-child DOM |
| 3 | CSS grid parent-child | Required* | Layout breaks without grid container |
| 3b | CSS implicit grid child | Required* | Same — grid layout dependency |
| 4 | CSS flex context | Allowed | Layout preference, not strict |
| 5 | CSS descendant ` ` | Allowed | Works at any depth |
| 5.5 | CSS layout children | Allowed | Shared CSS rule with flex-wrap/gap implies containment |
| 6 | React context | Required | Null context = crash/broken behavior |
| 7 | DOM nesting | Required | Invalid HTML without correct parent |
| 8 | cloneElement | Required | Missing injected props breaks functionality |
| 8.5 | BEM element orphan fallback | Allowed | Orphan BEM elements connected to root as last resort |
| 8.6 | Secondary BEM block sub-root | Allowed | Cross-block orphans connected to sub-root (e.g., ModalBox→ModalBody) |
| 8.7 | Prop-passed detection | Allowed | ReactNode/ReactElement prop name matches child component name |

*Steps 2, 3, 3b use `Allowed` instead of `Required` when the child component
equals the family root — this indicates recursive/self-nesting (e.g., DataList
inside DataListContent, Menu inside MenuItem) which is optional, not required.

Step 1 detects JSX elements in **parameter destructuring defaults**
(`({ bar = <Bar /> }) => ...`) and **variable destructuring defaults**
(`const { icon = <Icon /> } = this.props`), not just the function body.
This is critical for components like ChartBullet that receive sub-components
as props with JSX defaults.

Step 1 handles both `ClassElement::MethodDefinition` (standard `render()`)
AND `ClassElement::PropertyDefinition` (arrow property `render = () => {}`)
in class components. Both forms are walked for JSX elements, children slot
tracing, cloneElement detection, and managed attribute flow. The
`PropertyDefinition` support is applied in `mod.rs`, `children_slot.rs`,
`clone_element.rs`, and `managed_attrs.rs`.

Step 1 also handles **TypeScript expression wrappers** transparently.
`TSAsExpression` (`expr as Type`), `TSSatisfiesExpression`,
`TSNonNullExpression` (`expr!`), `TSTypeAssertion` (`<Type>expr`), and
`TSInstantiationExpression` (`expr<Type>`) are all unwrapped before JSX
walking. This is critical for class components like Modal whose render
returns `ReactDOM.createPortal(<ModalContent/>, el) as React.ReactElement`
— without this, the `as` cast hides the JSX from the walker.

Step 1.5 projects edges from a delegate family's composition tree onto
wrapper families. When a family like Dropdown wraps Menu (each Dropdown
component extends the corresponding Menu component's props via
`extends_props`), the Menu tree's edges are projected onto Dropdown.
This runs inside the builder before Step 10, so projected edges prevent
wrapper family members from being dropped. The projection uses
`DelegateContext` which provides the delegate tree and a
`wrapper_to_delegate` mapping (e.g., `DropdownList → MenuList`).

Composition tree building is **dependency-aware**: families are classified
as independent (no external `extends_props`) or deferred (depends on
another family's tree). Independent families are built in Phase 1.
Deferred families are resolved in Phase 2 by iterating until all
dependencies are available. Chains (A → B → C) resolve naturally
across iterations. Circular or unresolvable dependencies fall back
to building without delegate context (with a warning).

Step 5.5 consumes `layout_children` from `CssBlockProfile` — pairs of BEM
elements where one is a layout container (has flex-wrap/gap/grid) and the
other is a co-rule sibling. This data was previously computed but never
consumed. It catches intermediate nesting within families (e.g.,
EmptyStateFooter → EmptyStateActions from a shared CSS rule with flex-wrap).

Step 8 has two filters to prevent false edges from shared prop vocabularies:
(1) skip creating A→B if B→A already exists from a prior step (prevents
reverse-of-existing cycles), and (2) remove bidirectional cloneElement pairs
(A→B + B→A both from cloneElement = peers, not hierarchy).

Step 8.5 connects orphan BEM elements to the family root as a last resort.
It fires for family members with zero incoming edges after all structural
signals (Steps 1-8 + 5.5) if the member appears in `css_element_to_component_map`
(has BEM element CSS tokens of the root's block). Three guards prevent false
edges:
1. **Orphan gate**: Only fires for members with zero incoming edges, preventing
   wrong edges for already-connected components in Category 3 families.
2. **CSS element map membership**: Member must have CSS tokens that are BEM
   elements of the root's block (filters out context objects, type exports).
3. **BEM independence**: Member must NOT have its own distinct BEM block
   (prevents false edges for collision families like Label/LabelGroup,
   Menu/MenuToggle, Alert/AlertGroup where camelCase naming creates false
   prefix matches in the CSS element map).

Step 8.6 handles families where components use a **different BEM block**
than the root (cross-block sub-families). For example, Modal's root uses
block `"backdrop"` while ModalBody/ModalFooter/ModalHeader use block
`"modalBox"`. Similarly, TabContentBody uses block `"tabContent"` while the
Tabs root uses `"tabs"`. For each BEM block used by family members that
differs from the **root's** block (not the primary CSS profile key — those
may differ when the dominant block wins by vote), Step 8.6:
1. Builds a secondary `css_to_component` map for that block.
2. Finds the **sub-root**: the component mapping to element `""` (root) of
   that block with `has_children_prop` (e.g., ModalBox, TabContent).
3. Connects orphan members whose `bem_block` matches the secondary block
   to the sub-root via `Allowed` edges.
After `collapse_internal_nodes`, if the sub-root is internal (non-exported),
edges propagate to the family root (e.g., `ModalBox → ModalBody` becomes
`Modal → ModalBody`).

Step 8.7 detects **prop-passed** components — those passed via named
`ReactNode`/`ReactElement` props rather than as JSX `{children}`. For each
family member, it checks all other family members' `prop_types` for
ReactNode/ReactElement props (excluding `children`). When a member's name
(with the parent's name prefix stripped) matches a prop name
(case-insensitive, `starts_with` in both directions), it creates a
`PropPassed` edge with `Allowed` strength. The matched prop name is stored
in the edge's `prop_name` field. Step 8.7 also **reclassifies** existing
`DirectChild` edges to `PropPassed` when a prop name match is found (e.g.,
`CodeBlock → CodeBlockAction` reclassified via `actions` prop).

After all steps, members with zero edges (no incoming AND no outgoing) are
dropped from the tree **unless** they are barrel-file exports. Exported
orphans are retained as family members — they're part of the family's
public API even if no structural signal links them (e.g., convenience
composites like LoginForm, orchestrators like MenuContainer). Non-exported
members with zero edges are dropped (internal noise: context objects, type
exports, helper components). Members with outgoing edges but no incoming
edges are retained as **secondary roots** — top-level containers within
the family. Non-exported secondary roots are then properly collapsed by
`collapse_internal_nodes`.

#### EdgeStrength: Required vs Allowed

Every edge has a `strength: EdgeStrength` field:

- **Required** — Rendering breaks without this nesting. Generates conformance
  rules.
- **Allowed** — Valid placement documented in CSS but not the only option. Stays
  in the tree for migration guidance but produces zero conformance rules.
  Included in the `notParent` regex to prevent false positives on valid
  placements.

Collapsed edges (from `collapse_internal_nodes`) inherit the **stronger** of
the two edges in the chain.

#### Conformance Rule Generation (CRITICAL)

Conformance rule generation in `konveyor_v2.rs::generate_conformance_rules()`
uses a simple algorithm based on edge direction and incoming edges:

```
has_required_incoming: members with ≥1 incoming Required non-internal edge
parent_to_req_children: Required non-internal edges grouped by parent
child_to_all_parents: ALL non-internal edges grouped by child (for regex)

for (parent, children) in parent_to_req_children:
    if parent NOT in has_required_incoming:
        → requiresChild rule on parent (parent must contain children)
    else:
        → notParent rule on each child (child must be inside parent)
```

**Three rule types are generated:**

| Rule | Scanner Field | When | Example |
|------|--------------|------|---------|
| `requiresChild` | `requires_child` | Parent has no Required incoming edges (root/secondary root) | `AlertGroup` must contain `Alert` |
| `notParent` | `not_parent` | Child has a Required parent with incoming edges | `Td` must be inside `Tr` |
| `invalidDirectChild` | `parent` | Child skips required intermediate parent | `Td` directly in `Table` (needs `Tr`) |

**Key design decisions:**

- Only **Required** incoming edges determine `has_required_incoming`. Allowed
  back-edges (e.g., Tab→Tabs for recursive nesting) don't make the child
  mandatory, so they don't prevent the parent from getting `requiresChild`.
- Internal edges (`ChildRelationship::Internal`) are excluded from all maps.
  They represent parent-renders-child relationships that the consumer doesn't
  control.
- The `notParent` regex includes ALL non-internal parents (Required + Allowed)
  so that valid-but-not-required placements don't trigger false positives.
- Children that only have `no_incoming` parents get no `notParent` rule (they
  can exist standalone). The parent gets a `requiresChild` rule instead.
- Cycles (A→B Required + B→A Required) are tree accuracy bugs. Both edges
  should not be Required — the recursive direction should be `Allowed`.
  The rule generator does not handle cycles; fix the tree instead.

#### Conformance Rule ID Format

Conformance rule IDs use abbreviated segments and stripped component names
to keep IDs short. Each rule ID includes the family name to prevent
duplicates when regular and deprecated families share component names
(e.g., `DualListSelector` and `deprecated/DualListSelector`).

**Abbreviation scheme:**

| Full segment | Abbreviation |
|---|---|
| `conformance` | `cf` |
| `must-be-in` | `in` |
| `requires` | `req` |
| `requires-wrapper` | `req-wrap` |

**Component name shortening:** The family root name is stripped from
component names when the component starts with it. For example, in the
`DualListSelector` family, `DualListSelectorControl` becomes `control`.
When stripping would produce an empty string (component equals the family
root), the full name is kept. Components that don't start with the family
name are kept as-is (e.g., `Tr` in the `Table` family stays `tr`).

For deprecated families like `deprecated/DualListSelector`, only the base
name (`DualListSelector`) is used for prefix stripping.

**Rule ID formats:**

| Rule type | Format | Example |
|---|---|---|
| notParent | `sd-cf-{family}-{child}-in-{parent1-or-parent2}` | `sd-cf-duallistselector-control-in-list-or-tree` |
| invalidDirectChild | `sd-cf-{family}-{child}-not-in-{grandparent}-use-{parent1-or-parent2}` | `sd-cf-table-td-not-in-table-use-tr` |
| requiresChild | `sd-cf-{family}-{parent}-req-{child1-and-child2}` | `sd-cf-tabs-tabs-req-tab` |
| exclusiveWrapper | `sd-cf-{family}-{parent}-req-wrap` | `sd-cf-inputgroup-inputgroup-req-wrap` |

Implementation: `short_component_id()` in `konveyor_v2.rs` handles the
stripping, `sanitize()` handles lowercasing and special character replacement.

#### CSS Element → Component Mapping (CRITICAL)

`build_css_element_to_component_map` maps CSS BEM element names to React
components via `css_tokens_used`. The map type is
`HashMap<String, HashSet<String>>` — multiple components can map to the
same BEM element (e.g., `DrawerContentBody` and `DrawerPanelBody` both
render `__body`). When an element maps to multiple components, all
CSS-based signal steps (2, 3, 3b, 5, 5.5) create edges to all candidates,
but with `Allowed` strength (the CSS class is ambiguous across components).
Single-component elements use the step's normal strength logic.

**Tokens are stored with the `"styles."` prefix** (e.g.,
`"styles.drawerBody"`). The mapping function must strip `"styles."` before
matching against the BEM block name. Skip `"styles.modifiers."` tokens —
they don't map to BEM elements.

The root component is prevented from claiming child CSS tokens when a
dedicated component already exists for that element. This prevents the
root from shadowing dedicated components in the map.

#### CSS Profile Loading

CSS profiles are loaded from a dependency repo (e.g., `@patternfly/patternfly`).
The orchestrator uses `WorktreeGuard::create_only` for the dep repo (not
`WorktreeGuard::new`, which requires tsconfig.json). The caller-provided build
command (e.g., `yarn install && npx gulp buildPatternfly`) runs directly in
the worktree.

Multiple CSS files per component directory are all read and merged via
`merge_css_profile`. The old `enrich_trees_with_css` function is no longer
called — CSS enrichment is integrated into the v2 builder.

#### children_slot_detail

A parallel field to `children_slot_path` that captures CSS tokens alongside
tag names. Each entry is `(tag_name, Option<css_token>)`. Used by the flex
context step (step 4) to determine what CSS element wraps `{children}`:

```
children_slot_path:   ["div", "div"]
children_slot_detail: [("div", Some("toolbarContent")), ("div", Some("toolbarContentSection"))]
```

Extraction uses a single AST parse via `trace_children_slot_both` (not separate
parses for path and detail).

#### Known Issues

- **Non-component exports**: Context objects (AlertContext, FormContext,
  TabsContext, WizardContext, etc.) appear as orphan family members when
  they are barrel-file exports. They have zero edges and don't affect
  rule generation, but they add noise to the tree's member list. Future
  work: filter members whose source file only exports context/type
  declarations (no JSX rendering).
- **Primary vs secondary CSS block mismatch**: When the root's BEM block
  differs from the dominant BEM block (e.g., Modal root uses `"backdrop"`
  but most members use `"modalBox"`), the primary CSS profile covers the
  children's block. Step 8.6 handles this by treating the dominant block as
  a secondary block for sub-root fallback. A future refactor should unify
  primary/secondary processing into a single multi-block loop.

#### Single-Component Families (Skip for Composition)

The following 51 families are genuinely single-component — one file in the
directory, no sub-components, no composition tree needed. Skip these during
composition tree validation:

Avatar, BackToTop, Backdrop, BackgroundImage, Badge, Banner, Brand, Button,
CalendarMonth, Chart, ChartArea, ChartAxis, ChartBar, ChartBoxPlot,
ChartContainer, ChartCursorContainer, ChartDonut, ChartGroup, ChartLabel,
ChartLegend, ChartLine, ChartPie, ChartPoint, ChartScatter, ChartStack,
ChartThreshold, ChartTooltip, ChartVoronoiContainer, Charts, Checkbox,
Content, DatePicker, Divider, FormControl, Icon, Line, NotificationBadge,
NumberInput, Radio, Sankey, Skeleton, SkipToContent, Spinner, Switch,
TextArea, TextInput, Timestamp, Title, Truncate, deprecated/DragDrop,
deprecated/Tile.

#### Composition Tree Ground Truth (PatternFly v6.4.1)

The expected consumer-facing API for each multi-component family is derived
from the barrel file (index.ts) exports at the v6.4.1 tag. Context providers
and type exports are excluded. This is the definitive reference for
composition tree validation.

Families not listed here are either single-component (see above) or
internally-rendered-only (Popover, Tooltip, AboutModal, SearchInput, Slider,
TimePicker, ChartBullet, ChartCursorTooltip, ChartLegendTooltip — all
sub-components are internally rendered, not consumer-placed).

| Family | Expected Exports | Notes |
|--------|-----------------|-------|
| Accordion | Accordion, AccordionContent, AccordionExpandableContentBody, AccordionItem, AccordionToggle | — |
| ActionList | ActionList, ActionListGroup, ActionListItem | — |
| Alert | Alert, AlertActionCloseButton, AlertActionLink, AlertGroup | AlertActionLink: prop-passed via `actionLinks` |
| Breadcrumb | Breadcrumb, BreadcrumbHeading, BreadcrumbItem | — |
| Card | Card, CardBody, CardExpandableContent, CardFooter, CardHeader, CardTitle | — |
| ClipboardCopy | ClipboardCopy, ClipboardCopyAction, ClipboardCopyButton | — |
| CodeBlock | CodeBlock, CodeBlockAction, CodeBlockCode | CodeBlockAction: prop-passed via `actions` |
| CodeEditor | CodeEditor, CodeEditorControl | — |
| DataList | DataList, DataListAction, DataListCell, DataListCheck, DataListContent, DataListControl, DataListDragButton, DataListItem, DataListItemCells, DataListItemRow, DataListText, DataListToggle | — |
| DescriptionList | DescriptionList, DescriptionListDescription, DescriptionListGroup, DescriptionListTerm, DescriptionListTermHelpText, DescriptionListTermHelpTextButton | — |
| Drawer | Drawer, DrawerActions, DrawerCloseButton, DrawerContent, DrawerContentBody, DrawerHead, DrawerPanelBody, DrawerPanelContent, DrawerPanelDescription, DrawerSection | DrawerPanelBody: multi-component CSS map |
| Dropdown | Dropdown, DropdownGroup, DropdownItem, DropdownList | — |
| DualListSelector | DualListSelector, DualListSelectorControl, DualListSelectorControlsWrapper, DualListSelectorList, DualListSelectorListItem, DualListSelectorPane, DualListSelectorTree | — |
| EmptyState | EmptyState, EmptyStateActions, EmptyStateBody, EmptyStateFooter | — |
| ExpandableSection | ExpandableSection, ExpandableSectionToggle | ExpandableSectionToggle: prop-passed via `toggleContent` |
| FileUpload | FileUpload, FileUploadField, FileUploadHelperText | — |
| Form | ActionGroup, Form, FormAlert, FormFieldGroup, FormFieldGroupExpandable, FormFieldGroupHeader, FormGroup, FormGroupLabelHelp, FormHelperText, FormSection | FormGroupLabelHelp: prop-passed via `label` |
| FormSelect | FormSelect, FormSelectOption, FormSelectOptionGroup | — |
| HelperText | HelperText, HelperTextItem | — |
| Hint | Hint, HintBody, HintFooter, HintTitle | — |
| InputGroup | InputGroup, InputGroupItem, InputGroupText | — |
| JumpLinks | JumpLinks, JumpLinksItem, JumpLinksList | — |
| Label | Label, LabelGroup | — |
| List | List, ListItem | — |
| LoginPage | Login, LoginFooter, LoginFooterItem, LoginForm, LoginHeader, LoginMainBody, LoginMainFooter, LoginMainFooterBandItem, LoginMainFooterLinksItem, LoginMainHeader, LoginPage | LoginFooterItem: prop-passed via `footer`; LoginForm: exported orphan (convenience composite) |
| Masthead | Masthead, MastheadBrand, MastheadContent, MastheadLogo, MastheadMain, MastheadToggle | — |
| Menu | DrilldownMenu, Menu, MenuBreadcrumb, MenuContainer, MenuContent, MenuFooter, MenuGroup, MenuItem, MenuItemAction, MenuList, MenuSearch, MenuSearchInput | MenuContainer: exported orphan (standalone orchestrator) |
| MenuToggle | MenuToggle, MenuToggleAction, MenuToggleCheckbox | MenuToggleCheckbox: exported orphan (opaque slot via `splitButtonItems`) |
| Modal | Modal, ModalBody, ModalFooter, ModalHeader | Cross-block: ModalBody/ModalFooter via Step 8.6 (modalBox sub-block) |
| MultipleFileUpload | MultipleFileUpload, MultipleFileUploadMain, MultipleFileUploadStatus, MultipleFileUploadStatusItem | — |
| Nav | Nav, NavExpandable, NavGroup, NavItem, NavItemSeparator, NavList | — |
| NotificationDrawer | NotificationDrawer, NotificationDrawerBody, NotificationDrawerGroup, NotificationDrawerGroupList, NotificationDrawerHeader, NotificationDrawerList, NotificationDrawerListItem, NotificationDrawerListItemBody, NotificationDrawerListItemHeader | — |
| OverflowMenu | OverflowMenu, OverflowMenuContent, OverflowMenuControl, OverflowMenuDropdownItem, OverflowMenuGroup, OverflowMenuItem | — |
| Page | Page, PageBody, PageBreadcrumb, PageGroup, PageSection, PageSidebar, PageSidebarBody, PageToggleButton | PageSidebar: prop-passed via `sidebar`; PageBreadcrumb: prop-passed via `breadcrumb` |
| Pagination | Pagination, ToggleTemplate | — |
| Panel | Panel, PanelFooter, PanelHeader, PanelMain, PanelMainBody | — |
| Progress | Progress, ProgressBar, ProgressContainer | — |
| ProgressStepper | ProgressStepper, ProgressStep | — |
| Select | Select, SelectGroup, SelectList, SelectOption | — |
| Sidebar | Sidebar, SidebarContent, SidebarPanel | — |
| SimpleList | SimpleList, SimpleListGroup, SimpleListItem | — |
| Table | (see note) | (see note) |
| Tabs | Tab, TabAction, TabContent, TabContentBody, TabTitleIcon, TabTitleText, Tabs | TabContentBody: cross-block via Step 8.6 (tabContent sub-block) |
| TextInputGroup | TextInputGroup, TextInputGroupMain, TextInputGroupUtilities | — |
| ToggleGroup | ToggleGroup, ToggleGroupItem | — |
| Toolbar | Toolbar, ToolbarContent, ToolbarExpandableContent, ToolbarExpandIconWrapper, ToolbarFilter, ToolbarGroup, ToolbarItem, ToolbarToggleGroup | — |
| TreeView | TreeView, TreeViewSearch | — |
| Wizard | Wizard, WizardBody, WizardFooter, WizardHeader, WizardNav, WizardNavItem, WizardStep, WizardToggle | WizardHeader: prop-passed via `header` |
| deprecated/Chip | Chip, ChipGroup | — |
| deprecated/DualListSelector | DualListSelector, DualListSelectorControl, DualListSelectorControlsWrapper, DualListSelectorList, DualListSelectorListItem, DualListSelectorPane, DualListSelectorTree | — |
| deprecated/Modal | Modal, ModalBox, ModalBoxBody, ModalBoxCloseButton, ModalBoxFooter, ModalBoxHeader, ModalContent | — |
| deprecated/Wizard | Wizard, WizardBody, WizardFooter, WizardHeader, WizardNav, WizardNavItem, WizardToggle | WizardFooter: prop-passed via `footer` |

**Note on Table:** The Table family exports many components (Caption, Tbody,
Thead, Tr, Td, Th, etc.) plus utility wrappers (ActionsColumn, RowWrapper,
TreeRowWrapper, InnerScrollContainer, OuterScrollContainer, SelectColumn,
etc.). The current tree has 16 members and 18 edges. Some utility wrappers
appear as exported orphans (EditableSelectInputCell, EditableTextCell,
SelectColumn, FavoritesCell, OuterScrollContainer, InnerScrollContainer,
TableTypes).

**Summary:** All expected consumer-facing components are present in
composition trees. 3 components are retained as exported orphans with no
edges (LoginForm, MenuContainer, MenuToggleCheckbox) — these are
convenience composites or orchestrators with no structural composition
signal. Context providers (AlertContext, FormContext, TabsContext,
WizardContext, etc.) appear as orphan members when exported from barrel
files; they don't affect rule generation.

#### Family Grouping and Deprecated Separation

`extract_family_from_path` in `sd_pipeline.rs` determines which component
family a file belongs to. It looks for the `"components"` path segment and
takes the next segment as the family name. When a modifier directory
(`deprecated/` or `next/`) precedes `"components"`, it is included as a
prefix:

- `src/components/DualListSelector/` → `"DualListSelector"`
- `src/deprecated/components/DualListSelector/` → `"deprecated/DualListSelector"`
- `src/next/components/Foo/` → `"next/Foo"`

The tree's `root` field is set to the family name (not the component name)
after tree construction and collapse, so deprecated families are
distinguishable from main families in the output.

Files from the `code-connect` package (Figma integration) are excluded from
SD file discovery via `should_exclude_from_sd`.

#### Deprecated Profile Key Collision (CRITICAL)

When a component name exists in both main and deprecated paths (e.g.,
`ModalContent` in `src/components/Modal/` and
`src/deprecated/components/Modal/`), the main version wins in the global
`new_profiles` map. The deprecated version is preserved in a separate
`deprecated_profiles` map. `collect_family_profiles()` uses the deprecated
profile when building a deprecated family's tree, ensuring each family
sees its own version of shared component names.

**Never** allow a global profile map lookup to silently return the wrong
version's profile for a deprecated family. The deprecated version of a
component may render different sub-components (e.g., deprecated
`ModalContent` renders `ModalBoxBody/Footer/Header` while v6
`ModalContent` renders `{children}` passthrough).

#### `collapse_internal_nodes` Algorithm (CRITICAL)

`collapse_internal_nodes` in `sd_pipeline.rs` removes non-exported
components from the tree, creating transitive edges that bypass them.
It processes **one internal node at a time**, preferring leaf nodes
(those whose children are all resolved). This is critical for multi-level
internal chains like:

```
Modal → ModalContent(int) → ModalBox(int) → ModalBody
```

Processing one node at a time ensures:
1. Collapse ModalBox first → creates `ModalContent → ModalBody`
2. Collapse ModalContent → creates `Modal → ModalBody`

**Never** process all internal nodes in a single pass and remove all
their edges at once. This breaks multi-level chains because intermediate
transitive edges reference nodes that haven't been collapsed yet, and
removing all internal edges destroys the chain.

Collapsed edges inherit:
- The **stronger** `EdgeStrength` of the two edges in the chain
- The child edge's `relationship` type
- The child edge's `prop_name` (propagated through transitive edges)

Regression test:
`sd_pipeline::tests::test_collapse_three_level_internal_chain` — uses
the real Modal family structure with 9 members and verifies that the
3-level chain collapses to `Modal → ModalBody/ModalFooter/ModalHeader`.

Integration test:
`sd_pipeline::tests::test_modal_family_integration_real_files` — uses
real PatternFly source files and CSS to verify the full pipeline
end-to-end for the Modal family. Requires files at
`/tmp/semver-pipeline-v2/repos/` (marked `#[ignore]`).

### BEM Block Independence (CRITICAL)

When classifying parent-child relationships via BEM analysis, components with
their own distinct BEM block must be classified as `Independent`, not as
elements of another component's block. There are two code paths that enforce
this:

1. **`classify_bem_relationship()` in `bem.rs`** — Checks `child_block !=
   parent_block` BEFORE token prefix matching. This prevents camelCase naming
   collisions (e.g., `labelGroup` from `label-group` appearing to be element
   `Group` of block `label`).

2. **`infer_ownership_by_name_prefix()` in `composition/mod.rs`** — Uses strict
   block equality (`child_block_lower == root_name_lower`). Only proceeds when
   the child's dominant BEM block is the SAME as the root's name. Rejects all
   cases where they differ, because BEM blocks are stored in camelCase
   (`kebab_to_camel_case` at extraction time), making it impossible to
   distinguish separate blocks from sub-elements by name alone.

**Known collision families** (upstream-verified as independent):
- `label` vs `label-group` (Label / LabelGroup — LabelGroup CONTAINS Labels)
- `alert` vs `alert-group` (Alert / AlertGroup — AlertGroup CONTAINS Alerts)
- `menu` vs `menu-toggle` (Menu / MenuToggle — MenuToggle is a trigger, not a child)
- `form` vs `form-control` (Form / FormControl — separate component)
- `progress` vs `progress-stepper` (Progress / ProgressStepper — unrelated components)

**Never** add a composition edge between components that have different BEM
blocks. If a future PF component genuinely needs cross-block ownership, add
an explicit override rather than weakening the BEM independence checks.

### Composition Rule Generation (CRITICAL)

The rule generator in `konveyor_v2.rs` creates three types of composition rules:

- **`removed-member`** — Fires when a removed component is still used as JSX.
  These are kept and are correct.
- **`requires`** — Removed. Redundant with conformance rules which check the
  same parent-child relationship from the child's perspective (via `notParent`).
  Conformance is more precise because it only fires when the child is misplaced.
- **`new-member`** — Removed. The migration rule (`component-import-deprecated`)
  already lists new child components in its message. New-member rules fired on
  every parent usage regardless of whether the new child was already present.

**Conformance rules are filtered by `EdgeStrength::Required`.** Only edges
where rendering actually breaks (CSS `>` selectors, grid layout, context,
DOM nesting, cloneElement) generate `notParent` conformance rules. Edges from
CSS descendant selectors and flex context (tagged `Allowed`) stay in the tree
for documentation but don't generate scanner rules. This prevents false
positives from CSS descendant selectors that match at any depth.

**Migration rule `when` clauses** (`component-import-deprecated`) use:
- `JSX_PROP` conditions (one per removed prop) for Modified components — only
  fires when a deprecated prop is actually used
- `IMPORT` trigger for fully Removed components — importing a removed component
  is itself the issue
- `child` filter for structural detection — fires when old internal components
  are still used as children

### Type-Incompatible Member Renames (CRITICAL)

When a property is renamed AND its type changes to a structurally different
category (e.g., `splitButtonOptions: SplitButtonOptions` →
`splitButtonItems: ReactNode[]`), the diff engine must emit a **single
`StructuralChangeType::Changed`** entry — NOT separate Removed + Added entries.

- **rename.rs Pass 4** detects these via name similarity (≥0.6 threshold)
- **compare.rs `diff_members()`** separates compatible vs incompatible renames
  using `types_structurally_similar()` (compares `TypeCategory`: Reference vs
  Array vs Object vs Function vs Primitive vs Tuple)
- Type-compatible renames → `StructuralChangeType::Renamed` (mechanical codemod)
- Type-incompatible renames → `StructuralChangeType::Changed` with `before` =
  old signature, `after` = new signature (routes to LLM-assisted fixing via
  `ApiChangeType::SignatureChanged`)

**Never** drop type-incompatible renames back to separate Removed + Added. This
loses the linkage between old and new prop, producing a useless "remove prop,
find an alternative" fix strategy instead of "prop X was replaced by prop Y
with a different type."

Regression test:
`crates/core/src/diff/compare.rs::test_type_incompatible_member_rename_produces_changed_not_removed_plus_added`
— uses real PatternFly `MenuToggleProps` data
(`splitButtonOptions: SplitButtonOptions` → `splitButtonItems: ReactNode[]`).

### Deprecated Replacement Detection

When a component is relocated to `/deprecated/` AND a differently-named
component replaces it (e.g., `Chip` → `Label`), the standard rename detector
cannot find the relationship because:

1. `Label` already exists in both v5 and v6 (never enters the "added" pool)
2. Relocation detection claims `Chip` before rename detection runs
3. "Chip" and "Label" have zero lexical similarity (LCS = 0)

The **deprecated replacement detection** step in `src/orchestrator.rs` solves
this by using **rendering swap** signals from the SD pipeline. After both TD and
SD pipelines complete but before the report is assembled:

1. `detect_deprecated_replacements()` finds relocated components where host
   components stopped rendering the old component and started rendering a new
   one (e.g., ToolbarFilter stopped rendering `Chip`, started rendering `Label`)
2. `apply_deprecated_replacements()` transforms the structural changes:
   - Relocation entries → `Changed` with `before="Chip"`, `after="Label"`
   - Props relocations → `Changed` with `before="ChipProps"`, `after="LabelProps"`
   - Suppresses redundant signature-changed entries (base class change)
   - Preserves non-replaced relocations (Modal, Tile, etc.) unchanged

The detection filters out Fragment, React.Fragment, other relocated components,
and uses a Group-suffix tiebreaker when candidates have equal host evidence.

Key type: `DeprecatedReplacement` in `crates/core/src/types/sd.rs`
Key functions: `detect_deprecated_replacements()`, `apply_deprecated_replacements()`
  in `src/orchestrator.rs`
Tests: `deprecated_replacement_tests` module in `src/orchestrator.rs` (15 tests)

### Konveyor Rules

- `crates/ts/src/konveyor.rs` — v1 rule generation (TD pipeline)
- `crates/ts/src/konveyor_v2.rs` — v2 rule generation (SD pipeline: composition,
  conformance, context, prop-to-child migration, test impact, CSS removal,
  prop-attribute-override)
- `crates/konveyor-core/src/lib.rs` — Shared rule types, fix strategies

### Testing

```sh
cargo test -p semver-analyzer-ts --lib    # ~650 unit tests
cargo test -p semver-analyzer-ts          # + integration tests
cargo test                                # full suite
```

### Error Handling & Diagnostics (CRITICAL)

The project uses a three-layer error reporting architecture. **Every error the
user sees must answer: What happened? Why? What can I do about it?**

#### Three Layers

1. **Fatal errors** — propagate via `anyhow::Result` with tips attached via
   the `Diagnosed` wrapper. Rendered by `src/diagnostics.rs::render_error()`
   with colored output (red error, dimmed chain, cyan tips).
2. **Non-fatal degradation** — recorded via `DegradationTracker` on
   `SharedFindings`, summarized at end of run with `print_degradation_summary()`.
3. **Best-effort operations** — logged at `trace` level, return
   `None`/default (e.g., `read_git_file()` in `crates/ts/src/git_utils.rs`).

#### ErrorTip Trait and Diagnosed Wrapper

`ErrorTip` (in `crates/core/src/error.rs`) is the contract for errors that
carry user-facing remediation tips. `Diagnosed` is a marker type that carries
tips through the `anyhow` error chain. The CLI extracts tips via a single
`downcast_ref::<Diagnosed>()` — no per-language-type dispatch needed.

**When adding a new error type:**

1. Define the error enum with `thiserror::Error`
2. Implement `ErrorTip` — every variant that a user can trigger MUST have
   a tip explaining what to do
3. At the boundary where the error enters `anyhow::Result`, call
   `.diagnose()` (from `DiagnoseWithTip`) to capture the tip

**Extension traits for tip attachment:**

- `DiagnoseWithTip` — for `Result<T, E: ErrorTip>`: `.diagnose()` auto-extracts
  the tip from the error's `tip()` method
- `DiagnoseExt` — for any `Result`: `.with_diagnosis("tip text")` attaches
  an explicit tip string

**Never:**

- Return a bare `anyhow::bail!()` for errors caused by user input or
  environment issues — always attach a tip via `.with_diagnosis()` or
  `.diagnose()`
- Add `downcast_ref` calls in the CLI's `extract_tip()` — the `Diagnosed`
  wrapper handles dispatch for all languages automatically
- Use `eprintln!` directly for error output in production code — all
  user-facing errors flow through `render_error()` or
  `print_degradation_summary()`

**Example — Language implementor:**

```rust
// Define error type with thiserror
#[derive(Debug, thiserror::Error)]
pub enum GoBuildError {
    #[error("go build failed: {reason}")]
    BuildFailed { reason: String },
}

// Implement ErrorTip
impl ErrorTip for GoBuildError {
    fn tip(&self) -> Option<String> {
        Some(match self {
            Self::BuildFailed { .. } =>
                "Try running 'go build ./...' manually in the repo.".to_string(),
        })
    }
}

// At boundary — .diagnose() captures the tip into Diagnosed
fn extract(&self, repo: &Path, git_ref: &str) -> Result<ApiSurface> {
    let guard = WorktreeGuard::new(repo, git_ref, cmd).diagnose()?;
    // ...
}

// For non-ErrorTip errors — .with_diagnosis() attaches explicit tip
fs::read(path)
    .with_context(|| format!("Failed to read {}", path.display()))
    .with_diagnosis("Check the file exists and you have read permissions.")?;
```

#### DegradationTracker

`DegradationTracker` (in `crates/core/src/diagnostics.rs`) collects non-fatal
issues during the pipeline run. It lives on `SharedFindings` and is accessible
to all pipeline phases and Language implementations via
`shared.degradation()`.

**When to record a degradation:**

- A pipeline phase fails but execution can continue with empty/partial results
- An external tool (LLM, CSS extraction, dep repo build) fails
- Multiple per-item failures occur (batch into a single summary entry)

**When NOT to record:**

- Best-effort operations where failure is a normal code path (e.g.,
  `read_git_file` returning `None` for a file that may not exist)
- Cleanup/teardown failures (Drop impls, worktree removal)

Each `DegradationIssue` has three fields:

- `phase`: short tag ("TD", "SD", "BU", "CSS", "LLM")
- `message`: what happened (technical, concise)
- `impact`: what the user is missing (user-facing, actionable)

The tracker is surfaced to the CLI via `AnalysisResult::degradation` and
rendered by `diagnostics::print_degradation_summary()`.

#### Progress Reporter: Success vs Failure Icons

Use `phase.finish_failed()` (shows ✗) for phases that failed but were
non-fatal. Use `phase.finish()` / `phase.finish_with_detail()` (shows ✓)
for successful completion. **Never** show ✓ for a failed phase.

#### Silent Error Swallowing Rules

| Pattern | When to use | Must log? |
|---------|-------------|-----------|
| `.ok()?` returning `None` | File may legitimately not exist (git show, package.json) | `trace!` level |
| `.unwrap_or_default()` | Mutex poisoning in cleanup, broadcast send | No |
| `let _ =` | Drop/RAII cleanup, broadcast channels | No |
| `if let Ok(...)` | Optional enrichment that doesn't affect correctness | `trace!` level |
| `warn!` + fallback | Phase failure with graceful degradation | Yes + record degradation |

**Never** use `.ok()`, `.unwrap_or_default()`, or `if let Ok(...)` to swallow
errors from operations the user explicitly requested (git checkout, build
commands, file writes). These must propagate as fatal errors with tips.

#### Partial Extraction Warnings (WorktreeGuard)

When `WorktreeGuard::new()` succeeds but encountered non-fatal issues
(partial tsc success, fallback to project build), it stores
`ExtractionWarning` entries accessible via `guard.warnings()`.

The `Language::extract()` method accepts an optional `&DegradationTracker`.
`extract_at_ref()` inspects `guard.warnings()` after successful creation
and records them as degradation. This ensures partial-success scenarios
appear in the end-of-run summary rather than scrolling by as raw
`tracing::warn!` lines.

**Warning types (`ExtractionWarning` in `crates/ts/src/worktree/mod.rs`):**

| Variant | When |
|---------|------|
| `PartialTscBuildFailed` | Some packages compiled, project build also failed — API surface may be incomplete |
| `TscFailedBuildSucceeded` | tsc failed entirely, project build succeeded as fallback |

Per-package `tsc failed for package X` messages stay as `tracing::warn!`
(visible in `--log-file`). Only the aggregate outcome is captured as a
structured `ExtractionWarning`.

#### `read_git_file()` / `git_diff_file()` (Shared Utilities)

Single implementations in `crates/ts/src/git_utils.rs`. Return
`Option<String>`. Log at `trace!` level on failure. **Do not duplicate
these functions** — there were previously 4+ copies across the codebase.

#### Error Display at CLI Level

The CLI renderer (`src/diagnostics.rs::render_error()`) handles all error
formatting. It:

1. Walks the `anyhow` chain for `Diagnosed` markers (single downcast)
2. Renders colored output: red `error:`, dimmed `caused by:`, cyan `tip:`
3. Falls back to pattern-matching on error text for undiagnosed errors

The `main()` function catches the `anyhow::Error` and passes it to
`render_error()` — it does NOT use the default `anyhow::Result` return
from `main()`.

#### Key Files

| File | Purpose |
|------|---------|
| `crates/core/src/error.rs` | `ErrorTip` trait, `Diagnosed` wrapper, `DiagnoseWithTip` / `DiagnoseExt` |
| `crates/core/src/diagnostics.rs` | `DegradationTracker`, `DegradationIssue` |
| `crates/core/src/shared.rs` | `SharedFindings::degradation()` accessor |
| `crates/ts/src/worktree/error.rs` | `WorktreeError` + `ErrorTip` impl with tips for all variants |
| `crates/ts/src/git_utils.rs` | Shared `read_git_file()`, `git_diff_file()` with trace logging |
| `src/diagnostics.rs` | `render_error()`, `print_degradation_summary()` |
| `src/progress.rs` | `PhaseGuard::finish_failed()` for ✗ indicator |

## PatternFly v5 → v6 Reference

The primary test case is PatternFly React v5.4.0 → v6.4.1. Key stats:

- 15,525 total breaking changes
- 340 non-token removals, 4,094 renames (3,995 CSS tokens), 3,866 type changes
- 28 known false-positive renames (see design doc for full details)
- Full change landscape and verification data in
  `design/rename-detector-verification.md`
