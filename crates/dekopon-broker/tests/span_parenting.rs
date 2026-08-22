//! Which spans a suspended authorization leaves entered on its worker thread.
//!
//! `broker.authorize` covers work that awaits: the replay ledger, and on every denial a durable
//! audit append that flushes and fsyncs. A span guard held across those awaits stays entered in the
//! thread's context while the task is suspended, so whatever the runtime polls next on that thread
//! — another connection, another session — is recorded as a child of this request's authorization.
//! With OTLP export on, that is cross-request misattribution in production traces rather than a
//! test-only nicety.
//!
//! This lives in its own test binary because `tracing` resolves per-callsite interest against the
//! global dispatcher, and because the interleaving it depends on needs a single-threaded runtime.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use dekopon_broker::{
    AuditError, AuditEvent, AuditLog, AuditRecord, AuthenticatedContext, Broker, BrokerLimits,
    ConstraintCatalog, ConstraintSet, CredentialStore, IdentityDirectory, InMemoryAuditLog,
    InvocationRequest, PolicyEngine, PolicyWorld,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{EffectKind, ExecutionConstraints, Idempotency, InvocationOutcome};
use dekopon_core::{Actor, CapabilityId, PrincipalId, ProviderId, RiskLevel};
use tokio::sync::{Notify, mpsc};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

const POLICIES: &str = r#"
@id("allow-reverse")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"echo.reverse",
       resource == Dekopon::Provider::"echo");
"#;

/// Every captured event, with the span the subscriber would attribute it to.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<Record>>>);

#[derive(Clone, Debug)]
enum Record {
    Event {
        fields: String,
        parent: Option<String>,
    },
    Span {
        name: &'static str,
        fields: String,
    },
}

impl Captured {
    fn events(&self) -> Vec<(String, Option<String>)> {
        self.0
            .lock()
            .expect("capture sink")
            .iter()
            .filter_map(|record| match record {
                Record::Event { fields, parent } => Some((fields.clone(), parent.clone())),
                Record::Span { .. } => None,
            })
            .collect()
    }

    fn spans(&self) -> Vec<(&'static str, String)> {
        self.0
            .lock()
            .expect("capture sink")
            .iter()
            .filter_map(|record| match record {
                Record::Span { name, fields } => Some((*name, fields.clone())),
                Record::Event { .. } => None,
            })
            .collect()
    }
}

impl<S> tracing_subscriber::Layer<S> for Captured
where
    S: tracing::Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
{
    /// This test compiles a real component, and Wasmtime's own trace instrumentation would
    /// otherwise be captured event by event.
    fn register_callsite(
        &self,
        metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        if metadata.target().starts_with("dekopon") {
            tracing::subscriber::Interest::always()
        } else {
            tracing::subscriber::Interest::never()
        }
    }

    /// `Layer::enabled` is what actually turns a callsite off when the inner subscriber is a
    /// registry, so the filter has to live here rather than only in `register_callsite`.
    fn enabled(
        &self,
        metadata: &tracing::Metadata<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        metadata.target().starts_with("dekopon")
    }

    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = String::new();
        attributes.record(&mut Visitor(&mut fields));
        self.0.lock().expect("capture sink").push(Record::Span {
            name: attributes.metadata().name(),
            fields,
        });
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
        let mut fields = String::new();
        values.record(&mut Visitor(&mut fields));
        self.0.lock().expect("capture sink").push(Record::Span {
            name: span.metadata().name(),
            fields,
        });
    }

    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = String::new();
        event.record(&mut Visitor(&mut fields));
        self.0.lock().expect("capture sink").push(Record::Event {
            fields,
            parent: context
                .event_span(event)
                .map(|span| span.metadata().name().to_owned()),
        });
    }
}

struct Visitor<'a>(&'a mut String);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

/// An audit log that parks inside `append` until the test lets it finish.
///
/// Real durable appends write, flush, and fsync through `tokio::fs`, so the task always yields
/// there. This reproduces that yield deterministically on a single-threaded runtime.
struct GatedAudit {
    inner: InMemoryAuditLog,
    entered: mpsc::UnboundedSender<()>,
    release: Arc<Notify>,
}

impl AuditLog for GatedAudit {
    async fn append(&self, event: AuditEvent) -> Result<AuditRecord, AuditError> {
        let _ = self.entered.send(());
        self.release.notified().await;
        self.inner.append(event).await
    }
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(format!("examples/providers/{name}"))
}

