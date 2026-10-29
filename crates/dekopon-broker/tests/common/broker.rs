use dekopon_broker::{
    Broker, BrokerLimits, CapabilityRoute, ConstraintCatalog, ConstraintSet, CredentialStore,
    IdentityDirectory, InMemoryAuditLog, InvocationRequest, PolicyEngine,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{EffectKind, ExecutionConstraints};
use dekopon_core::{CapabilityId, InvocationId, PrincipalId, ProviderId, RiskLevel};
use dekopon_test_support::provider_fixture;
use serde_json::json;
use std::sync::Arc;

pub(crate) const TRACE_PARENT: &str = "00-0000000000000000000000000000f1c7-00000000000000f1-00";

pub(crate) fn principal(name: &str) -> PrincipalId {
    name.parse().expect("valid principal fixture")
}

pub(crate) fn capability(name: &str) -> CapabilityId {
    name.parse().expect("valid capability fixture")
}

pub(crate) fn provider() -> ProviderId {
    "cli-probe".parse().expect("valid provider fixture")
}

pub(crate) fn constraint_set(capability_id: &str) -> (CapabilityId, ConstraintSet) {
    (
        capability(capability_id),
        ConstraintSet {
            default_secret_use: None,
            route: CapabilityRoute::Generic,
            provider: provider(),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Low,
            credential: None,
            constraints: ExecutionConstraints::default(),
        },
    )
}

pub(crate) async fn configured_broker(
    policy: PolicyEngine,
    identities: IdentityDirectory,
    audit: Arc<InMemoryAuditLog>,
) -> Broker<InMemoryAuditLog> {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("cli-probe provider fixture loads");
    Broker::new(
        registry,
        principal("broker-test"),
        policy,
        ConstraintCatalog::new([constraint_set("cli-probe.reverse")])
            .expect("one capability builds a catalog"),
        CredentialStore::empty(),
        identities,
        audit,
        BrokerLimits::default(),
    )
    .expect("the broker starts")
}

pub(crate) fn request(index: usize, capability_id: &str) -> InvocationRequest {
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
