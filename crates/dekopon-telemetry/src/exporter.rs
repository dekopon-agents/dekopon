use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

use async_trait::async_trait;
use opentelemetry::KeyValue;
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

/// `Grpc` is still a recognised token so a configuration that names it gets
/// [`TelemetryError::GrpcTransport`] rather than an unknown-value parse error.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    #[default]
    Http,
    Grpc,
}

const MAX_QUEUED_SPANS: usize = 1024;

const MAX_SPANS_PER_EXPORT: usize = 256;

const MAX_QUEUED_LOG_RECORDS: usize = 256;

const MAX_LOG_RECORDS_PER_EXPORT: usize = 64;

pub const CA_CERTIFICATE_ENV: &str = "OTEL_EXPORTER_OTLP_CERTIFICATE";

#[derive(Clone, Debug)]
pub struct ExporterSettings {
    endpoint: String,
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
        match transport {
            Transport::Http => {}
            Transport::Grpc => return Err(TelemetryError::GrpcTransport),
        }
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

    fn resource(&self) -> Resource {
        Resource::builder()
            .with_service_name(self.service_name.clone())
            .with_attributes([
                KeyValue::new("service.version", self.service_version.clone()),
                KeyValue::new("process.executable.name", self.executable_name.clone()),
            ])
            .build()
    }

    #[must_use]
    pub fn ca_certificate(&self) -> Option<&[u8]> {
        self.ca_certificate.as_deref()
    }

    fn http_client(&self) -> Result<OtlpHttpClient, TelemetryError> {
        if let Some(client) = self.http_client.get() {
            return Ok(client.clone());
        }
        let client = OtlpHttpClient::new(self.timeout, self.ca_certificate.as_deref())?;
        Ok(self.http_client.get_or_init(|| client).clone())
    }

    pub fn tracer_provider(&self) -> Result<SdkTracerProvider, TelemetryError> {
        let exporter = SpanExporter::builder()
            .with_http()
            .with_http_client(self.http_client()?)
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(signal_endpoint(&self.endpoint, "traces"))
            .with_timeout(self.timeout)
            .build()
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
        let exporter = LogExporter::builder()
            .with_http()
            .with_http_client(self.http_client()?)
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(signal_endpoint(&self.endpoint, "logs"))
            .with_timeout(self.timeout)
            .build()
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
            .with_log_processor(EventArrayProcessor)
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
struct OtlpHttpClient {
    client: reqwest::Client,
    runtime: tokio::runtime::Handle,
}

/// Redirects are disabled so the ingest credential header can't follow a redirect to an unnamed
/// host, and environment proxies are disabled so it never routes through one either.
fn otlp_client_from(
    builder: reqwest::ClientBuilder,
    timeout: Duration,
    ca_certificate: Option<&[u8]>,
) -> Result<reqwest::ClientBuilder, reqwest::Error> {
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
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_outside| TelemetryError::NoRuntime)?;
        let client = otlp_client_from(reqwest::Client::builder(), timeout, ca_certificate)
            .and_then(reqwest::ClientBuilder::build)
            .map_err(TelemetryError::HttpClient)?;
        Ok(Self { client, runtime })
    }
}

pub(super) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
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
        let request: reqwest::Request = request.try_into()?;
        let client = self.client.clone();
        // The SDK's batch threads poll this future under `futures_executor`, which has no tokio
        // reactor or timer; the request must run on the daemon's runtime.
        #[expect(
            clippy::disallowed_methods,
            reason = "owner: this call awaits the handle; bound: each SDK batch thread runs one export at a time"
        )]
        let exported = self.runtime.spawn(async move {
            // This deliberately skips `error_for_status()`: converting a 4xx to an `Err` here
            // would force it down the SDK's generic network-error branch, indistinguishable from a
            // dead socket.
            let mut response = client.execute(request).await?;
            let headers = std::mem::take(response.headers_mut());
            let status = response.status();
            let mut response = Response::builder()
                .status(status)
                .body(response.bytes().await?)?;
            *response.headers_mut() = headers;
            Ok(response)
        });
        exported.await?
    }
}

