mod cli;
mod konveyor;
mod orchestrator;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::Path;

use cli::{Cli, Command};
use semver_analyzer_core::{
    AnalysisMetadata, AnalysisReport, ApiChange, ApiChangeKind, ApiChangeType, ApiSurface,
    BehavioralChange, Comparison, FileChanges, FileStatus, ManifestChange, StructuralChange,
    Summary,
};
use semver_analyzer_ts::OxcExtractor;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Extract {
            repo,
            git_ref,
            output,
            build_command,
        } => {
            cmd_extract(&repo, &git_ref, output.as_deref(), build_command.as_deref())?;
        }

        Command::Diff { from, to, output } => {
            cmd_diff(&from, &to, output.as_deref())?;
        }

        Command::Analyze {
            repo,
            from,
            to,
            output,
            no_llm,
            llm_command,
            max_llm_cost,
            build_command,
            llm_all_files,
        } => {
            cmd_analyze(
                &repo,
                &from,
                &to,
                output.as_deref(),
                no_llm,
                llm_command.as_deref(),
                max_llm_cost,
                build_command.as_deref(),
                llm_all_files,
            )
            .await?;
        }

        Command::Konveyor {
            from_report,
            repo,
            from,
            to,
            output_dir,
            file_pattern,
            provider,
            ruleset_name,
            no_llm,
            llm_command,
            max_llm_cost,
            build_command,
            llm_all_files,
        } => {
            cmd_konveyor(
                from_report.as_deref(),
                repo.as_deref(),
                from.as_deref(),
                to.as_deref(),
                &output_dir,
                &file_pattern,
                &provider,
                &ruleset_name,
                no_llm,
                llm_command.as_deref(),
                max_llm_cost,
                build_command.as_deref(),
                llm_all_files,
            )
            .await?;
        }

        Command::Serve => {
            eprintln!("MCP server not yet implemented");
        }
    }

    Ok(())
}

// ─── Extract command ─────────────────────────────────────────────────────

fn cmd_extract(
    repo: &Path,
    git_ref: &str,
    output: Option<&Path>,
    build_command: Option<&str>,
) -> Result<()> {
    eprintln!(
        "Extracting API surface from {} at ref {}",
        repo.display(),
        git_ref
    );

    let extractor = OxcExtractor::new();
    let surface = extractor
        .extract_at_ref(repo, git_ref, build_command)
        .context("Failed to extract API surface")?;

    eprintln!(
        "Extracted {} symbols from {} files",
        surface.symbols.len(),
        count_unique_files(&surface)
    );

    write_json_output(&surface, output)?;
    Ok(())
}

// ─── Diff command ────────────────────────────────────────────────────────

fn cmd_diff(from_path: &Path, to_path: &Path, output: Option<&Path>) -> Result<()> {
    eprintln!(
        "Diffing {} vs {}",
        from_path.display(),
        to_path.display()
    );

    let old_json = std::fs::read_to_string(from_path)
        .with_context(|| format!("Failed to read {}", from_path.display()))?;
    let new_json = std::fs::read_to_string(to_path)
        .with_context(|| format!("Failed to read {}", to_path.display()))?;

    let old: ApiSurface = serde_json::from_str(&old_json)
        .with_context(|| format!("Failed to parse {} as ApiSurface", from_path.display()))?;
    let new: ApiSurface = serde_json::from_str(&new_json)
        .with_context(|| format!("Failed to parse {} as ApiSurface", to_path.display()))?;

    let changes = semver_analyzer_core::diff::diff_surfaces(&old, &new);

    let breaking = changes.iter().filter(|c| c.is_breaking).count();
    let non_breaking = changes.len() - breaking;
    eprintln!(
        "Found {} changes ({} breaking, {} non-breaking)",
        changes.len(),
        breaking,
        non_breaking
    );

    write_json_output(&changes, output)?;
    Ok(())
}

// ─── Analyze command (concurrent TD+BU pipeline) ────────────────────────

