use std::{
    fs::File,
    io::{self, BufWriter},
    path::{Path, PathBuf},
    time::Duration,
};

use dekopon_telemetry::{ExporterSettings, TelemetryError};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::{logs::SdkLoggerProvider, trace::SdkTracerProvider};
use thiserror::Error;
use tracing_chrome::{ChromeLayerBuilder, FlushGuard};
use tracing_subscriber::{
    EnvFilter, Layer, fmt, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

use crate::cli::TelemetryArgs;

/// Crates whose execution spans and lifecycle events belong in runner telemetry.
const TRACE_FILTER: &str = "dekopon_run=trace,dekopon_agent=trace,dekopon_model=trace,dekopon_provider_host=trace,dekopon_shell=trace";
/// Transport crates are silenced explicitly: an OTLP exporter that logs through `tracing` would
/// feed its own export failures back into itself.
const OTEL_LOG_FILTER: &str = "dekopon_run=info,dekopon_agent=info,dekopon_model=info,dekopon_provider_host=info,dekopon_shell=info,hyper=off,h2=off,opentelemetry=off,reqwest=off";

pub(crate) struct TraceGuard {
    chrome: Option<FlushGuard>,
    logger_provider: Option<SdkLoggerProvider>,
    tracer_provider: Option<SdkTracerProvider>,
    shutdown_timeout: Duration,
}

impl TraceGuard {
    /// Flushes every configured exporter before the short-lived runner exits.
    ///
    /// A configured OTLP endpoint is treated as part of the command contract: a flush failure is
    /// returned to the caller so a successful guest execution is not silently reported as fully
    /// observed.
    pub(crate) fn shutdown(mut self) -> Result<(), TraceError> {
        // Dropping the Chrome guard flushes its buffered writer. Do this while the tracing
        // subscriber and the OpenTelemetry providers are still alive.
        drop(self.chrome.take());

        let mut failures = Vec::new();
        if let Some(provider) = self.logger_provider.take() {
            if let Err(error) = provider.force_flush() {
                failures.push(format!("logs flush: {error}"));
            }
            if let Err(error) = provider.shutdown_with_timeout(self.shutdown_timeout) {
                failures.push(format!("logs shutdown: {error}"));
            }
        }
        if let Some(provider) = self.tracer_provider.take() {
            if let Err(error) = provider.force_flush() {
                failures.push(format!("traces flush: {error}"));
            }
            if let Err(error) = provider.shutdown_with_timeout(self.shutdown_timeout) {
                failures.push(format!("traces shutdown: {error}"));
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(TraceError::Shutdown(failures.join("; ")))
        }
    }
}

pub(crate) fn initialize(
    verbosity: u8,
    no_color: bool,
    chrome_trace: Option<&Path>,
    telemetry: &TelemetryArgs,
) -> Result<TraceGuard, TraceError> {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    // Lifecycle audit events target `dekopon_run::audit` and `dekopon_agent::audit` so they reach
    // the OTLP and Chrome sinks (whose crate directives match the prefix) without ever printing on
    // the operator's stderr, which must stay byte-for-byte what it was before those events
    // existed.
    let stderr_layer = fmt::layer()
        .with_ansi(!no_color)
        .with_target(verbosity > 1)
        .with_writer(io::stderr)
        .with_filter(EnvFilter::new(format!(
            "{level},dekopon_run::audit=off,dekopon_agent::audit=off"
        )));

    let (chrome_layer, chrome_guard) = if let Some(path) = chrome_trace {
        let file = File::create(path).map_err(|source| TraceError::Create {
            path: path.to_path_buf(),
            source,
        })?;
        let (layer, guard) = ChromeLayerBuilder::new()
            .writer(BufWriter::new(file))
            .include_args(true)
            .include_locations(true)
            .build();
        (
            Some(layer.with_filter(EnvFilter::new(TRACE_FILTER))),
            Some(guard),
        )
    } else {
        (None, None)
    };

    let shutdown_timeout = Duration::from_millis(telemetry.otel_export_timeout_ms);

    // Process state rather than a parameter: it describes the sink's retention scope, not the call.
    // Applied before the endpoint check because `--trace` is a sink too — a local Chrome file is
    // exactly as in scope for payloads as a remote receiver, and a flag that silently did nothing
    // without an endpoint would be the same "configures nothing" failure the broker guard refuses.
    dekopon_core::set_telemetry_payloads(telemetry.otel_telemetry_payloads);

    // No endpoint means no exporter, no provider, and no extra layer: the subscriber built here is
    // exactly the one a build without this feature would install. Telemetry settings are not even
    // validated on this path — with export disabled they configure nothing.
    let Some(endpoint) = telemetry.otlp_endpoint.as_deref() else {
        tracing_subscriber::registry()
            .with(stderr_layer)
            .with(chrome_layer)
            .try_init()
            .map_err(|error| TraceError::Subscriber(error.to_string()))?;
        return Ok(TraceGuard {
            chrome: chrome_guard,
            logger_provider: None,
            tracer_provider: None,
            shutdown_timeout,
        });
    };

    // Endpoint, service name, and timeout are validated by the exporter crate rather than here:
    // one copy of that policy means one place a new rule has to be added.
    let settings = ExporterSettings::new(
        endpoint,
        telemetry.otlp_transport,
        &telemetry.otel_service_name,
        "dekopon-run",
        env!("CARGO_PKG_VERSION"),
        shutdown_timeout,
    )
    .map_err(|error| match error {
        TelemetryError::Configuration(message) => TraceError::Configuration(message),
        other => TraceError::Telemetry(other),
    })?;

    let tracer_provider = settings.tracer_provider()?;
    let tracer = tracer_provider.tracer("dekopon-run");
    let otel_trace_layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(EnvFilter::new(TRACE_FILTER));

    let logger_provider = settings.logger_provider()?;
    let otel_log_layer = OpenTelemetryTracingBridge::new(&logger_provider)
        .with_filter(EnvFilter::new(OTEL_LOG_FILTER));

    if let Err(error) = tracing_subscriber::registry()
        .with(stderr_layer)
        .with(chrome_layer)
        // Install the span layer before the log bridge so entered tracing spans activate an
        // OpenTelemetry context that the log SDK can use for trace/span correlation.
        .with(otel_trace_layer)
        .with(otel_log_layer)
        .try_init()
    {
        // Best-effort teardown of two providers that never received a span: `try_init` just
        // failed, so nothing was exported and there is no installed subscriber for a shutdown
        // diagnostic to reach anyway.
        #[allow(
            clippy::let_underscore_must_use,
            reason = "rollback of providers that exported nothing; the returned Subscriber error \
                      is the failure, and no subscriber is installed to carry a second one"
        )]
        let _ = logger_provider.shutdown_with_timeout(shutdown_timeout);
        #[allow(
            clippy::let_underscore_must_use,
            reason = "rollback of providers that exported nothing; the returned Subscriber error \
                      is the failure, and no subscriber is installed to carry a second one"
        )]
        let _ = tracer_provider.shutdown_with_timeout(shutdown_timeout);
        return Err(TraceError::Subscriber(error.to_string()));
    }

    Ok(TraceGuard {
        chrome: chrome_guard,
        logger_provider: Some(logger_provider),
        tracer_provider: Some(tracer_provider),
        shutdown_timeout,
    })
}

