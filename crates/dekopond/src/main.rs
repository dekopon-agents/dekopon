#[cfg(unix)]
use std::{future::Future, io, process::ExitCode, time::Duration};

#[cfg(unix)]
use clap::Parser as _;
#[cfg(unix)]
use dekopond::cli::Cli;
#[cfg(unix)]
use opentelemetry::trace::TracerProvider as _;
#[cfg(unix)]
use thiserror::Error;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
#[cfg(unix)]
use tracing_subscriber::{
    EnvFilter, Layer as _, fmt, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

/// Transport crates are silenced explicitly: an OTLP exporter that logs through `tracing` would
/// feed its own export failures back into itself, and a WebSocket library logs every frame.
#[cfg(unix)]
const OTEL_TRACE_FILTER: &str = "dekopond=trace,dekopon_agent=trace,dekopon_shell=trace,dekopon_model=trace,hyper=off,h2=off,opentelemetry=off,reqwest=off,tungstenite=off,tokio_tungstenite=off";

/// How long exit may still wait on blocking session work after everything else has stopped.
///
/// The gateway's shutdown grace is the deadline for a session to *finish*; abandoning one only
/// cancels the async owner's await on its blocking half. The synchronous prompt loop keeps running
/// until it observes cancellation at its next cooperative checkpoint, which can be on the far side
/// of a whole synchronous model round trip. Dropping a Tokio runtime waits for every one of those
/// threads, so the process would exit `shutdownGraceMs` *plus* a model timeout after the signal —
/// past a pod termination grace that the broker's own drain also has to fit inside. Here the wait
/// is bounded and the remaining threads are left to die with the process; a model request already
/// in flight was never rollbackable, and waiting for it does not make it so.
#[cfg(unix)]
const BLOCKING_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(unix)]
fn main() -> ExitCode {
    let cli = Cli::parse();
    match bounded_runtime(BLOCKING_EXIT_TIMEOUT, serve(cli)) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("dekopond: could not start the async runtime: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs `body` on an owned runtime whose teardown is bounded rather than unbounded.
///
/// `#[tokio::main]` drops its runtime, and that drop blocks until every blocking task returns.
/// Owning the runtime is what makes [`tokio::runtime::Runtime::shutdown_timeout`] — the one exit
/// bound Tokio offers — reachable at all.
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
async fn serve(cli: Cli) -> ExitCode {
    // Read the export settings before serving. A failure here is discarded rather than reported:
    // `run` parses the same file and surfaces every configuration error with full context, so
    // reporting it twice would only make the first message the confusing one.
    let settings = dekopond::telemetry_settings(&cli.config, dekopond::current_uid())
        .await
        .ok()
        .flatten();

    let tracer_provider = match settings
        .as_ref()
        .map(|telemetry| telemetry.settings.tracer_provider())
    {
        Some(Ok(provider)) => Some(provider),
        // Telemetry must never keep the gateway from starting. Answering messages under bounded
        // authority is the service's contract; observability is not.
        Some(Err(error)) => {
            eprintln!("dekopond: telemetry disabled: {error}");
            None
        }
        None => None,
    };

    // Structured JSON on stdout is the log contract, so a collector or shipper can pick it up
    // without the daemon holding a second credential.
    let stdout_layer = fmt::layer()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_writer(io::stdout)
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));

    let otel_layer = tracer_provider.as_ref().map(|provider| {
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("dekopond"))
            .with_filter(EnvFilter::new(OTEL_TRACE_FILTER))
    });

    if tracing_subscriber::registry()
        .with(stdout_layer)
        .with(otel_layer)
        .try_init()
        .is_err()
    {
        eprintln!("dekopond: could not install tracing subscriber");
        return ExitCode::FAILURE;
    }

    let code = match execute(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The whole source chain, not just the top Display: "gateway service failed"
            // without its cause sends an operator source-diving for what one log line
            // could have said.
            tracing::error!(
                event = "gateway_exit",
                error = %error,
                cause = %error_chain(&error)
            );
            ExitCode::FAILURE
        }
    };

    if let Some(provider) = tracer_provider {
        // Flush failures are reported but do not change the exit code: the broker's durable audit,
        // not this daemon's telemetry, is the record of what happened.
        if let Err(error) = provider.force_flush() {
            tracing::error!(event = "gateway_telemetry_flush_failed", error = %error);
        }
        if let Err(error) = provider.shutdown() {
            tracing::error!(event = "gateway_telemetry_shutdown_failed", error = %error);
        }
    }
    code
}

#[cfg(unix)]
async fn execute(cli: Cli) -> Result<(), AppError> {
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
    dekopond::run(cli.config, shutdown)
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

/// Renders an error's source chain as one `a: b: c` line for the exit log.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut rendered = String::new();
    let mut source = error.source();
    while let Some(current) = source {
        if !rendered.is_empty() {
            rendered.push_str(": ");
        }
        rendered.push_str(&current.to_string());
        source = current.source();
    }
    if rendered.is_empty() {
        rendered.push_str("no further detail");
    }
    rendered
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use clap::CommandFactory as _;
    use dekopond::cli::Cli;
    use tracing_subscriber::{
        EnvFilter, Layer as _, layer::Context, layer::SubscriberExt as _, registry,
    };

    use super::OTEL_TRACE_FILTER;

    #[test]
    fn cli_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    /// Records the target of every event a layer is actually asked to handle.
    #[derive(Clone, Default)]
    struct RecordTargets(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RecordTargets {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            self.0
                .lock()
                .expect("target log")
                .push(event.metadata().target().to_owned());
        }
    }

    /// The OTLP layer must never see the exporter's own diagnostics. `internal-logs` is enabled
    /// workspace-wide, so a layer that accepted them would export the failures of its own export.
    /// The SDK crates log under their package names, hyphens and all, which is why the directive
    /// has to be the `opentelemetry` prefix rather than an exact target.
    #[test]
    fn the_otlp_layer_never_sees_the_exporters_own_records() {
        let recorded = RecordTargets::default();
        let subscriber = registry().with(
            recorded
                .clone()
                .with_filter(EnvFilter::new(OTEL_TRACE_FILTER)),
        );

        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "opentelemetry", "api diagnostic");
            tracing::error!(target: "opentelemetry-sdk", "sdk diagnostic");
            tracing::error!(target: "opentelemetry-otlp", "exporter diagnostic");
            tracing::info!(target: "dekopond", "gateway event");
        });

        assert_eq!(
            *recorded.0.lock().expect("target log"),
            vec!["dekopond".to_owned()]
        );
    }

    #[test]
    fn exit_does_not_wait_for_blocking_work_that_outlived_its_session() {
        // The shape of an abandoned session: the async owner is gone, its synchronous half is
        // parked inside a model call, and nothing can interrupt it. Exit must not inherit that
        // wait — the pod's termination grace is shared with the broker's own drain, and
        // overshooting it turns a clean stop into SIGKILL mid-drain.
        let started = Instant::now();
        let (running, is_running) = tokio::sync::oneshot::channel();
        let value = super::bounded_runtime(Duration::from_millis(50), async move {
            tokio::task::spawn_blocking(move || {
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
