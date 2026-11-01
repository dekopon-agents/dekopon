#![allow(clippy::unwrap_used, clippy::disallowed_methods)]

use dekopon_capability::InvocationOutcome;
use dekopon_test_support::{CaptureLayer, LoopbackServer, Record};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

#[path = "common/http_probe.rs"]
mod http_probe;

use http_probe::{http_probe_broker, invoke_as_caller, request};

fn only(captured: &CaptureLayer, fields: &[(&str, &str)]) -> Record {
    let matching = captured.events_where("dekopon_broker::audit", fields);
    let [record] = <[Record; 1]>::try_from(matching).unwrap_or_else(|matching| {
        panic!(
            "{} audit records matched {fields:?}: {matching:?}",
            matching.len()
        )
    });
    record
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_request_ids_alone_reach_execution_evidence() {
    let captured = CaptureLayer::workspace();
    tracing_subscriber::registry().with(captured.clone()).init();
    for (index, request_id) in ["github-request-1".to_owned(), "x".repeat(65)]
        .into_iter()
        .enumerate()
    {
        let server = LoopbackServer::once(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-GitHub-Request-Id: {request_id}\r\nX-Private-Header: private-value\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{{\"ok\":true}}").as_bytes());
        let authority = server.authority().to_owned();
        let broker = http_probe_broker(&authority).await;
        let id = format!("upstream-{index}");
        let result = invoke_as_caller(
            &broker,
            request(
                &id,
                "http-probe.fetch",
                serde_json::json!({"uri": format!("http://{authority}/"), "method": "GET"}),
            ),
        )
        .await
        .expect("invocation");
        assert_eq!(result.result.outcome, InvocationOutcome::Succeeded);
        server.join();
        let execution = only(
            &captured,
            &[("audit.event", "broker.execution"), ("invocation.id", &id)],
        );
        let calls = execution.field("http.calls").expect("HTTP evidence");
        let calls: serde_json::Value = serde_json::from_str(calls).expect("evidence JSON");
        assert_eq!(
            calls[0]
                .get("upstreamRequestId")
                .and_then(serde_json::Value::as_str),
            (index == 0).then_some(request_id.as_str())
        );
        assert_eq!(execution.field("vm.session"), None);
        assert_eq!(calls[0].get("vmSession"), None);
        let record = format!("{execution:?}");
        assert!(!record.to_ascii_lowercase().contains("x-private-header"));
        assert!(!record.contains("private-value"));
    }
}
