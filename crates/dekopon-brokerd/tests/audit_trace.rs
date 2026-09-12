//! The W3C trace identifier every audit record has to carry, on both entrances to the broker.
//!
//! Nothing puts that identifier into the record's own fields, and nothing should: `dekopon-broker`
//! links no telemetry SDK. It carries the id because it is emitted *inside* the span that made the
//! decision, which descends from the `broker.invocation` span that adopted the client's
//! `traceparent` — so the console JSON formatter and the OTLP log bridge each stamp the live ids on
//! it. This test reads the same native context those two read, from the same place, at the moment
//! the record is emitted.
//!
//! Both entrances are covered because they build their span differently: `Invoke` names the
//! capability and any attested subject, while the storage-routed `RecordDeliveredTurn` deliberately
//! drops all of it. Only the parenting is shared, and it is the parenting under test.
//!
//! Its own test binary, because `tracing` caches per-callsite interest against the global
//! dispatcher for the whole process.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use dekopon_broker::{
    Broker, BrokerLimits, CapabilityRoute, ConstraintCatalog, ConstraintSet, CredentialStore,
    IdentityDirectory, InvocationRequest, PolicyEngine, PolicyWorld, TraceOnlyAuditLog,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_broker_protocol::{
    Attestation, BrokerClient, ChatScopeClaim, ChatTransportKind, DeliveredTurnRequest,
    DeliveryIdentity, FrameLimits, TraceParent,
};
use dekopon_brokerd::{BrokerServer, MappedPeer, ServerLimits, current_uid};
use dekopon_capability::{EffectKind, ExecutionConstraints, InvocationOutcome};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId, ProviderId,
    RiskLevel, TransportId,
};
use dekopon_test_support::{provider_fixture, shutdown_on};
use opentelemetry::trace::TraceContextExt as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tokio::{net::UnixListener, sync::oneshot};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

/// The trace the client claims to be part of; every audit record it causes must name it.
const CLIENT_TRACE_ID: [u8; 16] = [
    0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e, 0x47, 0x36,
];
const CLIENT_SPAN_ID: [u8; 8] = [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7];

const POLICY: &str = r#"
@id("caller-echo")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"echo.echo",
       resource == Dekopon::Provider::"echo")
when { context has agent && context.agent == "brokerd-test" }
unless { context has via };
"#;

/// One audit record, read exactly as the console formatter and the log bridge read it.
#[derive(Clone, Debug)]
struct Captured {
    fields: String,
    trace_id: String,
    scope: Vec<String>,
}

#[derive(Clone, Default)]
struct AuditProbe(Arc<Mutex<Vec<Captured>>>);

impl<S> tracing_subscriber::Layer<S> for AuditProbe
where
    S: tracing::Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() != "dekopon_broker::audit" {
            return;
        }
        let mut fields = String::new();
        event.record(&mut Visitor(&mut fields));
        // The formatter's own source: the OTel layer activated this context on span entry.
        let native = opentelemetry::Context::current();
        let trace_id = native.span().span_context().trace_id().to_string();
        let scope = context
            .event_span(event)
            .into_iter()
            .flat_map(|span| span.scope())
            .map(|span| span.metadata().name().to_owned())
            .collect();
        self.0.lock().expect("probe sink").push(Captured {
            fields,
            trace_id,
            scope,
        });
    }
}

struct Visitor<'a>(&'a mut String);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

fn principal(name: &str) -> PrincipalId {
    name.parse().expect("valid principal fixture")
}

fn trace_parent() -> TraceParent {
    TraceParent::new(CLIENT_TRACE_ID, CLIENT_SPAN_ID, 1).expect("valid W3C parent fixture")
}

fn chat_claim() -> Attestation {
    Attestation::for_chat(
        "slack.t0123abc.u9xyz"
            .parse::<ExternalSubject>()
            .expect("canonical subject fixture"),
        "chat-agent"
            .parse::<AgentId>()
            .expect("valid agent fixture"),
        ChatScopeClaim {
            transport: "scientist-slack"
                .parse::<TransportId>()
                .expect("valid transport fixture"),
            kind: ChatTransportKind::Slack,
            channel: "c0123abc".to_owned(),
            conversation: "c0123abc:1712345678.000100".to_owned(),
        },
    )
}

fn bind_fixture(path: &Path) -> UnixListener {
    let listener = UnixListener::bind(path).expect("bind server fixture");
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("secure server fixture");
    listener
}

