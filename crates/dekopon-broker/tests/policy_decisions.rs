//! The workflow decision table, evaluated end to end through a real broker.
//!
//! Every expectation below was first verified against the exact-match policy engine this Cedar
//! adapter replaced: on 2026-08-16 a temporary parity test built both engines from equivalent
//! configurations, ran all eight capability rows through each, and asserted identical allow/deny
//! outcomes. That test was deleted along with `ExactPolicy`; these hardcoded outcomes are what it
//! proved.
//!
//! The `agent.prompt` rows have no exact-engine counterpart — the session gate is authority the
//! Cedar migration adds — so they are asserted against their documented intent alone.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use dekopon_broker::{
    AttestorGrant, AuthenticatedContext, Broker, BrokerLimits, ChatScopeClaim, ChatSessionClaim,
    ChatTransportKind, ConstraintCatalog, ConstraintSet, CredentialStore, IdentityDirectory,
    InMemoryAuditLog, InvocationRequest, PolicyEngine, PolicyWorld, SubjectAttestation,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{EffectKind, ExecutionConstraints, Idempotency, InvocationOutcome};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId, ProviderId,
    RiskLevel, TraceId, TransportId,
};
use serde_json::json;

const SLACK_SUBJECT: &str = "slack.t0123abc.u9xyz";

/// `echo.echo` is direct-only; `echo.reverse` is attested-only; `agent.prompt` gates the session.
const POLICIES: &str = r#"
@id("direct-echo")
permit(principal == Dekopon::Principal::"direct-caller",
       action == Dekopon::Action::"echo.echo",
       resource == Dekopon::Provider::"echo")
when { context has agent && context.agent == "provider-test" }
unless { context has via };

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
"#;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(format!("examples/providers/{name}"))
}

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
    "echo".parse().expect("valid provider fixture")
}

fn subject() -> ExternalSubject {
    SLACK_SUBJECT.parse().expect("canonical subject fixture")
}

/// One row: who is asking, as which agent, through which gateway, for what.
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
        label: "direct caller reaches its own direct grant",
        principal: "direct-caller",
        agent: "provider-test",
        via: None,
        capability: "echo.echo",
        allowed: true,
    },
    Row {
        label: "direct caller does not reach the attested grant",
        principal: "direct-caller",
        agent: "provider-test",
        via: None,
        capability: "echo.reverse",
        allowed: false,
    },
    Row {
        label: "attested caller reaches its attested grant",
        principal: "cpetersen",
        agent: "some-agent",
        via: Some("gateway"),
        capability: "echo.reverse",
        allowed: true,
    },
    Row {
        label: "attested caller does not reach the direct grant",
        principal: "cpetersen",
        agent: "some-agent",
        via: Some("gateway"),
        capability: "echo.echo",
        allowed: false,
    },
    Row {
        label: "the mapped principal arriving directly matches nothing",
        principal: "cpetersen",
        agent: "some-agent",
        via: None,
        capability: "echo.reverse",
        allowed: false,
    },
    Row {
        label: "the direct grant is not reachable through a gateway",
        principal: "direct-caller",
        agent: "provider-test",
        via: Some("gateway"),
        capability: "echo.echo",
        allowed: false,
    },
    Row {
        label: "a different agent under the same attestation matches nothing",
        principal: "cpetersen",
        agent: "other-agent",
        via: Some("gateway"),
        capability: "echo.reverse",
        allowed: false,
    },
    Row {
        label: "an out-of-scope principal matches nothing",
        principal: "someone-else",
        agent: "provider-test",
        via: None,
        capability: "echo.echo",
        allowed: false,
    },
];

fn constraint_set(capability_id: &str) -> (CapabilityId, ConstraintSet) {
    (
        capability(capability_id),
        ConstraintSet {
            provider: provider(),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Low,
            idempotency: Idempotency::Idempotent,
            credential: None,
            credential_by_agent: BTreeMap::new(),
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
            principal("someone-else"),
        ],
        [
            (capability("echo.echo"), provider()),
            (capability("echo.reverse"), provider()),
        ],
    )
    .expect("the workflow world builds");
    PolicyEngine::new(POLICIES, &world).expect("the workflow policy set validates")
}

