//! What `broker.authorize` carries of the proposal it was handed.
//!
//! The proposal is the one payload this span exists to preserve, and it is also the one field a
//! caller controls the size of: a chat attachment reaches the broker as base64 inside `input`. So
//! `input` rides the same cap as every other attribute, with `input.bytes` beside it for the uncut
//! length, and the value is rendered through the cap rather than built and then cut — a subscriber
//! that received the whole proposal before cutting it would cost a copy of the image per layer.
//!
//! This lives in its own test binary because `tracing` resolves per-callsite interest against the
//! global dispatcher, so a sibling test reaching this callsite with no subscriber installed can
//! disable it for the whole process.

use std::{collections::BTreeMap, sync::Arc};

use dekopon_broker::{
    AuthenticatedContext, Broker, BrokerLimits, CapabilityRoute, ConstraintCatalog, ConstraintSet,
    CredentialStore, IdentityDirectory, InMemoryAuditLog, InvocationRequest, PolicyEngine,
    PolicyWorld,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{EffectKind, ExecutionConstraints, InvocationOutcome};
use dekopon_core::{Actor, CapabilityId, PrincipalId, ProviderId, RiskLevel};
use dekopon_test_support::{CaptureLayer, provider_fixture};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

/// One fixture trace context for the request this test builds.
const TRACE_PARENT: &str = "00-0000000000000000000000000000f1c7-00000000000000f1-00";

const POLICIES: &str = r#"
@id("allow-count")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"cli-probe.count",
       resource == Dekopon::Provider::"cli-probe");
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
            credential_by_agent: BTreeMap::new(),
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
        IdentityDirectory::empty(),
        Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound")),
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

/// A proposal past the attribute cap is recorded as its first `MAX_ATTRIBUTE_BYTES` plus a marker,
/// beside the byte length of the whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_proposal_past_the_cap_is_recorded_truncated_beside_its_full_length() {
    let captured = CaptureLayer::workspace();
    tracing_subscriber::registry().with(captured.clone()).init();

    // Well past the attribute cap and inside the fixture's own 16 KiB text bound.
    let input = serde_json::json!({ "text": "x".repeat(16_000) });
    let input_bytes = input.to_string().len();
    let result = broker()
        .await
        .invoke(
            &caller(),
            None,
            None,
            InvocationRequest {
                id: "invoke-bounded".parse().expect("valid invocation fixture"),
                capability: capability(),
                trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                input,
                secret_use: None,
            },
        )
        .await
        .expect("a permitted capability runs");
    assert_eq!(result.outcome, InvocationOutcome::Succeeded);

    let cap = dekopon_core::MAX_ATTRIBUTE_BYTES;
    let authorize = captured
        .spans()
        .into_iter()
        .filter(|(name, _)| *name == "broker.authorize")
        .map(|(_, fields)| fields)
        .collect::<Vec<_>>()
        .join("\n");
    // `{"text":"` is the nine bytes of the cut prefix that are not the model's own text. The value
    // is still recorded through `Display`, so a reader sees the proposal rather than an escaped
    // rendering of it; what changed is only where it ends.
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
