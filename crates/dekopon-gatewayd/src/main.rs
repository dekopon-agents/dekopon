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
use clap::{CommandFactory as _, Parser as _, error::ErrorKind};
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
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => return dekopon_core::render_cli_error(error, Cli::command()),
    };
    if let Some(env_file) = &cli.env_file {
        match &cli.command {
            None => {}
            Some(cli::Command::Auth(_) | cli::Command::Check(_)) => {
                eprintln!("dekopon-gatewayd: --env-file is accepted only when serving");
                return ExitCode::from(2);
            }
        }
        let error = dekopon_core::env_file::reexec(env_file, dekopon_gatewayd::current_uid());
        eprintln!("dekopon-gatewayd: {}", error_chain(&error));
        return ExitCode::FAILURE;
    }
    let builder = match runtime_builder(&cli) {
        Ok(builder) => builder,
        Err(error) => return dekopon_core::render_cli_error(error, Cli::command()),
    };
    match &cli.command {
        Some(cli::Command::Auth(options)) => {
            return match bounded_runtime(builder, BLOCKING_EXIT_TIMEOUT, auth_output::run(options))
            {
                Ok(0) => ExitCode::SUCCESS,
                Ok(_) => ExitCode::FAILURE,
                Err(error) => {
                    eprintln!("dekopon-gatewayd: could not start the async runtime: {error}");
                    ExitCode::FAILURE
                }
            };
        }
        Some(cli::Command::Check(check)) => {
            if cli.config.is_some() {
                eprintln!("dekopon-gatewayd: --config cannot be used with check");
                return ExitCode::from(2);
            }
            return match bounded_runtime(builder, BLOCKING_EXIT_TIMEOUT, run_check(check)) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("dekopon-gatewayd: could not start the async runtime: {error}");
                    ExitCode::FAILURE
                }
            };
        }
        None => {}
    }
    let Some(config) = cli.config.as_ref() else {
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
    match bounded_runtime(
        builder,
        BLOCKING_EXIT_TIMEOUT,
        serve(config, cli.shell_threads.get()),
    ) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("dekopon-gatewayd: could not start the async runtime: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(unix)]
const MAX_RUNTIME_THREADS: usize = 4096;

#[cfg(unix)]
#[expect(
    clippy::disallowed_methods,
    reason = "owner: bounded_runtime shuts it down within exit_timeout; bound: one runtime, configured workers and blocking cap or Tokio defaults"
)]
fn runtime_builder(cli: &Cli) -> Result<tokio::runtime::Builder, clap::Error> {
    let workers = match cli.worker_threads {
        Some(workers) => workers.get(),
        None => match std::env::var("TOKIO_WORKER_THREADS") {
            Ok(value) => value
                .parse::<std::num::NonZeroUsize>()
                .map_err(|error| {
                    Cli::command().error(
                        ErrorKind::ValueValidation,
                        format!("invalid TOKIO_WORKER_THREADS: {error}"),
                    )
                })?
                .get(),
            Err(std::env::VarError::NotPresent) => {
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
            }
            Err(error) => {
                return Err(Cli::command().error(ErrorKind::ValueValidation, error));
            }
        },
    };
    let blocking = cli.blocking_threads.map_or(8, std::num::NonZeroUsize::get);
    if workers > MAX_RUNTIME_THREADS || blocking > MAX_RUNTIME_THREADS {
        return Err(Cli::command().error(
            ErrorKind::ValueValidation,
            format!("worker and blocking thread counts must each be at most {MAX_RUNTIME_THREADS}"),
        ));
    }
    if workers.checked_add(blocking).is_none() {
        return Err(Cli::command().error(
            ErrorKind::ValueValidation,
            "worker threads plus blocking threads exceed the platform thread-count limit",
        ));
    }

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    let next = std::sync::atomic::AtomicUsize::new(0);
    builder.enable_all().thread_name_fn(move || {
        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 100_000_000;
        format!("gw-rt-{index}")
    });
    builder
        .worker_threads(workers)
        .max_blocking_threads(blocking);
    if let Some(keep_alive) = cli.blocking_keep_alive {
        builder.thread_keep_alive(keep_alive.into());
    }
    Ok(builder)
}

