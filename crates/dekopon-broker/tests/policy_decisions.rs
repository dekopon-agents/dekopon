#![allow(clippy::unwrap_used)]

use std::{collections::BTreeMap, sync::Arc};

use dekopon_broker::{
    Attestation, AttestorGrant, AuthenticatedContext, Broker, BrokerLimits, CapabilityRoute,
    ChatScopeClaim, ChatTransportKind, ConstraintCatalog, ConstraintSet, Conversation,
    ConversationKind, CredentialStore, IdentityDirectory, InMemoryAuditLog, InvocationRequest,
    PolicyEngine, PolicyWorld,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{EffectKind, ExecutionConstraints, InvocationOutcome};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId, ProviderId,
    RiskLevel, TransportId,
};
use dekopon_test_support::provider_fixture;
use serde_json::json;

const TRACE_PARENT: &str = "00-0000000000000000000000000000f1c7-00000000000000f1-00";

const SLACK_SUBJECT: &str = "slack.t0123abc.u9xyz";

const POLICIES: &str = r#"
@id("unconditional-upper")
permit(principal == Dekopon::Principal::"direct-caller",
       action == Dekopon::Action::"cli-probe.upper",
       resource == Dekopon::Provider::"cli-probe");

@id("attested-reverse")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"cli-probe.reverse",
       resource == Dekopon::Provider::"cli-probe")
when { context.via == "gateway"
    && context.agent == "some-agent" };

@id("prompt-gate")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"some-agent")
when { context.via == "gateway" };
"#;

fn principal(name: &str) -> PrincipalId {
    name.parse().expect("valid principal fixture")
}

fn agent(name: &str) -> AgentId {
    name.parse().expect("valid agent fixture")
}

fn capability(name: &str) -> CapabilityId {
    name.parse().expect("valid capability fixture")
}

fn provider() -> ProviderId {
    "cli-probe".parse().expect("valid provider fixture")
}

fn subject() -> ExternalSubject {
    SLACK_SUBJECT.parse().expect("canonical subject fixture")
}

struct Row {
    label: &'static str,
    principal: &'static str,
    agent: &'static str,
    via: Option<&'static str>,
    capability: &'static str,
    allowed: bool,
}

const TABLE: &[Row] = &[
    Row {
        label: "a direct peer is denied even an unconditional grant naming it",
        principal: "direct-caller",
        agent: "provider-test",
        via: None,
        capability: "cli-probe.upper",
        allowed: false,
    },
    Row {
        label: "the mapped principal arriving directly matches nothing",
        principal: "cpetersen",
        agent: "some-agent",
        via: None,
        capability: "cli-probe.reverse",
        allowed: false,
    },
    Row {
        label: "attested caller reaches its attested grant",
        principal: "cpetersen",
        agent: "some-agent",
        via: Some("gateway"),
        capability: "cli-probe.reverse",
        allowed: true,
    },
    Row {
        label: "attested caller does not reach another principal's grant",
        principal: "cpetersen",
        agent: "some-agent",
        via: Some("gateway"),
        capability: "cli-probe.upper",
        allowed: false,
    },
    Row {
        label: "a different agent under the same attestation matches nothing",
        principal: "cpetersen",
        agent: "other-agent",
        via: Some("gateway"),
        capability: "cli-probe.reverse",
        allowed: false,
    },
];

fn constraint_set(capability_id: &str) -> (CapabilityId, ConstraintSet) {
    (
        capability(capability_id),
        ConstraintSet {
            route: CapabilityRoute::Generic,
            provider: provider(),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Low,
            credential: None,
            constraints: ExecutionConstraints::default(),
        },
    )
}

fn policy_engine() -> PolicyEngine {
    let world = PolicyWorld::new(
        [
            principal("cpetersen"),
            principal("direct-caller"),
            principal("gateway"),
        ],
        [
            (capability("cli-probe.upper"), provider()),
            (capability("cli-probe.reverse"), provider()),
        ],
    )
    .expect("the workflow world builds");
    PolicyEngine::new(POLICIES, &world).expect("the workflow policy set validates")
}

async fn broker(mapped_principal: &str) -> Broker<InMemoryAuditLog> {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("cli-probe provider fixture loads");
    Broker::new(
        registry,
        principal("broker-test"),
        "policy-decision-table".to_owned(),
        policy_engine(),
        ConstraintCatalog::new([
            constraint_set("cli-probe.upper"),
            constraint_set("cli-probe.reverse"),
        ])
        .expect("distinct capabilities build a catalog"),
        CredentialStore::empty(),
        IdentityDirectory::new([(subject(), principal(mapped_principal))])
            .expect("one mapping builds a directory"),
        Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound")),
        BrokerLimits::default(),
    )
    .expect("the broker starts")
}

