//! TypeScript-specific CLI argument structs.
//!
//! Each struct flattens the shared `Common*Args` from core and adds
//! TypeScript-specific flags like `--build-command` and `--dep-repo`.

use clap::Args;
use semver_analyzer_core::cli::{CommonAnalyzeArgs, CommonExtractArgs, CommonKonveyorArgs};
use std::path::PathBuf;

/// TypeScript-specific arguments for the `analyze` command.
#[derive(Args, Debug)]
#[command(after_help = "\
EXAMPLES:
    # Analyze breaking changes between two tags
    semver-analyzer analyze typescript \\
        --repo ./my-lib --from v1.0.0 --to v2.0.0 -o report.json

    # With CSS dependency repo analysis
    semver-analyzer analyze typescript \\
        --repo ./my-lib --from v1.0.0 --to v2.0.0 \\
        --dep-repo ./my-css --dep-from v1.0.0 --dep-to v2.0.0 \\
        --dep-build-command 'npm install && npx gulp buildCSS'

    # Use behavioral pipeline with LLM
    semver-analyzer analyze typescript \\
        --repo ./my-lib --from v1.0.0 --to v2.0.0 \\
        --behavioral --llm-command 'goose run --no-session -q -t'")]
pub struct TsAnalyzeArgs {
    #[command(flatten)]
    pub common: CommonAnalyzeArgs,

    /// Custom build command to run before API extraction.
    /// If not set, the analyzer detects the package manager and runs tsc
    /// with monorepo-aware fallbacks (solution tsconfig, project build script).
    /// Used as the default for both refs unless overridden by
    /// --from-build-command / --to-build-command.
    #[arg(long, help_heading = "Build")]
    pub build_command: Option<String>,

    /// Build command for the "from" ref only (overrides --build-command).
    #[arg(long, help_heading = "Per-Ref Build")]
    pub from_build_command: Option<String>,

    /// Build command for the "to" ref only (overrides --build-command).
    #[arg(long, help_heading = "Per-Ref Build")]
    pub to_build_command: Option<String>,

    /// Node.js version for the "from" ref (e.g., "16", "18.19.0").
    /// Resolved via nvm. Requires nvm to be installed.
    #[arg(long, help_heading = "Per-Ref Build")]
    pub from_node_version: Option<String>,

    /// Node.js version for the "to" ref (e.g., "18", "20.11.0").
    /// Resolved via nvm. Requires nvm to be installed.
    #[arg(long, help_heading = "Per-Ref Build")]
    pub to_node_version: Option<String>,

    /// Install command for the "from" ref (e.g., "npm ci", "yarn install --frozen-lockfile").
    /// Overrides auto-detection from lockfiles.
    #[arg(long, help_heading = "Per-Ref Build")]
    pub from_install_command: Option<String>,

    /// Install command for the "to" ref (e.g., "npm ci", "yarn install --immutable").
    /// Overrides auto-detection from lockfiles.
    #[arg(long, help_heading = "Per-Ref Build")]
    pub to_install_command: Option<String>,

    /// Path to a dependency git repository (e.g., @patternfly/patternfly CSS repo).
    /// When provided, the SD pipeline extracts CSS profiles from this repo
    /// and uses them to enrich composition trees and generate CSS migration rules.
    #[arg(long, help_heading = "Dependency Repo")]
    pub dep_repo: Option<PathBuf>,

    /// Git ref for the "old" version of the dependency repo.
    #[arg(long, requires = "dep_repo", help_heading = "Dependency Repo")]
    pub dep_from: Option<String>,

    /// Git ref for the "new" version of the dependency repo.
    #[arg(long, requires = "dep_repo", help_heading = "Dependency Repo")]
    pub dep_to: Option<String>,

    /// Build command for the dependency repo (e.g., "npm install && npx gulp compileSASS").
    /// Runs in the worktree before CSS extraction.
    #[arg(long, requires = "dep_repo", help_heading = "Dependency Repo")]
    pub dep_build_command: Option<String>,
}

/// TypeScript-specific arguments for the `extract` command.
#[derive(Args, Debug)]
pub struct TsExtractArgs {
    #[command(flatten)]
    pub common: CommonExtractArgs,

    /// Custom build command to run before API extraction.
    /// If not set, the analyzer detects the package manager and runs tsc
    /// with monorepo-aware fallbacks (solution tsconfig, project build script).
    #[arg(long, help_heading = "Build")]
    pub build_command: Option<String>,