#[cfg(unix)]
fn bounded_runtime<T>(
    mut builder: tokio::runtime::Builder,
    exit_timeout: Duration,
    body: impl Future<Output = T>,
) -> Result<T, io::Error> {
    let runtime = builder.build()?;
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
    let scope = check.probe.then(|| match &check.against {
        Some(against) => dekopon_gatewayd::ProbeScope::ChangedFrom(against.clone()),
        None => dekopon_gatewayd::ProbeScope::Every,
    });
    let report =
        dekopon_gatewayd::check(&check.config, check.catalog.as_deref(), scope.as_ref()).await;
    let output = CheckOutput {
        ok: report.problems.is_empty(),
        problems: report
            .problems
            .iter()
            .map(|problem| error_chain(problem))
            .collect(),
        warnings: report
            .warnings
            .iter()
            .map(|warning| error_chain(warning))
            .collect(),
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
async fn serve(config: &std::path::Path, shell_threads: u32) -> ExitCode {
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
        writer: ConsoleWriter::Stdout("gw-stdout"),
        filter: ConsoleFilter::Environment("info".to_owned()),
    });
    if let Some(provider) = tracer_provider {
        install = install.with_traces(
            provider,
            "dekopon-gatewayd",
            dekopon_gatewayd::OTEL_TRACE_FILTER,
        );
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

    let code = match execute(config, shell_threads).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(event = "gateway_exit", error = %error_chain(&error), "gateway.exit");
            ExitCode::FAILURE
        }
    };

    match tokio::task::spawn_blocking(move || telemetry.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(
            event = "gateway_telemetry_shutdown_failed",
            error = %error,
            "gateway.telemetry.shutdown.failed"
        ),
        Err(error) => eprintln!("dekopon-gatewayd: telemetry flush failed: {error}"),
    }
    code
}

