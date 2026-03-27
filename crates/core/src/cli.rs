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
    #[arg(long)]
    pub log_file: Option<PathBuf>,

    /// Log level filter (trace, debug, info, warn, error).
    /// Controls file output verbosity. Stderr progress display is always shown.
    #[arg(long, default_value = "info")]
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

    /// Skip LLM-based behavioral analysis (static analysis only).
    #[arg(long)]
    pub no_llm: bool,

    /// Command to invoke for LLM analysis.
    /// The prompt is passed as the final argument.
    /// Examples:
    ///   --llm-command "goose run --no-session -q -t"
    ///   --llm-command "opencode run"
    #[arg(long)]
    pub llm_command: Option<String>,

    /// Maximum LLM cost in USD before circuit breaker triggers.
    #[arg(long, default_value = "5.0")]
    pub max_llm_cost: f64,

    /// Send ALL files with changed exported functions to the LLM,
    /// not just files that have associated test changes.
    #[arg(long)]
    pub llm_all_files: bool,
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

    /// Skip LLM-based behavioral analysis (static analysis only).
    /// Only used when running analysis internally (--repo mode).
    #[arg(long)]
    pub no_llm: bool,

    /// Command to invoke for LLM analysis.
    #[arg(long)]
    pub llm_command: Option<String>,

    /// Maximum LLM cost in USD before circuit breaker triggers.
    #[arg(long, default_value = "5.0")]
    pub max_llm_cost: f64,

    /// Send ALL files with changed exported functions to the LLM.
    #[arg(long)]
    pub llm_all_files: bool,

    /// Disable rule consolidation (keep one rule per declaration change).
    #[arg(long)]
    pub no_consolidate: bool,

    /// Path to a YAML file with regex-based rename patterns.
    #[arg(long)]
    pub rename_patterns: Option<PathBuf>,
}
