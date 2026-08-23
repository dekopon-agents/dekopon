//! Implementation of the `dekopon` operator CLI.
//!
//! The execution pipeline is intentionally explicit:
//!
//! `parse CLI -> discover config -> load typed catalog -> execute typed command -> render`.

#![forbid(unsafe_code)]

use std::{
    error::Error as _,
    io::{self, IsTerminal as _, Write},
};

use dekopon_config::{ConfigError, load_discovered};
use thiserror::Error;
use tracing_subscriber::EnvFilter;

#[cfg(unix)]
use crate::console::ConsoleError;
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
#[cfg(unix)]
mod console;
mod render;

/// Runs a parsed CLI invocation and returns a documented process exit code.
///
/// Clap handles syntax errors before this function and exits with code `2`.
#[must_use]
pub fn run(cli: Cli) -> i32 {
    initialize_tracing(
        cli.verbose,
        cli.no_color,
        diagnostics_would_land_on_the_screen(&cli),
    );

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
        // A bare invocation on a terminal is the console. Anywhere else it stays the usage error
        // `docs/cli.md` documents and `tests/cli.rs` pins, because a script that piped `dekopon`
        // asked for output, and a full-screen console would hang waiting for a key that never
        // comes.
        return console_or_usage(cli);
    };
    let result = match command {
        Command::Version => version_result(),
        Command::Auth { account } => auth::execute(account)?,
        Command::Get { resource } => with_catalog(cli, CatalogCommand::Get(resource))?,
        Command::Describe { resource } => with_catalog(cli, CatalogCommand::Describe(resource))?,
        Command::Validate => with_catalog(cli, CatalogCommand::Validate)?,
        Command::Config { command } => with_catalog(cli, CatalogCommand::Config(command))?,
        #[cfg(unix)]
        Command::Console(args) => {
            console::execute(cli, args).map_err(|error| AppError::Console(Box::new(error)))?;
            return Ok(String::new());
        }
    };

    render(&result, cli.output).map_err(AppError::Render)
}

/// Opens the console for a bare interactive invocation, or reports the usage error.
#[cfg(unix)]
fn console_or_usage(cli: &Cli) -> Result<String, AppError> {
    if !console::is_interactive() {
        return Err(AppError::MissingSubcommand);
    }
    let args = crate::cli::ConsoleArgs::interactive_default();
    console::execute(cli, &args).map_err(|error| AppError::Console(Box::new(error)))?;
    Ok(String::new())
}

#[cfg(not(unix))]
fn console_or_usage(_cli: &Cli) -> Result<String, AppError> {
    Err(AppError::MissingSubcommand)
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

/// Whether this invocation will draw a full-screen console over the same terminal diagnostics
/// would be written to.
///
/// Standard error is the console's problem, not just standard output: the alternate screen is the
/// terminal, so a `tracing` line written there lands inside a frame and stays until something
/// happens to overdraw it. Redirecting stderr is the way to keep diagnostics — `dekopon console -vv
/// 2> console.log` works exactly as it reads.
#[cfg(unix)]
fn diagnostics_would_land_on_the_screen(cli: &Cli) -> bool {
    let opens_console = match &cli.command {
        Some(Command::Console(_)) => true,
        None => console::is_interactive(),
        Some(_) => false,
    };
    opens_console && io::stderr().is_terminal()
}

#[cfg(not(unix))]
const fn diagnostics_would_land_on_the_screen(_cli: &Cli) -> bool {
    false
}

fn initialize_tracing(verbosity: u8, no_color: bool, discard: bool) {
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
    let _subscriber_result = if discard {
        builder.with_writer(io::sink).try_init()
    } else {
        builder.with_writer(io::stderr).try_init()
    };
}

#[derive(Debug, Error)]
enum AppError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// The console refused before it drew anything, or the terminal failed under it.
    #[cfg(unix)]
    #[error(transparent)]
    Console(Box<ConsoleError>),
    /// A bare invocation that is not a terminal.
    ///
    /// Exit code 2, matching what Clap emitted for the same invocation before the console existed.
    #[error("a subcommand is required when standard input and output are not a terminal")]
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
            #[cfg(unix)]
            Self::Console(_) => 1,
            Self::Auth(_) | Self::Config(_) | Self::Render(_) => 1,
        }
    }
}
