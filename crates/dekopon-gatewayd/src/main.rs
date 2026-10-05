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
use dekopon_gatewayd::cli;
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
use dekopon_gatewayd::cli::Cli;
#[cfg(unix)]
use dekopon_telemetry::{Console, ConsoleFilter, ConsoleFormat, ConsoleWriter, Install};
#[cfg(unix)]
use thiserror::Error;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

/// Category targets run at the standard level.
#[cfg(unix)]
const OTEL_LOG_FILTER: &str = "job=debug,meter=info";

#[cfg(unix)]
const OTEL_TRACE_FILTER: &str = "dekopon_gatewayd=trace,dekopon_agent=trace,dekopon_process=trace,dekopon_shell=trace,dekopon_model=trace,gateway=debug,prompt=debug,model=debug,asset=debug,shell=debug,job=debug,broker=debug,provider=debug,http=debug,credential=debug,memory=debug,telemetry=debug,hyper=off,h2=off,reqwest=off,tungstenite=off,tokio_tungstenite=off";

/// This bounds exit separately from the shutdown grace, since cancelling a session doesn't stop
/// non-preemptible blocking work already in flight; anything still running past this timeout is
/// left to die with the process.
#[cfg(unix)]
const BLOCKING_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(unix)]
fn main() -> ExitCode {
    if let Some(code) = dekopon_shell::run_jq_worker_if_requested() {
        return code;
    }
    let cli = Cli::parse();
    match &cli.command {
        Some(cli::Command::Auth(options)) => {
            return if auth_output::run(options) == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Some(cli::Command::Check(check)) => {
            if cli.config.is_some() {
                eprintln!("dekopon-gatewayd: --config cannot be used with check");
                return ExitCode::from(2);
            }
            return match bounded_runtime(BLOCKING_EXIT_TIMEOUT, run_check(check)) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("dekopon-gatewayd: could not start the async runtime: {error}");
                    ExitCode::FAILURE
                }
            };
        }
        None => {}
    }
    let Some(config) = cli.config else {
        eprintln!("dekopon-gatewayd: --config is required for serving");
        return ExitCode::from(2);
    };
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("dekopon-gatewayd: could not locate jq worker executable: {error}");
            return ExitCode::FAILURE;
        }
    };
    if dekopon_shell::set_jq_worker_executable(executable).is_err() {
        eprintln!("dekopon-gatewayd: jq worker executable already supplied");
        return ExitCode::FAILURE;
    }
    match bounded_runtime(BLOCKING_EXIT_TIMEOUT, serve(config)) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("dekopon-gatewayd: could not start the async runtime: {error}");
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
#[derive(serde::Serialize)]
struct CheckOutput {
    ok: bool,
    problems: Vec<String>,
    warnings: Vec<String>,
}

#[cfg(unix)]
async fn run_check(check: &cli::CheckArgs) -> ExitCode {
    let report = dekopon_gatewayd::check(&check.config, check.catalog.as_deref()).await;
    let output = CheckOutput {
        ok: report.problems.is_empty(),
        problems: report
            .problems
            .iter()
            .map(|problem| error_chain(problem))
            .collect(),
        warnings: report.warnings.iter().map(ToString::to_string).collect(),
    };
    match check.output {
        cli::CheckFormat::Table => {
            for problem in &output.problems {
                println!("problem\t{problem}");
            }
            for warning in &output.warnings {
                println!("warning\t{warning}");
            }
            if output.ok {
                println!("ok");
            }
        }
        cli::CheckFormat::Json => match serde_json::to_string_pretty(&output) {
            Ok(json) => println!("{json}"),
            Err(error) => {
                eprintln!("dekopon-gatewayd: could not render check output: {error}");
                return ExitCode::FAILURE;
            }
        },
    }
    if output.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(unix)]
async fn serve(config: std::path::PathBuf) -> ExitCode {
    let settings = dekopon_gatewayd::telemetry_settings(&config, dekopon_gatewayd::current_uid())
        .await
        .ok()
        .flatten();

    let tracer_provider = dekopon_telemetry::optional_tracer_provider(
        settings.as_ref().map(|telemetry| &telemetry.settings),
        "dekopon-gatewayd",
    );

    let logger_provider = dekopon_telemetry::optional_logger_provider(
        settings.as_ref().map(|telemetry| &telemetry.settings),
        "dekopon-gatewayd",
    );
    let mut install = Install::new(Console {
        format: ConsoleFormat::Json,
        writer: ConsoleWriter::Stdout,
        filter: ConsoleFilter::Environment("info".to_owned()),
    });
    if let Some(provider) = tracer_provider {
        install = install.with_traces(provider, "dekopon-gatewayd", OTEL_TRACE_FILTER);
    }
    if let Some(provider) = logger_provider {
        install = install.with_logs(provider, OTEL_LOG_FILTER);
    }
    let telemetry = match install.install() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("dekopon-gatewayd: could not install tracing subscriber: {error}");
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
    dekopon_gatewayd::run(config, shutdown)
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
    Gateway(#[source] dekopon_gatewayd::DekopondError),
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::{Duration, Instant};

    use clap::CommandFactory as _;
    use dekopon_gatewayd::cli::Cli;

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
    eprintln!("dekopon-gatewayd requires Unix peer credentials and Unix-domain sockets");
    std::process::ExitCode::FAILURE
}
