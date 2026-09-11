//! Shared OTLP exporter construction and W3C trace context for Dekopon processes.
//!
//! Both exporting Dekopon daemons — the privileged broker and the unprivileged chat
//! gateway — exports its own spans, so exporter construction lives here rather than in any one
//! binary. The crate deliberately depends on no Dekopon crate: it must remain linkable from the
//! gateway without dragging broker code into the gateway's dependency tree, which CI rejects.
//!
//! # Authority
//!
//! This crate configures transport and never resolves credentials. Ingest authentication is read
//! by the OpenTelemetry SDK from the standard `OTEL_EXPORTER_OTLP_HEADERS` environment variable,
//! so a token is never accepted as a command-line argument, never written to a configuration file
//! this crate parses, and never attached to a span attribute or log field.

mod install;

use std::{fmt, str::FromStr, sync::OnceLock, time::Duration};

use async_trait::async_trait;
use opentelemetry::{
    Context, KeyValue,
    trace::{SpanContext, TraceContextExt as _, TraceFlags, TraceState},
};
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use opentelemetry_otlp::{
    ExporterBuildError, LogExporter, Protocol, SpanExporter, WithExportConfig, WithHttpConfig,
};
use opentelemetry_sdk::{
    Resource, logs,
    logs::{BatchLogProcessor, SdkLoggerProvider},
    trace,
    trace::{BatchSpanProcessor, SdkTracerProvider},
};
use serde::Deserialize;
use thiserror::Error;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

pub use install::{
    Console, ConsoleFilter, ConsoleFormat, ConsoleWriter, Install, InstallError, ShutdownError,
    TelemetryGuard, optional_logger_provider, optional_tracer_provider,
};

/// Wire transport used to reach an OTLP receiver.
///
/// Both are first-class, and both reach an `https://` endpoint through WebPKI roots. A receiver
/// reached through a path-routing reverse proxy generally wants `Grpc`, whose method paths are
/// fixed by the protobuf service definition; a receiver behind a plain HTTP route wants `Http`,
/// whose signal paths are appended to the configured base.
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

/// Span records one export queue holds before the batch processor starts dropping.
///
/// See [`MAX_QUEUED_LOG_RECORDS`] for the arithmetic and for why the two signals differ.
const MAX_QUEUED_SPANS: usize = 1024;

/// Spans one export request carries.
///
/// The batch processor exports as soon as this many records have accumulated, so this is the
/// drain trigger as well as the size of one in-flight request. Every queue here holds four of
/// these — the same 4:1 ratio the SDK defaults to, so a smaller queue is not more drop-prone per
/// drain; it simply drains sooner and holds less while it waits.
const MAX_SPANS_PER_EXPORT: usize = 256;

/// Log records one export queue holds before the batch processor starts dropping.
///
/// The SDK's default is 2048 records per queue with **no byte ceiling anywhere**: `BatchConfig`
/// counts records, and `SpanLimits` counts attributes per span, events, and links. Nothing in
/// `opentelemetry_sdk` 0.32 truncates an attribute value, so a queue's size in bytes is only ever
/// `records × the largest attribute the process emits`. Lowering the attribute *counts* would not
/// bound bytes either; it would silently drop whole attributes, which goal 2 in `docs/design.md`
/// rejects more firmly than it rejects volume.
///
/// So the bound here is a record count, honestly, and it is set per signal because the two carry
/// different payloads:
///
/// - Log records are where the bulk lands. Audit is one structured record per broker decision and
///   under goal 2 a record carries a prompt, a model answer, or a whole script's output — bounded
///   by `dekopon-shell`'s 256 KiB accumulated-output ceiling and the broker host's 1 MiB output
///   ceiling. They also arrive at a fraction of the span rate. 256 records × 256 KiB ≈ **64 MiB**
///   worst case, against 512 MiB at the SDK default; a realistic queue of few-KiB records is under
///   a megabyte.
/// - Spans are numerous and individually small — names, kinds, counts, outcomes, ids. The
///   constitution says a span is never dropped, so that queue keeps four times the per-script
///   span budget in reserve rather than the tightest ceiling.
///
/// Together the two queues cost a process tens of MiB where they previously cost up to 1 GiB.
/// Dropping is still possible under a stalled receiver; the SDK counts drops and reports the total
/// at shutdown.
const MAX_QUEUED_LOG_RECORDS: usize = 256;

/// Log records one export request carries. See [`MAX_SPANS_PER_EXPORT`].
const MAX_LOG_RECORDS_PER_EXPORT: usize = 64;

/// Validated settings for one process's OTLP export.
#[derive(Clone, Debug)]
pub struct ExporterSettings {
    endpoint: String,
    transport: Transport,
    service_name: String,
    executable_name: String,
    service_version: String,
    timeout: Duration,
    /// Built on first use and shared by both signals: every `reqwest::blocking::Client` owns a
    /// private runtime thread and connection pool, and one process needs one, not one per signal.
    http_client: OnceLock<OtlpHttpClient>,
}

