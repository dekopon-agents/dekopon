#[cfg(unix)]
use std::{io, path::PathBuf, process::ExitCode};

#[cfg(unix)]
use clap::Parser;
#[cfg(unix)]
use thiserror::Error;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
#[cfg(unix)]
use tracing_subscriber::EnvFilter;

#[cfg(unix)]
#[derive(Debug, Parser)]
#[command(
    name = "dekopon-brokerd",
    version,
    about = "Run the authenticated local Dekopon capability broker"
)]
struct Cli {
    /// Strict owner-controlled broker YAML/JSON configuration.
    #[arg(long, value_name = "PATH")]
    config: PathBuf,
}

#[cfg(unix)]
#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    match execute(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(event = "broker_exit", error = %error);
            ExitCode::FAILURE
        }
    }
}

#[cfg(unix)]
async fn execute(cli: Cli) -> Result<(), AppError> {
    let mut terminate = signal(SignalKind::terminate()).map_err(AppError::Signal)?;
    let shutdown = async move {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if result.is_err() {
                    tracing::error!(event = "broker_signal_failed", signal = "interrupt");
                }
            }
            _ = terminate.recv() => {}
        }
    };
    dekopon_brokerd::run(cli.config, shutdown)
        .await
        .map_err(AppError::Broker)?;
    Ok(())
}

#[cfg(unix)]
#[derive(Debug, Error)]
enum AppError {
    #[error("could not install termination signal handler")]
    Signal(#[source] io::Error),
    #[error("broker service failed")]
    Broker(#[source] dekopon_brokerd::BrokerdError),
}

#[cfg(all(test, unix))]
mod tests {
    use clap::CommandFactory as _;

    use super::Cli;

    #[test]
    fn cli_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("dekopon-brokerd requires Unix peer credentials and Unix-domain sockets");
    std::process::ExitCode::FAILURE
}
