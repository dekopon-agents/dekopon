//! Implementation of the `dekopon` operator CLI.
//!
//! The execution pipeline is intentionally explicit:
//!
//! `parse CLI -> discover config -> load typed catalog -> execute typed command -> render`.

#![forbid(unsafe_code)]

use std::{
    error::Error as _,
    io::{self, Write},
};

use dekopon_config::{ConfigError, load_discovered};
use thiserror::Error;
use tracing_subscriber::EnvFilter;

use crate::{
    auth::AuthError,
    catalog::{CatalogError, LocalConfigReader},
    cli::{Cli, Command},
    command::{CatalogCommand, CommandResult, execute, version_result},
    render::{RenderError, render},
};

mod auth;
pub mod catalog;
pub mod cli;
mod command;
mod render;

/// Runs a parsed CLI invocation and returns a documented process exit code.
///
/// Clap handles syntax errors before this function and exits with code `2`.
#[must_use]
pub fn run(cli: Cli) -> i32 {
    initialize_tracing(cli.verbose, cli.no_color);

    match evaluate(&cli) {
        Ok(output) => {
            if cli.quiet {
                return 0;
            }
            match write_output(&output) {
                Ok(()) => 0,
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => 0,
                Err(error) => {
                    eprintln!("error: could not write output: {error}");
                    1
                }
            }
        }
        Err(error) => {
            report_error(&error, cli.verbose);
            error.exit_code()
        }
    }
}

fn evaluate(cli: &Cli) -> Result<String, AppError> {
    let Some(command) = &cli.command else {
        // Refused here rather than by Clap's own required-subcommand error, so the message names
        // what was missing in this CLI's own words. `docs/cli.md` documents the exit code and
        // `tests/cli.rs` pins it.
        return Err(AppError::MissingSubcommand);
    };
    let result = match command {
        Command::Version => version_result(),
        Command::Auth { account } => auth::execute(account)?,
        Command::Get { resource } => with_catalog(cli, CatalogCommand::Get(resource))?,
        Command::Describe { resource } => with_catalog(cli, CatalogCommand::Describe(resource))?,
        Command::Validate => with_catalog(cli, CatalogCommand::Validate)?,
        Command::Config { command } => with_catalog(cli, CatalogCommand::Config(command))?,
    };

    render(&result, cli.output).map_err(AppError::Render)
}

fn with_catalog(cli: &Cli, command: CatalogCommand<'_>) -> Result<CommandResult, AppError> {
    tracing::debug!(config = ?cli.config, "resolving local configuration");
    let catalog =
        load_discovered(cli.config.clone()).map_err(|error| AppError::Config(Box::new(error)))?;
    tracing::debug!(source = %catalog.source().display(), "loaded validated catalog");
    let reader = LocalConfigReader::new(catalog);
    Ok(execute(command, &reader)?)
}

fn write_output(output: &str) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(output.as_bytes())?;
    if !output.ends_with('\n') {
        handle.write_all(b"\n")?;
    }
    handle.flush()
}

fn report_error(error: &AppError, verbosity: u8) {
    eprintln!("error: {error}");

    if verbosity > 0 {
        let mut source = error.source();
        while let Some(cause) = source {
            eprintln!("  caused by: {cause}");
            source = cause.source();
        }
    }
    if verbosity > 1 {
        eprintln!("  debug: {error:#?}");
    }
}

fn initialize_tracing(verbosity: u8, no_color: bool) {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(level))
        .with_ansi(!no_color)
        .with_target(verbosity > 1)
        .without_time();
    let _subscriber_result = builder.with_writer(io::stderr).try_init();
}

#[derive(Debug, Error)]
enum AppError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// A bare invocation naming no operation.
    ///
    /// Exit code 2, matching what Clap emits for any other usage error.
    #[error("a subcommand is required")]
    MissingSubcommand,
    #[error(transparent)]
    Config(Box<ConfigError>),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Render(#[from] RenderError),
}

impl AppError {
    const fn exit_code(&self) -> i32 {
        match self {
            Self::MissingSubcommand => 2,
            Self::Catalog(CatalogError::NotFound { .. }) => 3,
            Self::Auth(_) | Self::Config(_) | Self::Render(_) => 1,
        }
    }
}
