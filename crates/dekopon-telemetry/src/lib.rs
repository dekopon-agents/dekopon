//! Shared OTLP exporter construction and W3C trace context for Dekopon processes.
//!
//! Both the unprivileged runner and the privileged broker export their own spans, so exporter
//! construction lives here rather than in either binary. The crate deliberately depends on no
//! Dekopon crate: it must remain linkable from the runner without dragging broker code into the
//! runner's dependency tree, which CI rejects.
//!
//! # Authority
//!
//! This crate configures transport and never resolves credentials. Ingest authentication is read
//! by the OpenTelemetry SDK from the standard `OTEL_EXPORTER_OTLP_HEADERS` environment variable,
//! so a token is never accepted as a command-line argument, never written to a configuration file
//! this crate parses, and never attached to a span attribute or log field.

use std::{fmt, str::FromStr, time::Duration};

use async_trait::async_trait;
use opentelemetry::{
    Context, KeyValue,
    trace::{SpanContext, TraceContextExt as _, TraceFlags, TraceState},
};
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use opentelemetry_otlp::{
    ExporterBuildError, LogExporter, Protocol, SpanExporter, WithExportConfig, WithHttpConfig,
};
use opentelemetry_sdk::{Resource, logs::SdkLoggerProvider, trace::SdkTracerProvider};
use serde::Deserialize;
use thiserror::Error;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

/// Wire transport used to reach an OTLP receiver.
///
/// Both are first-class. A receiver reached through a path-routing reverse proxy generally wants
/// `Grpc`, whose method paths are fixed by the protobuf service definition; a receiver behind a
/// plain HTTP route wants `Http`, whose signal paths are appended to the configured base.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    /// OTLP over gRPC. The endpoint is an authority; method paths come from the OTLP service.
    Grpc,
    /// OTLP over HTTP with protobuf payloads. `/v1/traces` and `/v1/logs` are appended.
    #[default]
    Http,
}

impl Transport {
    /// Returns the stable lowercase token for this transport.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Grpc => "grpc",
            Self::Http => "http",
        }
    }
}

impl fmt::Display for Transport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Transport {
    type Err = TelemetryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "grpc" => Ok(Self::Grpc),
            "http" => Ok(Self::Http),
            other => Err(TelemetryError::Configuration(format!(
                "OTLP transport must be `grpc` or `http`, not {other:?}"
            ))),
        }
    }
}

/// Validated settings for one process's OTLP export.
#[derive(Clone, Debug)]
pub struct ExporterSettings {
    endpoint: String,
    transport: Transport,
    service_name: String,
    executable_name: String,
    timeout: Duration,
}