fn principal(name: &str) -> PrincipalId {
    name.parse().expect("valid principal fixture")
}

fn constraint_set() -> (CapabilityId, ConstraintSet) {
    (
        "echo.reverse".parse().expect("valid capability fixture"),
        ConstraintSet {
            provider: "echo"
                .parse::<ProviderId>()
                .expect("valid provider fixture"),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Low,
            idempotency: Idempotency::Idempotent,
            credential: None,
            credential_by_agent: BTreeMap::new(),
            constraints: ExecutionConstraints::default(),
        },
    )
}

async fn broker(audit: Arc<GatedAudit>) -> Broker<GatedAudit> {
    let registry =
        BrokerProviderRegistry::load([fixture("echo-provider.wasm")], BrokerHostLimits::default())
            .await
            .expect("echo provider fixture loads");
    let world = PolicyWorld::new(
        [principal("caller")],
        [(
            "echo.reverse".parse::<CapabilityId>().expect("capability"),
            "echo".parse::<ProviderId>().expect("provider"),
        )],
    )
    .expect("the world builds");
    Broker::new(
        registry,
        principal("broker-test"),
        "span-parenting".to_owned(),
        PolicyEngine::new(POLICIES, &world).expect("the policy set validates"),
        ConstraintCatalog::new([constraint_set()]).expect("one capability builds a catalog"),
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        audit,
        BrokerLimits::default(),
    )
    .expect("the broker starts")
}

fn caller() -> AuthenticatedContext {
    AuthenticatedContext::new(
        principal("caller"),
        Actor::Service {
            principal: principal("caller"),
        },
    )
    .expect("caller context binds")
}

/// A denial suspended mid-audit must not adopt whatever the runtime polls next.
#[tokio::test]
async fn a_suspended_authorization_does_not_parent_another_task_s_events() {
    let captured = Captured::default();
    tracing_subscriber::registry().with(captured.clone()).init();

    let (entered, mut entered_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());
    let broker = Arc::new(
        broker(Arc::new(GatedAudit {
            inner: InMemoryAuditLog::new(8).expect("valid audit bound"),
            entered,
            release: Arc::clone(&release),
        }))
        .await,
    );

    let denial = tokio::spawn({
        let broker = Arc::clone(&broker);
        async move {
            broker
                .invoke(
                    &caller(),
                    InvocationRequest {
                        id: "invoke-suspended"
                            .parse()
                            .expect("valid invocation fixture"),
                        capability: "echo.echo".parse().expect("valid capability fixture"),
                        trace: "trace-suspended".parse().expect("valid trace fixture"),
                        trace_parent: None,
                        input: serde_json::json!({"message": "denied"}),
                    },
                )
                .await
                .expect("an unconstrained capability is still an accounted decision")
        }
    });

    // The authorizing task is now parked inside the audit append, exactly where a real fsync parks
    // it. Anything else this thread polls belongs to itself, not to that request.
    entered_rx
        .recv()
        .await
        .expect("the denial reached its audit");
    tracing::info!(target: "dekopon_span_probe", event = "unrelated_task");
    release.notify_one();

    let result = denial.await.expect("the denial task completes");
    assert_eq!(result.outcome, InvocationOutcome::Denied);
    assert_eq!(result.error.as_deref(), Some("unconstrained-capability"));

    let unrelated = captured
        .events()
        .into_iter()
        .find(|(fields, _)| fields.contains("unrelated_task"))
        .expect("the probe event was captured");
    assert_eq!(
        unrelated.1, None,
        "an event from another task was parented under {:?}",
        unrelated.1
    );

    // The span's own fields still arrive: instrumenting the section must not cost the recorded
    // outcome an entered guard used to sit next to.
    let authorize = captured
        .spans()
        .into_iter()
        .filter(|(name, _)| *name == "broker.authorize")
        .map(|(_, fields)| fields)
        .collect::<Vec<_>>()
        .join(" ");
    assert!(authorize.contains("invoke-suspended"), "{authorize}");
    assert!(authorize.contains("echo.echo"), "{authorize}");
    assert!(
        authorize.contains("outcome=\"unconstrained-capability\""),
        "{authorize}"
    );
}
