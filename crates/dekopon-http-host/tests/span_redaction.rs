//! Telemetry redaction for the native HTTP host.
//!
//! This lives in its own test binary on purpose. `tracing` caches per-callsite interest globally
//! and resolves it against the *global* dispatcher, so a sibling unit test that calls `send` with
//! no subscriber installed permanently disables the `http.request` callsite for the whole process.
//! A dedicated binary with one global subscriber is the only arrangement where this assertion is
//! not order-dependent.

use std::{
    io::{Read as _, Write as _},
    net::TcpListener,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use dekopon_capability::HttpConstraints;
use dekopon_http_host::{BufferedHttpClient, ErrorCode, Header, HttpHostCeilings, Request};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<String>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Captured {
    /// Only this workspace's callsites. An unfiltered layer over a registry also captures every
    /// transport callsite, which both drowns the assertions and makes them depend on a dependency's
    /// logging.
    fn enabled(
        &self,
        metadata: &tracing::Metadata<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        metadata.target().starts_with("dekopon")
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::Id,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut sink = self.0.lock().expect("capture sink");
        sink.push_str(attrs.metadata().name());
        attrs.record(&mut Visitor(&mut sink));
    }

    fn on_record(
        &self,
        _id: &tracing::Id,
        values: &tracing::span::Record<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut sink = self.0.lock().expect("capture sink");
        values.record(&mut Visitor(&mut sink));
    }

    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut sink = self.0.lock().expect("capture sink");
        sink.push_str(event.metadata().target());
        event.record(&mut Visitor(&mut sink));
    }
}

struct Visitor<'a>(&'a mut String);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

/// Minimal loopback server that reads one request and replies once.
fn mock_http() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
    let address = listener.local_addr().expect("fixture address");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept fixture request");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set fixture timeout");
        let mut buffer = [0_u8; 4096];
        #[allow(
            clippy::let_underscore_must_use,
            reason = "the fixture discards the request bytes by design and replies unconditionally; \
                      a read that actually failed breaks the exchange the span assertions below \
                      depend on"
        )]
        let _ = stream.read(&mut buffer);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
            .expect("write fixture response");
        stream.flush().expect("flush fixture response");
    });
    (format!("127.0.0.1:{}", address.port()), handle)
}

/// Telemetry is a second egress path for the same call the audit chain records, and it has none of
/// the audit chain's guarantees. A URL path, a query, a header, or a body reaching a span field
/// would undo the redaction `HttpCallEvidence` exists to enforce, so this drives a real request
/// whose every such component is a distinct sentinel and reads back what the span layer captured.
#[tokio::test]
async fn http_span_carries_evidence_fields_and_no_payload() {
    let captured = Captured::default();
    tracing_subscriber::registry().with(captured.clone()).init();

    let (authority, handle) = mock_http();
    let mut client = BufferedHttpClient::authorized(
        HttpConstraints {
            allowed_hosts: vec![authority.clone()],
            allowed_methods: vec!["POST".to_owned()],
            max_requests: 2,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: true,
        },
        HttpHostCeilings::default(),
        Duration::from_secs(5),
    )
    .expect("authorized fixture client");

    let response = client
        .send(Request {
            method: "POST".to_owned(),
            uri: format!("http://{authority}/SECRET_PATH?SECRET_QUERY=1"),
            headers: vec![Header {
                name: "x-probe".to_owned(),
                value: b"SECRET_HEADER".to_vec(),
            }],
            body: b"SECRET_BODY".to_vec(),
        })
        .await
        .expect("authorized loopback request succeeds");
    assert_eq!(response.status, 200);
    handle.join().expect("fixture server exits");

    let recorded = captured.0.lock().expect("capture sink").clone();

    // The sanitized set is present, so a failure below is redaction and not a dead span.
    assert!(recorded.contains("http.request"), "{recorded}");
    assert!(recorded.contains("POST"), "{recorded}");
    assert!(recorded.contains(&authority), "{recorded}");
    assert!(recorded.contains("200"), "{recorded}");

    for sentinel in [
        "SECRET_PATH",
        "SECRET_QUERY",
        "SECRET_HEADER",
        "SECRET_BODY",
    ] {
        assert!(
            !recorded.contains(sentinel),
            "{sentinel} leaked into a span field: {recorded}"
        );
    }

    // Same client, payloads enabled. The URL now appears, because the operator asked for it — and
    // headers and body still do not, because verbosity widens what spans carry, not everything.
    dekopon_core::set_telemetry_payloads(true);
    let (verbose_authority, verbose_handle) = mock_http();
    let mut verbose = BufferedHttpClient::authorized(
        HttpConstraints {
            allowed_hosts: vec![verbose_authority.clone()],
            allowed_methods: vec!["POST".to_owned()],
            max_requests: 2,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: true,
        },
        HttpHostCeilings::default(),
        Duration::from_secs(5),
    )
    .expect("verbose fixture client");

    captured.0.lock().expect("capture sink").clear();
    verbose
        .send(Request {
            method: "POST".to_owned(),
            uri: format!("http://{verbose_authority}/VERBOSE_PATH?VERBOSE_QUERY=1"),
            headers: vec![Header {
                name: "x-probe".to_owned(),
                value: b"STILL_SECRET_HEADER".to_vec(),
            }],
            body: b"STILL_SECRET_BODY".to_vec(),
        })
        .await
        .expect("verbose loopback request succeeds");
    verbose_handle.join().expect("verbose fixture exits");
    dekopon_core::set_telemetry_payloads(false);

    let verbose_recorded = captured.0.lock().expect("capture sink").clone();
    assert!(
        verbose_recorded.contains("VERBOSE_PATH"),
        "{verbose_recorded}"
    );
    assert!(
        verbose_recorded.contains("VERBOSE_QUERY"),
        "{verbose_recorded}"
    );
    for sentinel in ["STILL_SECRET_HEADER", "STILL_SECRET_BODY"] {
        assert!(
            !verbose_recorded.contains(sentinel),
            "{sentinel} leaked even though only URLs were opted into: {verbose_recorded}"
        );
    }

    refusals_carry_their_failure_class_and_are_still_accounted(&captured).await;
}

