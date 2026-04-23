//! Language-specific API surface extraction implementations.
//!
//! Currently supports TypeScript/JavaScript via OXC + tsc.
//! Future: Python (tree-sitter), Go (tree-sitter), Rust (rustdoc JSON).

use anyhow::{Context, Result};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use semver_analyzer_core::ApiSurface as CoreApiSurface;
use semver_analyzer_core::Symbol as CoreSymbol;
use semver_analyzer_core::{
    AccessorKind, Parameter, Signature, SymbolKind, TypeParameter, Visibility,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::TsSymbolData;

/// Type aliases: all Symbols/ApiSurfaces in the TS extractor carry `TsSymbolData`.
type Symbol = CoreSymbol<TsSymbolData>;
type ApiSurface = CoreApiSurface<TsSymbolData>;

/// Extracts API surfaces from TypeScript `.d.ts` files using the OXC parser.
///
/// This is the TypeScript/JavaScript implementation of `ApiExtractor`.
/// It relies on `tsc --declaration` having already been run to produce `.d.ts` files,
/// then parses those files with OXC to extract the public API surface.
///
/// Key design decision: type annotations are extracted as source-text strings
/// via span slicing (`source[start..end]`), not by walking/reconstructing the
/// type AST. This preserves exactly what `tsc` produced, which is already
/// partially canonicalized. Additional canonicalization is done in Step 4.
#[derive(Default)]
pub struct OxcExtractor;

impl OxcExtractor {
    pub fn new() -> Self {
        Self
    }

    /// Extract API surface from all `.d.ts` files in a directory.
    ///
    /// Uses a two-pass approach to build accurate import maps:
    ///
    /// **Phase 0**: Scan `node_modules/@types/` for packages that declare global
    /// namespaces (e.g., `@types/react` → `React`). This catches types used
    /// without explicit import statements.
    ///
    /// **Phase 1**: Parse all `.d.ts` files and collect their import declarations.
    /// Merge all namespace/default imports into a project-wide import map. If
    /// *any* file has `import * as React from 'react'`, we know `React.X` means
    /// `X` from `react` everywhere.
    ///
    /// **Phase 2**: Extract symbols from each file using the merged global import
    /// map as a fallback. Per-file imports take priority over the global map.
    pub fn extract_from_dir(&self, dir: &Path) -> Result<ApiSurface> {
        let all_files = find_dts_files(dir)?;

        // Phase -1: Filter to only files reachable from package entry points (index.d.ts).
        // This excludes internal implementation files that aren't re-exported.
        // Also builds a provenance map for import_path detection.
        let reachability = filter_to_reachable(&all_files, dir);
        let files = &reachability.files;

        // Phase 0: Scan @types/* for global namespace declarations
        let mut global_imports = scan_types_packages(dir);
        let types_count = global_imports.len();

        // Phase 1: Collect per-file imports, merge namespace/default into global
        let mut file_sources: Vec<(PathBuf, String)> = Vec::new();
        for file_path in files {
            let source = std::fs::read_to_string(file_path)
                .with_context(|| format!("Failed to read {}", file_path.display()))?;
            let file_imports = collect_imports_from_source(&source);
            global_imports.merge_namespaces_from(&file_imports);
            file_sources.push((file_path.clone(), source));
        }

        if !global_imports.is_empty() {
            let from_files = global_imports.len() - types_count;
            if types_count > 0 || from_files > 0 {
                tracing::debug!(
                    from_types = types_count,
                    from_files = from_files,
                    "Global import map built"
                );
            }
        }

        // Phase 2: Extract symbols using per-file imports + global fallback
        let mut symbols = Vec::new();
        for (file_path, source) in &file_sources {
            let relative = file_path.strip_prefix(dir).unwrap_or(file_path);
            let mapped = remap_dist_to_src(relative);
            symbols.extend(self.extract_from_source_with_globals(source, &mapped, &global_imports));
        }

        // Phase 3: Set package name based on file path.
        // Build a cache of directory name -> npm package name by reading
        // each packages/<dir>/package.json for the "name" field.
        let mut pkg_name_cache: HashMap<String, String> = HashMap::new();
        for sym in &symbols {
            let path_str = sym.file.to_string_lossy();
            let parts: Vec<&str> = path_str.split('/').collect();
            if parts.len() >= 2 && parts[0] == "packages" {
                let dir_name = parts[1].to_string();
                if let std::collections::hash_map::Entry::Vacant(e) = pkg_name_cache.entry(dir_name)
                {
                    let pkg_json_path = dir.join("packages").join(e.key()).join("package.json");
                    let npm_name = std::fs::read_to_string(&pkg_json_path)
                        .ok()
                        .and_then(|content| {
                            serde_json::from_str::<serde_json::Value>(&content).ok()
                        })
                        .and_then(|v| v.get("name")?.as_str().map(|s| s.to_string()));
                    if let Some(name) = npm_name {
                        e.insert(name);
                    }
                }
            }
        }

        for sym in &mut symbols {
            let path_str = sym.file.to_string_lossy();
            let parts: Vec<&str> = path_str.split('/').collect();
            if parts.len() >= 2 && parts[0] == "packages" {
                if let Some(npm_name) = pkg_name_cache.get(parts[1]) {
                    sym.package = Some(npm_name.clone());
                }
            }
        }

        // Phase 4: Set import_path based on entry point provenance.
        // If a symbol is only reachable from a subpath entry point (e.g.,
        // victory/index.d.ts), its import_path differs from the package root.
        if !reachability.provenance.is_empty() {
            set_import_paths(&mut symbols, &file_sources, &reachability.provenance, dir);
        }

        // Phase 5: Populate rendered_components from .tsx source files.
        // For each symbol that could be a React component, find the corresponding
        // .tsx file in the worktree and extract its JSX render tree.
        populate_rendered_components(&mut symbols, dir);

        Ok(ApiSurface { symbols })
    }

    /// Extract symbols from `.d.ts` source code.
    ///
    /// This is the core extraction function. It parses the source with OXC
    /// and walks the AST to find all exported declarations.
    ///
    /// First pass: collect import declarations into an `ImportMap`.
    /// Second pass: extract exported symbols, using the import map to
    /// canonicalize type annotations.
    pub fn extract_from_source(&self, source: &str, file_path: &Path) -> Vec<Symbol> {
        self.extract_from_source_with_globals(source, file_path, &crate::canon::ImportMap::new())
    }

    /// Extract symbols with a global import map as fallback.
    ///
    /// The global import map contains namespace/default imports discovered
    /// across all project files and from `@types/*` packages. Per-file imports
    /// take priority: the file's own `ImportMap` is built first, then the
    /// global entries are merged in as fallbacks.
    fn extract_from_source_with_globals(
        &self,
        source: &str,
        file_path: &Path,
        global_imports: &crate::canon::ImportMap,
    ) -> Vec<Symbol> {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, source, SourceType::d_ts()).parse();

        if !ret.errors.is_empty() {
            for err in &ret.errors {
                tracing::warn!(file = %file_path.display(), error = %err, "Parse error in .d.ts file");
            }
        }

        // Collect per-file imports, then merge global fallbacks
        let mut imports = collect_imports(source, &ret.program.body);
        imports.merge_all_from(global_imports);

        let line_offsets = compute_line_offsets(source);
        let mut symbols = Vec::new();

        for stmt in &ret.program.body {
            extract_statement(
                source,
                stmt,
                file_path,
                &line_offsets,
                &imports,
                &mut symbols,
            );
        }

        symbols
    }
}

impl OxcExtractor {
    /// Extract API surface from a repo at a given git ref.
    ///
    /// Creates a worktree, installs dependencies, runs tsc, and parses .d.ts files.
    /// The `config` parameter controls Node.js version, install command, and
    /// build command overrides for the worktree.
    pub fn extract_at_ref(
        &self,
        repo: &Path,
        git_ref: &str,
        config: &crate::worktree::RefBuildConfig,
        degradation: Option<&semver_analyzer_core::diagnostics::DegradationTracker>,
    ) -> Result<ApiSurface> {
        use crate::worktree::{ExtractionWarning, WorktreeGuard};
        use semver_analyzer_core::error::DiagnoseWithTip;

        // Create worktree, install deps, run tsc --declaration (with fallback)
        let guard = WorktreeGuard::new(repo, git_ref, config).diagnose()?;

        // Record any extraction warnings as degradation
        if let Some(tracker) = degradation {
            for warning in guard.warnings() {
                match warning {
                    ExtractionWarning::PartialTscBuildFailed {
                        succeeded, failed, ..
                    } => {
                        tracker.record(
                            "TD",
                            format!(
                                "tsc partially succeeded ({} packages ok, {} failed) \
                                 and project build also failed at ref {}",
                                succeeded, failed, git_ref
                            ),
                            "API surface may be incomplete — some package \
                             declarations could not be generated",
                        );
                    }
                    ExtractionWarning::TscFailedBuildSucceeded { .. } => {
                        tracker.record(
                            "TD",
                            format!("tsc failed at ref {}, fell back to project build", git_ref),
                            "API surface was extracted via project build — \
                             coverage should be complete",
                        );
                    }
                }
            }
        }

        // Extract from the generated .d.ts files
        self.extract_from_dir(guard.path())
    }
}

// ─── Path remapping ──────────────────────────────────────────────────────

/// Remap a `dist/` path back to its corresponding `src/` path.
///
/// TypeScript projects emit `.d.ts` declaration files into `dist/` directories
/// (e.g., `dist/esm/`, `dist/js/`) that mirror the `src/` layout exactly.
/// This function maps those paths back to `src/` so that the API surface
/// references source files instead of build artifacts.
///
/// The mapping handles all common `dist/` variants:
///   `packages/react-core/dist/esm/components/Button/Button.d.ts`
///   → `packages/react-core/src/components/Button/Button.d.ts`
///
/// Paths that don't contain a recognized `dist/` segment are returned unchanged.
fn remap_dist_to_src(path: &Path) -> PathBuf {
    let path_str = path.to_string_lossy();

    // Known dist output directory names (from tsconfig outDir patterns)
    let dist_segments = [
        "/dist/esm/",
        "/dist/js/",
        "/dist/cjs/",
        "/dist/mjs/",
        "/dist/es/",
        "/dist/commonjs/",
        "/dist/lib/",
    ];

    for segment in &dist_segments {
        if let Some(pos) = path_str.find(segment) {
            // Replace "/dist/<variant>/" with "/src/"
            let before = &path_str[..pos];
            let after = &path_str[pos + segment.len()..];
            return PathBuf::from(format!("{}/src/{}", before, after));
        }
    }

    // Handle bare "/dist/" (no variant subdirectory)
    if let Some(pos) = path_str.find("/dist/") {
        let before = &path_str[..pos];
        let after = &path_str[pos + "/dist/".len()..];
        return PathBuf::from(format!("{}/src/{}", before, after));
    }

    // No dist segment found -- return unchanged
    path.to_path_buf()
}

// ─── Rendered-component enrichment ───────────────────────────────────────

/// Populate `rendered_components` on symbols that represent React components.
///
/// For each symbol whose `.d.ts` declaration suggests it could be a React
/// component (PascalCase name, Variable/Function/Constant kind), we look for
/// the corresponding `.tsx` source file in the worktree and parse its JSX
/// render tree to discover which other components it renders internally.
///
/// Symbol file paths have already been remapped from `dist/` to `src/`
/// (e.g., `packages/react-core/src/components/Modal/Modal.d.ts`). We
/// resolve `.tsx` source by replacing the `.d.ts` extension.
/// Extract `styles.xxx` CSS token names from component source code.
///
/// Finds all `styles.xxx` references (excluding `styles.modifiers`)
/// and returns the token names (e.g., `["inputGroup", "inputGroupItem"]`).
fn extract_css_style_tokens(source: &str) -> Vec<String> {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"styles\.([a-zA-Z][a-zA-Z0-9]+)").unwrap());

    let mut tokens: Vec<String> = Vec::new();
    for cap in RE.captures_iter(source) {
        let token = &cap[1];
        if token == "modifiers" {
            continue;
        }
        if !tokens.contains(&token.to_string()) {
            tokens.push(token.to_string());
        }
    }
    tokens
}

fn populate_rendered_components(symbols: &mut [Symbol], worktree_dir: &Path) {
    use std::collections::HashMap;

    // Build a cache: source-relative .tsx path -> (rendered components, css tokens).
    // Multiple symbols can come from the same file (e.g., a file exports
    // both a component function and a related constant), so we avoid parsing
    // each .tsx file more than once.
    let mut cache: HashMap<PathBuf, (Vec<String>, Vec<String>)> = HashMap::new();

    let mut enriched = 0u32;

    for sym in symbols.iter_mut() {
        // Only React component candidates: PascalCase Variable/Function/Constant.
        if !matches!(
            sym.kind,
            SymbolKind::Variable | SymbolKind::Function | SymbolKind::Constant
        ) {
            continue;
        }
        if !sym.name.starts_with(|c: char| c.is_ascii_uppercase()) {
            continue;
        }

        // Derive the .tsx source path from the symbol's .d.ts file path.
        let dts_path = sym.file.to_string_lossy();
        let tsx_relative = if dts_path.ends_with(".d.ts") {
            PathBuf::from(dts_path.trim_end_matches(".d.ts").to_owned() + ".tsx")
        } else {
            continue;
        };

        if let Some((rendered, css_tokens)) = cache.get(&tsx_relative) {
            if !rendered.is_empty() {
                sym.language_data.rendered_components = rendered.clone();
                enriched += 1;
            }
            if !css_tokens.is_empty() {
                sym.language_data.css = css_tokens.clone();
            }
            continue;
        }

        // Try to read the .tsx file from the worktree.
        let tsx_abs = worktree_dir.join(&tsx_relative);
        let (rendered, css_tokens) = match std::fs::read_to_string(&tsx_abs) {
            Ok(source) => (
                crate::jsx_diff::extract_rendered_components_from_source(&source),
                extract_css_style_tokens(&source),
            ),
            Err(_) => (Vec::new(), Vec::new()),
        };

        if !rendered.is_empty() {
            sym.language_data.rendered_components = rendered.clone();
            enriched += 1;
        }
        if !css_tokens.is_empty() {
            sym.language_data.css = css_tokens.clone();
        }
        cache.insert(tsx_relative, (rendered, css_tokens));
    }

    if enriched > 0 {
        tracing::info!(
            enriched_symbols = enriched,
            tsx_files_parsed = cache.values().filter(|(v, _)| !v.is_empty()).count(),
            "Populated rendered_components from .tsx source files"
        );
    }
}

// ─── File discovery ───────────────────────────────────────────────────────

/// Recursively find all `.d.ts` files in a directory, excluding `node_modules`.
///
/// Deduplicates build outputs: when a package produces `.d.ts` files in
/// multiple `dist/` subdirectories (e.g., `dist/esm/` and `dist/js/`),
/// only the highest-priority variant is kept. Priority order:
/// `esm` > `mjs` > `es` > `js` > `cjs` > `commonjs` > `lib`
fn find_dts_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    find_dts_recursive(dir, &mut files)?;

    // Remove duplicate build outputs (e.g., dist/esm/ vs dist/js/)
    let excluded = find_redundant_dist_dirs(&files);
    if !excluded.is_empty() {
        let before = files.len();
        files.retain(|f| !excluded.iter().any(|dir| f.starts_with(dir)));
        let removed = before - files.len();
        if removed > 0 {
            tracing::debug!(
                removed_count = removed,
                "Deduplicated build outputs: removed redundant .d.ts files"
            );
        }
    }

    files.sort(); // Deterministic ordering
    Ok(files)
}