async fn broker(mapped_principal: &str) -> Broker<InMemoryAuditLog> {
    let registry =
        BrokerProviderRegistry::load([fixture("echo-provider.wasm")], BrokerHostLimits::default())
            .await
            .expect("echo provider fixture loads");
    Broker::new(
        registry,
        principal("broker-test"),
        "policy-decision-table".to_owned(),
        policy_engine(),
        ConstraintCatalog::new([constraint_set("echo.echo"), constraint_set("echo.reverse")])
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
        trace: "trace-table"
            .parse::<TraceId>()
            .expect("valid trace fixture"),
        trace_parent: None,
        input: json!({"message": "decision table"}),
    }
}

/// The eight capability rows, evaluated through `invoke`/`invoke_for` rather than the policy
/// engine directly, so the assertion covers the whole decision path.
#[tokio::test(flavor = "multi_thread")]
async fn the_workflow_decision_table_holds_end_to_end() {
    for (index, row) in TABLE.iter().enumerate() {
        let broker = broker(row.principal).await;
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
                    .invoke(&context, request)
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
                let attestation = SubjectAttestation {
                    subject: subject(),
                    agent: agent(row.agent),
                    invocation: request.id.clone(),
                };
                broker
                    .invoke_for(
                        &peer,
                        Some(&AttestorGrant {
                            namespaces: vec!["slack.t0123abc".to_owned()],
                            chat_scopes: Vec::new(),
                        }),
                        &attestation,
                        request,
                    )
                    .await
                    .expect("the attested proposal is accounted")
            }
        };
        assert_eq!(
            result.outcome != InvocationOutcome::Denied,
            row.allowed,
            "row {index} ({}) decided the wrong way: {result:?}",
            row.label
        );
    }
}

/// The session gate is its own statement: permitting `cpetersen` to talk to `some-agent` through
/// the gateway grants nothing to a different agent, a different principal, or a direct arrival.
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
        namespaces: vec!["slack.t0123abc".to_owned()],
        chat_scopes: Vec::new(),
    };

    assert!(
        broker
            .capabilities_for(&gateway, Some(&grant), &subject(), &agent("some-agent"))
            .is_some(),
        "the permitted agent may be driven"
    );
    let (capabilities, _words, memory) = broker
        .capabilities_for_chat(
            &gateway,
            Some(&grant),
            &ChatSessionClaim {
                subject: subject(),
                agent: agent("some-agent"),
                scope: ChatScopeClaim {
                    transport: "scientist-slack".parse::<TransportId>().expect("transport"),
                    kind: ChatTransportKind::Slack,
                    channel: "c0123abc".to_owned(),
                    conversation: "c0123abc:1712345678.000100".to_owned(),
                },
            },
        )
        .expect("legacy subject-only attestor remains compatible with chat operations");
    assert!(!capabilities.is_empty());
    assert!(
        memory.is_none(),
        "subject-only attestation grants no storage scope"
    );
    assert!(
        broker
            .capabilities_for(&gateway, Some(&grant), &subject(), &agent("other-agent"))
            .is_none(),
        "an agent no policy names is refused exactly like an unhonored attestation"
    );

    // The refusal is an audited denial rather than an error, and it names its own reason: the
    // attestation was honored, so `attestation-denied` would misattribute what was refused.
    let proposal = request(99, "echo.reverse");
    let refused = broker
        .invoke_for(
            &gateway,
            Some(&grant),
            &SubjectAttestation {
                subject: subject(),
                agent: agent("other-agent"),
                invocation: proposal.id.clone(),
            },
            proposal,
        )
        .await
        .expect("a refused agent is still an accounted decision");
    assert_eq!(refused.outcome, InvocationOutcome::Denied);
    assert_eq!(refused.error.as_deref(), Some("agent-denied"));
}
