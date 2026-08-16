#[cfg(unix)]
use std::{io, path::PathBuf, process::ExitCode};

#[cfg(unix)]
use clap::Parser;
#[cfg(unix)]
use dekopon_telemetry::ExporterSettings;
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

/// Transport crates are silenced explicitly: an OTLP exporter that logs through `tracing` would
/// feed its own export failures back into itself.
#[cfg(unix)]
const OTEL_TRACE_FILTER: &str = "dekopon_brokerd=trace,dekopon_broker=trace,dekopon_broker_host=trace,dekopon_http_host=trace,hyper=off,h2=off,opentelemetry=off,tonic=off,reqwest=off";

#[cfg(unix)]
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // Read the export settings before serving. A failure here is discarded rather than reported:
    // `run` parses the same file and surfaces every configuration error with full context, so
    // reporting it twice would only make the first message the confusing one.
    let settings = dekopon_brokerd::telemetry_settings(&cli.config, dekopon_brokerd::current_uid())
        .await
        .ok()
        .flatten();

    let tracer_provider = match settings.as_ref().map(ExporterSettings::tracer_provider) {
        Some(Ok(provider)) => Some(provider),
        // Telemetry must never keep the broker from starting. Authorization and audit are the
        // service's contract; observability is not, and failing closed here would trade a working
        // authority boundary for a missing dashboard.
        Some(Err(error)) => {
            eprintln!("dekopon-brokerd: telemetry disabled: {error}");
            None
        }
        None => None,
    };

    // Structured JSON on stdout is the log contract for now; a collector or shipper can pick it up
    // without the broker holding a second credential.
    let stdout_layer = fmt::layer()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_writer(io::stdout)
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));

    let otel_layer = tracer_provider.as_ref().map(|provider| {
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("dekopon-brokerd"))
            .with_filter(EnvFilter::new(OTEL_TRACE_FILTER))
    });

    if tracing_subscriber::registry()
        .with(stdout_layer)
        .with(otel_layer)
        .try_init()
        .is_err()
    {
        eprintln!("dekopon-brokerd: could not install tracing subscriber");
        return ExitCode::FAILURE;
    }

    let code = match execute(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(event = "broker_exit", error = %error);
            ExitCode::FAILURE
        }
    };

    if let Some(provider) = tracer_provider {
        // Flush failures are reported but do not change the exit code: the broker's durable audit,
        // not its telemetry, is the record of what happened.
        if let Err(error) = provider.force_flush() {
            tracing::error!(event = "broker_telemetry_flush_failed", error = %error);
        }
        if let Err(error) = provider.shutdown() {
            tracing::error!(event = "broker_telemetry_shutdown_failed", error = %error);
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