#[derive(Debug, Error)]
pub enum TelemetryError {
    #[error("invalid telemetry configuration: {0}")]
    Configuration(String),
    #[error("OTLP transport `grpc` is no longer supported; use `http`")]
    GrpcTransport,
    #[error("the OTLP HTTP client must be built inside a tokio runtime")]
    NoRuntime,
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
        ExporterSettings, OtlpHttpClient, TelemetryError, Transport, otlp_client_from,
        panic_message, read_ca_certificate, signal_endpoint,
    };
    use opentelemetry::{
        logs::{Logger as _, LoggerProvider as _},
        trace::{Tracer as _, TracerProvider as _},
    };
    use parking_lot::Mutex;
    use std::{sync::Arc, time::Duration};
    use tokio::{
        io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
        net::TcpListener,
    };

    const AMBIENT_PROXY: &str = "http://127.0.0.1:9";

    fn proxied_builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(AMBIENT_PROXY).expect("a well-formed proxy uri"))
    }

    fn settings(endpoint: &str) -> ExporterSettings {
        ExporterSettings::new(
            endpoint,
            Transport::Http,
            "svc",
            "exe",
            "1.2.3",
            Duration::from_secs(5),
        )
        .expect("valid settings")
    }

    struct Receiver {
        endpoint: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl Receiver {
        async fn start(reply: &'static str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&requests);
            tokio::spawn(async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    stream.read_line(&mut line).await.unwrap();
                    let path = line.split(' ').nth(1).unwrap().to_owned();
                    let (mut host, mut length) = (String::new(), 0);
                    loop {
                        line.clear();
                        stream.read_line(&mut line).await.unwrap();
                        let header = line.trim_end();
                        if header.is_empty() {
                            break;
                        }
                        let (name, value) = header.split_once(": ").unwrap();
                        match name.to_ascii_lowercase().as_str() {
                            "host" => value.clone_into(&mut host),
                            "content-length" => length = value.parse().unwrap(),
                            _ => {}
                        }
                    }
                    let mut body = vec![0; length];
                    stream.read_exact(&mut body).await.unwrap();
                    recorded.lock().push(format!("{host}{path}"));
                    let reply = reply.replace("{endpoint}", &format!("http://{host}"));
                    stream.get_mut().write_all(reply.as_bytes()).await.unwrap();
                }
            });
            Self { endpoint, requests }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().clone()
        }
    }

    const ACCEPTED: &str = "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

    const REDIRECTED: &str = "HTTP/1.1 307 Temporary Redirect\r\nlocation: {endpoint}/elsewhere\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

    async fn shut_down(providers: impl FnOnce() + Send + 'static) {
        tokio::task::spawn_blocking(providers).await.unwrap();
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn both_signals_export_from_the_sdk_threads_to_their_full_signal_uris() {
        let receiver = Receiver::start(ACCEPTED).await;
        let settings = settings(&format!("{}/api/default", receiver.endpoint));
        let traces = settings.tracer_provider().expect("trace exporter builds");
        let logs = settings.logger_provider().expect("log exporter builds");

        traces.tracer("export-test").start("exported");
        let logger = logs.logger("export-test");
        logger.emit(logger.create_log_record());
        shut_down(move || {
            traces.shutdown().expect("traces exported");
            logs.shutdown().expect("logs exported");
        })
        .await;

        let authority = receiver.endpoint.trim_start_matches("http://");
        let mut requests = receiver.requests();
        requests.sort();
        assert_eq!(
            requests,
            [
                format!("{authority}/api/default/v1/logs"),
                format!("{authority}/api/default/v1/traces"),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_redirecting_receiver_gets_no_second_request() {
        let receiver = Receiver::start(REDIRECTED).await;
        let traces = settings(&receiver.endpoint)
            .tracer_provider()
            .expect("trace exporter builds");

        traces.tracer("redirect-test").start("redirected");
        shut_down(move || {
            traces.shutdown().expect("the processor stops");
        })
        .await;

        let authority = receiver.endpoint.trim_start_matches("http://");
        assert_eq!(receiver.requests(), [format!("{authority}/v1/traces")]);
    }

    #[test]
    fn the_http_client_outside_a_runtime_is_refused() {
        assert!(matches!(
            OtlpHttpClient::new(Duration::from_secs(5), None),
            Err(TelemetryError::NoRuntime)
        ));
    }

    #[test]
    fn a_panic_payload_keeps_its_message() {
        let literal = std::panic::catch_unwind(|| panic!("failed to create tokio runtime"))
            .expect_err("the closure panics");
        assert_eq!(panic_message(&*literal), "failed to create tokio runtime");

        let formatted = std::panic::catch_unwind(|| panic!("{} roots missing", 3))
            .expect_err("the closure panics");
        assert_eq!(panic_message(&*formatted), "3 roots missing");
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
    fn grpc_is_refused_and_http_is_the_default() {
        assert_eq!(Transport::default(), Transport::Http);
        assert!(matches!(
            ExporterSettings::new(
                "http://host",
                Transport::Grpc,
                "svc",
                "exe",
                "1.2.3",
                Duration::from_secs(5)
            ),
            Err(TelemetryError::GrpcTransport)
        ));
        assert!(serde_json::from_str::<Transport>("\"thrift\"").is_err());
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
    }

    #[test]
    fn settings_reject_endpoints_carrying_a_query_or_fragment() {
        let timeout = Duration::from_secs(5);
        for endpoint in ["http://host/api/default?org=x", "http://host/api/default#f"] {
            assert!(
                ExporterSettings::new(endpoint, Transport::Http, "svc", "exe", "1.2.3", timeout)
                    .is_err(),
                "accepted {endpoint}"
            );
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

    #[tokio::test]
    async fn both_signals_share_one_http_client() {
        let settings = settings("http://host");

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
    async fn exporters_trust_an_extra_ca() {
        let mut settings = settings("https://collector.internal:4318");
        settings.ca_certificate =
            Some(read_ca_certificate(std::path::Path::new(PRIVATE_ROOT)).expect("fixture root"));
        settings.tracer_provider().expect("trace exporter builds");
        settings.logger_provider().expect("log exporter builds");
    }
}

#[derive(Debug)]
struct EventArrayProcessor;

impl logs::LogProcessor for EventArrayProcessor {
    fn emit(&self, record: &mut logs::SdkLogRecord, _: &opentelemetry::InstrumentationScope) {
        use opentelemetry::logs::LogRecord as _;
        let context = opentelemetry::Context::current();
        if let Some(array) = context.get::<crate::EventStringArray>() {
            record.add_attribute(
                array.name,
                opentelemetry::logs::AnyValue::ListAny(Box::new(
                    array
                        .values
                        .iter()
                        .cloned()
                        .map(opentelemetry::logs::AnyValue::from)
                        .collect(),
                )),
            );
        }
    }

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }
    fn shutdown_with_timeout(&self, _: Duration) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }
}

#[cfg(test)]
mod event_array_tests {
    use super::*;
    use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _};
    use parking_lot::Mutex;
    use std::sync::Arc;

    #[derive(Clone, Debug, Default)]
    struct Captured(Arc<Mutex<Vec<logs::SdkLogRecord>>>);
    impl logs::LogProcessor for Captured {
        fn emit(&self, record: &mut logs::SdkLogRecord, _: &opentelemetry::InstrumentationScope) {
            self.0.lock().push(record.clone());
        }
        fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }
        fn shutdown_with_timeout(&self, _: Duration) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }
    }

    #[test]
    fn scoped_event_arrays_are_native_and_do_not_leak_to_the_next_record() {
        let capture = Captured::default();
        let provider = SdkLoggerProvider::builder()
            .with_log_processor(EventArrayProcessor)
            .with_log_processor(capture.clone())
            .build();
        let logger = provider.logger("array-test");
        crate::with_event_string_array("github.deliveries", &["first", "second"], || {
            let mut record = logger.create_log_record();
            record.add_attribute("outcome", "queued");
            logger.emit(record);
        });
        logger.emit(logger.create_log_record());
        let records = capture.0.lock();
        let (_, value) = records[0]
            .attributes_iter()
            .find(|(key, _)| key.as_str() == "github.deliveries")
            .unwrap();
        assert_eq!(
            value,
            &AnyValue::ListAny(Box::new(vec![
                AnyValue::from("first"),
                AnyValue::from("second")
            ]))
        );
        assert!(
            !records[1]
                .attributes_iter()
                .any(|(key, _)| key.as_str() == "github.deliveries")
        );
    }
}