async fn broker() -> Arc<Broker<TraceOnlyAuditLog>> {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("echo-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("load echo fixture");
    let world = PolicyWorld::new(
        [principal("caller")],
        [(
            "echo.echo".parse::<CapabilityId>().expect("capability"),
            "echo".parse::<ProviderId>().expect("provider"),
        )],
    )
    .expect("distinct fixtures build a world");
    Arc::new(
        Broker::new(
            registry,
            principal("broker-test"),
            "policy-test".to_owned(),
            PolicyEngine::new(POLICY, &world).expect("fixture policy validates"),
            ConstraintCatalog::new([(
                "echo.echo".parse::<CapabilityId>().expect("capability"),
                ConstraintSet {
                    route: CapabilityRoute::Generic,
                    provider: "echo".parse::<ProviderId>().expect("provider"),
                    effect: EffectKind::ReadOnly,
                    risk: RiskLevel::Low,
                    credential: None,
                    credential_by_agent: BTreeMap::new(),
                    constraints: ExecutionConstraints::default(),
                },
            )])
            .expect("one capability builds a catalog"),
            CredentialStore::empty(),
            IdentityDirectory::empty(),
            Arc::new(TraceOnlyAuditLog),
            BrokerLimits::default(),
        )
        .expect("broker starts"),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn every_audit_record_carries_the_client_s_w3c_trace_id() {
    let probe = AuditProbe::default();
    // A provider with no exporter still mints span contexts, which is all the formatter and the
    // log bridge read; nothing here needs a receiver.
    let provider = SdkTracerProvider::builder().build();
    tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(
            opentelemetry::trace::TracerProvider::tracer(&provider, "audit-trace-test"),
        ))
        .with(probe.clone())
        .init();

    let directory = tempfile::tempdir().expect("create fixture directory");
    std::fs::set_permissions(
        directory.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("private fixture directory");
    let socket_path = directory.path().join("broker.sock");
    let listener = bind_fixture(&socket_path);
    let uid = current_uid();
    let server = BrokerServer::new(
        broker().await,
        BTreeMap::from([(
            uid,
            MappedPeer {
                context: dekopon_broker::AuthenticatedContext::new(
                    principal("caller"),
                    Actor::Agent {
                        agent: "brokerd-test".parse::<AgentId>().expect("valid agent"),
                    },
                )
                .expect("trusted context binds"),
                attestor: None,
            },
        )]),
        ServerLimits {
            frame: FrameLimits {
                max_frame_bytes: 64 * 1024,
                io_timeout: Duration::from_secs(2),
            },
            max_connections: 4,
            shutdown_grace: Duration::from_secs(2),
        },
    )
    .expect("server starts");
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, shutdown_on(stopped)));
    let client = BrokerClient::new(&socket_path, uid, FrameLimits::default()).expect("client");

    let invoked = client
        .invoke(
            None,
            InvocationRequest {
                id: "invoke-traced".parse::<InvocationId>().expect("invocation"),
                capability: "echo.echo".parse::<CapabilityId>().expect("capability"),
                trace_parent: trace_parent(),
                secret_use: None,
                input: serde_json::json!({"message": "hello through broker"}),
            },
        )
        .await
        .expect("the authorized invocation completes");
    assert_eq!(invoked.outcome, InvocationOutcome::Succeeded);

    // The storage-routed entrance. No chat memory is configured, so this is an audited refusal —
    // which is the point: a refusal is a decision, and it has to land in the caller's trace too.
    let refused = client
        .record_delivered_turn(
            chat_claim(),
            DeliveredTurnRequest {
                id: "record-traced".parse::<InvocationId>().expect("invocation"),
                trace_parent: trace_parent(),
                delivery: DeliveryIdentity::Slack {
                    channel: "c0123abc".to_owned(),
                    timestamp: "1712345678.000100".to_owned(),
                },
                user: "a question".to_owned(),
                assistant: "an answer".to_owned(),
            },
        )
        .await
        .expect("a refused turn is still an accounted decision");
    assert_eq!(refused.outcome, InvocationOutcome::Denied);

    stop.send(()).expect("signal clean shutdown");
    task.await
        .expect("server task exits")
        .expect("server shuts down");

    let records = probe.0.lock().expect("probe sink").clone();
    let expected = "4bf92f3577b34da6a3ce929d0e0e4736";
    for invocation in ["invoke-traced", "record-traced"] {
        let matching = records
            .iter()
            .filter(|record| {
                record
                    .fields
                    .contains(&format!("invocation.id={invocation}"))
            })
            .collect::<Vec<_>>();
        assert!(
            !matching.is_empty(),
            "{invocation} produced no audit record: {records:?}"
        );
        for record in matching {
            assert_eq!(
                record.trace_id, expected,
                "an audit record left the client's trace: {record:?}"
            );
            assert!(
                record.scope.contains(&"broker.invocation".to_owned()),
                "an audit record was emitted outside the invocation span: {record:?}"
            );
        }
    }
    // Both entrances, not one twice.
    assert!(
        records
            .iter()
            .any(|record| record.fields.contains("broker.execution")),
        "{records:?}"
    );
}