/// Result of filtering files to those reachable from package entry points.
struct ReachabilityResult {
    /// Files that are reachable from at least one entry point.
    files: Vec<PathBuf>,
    /// Map from reachable file to the entry point `index.d.ts` files it was reached from.
    /// Used to determine `import_path` for each symbol — if a file is only reachable
    /// from a subpath entry point (e.g., `victory/index.d.ts`), its symbols need a
    /// different import specifier than those reachable from the root `index.d.ts`.
    provenance: std::collections::HashMap<PathBuf, Vec<PathBuf>>,
}

/// Filter files to only those reachable from package entry points (`index.d.ts`).
///
/// Traces the `export * from './path'` and `export { X } from './path'` re-export
/// graph starting from each `index.d.ts` file. Only files reachable through this
/// graph are included. This excludes internal implementation files that have `.d.ts`
/// declarations but are not part of the public API.
///
/// Also builds a provenance map recording which entry point(s) each file is
/// reachable from, enabling `import_path` detection for subpath exports.
///
/// If no `index.d.ts` files are found, returns all files unchanged (fallback for
/// packages without barrel exports).
fn filter_to_reachable(files: &[PathBuf], _base_dir: &Path) -> ReachabilityResult {
    use std::collections::{HashMap, HashSet, VecDeque};

    // Find all index.d.ts entry points
    let index_files: Vec<&PathBuf> = files
        .iter()
        .filter(|f| f.file_name().map(|n| n == "index.d.ts").unwrap_or(false))
        .collect();

    if index_files.is_empty() {
        return ReachabilityResult {
            files: files.to_vec(),
            provenance: HashMap::new(),
        };
    }

    // Build a lookup set for fast path resolution
    let file_set: HashSet<PathBuf> = files.iter().cloned().collect();

    // Run a separate BFS from each entry point to track provenance.
    // A file may be reachable from multiple entry points.
    let mut provenance: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

    for entry_point in &index_files {
        let mut visited: HashSet<PathBuf> = HashSet::new();
        let mut queue: VecDeque<PathBuf> = VecDeque::new();

        let ep = entry_point.to_path_buf();
        visited.insert(ep.clone());
        queue.push_back(ep.clone());

        while let Some(file) = queue.pop_front() {
            // Record that this file is reachable from this entry point
            provenance.entry(file.clone()).or_default().push(ep.clone());

            let source = match std::fs::read_to_string(&file) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let parent_dir = file.parent().unwrap_or(std::path::Path::new("."));
            for line in source.lines() {
                let trimmed = line.trim();
                if let Some(from_path) = extract_export_from_path(trimmed) {
                    let resolved = resolve_dts_path(parent_dir, &from_path, &file_set);
                    if let Some(resolved) = resolved {
                        if visited.insert(resolved.clone()) {
                            queue.push_back(resolved);
                        }
                    }
                }
            }
        }
    }

    let original_count = files.len();
    let filtered: Vec<PathBuf> = files
        .iter()
        .filter(|f| provenance.contains_key(*f))
        .cloned()
        .collect();

    let excluded = original_count - filtered.len();
    if excluded > 0 {
        tracing::debug!(
            reachable = filtered.len(),
            total = original_count,
            excluded = excluded,
            "Entry-point filter applied to .d.ts files"
        );
    }

    ReachabilityResult {
        files: filtered,
        provenance,
    }
}

/// Extract the `from` path from an export statement.
///
/// Matches patterns like:
///   `export * from './components';`
///   `export { Button, ButtonProps } from './Button';`
///   `export type { CardProps } from './Card';`
///
/// Returns the path string without quotes (e.g., `./components`).
fn extract_export_from_path(line: &str) -> Option<String> {
    if !line.starts_with("export") {
        return None;
    }

    // Find "from" keyword followed by a quoted string
    let from_idx = line.find(" from ")?;
    let after_from = &line[from_idx + 6..];

    // Extract the quoted path
    let quote_char = after_from.chars().next()?;
    if quote_char != '\'' && quote_char != '"' {
        return None;
    }

    let rest = &after_from[1..];
    let end_idx = rest.find(quote_char)?;
    let path = &rest[..end_idx];

    // Only follow relative paths (not external packages)
    if path.starts_with('.') {
        Some(path.to_string())
    } else {
        None
    }
}

/// Resolve a relative path from an export statement to an actual `.d.ts` file.
///
/// Tries these resolution strategies (TypeScript module resolution):
///   1. `./path.d.ts` (exact file)
///   2. `./path/index.d.ts` (directory with index)
///   3. `./path.d.ts` with `.js` → `.d.ts` extension swap
fn resolve_dts_path(
    parent_dir: &Path,
    from_path: &str,
    file_set: &std::collections::HashSet<PathBuf>,
) -> Option<PathBuf> {
    let base = parent_dir.join(from_path);

    // Try: ./path.d.ts
    let with_dts = base.with_extension("d.ts");
    if file_set.contains(&with_dts) {
        return Some(with_dts);
    }

    // Try: ./path/index.d.ts
    let with_index = base.join("index.d.ts");
    if file_set.contains(&with_index) {
        return Some(with_index);
    }

    // Try: ./path (if it already has .d.ts extension somehow)
    if file_set.contains(&base) {
        return Some(base);
    }

    // Try: strip .js extension and add .d.ts
    // (TypeScript emits `export * from './Button.js'` in some configs)
    if let Some(without_js) = from_path.strip_suffix(".js") {
        let with_dts = parent_dir.join(without_js).with_extension("d.ts");
        if file_set.contains(&with_dts) {
            return Some(with_dts);
        }
        let with_index = parent_dir.join(without_js).join("index.d.ts");
        if file_set.contains(&with_index) {
            return Some(with_index);
        }
    }

    None
}

// ─── Import path resolution ──────────────────────────────────────────────

/// Set `import_path` on symbols based on entry point provenance.
///
/// When a symbol is only reachable from a subpath entry point (e.g.,
/// `victory/index.d.ts` rather than the root `index.d.ts`), its consumer-facing
/// import path differs from the package name. For example:
/// - Root entry: `import { Button } from '@patternfly/react-core'` → import_path = None
/// - Subpath entry: `import { Chart } from '@patternfly/react-charts/victory'` →
///   import_path = Some("@patternfly/react-charts/victory")
fn set_import_paths(
    symbols: &mut [Symbol],
    file_sources: &[(PathBuf, String)],
    provenance: &std::collections::HashMap<PathBuf, Vec<PathBuf>>,
    base_dir: &Path,
) {
    use std::collections::HashMap;

    // Build a map from remapped (src/) file path → original file path,
    // so we can look up provenance for each symbol.
    let mut remap_to_original: HashMap<PathBuf, PathBuf> = HashMap::new();
    for (file_path, _) in file_sources {
        let relative = file_path.strip_prefix(base_dir).unwrap_or(file_path);
        let mapped = remap_dist_to_src(relative);
        remap_to_original.insert(mapped, file_path.clone());
    }

    for sym in symbols.iter_mut() {
        let original_path = match remap_to_original.get(&sym.file) {
            Some(p) => p,
            None => continue,
        };

        let entry_points = match provenance.get(original_path) {
            Some(eps) => eps,
            None => continue,
        };

        if entry_points.is_empty() {
            continue;
        }

        // Compute the subpath for each entry point this symbol is reachable from.
        // If any entry point is the root (no subpath), import_path stays None.
        let mut shortest_subpath: Option<String> = None;
        let mut is_root = false;

        for ep in entry_points {
            let ep_relative = ep.strip_prefix(base_dir).unwrap_or(ep);
            match entry_point_subpath(ep_relative) {
                None => {
                    // Reachable from root entry point — no subpath needed
                    is_root = true;
                    break;
                }
                Some(subpath) => {
                    // Track the shortest subpath (most general)
                    if shortest_subpath
                        .as_ref()
                        .is_none_or(|s| subpath.len() < s.len())
                    {
                        shortest_subpath = Some(subpath);
                    }
                }
            }
        }

        if is_root {
            // Reachable from root — import_path is same as package (leave as None)
            continue;
        }

        // Symbol is only reachable from subpath entry point(s).
        // Set import_path = "<package>/<subpath>"
        if let (Some(ref pkg), Some(ref subpath)) = (&sym.package, &shortest_subpath) {
            sym.import_path = Some(format!("{}/{}", pkg, subpath));
            tracing::trace!(
                symbol = %sym.name,
                import_path = %sym.import_path.as_deref().unwrap_or("?"),
                "Symbol import path set from subpath entry point"
            );
        }
    }
}

/// Extract the subpath from an entry point `index.d.ts` path.
///
/// Given a relative path to an `index.d.ts` entry point file, extracts the
/// subpath between the `dist/<variant>/` (or `src/`) segment and the `index.d.ts`
/// filename.
///
/// Returns:
/// - `None` if this is the root entry point (no subpath)
/// - `Some("victory")` if the entry point is at `dist/esm/victory/index.d.ts`
///
/// Examples:
/// - `packages/react-charts/dist/esm/index.d.ts` → None (root)
/// - `packages/react-charts/dist/esm/victory/index.d.ts` → Some("victory")
/// - `packages/react-charts/src/victory/index.d.ts` → Some("victory")
/// - `packages/react-charts/dist/esm/charts/victory/index.d.ts` → Some("charts/victory")
fn entry_point_subpath(entry_point_relative: &Path) -> Option<String> {
    let path_str = entry_point_relative.to_string_lossy();

    // Known dist output directory patterns
    let dist_segments = [
        "/dist/esm/",
        "/dist/js/",
        "/dist/cjs/",
        "/dist/mjs/",
        "/dist/es/",
        "/dist/commonjs/",
        "/dist/lib/",
        "/dist/",
        "/src/",
    ];

    for segment in &dist_segments {
        if let Some(pos) = path_str.find(segment) {
            let after = &path_str[pos + segment.len()..];
            // Strip the trailing "index.d.ts" filename
            let subpath = after
                .strip_suffix("index.d.ts")
                .unwrap_or(after)
                .trim_end_matches('/');

            if subpath.is_empty() {
                return None; // Root entry point
            }
            return Some(subpath.to_string());
        }
    }

    // Fallback: no recognized structure, treat as root
    None
}

/// Build output directory priority. Lower index = higher priority.
/// When multiple `dist/<variant>/` directories exist in the same package,
/// only the highest-priority variant is kept.
const DIST_VARIANT_PRIORITY: &[&str] = &["esm", "mjs", "es", "js", "cjs", "commonjs", "lib"];

/// Identify redundant `dist/<variant>/` directories that should be excluded.
///
/// For each unique `<prefix>/dist/` parent, groups the variant subdirectories
/// (esm, js, cjs, etc.). If multiple variants exist, all except the
/// highest-priority one are marked for exclusion.
fn find_redundant_dist_dirs(files: &[PathBuf]) -> Vec<PathBuf> {
    use std::collections::{HashMap, HashSet};

    // Map: dist parent path -> set of variant names found
    // e.g., "/tmp/worktree/packages/react-core/dist" -> {"esm", "js"}
    let mut dist_variants: HashMap<PathBuf, HashSet<String>> = HashMap::new();

    for file in files {
        let components: Vec<_> = file.components().collect();
        for (i, comp) in components.iter().enumerate() {
            if comp.as_os_str() == "dist" && i + 1 < components.len() {
                let dist_parent: PathBuf = components[..=i].iter().collect();
                let variant = components[i + 1].as_os_str().to_string_lossy().to_string();
                // Only track known build output variants
                if DIST_VARIANT_PRIORITY.contains(&variant.as_str()) {
                    dist_variants
                        .entry(dist_parent)
                        .or_default()
                        .insert(variant);
                }
                break;
            }
        }
    }

    // For each dist/ parent with multiple variants, exclude all except the best
    let mut exclude_dirs = Vec::new();
    for (dist_dir, variants) in &dist_variants {
        if variants.len() <= 1 {
            continue;
        }
        // Find the highest priority variant
        if let Some(best) = DIST_VARIANT_PRIORITY
            .iter()
            .find(|p| variants.contains(**p))
        {
            for variant in variants {
                if variant != *best {
                    exclude_dirs.push(dist_dir.join(variant));
                }
            }
        }
    }

    exclude_dirs
}

fn find_dts_recursive(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("Failed to read directory {}", dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        // Skip node_modules and hidden directories
        if name == "node_modules" || name.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            find_dts_recursive(&path, files)?;
        } else if name.ends_with(".d.ts") {
            files.push(path);
        }
    }
    Ok(())
}

// ─── Line offset computation ─────────────────────────────────────────────

/// Compute byte offsets for each line start (0-indexed).
/// `line_offsets[i]` = byte offset where line `i+1` starts.
fn compute_line_offsets(source: &str) -> Vec<u32> {
    let mut offsets = vec![0u32]; // Line 1 starts at byte 0
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            offsets.push((i + 1) as u32);
        }
    }
    offsets
}

/// Convert a byte offset to a 1-indexed line number.
fn offset_to_line(offsets: &[u32], offset: u32) -> usize {
    match offsets.binary_search(&offset) {
        Ok(i) => i + 1,
        Err(i) => i, // offset is within line `i` (1-indexed)
    }
}

// ─── Source text extraction ───────────────────────────────────────────────

/// Extract source text for a span. Used for type annotations, default values, etc.
fn span_text(source: &str, span: oxc_span::Span) -> &str {
    &source[span.start as usize..span.end as usize]
}

/// Extract and canonicalize the type string from a TSTypeAnnotation.
///
/// The type is first extracted as source text, then canonicalized using the
/// 6-rule normalizer (including import resolution). If canonicalization fails
/// (malformed or unsupported syntax), the raw source text is used as-is.
fn type_annotation_str(
    source: &str,
    imports: &crate::canon::ImportMap,
    ta: &TSTypeAnnotation,
) -> String {
    let raw = span_text(source, ta.type_annotation.span());
    let import_ref = if imports.is_empty() {
        None
    } else {
        Some(imports)
    };
    crate::canon::canonicalize_type_with_imports(raw, import_ref).unwrap_or_else(|| raw.to_string())
}

/// Collect import declarations from a `.d.ts` file into an ImportMap.
///
/// Handles:
/// - `import React from 'react'`         → default import
/// - `import { ReactNode } from 'react'` → named import
/// - `import { X as Y } from 'module'`   → aliased named import
/// - `import * as React from 'react'`    → namespace import
/// - `import type { ... } from '...'`    → type imports (same handling)
/// - `/// <reference types="react" />`   → global namespace (convention: capitalized package name)
fn collect_imports(source: &str, stmts: &[Statement]) -> crate::canon::ImportMap {
    use oxc_ast::ast::ImportDeclarationSpecifier;
    let mut map = crate::canon::ImportMap::new();

    // Parse `/// <reference types="..." />` directives from source text.
    // These make global type namespaces available (e.g., `React` from `@types/react`).
    collect_reference_directives(source, &mut map);

    for stmt in stmts {
        let decl = match stmt {
            Statement::ImportDeclaration(d) => d,
            _ => continue,
        };

        let module = decl.source.value.as_str();

        if let Some(specifiers) = &decl.specifiers {
            for spec in specifiers {
                match spec {
                    ImportDeclarationSpecifier::ImportDefaultSpecifier(default) => {
                        map.add_default(&default.local.name, module);
                    }
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(ns) => {
                        map.add_namespace(&ns.local.name, module);
                    }
                    ImportDeclarationSpecifier::ImportSpecifier(named) => {
                        let original = module_export_name_str(&named.imported);
                        map.add_named(&named.local.name, &original, module);
                    }
                }
            }
        }
    }

    map
}

