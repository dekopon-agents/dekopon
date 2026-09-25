//! Since a caller controls input size, the span renders input through the attribute cap directly
//! rather than building the full value and cutting it, avoiding a copy of large attachments per
//! layer.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use dekopon_broker::{
    Attestation, AttestorGrant, AuthenticatedContext, Broker, BrokerLimits, CapabilityRoute,
    ConstraintCatalog, ConstraintSet, CredentialStore, IdentityDirectory, InMemoryAuditLog,
    InvocationRequest, PolicyEngine, PolicyWorld,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{EffectKind, ExecutionConstraints, InvocationOutcome};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, PrincipalId, ProviderId, RiskLevel,
};
use dekopon_test_support::{CaptureLayer, provider_fixture};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

const TRACE_PARENT: &str = "00-0000000000000000000000000000f1c7-00000000000000f1-00";

const SUBJECT: &str = "slack.t0123abc.u9xyz";

const POLICIES: &str = r#"
@id("allow-count")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"cli-probe.count",
       resource == Dekopon::Provider::"cli-probe");

@id("prompt")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"reviewer");
"#;

fn principal(name: &str) -> PrincipalId {
    name.parse().expect("valid principal fixture")
}

fn capability() -> CapabilityId {
    "cli-probe.count".parse().expect("valid capability fixture")
}

fn constraint_set() -> (CapabilityId, ConstraintSet) {
    (
        capability(),
        ConstraintSet {
            route: CapabilityRoute::Generic,
            provider: "cli-probe"
                .parse::<ProviderId>()
                .expect("valid provider fixture"),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Low,
            credential: None,
            constraints: ExecutionConstraints::default(),
        },
    )
}

async fn broker() -> Broker<InMemoryAuditLog> {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("cli-probe provider fixture loads");
    let world = PolicyWorld::new(
        [principal("caller")],
        [(
            capability(),
            "cli-probe".parse::<ProviderId>().expect("provider"),
        )],
    )
    .expect("the world builds");
    Broker::new(
        registry,
        principal("broker-test"),
        "span-payloads".to_owned(),
        PolicyEngine::new(POLICIES, &world).expect("the policy set validates"),
        ConstraintCatalog::new([constraint_set()]).expect("one capability builds a catalog"),
        CredentialStore::empty(),
        IdentityDirectory::new([(
            SUBJECT
                .parse::<ExternalSubject>()
                .expect("canonical subject"),
            principal("caller"),
        )])
        .expect("one mapping builds a directory"),
        Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound")),
        BrokerLimits::default(),
    )
    .expect("the broker starts")
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

#[tokio::test(flavor = "multi_thread")]
async fn a_proposal_past_the_cap_is_recorded_truncated_beside_its_full_length() {
    let captured = CaptureLayer::workspace();
    tracing_subscriber::registry().with(captured.clone()).init();

    let input = serde_json::json!({ "text": "x".repeat(16_000) });
    let input_bytes = input.to_string().len();
    let id = "invoke-bounded"
        .parse::<dekopon_core::InvocationId>()
        .expect("valid invocation fixture");
    let attestation = Attestation::for_subject(
        SUBJECT
            .parse::<ExternalSubject>()
            .expect("canonical subject"),
        "reviewer".parse::<AgentId>().expect("valid agent"),
    )
    .bound_to(id.clone());
    let result = broker()
        .await
        .invoke(
            &gateway(),
            Some(&AttestorGrant { namespaces: None }),
            Some(&attestation),
            InvocationRequest {
                id,
                capability: capability(),
                trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                input,
                secret_use: None,
            },
            Default::default(),
        )
        .await
        .expect("a permitted capability runs");
    assert_eq!(result.result.outcome, InvocationOutcome::Succeeded);

    let cap = dekopon_core::MAX_ATTRIBUTE_BYTES;
    let authorize = captured
        .spans()
        .into_iter()
        .filter(|(name, _)| *name == "broker.authorize")
        .map(|(_, fields)| fields)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        authorize.contains(&format!(
            "input={{\"text\":\"{}…[truncated]",
            "x".repeat(cap - 9)
        )),
        "{authorize}"
    );
    assert!(
        authorize.contains(&format!("input.bytes={input_bytes}")),
        "{authorize}"
    );
    assert!(
        !authorize.contains(&"x".repeat(cap)),
        "no attribute carries more than the cap"
    );
}
