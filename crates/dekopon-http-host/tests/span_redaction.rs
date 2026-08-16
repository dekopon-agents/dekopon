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
use dekopon_http_host::{BufferedHttpClient, Header, HttpHostCeilings, Request};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<String>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Captured {
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
}