/// Parse `/// <reference types="..." />` directives and add them to the import map.
///
/// Convention: `/// <reference types="react" />` makes `React` available as a global
/// namespace. The namespace name is derived by capitalizing the first letter of the
/// package name (e.g., `react` → `React`, `node` → `Node`).
///
/// For scoped packages like `@types/react`, the package name after the scope is used.
fn collect_reference_directives(source: &str, map: &mut crate::canon::ImportMap) {
    // Known mappings from package name to global namespace name.
    // These are the most common packages that expose global namespaces in .d.ts files.
    let known_namespaces: &[(&str, &str)] = &[
        ("react", "React"),
        ("react-dom", "ReactDOM"),
        ("node", "NodeJS"),
    ];

    for line in source.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("///") {
            // Triple-slash directives must be at the top of the file,
            // before any statements. Stop scanning once we hit non-directive content.
            if !trimmed.is_empty() && !trimmed.starts_with("//") {
                break;
            }
            continue;
        }

        // Match: /// <reference types="PACKAGE" />
        if let Some(start) = trimmed.find("types=\"") {
            let rest = &trimmed[start + 7..];
            if let Some(end) = rest.find('"') {
                let package = &rest[..end];

                // Check known namespace mappings first
                if let Some((_, ns)) = known_namespaces.iter().find(|(pkg, _)| *pkg == package) {
                    map.add_namespace(ns, package);
                } else {
                    // Fallback: capitalize first letter of package name
                    let ns_name = capitalize_first(package);
                    if !ns_name.is_empty() {
                        map.add_namespace(&ns_name, package);
                    }
                }
            }
        }
    }
}

/// Capitalize the first letter of a string.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

// ─── Global namespace detection ──────────────────────────────────────────

/// Collect import declarations from source text without full symbol extraction.
///
/// This is used in the first pass of two-pass extraction to build a project-wide
/// import map without paying the cost of full symbol extraction.
fn collect_imports_from_source(source: &str) -> crate::canon::ImportMap {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source, SourceType::d_ts()).parse();
    collect_imports(source, &ret.program.body)
}

/// Scan `node_modules/@types/` for packages that declare global namespaces.
///
/// Detects two patterns:
///
/// 1. **`export as namespace X`** — UMD global declaration (e.g., `@types/react`
///    declares `export as namespace React`, making `React.ReactNode` available
///    globally without importing).
///
/// 2. **Top-level `declare namespace X`** — Ambient namespace in script-mode
///    files (e.g., `@types/node/globals.d.ts` declares `declare namespace NodeJS`).
///
/// Searches for `node_modules/@types/` starting from `dir` and walking up
/// parent directories (handles monorepo hoisting where types are installed
/// at the repo root but packages are in subdirectories).
fn scan_types_packages(dir: &Path) -> crate::canon::ImportMap {
    let mut map = crate::canon::ImportMap::new();

    // Walk up to find node_modules/@types/
    let types_dir = find_types_dir(dir);
    let types_dir = match types_dir {
        Some(d) => d,
        None => return map,
    };

    let entries = match std::fs::read_dir(&types_dir) {
        Ok(e) => e,
        Err(_) => return map,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let pkg_name = entry.file_name().to_string_lossy().to_string();

        // Read the entry .d.ts file (usually index.d.ts)
        let entry_file = resolve_types_entry(&path);
        let entry_file = match entry_file {
            Some(f) => f,
            None => continue,
        };

        let source = match std::fs::read_to_string(&entry_file) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // Look for `export as namespace X` — the definitive way @types packages
        // declare their global namespace name.
        if let Some(ns_name) = find_export_as_namespace(&source) {
            map.add_namespace(&ns_name, &pkg_name);
            continue;
        }

        // Fallback: look for top-level `declare namespace X` in script-mode files
        // (files with no import/export statements). These are ambient declarations.
        if !has_module_syntax(&source) {
            for ns_name in find_declare_namespaces(&source) {
                map.add_namespace(&ns_name, &pkg_name);
            }
        }
    }

    map
}

/// Find the `node_modules/@types/` directory, searching from `dir` upward.
fn find_types_dir(dir: &Path) -> Option<PathBuf> {
    let mut current = dir.to_path_buf();
    loop {
        let candidate = current.join("node_modules/@types");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Resolve the entry `.d.ts` file for an `@types` package.
///
/// Reads `package.json` for the `types` or `typings` field, falls back to `index.d.ts`.
fn resolve_types_entry(pkg_dir: &Path) -> Option<PathBuf> {
    let pkg_json = pkg_dir.join("package.json");
    if let Ok(content) = std::fs::read_to_string(&pkg_json) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            // Check "types" then "typings" fields
            for field in &["types", "typings"] {
                if let Some(entry) = json.get(field).and_then(|v| v.as_str()) {
                    if !entry.is_empty() {
                        let path = pkg_dir.join(entry);
                        if path.exists() {
                            return Some(path);
                        }
                    }
                }
            }
        }
    }

    // Fallback: index.d.ts
    let index = pkg_dir.join("index.d.ts");
    if index.exists() {
        Some(index)
    } else {
        None
    }
}

/// Find `export as namespace X` in source text.
///
/// This is the standard pattern for UMD global declarations in `@types` packages.
/// Returns the namespace name `X`.
fn find_export_as_namespace(source: &str) -> Option<String> {
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("export as namespace ") {
            // Strip trailing semicolon and whitespace
            let name = rest.trim_end_matches(';').trim();
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Check if source text contains module syntax (import/export statements).
///
/// Files without module syntax are script-mode: their top-level declarations
/// are automatically global/ambient.
fn has_module_syntax(source: &str) -> bool {
    for line in source.lines() {
        let trimmed = line.trim();
        // Skip comments and directives
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            continue;
        }
        if trimmed.starts_with("import ") || trimmed.starts_with("export ") {
            return true;
        }
    }
    false
}

/// Find top-level `declare namespace X` declarations in ambient/script-mode files.
fn find_declare_namespaces(source: &str) -> Vec<String> {
    let mut namespaces = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("declare namespace ") {
            // Extract the namespace name (stop at '{' or whitespace)
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                namespaces.push(name);
            }
        }
    }
    namespaces
}

/// Extract a qualified name for a symbol: `file_stem.name` or `file_stem.parent.name`.
fn qualified_name(file: &Path, parts: &[&str]) -> String {
    let stem = file
        .with_extension("") // removes .ts
        .with_extension("") // removes .d (from .d.ts)
        .to_string_lossy()
        .replace('\\', "/");
    let mut qn = stem;
    for part in parts {
        qn.push('.');
        qn.push_str(part);
    }
    qn
}

// ─── Statement-level extraction ──────────────────────────────────────────

fn extract_statement(
    source: &str,
    stmt: &Statement,
    file: &Path,
    line_offsets: &[u32],
    imports: &crate::canon::ImportMap,
    symbols: &mut Vec<Symbol>,
) {
    match stmt {
        Statement::ExportNamedDeclaration(export) => {
            // Named export with declaration: `export declare function ...`
            if let Some(decl) = &export.declaration {
                extract_declaration(
                    source,
                    imports,
                    decl,
                    file,
                    line_offsets,
                    Visibility::Exported,
                    symbols,
                );
            }

            // Re-export specifiers: `export { Foo, Bar as Baz } from './other'`
            for spec in &export.specifiers {
                let local_name = module_export_name_str(&spec.local);
                let exported_name = module_export_name_str(&spec.exported);
                let source_module = export.source.as_ref().map(|s| s.value.to_string());
                let line = offset_to_line(line_offsets, spec.span.start);

                let mut sym = Symbol::new(
                    exported_name.clone(),
                    qualified_name(file, &[&exported_name]),
                    SymbolKind::Variable, // We don't know the kind until resolved
                    Visibility::Exported,
                    file,
                    line,
                );
                // Store re-export metadata in type_dependencies for now.
                // When we add oxc_resolver, we'll resolve these to actual symbols.
                if let Some(src) = source_module {
                    sym.type_dependencies
                        .push(format!("reexport:{}:{}", src, local_name));
                }
                symbols.push(sym);
            }
        }

        Statement::ExportDefaultDeclaration(export) => {
            extract_default_export(source, imports, export, file, line_offsets, symbols);
        }

        Statement::ExportAllDeclaration(export) => {
            // `export * from './module'` or `export * as ns from './module'`
            let line = offset_to_line(line_offsets, export.span.start);
            let source_module = export.source.value.to_string();

            if let Some(exported) = &export.exported {
                // `export * as ns from './module'`
                let name = module_export_name_str(exported);
                let mut sym = Symbol::new(
                    name.clone(),
                    qualified_name(file, &[&name]),
                    SymbolKind::Namespace,
                    Visibility::Exported,
                    file,
                    line,
                );
                sym.type_dependencies
                    .push(format!("reexport-all-as:{}", source_module));
                symbols.push(sym);
            } else {
                // `export * from './module'`
                let mut sym = Symbol::new(
                    "*".to_string(),
                    qualified_name(file, &["*"]),
                    SymbolKind::Namespace,
                    Visibility::Exported,
                    file,
                    line,
                );
                sym.type_dependencies
                    .push(format!("reexport-all:{}", source_module));
                symbols.push(sym);
            }
        }

        _ => {} // Non-export statements are not part of the public API surface
    }
}

// ─── Declaration-level extraction ────────────────────────────────────────

fn extract_declaration(
    source: &str,
    imports: &crate::canon::ImportMap,
    decl: &Declaration,
    file: &Path,
    line_offsets: &[u32],
    visibility: Visibility,
    symbols: &mut Vec<Symbol>,
) {
    match decl {
        Declaration::FunctionDeclaration(func) => {
            if let Some(sym) =
                extract_function(source, imports, func, file, line_offsets, visibility)
            {
                symbols.push(sym);
            }
        }
        Declaration::ClassDeclaration(cls) => {
            if let Some(sym) = extract_class(source, imports, cls, file, line_offsets, visibility) {
                symbols.push(sym);
            }
        }
        Declaration::TSInterfaceDeclaration(iface) => {
            symbols.push(extract_interface(
                source,
                imports,
                iface,
                file,
                line_offsets,
                visibility,
            ));
        }
        Declaration::TSTypeAliasDeclaration(alias) => {
            symbols.push(extract_type_alias(
                source,
                imports,
                alias,
                file,
                line_offsets,
                visibility,
            ));
        }
        Declaration::TSEnumDeclaration(enum_decl) => {
            symbols.push(extract_enum(
                source,
                imports,
                enum_decl,
                file,
                line_offsets,
                visibility,
            ));
        }
        Declaration::VariableDeclaration(var) => {
            extract_variables(
                source,
                imports,
                var,
                file,
                line_offsets,
                visibility,
                symbols,
            );
        }
        Declaration::TSModuleDeclaration(ns) => {
            extract_namespace(source, imports, ns, file, line_offsets, visibility, symbols);
        }
        _ => {} // TSImportEqualsDeclaration, etc.
    }
}

fn extract_default_export(
    source: &str,
    imports: &crate::canon::ImportMap,
    export: &ExportDefaultDeclaration,
    file: &Path,
    line_offsets: &[u32],
    symbols: &mut Vec<Symbol>,
) {
    let line = offset_to_line(line_offsets, export.span.start);

    match &export.declaration {
        ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
            let name = func
                .id
                .as_ref()
                .map(|id| id.name.to_string())
                .unwrap_or_else(|| "default".to_string());
            let mut sym = Symbol::new(
                name.clone(),
                qualified_name(file, &[&name]),
                SymbolKind::Function,
                Visibility::Exported,
                file,
                line,
            );
            sym.signature = Some(extract_signature(source, imports, func));
            sym.type_dependencies = collect_function_type_deps(source, func);
            symbols.push(sym);
        }
        ExportDefaultDeclarationKind::ClassDeclaration(cls) => {
            if let Some(mut sym) = extract_class(
                source,
                imports,
                cls,
                file,
                line_offsets,
                Visibility::Exported,
            ) {
                if sym.name == "<anonymous>" {
                    sym.name = "default".to_string();
                    sym.qualified_name = qualified_name(file, &["default"]);
                }
                symbols.push(sym);
            }
        }
        ExportDefaultDeclarationKind::TSInterfaceDeclaration(iface) => {
            symbols.push(extract_interface(
                source,
                imports,
                iface,
                file,
                line_offsets,
                Visibility::Exported,
            ));
        }
        _ => {
            // Expression exports (rare in .d.ts): `export default expr`
            let sym = Symbol::new(
                "default",
                qualified_name(file, &["default"]),
                SymbolKind::Variable,
                Visibility::Exported,
                file,
                line,
            );
            symbols.push(sym);
        }
    }
}

// ─── Function extraction ─────────────────────────────────────────────────

fn extract_function(
    source: &str,
    imports: &crate::canon::ImportMap,
    func: &Function,
    file: &Path,
    line_offsets: &[u32],
    visibility: Visibility,
) -> Option<Symbol> {
    let name = func.id.as_ref()?.name.to_string();
    let line = offset_to_line(line_offsets, func.span.start);

    let mut sym = Symbol::new(
        name.clone(),
        qualified_name(file, &[&name]),
        SymbolKind::Function,
        visibility,
        file,
        line,
    );
    sym.signature = Some(extract_signature(source, imports, func));
    sym.type_dependencies = collect_function_type_deps(source, func);

    Some(sym)
}

fn extract_signature(
    source: &str,
    imports: &crate::canon::ImportMap,
    func: &Function,
) -> Signature {
    let parameters = extract_params(source, imports, &func.params);
    let return_type = func
        .return_type
        .as_ref()
        .map(|ta| type_annotation_str(source, imports, ta));
    let type_parameters = func
        .type_parameters
        .as_ref()
        .map(|tp| extract_type_parameters(source, tp))
        .unwrap_or_default();

    Signature {
        parameters,
        return_type,
        type_parameters,
        is_async: func.r#async,
    }
}

// ─── Class extraction ────────────────────────────────────────────────────

fn extract_class(
    source: &str,
    imports: &crate::canon::ImportMap,
    cls: &Class,
    file: &Path,
    line_offsets: &[u32],
    visibility: Visibility,
) -> Option<Symbol> {
    let name = cls
        .id
        .as_ref()
        .map(|id| id.name.to_string())
        .unwrap_or_else(|| "<anonymous>".to_string());
    let line = offset_to_line(line_offsets, cls.span.start);

    let mut sym = Symbol::new(
        name.clone(),
        qualified_name(file, &[&name]),
        SymbolKind::Class,
        visibility,
        file,
        line,
    );

    // extends clause
    if let Some(super_class) = &cls.super_class {
        sym.extends = Some(span_text(source, super_class.span()).to_string());
    }

    // implements clause
    sym.implements = cls
        .implements
        .iter()
        .map(|imp| {
            // TSClassImplements.expression is a TSTypeName (IdentifierReference or TSQualifiedName)
            span_text(source, imp.expression.span()).to_string()
        })
        .collect();

    sym.is_abstract = cls.r#abstract;

    // Type parameters
    if let Some(tp) = &cls.type_parameters {
        // Store as type dependencies for the class itself
        for param in &tp.params {
            if let Some(constraint) = &param.constraint {
                collect_type_deps_from_ts_type(source, constraint, &mut sym.type_dependencies);
            }
        }
    }

    // Class members
    for element in &cls.body.body {
        if let Some(member) =
            extract_class_element(source, imports, element, file, &name, line_offsets)
        {
            sym.members.push(member);
        }
    }

    Some(sym)
}