    /// Node.js version to use (e.g., "18", "18.19.0").
    /// Resolved via nvm. Requires nvm to be installed.
    #[arg(long, help_heading = "Build")]
    pub node_version: Option<String>,

    /// Install command override (e.g., "npm ci", "yarn install --frozen-lockfile").
    /// Overrides auto-detection from lockfiles.
    #[arg(long, help_heading = "Build")]
    pub install_command: Option<String>,
}

/// TypeScript-specific arguments for the `konveyor` command.
#[derive(Args, Debug)]
#[command(after_help = "\
EXAMPLES:
    # Generate rules from a pre-existing analysis report
    semver-analyzer konveyor typescript \\
        --from-report report.json --output-dir ./rules

    # Run full analysis and generate rules in one step
    semver-analyzer konveyor typescript \\
        --repo ./my-lib --from v1.0.0 --to v2.0.0 \\
        --output-dir ./rules

    # With custom rename patterns and CSS dependency
    semver-analyzer konveyor typescript \\
        --repo ./my-lib --from v1.0.0 --to v2.0.0 \\
        --output-dir ./rules \\
        --rename-patterns renames.yaml \\
        --dep-repo ./my-css --dep-from v1.0.0 --dep-to v2.0.0")]
pub struct TsKonveyorArgs {
    #[command(flatten)]
    pub common: CommonKonveyorArgs,

    /// Custom build command to run before API extraction.
    /// If not set, the analyzer detects the package manager and runs tsc
    /// with monorepo-aware fallbacks (solution tsconfig, project build script).
    /// Used as the default for both refs unless overridden by
    /// --from-build-command / --to-build-command.
    #[arg(long, help_heading = "Build")]
    pub build_command: Option<String>,

    /// Build command for the "from" ref only (overrides --build-command).
    #[arg(long, help_heading = "Per-Ref Build")]
    pub from_build_command: Option<String>,

    /// Build command for the "to" ref only (overrides --build-command).
    #[arg(long, help_heading = "Per-Ref Build")]
    pub to_build_command: Option<String>,

    /// Node.js version for the "from" ref (e.g., "16", "18.19.0").
    /// Resolved via nvm. Requires nvm to be installed.
    #[arg(long, help_heading = "Per-Ref Build")]
    pub from_node_version: Option<String>,

    /// Node.js version for the "to" ref (e.g., "18", "20.11.0").
    /// Resolved via nvm. Requires nvm to be installed.
    #[arg(long, help_heading = "Per-Ref Build")]
    pub to_node_version: Option<String>,

    /// Install command for the "from" ref (e.g., "npm ci", "yarn install --frozen-lockfile").
    /// Overrides auto-detection from lockfiles.
    #[arg(long, help_heading = "Per-Ref Build")]
    pub from_install_command: Option<String>,

    /// Install command for the "to" ref (e.g., "npm ci", "yarn install --immutable").
    /// Overrides auto-detection from lockfiles.
    #[arg(long, help_heading = "Per-Ref Build")]
    pub to_install_command: Option<String>,

    /// File glob pattern for filecontent rules.
    /// Determines which files Konveyor will scan for violations.
    #[arg(
        long,
        default_value = "*.{ts,tsx,js,jsx,mjs,cjs}",
        help_heading = "Rule Generation"
    )]
    pub file_pattern: String,

    /// Name for the generated ruleset.
    #[arg(
        long,
        default_value = "semver-breaking-changes",
        help_heading = "Rule Generation"
    )]
    pub ruleset_name: String,

    /// Path to a dependency git repository (e.g., @patternfly/patternfly CSS repo).
    /// When provided, the SD pipeline extracts CSS profiles from this repo
    /// and uses them to enrich composition trees and generate CSS migration rules.
    /// Only used in --repo mode.
    #[arg(long, help_heading = "Dependency Repo")]
    pub dep_repo: Option<PathBuf>,

    /// Git ref for the "old" version of the dependency repo.
    #[arg(long, requires = "dep_repo", help_heading = "Dependency Repo")]
    pub dep_from: Option<String>,

    /// Git ref for the "new" version of the dependency repo.
    #[arg(long, requires = "dep_repo", help_heading = "Dependency Repo")]
    pub dep_to: Option<String>,

    /// Build command for the dependency repo.
    /// Runs in the worktree before CSS extraction.
    #[arg(long, requires = "dep_repo", help_heading = "Dependency Repo")]
    pub dep_build_command: Option<String>,
}
