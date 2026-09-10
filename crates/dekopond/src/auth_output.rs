//! Synchronous auth diagnostics and stdout writer, never gateway telemetry.
use crate::{
    auth::{self, AuthError},
    auth_render::{RenderError, render},
    cli::AuthOptions,
};
use std::{
    error::Error as _,
    io::{self, Write},
};
use thiserror::Error;
use tracing_subscriber::EnvFilter;
/// Runs a parsed CLI invocation and returns a documented process exit code.
///
/// Clap handles syntax errors before this function and exits with code `2`.
#[must_use]
pub(crate) fn run(cli: &AuthOptions) -> i32 {
    initialize_tracing(cli.verbose, cli.no_color);

    match evaluate(cli) {
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
            1
        }
    }
}

fn evaluate(cli: &AuthOptions) -> Result<String, AppError> {
    let result = auth::execute(&cli.account)?;
    render(&result, cli.output).map_err(AppError::Render)
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
    // Serde's Display and derived Debug may reflect arbitrary credential values.
    // Project this typed failure before formatting any part of its error tree.
    if let AppError::Auth(AuthError::ChatGpt(dekopon_model::chatgpt::ChatGptError::ParseAuth {
        path,
        source,
    })) = error
    {
        eprintln!(
            "error: could not parse ChatGPT credentials at {}",
            path.display()
        );
        if verbosity > 0 {
            eprintln!(
                "  caused by: credential JSON {:?} at line {} column {}",
                source.classify(),
                source.line(),
                source.column()
            );
            if let Some(kind) = source.io_error_kind() {
                eprintln!("  I/O kind: {kind:?}");
            }
        }
        if verbosity > 1 {
            eprintln!("  debug: ChatGpt::ParseAuth (credential values withheld)");
        }
        return;
    }
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
    if let Err(error) = builder.with_writer(io::stderr).try_init() {
        // A second installation in one process is a real event rather than nothing: the
        // subscriber that won owns the verbosity and the writer, so `--verbose` and `--no-color`
        // on this call did not take effect. The winner receives this record.
        tracing::debug!(
            event = "cli_tracing_already_installed",
            error = %error,
            "another tracing subscriber is already installed"
        );
    }
}

#[derive(Debug, Error)]
enum AppError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Render(#[from] RenderError),
}
