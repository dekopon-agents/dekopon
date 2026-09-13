//! The W3C trace identifier every audit record and every command run has to carry, on every
//! entrance to the broker.
//!
//! Nothing puts that identifier into an audit record's own fields, and nothing should:
//! `dekopon-broker` links no telemetry SDK. It carries the id because it is emitted *inside* the
//! span that made the decision, which descends from the `broker.invocation` span that adopted the
//! client's `traceparent` — so the console JSON formatter and the OTLP log bridge each stamp the
//! live ids on it. These tests read the same native context those two read, from the same place.
//!
//! Both invocation entrances are covered because they build their span from different requests:
//! `Invoke` from a proposal, the storage-routed `RecordDeliveredTurn` from a delivered turn.
//! Storage withholds nothing from that span; the attested subject and agent ride both. A command
//! run makes no audit record at all, so `broker.command_run` is read as it is entered instead: the
//! word an agent ran belongs to the trace of the invocation its proposal becomes.
//!
//! Its own test binary, because `tracing` caches per-callsite interest against the global
//! dispatcher for the whole process. Both tests share the one subscriber a process can install.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use dekopon_broker::{
    Broker, BrokerLimits, CapabilityRoute, ConstraintCatalog, ConstraintSet, CredentialStore,
    IdentityDirectory, InvocationRequest, PolicyEngine, PolicyWorld, TraceOnlyAuditLog,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_broker_protocol::{
    Attestation, BrokerClient, ChatScopeClaim, ChatTransportKind, CommandRunOutcome, Conversation,
    ConversationKind, DeliveredTurnRequest, DeliveryIdentity, FrameLimits, TraceParent,
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
use tokio::{net::UnixListener, sync::oneshot, task::JoinHandle};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

/// The trace the client claims to be part of; every audit record and command run it causes must
/// name it.
const CLIENT_TRACE_ID: [u8; 16] = [
    0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e, 0x47, 0x36,
];
const CLIENT_SPAN_ID: [u8; 8] = [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7];
/// [`CLIENT_TRACE_ID`] as the native context renders it.
const CLIENT_TRACE_HEX: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
/// [`CLIENT_SPAN_ID`] as the SDK renders a span id: the parent a span joined to the client names.
const CLIENT_SPAN_HEX: &str = "00f067aa0ba902b7";

/// The subject and agent the chat claim speaks for.
const SUBJECT: &str = "slack.t0123abc.u9xyz";
const AGENT: &str = "chat-agent";

const POLICY: &str = r#"
@id("caller-upper")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"cli-probe.upper",
       resource == Dekopon::Provider::"cli-probe")
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

/// One span entry: the trace the native context held once the OTel layer beneath had entered it.
#[derive(Clone, Debug)]
struct Entered {
    name: &'static str,
    trace_id: String,
}

/// One span as the SDK ended it: the parent span id an exporter would send for it.
#[derive(Clone, Debug)]
struct Ended {
    name: String,
    parent_span_id: String,
}

/// What the one global subscriber saw of this workspace: audit records, span entries, every field
/// rendered onto a span, and every span the SDK ended.
#[derive(Clone, Debug, Default)]
struct Probe {
    audits: Arc<Mutex<Vec<Captured>>>,
    entered: Arc<Mutex<Vec<Entered>>>,
    fields: Arc<Mutex<Vec<(&'static str, String)>>>,
    ended: Arc<Mutex<Vec<Ended>>>,
}

impl Probe {
    fn audits(&self) -> Vec<Captured> {
        self.audits.lock().expect("probe sink").clone()
    }

    /// The parent span id each span named `name` ended with, as the SDK hands it to an exporter.
    fn parents_ended(&self, name: &str) -> Vec<String> {
        self.ended
            .lock()
            .expect("probe sink")
            .iter()
            .filter(|ended| ended.name == name)
            .map(|ended| ended.parent_span_id.clone())
            .collect()
    }

    /// The trace the native context held each time a span named `name` was entered.
    fn traces_entered(&self, name: &str) -> Vec<String> {
        self.entered
            .lock()
            .expect("probe sink")
            .iter()
            .filter(|entered| entered.name == name)
            .map(|entered| entered.trace_id.clone())
            .collect()
    }