async fn cmd_analyze(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    output: Option<&Path>,
    no_llm: bool,
    llm_command: Option<&str>,
    _max_llm_cost: f64,
    build_command: Option<&str>,
    llm_all_files: bool,
) -> Result<()> {
    eprintln!(
        "Analyzing {} from {} to {}",
        repo.display(),
        from_ref,
        to_ref
    );
    if no_llm {
        eprintln!("Mode: static analysis only (--no-llm)");
    }

    // Run concurrent TD+BU pipeline
    let result = orchestrator::run_concurrent_analysis(
        repo,
        from_ref,
        to_ref,
        no_llm,
        llm_command,
        build_command,
        llm_all_files,
    )
    .await?;

    // Print summary stats
    let manifest_breaking = result.manifest_changes.iter().filter(|c| c.is_breaking).count();
    if !result.manifest_changes.is_empty() {
        eprintln!(
            "[TD]   {} manifest changes ({} breaking)",
            result.manifest_changes.len(),
            manifest_breaking
        );
    }

    // Build report
    let report = build_report(
        repo,
        from_ref,
        to_ref,
        result.structural_changes,
        result.behavioral_changes,
        result.manifest_changes,
        result.llm_api_changes,
    );

    let total_breaking = report.summary.total_breaking_changes;
    eprintln!();
    if total_breaking == 0 {
        eprintln!("No breaking changes detected.");
    } else {
        eprintln!(
            "BREAKING: {} total breaking change(s) detected.",
            total_breaking
        );
        eprintln!(
            "  {} API changes, {} behavioral changes",
            report.summary.breaking_api_changes,
            report.summary.breaking_behavioral_changes
        );
    }

    write_json_output(&report, output)?;
    Ok(())
}

// ─── Konveyor command ────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn cmd_konveyor(
    from_report: Option<&Path>,
    repo: Option<&Path>,
    from_ref: Option<&str>,
    to_ref: Option<&str>,
    output_dir: &Path,
    file_pattern: &str,
    provider_str: &str,
    ruleset_name: &str,
    no_llm: bool,
    llm_command: Option<&str>,
    _max_llm_cost: f64,
    build_command: Option<&str>,
    llm_all_files: bool,
) -> Result<()> {
    let provider = match provider_str {
        "frontend" => konveyor::RuleProvider::Frontend,
        "builtin" => konveyor::RuleProvider::Builtin,
        other => {
            eprintln!(
                "Warning: unknown provider '{}', falling back to 'builtin'",
                other
            );
            konveyor::RuleProvider::Builtin
        }
    };
    let report = if let Some(report_path) = from_report {
        // Mode 1: Load pre-existing report
        eprintln!("Loading report from {}", report_path.display());
        let json = std::fs::read_to_string(report_path)
            .with_context(|| format!("Failed to read {}", report_path.display()))?;
        let report: semver_analyzer_core::AnalysisReport = serde_json::from_str(&json)
            .with_context(|| format!("Failed to parse {} as AnalysisReport", report_path.display()))?;
        report
    } else {
        // Mode 2: Run analysis internally
        let repo = repo.context("--repo is required when --from-report is not provided")?;
        let from = from_ref.context("--from is required when --from-report is not provided")?;
        let to = to_ref.context("--to is required when --from-report is not provided")?;

        eprintln!(
            "Analyzing {} from {} to {}",
            repo.display(),
            from,
            to
        );
        if no_llm {
            eprintln!("Mode: static analysis only (--no-llm)");
        }

        let result = orchestrator::run_concurrent_analysis(
            repo, from, to, no_llm, llm_command, build_command, llm_all_files,
        )
        .await?;

        build_report(
            repo,
            from,
            to,
            result.structural_changes,
            result.behavioral_changes,
            result.manifest_changes,
            result.llm_api_changes,
        )
    };

    // Generate rules and fix guidance
    let rules = konveyor::generate_rules(&report, file_pattern, provider);
    let fix_guidance = konveyor::generate_fix_guidance(&report, &rules, file_pattern);
    let rule_count = rules.len();

    // Write ruleset directory
    konveyor::write_ruleset_dir(output_dir, ruleset_name, &report, &rules)?;

    // Write fix guidance to sibling directory
    let fix_dir = konveyor::write_fix_guidance_dir(output_dir, &fix_guidance)?;

    eprintln!(
        "Generated {} Konveyor rules in {}",
        rule_count,
        output_dir.display()
    );
    eprintln!("  Ruleset:  {}/ruleset.yaml", output_dir.display());
    eprintln!("  Rules:    {}/breaking-changes.yaml", output_dir.display());
    eprintln!(
        "  Fixes:    {}/fix-guidance.yaml",
        fix_dir.display()
    );
    eprintln!(
        "  Summary:  {} auto-fixable, {} need review, {} manual only",
        fix_guidance.summary.auto_fixable,
        fix_guidance.summary.needs_review,
        fix_guidance.summary.manual_only,
    );
    eprintln!();
    eprintln!(
        "Use with: konveyor-analyzer --rules {}",
        output_dir.display()
    );

    Ok(())
}