fn extract_class_element(
    source: &str,
    imports: &crate::canon::ImportMap,
    element: &ClassElement,
    file: &Path,
    class_name: &str,
    line_offsets: &[u32],
) -> Option<Symbol> {
    match element {
        ClassElement::MethodDefinition(method) => {
            extract_method_definition(source, imports, method, file, class_name, line_offsets)
        }
        ClassElement::PropertyDefinition(prop) => {
            extract_property_definition(source, imports, prop, file, class_name, line_offsets)
        }
        ClassElement::AccessorProperty(prop) => {
            extract_accessor_property(source, imports, prop, file, class_name, line_offsets)
        }
        ClassElement::TSIndexSignature(idx) => {
            let line = offset_to_line(line_offsets, idx.span.start);
            let mut sym = Symbol::new(
                "[index]",
                qualified_name(file, &[class_name, "[index]"]),
                SymbolKind::Property,
                ts_accessibility_to_visibility(None),
                file,
                line,
            );
            sym.is_readonly = idx.readonly;
            sym.type_dependencies
                .extend(collect_type_deps_from_annotation(
                    source,
                    &idx.type_annotation,
                ));
            Some(sym)
        }
        ClassElement::StaticBlock(_) => None, // Not relevant for .d.ts
    }
}

fn extract_method_definition(
    source: &str,
    imports: &crate::canon::ImportMap,
    method: &MethodDefinition,
    file: &Path,
    class_name: &str,
    line_offsets: &[u32],
) -> Option<Symbol> {
    let name = property_key_name(&method.key)?;
    let line = offset_to_line(line_offsets, method.span.start);

    // Skip private members (not part of public API)
    if matches!(method.accessibility, Some(TSAccessibility::Private)) {
        return None;
    }

    let (kind, accessor_kind) = match method.kind {
        MethodDefinitionKind::Constructor => (SymbolKind::Constructor, None),
        MethodDefinitionKind::Method => (SymbolKind::Method, None),
        MethodDefinitionKind::Get => (SymbolKind::GetAccessor, Some(AccessorKind::Get)),
        MethodDefinitionKind::Set => (SymbolKind::SetAccessor, Some(AccessorKind::Set)),
    };

    let visibility = ts_accessibility_to_visibility(method.accessibility);
    let is_abstract = matches!(
        method.r#type,
        MethodDefinitionType::TSAbstractMethodDefinition
    );

    let mut sym = Symbol::new(
        name.clone(),
        qualified_name(file, &[class_name, &name]),
        kind,
        visibility,
        file,
        line,
    );
    sym.is_abstract = is_abstract;
    sym.is_static = method.r#static;
    sym.accessor_kind = accessor_kind;
    sym.signature = Some(extract_signature(source, imports, &method.value));
    sym.type_dependencies = collect_function_type_deps(source, &method.value);

    Some(sym)
}

fn extract_property_definition(
    source: &str,
    imports: &crate::canon::ImportMap,
    prop: &PropertyDefinition,
    file: &Path,
    class_name: &str,
    line_offsets: &[u32],
) -> Option<Symbol> {
    let name = property_key_name(&prop.key)?;
    let line = offset_to_line(line_offsets, prop.span.start);

    // Skip private members
    if matches!(prop.accessibility, Some(TSAccessibility::Private)) {
        return None;
    }

    let visibility = ts_accessibility_to_visibility(prop.accessibility);

    let mut sym = Symbol::new(
        name.clone(),
        qualified_name(file, &[class_name, &name]),
        SymbolKind::Property,
        visibility,
        file,
        line,
    );
    sym.is_readonly = prop.readonly;
    sym.is_static = prop.r#static;
    sym.is_abstract = matches!(
        prop.r#type,
        PropertyDefinitionType::TSAbstractPropertyDefinition
    );

    // Type annotation → type_dependencies
    if let Some(ta) = &prop.type_annotation {
        sym.type_dependencies = collect_type_deps_from_annotation(source, ta);
    }

    // Store the type annotation string in the signature's return_type field
    // for uniform access during diffing (properties are "read" via their type).
    if let Some(ta) = &prop.type_annotation {
        sym.signature = Some(Signature {
            parameters: Vec::new(),
            return_type: Some(type_annotation_str(source, imports, ta)),
            type_parameters: Vec::new(),
            is_async: false,
        });
    }

    Some(sym)
}

fn extract_accessor_property(
    source: &str,
    imports: &crate::canon::ImportMap,
    prop: &AccessorProperty,
    file: &Path,
    class_name: &str,
    line_offsets: &[u32],
) -> Option<Symbol> {
    let name = property_key_name(&prop.key)?;
    let line = offset_to_line(line_offsets, prop.span.start);

    let visibility = ts_accessibility_to_visibility(prop.accessibility);

    let mut sym = Symbol::new(
        name.clone(),
        qualified_name(file, &[class_name, &name]),
        SymbolKind::Property,
        visibility,
        file,
        line,
    );
    sym.is_static = prop.r#static;
    sym.accessor_kind = Some(AccessorKind::Get); // auto-accessor

    if let Some(ta) = &prop.type_annotation {
        sym.type_dependencies = collect_type_deps_from_annotation(source, ta);
        sym.signature = Some(Signature {
            parameters: Vec::new(),
            return_type: Some(type_annotation_str(source, imports, ta)),
            type_parameters: Vec::new(),
            is_async: false,
        });
    }

    Some(sym)
}

// ─── Interface extraction ────────────────────────────────────────────────

fn extract_interface(
    source: &str,
    imports: &crate::canon::ImportMap,
    iface: &TSInterfaceDeclaration,
    file: &Path,
    line_offsets: &[u32],
    visibility: Visibility,
) -> Symbol {
    let name = iface.id.name.to_string();
    let line = offset_to_line(line_offsets, iface.span.start);

    let mut sym = Symbol::new(
        name.clone(),
        qualified_name(file, &[&name]),
        SymbolKind::Interface,
        visibility,
        file,
        line,
    );

    // extends (Vec, not Option)
    if !iface.extends.is_empty() {
        // Use first extends as `extends` field, rest go into implements
        // (interfaces can extend multiple interfaces).
        // Use the full span (expression + type arguments) to capture
        // generic wrappers like `Omit<React.HTMLProps<HTMLElement>, 'ref'>`.
        let names: Vec<String> = iface
            .extends
            .iter()
            .map(|ext| span_text(source, ext.span).to_string())
            .collect();
        if let Some(first) = names.first() {
            sym.extends = Some(first.clone());
        }
        if names.len() > 1 {
            sym.implements = names[1..].to_vec();
        }
    }

    // Type parameters → type_dependencies
    if let Some(tp) = &iface.type_parameters {
        for param in &tp.params {
            if let Some(constraint) = &param.constraint {
                collect_type_deps_from_ts_type(source, constraint, &mut sym.type_dependencies);
            }
        }
    }

    // Interface members
    for member in &iface.body.body {
        if let Some(member_sym) =
            extract_ts_signature(source, imports, member, file, &name, line_offsets)
        {
            sym.members.push(member_sym);
        }
    }

    sym
}

fn extract_ts_signature(
    source: &str,
    imports: &crate::canon::ImportMap,
    sig: &TSSignature,
    file: &Path,
    parent_name: &str,
    line_offsets: &[u32],
) -> Option<Symbol> {
    match sig {
        TSSignature::TSPropertySignature(prop) => {
            let name = property_key_name(&prop.key)?;
            let line = offset_to_line(line_offsets, prop.span.start);

            let mut sym = Symbol::new(
                name.clone(),
                qualified_name(file, &[parent_name, &name]),
                SymbolKind::Property,
                Visibility::Public,
                file,
                line,
            );
            sym.is_readonly = prop.readonly;

            if let Some(ta) = &prop.type_annotation {
                sym.type_dependencies = collect_type_deps_from_annotation(source, ta);
                sym.signature = Some(Signature {
                    parameters: Vec::new(),
                    return_type: Some(type_annotation_str(source, imports, ta)),
                    type_parameters: Vec::new(),
                    is_async: false,
                });
            }

            // Mark optional properties
            // Note: we store optionality separately; the Parameter.optional
            // field is for function params. For interface properties, we use
            // the signature to capture the type, and the caller can check
            // whether it was optional from the original .d.ts.

            Some(sym)
        }

        TSSignature::TSMethodSignature(method) => {
            let name = property_key_name(&method.key)?;
            let line = offset_to_line(line_offsets, method.span.start);

            let kind = match method.kind {
                TSMethodSignatureKind::Method => SymbolKind::Method,
                TSMethodSignatureKind::Get => SymbolKind::GetAccessor,
                TSMethodSignatureKind::Set => SymbolKind::SetAccessor,
            };

            let mut sym = Symbol::new(
                name.clone(),
                qualified_name(file, &[parent_name, &name]),
                kind,
                Visibility::Public,
                file,
                line,
            );

            // Build signature
            let parameters = extract_params(source, imports, &method.params);
            let return_type = method
                .return_type
                .as_ref()
                .map(|ta| type_annotation_str(source, imports, ta));
            let type_parameters = method
                .type_parameters
                .as_ref()
                .map(|tp| extract_type_parameters(source, tp))
                .unwrap_or_default();

            sym.signature = Some(Signature {
                parameters,
                return_type,
                type_parameters,
                is_async: false,
            });

            // Type dependencies from params and return type
            let mut deps = Vec::new();
            for param in &method.params.items {
                if let Some(ta) = &param.type_annotation {
                    collect_type_deps_from_ts_type(source, &ta.type_annotation, &mut deps);
                }
            }
            if let Some(ta) = &method.return_type {
                collect_type_deps_from_ts_type(source, &ta.type_annotation, &mut deps);
            }
            deps.sort();
            deps.dedup();
            sym.type_dependencies = deps;

            Some(sym)
        }

        TSSignature::TSIndexSignature(idx) => {
            let line = offset_to_line(line_offsets, idx.span.start);
            let mut sym = Symbol::new(
                "[index]",
                qualified_name(file, &[parent_name, "[index]"]),
                SymbolKind::Property,
                Visibility::Public,
                file,
                line,
            );
            sym.is_readonly = idx.readonly;
            sym.type_dependencies = collect_type_deps_from_annotation(source, &idx.type_annotation);
            Some(sym)
        }

        TSSignature::TSCallSignatureDeclaration(call) => {
            let line = offset_to_line(line_offsets, call.span.start);
            let parameters = extract_params(source, imports, &call.params);
            let return_type = call
                .return_type
                .as_ref()
                .map(|ta| type_annotation_str(source, imports, ta));
            let type_parameters = call
                .type_parameters
                .as_ref()
                .map(|tp| extract_type_parameters(source, tp))
                .unwrap_or_default();

            let mut sym = Symbol::new(
                "(call)",
                qualified_name(file, &[parent_name, "(call)"]),
                SymbolKind::Method,
                Visibility::Public,
                file,
                line,
            );
            sym.signature = Some(Signature {
                parameters,
                return_type,
                type_parameters,
                is_async: false,
            });
            Some(sym)
        }

        TSSignature::TSConstructSignatureDeclaration(ctor) => {
            let line = offset_to_line(line_offsets, ctor.span.start);
            let parameters = extract_params(source, imports, &ctor.params);
            let return_type = ctor
                .return_type
                .as_ref()
                .map(|ta| type_annotation_str(source, imports, ta));
            let type_parameters = ctor
                .type_parameters
                .as_ref()
                .map(|tp| extract_type_parameters(source, tp))
                .unwrap_or_default();

            let mut sym = Symbol::new(
                "new",
                qualified_name(file, &[parent_name, "new"]),
                SymbolKind::Constructor,
                Visibility::Public,
                file,
                line,
            );
            sym.signature = Some(Signature {
                parameters,
                return_type,
                type_parameters,
                is_async: false,
            });
            Some(sym)
        }
    }
}

// ─── Type alias extraction ───────────────────────────────────────────────

fn extract_type_alias(
    source: &str,
    _imports: &crate::canon::ImportMap,
    alias: &TSTypeAliasDeclaration,
    file: &Path,
    line_offsets: &[u32],
    visibility: Visibility,
) -> Symbol {
    let name = alias.id.name.to_string();
    let line = offset_to_line(line_offsets, alias.span.start);

    let mut sym = Symbol::new(
        name.clone(),
        qualified_name(file, &[&name]),
        SymbolKind::TypeAlias,
        visibility,
        file,
        line,
    );

    // Store the type value as the "return type" in the signature for diffing
    let type_str = span_text(source, alias.type_annotation.span()).to_string();
    sym.signature = Some(Signature {
        parameters: Vec::new(),
        return_type: Some(type_str),
        type_parameters: alias
            .type_parameters
            .as_ref()
            .map(|tp| extract_type_parameters(source, tp))
            .unwrap_or_default(),
        is_async: false,
    });

    // Collect type dependencies from the type definition
    collect_type_deps_from_ts_type(source, &alias.type_annotation, &mut sym.type_dependencies);
    // Also collect from type parameter constraints and defaults
    if let Some(tp) = &alias.type_parameters {
        for param in &tp.params {
            if let Some(constraint) = &param.constraint {
                collect_type_deps_from_ts_type(source, constraint, &mut sym.type_dependencies);
            }
            if let Some(default) = &param.default {
                collect_type_deps_from_ts_type(source, default, &mut sym.type_dependencies);
            }
        }
    }
    sym.type_dependencies.sort();
    sym.type_dependencies.dedup();

    sym
}

// ─── Enum extraction ─────────────────────────────────────────────────────

fn extract_enum(
    source: &str,
    _imports: &crate::canon::ImportMap,
    enum_decl: &TSEnumDeclaration,
    file: &Path,
    line_offsets: &[u32],
    visibility: Visibility,
) -> Symbol {
    let name = enum_decl.id.name.to_string();
    let line = offset_to_line(line_offsets, enum_decl.span.start);

    let mut sym = Symbol::new(
        name.clone(),
        qualified_name(file, &[&name]),
        SymbolKind::Enum,
        visibility,
        file,
        line,
    );

    // Extract enum members
    for member in &enum_decl.body.members {
        let member_name = enum_member_name(&member.id);
        let member_line = offset_to_line(line_offsets, member.span.start);

        let mut member_sym = Symbol::new(
            member_name.clone(),
            qualified_name(file, &[&name, &member_name]),
            SymbolKind::EnumMember,
            Visibility::Public,
            file,
            member_line,
        );

        // Store initializer value if present
        if let Some(init) = &member.initializer {
            member_sym.signature = Some(Signature {
                parameters: Vec::new(),
                return_type: Some(span_text(source, init.span()).to_string()),
                type_parameters: Vec::new(),
                is_async: false,
            });
        }

        sym.members.push(member_sym);
    }

    sym
}

fn enum_member_name(name: &TSEnumMemberName) -> String {
    match name {
        TSEnumMemberName::Identifier(id) => id.name.to_string(),
        TSEnumMemberName::String(s) => s.value.to_string(),
        TSEnumMemberName::ComputedString(s) => s.value.to_string(),
        TSEnumMemberName::ComputedTemplateString(t) => {
            // Template literal as enum member name — rare, use quasis
            t.quasis
                .first()
                .map(|q| q.value.raw.to_string())
                .unwrap_or_else(|| "<template>".to_string())
        }
    }
}

// ─── Variable/constant extraction ────────────────────────────────────────

