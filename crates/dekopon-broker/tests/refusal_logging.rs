//! What the broker says about a session it refuses before anything is ever invoked.
//!
//! An attested `capabilities` answers a refused caller with the same opaque nothing whatever went
//! wrong — that is deliberate, because a distinguishable answer would tell an unauthorized gateway
//! whether a subject is mapped. The cost was that the broker's own side of the socket recorded
//! nothing either, so bootstrapping an `identityMapping` for a new Slack sender meant reading the
//! sender's subject out of a payload-carrying gateway span. These tests hold the opaque wire answer
//! and the named broker-side event together.
//!
//! This lives in its own test binary because `tracing` resolves per-callsite interest against the
//! global dispatcher, so a sibling test hitting these callsites with no subscriber installed can
//! disable them for the whole process.

use std::{collections::BTreeMap, sync::Arc};

use dekopon_broker::{
    Attestation, AttestorGrant, AuditEvent, AuthenticatedContext, Broker, BrokerLimits,
    CapabilityRoute, ChatScopeClaim, ChatTransportKind, ConstraintCatalog, ConstraintSet,
    CredentialStore, IdentityDirectory, InMemoryAuditLog, InvocationRequest, PolicyEngine,
    PolicyWorld,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{EffectKind, ExecutionConstraints, Idempotency, InvocationOutcome};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, PrincipalId, ProviderId, RiskLevel, TransportId,
};
use dekopon_test_support::{CaptureLayer, provider_fixture};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

const MAPPED_SUBJECT: &str = "slack.t0123abc.u9xyz";
const UNMAPPED_SUBJECT: &str = "slack.t0123abc.unobody";

/// `cpetersen` may drive `some-agent` through the gateway and nothing else may drive anything.
const POLICIES: &str = r#"
@id("attested-reverse")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"echo.reverse",
       resource == Dekopon::Provider::"echo")
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
        "echo.reverse".parse().expect("valid capability fixture"),
        ConstraintSet {
            route: CapabilityRoute::Generic,
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

async fn broker() -> (Broker<InMemoryAuditLog>, Arc<InMemoryAuditLog>) {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("echo-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("echo provider fixture loads");
    let world = PolicyWorld::new(
        [principal("cpetersen"), principal("gateway")],
        [(
            "echo.reverse".parse::<CapabilityId>().expect("capability"),
            "echo".parse::<ProviderId>().expect("provider"),
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
        capability: "echo.reverse".parse().expect("valid capability fixture"),
        trace: "trace-refusal".parse().expect("valid trace fixture"),
        trace_parent: None,
        input: serde_json::json!({"message": "refused"}),
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
            channel: "c0123abc".to_owned(),
            conversation: "c0123abc:1712345678.000100".to_owned(),
        },
    )
}

/// Four refusals that answer identically on the wire must not be one refusal in the logs.
#[tokio::test(flavor = "multi_thread")]
async fn every_inspection_refusal_names_its_class_and_its_subject() {
    let captured = CaptureLayer::workspace();
    tracing_subscriber::registry().with(captured.clone()).init();
    let (broker, audit) = broker().await;

    // No attestor authority at all.
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

    // A grant that does not reach this namespace is the same wire answer, a different class.
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

    // The bootstrap case: a sender no `identityMapping` names yet. The canonical subject in this
    // event is the whole point — it is the value an operator has to copy into configuration.
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

    // Mapped, attested, and still refused: policy does not let this principal drive that agent.
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

    // A policy that cannot be evaluated denies exactly like one that does not match. Cedar's
    // strict validator cannot rule this out — the overflow above is well typed — so the refusal
    // class is the only thing that separates a broken rule from a deliberate one.
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

    // A refusal a `forbid` rule determined is the case where the identifiers are not empty, and
    // they are the only route from the class back to the rule that reached it.
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

    // The same distinction reaches the durable decision record, where a denial that was really a
    // broken policy used to be filed as an ordinary refusal.
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
        )
        .await
        .expect("a refused agent is still an accounted decision");
    assert_eq!(refused.outcome, InvocationOutcome::Denied);
    assert_eq!(refused.error.as_deref(), Some("policy-error"));
    captured.clear();

    // The chat surface the gateway actually opens takes the same path and reports the same way.
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

    // A command word a refused session may not reach answers `UnknownCommandWord` whatever went
    // wrong — naming the word would disclose the surface the refusal withheld — so this event is
    // the only place the class exists at all.
    assert!(
        broker
            .resolve_command(
                &gateway(),
                Some(&grant()),
                Some(&chat_claim(UNMAPPED_SUBJECT, "some-agent")),
                "echo",
                &[],
            )
            .await
            .is_err()
    );
    let command = captured.take_events();
    assert!(command.contains("broker_capabilities_refused"), "{command}");
    assert!(command.contains("unmapped-subject"), "{command}");
    assert!(command.contains(UNMAPPED_SUBJECT), "{command}");

    // The chat invocation path carries all four live transports. Every one of these classes used
    // to be filed as a single `chat-attestation-denied` with no policy identifiers at all, and the
    // wire answer must stay exactly that literal: the class and its policies belong to the audit
    // record, and a peer that could read them off its own denial would learn from a refusal which
    // subjects the directory maps and which agents a principal may drive.
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
            )
            .await
            .expect("a refused chat proposal is still an accounted decision");
        assert_eq!(refused.outcome, InvocationOutcome::Denied);
        assert_eq!(
            refused.error.as_deref(),
            Some("chat-attestation-denied"),
            "the wire answer is the same literal for every class ({agent_id})"
        );

        let records = audit.records().await;
        let decision = records
            .iter()
            .find_map(|record| match &record.event {
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

    // An honored session stays silent: this event marks refusals, not traffic.
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
