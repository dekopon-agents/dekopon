use std::{
    fs::File,
    io::{self, BufWriter},
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use opentelemetry::{KeyValue, trace::TracerProvider as _};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use opentelemetry_otlp::{
    ExporterBuildError, LogExporter, Protocol, SpanExporter, WithExportConfig, WithHttpConfig,
};
use opentelemetry_sdk::{Resource, logs::SdkLoggerProvider, trace::SdkTracerProvider};
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

/// Adapter around the workspace's existing TLS-enabled reqwest client.
///
/// `opentelemetry-otlp` otherwise selects its own newer reqwest line, duplicating the HTTP/TLS
/// stack. Supplying the client also lets us reject redirects so an authorization header cannot be
/// forwarded to a receiver-selected destination.
#[derive(Clone, Debug)]
struct OtlpHttpClient(reqwest::blocking::Client);

impl OtlpHttpClient {
    fn new(timeout: Duration) -> Result<Self, TraceError> {
        // reqwest's blocking client owns a private runtime and refuses to create it from within
        // Dekopon's Tokio runtime. Build it on a plain thread, as the upstream OTLP adapter does.
        let client = std::thread::Builder::new()
            .name("dekopon-otlp-http-client".to_owned())
            .spawn(move || {
                reqwest::blocking::Client::builder()
                    .timeout(timeout)
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
            })
            .map_err(TraceError::HttpClientThread)?
            .join()
            .map_err(|_| TraceError::HttpClientThreadPanicked)?
            .map_err(TraceError::HttpClient)?;
        Ok(Self(client))
    }
}

#[async_trait]
impl HttpClient for OtlpHttpClient {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let request: reqwest::blocking::Request = request.try_into()?;
        let mut response = self.0.execute(request)?.error_for_status()?;
        let headers = std::mem::take(response.headers_mut());
        let status = response.status();
        let mut response = Response::builder().status(status).body(response.bytes()?)?;
        *response.headers_mut() = headers;
        Ok(response)
    }
}

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

    let resource = Resource::builder()
        .with_service_name(service_name.to_owned())
        .with_attributes([
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new("process.executable.name", "dekopon-run"),
        ])
        .build();

    // `--otlp-endpoint` follows the standard generic OTLP/HTTP endpoint contract. Signal paths
    // are appended here because passing a programmatic endpoint to the SDK makes it exact rather
    // than applying the environment variable's `/v1/<signal>` behavior.
    let traces_endpoint = signal_endpoint(endpoint, "traces");
    let logs_endpoint = signal_endpoint(endpoint, "logs");
    let http_client = OtlpHttpClient::new(shutdown_timeout)?;

    let span_exporter = SpanExporter::builder()
        .with_http()
        .with_http_client(http_client.clone())
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(traces_endpoint)
        .with_timeout(shutdown_timeout)
        .build()
        .map_err(|source| TraceError::Exporter {
            signal: "trace",
            source,
        })?;
    let tracer_provider = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_batch_exporter(span_exporter)
        .build();
    let tracer = tracer_provider.tracer("dekopon-run");
    let otel_trace_layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(EnvFilter::new(TRACE_FILTER));

    let log_exporter = LogExporter::builder()
        .with_http()
        .with_http_client(http_client)
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(logs_endpoint)
        .with_timeout(shutdown_timeout)
        .build()
        .map_err(|source| TraceError::Exporter {
            signal: "log",
            source,
        })?;
    let logger_provider = SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(log_exporter)
        .build();
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

fn signal_endpoint(base: &str, signal: &str) -> String {
    format!("{}/v1/{signal}", base.trim_end_matches('/'))
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
    #[error("could not start OTLP HTTP client builder")]
    HttpClientThread(#[source] io::Error),
    #[error("OTLP HTTP client builder panicked")]
    HttpClientThreadPanicked,
    #[error("could not build OTLP HTTP client")]
    HttpClient(#[source] reqwest::Error),
    #[error("could not build OTLP {signal} exporter")]
    Exporter {
        signal: &'static str,
        #[source]
        source: ExporterBuildError,
    },
    #[error("could not install tracing subscriber: {0}")]
    Subscriber(String),
    #[error("could not flush OTLP telemetry: {0}")]
    Shutdown(String),
}

#[cfg(test)]
mod tests {
    use super::{TraceError, initialize, signal_endpoint};
    use crate::cli::TelemetryArgs;

    #[test]
    fn generic_otlp_http_endpoint_gets_signal_paths() {
        assert_eq!(
            signal_endpoint("http://openobserve:5080/api/default", "traces"),
            "http://openobserve:5080/api/default/v1/traces"
        );
        assert_eq!(
            signal_endpoint("http://openobserve:5080/api/default/", "logs"),
            "http://openobserve:5080/api/default/v1/logs"
        );
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