fn extract_variables(
    source: &str,
    imports: &crate::canon::ImportMap,
    var: &VariableDeclaration,
    file: &Path,
    line_offsets: &[u32],
    visibility: Visibility,
    symbols: &mut Vec<Symbol>,
) {
    let kind = match var.kind {
        VariableDeclarationKind::Const => SymbolKind::Constant,
        _ => SymbolKind::Variable,
    };

    for declarator in &var.declarations {
        match &declarator.id {
            BindingPattern::BindingIdentifier(id) => {
                let name = id.name.to_string();
                let line = offset_to_line(line_offsets, declarator.span.start);

                let mut sym = Symbol::new(
                    name.clone(),
                    qualified_name(file, &[&name]),
                    kind,
                    visibility,
                    file,
                    line,
                );
                sym.is_readonly = matches!(var.kind, VariableDeclarationKind::Const);

                if let Some(ta) = &declarator.type_annotation {
                    sym.type_dependencies = collect_type_deps_from_annotation(source, ta);
                    sym.signature = Some(Signature {
                        parameters: Vec::new(),
                        return_type: Some(type_annotation_str(source, imports, ta)),
                        type_parameters: Vec::new(),
                        is_async: false,
                    });
                }

                symbols.push(sym);
            }
            _ => {
                // Destructuring patterns in .d.ts are rare; skip for now
            }
        }
    }
}

// ─── Namespace (module) extraction ───────────────────────────────────────

fn extract_namespace(
    source: &str,
    imports: &crate::canon::ImportMap,
    ns: &TSModuleDeclaration,
    file: &Path,
    line_offsets: &[u32],
    visibility: Visibility,
    symbols: &mut Vec<Symbol>,
) {
    let name = match &ns.id {
        TSModuleDeclarationName::Identifier(id) => id.name.to_string(),
        TSModuleDeclarationName::StringLiteral(s) => s.value.to_string(),
    };
    let line = offset_to_line(line_offsets, ns.span.start);

    let mut sym = Symbol::new(
        name.clone(),
        qualified_name(file, &[&name]),
        SymbolKind::Namespace,
        visibility,
        file,
        line,
    );

    // Extract namespace body members
    if let Some(body) = &ns.body {
        match body {
            TSModuleDeclarationBody::TSModuleBlock(block) => {
                for stmt in &block.body {
                    extract_namespace_statement(
                        source,
                        imports,
                        stmt,
                        file,
                        &name,
                        line_offsets,
                        &mut sym.members,
                    );
                }
            }
            TSModuleDeclarationBody::TSModuleDeclaration(inner_ns) => {
                // Nested namespace: `namespace A.B { ... }`
                let mut inner_symbols = Vec::new();
                extract_namespace(
                    source,
                    imports,
                    inner_ns,
                    file,
                    line_offsets,
                    visibility,
                    &mut inner_symbols,
                );
                for s in inner_symbols {
                    sym.members.push(s);
                }
            }
        }
    }

    symbols.push(sym);
}

/// Extract declarations from a namespace body statement.
/// In `declare namespace` blocks, members are declared without `export`.
fn extract_namespace_statement(
    source: &str,
    imports: &crate::canon::ImportMap,
    stmt: &Statement,
    file: &Path,
    ns_name: &str,
    line_offsets: &[u32],
    members: &mut Vec<Symbol>,
) {
    match stmt {
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                let mut ns_symbols = Vec::new();
                extract_declaration(
                    source,
                    imports,
                    decl,
                    file,
                    line_offsets,
                    Visibility::Exported,
                    &mut ns_symbols,
                );
                for mut s in ns_symbols {
                    s.qualified_name = qualified_name(file, &[ns_name, &s.name]);
                    members.push(s);
                }
            }
        }
        // In `declare namespace`, bare declarations (without export) are still public
        Statement::FunctionDeclaration(func) => {
            if let Some(mut sym) = extract_function(
                source,
                imports,
                func,
                file,
                line_offsets,
                Visibility::Public,
            ) {
                sym.qualified_name = qualified_name(file, &[ns_name, &sym.name]);
                members.push(sym);
            }
        }
        Statement::ClassDeclaration(cls) => {
            if let Some(mut sym) =
                extract_class(source, imports, cls, file, line_offsets, Visibility::Public)
            {
                sym.qualified_name = qualified_name(file, &[ns_name, &sym.name]);
                members.push(sym);
            }
        }
        Statement::TSInterfaceDeclaration(iface) => {
            let mut sym = extract_interface(
                source,
                imports,
                iface,
                file,
                line_offsets,
                Visibility::Public,
            );
            sym.qualified_name = qualified_name(file, &[ns_name, &sym.name]);
            members.push(sym);
        }
        Statement::TSTypeAliasDeclaration(alias) => {
            let mut sym = extract_type_alias(
                source,
                imports,
                alias,
                file,
                line_offsets,
                Visibility::Public,
            );
            sym.qualified_name = qualified_name(file, &[ns_name, &sym.name]);
            members.push(sym);
        }
        Statement::TSEnumDeclaration(enum_decl) => {
            let mut sym = extract_enum(
                source,
                imports,
                enum_decl,
                file,
                line_offsets,
                Visibility::Public,
            );
            sym.qualified_name = qualified_name(file, &[ns_name, &sym.name]);
            members.push(sym);
        }
        Statement::VariableDeclaration(var) => {
            let mut var_symbols = Vec::new();
            extract_variables(
                source,
                imports,
                var,
                file,
                line_offsets,
                Visibility::Public,
                &mut var_symbols,
            );
            for mut s in var_symbols {
                s.qualified_name = qualified_name(file, &[ns_name, &s.name]);
                members.push(s);
            }
        }
        Statement::TSModuleDeclaration(inner_ns) => {
            let mut ns_symbols = Vec::new();
            extract_namespace(
                source,
                imports,
                inner_ns,
                file,
                line_offsets,
                Visibility::Public,
                &mut ns_symbols,
            );
            for s in ns_symbols {
                members.push(s);
            }
        }
        _ => {}
    }
}

// ─── Parameter extraction ────────────────────────────────────────────────

fn extract_params(
    source: &str,
    imports: &crate::canon::ImportMap,
    params: &FormalParameters,
) -> Vec<Parameter> {
    let mut result: Vec<Parameter> = params
        .items
        .iter()
        .map(|p| extract_single_param(source, imports, p))
        .collect();

    // Rest parameter
    if let Some(rest) = &params.rest {
        let name = binding_rest_name(&rest.rest);
        let type_annotation = rest
            .type_annotation
            .as_ref()
            .map(|ta| type_annotation_str(source, imports, ta));

        result.push(Parameter {
            name,
            type_annotation,
            optional: false,
            has_default: false,
            default_value: None,
            is_variadic: true,
        });
    }

    result
}

fn extract_single_param(
    source: &str,
    imports: &crate::canon::ImportMap,
    param: &FormalParameter,
) -> Parameter {
    let name = binding_pattern_name(&param.pattern);
    let type_annotation = param
        .type_annotation
        .as_ref()
        .map(|ta| type_annotation_str(source, imports, ta));
    let has_default = param.initializer.is_some();
    let default_value = param
        .initializer
        .as_ref()
        .map(|init| span_text(source, init.span()).to_string());

    Parameter {
        name,
        type_annotation,
        optional: param.optional || has_default,
        has_default,
        default_value,
        is_variadic: false,
    }
}

fn binding_pattern_name(pattern: &BindingPattern) -> String {
    match pattern {
        BindingPattern::BindingIdentifier(id) => id.name.to_string(),
        BindingPattern::ObjectPattern(_) => "<destructured>".to_string(),
        BindingPattern::ArrayPattern(_) => "<destructured>".to_string(),
        BindingPattern::AssignmentPattern(assign) => binding_pattern_name(&assign.left),
    }
}

fn binding_rest_name(rest: &BindingRestElement) -> String {
    binding_pattern_name(&rest.argument)
}

// ─── Type parameter extraction ───────────────────────────────────────────

fn extract_type_parameters(source: &str, tp: &TSTypeParameterDeclaration) -> Vec<TypeParameter> {
    tp.params
        .iter()
        .map(|p| TypeParameter {
            name: p.name.to_string(),
            constraint: p
                .constraint
                .as_ref()
                .map(|c| span_text(source, c.span()).to_string()),
            default: p
                .default
                .as_ref()
                .map(|d| span_text(source, d.span()).to_string()),
        })
        .collect()
}

// ─── Type dependency collection ──────────────────────────────────────────

/// Collect all type dependencies from a function's signature.
fn collect_function_type_deps(source: &str, func: &Function) -> Vec<String> {
    let mut deps = Vec::new();

    // Parameter types
    for param in &func.params.items {
        if let Some(ta) = &param.type_annotation {
            collect_type_deps_from_ts_type(source, &ta.type_annotation, &mut deps);
        }
    }

    // Rest parameter type
    if let Some(rest) = &func.params.rest {
        if let Some(ta) = &rest.type_annotation {
            collect_type_deps_from_ts_type(source, &ta.type_annotation, &mut deps);
        }
    }

    // Return type
    if let Some(ta) = &func.return_type {
        collect_type_deps_from_ts_type(source, &ta.type_annotation, &mut deps);
    }

    // Type parameter constraints and defaults
    if let Some(tp) = &func.type_parameters {
        for param in &tp.params {
            if let Some(constraint) = &param.constraint {
                collect_type_deps_from_ts_type(source, constraint, &mut deps);
            }
            if let Some(default) = &param.default {
                collect_type_deps_from_ts_type(source, default, &mut deps);
            }
        }
    }

    deps.sort();
    deps.dedup();
    deps
}

/// Collect type dependencies from a type annotation.
fn collect_type_deps_from_annotation(source: &str, ta: &TSTypeAnnotation) -> Vec<String> {
    let mut deps = Vec::new();
    collect_type_deps_from_ts_type(source, &ta.type_annotation, &mut deps);
    deps.sort();
    deps.dedup();
    deps
}

/// Recursively walk a TSType and collect all referenced type names.
///
/// This finds all `TSTypeReference` nodes, which represent user-defined types
/// like `User`, `Promise<T>`, `Map<K, V>`, etc. Built-in keywords like
/// `string`, `number`, `boolean` are not collected.
fn collect_type_deps_from_ts_type(source: &str, ts_type: &TSType, deps: &mut Vec<String>) {
    match ts_type {
        TSType::TSTypeReference(r) => {
            // Extract the type name (may be qualified: A.B.C)
            let name = ts_type_name_str(&r.type_name);
            deps.push(name);

            // Also collect from type arguments: Promise<User> → User
            if let Some(type_args) = &r.type_arguments {
                for arg in &type_args.params {
                    collect_type_deps_from_ts_type(source, arg, deps);
                }
            }
        }
        TSType::TSUnionType(u) => {
            for t in &u.types {
                collect_type_deps_from_ts_type(source, t, deps);
            }
        }
        TSType::TSIntersectionType(i) => {
            for t in &i.types {
                collect_type_deps_from_ts_type(source, t, deps);
            }
        }
        TSType::TSArrayType(a) => {
            collect_type_deps_from_ts_type(source, &a.element_type, deps);
        }
        TSType::TSTupleType(t) => {
            for elem in &t.element_types {
                collect_type_deps_from_tuple_element(source, elem, deps);
            }
        }
        TSType::TSTypeLiteral(lit) => {
            for member in &lit.members {
                match member {
                    TSSignature::TSPropertySignature(prop) => {
                        if let Some(ta) = &prop.type_annotation {
                            collect_type_deps_from_ts_type(source, &ta.type_annotation, deps);
                        }
                    }
                    TSSignature::TSMethodSignature(method) => {
                        for param in &method.params.items {
                            if let Some(ta) = &param.type_annotation {
                                collect_type_deps_from_ts_type(source, &ta.type_annotation, deps);
                            }
                        }
                        if let Some(ta) = &method.return_type {
                            collect_type_deps_from_ts_type(source, &ta.type_annotation, deps);
                        }
                    }
                    TSSignature::TSIndexSignature(idx) => {
                        collect_type_deps_from_ts_type(
                            source,
                            &idx.type_annotation.type_annotation,
                            deps,
                        );
                    }
                    TSSignature::TSCallSignatureDeclaration(call) => {
                        for param in &call.params.items {
                            if let Some(ta) = &param.type_annotation {
                                collect_type_deps_from_ts_type(source, &ta.type_annotation, deps);
                            }
                        }
                        if let Some(ta) = &call.return_type {
                            collect_type_deps_from_ts_type(source, &ta.type_annotation, deps);
                        }
                    }
                    TSSignature::TSConstructSignatureDeclaration(ctor) => {
                        for param in &ctor.params.items {
                            if let Some(ta) = &param.type_annotation {
                                collect_type_deps_from_ts_type(source, &ta.type_annotation, deps);
                            }
                        }
                        if let Some(ta) = &ctor.return_type {
                            collect_type_deps_from_ts_type(source, &ta.type_annotation, deps);
                        }
                    }
                }
            }
        }
        TSType::TSFunctionType(f) => {
            for param in &f.params.items {
                if let Some(ta) = &param.type_annotation {
                    collect_type_deps_from_ts_type(source, &ta.type_annotation, deps);
                }
            }
            collect_type_deps_from_ts_type(source, &f.return_type.type_annotation, deps);
        }
        TSType::TSConstructorType(c) => {
            for param in &c.params.items {
                if let Some(ta) = &param.type_annotation {
                    collect_type_deps_from_ts_type(source, &ta.type_annotation, deps);
                }
            }
            collect_type_deps_from_ts_type(source, &c.return_type.type_annotation, deps);
        }
        TSType::TSConditionalType(c) => {
            collect_type_deps_from_ts_type(source, &c.check_type, deps);
            collect_type_deps_from_ts_type(source, &c.extends_type, deps);
            collect_type_deps_from_ts_type(source, &c.true_type, deps);
            collect_type_deps_from_ts_type(source, &c.false_type, deps);
        }
        TSType::TSMappedType(m) => {
            collect_type_deps_from_ts_type(source, &m.constraint, deps);
            if let Some(ta) = &m.type_annotation {
                collect_type_deps_from_ts_type(source, ta, deps);
            }
            if let Some(name_type) = &m.name_type {
                collect_type_deps_from_ts_type(source, name_type, deps);
            }
        }
        TSType::TSIndexedAccessType(idx) => {
            collect_type_deps_from_ts_type(source, &idx.object_type, deps);
            collect_type_deps_from_ts_type(source, &idx.index_type, deps);
        }
        TSType::TSTypeOperatorType(op) => {
            collect_type_deps_from_ts_type(source, &op.type_annotation, deps);
        }
        TSType::TSParenthesizedType(p) => {
            collect_type_deps_from_ts_type(source, &p.type_annotation, deps);
        }
        TSType::TSInferType(infer) => {
            if let Some(constraint) = &infer.type_parameter.constraint {
                collect_type_deps_from_ts_type(source, constraint, deps);
            }
        }
        TSType::TSTemplateLiteralType(tpl) => {
            for t in &tpl.types {
                collect_type_deps_from_ts_type(source, t, deps);
            }
        }
        TSType::TSTypeQuery(q) => {
            match &q.expr_name {
                TSTypeQueryExprName::IdentifierReference(id) => {
                    deps.push(id.name.to_string());
                }
                TSTypeQueryExprName::QualifiedName(qn) => {
                    deps.push(span_text(source, qn.span()).to_string());
                }
                TSTypeQueryExprName::TSImportType(import) => {
                    if let Some(qualifier) = &import.qualifier {
                        deps.push(import_type_qualifier_str(source, qualifier));
                    }
                }
                TSTypeQueryExprName::ThisExpression(_) => {
                    // typeof this — no external dependency
                }
            }
        }
        TSType::TSImportType(import) => {
            if let Some(qualifier) = &import.qualifier {
                deps.push(import_type_qualifier_str(source, qualifier));
            }
            if let Some(type_args) = &import.type_arguments {
                for arg in &type_args.params {
                    collect_type_deps_from_ts_type(source, arg, deps);
                }
            }
        }
        TSType::TSTypePredicate(pred) => {
            if let Some(ta) = &pred.type_annotation {
                collect_type_deps_from_ts_type(source, &ta.type_annotation, deps);
            }
        }
        // Built-in type keywords — no dependencies
        TSType::TSAnyKeyword(_)
        | TSType::TSBigIntKeyword(_)
        | TSType::TSBooleanKeyword(_)
        | TSType::TSNeverKeyword(_)
        | TSType::TSNullKeyword(_)
        | TSType::TSNumberKeyword(_)
        | TSType::TSObjectKeyword(_)
        | TSType::TSStringKeyword(_)
        | TSType::TSSymbolKeyword(_)
        | TSType::TSUndefinedKeyword(_)
        | TSType::TSUnknownKeyword(_)
        | TSType::TSVoidKeyword(_)
        | TSType::TSIntrinsicKeyword(_)
        | TSType::TSThisType(_)
        | TSType::TSLiteralType(_) => {}

        TSType::TSNamedTupleMember(named) => {
            collect_type_deps_from_tuple_element(source, &named.element_type, deps);
        }

        // JSDoc types — rare in .d.ts output
        TSType::JSDocNullableType(jsdoc) => {
            collect_type_deps_from_ts_type(source, &jsdoc.type_annotation, deps);
        }
        TSType::JSDocNonNullableType(jsdoc) => {
            collect_type_deps_from_ts_type(source, &jsdoc.type_annotation, deps);
        }
        TSType::JSDocUnknownType(_) => {}
    }
}

