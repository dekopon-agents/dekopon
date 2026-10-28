use dekopon_broker::*;
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{EffectKind, ExecutionConstraints};
use dekopon_core::{Actor, PrincipalId, RiskLevel};
use dekopon_test_support::provider_fixture;
use serde_json::{Value, json};
use std::sync::Arc;

pub async fn broker(policies: &str) -> (Broker<InMemoryAuditLog>, Arc<InMemoryAuditLog>) {
    let principal = |name: &str| name.parse::<PrincipalId>().unwrap();
    let mappings = [
        ("github.owner.repo".parse().unwrap(), principal("repo")),
        ("slack.team.user".parse().unwrap(), principal("person")),
    ];
    let world = PolicyWorld::new(
        [principal("repo"), principal("person")],
        [(
            "cli-probe.reverse".parse().unwrap(),
            "cli-probe".parse().unwrap(),
        )],
    )
    .unwrap()
    .with_subjects(mappings.clone());
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .unwrap();
    let audit = Arc::new(InMemoryAuditLog::new(32).unwrap());
    let broker = Broker::new(
        registry,
        principal("broker-test"),
        PolicyEngine::new(policies, &world).unwrap(),
        ConstraintCatalog::new([(
            "cli-probe.reverse".parse().unwrap(),
            ConstraintSet {
                default_secret_use: None,
                route: CapabilityRoute::Generic,
                provider: "cli-probe".parse().unwrap(),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                credential: None,
                constraints: ExecutionConstraints::default(),
            },
        )])
        .unwrap(),
        CredentialStore::empty(),
        IdentityDirectory::new(mappings).unwrap(),
        Arc::clone(&audit),
        BrokerLimits::default(),
    )
    .unwrap();
    (broker, audit)
}

pub fn event(name: &str, delivery: &str) -> Value {
    let mut event = json!({"delivery":delivery,"event":name,"repository":{"id":10,"fullName":"owner/repo"},"actor":{"id":1},"number":7,"authorSet":{"actorLogin":"owner","headRef":"x"}});
    let target = if name == "issue_comment" {
        json!({"kind":"issueComment","issue":{"authorId":1,"authorAssociation":"OWNER","labels":[],"assignees":[],"isPullRequest":false},"comment":{"id":2,"authorAssociation":"OWNER"}})
    } else {
        json!({"kind":"pullRequest","action":"synchronize","pullRequest":{"baseRef":"main","baseSha":"a".repeat(40),"merged":false,"crossRepository":false,"authorId":1,"authorAssociation":"OWNER","labels":[]}})
    };
    event
        .as_object_mut()
        .unwrap()
        .extend(target.as_object().unwrap().clone());
    event
}

pub fn claim(events: Vec<Value>) -> Attestation {
    let scope:ChatScopeClaim=serde_json::from_value(json!({"transport":"github","kind":"github","conversation":{"kind":"thread","container":"owner","id":"owner.repo","thread":"7"},"trigger":"message","delivery":{"kind":"github","delivery":"delivery-1"},"github":events})).unwrap();
    Attestation::for_chat(
        "github.owner.repo".parse().unwrap(),
        "some-agent".parse().unwrap(),
        scope,
    )
}

pub fn gateway() -> AuthenticatedContext {
    let principal = "gateway".parse().unwrap();
    AuthenticatedContext::new(
        principal,
        Actor::Service {
            principal: "gateway".parse().unwrap(),
        },
    )
    .unwrap()
}