// ─── Report building ─────────────────────────────────────────────────────

fn build_report(
    repo: &Path,
    from_ref: &str,
    to_ref: &str,
    structural_changes: Vec<StructuralChange>,
    behavioral_changes: Vec<BehavioralChange>,
    manifest_changes: Vec<ManifestChange>,
    llm_api_changes: Vec<orchestrator::LlmApiChangeEntry>,
) -> AnalysisReport {
    // Group breaking structural changes by file, converting to v2 ApiChange format.
    // Non-breaking changes (symbol_added, etc.) are excluded from the report.
    let mut file_api_map: std::collections::BTreeMap<std::path::PathBuf, Vec<ApiChange>> =
        std::collections::BTreeMap::new();

    for change in &structural_changes {
        if !change.is_breaking {
            continue;
        }
        let file = qualified_name_to_file(&change.qualified_name);
        let api_change = structural_to_api_change(change);
        file_api_map.entry(file).or_default().push(api_change);
    }

    // Merge LLM-detected API changes into the file map.
    // These catch type-level changes that static .d.ts analysis misses
    // (interface extends, optionality, enum member changes, etc.)
    for entry in &llm_api_changes {
        let file = std::path::PathBuf::from(&entry.file_path);
        let change_type = match entry.change.as_str() {
            "type_changed" => ApiChangeType::TypeChanged,
            "removed" => ApiChangeType::Removed,
            "default_changed" => ApiChangeType::SignatureChanged,
            _ => ApiChangeType::SignatureChanged,
        };
        let kind = if entry.symbol.contains('.') {
            ApiChangeKind::Property
        } else {
            ApiChangeKind::Interface
        };
        let api_change = ApiChange {
            symbol: entry.symbol.clone(),
            kind,
            change: change_type,
            before: None,
            after: None,
            description: entry.description.clone(),
        };
        // Only add if not already present (avoid duplicating TD findings)
        let existing = file_api_map.entry(file).or_default();
        let already_exists = existing.iter().any(|c| c.symbol == entry.symbol);
        if !already_exists {
            existing.push(api_change);
        }
    }

    // Sort changes within each file by symbol name
    for changes in file_api_map.values_mut() {
        changes.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    }

    let api_breaking: usize = file_api_map.values().map(|v| v.len()).sum();
    let behavioral_breaking = behavioral_changes.len();

    // Merge behavioral changes into file map using the source_file
    // extracted from the BU pipeline's qualified names.
    let mut file_behavioral_map: std::collections::BTreeMap<
        std::path::PathBuf,
        Vec<BehavioralChange>,
    > = std::collections::BTreeMap::new();
    for bc in behavioral_changes {
        let file = if let Some(ref src) = bc.source_file {
            std::path::PathBuf::from(src)
        } else {
            std::path::PathBuf::from("(behavioral)")
        };
        file_behavioral_map.entry(file).or_default().push(bc);
    }

    // Build the combined file changes list
    let mut all_files: std::collections::BTreeSet<std::path::PathBuf> =
        std::collections::BTreeSet::new();
    all_files.extend(file_api_map.keys().cloned());
    all_files.extend(file_behavioral_map.keys().cloned());

    let changes: Vec<FileChanges> = all_files
        .into_iter()
        .map(|file| {
            let api_changes = file_api_map.remove(&file).unwrap_or_default();
            let behavioral = file_behavioral_map.remove(&file).unwrap_or_default();

            let status = if api_changes
                .iter()
                .all(|c| c.change == semver_analyzer_core::ApiChangeType::Removed)
                && behavioral.is_empty()
            {
                FileStatus::Deleted
            } else {
                FileStatus::Modified
            };
            FileChanges {
                file,
                status,
                renamed_from: None,
                breaking_api_changes: api_changes,
                breaking_behavioral_changes: behavioral,
            }
        })
        .collect();

    let files_with_breaking = changes.len();

    // Get SHAs
    let from_sha = resolve_sha(repo, from_ref).unwrap_or_else(|| from_ref.to_string());
    let to_sha = resolve_sha(repo, to_ref).unwrap_or_else(|| to_ref.to_string());
    let commit_count = count_commits(repo, from_ref, to_ref).unwrap_or(0);

    let call_graph_info = if behavioral_breaking > 0 {
        "static_with_hof_heuristics"
    } else {
        "none (no behavioral analysis)"
    };

    AnalysisReport {
        repository: repo.to_path_buf(),
        comparison: Comparison {
            from_ref: from_ref.to_string(),
            to_ref: to_ref.to_string(),
            from_sha,
            to_sha,
            commit_count,
            analysis_timestamp: chrono::Utc::now().to_rfc3339(),
        },
        summary: Summary {
            total_breaking_changes: api_breaking + behavioral_breaking,
            breaking_api_changes: api_breaking,
            breaking_behavioral_changes: behavioral_breaking,
            files_with_breaking_changes: files_with_breaking,
        },
        changes,
        manifest_changes,
        metadata: AnalysisMetadata {
            call_graph_analysis: call_graph_info.to_string(),
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            llm_usage: None,
        },
    }
}