/// Collect type dependencies from a tuple element.
/// TSTupleElement mirrors TSType for most variants but adds tuple-specific ones.
fn collect_type_deps_from_tuple_element(
    source: &str,
    elem: &TSTupleElement,
    deps: &mut Vec<String>,
) {
    match elem {
        TSTupleElement::TSOptionalType(opt) => {
            collect_type_deps_from_ts_type(source, &opt.type_annotation, deps);
        }
        TSTupleElement::TSRestType(rest) => {
            collect_type_deps_from_ts_type(source, &rest.type_annotation, deps);
        }
        TSTupleElement::TSNamedTupleMember(named) => {
            collect_type_deps_from_tuple_element(source, &named.element_type, deps);
        }
        // Most other TSTupleElement variants correspond to TSType variants.
        // Use the span to extract any type references.
        TSTupleElement::TSTypeReference(r) => {
            let name = ts_type_name_str(&r.type_name);
            deps.push(name);
            if let Some(type_args) = &r.type_arguments {
                for arg in &type_args.params {
                    collect_type_deps_from_ts_type(source, arg, deps);
                }
            }
        }
        TSTupleElement::TSUnionType(u) => {
            for t in &u.types {
                collect_type_deps_from_ts_type(source, t, deps);
            }
        }
        TSTupleElement::TSIntersectionType(i) => {
            for t in &i.types {
                collect_type_deps_from_ts_type(source, t, deps);
            }
        }
        TSTupleElement::TSArrayType(a) => {
            collect_type_deps_from_ts_type(source, &a.element_type, deps);
        }
        TSTupleElement::TSFunctionType(f) => {
            for param in &f.params.items {
                if let Some(ta) = &param.type_annotation {
                    collect_type_deps_from_ts_type(source, &ta.type_annotation, deps);
                }
            }
            collect_type_deps_from_ts_type(source, &f.return_type.type_annotation, deps);
        }
        // Keyword types — no dependencies
        _ => {}
    }
}

// ─── Utility functions ───────────────────────────────────────────────────

/// Extract a human-readable name from a PropertyKey.
fn property_key_name(key: &PropertyKey) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some(id.name.to_string()),
        PropertyKey::PrivateIdentifier(id) => Some(format!("#{}", id.name)),
        // Computed property names: [Symbol.iterator], etc.
        // Use the source text as the name
        _ => None,
    }
}

/// Convert TSTypeName to a string representation.
fn ts_type_name_str(name: &TSTypeName) -> String {
    match name {
        TSTypeName::IdentifierReference(id) => id.name.to_string(),
        TSTypeName::QualifiedName(qn) => {
            let left = ts_type_name_str(&qn.left);
            format!("{}.{}", left, qn.right.name)
        }
        TSTypeName::ThisExpression(_) => "this".to_string(),
    }
}

/// Convert a TSImportTypeQualifier to a string.
fn import_type_qualifier_str(source: &str, qualifier: &TSImportTypeQualifier) -> String {
    // Use span text for simplicity — covers both simple and qualified names
    span_text(source, qualifier.span()).to_string()
}

/// Convert TSAccessibility to our Visibility enum.
fn ts_accessibility_to_visibility(accessibility: Option<TSAccessibility>) -> Visibility {
    match accessibility {
        Some(TSAccessibility::Private) => Visibility::Private,
        Some(TSAccessibility::Protected) => Visibility::Internal, // treated as non-public
        Some(TSAccessibility::Public) | None => Visibility::Public,
    }
}

