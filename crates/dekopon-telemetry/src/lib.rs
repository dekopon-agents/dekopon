//! Ingest credentials are read only from OTEL_EXPORTER_OTLP_HEADERS by the SDK itself; this crate
//! never accepts a token as an argument or attaches one to a span or log field.

#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
mod install;

use std::{
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
    sync::OnceLock,
    time::Duration,
};

use async_trait::async_trait;
use opentelemetry::{
    Context, KeyValue,
    trace::{SpanContext, TraceContextExt as _, TraceFlags, TraceState},
};
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use opentelemetry_otlp::{
    ExporterBuildError, LogExporter, Protocol, SpanExporter, WithExportConfig, WithHttpConfig,
    WithTonicConfig,
    tonic_types::transport::{Certificate, ClientTlsConfig},
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

pub fn link_span(execution: &tracing::Span, receipt: &tracing::Span) {
    let context = receipt.context();
    let span = context.span();
    if span.span_context().is_valid() {
        execution.add_link(span.span_context().clone());
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    Grpc,
    #[default]
    Http,
}

impl Transport {
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

const MAX_QUEUED_SPANS: usize = 1024;

const MAX_SPANS_PER_EXPORT: usize = 256;

const MAX_QUEUED_LOG_RECORDS: usize = 256;

const MAX_LOG_RECORDS_PER_EXPORT: usize = 64;

pub const CA_CERTIFICATE_ENV: &str = "OTEL_EXPORTER_OTLP_CERTIFICATE";

#[derive(Clone, Debug)]
pub struct ExporterSettings {
    endpoint: String,
    transport: Transport,
    service_name: String,
    executable_name: String,
    service_version: String,
    timeout: Duration,
    ca_certificate: Option<Vec<u8>>,
    http_client: OnceLock<OtlpHttpClient>,
}

impl ExporterSettings {
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
        if let Some(index) = endpoint.find(['?', '#']) {
            return Err(TelemetryError::Configuration(format!(
                "OTLP endpoint must be a base URL without a query or fragment; found {:?} at byte {index}",
                &endpoint[index..index + 1]
            )));
        }
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
        let ca_certificate = std::env::var_os(CA_CERTIFICATE_ENV)
            .filter(|path| !path.is_empty())
            .map(|path| read_ca_certificate(Path::new(&path)))
            .transpose()?;
        Ok(Self {
            endpoint: endpoint.to_owned(),
            transport,
            service_name: service_name.to_owned(),
            executable_name: executable_name.to_owned(),
            service_version: service_version.to_owned(),
            timeout,
            ca_certificate,
            http_client: OnceLock::new(),
        })
    }

    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    #[must_use]
    pub fn service_name(&self) -> &str {
        &self.service_name
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

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

    fn http_client(&self) -> Result<OtlpHttpClient, TelemetryError> {
        if let Some(client) = self.http_client.get() {
            return Ok(client.clone());
        }
        let client = OtlpHttpClient::new(self.timeout, self.ca_certificate.as_deref())?;
        Ok(self.http_client.get_or_init(|| client).clone())
    }

    fn tonic_tls<B: WithTonicConfig>(&self, builder: B) -> B {
        match &self.ca_certificate {
            Some(pem) => builder.with_tls_config(
                ClientTlsConfig::new()
                    .with_webpki_roots()
                    .ca_certificate(Certificate::from_pem(pem)),
            ),
            None => builder,
        }
    }

    pub fn tracer_provider(&self) -> Result<SdkTracerProvider, TelemetryError> {
        let builder = SpanExporter::builder();
        let exporter = match self.transport {
            Transport::Grpc => self
                .tonic_tls(
                    builder
                        .with_tonic()
                        .with_endpoint(self.endpoint.clone())
                        .with_timeout(self.timeout),
                )
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

    pub fn logger_provider(&self) -> Result<SdkLoggerProvider, TelemetryError> {
        let builder = LogExporter::builder();
        let exporter = match self.transport {
            Transport::Grpc => self
                .tonic_tls(
                    builder
                        .with_tonic()
                        .with_endpoint(self.endpoint.clone())
                        .with_timeout(self.timeout),
                )
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

fn read_ca_certificate(path: &Path) -> Result<Vec<u8>, TelemetryError> {
    let pem = std::fs::read(path).map_err(|source| TelemetryError::CaCertificate {
        path: path.to_owned(),
        source,
    })?;
    if reqwest::Certificate::from_pem_bundle(&pem)
        .map_or(true, |certificates| certificates.is_empty())
    {
        return Err(TelemetryError::Configuration(format!(
            "{CA_CERTIFICATE_ENV} names {}, which holds no PEM certificate",
            path.display()
        )));
    }
    Ok(pem)
}

fn signal_endpoint(base: &str, signal: &str) -> String {
    format!("{}/v1/{signal}", base.trim_end_matches('/'))
}

#[derive(Clone, Debug)]
struct OtlpHttpClient(reqwest::blocking::Client);

/// Redirects are disabled so the ingest credential header can't follow a redirect to an unnamed
/// host, and environment proxies are disabled so it never routes through one either.
fn otlp_client_from(
    builder: reqwest::blocking::ClientBuilder,
    timeout: Duration,
    ca_certificate: Option<&[u8]>,
) -> Result<reqwest::blocking::ClientBuilder, reqwest::Error> {
    let mut builder = builder
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if let Some(pem) = ca_certificate {
        for certificate in reqwest::Certificate::from_pem_bundle(pem)? {
            builder = builder.add_root_certificate(certificate);
        }
    }
    Ok(builder)
}

impl OtlpHttpClient {
    fn new(timeout: Duration, ca_certificate: Option<&[u8]>) -> Result<Self, TelemetryError> {
        let client = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("dekopon-otlp-http-client".to_owned())
                .spawn_scoped(scope, move || {
                    otlp_client_from(
                        reqwest::blocking::Client::builder(),
                        timeout,
                        ca_certificate,
                    )?
                    .build()
                })
                .map_err(TelemetryError::HttpClientThread)?
                .join()
                .map_err(|payload| TelemetryError::HttpClientThreadPanicked {
                    message: panic_message(&*payload),
                })
        })?
        .map_err(TelemetryError::HttpClient)?;
        Ok(Self(client))
    }
}

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
        // This deliberately skips `error_for_status()`: converting a 4xx to an `Err` here would
        // force it down the SDK's generic network-error branch, indistinguishable from a dead
        // socket.
        let mut response = self.0.execute(request)?;
        let headers = std::mem::take(response.headers_mut());
        let status = response.status();
        let mut response = Response::builder().status(status).body(response.bytes()?)?;
        *response.headers_mut() = headers;
        Ok(response)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TraceContextParts {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub flags: u8,
}

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

#[derive(Debug, Error)]
pub enum TelemetryError {
    #[error("invalid telemetry configuration: {0}")]
    Configuration(String),
    #[error("could not start OTLP HTTP client builder")]
    HttpClientThread(#[source] std::io::Error),
    #[error("OTLP HTTP client builder panicked: {message}")]
    HttpClientThreadPanicked { message: String },
    #[error("could not build OTLP HTTP client")]
    HttpClient(#[source] reqwest::Error),
    #[error("could not read OTLP CA certificate {}", path.display())]
    CaCertificate {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not build OTLP {signal} exporter")]
    Exporter {
        signal: &'static str,
        #[source]
        source: ExporterBuildError,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        ExporterSettings, TelemetryError, TraceContextParts, Transport, otlp_client_from,
        panic_message, read_ca_certificate, remote_context, signal_endpoint,
    };
    use opentelemetry::trace::TraceContextExt as _;
    use std::time::Duration;

    const AMBIENT_PROXY: &str = "http://127.0.0.1:9";

    fn proxied_builder() -> reqwest::blocking::ClientBuilder {
        reqwest::blocking::Client::builder()
            .proxy(reqwest::Proxy::all(AMBIENT_PROXY).expect("a well-formed proxy uri"))
    }

    #[test]
    fn the_otlp_client_ignores_ambient_proxy_configuration() {
        assert!(
            format!("{:?}", proxied_builder()).contains("proxies"),
            "the fixture must carry the proxy this test is about"
        );

        let rendered = format!(
            "{:?}",
            otlp_client_from(proxied_builder(), Duration::from_secs(10), None)
                .expect("no certificate to parse")
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

    const PRIVATE_ROOT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../dekopon-http-host/tests/fixtures/private-root.pem"
    );

    #[test]
    fn a_ca_certificate_must_be_a_readable_pem() {
        assert!(matches!(
            read_ca_certificate(std::path::Path::new("/nonexistent/otlp-ca.pem")),
            Err(TelemetryError::CaCertificate { .. })
        ));
        assert!(matches!(
            read_ca_certificate(std::path::Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/Cargo.toml"
            ))),
            Err(TelemetryError::Configuration(_))
        ));
        assert!(read_ca_certificate(std::path::Path::new(PRIVATE_ROOT)).is_ok());
    }

    #[tokio::test]
    async fn both_transports_build_exporters_that_trust_an_extra_ca() {
        for (endpoint, transport) in [
            ("https://collector.internal:4317", Transport::Grpc),
            ("https://collector.internal:4318", Transport::Http),
        ] {
            let mut settings = ExporterSettings::new(
                endpoint,
                transport,
                "svc",
                "exe",
                "1.2.3",
                Duration::from_secs(5),
            )
            .expect("valid settings");
            settings.ca_certificate = Some(
                read_ca_certificate(std::path::Path::new(PRIVATE_ROOT)).expect("fixture root"),
            );
            settings.tracer_provider().expect("trace exporter builds");
            settings.logger_provider().expect("log exporter builds");
        }
    }

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

#[cfg(test)]
mod aggregation_tests {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::{
        error::OTelSdkResult,
        trace::{SdkTracerProvider, SpanData, SpanExporter},
    };
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::prelude::*;

    #[derive(Clone, Debug, Default)]
    struct Exported(Arc<Mutex<Vec<SpanData>>>);
    impl SpanExporter for Exported {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.0.lock().expect("exported spans").extend(batch);
            Ok(())
        }
    }

    #[test]
    fn collected_execution_exports_lead_parent_and_every_constituent_link() {
        let exported = Exported::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exported.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("aggregation-test")));
        tracing::subscriber::with_default(subscriber, || {
            let lead = tracing::info_span!(parent: None, "receipt.lead");
            let second = tracing::info_span!(parent: None, "receipt.second");
            let third = tracing::info_span!(parent: None, "receipt.third");
            let execution = tracing::info_span!(parent: &lead, "gateway.message");
            for receipt in [&lead, &second, &third] {
                super::link_span(&execution, receipt);
            }
        });
        provider.force_flush().expect("flush");
        let spans = exported.0.lock().expect("exported spans");
        let execution = spans
            .iter()
            .find(|span| span.name == "gateway.message")
            .expect("execution exported");
        let receipts: Vec<_> = spans
            .iter()
            .filter(|span| span.name.starts_with("receipt."))
            .collect();
        assert_eq!(receipts.len(), 3);
        assert_eq!(execution.links.links.len(), 3);
        for receipt in &receipts {
            assert!(
                execution
                    .links
                    .links
                    .iter()
                    .any(|link| link.span_context == receipt.span_context)
            );
        }
        let lead = receipts
            .iter()
            .find(|span| span.name == "receipt.lead")
            .expect("lead");
        assert_eq!(execution.parent_span_id, lead.span_context.span_id());
        assert_eq!(
            execution.span_context.trace_id(),
            lead.span_context.trace_id()
        );
        drop(spans);
        provider.shutdown().expect("shutdown");
    }
}