/// Convert an internal StructuralChange to a v2-format ApiChange.
fn structural_to_api_change(sc: &StructuralChange) -> ApiChange {
    let kind = symbol_kind_to_api_kind(&sc.kind);
    let change = sc.change_type.to_api_change_type();

    // Build the v2 symbol name: `ComponentName.propName` format.
    // The qualified_name is like "packages/react-core/dist/esm/components/Card/Card.CardProps.isFlat"
    // We want: "Card.isFlat" or just "Card" for top-level symbols.
    let symbol = qualified_name_to_display_symbol(&sc.qualified_name, &sc.symbol);

    ApiChange {
        symbol,
        kind,
        change,
        before: sc.before.clone(),
        after: sc.after.clone(),
        description: sc.description.clone(),
    }
}

/// Map internal symbol kind string to v2 ApiChangeKind.
fn symbol_kind_to_api_kind(kind: &str) -> ApiChangeKind {
    match kind {
        "Function" => ApiChangeKind::Function,
        "Method" => ApiChangeKind::Method,
        "Class" => ApiChangeKind::Class,
        "Interface" => ApiChangeKind::Interface,
        "TypeAlias" => ApiChangeKind::TypeAlias,
        "Constant" | "Variable" => ApiChangeKind::Constant,
        "Enum" => ApiChangeKind::TypeAlias, // v2 uses type_alias for enums
        "Property" => ApiChangeKind::Property,
        "Namespace" => ApiChangeKind::ModuleExport,
        _ => ApiChangeKind::Property,
    }
}

