//! Java-specific CLI argument structs.

use clap::Args;
use semver_analyzer_core::cli::{CommonAnalyzeArgs, CommonExtractArgs, CommonKonveyorArgs};

/// Java-specific arguments for the `analyze` command.
#[derive(Args, Debug)]
pub struct JavaAnalyzeArgs {
    #[command(flatten)]
    pub common: CommonAnalyzeArgs,
}

/// Java-specific arguments for the `extract` command.
#[derive(Args, Debug)]
pub struct JavaExtractArgs {
    #[command(flatten)]
    pub common: CommonExtractArgs,
}

/// Java-specific arguments for the `konveyor` command.
#[derive(Args, Debug)]
pub struct JavaKonveyorArgs {
    #[command(flatten)]
    pub common: CommonKonveyorArgs,
}
