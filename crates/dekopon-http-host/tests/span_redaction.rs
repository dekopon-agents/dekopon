//! Telemetry redaction for the native HTTP host.
//!
//! This lives in its own test binary on purpose. `tracing` caches per-callsite interest globally
//! and resolves it against the *global* dispatcher, so a sibling unit test that calls `send` with
//! no subscriber installed permanently disables the `http.request` callsite for the whole process.
//! A dedicated binary with one global subscriber is the only arrangement where this assertion is
//! not order-dependent.

use std::time::Duration;

use dekopon_capability::HttpConstraints;
use dekopon_http_host::{BufferedHttpClient, ErrorCode, Header, HttpHostCeilings, Request};
use dekopon_test_support::{CaptureLayer, LoopbackServer};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

/// The span names the egress in full — method, authority, status, and `url.full` with its path and
/// query — and stops exactly at the request and response headers and bodies. A credential is
/// injected into a header at this boundary, so a header or body field would hand the trace the one
/// thing goal 1 keeps out of it. This drives a real request whose every component is a distinct
/// sentinel and reads back what the span layer captured.
#[tokio::test]
async fn http_span_carries_evidence_fields_and_no_payload() {
    let captured = CaptureLayer::workspace();
    tracing_subscriber::registry().with(captured.clone()).init();

    let server = LoopbackServer::once(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
    let authority = server.authority().to_owned();
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
            uri: format!("http://{authority}/TRACED_PATH?TRACED_QUERY=1"),
            headers: vec![Header {
                name: "x-probe".to_owned(),
                value: b"SECRET_HEADER".to_vec(),
            }],
            body: b"SECRET_BODY".to_vec(),
        })
        .await
        .expect("authorized loopback request succeeds");
    assert_eq!(response.status, 200);
    server.join();

    let recorded = captured.text();

    // The whole exported set is present, so a failure below is redaction and not a dead span.
    assert!(recorded.contains("http.request"), "{recorded}");
    assert!(recorded.contains("POST"), "{recorded}");
    assert!(recorded.contains(&authority), "{recorded}");
    assert!(recorded.contains("200"), "{recorded}");
    assert!(recorded.contains("TRACED_PATH"), "{recorded}");
    assert!(recorded.contains("TRACED_QUERY"), "{recorded}");

    for sentinel in ["SECRET_HEADER", "SECRET_BODY"] {
        assert!(
            !recorded.contains(sentinel),
            "{sentinel} leaked into a span field: {recorded}"
        );
    }

    refusals_carry_their_failure_class_and_are_still_accounted(&captured).await;
}

/// A refusal that never reaches `prepare` has no method or authority to report, and used to reach
/// telemetry as a bare `outcome` with no reason and no accounting record at all — six failure
/// classes flattened into one word. Recording the class and its message is safe because every
/// message this crate produces is a static, pre-sanitized `&str`.
///
/// The refused destination is on the span too: a denied egress is an egress the trace has to be
/// able to name, and "which host did the model keep trying to reach" is the whole value of a refusal
/// record. Its headers and body are not, and cannot be — the refusal lands before any of that is
/// touched, which this phase pins with sentinels the request really carried.
///
/// This runs inside the one test that owns the global subscriber, for the reason the module
/// comment gives.
async fn refusals_carry_their_failure_class_and_are_still_accounted(captured: &CaptureLayer) {
    captured.clear();

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
            headers: vec![Header {
                name: "x-probe".to_owned(),
                value: b"REFUSED_SECRET_HEADER".to_vec(),
            }],
            body: b"REFUSED_SECRET_BODY".to_vec(),
        })
        .await
        .expect_err("an unauthorized destination is refused");
    assert_eq!(error.code, ErrorCode::Denied);

    let recorded = captured.text();
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
    // The destination the model reached for, refused or not.
    assert!(recorded.contains("REFUSED_PATH"), "{recorded}");
    assert!(recorded.contains("REFUSED_QUERY"), "{recorded}");

    for sentinel in ["REFUSED_SECRET_HEADER", "REFUSED_SECRET_BODY"] {
        assert!(
            !recorded.contains(sentinel),
            "{sentinel} leaked into failure telemetry: {recorded}"
        );
    }
}

/// A credential inside a payload renders its marker on every path a span field can take. This is
/// the property that makes "the operator's trace is complete" a safe statement to make: completeness
/// is about the data the operator handles, and a credential is not that data.
#[test]
fn redacted_values_never_render_their_secret() {
    use dekopon_core::Redacted;

    let secret = Redacted::new("sk-live-abcdef0123456789".to_owned());
    assert!(!format!("{secret}").contains("sk-live"));
    assert!(!format!("{secret:?}").contains("sk-live"));
    assert!(
        !serde_json::to_string(&secret)
            .expect("redacted serializes")
            .contains("sk-live")
    );
}
