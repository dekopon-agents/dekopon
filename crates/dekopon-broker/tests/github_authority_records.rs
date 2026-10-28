#![allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
#[path = "common/github.rs"]
mod github;
use dekopon_broker::AttestorGrant;
#[path = "common/broker.rs"]
mod broker_support;
use dekopon_test_support::CaptureLayer;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

type NativeRecord = std::collections::BTreeMap<&'static str, Vec<opentelemetry::Value>>;
#[derive(Clone, Default)]
struct NativeArrays(std::sync::Arc<std::sync::Mutex<Vec<NativeRecord>>>);
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for NativeArrays {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "dekopon_broker::audit" {
            return;
        }
        let context = opentelemetry::Context::current();
        if let Some(arrays) =
            context.get::<Vec<dekopon_telemetry::EventStringArray<opentelemetry::Value>>>()
        {
            self.0
                .lock()
                .unwrap()
                .push(arrays.iter().map(|a| (a.name, a.values.clone())).collect());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn broker_decision_names_the_waking_events() {
    let captured = CaptureLayer::workspace();
    let arrays = NativeArrays::default();
    tracing_subscriber::registry()
        .with(arrays.clone())
        .with(captured.clone())
        .init();
    let (broker,_)=github::broker(r#"@id("allow") permit(principal,action == Dekopon::Action::"cli-probe.reverse",resource) when { context has github && context.github.event == "issue_comment" };"#).await;
    let claim = github::claim(vec![
        github::event("issue_comment", "delivery-1"),
        github::event("pull_request", "delivery-2"),
    ]);
    let request = broker_support::request(97, "cli-probe.reverse");
    broker
        .invoke(
            &github::gateway(),
            Some(&AttestorGrant { namespaces: None }),
            Some(&claim.bound_to(request.id.clone())),
            request,
            Default::default(),
        )
        .await
        .unwrap();
    let records = captured.events_where(
        "dekopon_broker::audit",
        &[
            ("audit.event", "broker.decision"),
            ("invocation.id", "invoke-table-97"),
        ],
    );
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.field("github.denier"), Some("facts"));
    assert_eq!(record.field("github.denier.delivery"), Some("delivery-2"));
    let captured = arrays.0.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0]["github.delivery"],
        vec![
            opentelemetry::Value::String("delivery-1".into()),
            opentelemetry::Value::String("delivery-2".into())
        ]
    );
    assert_eq!(
        captured[0]["github.event"],
        vec![
            opentelemetry::Value::String("issue_comment".into()),
            opentelemetry::Value::String("pull_request.synchronize".into())
        ]
    );
    assert_eq!(
        captured[0]["github.actor.id"],
        vec![opentelemetry::Value::I64(1), opentelemetry::Value::I64(1)]
    );
}