fn request(index: usize, capability_id: &str) -> InvocationRequest {
    InvocationRequest {
        id: format!("invoke-table-{index}")
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        capability: capability(capability_id),
        trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
        input: json!({"text": "decision table"}),
        secret_use: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_workflow_decision_table_holds_end_to_end() {
    let mut brokers = BTreeMap::new();
    for (index, row) in TABLE.iter().enumerate() {
        if !brokers.contains_key(row.principal) {
            brokers.insert(row.principal, broker(row.principal).await);
        }
        let broker = &brokers[row.principal];
        let request = request(index, row.capability);
        let result = match row.via {
            None => {
                let context = AuthenticatedContext::new(
                    principal(row.principal),
                    Actor::Agent {
                        agent: agent(row.agent),
                    },
                )
                .expect("direct context binds");
                broker
                    .invoke(&context, None, None, request, Default::default())
                    .await
                    .expect("the proposal is accounted")
            }
            Some(via) => {
                let peer = AuthenticatedContext::new(
                    principal(via),
                    Actor::Service {
                        principal: principal(via),
                    },
                )
                .expect("gateway context binds");
                let attestation = Attestation::for_subject(subject(), agent(row.agent))
                    .bound_to(request.id.clone());
                broker
                    .invoke(
                        &peer,
                        Some(&AttestorGrant {
                            namespaces: Some(vec!["slack.t0123abc".to_owned()]),
                        }),
                        Some(&attestation),
                        request,
                        Default::default(),
                    )
                    .await
                    .expect("the attested proposal is accounted")
            }
        };
        assert_eq!(
            result.result.outcome != InvocationOutcome::Denied,
            row.allowed,
            "row {index} ({}) decided the wrong way: {result:?}",
            row.label
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_prompt_gate_is_a_separate_grant() {
    let broker = broker("cpetersen").await;
    let gateway = AuthenticatedContext::new(
        principal("gateway"),
        Actor::Service {
            principal: principal("gateway"),
        },
    )
    .expect("gateway context binds");
    let grant = AttestorGrant {
        namespaces: Some(vec!["slack.t0123abc".to_owned()]),
    };

    assert!(
        broker
            .capability_surface(
                &gateway,
                Some(&grant),
                Some(&Attestation::for_subject(subject(), agent("some-agent"))),
            )
            .is_some(),
        "the permitted agent may be driven"
    );
    let (capabilities, _words, memory) = broker
        .capability_surface(
            &gateway,
            Some(&grant),
            Some(&Attestation::for_chat(
                subject(),
                agent("some-agent"),
                ChatScopeClaim {
                    transport: "scientist-slack".parse::<TransportId>().expect("transport"),
                    kind: ChatTransportKind::Slack,
                    conversation: Conversation {
                        kind: ConversationKind::Thread,
                        container: Some("t0123abc".to_owned()),
                        id: "c0123abc".to_owned(),
                        thread: Some("1712345678.000100".to_owned()),
                    },
                    trigger: dekopon_broker::Trigger::Message,
                },
            )),
        )
        .expect("legacy subject-only attestor remains compatible with chat operations");
    assert!(!capabilities.is_empty());
    assert!(
        memory.is_none(),
        "subject-only attestation grants no storage scope"
    );

    let ordinary = request(98, "cli-probe.reverse");
    let ordinary_result = broker
        .invoke(
            &gateway,
            Some(&grant),
            Some(
                &Attestation::for_chat(
                    subject(),
                    agent("some-agent"),
                    ChatScopeClaim {
                        transport: "scientist-slack".parse::<TransportId>().expect("transport"),
                        kind: ChatTransportKind::Slack,
                        conversation: Conversation {
                            kind: ConversationKind::Thread,
                            container: Some("t0123abc".to_owned()),
                            id: "c0123abc".to_owned(),
                            thread: Some("1712345678.000100".to_owned()),
                        },
                        trigger: dekopon_broker::Trigger::Message,
                    },
                )
                .bound_to(ordinary.id.clone()),
            ),
            ordinary,
            Default::default(),
        )
        .await
        .expect("ordinary subject-only chat executes through the upgraded chat operation");
    assert_eq!(ordinary_result.result.outcome, InvocationOutcome::Succeeded);

    assert!(
        broker
            .capability_surface(
                &gateway,
                Some(&grant),
                Some(&Attestation::for_subject(subject(), agent("other-agent"))),
            )
            .is_none(),
        "an agent no policy names is refused exactly like an unhonored attestation"
    );

    let proposal = request(99, "cli-probe.reverse");
    let refused = broker
        .invoke(
            &gateway,
            Some(&grant),
            Some(
                &Attestation::for_subject(subject(), agent("other-agent"))
                    .bound_to(proposal.id.clone()),
            ),
            proposal,
            Default::default(),
        )
        .await
        .expect("a refused agent is still an accounted decision");
    assert_eq!(refused.result.outcome, InvocationOutcome::Denied);
    assert_eq!(refused.result.error.as_deref(), Some("agent-denied"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_attestor_without_namespaces_speaks_for_exactly_the_mapped_subjects() {
    let broker = broker("cpetersen").await;
    let gateway = AuthenticatedContext::new(
        principal("gateway"),
        Actor::Service {
            principal: principal("gateway"),
        },
    )
    .expect("gateway context binds");
    let grant = AttestorGrant { namespaces: None };
    assert!(
        broker
            .capability_surface(
                &gateway,
                Some(&grant),
                Some(&Attestation::for_subject(subject(), agent("some-agent"))),
            )
            .is_some()
    );
    assert!(
        broker
            .capability_surface(
                &gateway,
                Some(&grant),
                Some(&Attestation::for_subject(
                    "slack.t0123abc.uother".parse().expect("subject"),
                    agent("some-agent"),
                )),
            )
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_the_senders_service_cannot_produce_is_refused() {
    let broker = broker("cpetersen").await;
    let gateway = AuthenticatedContext::new(
        principal("gateway"),
        Actor::Service {
            principal: principal("gateway"),
        },
    )
    .expect("gateway context binds");
    let discord_claim_for_a_slack_sender = Attestation::for_chat(
        subject(),
        agent("some-agent"),
        ChatScopeClaim {
            transport: "elote-logs".parse::<TransportId>().expect("transport"),
            kind: ChatTransportKind::Discord,
            conversation: Conversation {
                kind: ConversationKind::DirectMessage,
                container: None,
                id: "1338356895504793623".to_owned(),
                thread: None,
            },
        },
    );
    assert!(
        broker
            .capability_surface(
                &gateway,
                Some(&AttestorGrant { namespaces: None }),
                Some(&discord_claim_for_a_slack_sender),
            )
            .is_none()
    );
}