impl ExporterSettings {
    /// Validates raw settings before any exporter is constructed.
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError::Configuration`] when the endpoint or service name is blank, or
    /// when the export timeout is zero.
    pub fn new(
        endpoint: &str,
        transport: Transport,
        service_name: &str,
        executable_name: &str,
        timeout: Duration,
    ) -> Result<Self, TelemetryError> {
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            return Err(TelemetryError::Configuration(
                "OTLP endpoint must not be empty".to_owned(),
            ));
        }
        // Under `Http` the signal path is appended as text, so a query or fragment would end up
        // behind it: `http://host/api/default?org=x` becomes `...?org=x/v1/traces`, a valid URI
        // that silently posts to the wrong place. Rejected for both transports so one endpoint
        // string means the same thing whichever is selected.
        if let Some(index) = endpoint.find(['?', '#']) {
            return Err(TelemetryError::Configuration(format!(
                "OTLP endpoint must be a base URL without a query or fragment; found {:?} at byte {index}",
                &endpoint[index..index + 1]
            )));
        }
        // Ingest credentials belong in OTEL_EXPORTER_OTLP_HEADERS. Userinfo would put one in a
        // parsed configuration value, exporter diagnostics, and the informational web UI.
        let endpoint_authority = endpoint
            .split_once("://")
            .map_or(endpoint, |(_, rest)| rest)
            .split('/')
            .next()
            .unwrap_or_default();
        if endpoint_authority.contains('@') {
            return Err(TelemetryError::Configuration(
                "OTLP endpoint must not contain username/password userinfo; use OTEL_EXPORTER_OTLP_HEADERS"
                    .to_owned(),
            ));
        }
        let service_name = service_name.trim();
        if service_name.is_empty() {
            return Err(TelemetryError::Configuration(
                "OpenTelemetry service name must not be empty".to_owned(),
            ));
        }
        if timeout.is_zero() {
            return Err(TelemetryError::Configuration(
                "OTLP export timeout must be greater than zero".to_owned(),
            ));
        }
        Ok(Self {
            endpoint: endpoint.to_owned(),
            transport,
            service_name: service_name.to_owned(),
            executable_name: executable_name.to_owned(),
            timeout,
        })
    }

    /// Configured OTLP receiver base endpoint, validated to contain no URL userinfo.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// OpenTelemetry service name attached to exported resources.
    #[must_use]
    pub fn service_name(&self) -> &str {
        &self.service_name
    }

    /// Export timeout applied to each batch and to the final shutdown flush.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Selected wire transport.
    #[must_use]
    pub const fn transport(&self) -> Transport {
        self.transport
    }

    fn resource(&self) -> Resource {
        Resource::builder()
            .with_service_name(self.service_name.clone())
            .with_attributes([
                KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                KeyValue::new("process.executable.name", self.executable_name.clone()),
            ])
            .build()
    }

    /// Builds the batching tracer provider for this process.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be constructed or the exporter rejects the
    /// configured endpoint.
    pub fn tracer_provider(&self) -> Result<SdkTracerProvider, TelemetryError> {
        let builder = SpanExporter::builder();
        let exporter = match self.transport {
            Transport::Grpc => builder
                .with_tonic()
                .with_endpoint(self.endpoint.clone())
                .with_timeout(self.timeout)
                .build(),
            Transport::Http => builder
                .with_http()
                .with_http_client(OtlpHttpClient::new(self.timeout)?)
                .with_protocol(Protocol::HttpBinary)
                .with_endpoint(signal_endpoint(&self.endpoint, "traces"))
                .with_timeout(self.timeout)
                .build(),
        }
        .map_err(|source| TelemetryError::Exporter {
            signal: "trace",
            source,
        })?;
        Ok(SdkTracerProvider::builder()
            .with_resource(self.resource())
            .with_batch_exporter(exporter)
            .build())
    }

    /// Builds the batching logger provider for this process.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be constructed or the exporter rejects the
    /// configured endpoint.
    pub fn logger_provider(&self) -> Result<SdkLoggerProvider, TelemetryError> {
        let builder = LogExporter::builder();
        let exporter = match self.transport {
            Transport::Grpc => builder
                .with_tonic()
                .with_endpoint(self.endpoint.clone())
                .with_timeout(self.timeout)
                .build(),
            Transport::Http => builder
                .with_http()
                .with_http_client(OtlpHttpClient::new(self.timeout)?)
                .with_protocol(Protocol::HttpBinary)
                .with_endpoint(signal_endpoint(&self.endpoint, "logs"))
                .with_timeout(self.timeout)
                .build(),
        }
        .map_err(|source| TelemetryError::Exporter {
            signal: "log",
            source,
        })?;
        Ok(SdkLoggerProvider::builder()
            .with_resource(self.resource())
            .with_batch_exporter(exporter)
            .build())
    }
}

/// Appends the OTLP/HTTP signal path to a generic base endpoint.
///
/// Passing a programmatic endpoint to the SDK makes it exact rather than applying the environment
/// variable's `/v1/<signal>` behavior, so the suffix is added here.
fn signal_endpoint(base: &str, signal: &str) -> String {
    format!("{}/v1/{signal}", base.trim_end_matches('/'))
}

/// Adapter around the workspace's existing TLS-enabled reqwest client.
///
/// `opentelemetry-otlp` otherwise selects its own newer reqwest line, duplicating the HTTP/TLS
/// stack. Supplying the client also lets us reject redirects so an authorization header cannot be
/// forwarded to a receiver-selected destination.
#[derive(Clone, Debug)]
struct OtlpHttpClient(reqwest::blocking::Client);

impl OtlpHttpClient {
    fn new(timeout: Duration) -> Result<Self, TelemetryError> {
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
            .map_err(TelemetryError::HttpClientThread)?
            .join()
            .map_err(|payload| TelemetryError::HttpClientThreadPanicked {
                message: panic_message(&*payload),
            })?
            .map_err(TelemetryError::HttpClient)?;
        Ok(Self(client))
    }
}

