use std::{collections::BTreeSet, path::PathBuf};

use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_broker_protocol::{
    AgentInventory, ModelUsageReport, Permission, ReportedAgent, ReportedAgentCapability,
};
use tower::ServiceExt as _;

use crate::{Dashboard, OtelSummary, ServiceStatus, router};

fn echo_provider() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/providers/echo-provider.wasm")
}

async fn dashboard() -> Dashboard {
    let registry = BrokerProviderRegistry::load([echo_provider()], BrokerHostLimits::default())
        .await
        .expect("echo provider loads");
    let metrics = registry.metrics();
    let providers = registry.loaded_provider_metadata().collect();
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
    Dashboard::new(
        "0.5.0-test",
        providers,
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
    )
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
    let body = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads")
            .to_vec(),
    )
    .expect("HTML is UTF-8");
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
async fn provider_page_is_rustdoc_like_and_complete() {
    let dashboard = dashboard().await;
    let sha256 = dashboard
        .provider("echo")
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
    let body = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads")
            .to_vec(),
    )
    .expect("HTML is UTF-8");
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
}

#[tokio::test]
async fn provider_metadata_and_counters_are_populated_by_the_host() {
    let dashboard = dashboard().await;
    let provider = dashboard.provider("echo").expect("echo provider retained");
    assert!(provider.artifact_bytes > 0);
    assert_eq!(provider.artifact_sha256.len(), 64);
    assert!(
        provider
            .artifact_sha256
            .chars()
            .collect::<BTreeSet<_>>()
            .is_subset(&"0123456789abcdef".chars().collect())
    );
    let stats = dashboard.host_metrics.snapshot();
    assert_eq!(stats.providers_loaded, 1);
    assert_eq!(stats.component_compilations, 1);
    assert!(stats.stores_created >= 1);
    assert!(stats.fuel_observations >= 1);
}
