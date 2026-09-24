#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
#[cfg(unix)]
use dekopond::cli;
#[cfg(unix)]
mod auth;
#[cfg(unix)]
mod auth_output;
#[cfg(unix)]
mod auth_render;
#[cfg(unix)]
mod auth_result;

#[cfg(unix)]
use std::{future::Future, io, process::ExitCode, time::Duration};

#[cfg(unix)]
use clap::Parser as _;
#[cfg(unix)]
use dekopon_core::error_chain;
#[cfg(unix)]
use dekopon_telemetry::{Console, ConsoleFilter, ConsoleFormat, ConsoleWriter, Install};
#[cfg(unix)]
use dekopond::cli::Cli;
#[cfg(unix)]
use thiserror::Error;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

#[cfg(unix)]
const OTEL_TRACE_FILTER: &str = "dekopond=trace,dekopon_agent=trace,dekopon_process=trace,dekopon_shell=trace,dekopon_model=trace,hyper=off,h2=off,reqwest=off,tungstenite=off,tokio_tungstenite=off";

/// This bounds exit separately from the shutdown grace, since cancelling a session doesn't stop
/// non-preemptible blocking work already in flight; anything still running past this timeout is
/// left to die with the process.
#[cfg(unix)]
const BLOCKING_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(unix)]
fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(cli::Command::Auth(options)) = &cli.command {
        return if auth_output::run(options) == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    let Some(config) = cli.config else {
        eprintln!("dekopond: --config is required for serving");
        return ExitCode::from(2);
    };
    match bounded_runtime(BLOCKING_EXIT_TIMEOUT, serve(config)) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("dekopond: could not start the async runtime: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(unix)]
fn bounded_runtime<T>(
    exit_timeout: Duration,
    body: impl Future<Output = T>,
) -> Result<T, io::Error> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let value = runtime.block_on(body);
    runtime.shutdown_timeout(exit_timeout);
    Ok(value)
}

#[cfg(unix)]
async fn serve(config: std::path::PathBuf) -> ExitCode {
    let settings = dekopond::telemetry_settings(&config, dekopond::current_uid())
        .await
        .ok()
        .flatten();

    let tracer_provider = dekopon_telemetry::optional_tracer_provider(
        settings.as_ref().map(|telemetry| &telemetry.settings),
        "dekopond",
    );

    let mut install = Install::new(Console {
        format: ConsoleFormat::Json,
        writer: ConsoleWriter::Stdout,
        filter: ConsoleFilter::Environment("info".to_owned()),
    });
    if let Some(provider) = tracer_provider {
        install = install.with_traces(provider, "dekopond", OTEL_TRACE_FILTER);
    }
    let telemetry = match install.install() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("dekopond: could not install tracing subscriber: {error}");
            return ExitCode::FAILURE;
        }
    };

    let code = match execute(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(event = "gateway_exit", error = %error_chain(&error));
            ExitCode::FAILURE
        }
    };

    if let Err(error) = telemetry.shutdown() {
        tracing::error!(event = "gateway_telemetry_shutdown_failed", error = %error);
    }
    code
}

#[cfg(unix)]
async fn execute(config: std::path::PathBuf) -> Result<(), AppError> {
    let mut terminate = signal(SignalKind::terminate()).map_err(AppError::Signal)?;
    let shutdown = async move {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if result.is_err() {
                    tracing::error!(event = "gateway_signal_failed", signal = "interrupt");
                }
            }
            _ = terminate.recv() => {}
        }
    };
    dekopond::run(config, shutdown)
        .await
        .map_err(AppError::Gateway)?;
    Ok(())
}

#[cfg(unix)]
#[derive(Debug, Error)]
enum AppError {
    #[error("could not install termination signal handler")]
    Signal(#[source] io::Error),
    #[error("gateway service failed")]
    Gateway(#[source] dekopond::DekopondError),
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::{Duration, Instant};

    use clap::CommandFactory as _;
    use dekopond::cli::Cli;

    #[test]
    fn cli_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn exit_does_not_wait_for_blocking_work_that_outlived_its_session() {
        let started = Instant::now();
        let (running, is_running) = tokio::sync::oneshot::channel();
        let value = super::bounded_runtime(Duration::from_millis(50), async move {
            tokio::task::spawn_blocking(move || {
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "the receiver is awaited on the next line and its expect is the real \
                              assertion; a dropped receiver fails the test there, not here"
                )]
                let _ = running.send(());
                std::thread::sleep(Duration::from_secs(10));
            });
            is_running.await.expect("the blocking half is running");
            "served"
        })
        .expect("the runtime builds");

        assert_eq!(value, "served");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "exit waited on abandoned blocking work: {:?}",
            started.elapsed()
        );
    }
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("dekopond requires Unix peer credentials and Unix-domain sockets");
    std::process::ExitCode::FAILURE
}
