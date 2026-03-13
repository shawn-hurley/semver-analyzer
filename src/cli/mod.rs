//! CLI argument parsing and command dispatch.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Semantic Breaking Change Analyzer
///
/// Deterministic, structured analysis of breaking changes between two git refs.
/// Combines static API surface extraction with optional LLM-based behavioral analysis.
#[derive(Parser, Debug)]
#[command(name = "semver-analyzer", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Extract API surface from source code at a specific ref.
    Extract {
        /// Path to the git repository.
        #[arg(long)]
        repo: PathBuf,

        /// Git ref (tag, branch, or commit SHA) to extract from.
        #[arg(long, name = "ref")]
        git_ref: String,

        /// Output file path (writes JSON). Defaults to stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Custom build command to run instead of `tsc --declaration`.
        /// Use for projects that require custom generation steps before tsc.
        /// Example: --build-command "yarn build"
        #[arg(long)]
        build_command: Option<String>,
    },

    /// Compare two API surfaces and identify structural changes.
    Diff {
        /// Path to the "from" API surface JSON file.
        #[arg(long)]
        from: PathBuf,

        /// Path to the "to" API surface JSON file.
        #[arg(long)]
        to: PathBuf,

        /// Output file path (writes JSON). Defaults to stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Full pipeline: extract -> diff -> impact -> behavioral analysis.
    Analyze {
        /// Path to the git repository.
        #[arg(long)]
        repo: PathBuf,

        /// Git ref to compare from (the "old" version).
        #[arg(long)]
        from: String,

        /// Git ref to compare to (the "new" version).
        #[arg(long)]
        to: String,

        /// Output file path (writes JSON). Defaults to stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Skip LLM-based behavioral analysis (static analysis only).
        #[arg(long)]
        no_llm: bool,

        /// Command to invoke for LLM analysis.
        /// The prompt is passed as the final argument.
        /// Examples:
        ///   --llm-command "goose run --no-session -q -t"
        ///   --llm-command "opencode run"
        #[arg(long)]
        llm_command: Option<String>,

        /// Maximum LLM cost in USD before circuit breaker triggers.
        #[arg(long, default_value = "5.0")]
        max_llm_cost: f64,

        /// Custom build command to run instead of `tsc --declaration`.
        /// Use for projects that require custom generation steps before tsc.
        /// Example: --build-command "yarn build"
        #[arg(long)]
        build_command: Option<String>,

        /// Send ALL files with changed exported functions to the LLM,
        /// not just files that have associated test changes. By default,
        /// only files whose tests also changed are sent to the LLM
        /// (much faster and cheaper).
        #[arg(long)]
        llm_all_files: bool,
    },

    /// Start as an MCP server (stdio transport).
    Serve,
}
