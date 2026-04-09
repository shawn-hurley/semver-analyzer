//! Shared CLI argument structs for the semver-analyzer.
//!
//! These structs define the common flags shared across all language
//! implementations. Language crates use `#[command(flatten)]` to include
//! them and add their own language-specific flags.

use clap::Args;
use std::path::PathBuf;

/// Shared logging/tracing arguments available to all commands.
///
/// Language crates and command structs flatten this to get consistent
/// `--log-file` and `--log-level` flags.
#[derive(Args, Debug, Clone)]
pub struct LoggingArgs {
    /// Path to a log file for debug/trace output.
    /// When set, all tracing events at the configured level are written here.
    #[arg(long, help_heading = "Logging")]
    pub log_file: Option<PathBuf>,

    /// Log level filter (trace, debug, info, warn, error).
    /// Controls file output verbosity. Stderr progress display is always shown.
    #[arg(long, default_value = "info", help_heading = "Logging")]
    pub log_level: String,
}

/// Common arguments for the `analyze` command.
///
/// Language crates flatten this into their own `XxxAnalyzeArgs` struct
/// and add language-specific flags (e.g., `--build-command` for TypeScript).
#[derive(Args, Debug, Clone)]
pub struct CommonAnalyzeArgs {
    #[command(flatten)]
    pub logging: LoggingArgs,

    /// Path to the git repository.
    #[arg(long)]
    pub repo: PathBuf,

    /// Git ref to compare from (the "old" version).
    #[arg(long)]
    pub from: String,

    /// Git ref to compare to (the "new" version).
    #[arg(long)]
    pub to: String,

    /// Output file path (writes JSON). Defaults to stdout.
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Use the behavioral analysis (BU) pipeline instead of the default
    /// source-level diff (SD) pipeline.
    ///
    /// The BU pipeline uses test-delta heuristics and optional LLM inference
    /// to detect behavioral breaking changes. The default SD pipeline produces
    /// deterministic, AST-based source-level change facts.
    #[arg(long, help_heading = "Pipeline")]
    pub behavioral: bool,

    /// Backwards-compatible alias for the default pipeline (no-op).
    /// The SD pipeline is now the default; this flag is accepted but ignored.
    #[arg(long, hide = true)]
    pub pipeline_v2: bool,

    /// Skip LLM-based behavioral analysis (static analysis only).
    #[arg(long, help_heading = "LLM Options")]
    pub no_llm: bool,

    /// Command to invoke for LLM analysis.
    /// The prompt is passed as the final argument.
    #[arg(long, help_heading = "LLM Options")]
    pub llm_command: Option<String>,

    /// Send ALL files with changed exported functions to the LLM,
    /// not just files that have associated test changes.
    /// Only used with --behavioral pipeline.
    #[arg(long, requires = "behavioral", help_heading = "LLM Options")]
    pub llm_all_files: bool,

    /// Timeout in seconds for each LLM invocation.
    #[arg(long, default_value = "120", help_heading = "LLM Options")]
    pub llm_timeout: u64,
}

/// Common arguments for the `extract` command.
#[derive(Args, Debug, Clone)]
pub struct CommonExtractArgs {
    #[command(flatten)]
    pub logging: LoggingArgs,

    /// Path to the git repository.
    #[arg(long)]
    pub repo: PathBuf,

    /// Git ref (tag, branch, or commit SHA) to extract from.
    #[arg(long, name = "ref")]
    pub git_ref: String,

    /// Output file path (writes JSON). Defaults to stdout.
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

/// Arguments for the `diff` command (language-agnostic).
#[derive(Args, Debug, Clone)]
pub struct DiffArgs {
    #[command(flatten)]
    pub logging: LoggingArgs,

    /// Path to the "from" API surface JSON file.
    #[arg(long)]
    pub from: PathBuf,

    /// Path to the "to" API surface JSON file.
    #[arg(long)]
    pub to: PathBuf,

    /// Output file path (writes JSON). Defaults to stdout.
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

/// Common arguments for the `konveyor` command.
#[derive(Args, Debug, Clone)]
pub struct CommonKonveyorArgs {
    #[command(flatten)]
    pub logging: LoggingArgs,

    /// Path to a pre-existing AnalysisReport JSON file.
    /// Mutually exclusive with --repo/--from/--to.
    #[arg(long, conflicts_with_all = ["repo", "from", "to"])]
    pub from_report: Option<PathBuf>,

    /// Path to the git repository (runs full analysis pipeline).
    #[arg(long, required_unless_present = "from_report")]
    pub repo: Option<PathBuf>,

    /// Git ref to compare from (the "old" version).
    #[arg(long, required_unless_present = "from_report")]
    pub from: Option<String>,

    /// Git ref to compare to (the "new" version).
    #[arg(long, required_unless_present = "from_report")]
    pub to: Option<String>,

    /// Output directory for the generated ruleset.
    #[arg(long)]
    pub output_dir: PathBuf,

    /// Use the behavioral analysis (BU) pipeline instead of the default
    /// source-level diff (SD) pipeline. Only used in --repo mode.
    #[arg(long, help_heading = "Pipeline")]
    pub behavioral: bool,

    /// Backwards-compatible alias for the default pipeline (no-op).
    #[arg(long, hide = true)]
    pub pipeline_v2: bool,

    /// Skip LLM-based behavioral analysis (static analysis only).
    /// Only used when running analysis internally (--repo mode).
    #[arg(long, help_heading = "LLM Options")]
    pub no_llm: bool,

    /// Command to invoke for LLM analysis.
    #[arg(long, help_heading = "LLM Options")]
    pub llm_command: Option<String>,

    /// Send ALL files with changed exported functions to the LLM.
    /// Only used with --behavioral pipeline.
    #[arg(long, requires = "behavioral", help_heading = "LLM Options")]
    pub llm_all_files: bool,

    /// Timeout in seconds for each LLM invocation.
    #[arg(long, default_value = "120", help_heading = "LLM Options")]
    pub llm_timeout: u64,

    /// Disable rule consolidation (keep one rule per declaration change).
    #[arg(long, help_heading = "Rule Generation")]
    pub no_consolidate: bool,

    /// Path to a YAML file with regex-based rename patterns.
    #[arg(long, help_heading = "Rule Generation")]
    pub rename_patterns: Option<PathBuf>,
}