/// Convert a qualified name to a human-readable display symbol.
///
/// Input: `packages/react-core/dist/esm/components/Card/Card.CardProps.isFlat`
/// Output: `CardProps.isFlat`
///
/// Input: `packages/react-core/dist/esm/components/Card/Card.Card`
/// Output: `Card`
///
/// Strategy: take the parts after the last path separator (the `.`-separated
/// symbol chain from the file stem), then drop the file stem if it matches
/// the first symbol part.
fn qualified_name_to_display_symbol(qualified_name: &str, symbol_name: &str) -> String {
    // Split by '/' to get path components, then take the last one which
    // contains the file stem + symbol chain
    let last_path = qualified_name.rsplit('/').next().unwrap_or(qualified_name);

    // Split by '.' to get [file_stem, symbol_parts...]
    let parts: Vec<&str> = last_path.split('.').collect();

    if parts.len() <= 1 {
        return symbol_name.to_string();
    }

    // parts[0] is the file stem (e.g., "Card"), parts[1..] are symbol parts
    let symbol_parts = &parts[1..];

    if symbol_parts.is_empty() {
        return symbol_name.to_string();
    }

    // If there's only one symbol part, it's the top-level symbol
    if symbol_parts.len() == 1 {
        return symbol_parts[0].to_string();
    }

    // Multiple parts: join with '.' (e.g., "CardProps.isFlat")
    // But use Component name instead of Props interface name where possible:
    // "CardProps.isFlat" → "Card.isFlat" if the first part ends with "Props"
    let mut display_parts = symbol_parts.to_vec();
    if display_parts[0].ends_with("Props") {
        let component = display_parts[0].strip_suffix("Props").unwrap();
        if !component.is_empty() {
            display_parts[0] = component;
        }
    }

    display_parts.join(".")
}

// ─── Git helpers ─────────────────────────────────────────────────────────

