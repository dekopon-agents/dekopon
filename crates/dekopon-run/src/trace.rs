use std::{
    fs::File,
    io::{self, BufWriter},
    path::{Path, PathBuf},
    time::Duration,
};

use dekopon_broker_protocol::TraceParent;
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
const TRACE_FILTER: &str =
    "dekopon_run=trace,dekopon_model=trace,dekopon_provider_host=trace,dekopon_shell=trace";
/// Transport crates are silenced explicitly: an OTLP exporter that logs through `tracing` would
/// feed its own export failures back into itself.
const OTEL_LOG_FILTER: &str = "dekopon_run=info,dekopon_model=info,dekopon_provider_host=info,dekopon_shell=info,hyper=off,h2=off,opentelemetry=off,reqwest=off";

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
    // Lifecycle audit events target `dekopon_run::audit` so they reach the OTLP and Chrome sinks
    // (whose `dekopon_run` directives match the prefix) without ever printing on the operator's
    // stderr, which must stay byte-for-byte what it was before those events existed.
    let stderr_layer = fmt::layer()
        .with_ansi(!no_color)
        .with_target(verbosity > 1)
        .with_writer(io::stderr)
        .with_filter(EnvFilter::new(format!("{level},dekopon_run::audit=off")));

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

    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err(TraceError::Configuration(
            "OTLP endpoint must not be empty".to_owned(),
        ));
    }
    // The signal path is appended as text, so a query or fragment would end up behind it:
    // `http://host/api/default?org=x` becomes `...?org=x/v1/traces`, which is a valid URI that
    // silently posts to the wrong place. Reject it here rather than let it surface as a 404.
    if let Some(index) = endpoint.find(['?', '#']) {
        return Err(TraceError::Configuration(format!(
            "OTLP endpoint must be a base URL without a query or fragment; found {:?} at byte {index}",
            &endpoint[index..index + 1]
        )));
    }
    if shutdown_timeout.is_zero() {
        return Err(TraceError::Configuration(
            "OTLP export timeout must be greater than zero".to_owned(),
        ));
    }
    let service_name = telemetry.otel_service_name.trim();
    if service_name.is_empty() {
        return Err(TraceError::Configuration(
            "OpenTelemetry service name must not be empty".to_owned(),
        ));
    }

    // Process state rather than a parameter: it describes the sink's retention scope, not the call.
    dekopon_core::set_telemetry_payloads(telemetry.otel_telemetry_payloads);

    let settings = ExporterSettings::new(
        endpoint,
        telemetry.otlp_transport,
        service_name,
        "dekopon-run",
        shutdown_timeout,
    )?;

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
        let _ = logger_provider.shutdown_with_timeout(shutdown_timeout);
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

/// Trace context to send with a broker proposal, if this process is exporting one.
///
/// `None` is the ordinary state when export is disabled: the broker then records its own root
/// span rather than a child of a trace nothing will ever receive.
pub(crate) fn current_trace_parent() -> Option<TraceParent> {
    let parts = dekopon_telemetry::current_trace_context()?;
    // A context the SDK considers valid can still be rejected here (all-zero identifiers), and a
    // malformed parent is worse than none: it would attach broker spans to a trace that does not
    // exist. Dropping it degrades correlation instead of corrupting it.
    TraceParent::new(parts.trace_id, parts.span_id, parts.flags).ok()
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
    use super::{TraceError, current_trace_parent, initialize};
    use crate::cli::{TelemetryArgs, Transport};

    /// Outside an exporting span there is no context to send, and the runner must not invent one.
    #[test]
    fn trace_parent_is_absent_without_an_active_exporting_span() {
        assert!(current_trace_parent().is_none());
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
