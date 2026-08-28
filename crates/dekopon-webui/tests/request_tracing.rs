//! Per-request tracing on the informational listener.
//!
//! Its own test binary because `tracing` caches per-callsite interest against the *global*
//! dispatcher: a sibling test that drives the router with no subscriber installed would
//! permanently disable the `webui_request` callsite for the whole process.

use axum::{
    body::Body,
    http::{Method, Request},
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_test_support::{CaptureLayer, provider_fixture};
use dekopon_webui::{Dashboard, ServiceStatus, router};
use tower::ServiceExt as _;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

/// An operator diagnosing unexpected LAN traffic to an unauthenticated listener needs to see the
/// requests. `debug` keeps a production `info` filter shipping nothing, and the path is recorded
/// without its query string so scanning is visible without the scan's payload being retained.
#[tokio::test]
async fn every_request_emits_one_debug_event_with_no_payload() {
    let captured = CaptureLayer::workspace();
    tracing_subscriber::registry().with(captured.clone()).init();

    let registry = BrokerProviderRegistry::load(
        [provider_fixture("echo-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("echo provider loads");
    let dashboard = Dashboard::new(
        "0.5.0-test",
        registry.loaded_provider_metadata().collect(),
        registry.metrics(),
        ServiceStatus::default(),
        None,
    );
    let app = router(dashboard);

    let served = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ui/providers/echo?SECRET_QUERY=1")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router answers");
    assert_eq!(served.status(), axum::http::StatusCode::OK);

    let scanned = app
        .oneshot(
            Request::builder()
                .method(Method::HEAD)
                .uri("/wp-login.php")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router answers");
    assert_eq!(scanned.status(), axum::http::StatusCode::NOT_FOUND);

    let recorded = captured.events_text();
    assert!(recorded.contains("webui_request"), "{recorded}");
    assert!(recorded.contains("/ui/providers/echo"), "{recorded}");
    assert!(recorded.contains("http.status=200"), "{recorded}");
    assert!(recorded.contains("/wp-login.php"), "{recorded}");
    assert!(recorded.contains("http.status=404"), "{recorded}");
    assert!(recorded.contains("http.response_bytes="), "{recorded}");
    assert!(
        !recorded.contains("SECRET_QUERY"),
        "a query string is not part of the recorded path: {recorded}"
    );
}
