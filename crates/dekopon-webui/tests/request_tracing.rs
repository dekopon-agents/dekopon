//! Per-request tracing on the informational listener.
//!
//! Its own test binary because `tracing` caches per-callsite interest against the *global*
//! dispatcher: a sibling test that drives the router with no subscriber installed would
//! permanently disable the `webui_request` callsite for the whole process.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    http::{Method, Request},
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_webui::{Dashboard, ServiceStatus, router};
use tower::ServiceExt as _;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<String>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Captured {
    /// Wasmtime's Cranelift backend is densely instrumented, and this binary compiles a real
    /// component. Deciding interest here — not in `register_callsite` — is what actually keeps
    /// those callsites disabled when the inner subscriber is a registry.
    fn enabled(
        &self,
        metadata: &tracing::Metadata<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        metadata.target().starts_with("dekopon")
    }

    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut sink = self.0.lock().expect("capture sink");
        event.record(&mut Visitor(&mut sink));
    }
}

struct Visitor<'a>(&'a mut String);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

/// An operator diagnosing unexpected LAN traffic to an unauthenticated listener needs to see the
/// requests. `debug` keeps a production `info` filter shipping nothing, and the path is recorded
/// without its query string so scanning is visible without the scan's payload being retained.
#[tokio::test]
async fn every_request_emits_one_debug_event_with_no_payload() {
    let captured = Captured::default();
    tracing_subscriber::registry().with(captured.clone()).init();

    let registry = BrokerProviderRegistry::load(
        [PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/providers/echo-provider.wasm")],
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

    let recorded = captured.0.lock().expect("capture sink").clone();
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
