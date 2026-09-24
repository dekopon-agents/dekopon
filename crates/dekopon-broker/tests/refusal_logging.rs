//! Refused capability and command lookups return the same opaque error regardless of cause, so an
//! unauthorized gateway can't learn whether a subject is mapped.

#![allow(clippy::unwrap_used)]

use std::{collections::BTreeMap, sync::Arc};

use dekopon_broker::{
    Attestation, AttestorGrant, AuditEvent, AuthenticatedContext, Broker, BrokerLimits,
    CapabilityRoute, ChatScopeClaim, ChatTransportKind, ConstraintCatalog, ConstraintSet,
    Conversation, ConversationKind, CredentialStore, IdentityDirectory, InMemoryAuditLog,
    InvocationRequest, PolicyEngine, PolicyWorld,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{EffectKind, ExecutionConstraints, InvocationOutcome};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, PrincipalId, ProviderId, RiskLevel, TransportId,
};
use dekopon_test_support::{CaptureLayer, provider_fixture};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

const TRACE_PARENT: &str = "00-0000000000000000000000000000f1c7-00000000000000f1-00";

const MAPPED_SUBJECT: &str = "slack.t0123abc.u9xyz";
const UNMAPPED_SUBJECT: &str = "slack.t0123abc.unobody";

const POLICIES: &str = r#"
@id("attested-reverse")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"cli-probe.reverse",
       resource == Dekopon::Provider::"cli-probe")
when { context has via && context.via == "gateway"
    && context has agent && context.agent == "some-agent" };

@id("prompt-gate")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"some-agent")
when { context has via && context.via == "gateway" };

@id("broken-gate")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"broken-agent")
when { 9223372036854775807 + 1 == 0 };

@id("forbidden-gate")
forbid(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"forbidden-agent");
"#;

fn principal(name: &str) -> PrincipalId {
    name.parse().expect("valid principal fixture")
}

fn agent(name: &str) -> AgentId {
    name.parse().expect("valid agent fixture")
}

fn subject(canonical: &str) -> ExternalSubject {
    canonical.parse().expect("canonical subject fixture")
}

fn constraint_set() -> (CapabilityId, ConstraintSet) {
    (
        "cli-probe.reverse"
            .parse()
            .expect("valid capability fixture"),
        ConstraintSet {
            route: CapabilityRoute::Generic,
            provider: "cli-probe"
                .parse::<ProviderId>()
                .expect("valid provider fixture"),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Low,
            credential: None,
            credential_by_agent: BTreeMap::new(),
            constraints: ExecutionConstraints::default(),
        },
    )
}

async fn broker() -> (Broker<InMemoryAuditLog>, Arc<InMemoryAuditLog>) {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("cli-probe provider fixture loads");
    let world = PolicyWorld::new(
        [principal("cpetersen"), principal("gateway")],
        [(
            "cli-probe.reverse"
                .parse::<CapabilityId>()
                .expect("capability"),
            "cli-probe".parse::<ProviderId>().expect("provider"),
        )],
    )
    .expect("the refusal world builds");
    let audit = Arc::new(InMemoryAuditLog::new(64).expect("valid audit bound"));
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        "refusal-logging".to_owned(),
        PolicyEngine::new(POLICIES, &world).expect("the refusal policy set validates"),
        ConstraintCatalog::new([constraint_set()]).expect("one capability builds a catalog"),
        CredentialStore::empty(),
        IdentityDirectory::new([(subject(MAPPED_SUBJECT), principal("cpetersen"))])
            .expect("one mapping builds a directory"),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .expect("the broker starts");
    (broker, audit)
}

fn gateway() -> AuthenticatedContext {
    AuthenticatedContext::new(
        principal("gateway"),
        Actor::Service {
            principal: principal("gateway"),
        },
    )
    .expect("gateway context binds")
}

fn grant() -> AttestorGrant {
    AttestorGrant {
        namespaces: vec!["slack.t0123abc".to_owned()],
        chat_scopes: Vec::new(),
    }
}

fn proposal(id: &str) -> InvocationRequest {
    InvocationRequest {
        id: id.parse().expect("valid invocation fixture"),
        capability: "cli-probe.reverse"
            .parse()
            .expect("valid capability fixture"),
        trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
        input: serde_json::json!({"text": "refused"}),
        secret_use: None,
    }
}

