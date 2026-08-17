//! Command-line surface: one flag, because everything else belongs in reviewed configuration.

use std::path::PathBuf;

use clap::Parser;

/// Parsed `dekopond` invocation.
#[derive(Debug, Parser)]
#[command(
    name = "dekopond",
    version,
    about = "Run the unprivileged Dekopon chat gateway and agent daemon"
)]
pub struct Cli {
    /// Strict owner-controlled gateway YAML/JSON configuration.
    #[arg(long, value_name = "PATH")]
    pub config: PathBuf,
}