/// A refusal that never reaches `prepare` has no method or authority to report, and used to reach
/// telemetry as a bare `outcome` with no reason and no accounting record at all — six failure
/// classes flattened into one word. Recording the class and its message is safe because every
/// message this crate produces is a static, pre-sanitized `&str`, which this phase also pins: the
/// refused URL's path must not appear anywhere.
///
/// This runs inside the one test that owns the global subscriber, for the reason the module
/// comment gives.
async fn refusals_carry_their_failure_class_and_are_still_accounted(captured: &Captured) {
    captured.0.lock().expect("capture sink").clear();

    let mut client = BufferedHttpClient::authorized(
        HttpConstraints {
            allowed_hosts: vec!["127.0.0.1:9".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            max_requests: 2,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: true,
        },
        HttpHostCeilings::default(),
        Duration::from_secs(5),
    )
    .expect("authorized fixture client");

    let error = client
        .send(Request {
            method: "GET".to_owned(),
            uri: "http://127.0.0.1:10/REFUSED_PATH?REFUSED_QUERY=1".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        })
        .await
        .expect_err("an unauthorized destination is refused");
    assert_eq!(error.code, ErrorCode::Denied);

    let recorded = captured.0.lock().expect("capture sink").clone();
    assert!(recorded.contains("Denied"), "{recorded}");
    assert!(
        recorded.contains("HTTP destination is not authorized for this invocation"),
        "{recorded}"
    );
    assert!(recorded.contains("denied"), "{recorded}");
    // The attempt consumed a unit of the request budget, so it is accounted even though nothing
    // reached the wire — with the fields it cannot know absent rather than zero.
    assert!(recorded.contains("accounting.http.request"), "{recorded}");
    assert!(!recorded.contains("status_code"), "{recorded}");

    for sentinel in ["REFUSED_PATH", "REFUSED_QUERY"] {
        assert!(
            !recorded.contains(sentinel),
            "{sentinel} leaked into failure telemetry: {recorded}"
        );
    }
}

/// A credential inside a payload stays redacted whichever mode the process runs in. This is the
/// property that makes "the sink is in scope for our data" a safe statement to make: it is a
/// statement about data, and a credential is not data the operator agreed to retain.
#[test]
fn redacted_values_survive_verbose_mode() {
    use dekopon_core::Redacted;

    let secret = Redacted::new("sk-live-abcdef0123456789".to_owned());
    for enabled in [false, true] {
        dekopon_core::set_telemetry_payloads(enabled);
        assert!(!format!("{secret}").contains("sk-live"));
        assert!(!format!("{secret:?}").contains("sk-live"));
        assert!(
            !serde_json::to_string(&secret)
                .expect("redacted serializes")
                .contains("sk-live")
        );
    }
    dekopon_core::set_telemetry_payloads(false);
}
