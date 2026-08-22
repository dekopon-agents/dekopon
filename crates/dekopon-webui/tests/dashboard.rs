//! Router, rendering, escaping, and listener-ceiling coverage for the informational web UI.
//!
//! These live in `tests/` rather than `src/` because their fixture is the workspace's generated
//! `echo-provider.wasm`, which is deliberately outside the published package: an in-package test
//! that cannot find its fixture fails for every downstream packager who runs `cargo test`.

use std::{
    collections::BTreeSet,
    io::{Read as _, Write as _},
    net::TcpStream,
    path::PathBuf,
    time::Duration,
};

use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry, LoadedProviderMetadata};
use dekopon_broker_protocol::{
    AgentInventory, ModelUsageReport, Permission, ReportedAgent, ReportedAgentCapability,
};
use dekopon_webui::{
    Dashboard, OtelSummary, ServiceStatus, WebUiLimits, router, serve_with_limits,
};
use tower::ServiceExt as _;

fn echo_provider() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/providers/echo-provider.wasm")
}

async fn loaded() -> (Vec<LoadedProviderMetadata>, Dashboard) {
    let registry = BrokerProviderRegistry::load([echo_provider()], BrokerHostLimits::default())
        .await
        .expect("echo provider loads");
    let metrics = registry.metrics();
    let providers: Vec<_> = registry.loaded_provider_metadata().collect();
    let status = ServiceStatus::default();
    status.replace_agents(AgentInventory {
        agents: vec![ReportedAgent {
            id: "reviewer".parse().expect("valid agent"),
            description: "Reviews \0<script>alert('x')</script>".to_owned(),
            enabled: true,
            model_class: Some("reasoning".to_owned()),
            providers: vec!["echo".parse().expect("valid provider")],
            capabilities: vec![ReportedAgentCapability {
                id: "echo.echo".parse().expect("valid capability"),
                provider: "echo".parse().expect("valid provider"),
                permissions: vec![Permission {
                    operation: "messages:read".to_owned(),
                    resource: Some("team<&>".to_owned()),
                }],
            }],
        }],
        truncated: false,
    });
    status.record_usage(ModelUsageReport {
        model_calls: 2,
        input_tokens: 1_234,
        cached_input_tokens: 500,
        output_tokens: 56,
        reasoning_output_tokens: 20,
        total_tokens: 1_290,
        cached_input_unreported_calls: 1,
        ..ModelUsageReport::default()
    });
    let dashboard = Dashboard::new(
        "0.5.0-test",
        providers.clone(),
        metrics,
        status,
        Some(OtelSummary {
            endpoint: "http://observe.example/api/default".to_owned(),
            transport: "grpc".to_owned(),
            service_name: "dekopon-brokerd".to_owned(),
            export_timeout_ms: 5_000,
            telemetry_payloads: false,
            headers_configured: true,
            resource_attributes_configured: true,
        }),
    );
    (providers, dashboard)
}

async fn dashboard() -> Dashboard {
    loaded().await.1
}

async fn body_of(response: axum::response::Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads")
            .to_vec(),
    )
    .expect("HTML is UTF-8")
}

