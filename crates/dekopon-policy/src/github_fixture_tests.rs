use super::*;
use dekopon_capability::EffectKind;
use dekopon_core::RiskLevel;

#[test]
fn github_service_notes_need_their_own_agent_scoped_grant() {
    let world = PolicyWorld::new(
        ["repo-principal", "other-principal"].map(|name| name.parse().unwrap()),
        [
            ("github.read".parse().unwrap(), "github".parse().unwrap()),
            ("turso.exec".parse().unwrap(), "turso".parse().unwrap()),
        ],
    )
    .unwrap();
    let base = r#"@id("prompt")
permit(principal == Dekopon::Principal::"repo-principal",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"reviewer")
when { context.via == "gateway" };
@id("read")
permit(principal == Dekopon::Principal::"repo-principal",
       action == Dekopon::Action::"github.read",
       resource == Dekopon::Provider::"github")
when { context.via == "gateway" && context.agent == "reviewer" };"#;
    let notes = r#"@id("optional-notes")
permit(principal == Dekopon::Principal::"repo-principal",
       action == Dekopon::Action::"turso.exec",
       resource == Dekopon::Provider::"turso")
when { context.via == "gateway" && context.agent == "reviewer" };"#;
    let request = |principal: &str, name: &str, provider: &str| PolicyRequest {
        principal: principal.parse().unwrap(),
        target: PolicyTarget::Capability {
            capability: name.parse().unwrap(),
            provider: provider.parse().unwrap(),
            effect: if name == "turso.exec" {
                EffectKind::ExternalWrite
            } else {
                EffectKind::ReadOnly
            },
            risk: RiskLevel::Low,
        },
        context: PolicyContext {
            via: Some("gateway".into()),
            agent: Some("reviewer".into()),
            ..PolicyContext::default()
        },
    };
    let without = PolicyEngine::new(base, &world).unwrap();
    assert!(
        without
            .authorize(request("repo-principal", "github.read", "github"))
            .allowed
    );
    assert!(
        !without
            .authorize(request("repo-principal", "turso.exec", "turso"))
            .allowed
    );
    let with = PolicyEngine::new(&format!("{base}\n{notes}"), &world).unwrap();
    assert!(
        with.authorize(request("repo-principal", "turso.exec", "turso"))
            .allowed
    );
    assert!(
        !with
            .authorize(request("other-principal", "turso.exec", "turso"))
            .allowed
    );
    let wrong_agent = PolicyRequest {
        context: PolicyContext {
            agent: Some("other-agent".into()),
            ..request("repo-principal", "turso.exec", "turso").context
        },
        ..request("repo-principal", "turso.exec", "turso")
    };
    assert!(!with.authorize(wrong_agent).allowed);
}