    /// Every field set rendered onto spans named `name`, at creation and at each later recording.
    fn span_fields(&self, name: &str) -> Vec<String> {
        self.fields
            .lock()
            .expect("probe sink")
            .iter()
            .filter(|(span, _)| *span == name)
            .map(|(_, fields)| fields.clone())
            .collect()
    }
}

impl<S> tracing_subscriber::Layer<S> for Probe
where
    S: tracing::Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let metadata = attributes.metadata();
        if !metadata.target().starts_with("dekopon") {
            return;
        }
        let mut fields = String::new();
        attributes.record(&mut Visitor(&mut fields));
        self.fields
            .lock()
            .expect("probe sink")
            .push((metadata.name(), fields));
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let Some(span) = context.span(id) else {
            return;
        };
        if !span.metadata().target().starts_with("dekopon") {
            return;
        }
        let mut fields = String::new();
        values.record(&mut Visitor(&mut fields));
        self.fields
            .lock()
            .expect("probe sink")
            .push((span.metadata().name(), fields));
    }

    fn on_enter(&self, id: &tracing::span::Id, context: tracing_subscriber::layer::Context<'_, S>) {
        let Some(span) = context.span(id) else {
            return;
        };
        if !span.metadata().target().starts_with("dekopon") {
            return;
        }
        // The OTel layer sits beneath this one, so it has already made this span's context current.
        let trace_id = opentelemetry::Context::current()
            .span()
            .span_context()
            .trace_id()
            .to_string();
        self.entered.lock().expect("probe sink").push(Entered {
            name: span.metadata().name(),
            trace_id,
        });
    }

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
        self.audits.lock().expect("probe sink").push(Captured {
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

/// The probe is also the SDK's span processor, so a span's parent is read from the `SpanData` an
/// exporter would receive rather than inferred from the tracing side.
impl opentelemetry_sdk::trace::SpanProcessor for Probe {
    fn on_start(&self, _span: &mut opentelemetry_sdk::trace::Span, _cx: &opentelemetry::Context) {}

    fn on_end(&self, span: opentelemetry_sdk::trace::SpanData) {
        self.ended.lock().expect("probe sink").push(Ended {
            name: span.name.into_owned(),
            parent_span_id: span.parent_span_id.to_string(),
        });
    }

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }
}

/// Installs the one global subscriber this binary can have — the OTel layer with the probe above
/// it — on first use, and hands every test the same probe.
fn install() -> Probe {
    static INSTALLED: OnceLock<(Probe, SdkTracerProvider)> = OnceLock::new();
    INSTALLED
        .get_or_init(|| {
            let probe = Probe::default();
            // A provider with no exporter still mints span contexts, which is all the formatter and
            // the log bridge read; the probe ends each span itself, so nothing needs a receiver.
            let provider = SdkTracerProvider::builder()
                .with_span_processor(probe.clone())
                .build();
            tracing_subscriber::registry()
                .with(tracing_opentelemetry::layer().with_tracer(
                    opentelemetry::trace::TracerProvider::tracer(&provider, "audit-trace-test"),
                ))
                .with(probe.clone())
                .init();
            (probe, provider)
        })
        .0
        .clone()
}

fn principal(name: &str) -> PrincipalId {
    name.parse().expect("valid principal fixture")
}

fn trace_parent() -> TraceParent {
    TraceParent::new(CLIENT_TRACE_ID, CLIENT_SPAN_ID, 1).expect("valid W3C parent fixture")
}

fn chat_claim() -> Attestation {
    Attestation::for_chat(
        SUBJECT
            .parse::<ExternalSubject>()
            .expect("canonical subject fixture"),
        AGENT.parse::<AgentId>().expect("valid agent fixture"),
        ChatScopeClaim {
            transport: "scientist-slack"
                .parse::<TransportId>()
                .expect("valid transport fixture"),
            kind: ChatTransportKind::Slack,
            conversation: Conversation {
                kind: ConversationKind::Thread,
                container: Some("t0123abc".to_owned()),
                id: "c0123abc".to_owned(),
                thread: Some("1712345678.000100".to_owned()),
            },
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
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("load cli-probe fixture");
    let world = PolicyWorld::new(
        [principal("caller")],
        [(
            "cli-probe.upper"
                .parse::<CapabilityId>()
                .expect("capability"),
            "cli-probe".parse::<ProviderId>().expect("provider"),
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
                "cli-probe.upper"
                    .parse::<CapabilityId>()
                    .expect("capability"),
                ConstraintSet {
                    route: CapabilityRoute::Generic,
                    provider: "cli-probe".parse::<ProviderId>().expect("provider"),
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

/// A broker serving the current UID on a private socket, and a client connected to it.
struct Served<E> {
    client: BrokerClient,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<Result<(), E>>,
    _directory: tempfile::TempDir,
}

impl<E: std::fmt::Debug> Served<E> {
    async fn stop(self) {
        self.shutdown.send(()).expect("signal clean shutdown");
        self.task
            .await
            .expect("server task exits")
            .expect("server shuts down");
    }
}

async fn serve() -> Served<impl std::fmt::Debug> {
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
    let (shutdown, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(listener, shutdown_on(stopped)));
    let client = BrokerClient::new(&socket_path, uid, FrameLimits::default()).expect("client");
    Served {
        client,
        shutdown,
        task,
        _directory: directory,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_audit_record_carries_the_client_s_w3c_trace_id() {
    let probe = install();
    let served = serve().await;

    let invoked = served
        .client
        .invoke(
            None,
            InvocationRequest {
                id: "invoke-traced".parse::<InvocationId>().expect("invocation"),
                capability: "cli-probe.upper"
                    .parse::<CapabilityId>()
                    .expect("capability"),
                trace_parent: trace_parent(),
                secret_use: None,
                input: serde_json::json!({"text": "hello through broker"}),
            },
        )
        .await
        .expect("the authorized invocation completes");
    assert_eq!(invoked.outcome, InvocationOutcome::Succeeded);

    // The storage-routed entrance. No chat memory is configured, so this is an audited refusal —
    // which is the point: a refusal is a decision, and it has to land in the caller's trace too.
    let refused = served
        .client
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

    served.stop().await;

    let records = probe.audits();
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
                record.trace_id, CLIENT_TRACE_HEX,
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

    // The storage-routed span withholds nothing the attested direct one carries: the claim's
    // subject and agent ride it too.
    let turn = probe
        .span_fields("broker.invocation")
        .into_iter()
        .find(|fields| fields.contains("invocation=record-traced"))
        .expect("the turn opened its invocation span");
    assert!(turn.contains(&format!("subject={SUBJECT}")), "{turn}");
    assert!(turn.contains(&format!("agent={AGENT}")), "{turn}");
}

/// A command run makes no audit record, so its trace is read where the run starts:
/// `broker.command_run` adopts the client's `traceparent` exactly as `broker.invocation` does, and
/// the host's `provider.run_command` inherits the same trace beneath it.
///
/// A shared trace id is not enough: the span must be the child of the exact span the client
/// named, or the operator's trace shows the run beside the client's work rather than under it. Each
/// of the four outcomes a run ends in is recorded under its exact name.
#[tokio::test(flavor = "multi_thread")]
async fn every_command_run_carries_the_client_s_w3c_trace_id() {
    let probe = install();
    let served = serve().await;

    let proposed = served
        .client
        .run_command(
            None,
            "probe".to_owned(),
            vec!["upper".to_owned(), "--text".to_owned(), "hello".to_owned()],
            None,
            trace_parent(),
        )
        .await
        .expect("the word proposes");
    assert!(
        matches!(
            proposed,
            CommandRunOutcome::Proposed { ref capability, .. }
                if capability.as_str() == "cli-probe.upper"
        ),
        "{proposed:?}"
    );
    let help = served
        .client
        .run_command(
            None,
            "probe".to_owned(),
            vec!["--help".to_owned()],
            None,
            trace_parent(),
        )
        .await
        .expect("the help page renders");
    assert!(
        matches!(help, CommandRunOutcome::Rendered { status: 0, .. }),
        "{help:?}"
    );
    // The guest's own decline: `-` asks for piped text, and nothing was piped.
    let declined = served
        .client
        .run_command(
            None,
            "probe".to_owned(),
            vec!["upper".to_owned(), "-".to_owned()],
            None,
            trace_parent(),
        )
        .await
        .expect("the guest's decline is an answer");
    assert!(
        matches!(
            declined,
            CommandRunOutcome::Failed { ref error }
                if error.message.contains("nothing was piped in")
        ),
        "{declined:?}"
    );
    // No answer at all: no loaded provider declares the word.
    let refused = served
        .client
        .run_command(
            None,
            "nosuchword".to_owned(),
            Vec::new(),
            None,
            trace_parent(),
        )
        .await;
    assert!(refused.is_err(), "an undeclared word answers: {refused:?}");

    served.stop().await;

    for name in ["broker.command_run", "provider.run_command"] {
        let traces = probe.traces_entered(name);
        assert!(!traces.is_empty(), "{name} was never entered");
        assert!(
            traces.iter().all(|trace| trace == CLIENT_TRACE_HEX),
            "{name} left the client's trace: {traces:?}"
        );
    }
    assert_eq!(
        probe.parents_ended("broker.command_run"),
        [CLIENT_SPAN_HEX; 4],
        "every run is the child of the span the client's traceparent named"
    );
    let outcomes = probe
        .span_fields("broker.command_run")
        .into_iter()
        .filter_map(|fields| fields.strip_prefix(" outcome=").map(str::to_owned))
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes,
        [
            r#""proposed""#,
            r#""rendered""#,
            r#""failed""#,
            r#""error""#
        ],
        "each run records its outcome under its exact name, in order"
    );
}