#[derive(Debug, Error)]
pub(crate) enum TraceError {
    #[error("could not create trace file {}", path.display())]
    Create {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid telemetry configuration: {0}")]
    Configuration(String),
    #[error(transparent)]
    Telemetry(#[from] TelemetryError),
    #[error("could not install tracing subscriber: {0}")]
    Subscriber(String),
    #[error("could not flush OTLP telemetry: {0}")]
    Shutdown(String),
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::{layer::Context, registry};

    use super::{EnvFilter, Layer as _, OTEL_LOG_FILTER, TRACE_FILTER, TraceError, initialize};
    use crate::cli::{TelemetryArgs, Transport};
    use tracing_subscriber::layer::SubscriberExt as _;

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

    /// Neither OTLP layer may see the exporter's own diagnostics. `internal-logs` is enabled
    /// workspace-wide, so a layer that accepted them would export the failures of its own export.
    /// The SDK crates log under their package names, hyphens and all, which is why the log filter
    /// silences the `opentelemetry` prefix rather than an exact target.
    #[test]
    fn the_otlp_layers_never_see_the_exporters_own_records() {
        for filter in [TRACE_FILTER, OTEL_LOG_FILTER] {
            let recorded = RecordTargets::default();
            let subscriber = registry().with(recorded.clone().with_filter(EnvFilter::new(filter)));

            tracing::subscriber::with_default(subscriber, || {
                tracing::error!(target: "opentelemetry", "api diagnostic");
                tracing::error!(target: "opentelemetry-sdk", "sdk diagnostic");
                tracing::error!(target: "opentelemetry-otlp", "exporter diagnostic");
                tracing::info!(target: "dekopon_run", "runner event");
            });

            assert_eq!(
                *recorded.0.lock().expect("target log"),
                vec!["dekopon_run".to_owned()],
                "{filter}"
            );
        }
    }

    /// A query or fragment would sit in front of the appended signal path, producing a URI that
    /// parses and posts to the wrong place. Failing at configuration time names the real problem.
    #[test]
    fn endpoint_with_query_or_fragment_is_rejected() {
        for endpoint in [
            "http://openobserve:5080/api/default?org=x",
            "http://openobserve:5080/api/default#frag",
        ] {
            let telemetry = TelemetryArgs {
                otlp_endpoint: Some(endpoint.to_owned()),
                otlp_transport: Transport::Http,
                otel_telemetry_payloads: false,
                otel_service_name: "dekopon-run".to_owned(),
                otel_export_timeout_ms: 5_000,
            };
            let error = initialize(0, true, None, &telemetry)
                .err()
                .expect("endpoint with query or fragment is rejected");
            assert!(
                matches!(&error, TraceError::Configuration(message) if message.contains("query or fragment")),
                "unexpected error for {endpoint}: {error}"
            );
        }
    }
}