#[cfg(unix)]
async fn execute(config: &std::path::Path, shell_threads: u32) -> Result<(), AppError> {
    let mut terminate = signal(SignalKind::terminate()).map_err(AppError::Signal)?;
    let shutdown = async move {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if result.is_err() {
                    tracing::error!(
                        event = "gateway_signal_failed",
                        signal = "interrupt",
                        "gateway.signal.failed"
                    );
                }
            }
            _ = terminate.recv() => {}
        }
    };
    dekopon_gatewayd::run(config, shutdown, shell_threads)
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
    Gateway(#[source] dekopon_gatewayd::GatewaydError),
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::{Duration, Instant};

    use clap::{CommandFactory as _, Parser as _};
    use dekopon_gatewayd::cli::Cli;

    #[test]
    fn cli_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn exit_does_not_wait_for_blocking_work_that_outlived_its_session() {
        let started = Instant::now();
        let (running, is_running) = tokio::sync::oneshot::channel();
        let builder =
            super::runtime_builder(&Cli::parse_from(["gateway", "--config=gateway.yaml"])).unwrap();
        let value = super::bounded_runtime(builder, Duration::from_millis(50), async move {
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

#[cfg(all(test, unix))]
mod runtime_sizing_tests {
    use super::*;
    use std::{collections::BTreeSet, process::Command, time::Duration};

    #[test]
    fn flags_and_environment() {
        const CHILD: &str = "DEKOPON_RUNTIME_SIZING_TEST";
        let variables = [
            "DEKOPON_WORKER_THREADS",
            "DEKOPON_BLOCKING_THREADS",
            "DEKOPON_BLOCKING_KEEP_ALIVE",
            "DEKOPON_SHELL_THREADS",
        ];
        let mode = std::env::var(CHILD).ok();
        let Some(mode) = mode else {
            for case in ["defaults", "environment", "0", "1", "2", "3"] {
                let mut child = Command::new(std::env::current_exe().unwrap());
                child
                    .args(["--exact", "runtime_sizing_tests::flags_and_environment"])
                    .env(CHILD, case);
                for (variable, value) in variables.iter().zip(["2", "3", "250ms", "4"]) {
                    if case == "defaults" {
                        child.env_remove(variable);
                    } else {
                        child.env(variable, value);
                    }
                }
                if let Ok(index) = case.parse::<usize>() {
                    child.env(variables[index], if index == 2 { "invalid" } else { "0" });
                }
                let output = child.output().unwrap();
                assert!(
                    output.status.success(),
                    "{case}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        };
        if mode.parse::<usize>().is_ok() {
            let error = Cli::try_parse_from(["daemon", "--config=unused.yaml"]).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        } else {
            let cli = Cli::try_parse_from(["daemon", "--config=unused.yaml"]).unwrap();
            if mode == "defaults" {
                assert_eq!(cli.worker_threads, None);
                assert_eq!(cli.blocking_threads, None);
                assert_eq!(cli.blocking_keep_alive, None);
                assert_eq!(Some(cli.shell_threads.get() as usize), Some(32));
            } else {
                assert_eq!(cli.worker_threads.unwrap().get(), 2);
                assert_eq!(cli.blocking_threads.unwrap().get(), 3);
                assert_eq!(
                    Duration::from(cli.blocking_keep_alive.unwrap()),
                    Duration::from_millis(250)
                );
                assert_eq!(Some(cli.shell_threads.get() as usize), Some(4));
            }
            for argument in [
                "--worker-threads=0",
                "--blocking-threads=0",
                "--blocking-keep-alive=invalid",
                "--shell-threads=0",
            ] {
                let error =
                    Cli::try_parse_from(["daemon", "--config=unused.yaml", argument]).unwrap_err();
                assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
            }
        }
        let cli = Cli::try_parse_from([
            "daemon",
            "--config=unused.yaml",
            "--worker-threads=3",
            "--blocking-threads=4",
            "--blocking-keep-alive=500ms",
            "--shell-threads=5",
        ])
        .unwrap();
        assert_eq!(cli.worker_threads.unwrap().get(), 3);
        assert_eq!(cli.blocking_threads.unwrap().get(), 4);
        assert_eq!(
            Duration::from(cli.blocking_keep_alive.unwrap()),
            Duration::from_millis(500)
        );
        assert_eq!(Some(cli.shell_threads.get() as usize), Some(5));
    }

    #[test]
    fn runtime_thread_limits_are_cli_errors() {
        const CHILD: &str = "DEKOPON_RUNTIME_LIMIT_TEST";
        let arguments = ["daemon", "--config=unused.yaml"];
        if std::env::var_os(CHILD).is_some() {
            let cli = Cli::try_parse_from(arguments).unwrap();
            let error = runtime_builder(&cli).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ValueValidation);
            assert_eq!(error.exit_code(), 2);
            return;
        }
        let invalid_counts = [usize::MAX, usize::MAX - 512, MAX_RUNTIME_THREADS + 1];
        for count in invalid_counts {
            for flag in ["--worker-threads", "--blocking-threads"] {
                let cli = Cli::try_parse_from(
                    arguments
                        .into_iter()
                        .map(str::to_owned)
                        .chain([format!("{flag}={count}")]),
                )
                .unwrap();
                let error = runtime_builder(&cli).unwrap_err();
                assert_eq!(error.kind(), ErrorKind::ValueValidation);
                assert_eq!(error.exit_code(), 2);
            }
        }
        let variables = [
            "DEKOPON_WORKER_THREADS",
            "DEKOPON_BLOCKING_THREADS",
            "TOKIO_WORKER_THREADS",
        ];
        for count in invalid_counts {
            for variable in variables {
                let mut child = Command::new(std::env::current_exe().unwrap());
                child.args([
                    "--exact",
                    "runtime_sizing_tests::runtime_thread_limits_are_cli_errors",
                ]);
                for variable in variables {
                    child.env_remove(variable);
                }
                let output = child
                    .env(CHILD, "1")
                    .env(variable, count.to_string())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{variable}={count}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }

    #[test]
    fn configured_runtime_names_workers_and_blocking_threads() {
        let cli = Cli::parse_from([
            "daemon",
            "--config=unused.yaml",
            "--worker-threads=2",
            "--blocking-threads=1",
        ]);
        let runtime = runtime_builder(&cli).unwrap().build().unwrap();
        assert_eq!(runtime.metrics().num_workers(), 2);
        let workers = runtime.block_on(async {
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..2 {
                let barrier = std::sync::Arc::clone(&barrier);
                tasks.spawn(async move {
                    let name = std::thread::current().name().unwrap().to_owned();
                    barrier.wait();
                    name
                });
            }
            let mut names = BTreeSet::new();
            while let Some(name) = tasks.join_next().await {
                names.insert(name.unwrap());
            }
            names
        });
        assert_eq!(
            workers,
            BTreeSet::from(["gw-rt-0".to_owned(), "gw-rt-1".to_owned()])
        );
        let blocking = runtime.block_on(async {
            tokio::task::spawn_blocking(|| std::thread::current().name().unwrap().to_owned())
                .await
                .unwrap()
        });
        assert!(blocking.starts_with("gw-rt-"));
        assert!(blocking.len() <= 15);
        runtime.shutdown_timeout(Duration::from_secs(1));
    }
}