/// Convert a ModuleExportName to a string.
fn module_export_name_str(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::IdentifierName(id) => id.name.to_string(),
        ModuleExportName::IdentifierReference(id) => id.name.to_string(),
        ModuleExportName::StringLiteral(s) => s.value.to_string(),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn extract(source: &str) -> Vec<Symbol> {
        let extractor = OxcExtractor::new();
        extractor.extract_from_source(source, Path::new("test.d.ts"))
    }

    fn find_symbol<'a>(symbols: &'a [Symbol], name: &str) -> &'a Symbol {
        symbols.iter().find(|s| s.name == name).unwrap_or_else(|| {
            panic!(
                "Symbol '{}' not found in {:?}",
                name,
                symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        })
    }

    // ── Function extraction ──────────────────────────────────────────

    #[test]
    fn extract_simple_function() {
        let symbols = extract("export declare function greet(name: string): void;");
        assert_eq!(symbols.len(), 1);

        let sym = &symbols[0];
        assert_eq!(sym.name, "greet");
        assert_eq!(sym.kind, SymbolKind::Function);
        assert_eq!(sym.visibility, Visibility::Exported);

        let sig = sym.signature.as_ref().unwrap();
        assert_eq!(sig.parameters.len(), 1);
        assert_eq!(sig.parameters[0].name, "name");
        assert_eq!(sig.parameters[0].type_annotation.as_deref(), Some("string"));
        assert!(!sig.parameters[0].optional);
        assert_eq!(sig.return_type.as_deref(), Some("void"));
        assert!(!sig.is_async);
    }

    #[test]
    fn extract_function_with_optional_params() {
        let symbols = extract(
            "export declare function create(name: string, age?: number, active?: boolean): void;",
        );
        let sig = symbols[0].signature.as_ref().unwrap();
        assert_eq!(sig.parameters.len(), 3);
        assert!(!sig.parameters[0].optional);
        assert!(sig.parameters[1].optional);
        assert!(sig.parameters[2].optional);
    }

    #[test]
    fn extract_function_with_rest_params() {
        let symbols =
            extract("export declare function log(msg: string, ...args: unknown[]): void;");
        let sig = symbols[0].signature.as_ref().unwrap();
        assert_eq!(sig.parameters.len(), 2);
        assert!(!sig.parameters[0].is_variadic);
        assert!(sig.parameters[1].is_variadic);
        assert_eq!(sig.parameters[1].name, "args");
        assert_eq!(
            sig.parameters[1].type_annotation.as_deref(),
            Some("unknown[]")
        );
    }

    #[test]
    fn extract_async_function() {
        let symbols = extract("export declare function fetchData(url: string): Promise<Response>;");
        let sig = symbols[0].signature.as_ref().unwrap();
        assert_eq!(sig.return_type.as_deref(), Some("Promise<Response>"));
        // Note: .d.ts functions from tsc don't have the `async` keyword;
        // async is conveyed via Promise return type. The function is not
        // marked async in the declaration.
    }

    #[test]
    fn extract_generic_function() {
        let symbols = extract(
            "export declare function identity<T extends Serializable, U = unknown>(input: T, fallback: U): T | U;",
        );
        let sig = symbols[0].signature.as_ref().unwrap();
        assert_eq!(sig.type_parameters.len(), 2);

        assert_eq!(sig.type_parameters[0].name, "T");
        assert_eq!(
            sig.type_parameters[0].constraint.as_deref(),
            Some("Serializable")
        );
        assert!(sig.type_parameters[0].default.is_none());

        assert_eq!(sig.type_parameters[1].name, "U");
        assert!(sig.type_parameters[1].constraint.is_none());
        assert_eq!(sig.type_parameters[1].default.as_deref(), Some("unknown"));

        // Type dependencies
        assert!(symbols[0]
            .type_dependencies
            .contains(&"Serializable".to_string()));
    }

    #[test]
    fn extract_function_type_dependencies() {
        let symbols =
            extract("export declare function createUser(opts: UserOptions): Promise<User>;");
        let deps = &symbols[0].type_dependencies;
        assert!(deps.contains(&"UserOptions".to_string()));
        assert!(deps.contains(&"Promise".to_string()));
        assert!(deps.contains(&"User".to_string()));
    }

    // ── Class extraction ─────────────────────────────────────────────

    #[test]
    fn extract_simple_class() {
        let symbols = extract(
            r#"
export declare class UserService extends BaseService implements Serializable {
    readonly name: string;
    constructor(name: string);
    getUser(id: string): Promise<User>;
    static create(): UserService;
}
"#,
        );

        let cls = find_symbol(&symbols, "UserService");
        assert_eq!(cls.kind, SymbolKind::Class);
        assert_eq!(cls.extends.as_deref(), Some("BaseService"));
        assert_eq!(cls.implements, vec!["Serializable"]);
        assert!(!cls.is_abstract);

        // Members
        assert!(cls.members.len() >= 4); // name, constructor, getUser, create

        let name_prop = find_symbol(&cls.members, "name");
        assert_eq!(name_prop.kind, SymbolKind::Property);
        assert!(name_prop.is_readonly);

        let ctor = find_symbol(&cls.members, "constructor");
        assert_eq!(ctor.kind, SymbolKind::Constructor);

        let get_user = find_symbol(&cls.members, "getUser");
        assert_eq!(get_user.kind, SymbolKind::Method);
        assert!(!get_user.is_static);

        let create = find_symbol(&cls.members, "create");
        assert!(create.is_static);
    }

    #[test]
    fn extract_abstract_class() {
        let symbols = extract(
            r#"
export declare abstract class Validator {
    abstract validate(): boolean;
    protected helper(): void;
}
"#,
        );

        let cls = find_symbol(&symbols, "Validator");
        assert!(cls.is_abstract);

        let validate = find_symbol(&cls.members, "validate");
        assert!(validate.is_abstract);

        let helper = find_symbol(&cls.members, "helper");
        // Protected members are included with Internal visibility
        assert_eq!(helper.visibility, Visibility::Internal);
    }

    #[test]
    fn extract_class_skips_private_members() {
        let symbols = extract(
            r#"
export declare class MyClass {
    private _internal: string;
    private doStuff(): void;
    public visible: number;
}
"#,
        );

        let cls = find_symbol(&symbols, "MyClass");
        // Private members should be skipped
        assert!(cls.members.iter().all(|m| m.name != "_internal"));
        assert!(cls.members.iter().all(|m| m.name != "doStuff"));
        assert!(cls.members.iter().any(|m| m.name == "visible"));
    }

    #[test]
    fn extract_class_accessors() {
        let symbols = extract(
            r#"
export declare class Widget {
    get count(): number;
    set count(value: number);
}
"#,
        );

        let cls = find_symbol(&symbols, "Widget");
        let getters: Vec<_> = cls.members.iter().filter(|m| m.name == "count").collect();
        // Should have both getter and setter as separate symbols
        assert_eq!(getters.len(), 2);
        assert!(getters.iter().any(|m| m.kind == SymbolKind::GetAccessor));
        assert!(getters.iter().any(|m| m.kind == SymbolKind::SetAccessor));
    }

    // ── Interface extraction ─────────────────────────────────────────

    #[test]
    fn extract_interface() {
        let symbols = extract(
            r#"
export interface UserOptions {
    name: string;
    age?: number;
    readonly id: string;
    greet(msg: string): void;
}
"#,
        );

        let iface = find_symbol(&symbols, "UserOptions");
        assert_eq!(iface.kind, SymbolKind::Interface);

        let name = find_symbol(&iface.members, "name");
        assert_eq!(name.kind, SymbolKind::Property);
        assert!(!name.is_readonly);

        let id = find_symbol(&iface.members, "id");
        assert!(id.is_readonly);

        let greet = find_symbol(&iface.members, "greet");
        assert_eq!(greet.kind, SymbolKind::Method);
        let sig = greet.signature.as_ref().unwrap();
        assert_eq!(sig.parameters.len(), 1);
        assert_eq!(sig.return_type.as_deref(), Some("void"));
    }

    #[test]
    fn extract_interface_extends() {
        let symbols = extract(
            r#"
export interface Admin extends User, Permissions {
    level: number;
}
"#,
        );

        let iface = find_symbol(&symbols, "Admin");
        assert_eq!(iface.extends.as_deref(), Some("User"));
        assert_eq!(iface.implements, vec!["Permissions"]);
    }

    #[test]
    fn extract_interface_with_index_signature() {
        let symbols = extract(
            r#"
export interface Dictionary {
    [key: string]: unknown;
}
"#,
        );
        let iface = find_symbol(&symbols, "Dictionary");
        let idx = find_symbol(&iface.members, "[index]");
        assert_eq!(idx.kind, SymbolKind::Property);
    }

    // ── Type alias extraction ────────────────────────────────────────

    #[test]
    fn extract_type_alias() {
        let symbols = extract("export type UserId = string | number;");

        let alias = find_symbol(&symbols, "UserId");
        assert_eq!(alias.kind, SymbolKind::TypeAlias);
        let sig = alias.signature.as_ref().unwrap();
        assert_eq!(sig.return_type.as_deref(), Some("string | number"));
    }

    #[test]
    fn extract_generic_type_alias() {
        let symbols = extract(
            "export type Result<T, E = Error> = { ok: true; value: T } | { ok: false; error: E };",
        );
        let alias = find_symbol(&symbols, "Result");
        let sig = alias.signature.as_ref().unwrap();
        assert_eq!(sig.type_parameters.len(), 2);
        assert_eq!(sig.type_parameters[0].name, "T");
        assert_eq!(sig.type_parameters[1].name, "E");
        assert_eq!(sig.type_parameters[1].default.as_deref(), Some("Error"));

        // Type dep: Error
        assert!(alias.type_dependencies.contains(&"Error".to_string()));
    }

    // ── Enum extraction ──────────────────────────────────────────────

    #[test]
    fn extract_enum() {
        let symbols = extract(
            r#"
export declare enum Color {
    Red = 0,
    Green = 1,
    Blue = 2
}
"#,
        );

        let e = find_symbol(&symbols, "Color");
        assert_eq!(e.kind, SymbolKind::Enum);
        assert_eq!(e.members.len(), 3);

        let red = find_symbol(&e.members, "Red");
        assert_eq!(red.kind, SymbolKind::EnumMember);
        assert_eq!(
            red.signature.as_ref().unwrap().return_type.as_deref(),
            Some("0")
        );
    }

    #[test]
    fn extract_string_enum() {
        let symbols = extract(
            r#"
export declare enum Direction {
    Up = "UP",
    Down = "DOWN"
}
"#,
        );
        let e = find_symbol(&symbols, "Direction");
        let up = find_symbol(&e.members, "Up");
        assert_eq!(
            up.signature.as_ref().unwrap().return_type.as_deref(),
            Some("\"UP\"")
        );
    }

    // ── Variable/constant extraction ─────────────────────────────────

    #[test]
    fn extract_const() {
        let symbols = extract("export declare const API_VERSION: string;");
        let sym = find_symbol(&symbols, "API_VERSION");
        assert_eq!(sym.kind, SymbolKind::Constant);
        assert!(sym.is_readonly);
        assert_eq!(
            sym.signature.as_ref().unwrap().return_type.as_deref(),
            Some("string")
        );
    }

    #[test]
    fn extract_let_variable() {
        let symbols = extract("export declare let counter: number;");
        let sym = find_symbol(&symbols, "counter");
        assert_eq!(sym.kind, SymbolKind::Variable);
        assert!(!sym.is_readonly);
    }

    // ── Namespace extraction ─────────────────────────────────────────

    #[test]
    fn extract_namespace() {
        let symbols = extract(
            r#"
export declare namespace Utils {
    function helper(x: number): string;
    const VERSION: string;
}
"#,
        );

        let ns = find_symbol(&symbols, "Utils");
        assert_eq!(ns.kind, SymbolKind::Namespace);

        // Namespace members
        let helper = find_symbol(&ns.members, "helper");
        assert_eq!(helper.kind, SymbolKind::Function);

        let version = find_symbol(&ns.members, "VERSION");
        assert_eq!(version.kind, SymbolKind::Constant);
    }

    // ── Re-export extraction ─────────────────────────────────────────

    #[test]
    fn extract_named_reexports() {
        let symbols = extract("export { Foo, Bar as Baz } from './other';");
        assert_eq!(symbols.len(), 2);

        let foo = find_symbol(&symbols, "Foo");
        assert!(foo
            .type_dependencies
            .iter()
            .any(|d| d.contains("reexport:./other:Foo")));

        let baz = find_symbol(&symbols, "Baz");
        assert!(baz
            .type_dependencies
            .iter()
            .any(|d| d.contains("reexport:./other:Bar")));
    }

    #[test]
    fn extract_star_reexport() {
        let symbols = extract("export * from './utils';");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "*");
        assert!(symbols[0]
            .type_dependencies
            .iter()
            .any(|d| d.contains("reexport-all:./utils")));
    }

    #[test]
    fn extract_star_as_reexport() {
        let symbols = extract("export * as utils from './utils';");
        let ns = find_symbol(&symbols, "utils");
        assert_eq!(ns.kind, SymbolKind::Namespace);
        assert!(ns
            .type_dependencies
            .iter()
            .any(|d| d.contains("reexport-all-as:./utils")));
    }

    // ── Default export extraction ────────────────────────────────────

    #[test]
    fn extract_default_function() {
        let symbols = extract("export default function main(): void;");
        let sym = find_symbol(&symbols, "main");
        assert_eq!(sym.kind, SymbolKind::Function);
        assert_eq!(sym.visibility, Visibility::Exported);
    }

    // ── Qualified names ──────────────────────────────────────────────

    #[test]
    fn qualified_name_structure() {
        let symbols = extract("export declare function greet(): void;");
        // File is test.d.ts → stem is "test"
        assert_eq!(symbols[0].qualified_name, "test.greet");
    }

    #[test]
    fn qualified_name_for_class_member() {
        let symbols = extract(
            r#"
export declare class Foo {
    bar(): void;
}
"#,
        );
        let cls = find_symbol(&symbols, "Foo");
        let bar = find_symbol(&cls.members, "bar");
        assert_eq!(bar.qualified_name, "test.Foo.bar");
    }

    // ── Line numbers ─────────────────────────────────────────────────

    #[test]
    fn correct_line_numbers() {
        let symbols = extract(
            r#"export declare function first(): void;
export declare function second(): void;
export declare function third(): void;
"#,
        );
        assert_eq!(find_symbol(&symbols, "first").line, 1);
        assert_eq!(find_symbol(&symbols, "second").line, 2);
        assert_eq!(find_symbol(&symbols, "third").line, 3);
    }

    // ── Type dependency collection ───────────────────────────────────

    #[test]
    fn type_deps_from_complex_types() {
        let symbols = extract(
            "export declare function process(data: Map<string, Array<Item>>): Result<Output, AppError>;",
        );
        let deps = &symbols[0].type_dependencies;
        assert!(deps.contains(&"Map".to_string()));
        assert!(deps.contains(&"Array".to_string()));
        assert!(deps.contains(&"Item".to_string()));
        assert!(deps.contains(&"Result".to_string()));
        assert!(deps.contains(&"Output".to_string()));
        assert!(deps.contains(&"AppError".to_string()));
    }

    #[test]
    fn type_deps_skip_builtins() {
        let symbols =
            extract("export declare function basic(a: string, b: number, c: boolean): void;");
        // No type deps for built-in types
        assert!(symbols[0].type_dependencies.is_empty());
    }

    // ── File discovery ───────────────────────────────────────────────

    #[test]
    fn find_dts_files_skips_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Create structure:
        // root/
        //   src/
        //     api.d.ts
        //     utils.d.ts
        //   node_modules/
        //     pkg/
        //       index.d.ts
        //   .hidden/
        //     secret.d.ts
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();

        std::fs::write(root.join("src/api.d.ts"), "").unwrap();
        std::fs::write(root.join("src/utils.d.ts"), "").unwrap();
        std::fs::write(root.join("node_modules/pkg/index.d.ts"), "").unwrap();
        std::fs::write(root.join(".hidden/secret.d.ts"), "").unwrap();

        let files = find_dts_files(root).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f.ends_with("api.d.ts")));
        assert!(files.iter().any(|f| f.ends_with("utils.d.ts")));
    }

    // ── Build output deduplication ───────────────────────────────────

    #[test]
    fn dedup_esm_and_js_keeps_esm() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Simulate patternfly-react layout:
        // packages/react-core/dist/esm/Alert.d.ts
        // packages/react-core/dist/js/Alert.d.ts
        std::fs::create_dir_all(root.join("packages/react-core/dist/esm")).unwrap();
        std::fs::create_dir_all(root.join("packages/react-core/dist/js")).unwrap();
        std::fs::write(root.join("packages/react-core/dist/esm/Alert.d.ts"), "").unwrap();
        std::fs::write(root.join("packages/react-core/dist/js/Alert.d.ts"), "").unwrap();

        let files = find_dts_files(root).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].to_string_lossy().contains("dist/esm/"));
    }

    #[test]
    fn dedup_esm_and_cjs_keeps_esm() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join("dist/esm/components")).unwrap();
        std::fs::create_dir_all(root.join("dist/cjs/components")).unwrap();
        std::fs::write(root.join("dist/esm/components/Foo.d.ts"), "").unwrap();
        std::fs::write(root.join("dist/cjs/components/Foo.d.ts"), "").unwrap();
        std::fs::write(root.join("dist/esm/index.d.ts"), "").unwrap();
        std::fs::write(root.join("dist/cjs/index.d.ts"), "").unwrap();

        let files = find_dts_files(root).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files
            .iter()
            .all(|f| f.to_string_lossy().contains("dist/esm/")));
    }

    #[test]
    fn dedup_js_and_cjs_keeps_js() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // No ESM — js > cjs priority
        std::fs::create_dir_all(root.join("dist/js")).unwrap();
        std::fs::create_dir_all(root.join("dist/cjs")).unwrap();
        std::fs::write(root.join("dist/js/index.d.ts"), "").unwrap();
        std::fs::write(root.join("dist/cjs/index.d.ts"), "").unwrap();

        let files = find_dts_files(root).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].to_string_lossy().contains("dist/js/"));
    }

    #[test]
    fn dedup_single_variant_not_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Only one variant — should not be removed
        std::fs::create_dir_all(root.join("dist/js")).unwrap();
        std::fs::write(root.join("dist/js/index.d.ts"), "").unwrap();

        let files = find_dts_files(root).unwrap();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn dedup_non_dist_files_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // ESM/JS duplicates + non-dist files (like CSS module decls)
        std::fs::create_dir_all(root.join("dist/esm")).unwrap();
        std::fs::create_dir_all(root.join("dist/js")).unwrap();
        std::fs::create_dir_all(root.join("css/components")).unwrap();
        std::fs::write(root.join("dist/esm/index.d.ts"), "").unwrap();
        std::fs::write(root.join("dist/js/index.d.ts"), "").unwrap();
        std::fs::write(root.join("css/components/button.d.ts"), "").unwrap();

        let files = find_dts_files(root).unwrap();
        assert_eq!(files.len(), 2); // esm/index.d.ts + css/button.d.ts
        assert!(files
            .iter()
            .any(|f| f.to_string_lossy().contains("dist/esm/")));
        assert!(files.iter().any(|f| f.to_string_lossy().contains("css/")));
    }

    #[test]
    fn dedup_multiple_packages_independent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Two packages, each with esm+js — each deduped independently
        for pkg in &["pkg-a", "pkg-b"] {
            std::fs::create_dir_all(root.join(format!("packages/{}/dist/esm", pkg))).unwrap();
            std::fs::create_dir_all(root.join(format!("packages/{}/dist/js", pkg))).unwrap();
            std::fs::write(
                root.join(format!("packages/{}/dist/esm/index.d.ts", pkg)),
                "",
            )
            .unwrap();
            std::fs::write(
                root.join(format!("packages/{}/dist/js/index.d.ts", pkg)),
                "",
            )
            .unwrap();
        }

        let files = find_dts_files(root).unwrap();
        assert_eq!(files.len(), 2); // One per package, both from esm
        assert!(files
            .iter()
            .all(|f| f.to_string_lossy().contains("dist/esm/")));
    }

    #[test]
    fn dedup_unknown_variant_dirs_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // "types" is not a known build output variant
        std::fs::create_dir_all(root.join("dist/types")).unwrap();
        std::fs::create_dir_all(root.join("dist/esm")).unwrap();
        std::fs::write(root.join("dist/types/index.d.ts"), "").unwrap();
        std::fs::write(root.join("dist/esm/index.d.ts"), "").unwrap();

        let files = find_dts_files(root).unwrap();
        // "types" is not in priority list, so it's not considered a variant
        // and kept alongside esm
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn find_redundant_dist_dirs_empty() {
        let files: Vec<PathBuf> = vec![];
        assert!(find_redundant_dist_dirs(&files).is_empty());
    }

    // ── Edge cases ───────────────────────────────────────────────────

    #[test]
    fn extract_empty_file() {
        let symbols = extract("");
        assert!(symbols.is_empty());
    }

    #[test]
    fn extract_non_exported_declarations_are_skipped() {
        let symbols = extract(
            r#"
declare function internal(): void;
declare class InternalClass {}
"#,
        );
        // Non-exported declarations in .d.ts are not part of the public API
        assert!(symbols.is_empty());
    }

    #[test]
    fn extract_multiple_declarations() {
        let symbols = extract(
            r#"
export declare function foo(): void;
export interface Bar { x: number; }
export type Baz = string;
export declare const QUX: boolean;
export declare enum Status { Active = 0, Inactive = 1 }
"#,
        );
        assert_eq!(symbols.len(), 5);
        assert!(symbols
            .iter()
            .any(|s| s.name == "foo" && s.kind == SymbolKind::Function));
        assert!(symbols
            .iter()
            .any(|s| s.name == "Bar" && s.kind == SymbolKind::Interface));
        assert!(symbols
            .iter()
            .any(|s| s.name == "Baz" && s.kind == SymbolKind::TypeAlias));
        assert!(symbols
            .iter()
            .any(|s| s.name == "QUX" && s.kind == SymbolKind::Constant));
        assert!(symbols
            .iter()
            .any(|s| s.name == "Status" && s.kind == SymbolKind::Enum));
    }

    #[test]
    fn extract_class_with_type_params() {
        let symbols = extract(
            r#"
export declare class Container<T extends Serializable> {
    value: T;
    get(): T;
}
"#,
        );
        let cls = find_symbol(&symbols, "Container");
        assert!(cls.type_dependencies.contains(&"Serializable".to_string()));
    }

    #[test]
    fn extract_interface_call_and_construct_signatures() {
        let symbols = extract(
            r#"
export interface Factory {
    (): Widget;
    new (config: Config): Widget;
}
"#,
        );
        let iface = find_symbol(&symbols, "Factory");
        assert!(iface.members.iter().any(|m| m.name == "(call)"));
        assert!(iface.members.iter().any(|m| m.name == "new"));
    }

    // ── Line offset computation ──────────────────────────────────────

    #[test]
    fn line_offset_computation() {
        let offsets = compute_line_offsets("abc\ndef\nghi");
        assert_eq!(offsets, vec![0, 4, 8]);
        assert_eq!(offset_to_line(&offsets, 0), 1); // 'a'
        assert_eq!(offset_to_line(&offsets, 3), 1); // '\n'
        assert_eq!(offset_to_line(&offsets, 4), 2); // 'd'
        assert_eq!(offset_to_line(&offsets, 8), 3); // 'g'
    }

    // ── @types/* scanning ────────────────────────────────────────────

    #[test]
    fn find_export_as_namespace_found() {
        let source = r#"
export = React;
export as namespace React;
declare namespace React {
    type ReactNode = string;
}
"#;
        assert_eq!(find_export_as_namespace(source), Some("React".to_string()));
    }

    #[test]
    fn find_export_as_namespace_not_found() {
        let source = r#"
export declare function foo(): void;
export interface Bar {}
"#;
        assert_eq!(find_export_as_namespace(source), None);
    }

    #[test]
    fn find_export_as_namespace_with_semicolon() {
        let source = "export as namespace MyLib;";
        assert_eq!(find_export_as_namespace(source), Some("MyLib".to_string()));
    }

    #[test]
    fn find_declare_namespaces_finds_global() {
        let source = r#"
declare namespace NodeJS {
    interface Process {}
}
declare var process: NodeJS.Process;
"#;
        let ns = find_declare_namespaces(source);
        assert_eq!(ns, vec!["NodeJS".to_string()]);
    }

    #[test]
    fn find_declare_namespaces_multiple() {
        let source = r#"
declare namespace NodeJS { }
declare namespace Buffer { }
"#;
        let ns = find_declare_namespaces(source);
        assert_eq!(ns.len(), 2);
        assert!(ns.contains(&"NodeJS".to_string()));
        assert!(ns.contains(&"Buffer".to_string()));
    }

    #[test]
    fn has_module_syntax_detects_import() {
        assert!(has_module_syntax("import React from 'react';"));
        assert!(has_module_syntax("export declare function foo(): void;"));
    }

    #[test]
    fn has_module_syntax_false_for_ambient() {
        let source = r#"
// A comment
declare namespace NodeJS { }
declare var process: NodeJS.Process;
"#;
        assert!(!has_module_syntax(source));
    }

    #[test]
    fn scan_types_packages_from_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let types_dir = dir.path().join("node_modules/@types/react");
        std::fs::create_dir_all(&types_dir).unwrap();

        // Write a minimal package.json
        std::fs::write(
            types_dir.join("package.json"),
            r#"{"types": "index.d.ts", "main": ""}"#,
        )
        .unwrap();

        // Write a minimal index.d.ts with export as namespace
        std::fs::write(
            types_dir.join("index.d.ts"),
            r#"
export = React;
export as namespace React;
declare namespace React {
    type ReactNode = string | number;
}
"#,
        )
        .unwrap();

        let map = scan_types_packages(dir.path());
        assert!(map.is_namespace_or_default("React"));
    }

    #[test]
    fn scan_types_packages_ambient_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let types_dir = dir.path().join("node_modules/@types/node");
        std::fs::create_dir_all(&types_dir).unwrap();

        std::fs::write(
            types_dir.join("package.json"),
            r#"{"types": "globals.d.ts"}"#,
        )
        .unwrap();

        // Script-mode file (no import/export) with declare namespace
        std::fs::write(
            types_dir.join("globals.d.ts"),
            r#"
declare namespace NodeJS {
    interface Process { }
}
declare var process: NodeJS.Process;
"#,
        )
        .unwrap();

        let map = scan_types_packages(dir.path());
        assert!(map.is_namespace_or_default("NodeJS"));
    }

    #[test]
    fn scan_types_packages_walks_up_for_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        // node_modules at root, but we'll pass a subdirectory
        let types_dir = dir.path().join("node_modules/@types/react");
        std::fs::create_dir_all(&types_dir).unwrap();
        let subdir = dir.path().join("packages/react-core");
        std::fs::create_dir_all(&subdir).unwrap();

        std::fs::write(types_dir.join("package.json"), r#"{"types": "index.d.ts"}"#).unwrap();
        std::fs::write(
            types_dir.join("index.d.ts"),
            "export as namespace React;\ndeclare namespace React { }",
        )
        .unwrap();

        let map = scan_types_packages(&subdir);
        assert!(
            map.is_namespace_or_default("React"),
            "Should find @types/react by walking up from subdirectory"
        );
    }

    #[test]
    fn scan_types_packages_ignores_module_files() {
        // A @types package whose entry file has import/export (module-mode)
        // and no `export as namespace`. Bare `declare namespace` in a module
        // file is NOT global.
        let dir = tempfile::tempdir().unwrap();
        let types_dir = dir.path().join("node_modules/@types/somepkg");
        std::fs::create_dir_all(&types_dir).unwrap();

        std::fs::write(types_dir.join("package.json"), r#"{"types": "index.d.ts"}"#).unwrap();
        std::fs::write(
            types_dir.join("index.d.ts"),
            r#"
import { Dependency } from 'other';
declare namespace Internal { }
export declare function foo(): void;
"#,
        )
        .unwrap();

        let map = scan_types_packages(dir.path());
        // Should NOT pick up "Internal" because the file is a module
        assert!(!map.is_namespace_or_default("Internal"));
    }

    // ── Two-pass extraction / global import map ──────────────────────

    #[test]
    fn collect_imports_from_source_finds_namespace() {
        let source = r#"
import * as React from 'react';
export declare const Foo: React.FunctionComponent<{}>;
"#;
        let map = collect_imports_from_source(source);
        assert!(map.is_namespace_or_default("React"));
    }

    #[test]
    fn global_import_map_resolves_unimported_namespace() {
        // Simulates the v6 CardBody.d.ts scenario:
        // File only imports { JSX } from 'react', but uses React.ReactNode
        // (React is a global namespace from @types/react).
        let source = r#"
import type { JSX } from 'react';
export interface CardBodyProps {
    children?: React.ReactNode;
    component?: keyof JSX.IntrinsicElements;
}
export declare const CardBody: React.FunctionComponent<CardBodyProps>;
"#;
        // Without global imports, React.ReactNode passes through unstripped
        let extractor = OxcExtractor::new();
        let symbols_no_globals = extractor.extract_from_source(source, Path::new("CardBody.d.ts"));
        let body_no_globals = find_symbol(&symbols_no_globals, "CardBody");
        let sig_no = body_no_globals.signature.as_ref().unwrap();
        // Without globals: React.FunctionComponent is NOT stripped
        assert!(
            sig_no.return_type.as_ref().unwrap().contains("React."),
            "Without globals, React. prefix should remain: {:?}",
            sig_no.return_type
        );

        // With global imports (simulating @types/react detection)
        let mut global = crate::canon::ImportMap::new();
        global.add_namespace("React", "react");
        let symbols_with_globals =
            extractor.extract_from_source_with_globals(source, Path::new("CardBody.d.ts"), &global);
        let body_with_globals = find_symbol(&symbols_with_globals, "CardBody");
        let sig_yes = body_with_globals.signature.as_ref().unwrap();
        // With globals: React.FunctionComponent → FunctionComponent
        assert!(
            !sig_yes.return_type.as_ref().unwrap().contains("React."),
            "With globals, React. prefix should be stripped: {:?}",
            sig_yes.return_type
        );
    }

    #[test]
    fn two_pass_merges_namespace_imports_across_files() {
        // File A has `import * as React from 'react'`
        // File B has no React import but uses React.ReactNode
        // Two-pass extraction should merge React namespace from A → B

        let dir = tempfile::tempdir().unwrap();

        // File A: has explicit namespace import
        std::fs::write(
            dir.path().join("FileA.d.ts"),
            r#"
import * as React from 'react';
export declare const Foo: React.FunctionComponent<{}>;
"#,
        )
        .unwrap();

        // File B: uses React.ReactNode without importing React
        std::fs::write(
            dir.path().join("FileB.d.ts"),
            r#"
export interface Props {
    children?: React.ReactNode;
}
"#,
        )
        .unwrap();

        let extractor = OxcExtractor::new();
        let surface = extractor.extract_from_dir(dir.path()).unwrap();

        // Find the children property in Props
        let props = surface
            .symbols
            .iter()
            .find(|s| s.name == "Props")
            .expect("Props interface should be extracted");

        let children = props
            .members
            .iter()
            .find(|m| m.name == "children")
            .expect("children member should exist");

        // The type should be "ReactNode" (stripped), not "React.ReactNode"
        let sig = children.signature.as_ref().unwrap();
        assert_eq!(
            sig.return_type.as_deref(),
            Some("ReactNode"),
            "React. prefix should be stripped via two-pass import map merge"
        );
    }

    #[test]
    fn two_pass_with_types_packages() {
        // Simulates the full pipeline: @types/react provides the global
        // React namespace, and files use React.X without importing it.

        let dir = tempfile::tempdir().unwrap();

        // Set up @types/react
        let types_dir = dir.path().join("node_modules/@types/react");
        std::fs::create_dir_all(&types_dir).unwrap();
        std::fs::write(types_dir.join("package.json"), r#"{"types": "index.d.ts"}"#).unwrap();
        std::fs::write(
            types_dir.join("index.d.ts"),
            "export = React;\nexport as namespace React;\ndeclare namespace React { type ReactNode = string; }",
        )
        .unwrap();

        // A .d.ts file that uses React.ReactNode without importing React
        std::fs::write(
            dir.path().join("Component.d.ts"),
            r#"
export interface MyProps {
    label: React.ReactNode;
}
"#,
        )
        .unwrap();

        let extractor = OxcExtractor::new();
        let surface = extractor.extract_from_dir(dir.path()).unwrap();

        let props = surface
            .symbols
            .iter()
            .find(|s| s.name == "MyProps")
            .expect("MyProps should be extracted");
        let label = props
            .members
            .iter()
            .find(|m| m.name == "label")
            .expect("label member should exist");
        let sig = label.signature.as_ref().unwrap();
        assert_eq!(
            sig.return_type.as_deref(),
            Some("ReactNode"),
            "React.ReactNode should be canonicalized to ReactNode via @types scan"
        );
    }

    #[test]
    fn local_import_takes_priority_over_global() {
        // If a file has its own import for a name, it should take priority
        // over the global import map.
        let source = r#"
import MyReact from 'custom-react';
export declare const Foo: MyReact.Component;
"#;
        // Global map says "MyReact" is a namespace for "different-module"
        let mut global = crate::canon::ImportMap::new();
        global.add_namespace("MyReact", "different-module");

        let extractor = OxcExtractor::new();
        let symbols =
            extractor.extract_from_source_with_globals(source, Path::new("test.d.ts"), &global);

        // Should still strip MyReact. because the local import is a default import,
        // which is also recognized as a qualifier
        let foo = find_symbol(&symbols, "Foo");
        let sig = foo.signature.as_ref().unwrap();
        assert_eq!(
            sig.return_type.as_deref(),
            Some("Component"),
            "Local import should take priority; MyReact. should be stripped"
        );
    }

    // ── ImportMap merge tests ────────────────────────────────────────

    #[test]
    fn import_map_merge_namespaces_only() {
        let mut base = crate::canon::ImportMap::new();
        base.add_namespace("React", "react");

        let mut other = crate::canon::ImportMap::new();
        other.add_namespace("Lodash", "lodash");
        other.add_named("useState", "useState", "react");

        base.merge_namespaces_from(&other);

        // Namespace import merged
        assert!(base.is_namespace_or_default("Lodash"));
        // Named import NOT merged
        assert!(base.named_import_module("useState").is_none());
    }

    #[test]
    fn import_map_merge_does_not_overwrite() {
        let mut base = crate::canon::ImportMap::new();
        base.add_namespace("React", "custom-react");

        let mut other = crate::canon::ImportMap::new();
        other.add_namespace("React", "react");

        base.merge_all_from(&other);

        // Original entry should be preserved
        assert_eq!(base.module_for("React"), Some("custom-react"));
    }

    // ── remap_dist_to_src ────────────────────────────────────────────

    #[test]
    fn remap_dist_esm_to_src() {
        assert_eq!(
            remap_dist_to_src(Path::new(
                "packages/react-core/dist/esm/components/Button/Button.d.ts"
            )),
            PathBuf::from("packages/react-core/src/components/Button/Button.d.ts")
        );
    }

    #[test]
    fn remap_dist_js_to_src() {
        assert_eq!(
            remap_dist_to_src(Path::new(
                "packages/react-core/dist/js/components/Card/Card.d.ts"
            )),
            PathBuf::from("packages/react-core/src/components/Card/Card.d.ts")
        );
    }

    #[test]
    fn remap_dist_cjs_to_src() {
        assert_eq!(
            remap_dist_to_src(Path::new("packages/react-core/dist/cjs/index.d.ts")),
            PathBuf::from("packages/react-core/src/index.d.ts")
        );
    }

    #[test]
    fn remap_bare_dist_to_src() {
        assert_eq!(
            remap_dist_to_src(Path::new("packages/react-core/dist/components/Button.d.ts")),
            PathBuf::from("packages/react-core/src/components/Button.d.ts")
        );
    }

    #[test]
    fn remap_no_dist_unchanged() {
        let path = Path::new("packages/react-core/src/components/Button/Button.d.ts");
        assert_eq!(remap_dist_to_src(path), path.to_path_buf());
    }

    #[test]
    fn remap_preserves_deprecated_subpath() {
        assert_eq!(
            remap_dist_to_src(Path::new(
                "packages/react-core/dist/esm/deprecated/components/Chip/Chip.d.ts"
            )),
            PathBuf::from("packages/react-core/src/deprecated/components/Chip/Chip.d.ts")
        );
    }

    // ─── populate_rendered_components tests ───────────────────────────────

    #[test]
    fn populate_rendered_components_from_tsx_files() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let base = tmp.path();

        // Create a component directory structure
        let comp_dir = base.join("src/components/Dropdown");
        fs::create_dir_all(&comp_dir).unwrap();

        // Write a .tsx source file with JSX
        fs::write(
            comp_dir.join("Dropdown.tsx"),
            r#"
import React from 'react';
export const Dropdown: React.FC = ({ children }) => {
    return (
        <div className="dropdown">
            <DropdownToggle />
            <DropdownMenu>
                {children}
            </DropdownMenu>
        </div>
    );
};
"#,
        )
        .unwrap();

        // Create matching Symbol with .d.ts file path
        let sym = Symbol::new(
            "Dropdown",
            "src/components/Dropdown/Dropdown.Dropdown",
            SymbolKind::Variable,
            Visibility::Exported,
            PathBuf::from("src/components/Dropdown/Dropdown.d.ts"),
            1,
        );

        // Also create a non-component symbol (should be skipped)
        let type_sym = Symbol::new(
            "DropdownProps",
            "src/components/Dropdown/Dropdown.DropdownProps",
            SymbolKind::Interface,
            Visibility::Exported,
            PathBuf::from("src/components/Dropdown/Dropdown.d.ts"),
            5,
        );

        // Also a lowercase variable (should be skipped)
        let const_sym = Symbol::new(
            "defaultDropdownWidth",
            "src/components/Dropdown/Dropdown.defaultDropdownWidth",
            SymbolKind::Variable,
            Visibility::Exported,
            PathBuf::from("src/components/Dropdown/Dropdown.d.ts"),
            10,
        );

        let mut symbols = vec![sym.clone(), type_sym, const_sym];
        populate_rendered_components(&mut symbols, base);

        // The Dropdown symbol should have rendered_components populated
        assert!(
            !symbols[0].language_data.rendered_components.is_empty(),
            "Dropdown should have rendered_components"
        );
        assert!(
            symbols[0]
                .language_data
                .rendered_components
                .contains(&"DropdownToggle".to_string()),
            "should contain DropdownToggle"
        );
        assert!(
            symbols[0]
                .language_data
                .rendered_components
                .contains(&"DropdownMenu".to_string()),
            "should contain DropdownMenu"
        );

        // Interface and lowercase symbols should NOT have rendered_components
        assert!(symbols[1].language_data.rendered_components.is_empty());
        assert!(symbols[2].language_data.rendered_components.is_empty());
    }

    #[test]
    fn populate_rendered_components_missing_tsx_file() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();

        // Symbol pointing to a .tsx that doesn't exist
        let sym = Symbol::new(
            "Missing",
            "src/components/Missing/Missing.Missing",
            SymbolKind::Variable,
            Visibility::Exported,
            PathBuf::from("src/components/Missing/Missing.d.ts"),
            1,
        );

        let mut symbols = vec![sym];
        populate_rendered_components(&mut symbols, tmp.path());

        // Should gracefully handle missing file
        assert!(symbols[0].language_data.rendered_components.is_empty());
    }

    #[test]
    fn populate_rendered_components_caches_per_file() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let comp_dir = tmp.path().join("src/components/Modal");
        fs::create_dir_all(&comp_dir).unwrap();

        // Single .tsx file with a component
        fs::write(
            comp_dir.join("Modal.tsx"),
            r#"
export const Modal = () => {
    return <ModalBody />;
};
"#,
        )
        .unwrap();

        // Two symbols from the same file
        let sym1 = Symbol::new(
            "Modal",
            "src/components/Modal/Modal.Modal",
            SymbolKind::Function,
            Visibility::Exported,
            PathBuf::from("src/components/Modal/Modal.d.ts"),
            1,
        );
        let sym2 = Symbol::new(
            "ModalVariant",
            "src/components/Modal/Modal.ModalVariant",
            SymbolKind::Variable,
            Visibility::Exported,
            PathBuf::from("src/components/Modal/Modal.d.ts"),
            5,
        );

        let mut symbols = vec![sym1, sym2];
        populate_rendered_components(&mut symbols, tmp.path());

        // Both should get the same rendered_components (from cache)
        assert_eq!(
            symbols[0].language_data.rendered_components,
            symbols[1].language_data.rendered_components
        );
        assert!(symbols[0]
            .language_data
            .rendered_components
            .contains(&"ModalBody".to_string()));
    }
}