fn resolve_sha(repo: &Path, git_ref: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", git_ref])
        .current_dir(repo)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn count_commits(repo: &Path, from_ref: &str, to_ref: &str) -> Option<usize> {
    let output = std::process::Command::new("git")
        .args(["rev-list", "--count", &format!("{}..{}", from_ref, to_ref)])
        .current_dir(repo)
        .output()
        .ok()?;
    if output.status.success() {
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .ok()
    } else {
        None
    }
}

// ─── Output helpers ──────────────────────────────────────────────────────

fn write_json_output(value: &impl serde::Serialize, output: Option<&Path>) -> Result<()> {
    let json = serde_json::to_string_pretty(value)?;
    if let Some(path) = output {
        std::fs::write(path, &json)
            .with_context(|| format!("Failed to write output to {}", path.display()))?;
        eprintln!("Output written to {}", path.display());
    } else {
        println!("{}", json);
    }
    Ok(())
}

fn count_unique_files(surface: &ApiSurface) -> usize {
    let files: std::collections::HashSet<&std::path::Path> =
        surface.symbols.iter().map(|s| s.file.as_path()).collect();
    files.len()
}

/// Convert a qualified name like "src/api/users.createUser" to a file path.
fn qualified_name_to_file(qualified_name: &str) -> std::path::PathBuf {
    if let Some(dot_pos) = qualified_name.rfind('.') {
        let file_part = &qualified_name[..dot_pos];
        std::path::PathBuf::from(format!("{}.d.ts", file_part))
    } else {
        std::path::PathBuf::from(qualified_name)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use semver_analyzer_core::StructuralChangeType;

    #[test]
    fn qualified_name_to_file_simple() {
        assert_eq!(
            qualified_name_to_file("test.greet"),
            std::path::PathBuf::from("test.d.ts")
        );
    }

    #[test]
    fn qualified_name_to_file_nested() {
        assert_eq!(
            qualified_name_to_file("src/api/users.createUser"),
            std::path::PathBuf::from("src/api/users.d.ts")
        );
    }

    #[test]
    fn qualified_name_to_file_class_member() {
        // "test.Foo.bar" → last dot separates the member name
        assert_eq!(
            qualified_name_to_file("test.Foo.bar"),
            std::path::PathBuf::from("test.Foo.d.ts")
        );
    }

    #[test]
    fn build_report_empty() {
        let report = build_report(
            Path::new("/tmp/repo"),
            "v1.0.0",
            "v2.0.0",
            vec![],
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(report.summary.total_breaking_changes, 0);
        assert!(report.changes.is_empty());
        assert!(report.manifest_changes.is_empty());
    }

    #[test]
    fn build_report_counts_breaking() {
        let changes = vec![
            StructuralChange {
                symbol: "foo".into(),
                qualified_name: "test.foo".into(),
                kind: "Function".into(),
                change_type: StructuralChangeType::SymbolRemoved,
                before: None,
                after: None,
                description: "removed".into(),
                is_breaking: true,
                impact: None,
            },
            StructuralChange {
                symbol: "bar".into(),
                qualified_name: "test.bar".into(),
                kind: "Function".into(),
                change_type: StructuralChangeType::SymbolAdded,
                before: None,
                after: None,
                description: "added".into(),
                is_breaking: false,
                impact: None,
            },
        ];
        let manifest = vec![ManifestChange {
            field: "type".into(),
            change_type: semver_analyzer_core::ManifestChangeType::ModuleSystemChanged,
            before: Some("commonjs".into()),
            after: Some("module".into()),
            description: "CJS to ESM".into(),
            is_breaking: true,
        }];

        let report = build_report(
            Path::new("/tmp/repo"),
            "v1",
            "v2",
            changes,
            vec![], // no behavioral changes
            manifest,
            vec![],
        );
        // Only the breaking structural change counts (non-breaking excluded)
        assert_eq!(report.summary.breaking_api_changes, 1);
        assert_eq!(report.summary.breaking_behavioral_changes, 0);
        assert_eq!(report.summary.total_breaking_changes, 1);
        assert_eq!(report.summary.files_with_breaking_changes, 1);
        // Non-breaking changes excluded from output
        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].breaking_api_changes.len(), 1);
    }

    #[test]
    fn build_report_with_behavioral_changes() {
        use semver_analyzer_core::BehavioralChangeKind;

        let behavioral = vec![BehavioralChange {
            symbol: "createUser".into(),
            kind: BehavioralChangeKind::Function,
            category: None,
            description: "Email normalization now strips + aliases".into(),
            source_file: Some("src/api/users.ts".into()),
        }];

        let report = build_report(
            Path::new("/tmp/repo"),
            "v1",
            "v2",
            vec![],
            behavioral,
            vec![],
            vec![],
        );
        assert_eq!(report.summary.breaking_api_changes, 0);
        assert_eq!(report.summary.breaking_behavioral_changes, 1);
        assert_eq!(report.summary.total_breaking_changes, 1);
        assert_eq!(report.summary.files_with_breaking_changes, 1);
    }

    #[test]
    fn display_symbol_simple() {
        assert_eq!(
            qualified_name_to_display_symbol("test.greet", "greet"),
            "greet"
        );
    }

    #[test]
    fn display_symbol_component_prop() {
        // CardProps.isFlat → Card.isFlat
        assert_eq!(
            qualified_name_to_display_symbol(
                "packages/react-core/dist/esm/components/Card/Card.CardProps.isFlat",
                "isFlat"
            ),
            "Card.isFlat"
        );
    }

    #[test]
    fn display_symbol_top_level() {
        assert_eq!(
            qualified_name_to_display_symbol(
                "packages/react-core/dist/esm/components/Card/Card.Card",
                "Card"
            ),
            "Card"
        );
    }

    #[test]
    fn display_symbol_non_props_interface() {
        // AccordionContent.isHidden (not a Props interface)
        assert_eq!(
            qualified_name_to_display_symbol(
                "packages/react-core/dist/esm/components/Accordion/AccordionContent.AccordionContent.isHidden",
                "isHidden"
            ),
            "AccordionContent.isHidden"
        );
    }

    #[test]
    fn display_symbol_interface_member() {
        assert_eq!(
            qualified_name_to_display_symbol(
                "packages/react-core/dist/esm/components/Button/Button.ButtonProps.variant",
                "variant"
            ),
            "Button.variant"
        );
    }
}
