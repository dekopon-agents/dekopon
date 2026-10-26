#![allow(clippy::unwrap_used, clippy::disallowed_methods)]

use dekopon_broker::{
    Attestation, AttestorGrant, AuditEvent, AuthenticatedContext, Broker, BrokerLimits,
    CapabilityRoute, ChatScopeClaim, ChatTransportKind, ConstraintCatalog, ConstraintSet,
    Conversation, ConversationKind, CredentialStore, DeliveryIdentity, IdentityDirectory,
    InMemoryAuditLog, InvocationRequest, PolicyEngine, PolicyWorld, Trigger,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{EffectKind, ExecutionConstraints};
use dekopon_core::{Actor, PrincipalId, RiskLevel};
use dekopon_test_support::{CaptureLayer, Record, provider_fixture};
use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

fn principal(name: &str) -> PrincipalId {
    name.parse().expect("principal")
}

fn only(captured: &CaptureLayer, id: &str) -> Record {
    let matching = captured.events_where(
        "dekopon_broker::audit",
        &[("audit.event", "broker.decision"), ("invocation.id", id)],
    );
    let [record] = <[Record; 1]>::try_from(matching).expect("one decision");
    record
}

async fn broker_fixture() -> (Broker<InMemoryAuditLog>, Arc<InMemoryAuditLog>) {
    let mappings = [
        ("slack.team.user", "asker"),
        ("slack.team.other", "participant"),
        ("slack.team.channel", "space"),
        ("github.owner.repo", "repo"),
    ]
    .map(|(subject, id)| (subject.parse().expect("subject"), principal(id)));
    let world = PolicyWorld::new(
        [
            principal("asker"),
            principal("participant"),
            principal("space"),
            principal("repo"),
        ],
        [(
            "cli-probe.reverse".parse().expect("capability"),
            "cli-probe".parse().expect("provider"),
        )],
    )
    .expect("world")
    .with_subjects(mappings.clone());
    let policies = r#"
        @id("prompt") permit(principal, action == Dekopon::Action::"agent.prompt", resource);
        @id("asker-capability") permit(principal == Dekopon::Principal::"asker", action == Dekopon::Action::"cli-probe.reverse", resource);
        @id("space-forbid") forbid(principal == Dekopon::Principal::"space", action == Dekopon::Action::"cli-probe.reverse", resource);
    "#;
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("provider");
    let audit = Arc::new(InMemoryAuditLog::new(32).expect("audit"));
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        PolicyEngine::new(policies, &world).expect("policy"),
        ConstraintCatalog::new([(
            "cli-probe.reverse".parse().expect("capability"),
            ConstraintSet {
                default_secret_use: None,
                route: CapabilityRoute::Generic,
                provider: "cli-probe".parse().expect("provider"),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                credential: None,
                constraints: ExecutionConstraints::default(),
            },
        )])
        .expect("catalog"),
        CredentialStore::empty(),
        IdentityDirectory::new(mappings).expect("directory"),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("broker");
    (broker, audit)
}

fn slack_scope() -> ChatScopeClaim {
    ChatScopeClaim {
        transport: "chat".parse().expect("transport"),
        kind: ChatTransportKind::Slack,
        conversation: Conversation {
            kind: ConversationKind::Thread,
            container: Some("team".into()),
            id: "channel".into(),
            thread: Some("1712345678.000100".into()),
        },
        trigger: Trigger::Message,
        delivery: Some(DeliveryIdentity::Slack {
            channel: "channel".into(),
            timestamp: "1712345678.000101".into(),
        }),
    }
}

async fn assert_refused(
    broker: &Broker<InMemoryAuditLog>,
    audit: &InMemoryAuditLog,
    id: &str,
    claim: Attestation,
    reason: &str,
) {
    let gateway = AuthenticatedContext::new(
        principal("gateway"),
        Actor::Service {
            principal: principal("gateway"),
        },
    )
    .expect("gateway");
    let request = InvocationRequest {
        id: id.parse().expect("invocation"),
        capability: "cli-probe.reverse".parse().expect("capability"),
        trace_parent: "00-0000000000000000000000000000f1c7-00000000000000f1-00"
            .parse()
            .expect("trace"),
        input: serde_json::json!({}),
        secret_use: None,
    };
    let chat = claim.scope.is_some();
    let result = broker
        .invoke(
            &gateway,
            Some(&AttestorGrant { namespaces: None }),
            Some(&claim.bound_to(request.id.clone())),
            request,
            Default::default(),
        )
        .await
        .expect("accounted refusal");
    assert_eq!(
        result.result.error.as_deref(),
        Some(if chat && reason == "attestation-denied" {
            "chat-attestation-denied"
        } else {
            reason
        })
    );
    assert!(audit.records().iter().any(|event| matches!(event, AuditEvent::Decision { invocation, reason: Some(recorded), .. } if invocation.as_str() == id && recorded == reason)));
}

#[tokio::test(flavor = "multi_thread")]
async fn decisions_record_both_principals_and_attested_coordinates() {
    let captured = CaptureLayer::workspace();
    tracing_subscriber::registry().with(captured.clone()).init();
    let (broker, audit) = broker_fixture().await;
    let github = ChatScopeClaim {
        transport: "github".parse().expect("transport"),
        kind: ChatTransportKind::Github,
        conversation: Conversation {
            kind: ConversationKind::Thread,
            container: Some("owner".into()),
            id: "owner.repo".into(),
            thread: Some("7".into()),
        },
        trigger: Trigger::Message,
        delivery: Some(DeliveryIdentity::Github {
            delivery: "delivery-1".into(),
        }),
    };

    for (id, form, scope) in [
        ("channel-denial", "slack.team.user", slack_scope()),
        ("repository-denial", "github.owner.repo", github),
    ] {
        let claim = Attestation::for_chat(
            form.parse().expect("subject"),
            "test-agent".parse().expect("agent"),
            scope,
        );
        assert_refused(&broker, &audit, id, claim, "policy-denied").await;
    }
    let record = only(&captured, "channel-denial");
    for (field, value) in [
        ("decision.allowed", "false"),
        ("principal", "asker"),
        ("principal.kind", "person"),
        ("space.id", "space"),
        ("asker.allowed", "true"),
        ("space.allowed", "false"),
        ("policy.ids", "asker-capability"),
        ("space.policy.ids", "space-forbid"),
        ("decision.denied_by", "space"),
        ("turn.started_by", "person"),
        ("transport.id", "chat"),
        ("transport.kind", "slack"),
        ("conversation.kind", "thread"),
        ("conversation.container", "team"),
        ("conversation.id", "channel"),
        ("conversation.thread", "1712345678.000100"),
        ("delivery.kind", "slack"),
        ("delivery.id", "1712345678.000101"),
    ] {
        assert_eq!(record.field(field), Some(value), "{field}: {record:?}");
    }
    let repository = only(&captured, "repository-denial");
    assert_eq!(repository.field("principal.kind"), Some("repository"));
    assert_eq!(repository.field("turn.started_by"), Some("message"));
    for field in [
        "space.id",
        "space.allowed",
        "space.policy.ids",
        "decision.denied_by",
        "asker.allowed",
    ] {
        assert_eq!(repository.field(field), None);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delivery_for_another_transport_is_refused() {
    let (broker, audit) = broker_fixture().await;
    let mut scope = slack_scope();
    scope.delivery = Some(DeliveryIdentity::Github {
        delivery: "delivery-1".into(),
    });
    let claim = Attestation::for_chat(
        "slack.team.user".parse().expect("subject"),
        "test-agent".parse().expect("agent"),
        scope,
    );
    assert_refused(
        &broker,
        &audit,
        "invalid-delivery",
        claim,
        "attestation-denied",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_space_subject_cannot_be_an_attested_sender() {
    let (broker, audit) = broker_fixture().await;
    for (id, form, reason) in [
        ("attested-space", "slack.team.channel", "attestation-denied"),
        ("unmapped-space", "slack.team.cother", "unmapped-subject"),
    ] {
        let claim = Attestation::for_subject(
            form.parse().expect("subject"),
            "test-agent".parse().expect("agent"),
        );
        assert_refused(&broker, &audit, id, claim, reason).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn participant_capability_lookups_do_not_record_effect_decisions() {
    use tracing::instrument::WithSubscriber as _;

    let (broker, audit) = broker_fixture().await;
    let captured = CaptureLayer::workspace();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(captured.clone()));
    let gateway = AuthenticatedContext::new(
        principal("gateway"),
        Actor::Service {
            principal: principal("gateway"),
        },
    )
    .expect("gateway");
    tracing::dispatcher::with_default(&dispatch, || {
        let turn = tracing::info_span!("gateway.session", turn.owner = "asker");
        let _turn = turn.enter();
        for subject in ["slack.team.user", "slack.team.other"] {
            let claim = Attestation::for_chat(
                subject.parse().unwrap(),
                "test-agent".parse().unwrap(),
                slack_scope(),
            );
            broker
                .session_surface(
                    &gateway,
                    Some(&AttestorGrant { namespaces: None }),
                    Some(&claim),
                )
                .unwrap_or_else(|refusal| panic!("lookup refused: {}", refusal.reason));
        }
    });
    assert!(audit.records().is_empty());
    assert!(
        captured
            .events_where(
                "dekopon_broker::audit",
                &[("audit.event", "broker.decision")]
            )
            .is_empty()
    );
    let claim = Attestation::for_chat(
        "slack.team.user".parse().unwrap(),
        "test-agent".parse().unwrap(),
        slack_scope(),
    );
    assert_refused(
        &broker,
        &audit,
        "effect-after-lookups",
        claim,
        "policy-denied",
    )
    .with_subscriber(dispatch)
    .await;
    assert_eq!(audit.records().len(), 1);
    assert_eq!(
        only(&captured, "effect-after-lookups").field("principal"),
        Some("asker")
    );
}