/// Recovers the printable message from a panic payload.
///
/// A panic payload is the one failure a `Result` cannot carry, so the message has to be lifted
/// out here or it is lost with the box. `std::panic` stores a literal message as `&'static str`
/// and a formatted one as `String`; anything else came from `panic_any` and has no text at all.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_owned();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "no panic message".to_owned()
}

#[async_trait]
impl HttpClient for OtlpHttpClient {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let request: reqwest::blocking::Request = request.try_into()?;
        // Deliberately no `error_for_status()`, which upstream's adapter calls. The OTLP SDK has
        // two failure branches: a status branch that reports the code, and a network branch whose
        // message is the constant "network error". Turning a 4xx into an `Err` here forces every
        // response down the network branch, so an expired token or a wrong org path arrives
        // indistinguishable from a dead socket — and there is no second channel to recover it
        // from, since the SDK's debug macros compile out without `internal-logs`. Returning the
        // response lets the SDK classify it and say what to fix.
        let mut response = self.0.execute(request)?;
        let headers = std::mem::take(response.headers_mut());
        let status = response.status();
        let mut response = Response::builder().status(status).body(response.bytes()?)?;
        *response.headers_mut() = headers;
        Ok(response)
    }
}

/// The identifiers a W3C `traceparent` carries, in wire byte order.
///
/// This crate speaks raw bytes rather than a Dekopon wire type so it stays free of protocol
/// dependencies; the protocol crate owns parsing, formatting, and validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TraceContextParts {
    /// 16-byte trace identifier.
    pub trace_id: [u8; 16],
    /// 8-byte identifier of the span that should parent the remote work.
    pub span_id: [u8; 8],
    /// W3C trace flags; bit 0 is the sampled flag.
    pub flags: u8,
}

/// Reads the OpenTelemetry context attached to the current `tracing` span.
///
/// Returns `None` when no span is active or the active span has no valid OpenTelemetry context,
/// which is the ordinary state when export is disabled.
#[must_use]
pub fn current_trace_context() -> Option<TraceContextParts> {
    let context = tracing::Span::current().context();
    let span = context.span();
    let span_context = span.span_context();
    if !span_context.is_valid() {
        return None;
    }
    Some(TraceContextParts {
        trace_id: span_context.trace_id().to_bytes(),
        span_id: span_context.span_id().to_bytes(),
        flags: span_context.trace_flags().to_u8(),
    })
}

/// Rebuilds a remote parent context from identifiers received over the wire.
///
/// The resulting context is marked remote, so a span opened beneath it is recorded as a child of
/// work that happened in another process rather than as a new trace root.
#[must_use]
pub fn remote_context(parts: TraceContextParts) -> Context {
    let span_context = SpanContext::new(
        opentelemetry::trace::TraceId::from_bytes(parts.trace_id),
        opentelemetry::trace::SpanId::from_bytes(parts.span_id),
        TraceFlags::new(parts.flags),
        true,
        TraceState::default(),
    );
    Context::new().with_remote_span_context(span_context)
}

