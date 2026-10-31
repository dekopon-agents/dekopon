use dekopon_broker::*;
use dekopon_core::{Actor, PrincipalId};
use serde_json::{Value, json};
use std::sync::Arc;

pub async fn broker(policies: &str) -> (Broker<InMemoryAuditLog>, Arc<InMemoryAuditLog>) {
    let principal = |name: &str| name.parse::<PrincipalId>().unwrap();
    let mappings = [
        (
            "drn:github:repo:owner:repo".parse().unwrap(),
            principal("repo"),
        ),
        (
            "drn:slack:user:team:user".parse().unwrap(),
            principal("person"),
        ),
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
    let policies = format!(
        r#"@id("fixture-prompt") permit(principal,action == Dekopon::Action::"agent.prompt",resource);
{policies}"#
    );
    let audit = Arc::new(InMemoryAuditLog::new(32).unwrap());
    let broker = super::broker_support::configured_broker(
        PolicyEngine::new(&policies, &world).unwrap(),
        IdentityDirectory::new(mappings).unwrap(),
        Arc::clone(&audit),
    )
    .await;
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
        dekopon_core::AttestedSubject::github_repository("owner", "repo", 1).unwrap(),
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