impl ExporterSettings {
    /// Validates raw settings before any exporter is constructed.
    ///
    /// `service_version` becomes the `service.version` resource attribute; it is the *calling*
    /// executable's version, normally `env!("CARGO_PKG_VERSION")` at the call site. A blank value
    /// falls back to this crate's own version, which is correct only inside this workspace.
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
        service_version: &str,
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
        // parsed configuration value or exporter diagnostics.
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
        let service_version = match service_version.trim() {
            "" => env!("CARGO_PKG_VERSION"),
            version => version,
        };
        Ok(Self {
            endpoint: endpoint.to_owned(),
            transport,
            service_name: service_name.to_owned(),
            executable_name: executable_name.to_owned(),
            service_version: service_version.to_owned(),
            timeout,
            http_client: OnceLock::new(),
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

    /// Export timeout applied to each batch, and intended for the final shutdown flush.
    ///
    /// The batch half is enforced here; the flush half depends on the caller passing this value to
    /// `shutdown_with_timeout` rather than the SDK's own default.
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
                KeyValue::new("service.version", self.service_version.clone()),
                KeyValue::new("process.executable.name", self.executable_name.clone()),
            ])
            .build()
    }

    /// Returns the process's single OTLP HTTP client, building it on first use.
    fn http_client(&self) -> Result<OtlpHttpClient, TelemetryError> {
        if let Some(client) = self.http_client.get() {
            return Ok(client.clone());
        }
        let client = OtlpHttpClient::new(self.timeout)?;
        Ok(self.http_client.get_or_init(|| client).clone())
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
                .with_http_client(self.http_client()?)
                .with_protocol(Protocol::HttpBinary)
                .with_endpoint(signal_endpoint(&self.endpoint, "traces"))
                .with_timeout(self.timeout)
                .build(),
        }
        .map_err(|source| TelemetryError::Exporter {
            signal: "trace",
            source,
        })?;
        let processor = BatchSpanProcessor::builder(exporter)
            .with_batch_config(
                trace::BatchConfigBuilder::default()
                    .with_max_queue_size(MAX_QUEUED_SPANS)
                    .with_max_export_batch_size(MAX_SPANS_PER_EXPORT)
                    .build(),
            )
            .build();
        Ok(SdkTracerProvider::builder()
            .with_resource(self.resource())
            .with_span_processor(processor)
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
                .with_http_client(self.http_client()?)
                .with_protocol(Protocol::HttpBinary)
                .with_endpoint(signal_endpoint(&self.endpoint, "logs"))
                .with_timeout(self.timeout)
                .build(),
        }
        .map_err(|source| TelemetryError::Exporter {
            signal: "log",
            source,
        })?;
        let processor = BatchLogProcessor::builder(exporter)
            .with_batch_config(
                logs::BatchConfigBuilder::default()
                    .with_max_queue_size(MAX_QUEUED_LOG_RECORDS)
                    .with_max_export_batch_size(MAX_LOG_RECORDS_PER_EXPORT)
                    .build(),
            )
            .build();
        Ok(SdkLoggerProvider::builder()
            .with_resource(self.resource())
            .with_log_processor(processor)
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
/// stack. Supplying the client also lets us bound where the ingest header may go.
#[derive(Clone, Debug)]
struct OtlpHttpClient(reqwest::blocking::Client);

/// The stance the OTLP/HTTP client takes, separated from the thread that builds it.
///
/// The SDK reads ingest authentication from `OTEL_EXPORTER_OTLP_HEADERS`, so every export carries
/// a credential, and neither setting here is reqwest's default:
///
/// - `redirect(Policy::none())` keeps that header on the collector the operator named; a followed
///   redirect would hand it to whatever host answered.
/// - `no_proxy()` overrides the `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY` reqwest otherwise reads from
///   the environment, which would route the ingest header — and every span and log record — through
///   a host nobody named to Dekopon. The telemetry store is inside the operator's trust boundary; a
///   proxy on the way to it is not.
///
/// Taking the builder as an argument is what makes the proxy assertion possible: a default builder
/// on a proxy-free runner carries no proxy whether or not `.no_proxy()` is there, so the test
/// starts from one that definitely carries a proxy and watches this clear it.
fn otlp_client_from(
    builder: reqwest::blocking::ClientBuilder,
    timeout: Duration,
) -> reqwest::blocking::ClientBuilder {
    builder
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
}

impl OtlpHttpClient {
    fn new(timeout: Duration) -> Result<Self, TelemetryError> {
        // reqwest's blocking client owns a private runtime and refuses to create it from within
        // Dekopon's Tokio runtime. Build it on a plain thread, as the upstream OTLP adapter does.
        let client = std::thread::Builder::new()
            .name("dekopon-otlp-http-client".to_owned())
            .spawn(move || otlp_client_from(reqwest::blocking::Client::builder(), timeout).build())
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
        ExporterSettings, TelemetryError, TraceContextParts, Transport, otlp_client_from,
        panic_message, remote_context, signal_endpoint,
    };
    use opentelemetry::trace::TraceContextExt as _;
    use std::time::Duration;

    /// The discard port: a proxy that is well formed, never dialled, and obvious in a diff.
    const AMBIENT_PROXY: &str = "http://127.0.0.1:9";

    /// The shape an exported `HTTPS_PROXY=http://127.0.0.1:9` leaves in reqwest's default builder.
    fn proxied_builder() -> reqwest::blocking::ClientBuilder {
        reqwest::blocking::Client::builder()
            .proxy(reqwest::Proxy::all(AMBIENT_PROXY).expect("a well-formed proxy uri"))
    }

    /// Every export carries the `OTEL_EXPORTER_OTLP_HEADERS` ingest credential, so the collector
    /// this client reaches has to be the one the operator named. `reqwest` exposes no getters for
    /// a builder's configuration, so the builder's own `Debug` is the reading: it prints `proxies`
    /// only when the proxy list is non-empty and `redirect_policy` only when the policy is not the
    /// default ten-hop limit. The blocking builder keeps its deadline beside the inner
    /// configuration this `Debug` renders, so the timeout is not visible here and is not asserted.
    #[test]
    fn the_otlp_client_ignores_ambient_proxy_configuration() {
        // A default builder on a proxy-free runner produces the same empty proxy list whether or
        // not `.no_proxy()` is there, so starting from one would assert nothing. Starting from a
        // builder that carries a proxy mutates no process state and races no other test.
        assert!(
            format!("{:?}", proxied_builder()).contains("proxies"),
            "the fixture must carry the proxy this test is about"
        );

        let rendered = format!(
            "{:?}",
            otlp_client_from(proxied_builder(), Duration::from_secs(10))
        );

        assert!(
            !rendered.contains("proxies"),
            "the OTLP client must not inherit an ambient proxy: {rendered}"
        );
        assert!(
            rendered.contains("redirect_policy: Policy(None)"),
            "the ingest header must not follow a redirect: {rendered}"
        );
    }

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
        assert!(
            ExporterSettings::new("  ", Transport::Http, "svc", "exe", "1.2.3", timeout).is_err()
        );
        assert!(
            ExporterSettings::new("http://host", Transport::Http, " ", "exe", "1.2.3", timeout)
                .is_err()
        );
        assert!(
            ExporterSettings::new(
                "http://host",
                Transport::Http,
                "svc",
                "exe",
                "1.2.3",
                Duration::from_millis(0)
            )
            .is_err()
        );
        assert!(
            ExporterSettings::new(
                "http://host",
                Transport::Grpc,
                "svc",
                "exe",
                "1.2.3",
                timeout
            )
            .is_ok()
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
                    ExporterSettings::new(endpoint, transport, "svc", "exe", "1.2.3", timeout)
                        .is_err(),
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
                ExporterSettings::new(endpoint, Transport::Http, "svc", "exe", "1.2.3", timeout)
                    .is_err(),
                "accepted endpoint userinfo in {endpoint}"
            );
        }
    }

    /// `service.version` describes the executable that emitted the span, not the library that
    /// built its exporter. Reading it from this crate's `CARGO_PKG_VERSION` is right only while
    /// every workspace crate shares one version, and simply wrong for a crates.io consumer.
    #[test]
    fn service_version_comes_from_the_caller_and_falls_back_to_this_crate() {
        let timeout = Duration::from_secs(5);
        let version = |service_version| {
            ExporterSettings::new(
                "http://host",
                Transport::Http,
                "svc",
                "exe",
                service_version,
                timeout,
            )
            .expect("valid settings")
            .resource()
            .get(&opentelemetry::Key::from_static_str("service.version"))
            .expect("the resource carries a service version")
            .to_string()
        };

        assert_eq!(version("4.5.6"), "4.5.6");
        assert_eq!(version("  "), env!("CARGO_PKG_VERSION"));
    }

    /// One process needs one blocking client, not one per signal: each `reqwest::blocking::Client`
    /// owns a private runtime thread and connection pool for as long as the process lives.
    #[test]
    fn both_signals_share_one_blocking_http_client() {
        let settings = ExporterSettings::new(
            "http://host",
            Transport::Http,
            "svc",
            "exe",
            "1.2.3",
            Duration::from_secs(5),
        )
        .expect("valid settings");

        assert!(
            settings.http_client.get().is_none(),
            "a client was built before any signal asked for one"
        );
        let _traces = settings.http_client().expect("the trace signal builds one");
        let _logs = settings.http_client().expect("the log signal reuses it");
        assert!(
            settings.http_client.get().is_some(),
            "the client is rebuilt per signal instead of being shared"
        );
    }

    /// A rebuilt parent must stay byte-identical and remote, or broker spans silently start a new
    /// trace instead of joining the gateway's.
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
