use std::sync::Arc;

use dekopon_capability::ExecutionConstraints;
use dekopon_core::{Actor, AgentId, CapabilityId, InvocationId, PrincipalId, TraceId};

use super::{
    AuditConfigurationError, AuditError, AuditEvent, AuditIntegrityError, AuditLog,
    AuthenticatedContext, ContextError, InMemoryAuditLog, verify_audit_chain,
};

fn decision(invocation: &str, allowed: bool) -> AuditEvent {
    AuditEvent::Decision {
        invocation: invocation
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        trace: "trace-test"
            .parse::<TraceId>()
            .expect("valid trace fixture"),
        principal: "caller"
            .parse::<PrincipalId>()
            .expect("valid principal fixture"),
        actor: Actor::Agent {
            agent: "reviewer".parse::<AgentId>().expect("valid agent fixture"),
        },
        capability: "echo.echo"
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        provider: None,
        authorized_by: "broker"
            .parse::<PrincipalId>()
            .expect("valid principal fixture"),
        decision_id: format!("decision-{invocation}"),
        policy_revision: "policy-test".to_owned(),
        allowed,
        reason: (!allowed).then(|| "policy-denied".to_owned()),
        decision_digest: format!("sha256:{}", "a".repeat(64)),
    }
}

#[test]
fn authenticated_human_identity_must_match_transport_principal() {
    let error = AuthenticatedContext::new(
        "alice"
            .parse::<PrincipalId>()
            .expect("valid principal fixture"),
        Actor::Human {
            principal: "mallory"
                .parse::<PrincipalId>()
                .expect("valid principal fixture"),
        },
    )
    .expect_err("payload identity cannot override transport identity");
    assert_eq!(error, ContextError::PrincipalMismatch);
}

#[tokio::test]
async fn audit_chain_detects_content_and_link_mutation() {
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    audit
        .append(decision("invoke-one", true))
        .await
        .expect("first append succeeds");
    audit
        .append(decision("invoke-two", false))
        .await
        .expect("second append succeeds");
    let records = audit.records().await;
    verify_audit_chain(&records).expect("fresh chain verifies");

    let mut mutated = records.clone();
    if let AuditEvent::Decision { allowed, .. } = &mut mutated[0].event {
        *allowed = false;
    }
    assert_eq!(
        verify_audit_chain(&mutated),
        Err(AuditIntegrityError::RecordHash { index: 0 })
    );

    let mut reordered = records;
    reordered.swap(0, 1);
    assert_eq!(
        verify_audit_chain(&reordered),
        Err(AuditIntegrityError::Sequence { index: 0 })
    );
}

#[tokio::test]
async fn in_memory_audit_fails_closed_at_its_bound() {
    assert!(matches!(
        InMemoryAuditLog::new(0),
        Err(AuditConfigurationError::ZeroMaximum)
    ));
    let audit = InMemoryAuditLog::new(1).expect("valid audit bound");
    audit
        .append(decision("invoke-one", true))
        .await
        .expect("first append succeeds");
    let error = audit
        .append(decision("invoke-two", true))
        .await
        .expect_err("second append exceeds bound");
    assert!(matches!(error, AuditError::Full { maximum: 1 }));
}

#[test]
fn policy_http_scope_values_are_bounded() {
    let constraints = ExecutionConstraints {
        http: Some(dekopon_capability::HttpConstraints {
            allowed_hosts: vec![" ".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            max_requests: 1,
            max_request_bytes: 1,
            max_response_bytes: 1,
            allow_plaintext_loopback: false,
        }),
        ..ExecutionConstraints::default()
    };
    assert!(super::validate_rule_constraints(&constraints).is_err());
}
