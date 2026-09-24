//! This must stay a separate test binary: tracing caches per-callsite interest against the global
//! dispatcher, so a sibling test calling send without a subscriber would permanently disable this
//! callsite process-wide.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use dekopon_capability::HttpConstraints;
use dekopon_http_host::{BufferedHttpClient, ErrorCode, Header, HttpHostCeilings, Request};
use dekopon_test_support::{CaptureLayer, LoopbackServer};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

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
    assert!(recorded.contains("accounting.http.request"), "{recorded}");
    assert!(!recorded.contains("status_code"), "{recorded}");
    assert!(recorded.contains("REFUSED_PATH"), "{recorded}");
    assert!(recorded.contains("REFUSED_QUERY"), "{recorded}");

    for sentinel in ["REFUSED_SECRET_HEADER", "REFUSED_SECRET_BODY"] {
        assert!(
            !recorded.contains(sentinel),
            "{sentinel} leaked into failure telemetry: {recorded}"
        );
    }
}

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