/// Failures raised while configuring telemetry.
#[derive(Debug, Error)]
pub enum TelemetryError {
    /// Settings were rejected before any exporter was built.
    #[error("invalid telemetry configuration: {0}")]
    Configuration(String),
    /// The dedicated HTTP client thread could not be spawned.
    #[error("could not start OTLP HTTP client builder")]
    HttpClientThread(#[source] std::io::Error),
    /// The dedicated HTTP client thread panicked.
    #[error("OTLP HTTP client builder panicked: {message}")]
    HttpClientThreadPanicked {
        /// The panic's own message; a bare "the builder panicked" names no cause to act on.
        message: String,
    },
    /// The reqwest client could not be constructed.
    #[error("could not build OTLP HTTP client")]
    HttpClient(#[source] reqwest::Error),
    /// The OTLP SDK rejected the exporter configuration.
    #[error("could not build OTLP {signal} exporter")]
    Exporter {
        /// Signal whose exporter failed.
        signal: &'static str,
        /// Underlying SDK error.
        #[source]
        source: ExporterBuildError,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        ExporterSettings, TelemetryError, TraceContextParts, Transport, panic_message,
        remote_context, signal_endpoint,
    };
    use opentelemetry::trace::TraceContextExt as _;
    use std::time::Duration;

    /// The panic payload is the only account of why the client thread died, and it is the whole
    /// reason the failure is reachable: a builder that panics says what it could not build —
    /// a runtime it could not spawn, a TLS root store it could not load. Reporting "the builder
    /// panicked" and nothing else leaves an operator with the fact of a dead thread and no cause.
    #[test]
    fn a_client_thread_panic_keeps_its_message() {
        let literal = std::panic::catch_unwind(|| panic!("failed to create tokio runtime"))
            .expect_err("the closure panics");
        assert_eq!(panic_message(&*literal), "failed to create tokio runtime");

        let formatted = std::panic::catch_unwind(|| panic!("{} roots missing", 3))
            .expect_err("the closure panics");
        assert_eq!(panic_message(&*formatted), "3 roots missing");

        assert_eq!(
            TelemetryError::HttpClientThreadPanicked {
                message: panic_message(&*literal),
            }
            .to_string(),
            "OTLP HTTP client builder panicked: failed to create tokio runtime"
        );
    }

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

    #[test]
    fn transport_round_trips_through_its_stable_token() {
        for transport in [Transport::Grpc, Transport::Http] {
            assert_eq!(
                transport
                    .as_str()
                    .parse::<Transport>()
                    .expect("valid token"),
                transport
            );
        }
    }

    #[test]
    fn transport_rejects_unknown_tokens() {
        assert!("thrift".parse::<Transport>().is_err());
        assert!("".parse::<Transport>().is_err());
    }

    #[test]
    fn settings_reject_blank_and_zero_values() {
        let timeout = Duration::from_secs(5);
        assert!(ExporterSettings::new("  ", Transport::Http, "svc", "exe", timeout).is_err());
        assert!(
            ExporterSettings::new("http://host", Transport::Http, " ", "exe", timeout).is_err()
        );
        assert!(
            ExporterSettings::new(
                "http://host",
                Transport::Http,
                "svc",
                "exe",
                Duration::from_millis(0)
            )
            .is_err()
        );
        assert!(
            ExporterSettings::new("http://host", Transport::Grpc, "svc", "exe", timeout).is_ok()
        );
    }

    /// A query or fragment would sit in front of the appended signal path under `Http`, producing
    /// a URI that parses and posts to the wrong place. Rejected under both transports so the same
    /// endpoint string cannot mean two different things.
    #[test]
    fn settings_reject_endpoints_carrying_a_query_or_fragment() {
        let timeout = Duration::from_secs(5);
        for transport in [Transport::Grpc, Transport::Http] {
            for endpoint in ["http://host/api/default?org=x", "http://host/api/default#f"] {
                assert!(
                    ExporterSettings::new(endpoint, transport, "svc", "exe", timeout).is_err(),
                    "accepted {endpoint} on {transport}"
                );
            }
        }
    }

    #[test]
    fn settings_reject_endpoint_userinfo_so_credentials_cannot_reach_status_views() {
        let timeout = Duration::from_secs(5);
        for endpoint in [
            "https://operator:password@observe.example/api/default",
            "http://token@127.0.0.1:4318",
            "token@observe.example:4317",
        ] {
            assert!(
                ExporterSettings::new(endpoint, Transport::Http, "svc", "exe", timeout).is_err(),
                "accepted endpoint userinfo in {endpoint}"
            );
        }
    }

    /// A rebuilt parent must stay byte-identical and remote, or broker spans silently start a new
    /// trace instead of joining the runner's.
    #[test]
    fn remote_context_preserves_identifiers_and_marks_them_remote() {
        let parts = TraceContextParts {
            trace_id: [
                0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e,
                0x47, 0x36,
            ],
            span_id: [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7],
            flags: 1,
        };

        let context = remote_context(parts);
        let span_context = context.span().span_context().clone();

        assert!(span_context.is_valid());
        assert!(span_context.is_remote());
        assert_eq!(span_context.trace_id().to_bytes(), parts.trace_id);
        assert_eq!(span_context.span_id().to_bytes(), parts.span_id);
        assert_eq!(span_context.trace_flags().to_u8(), 1);
    }
}