#[tokio::test]
async fn root_permanently_redirects_to_ui() {
    let response = router(dashboard().await)
        .oneshot(
            Request::builder()
                .uri("/")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router answers");

    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.headers()[header::LOCATION], "/ui");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn dashboard_renders_live_sections_and_escapes_reported_text() {
    let response = router(dashboard().await)
        .oneshot(
            Request::builder()
                .uri("/ui")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router answers");

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().contains_key("content-security-policy"),
        "an unauthenticated page still needs a closed content policy"
    );
    let body = body_of(response).await;
    for expected in [
        "Agents",
        "Providers",
        "Wasmtime",
        "OpenTelemetry",
        "1,234",
        "echo.echo",
        "messages:read",
        "observe.example",
    ] {
        assert!(body.contains(expected), "missing {expected:?} in {body}");
    }
    assert!(body.contains("&#x0;&lt;script&gt;"), "{body}");
    assert!(!body.contains("<script>alert"), "{body}");
    assert!(body.contains("team&lt;&amp;&gt;"), "{body}");
}

#[tokio::test]
async fn dashboard_reports_the_hosts_own_fuel_yield_interval() {
    let response = router(dashboard().await)
        .oneshot(
            Request::builder()
                .uri("/ui")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router answers");
    let body = body_of(response).await;

    // The value the broker host actually configures, not a formula this crate re-derives.
    let cell = body
        .split("<tr><th>Fuel yield interval</th><td class=mono>")
        .nth(1)
        .and_then(|rest| rest.split("</td>").next())
        .expect("the engine configuration table names the yield interval");
    assert_eq!(
        cell.replace(',', ""),
        BrokerHostLimits::default()
            .fuel_yield_interval()
            .to_string()
    );
}

#[tokio::test]
async fn provider_page_is_rustdoc_like_and_complete() {
    let (providers, dashboard) = loaded().await;
    let sha256 = providers
        .iter()
        .find(|provider| provider.manifest.id.as_str() == "echo")
        .expect("echo provider retained")
        .artifact_sha256
        .clone();
    let response = router(dashboard)
        .oneshot(
            Request::builder()
                .uri("/ui/providers/echo")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router answers");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    let body = body_of(response).await;
    for expected in [
        "pub capability",
        "echo.echo",
        "Complete manifest",
        "SHA-256",
        "Component interface",
        "inputSchema",
    ] {
        assert!(body.contains(expected), "missing {expected:?} in {body}");
    }
    assert!(body.contains(&sha256), "provider digest should be visible");
}

/// The Artifact table and the complete manifest below it describe the same field, so they must
/// agree: rendering the Rust variant name showed operators an identifier the wire never uses.
#[tokio::test]
async fn manifest_api_version_matches_the_serialized_manifest() {
    let response = router(dashboard().await)
        .oneshot(
            Request::builder()
                .uri("/ui/providers/echo")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router answers");
    let body = body_of(response).await;

    assert!(
        body.contains("<code>dekopon.dev/provider/v1alpha1</code>"),
        "{body}"
    );
    assert!(
        !body.contains("V1Alpha1"),
        "the Rust Debug spelling is not the manifest contract: {body}"
    );
}

#[tokio::test]
async fn unknown_pages_are_404_and_mutating_methods_are_405() {
    let app = router(dashboard().await);
    let missing = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ui/providers/absent")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router answers");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    let post = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/ui")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router answers");
    assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);
    // Axum authors this response, not this crate; the closed policy set still has to be on it.
    assert_eq!(post.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(post.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(post.headers()[header::REFERRER_POLICY], "no-referrer");
    assert!(
        post.headers()["content-security-policy"]
            .to_str()
            .expect("ASCII policy")
            .contains("default-src 'none'")
    );
}

#[tokio::test]
async fn provider_metadata_and_counters_are_populated_by_the_host() {
    let (providers, dashboard) = loaded().await;
    let provider = providers
        .iter()
        .find(|provider| provider.manifest.id.as_str() == "echo")
        .expect("echo provider retained");
    assert!(provider.artifact_bytes > 0);
    assert_eq!(provider.artifact_sha256.len(), 64);
    assert!(
        provider
            .artifact_sha256
            .chars()
            .collect::<BTreeSet<_>>()
            .is_subset(&"0123456789abcdef".chars().collect())
    );
    let response = router(dashboard)
        .oneshot(
            Request::builder()
                .uri("/ui")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router answers");
    let body = body_of(response).await;
    for expected in [
        "<tr><th>Components compiled</th><td class=mono>1</td></tr>",
        "<tr><th>Manifest descriptions</th><td class=mono>1</td></tr>",
    ] {
        assert!(body.contains(expected), "missing {expected:?} in {body}");
    }
}

/// A saturated listener must refuse, not queue: a queued connection is retained memory and a
/// retained descriptor inside the same 1Gi process whose OOM kill is the worst deployment failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saturated_listener_refuses_further_connections() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback bind");
    let address = listener.local_addr().expect("listener address");
    let (shutdown_send, shutdown_receive) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve_with_limits(
        listener,
        dashboard().await,
        WebUiLimits {
            max_connections: 1,
            connection_timeout: Duration::from_secs(30),
        },
        async {
            let _ = shutdown_receive.await;
        },
    ));

    // One connection that occupies the only permit without ever completing a request.
    let mut held = TcpStream::connect(address).expect("first connection is accepted");
    held.write_all(b"GET /ui HTTP/1.1\r\n")
        .expect("partial request writes");
    held.flush().expect("partial request flushes");
    // The server task has to reach `accept` before the second connection is attempted.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut refused = TcpStream::connect(address).expect("second connection reaches the listener");
    refused
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout applies");
    let _ = refused.write_all(b"GET /ui HTTP/1.1\r\nhost: localhost\r\n\r\n");
    let mut answer = Vec::new();
    // Closed without ever being read, so the peer sees either a clean EOF or a reset — never a
    // response, and never an open connection waiting for a permit.
    match refused.read_to_end(&mut answer) {
        Ok(_) => assert!(
            answer.is_empty(),
            "a refused connection gets no response: {}",
            String::from_utf8_lossy(&answer)
        ),
        Err(error) => assert_eq!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset,
            "a refused connection closes rather than hanging"
        ),
    }

    // The permit returns when the holder goes away, so the ceiling is a ceiling, not a fuse.
    drop(held);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut served = TcpStream::connect(address).expect("third connection is accepted");
    served
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout applies");
    served
        .write_all(b"GET /ui HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .expect("request writes");
    let mut answer = Vec::new();
    served.read_to_end(&mut answer).expect("response reads");
    assert!(
        String::from_utf8_lossy(&answer).starts_with("HTTP/1.1 200 OK"),
        "{}",
        String::from_utf8_lossy(&answer)
    );

    let _ = shutdown_send.send(());
    server
        .await
        .expect("server task exits")
        .expect("server shuts down cleanly");
}

/// The deadline covers the whole connection, so a client that opens one and then stalls is cut
/// loose instead of pinning a permit and a rendered response indefinitely.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_connection_is_closed_when_its_deadline_elapses() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback bind");
    let address = listener.local_addr().expect("listener address");
    let (shutdown_send, shutdown_receive) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve_with_limits(
        listener,
        dashboard().await,
        WebUiLimits {
            max_connections: 4,
            connection_timeout: Duration::from_millis(300),
        },
        async {
            let _ = shutdown_receive.await;
        },
    ));

    let stalled = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(address).expect("connection is accepted");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout applies");
        stream
            .write_all(b"GET /ui HTTP/1.1\r\n")
            .expect("partial request writes");
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).expect("connection closes");
        answer
    });
    assert!(
        stalled.await.expect("stalled client finishes").is_empty(),
        "a connection cut at its deadline never produced a response"
    );

    let _ = shutdown_send.send(());
    server
        .await
        .expect("server task exits")
        .expect("server shuts down cleanly");
}

#[tokio::test]
async fn zero_ceilings_are_refused_rather_than_serving_unbounded() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback bind");
    let error = serve_with_limits(
        listener,
        dashboard().await,
        WebUiLimits {
            max_connections: 0,
            connection_timeout: Duration::from_secs(30),
        },
        std::future::pending(),
    )
    .await
    .expect_err("an unbounded listener is not a valid configuration");
    assert!(matches!(error, dekopon_webui::WebUiError::InvalidLimits));
}