fn chat_claim(canonical: &str, agent_id: &str) -> Attestation {
    Attestation::for_chat(
        subject(canonical),
        agent(agent_id),
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

#[tokio::test(flavor = "multi_thread")]
async fn every_inspection_refusal_names_its_class_and_its_subject() {
    let captured = CaptureLayer::workspace();
    tracing_subscriber::registry().with(captured.clone()).init();
    let (broker, audit) = broker().await;

    assert!(
        broker
            .capability_surface(
                &gateway(),
                None,
                Some(&Attestation::for_subject(
                    subject(MAPPED_SUBJECT),
                    agent("some-agent")
                )),
            )
            .is_none()
    );
    let ungranted = captured.take_events();
    assert!(
        ungranted.contains("broker_capabilities_refused"),
        "{ungranted}"
    );
    assert!(ungranted.contains("attestation-denied"), "{ungranted}");
    assert!(ungranted.contains(MAPPED_SUBJECT), "{ungranted}");
    assert!(ungranted.contains("gateway"), "{ungranted}");

    let narrow = AttestorGrant {
        namespaces: vec!["slack.tother".to_owned()],
        chat_scopes: Vec::new(),
    };
    assert!(
        broker
            .capability_surface(
                &gateway(),
                Some(&narrow),
                Some(&Attestation::for_subject(
                    subject(MAPPED_SUBJECT),
                    agent("some-agent")
                )),
            )
            .is_none()
    );
    assert!(captured.take_events().contains("attestation-denied"));

    assert!(
        broker
            .capability_surface(
                &gateway(),
                Some(&grant()),
                Some(&Attestation::for_subject(
                    subject(UNMAPPED_SUBJECT),
                    agent("some-agent")
                )),
            )
            .is_none()
    );
    let unmapped = captured.take_events();
    assert!(unmapped.contains("unmapped-subject"), "{unmapped}");
    assert!(unmapped.contains(UNMAPPED_SUBJECT), "{unmapped}");

    assert!(
        broker
            .capability_surface(
                &gateway(),
                Some(&grant()),
                Some(&Attestation::for_subject(
                    subject(MAPPED_SUBJECT),
                    agent("other-agent")
                )),
            )
            .is_none()
    );
    let denied = captured.take_events();
    assert!(denied.contains("agent-denied"), "{denied}");
    assert!(denied.contains("other-agent"), "{denied}");

    assert!(
        broker
            .capability_surface(
                &gateway(),
                Some(&grant()),
                Some(&Attestation::for_subject(
                    subject(MAPPED_SUBJECT),
                    agent("broken-agent")
                )),
            )
            .is_none()
    );
    let erroring = captured.take_events();
    assert!(erroring.contains("policy-error"), "{erroring}");
    assert!(!erroring.contains("agent-denied"), "{erroring}");

    assert!(
        broker
            .capability_surface(
                &gateway(),
                Some(&grant()),
                Some(&Attestation::for_subject(
                    subject(MAPPED_SUBJECT),
                    agent("forbidden-agent")
                )),
            )
            .is_none()
    );
    let forbidden = captured.take_events();
    assert!(forbidden.contains("agent-denied"), "{forbidden}");
    assert!(forbidden.contains("forbidden-gate"), "{forbidden}");

    let denied = proposal("invoke-policy-error");
    let refused = broker
        .invoke(
            &gateway(),
            Some(&grant()),
            Some(
                &Attestation::for_subject(subject(MAPPED_SUBJECT), agent("broken-agent"))
                    .bound_to(denied.id.clone()),
            ),
            denied,
            Default::default(),
        )
        .await
        .expect("a refused agent is still an accounted decision");
    assert_eq!(refused.result.outcome, InvocationOutcome::Denied);
    assert_eq!(refused.result.error.as_deref(), Some("policy-error"));
    captured.clear();

    assert!(
        broker
            .capability_surface(
                &gateway(),
                Some(&grant()),
                Some(&chat_claim(UNMAPPED_SUBJECT, "some-agent"))
            )
            .is_none()
    );
    let chat = captured.take_events();
    assert!(chat.contains("broker_capabilities_refused"), "{chat}");
    assert!(chat.contains("unmapped-subject"), "{chat}");
    assert!(chat.contains(UNMAPPED_SUBJECT), "{chat}");

    assert!(
        broker
            .run_command(
                &gateway(),
                Some(&grant()),
                Some(&chat_claim(UNMAPPED_SUBJECT, "some-agent")),
                "probe",
                &[],
                None,
            )
            .await
            .is_err()
    );
    let command = captured.take_events();
    assert!(command.contains("broker_capabilities_refused"), "{command}");
    assert!(command.contains("unmapped-subject"), "{command}");
    assert!(command.contains(UNMAPPED_SUBJECT), "{command}");

    for (index, (attestor, canonical, agent_id, reason, policies)) in [
        (
            None,
            MAPPED_SUBJECT,
            "some-agent",
            "attestation-denied",
            &[][..],
        ),
        (
            Some(grant()),
            UNMAPPED_SUBJECT,
            "some-agent",
            "unmapped-subject",
            &[][..],
        ),
        (
            Some(grant()),
            MAPPED_SUBJECT,
            "other-agent",
            "agent-denied",
            &[][..],
        ),
        (
            Some(grant()),
            MAPPED_SUBJECT,
            "forbidden-agent",
            "agent-denied",
            &["forbidden-gate"][..],
        ),
        (
            Some(grant()),
            MAPPED_SUBJECT,
            "broken-agent",
            "policy-error",
            &[][..],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let request = proposal(&format!("invoke-chat-{index}"));
        let identifier = request.id.clone();
        let refused = broker
            .invoke(
                &gateway(),
                attestor.as_ref(),
                Some(&chat_claim(canonical, agent_id).bound_to(identifier.clone())),
                request,
                Default::default(),
            )
            .await
            .expect("a refused chat proposal is still an accounted decision");
        assert_eq!(refused.result.outcome, InvocationOutcome::Denied);
        assert_eq!(
            refused.result.error.as_deref(),
            Some("chat-attestation-denied"),
            "the wire answer is the same literal for every class ({agent_id})"
        );

        let records = audit.records();
        let decision = records
            .iter()
            .find_map(|record| match record {
                AuditEvent::Decision {
                    invocation,
                    reason,
                    policy_ids,
                    ..
                } if *invocation == identifier => Some((reason.clone(), policy_ids.clone())),
                _ => None,
            })
            .expect("the refusal is durably recorded");
        assert_eq!(decision.0.as_deref(), Some(reason), "{agent_id}");
        assert_eq!(decision.1, policies, "{agent_id}");
    }
    captured.clear();

    assert!(
        broker
            .capability_surface(
                &gateway(),
                Some(&grant()),
                Some(&Attestation::for_subject(
                    subject(MAPPED_SUBJECT),
                    agent("some-agent")
                )),
            )
            .is_some()
    );
    let allowed = captured.take_events();
    assert!(
        !allowed.contains("broker_capabilities_refused"),
        "{allowed}"
    );
}
