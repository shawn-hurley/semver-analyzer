//! CLI argument parsing and command dispatch.

use clap::{Parser, Subcommand};
use semver_analyzer_core::cli::{DiffArgs, LoggingArgs};
use semver_analyzer_java::cli::{JavaAnalyzeArgs, JavaExtractArgs, JavaKonveyorArgs};
use semver_analyzer_ts::cli::{TsAnalyzeArgs, TsExtractArgs, TsKonveyorArgs};

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

impl Cli {
    /// Extract `LoggingArgs` from whichever command variant is active.
    pub fn logging_args(&self) -> &LoggingArgs {
        match &self.command {
            Command::Analyze { language } => match language {
                AnalyzeLanguage::Typescript(args) => &args.common.logging,
                AnalyzeLanguage::Java(args) => &args.common.logging,
            },
            Command::Extract { language } => match language {
                ExtractLanguage::Typescript(args) => &args.common.logging,
                ExtractLanguage::Java(args) => &args.common.logging,
            },
            Command::Diff(args) => &args.logging,
            Command::Konveyor { language } => match language {
                KonveyorLanguage::Typescript(args) => &args.common.logging,
                KonveyorLanguage::Java(args) => &args.common.logging,
            },
            Command::Serve => {
                // Serve has no logging args yet; return a static default.
                static DEFAULT: std::sync::OnceLock<LoggingArgs> = std::sync::OnceLock::new();
                DEFAULT.get_or_init(|| LoggingArgs {
                    log_file: None,
                    log_level: "info".to_string(),
                })
            }
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Full pipeline: extract -> diff -> impact -> behavioral analysis.
    Analyze {
        #[command(subcommand)]
        language: AnalyzeLanguage,
    },

    /// Extract API surface from source code at a specific ref.
    Extract {
        #[command(subcommand)]
        language: ExtractLanguage,
    },

    /// Compare two API surfaces and identify structural changes.
    ///
    /// This command is language-agnostic — it compares two JSON surface
    /// files using minimal semantics (no language-specific rules).
    Diff(DiffArgs),

    /// Generate Konveyor analyzer rules from breaking change analysis.
    Konveyor {
        #[command(subcommand)]
        language: KonveyorLanguage,
    },

    /// Start as an MCP server (stdio transport).
    Serve,
}

/// Language-specific subcommands for the `analyze` action.
#[derive(Subcommand, Debug)]
pub enum AnalyzeLanguage {
    /// Analyze a TypeScript/JavaScript project.
    Typescript(TsAnalyzeArgs),
    /// Analyze a Java project.
    Java(JavaAnalyzeArgs),
}

/// Language-specific subcommands for the `extract` action.
#[derive(Subcommand, Debug)]
pub enum ExtractLanguage {
    /// Extract API surface from a TypeScript/JavaScript project.
    Typescript(TsExtractArgs),
    /// Extract API surface from a Java project.
    Java(JavaExtractArgs),
}

/// Language-specific subcommands for the `konveyor` action.
#[derive(Subcommand, Debug)]
pub enum KonveyorLanguage {
    /// Generate Konveyor rules for a TypeScript/JavaScript project.
    Typescript(TsKonveyorArgs),
    /// Generate Konveyor rules for a Java project.
    Java(JavaKonveyorArgs),
}
