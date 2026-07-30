use std::{fs, sync::Arc};

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt as _, symlink};

use dekopon_capability::ExecutionConstraints;
use dekopon_core::{Actor, AgentId, CapabilityId, InvocationId, PrincipalId, TraceId};

use super::{
    AuditConfigurationError, AuditError, AuditEvent, AuditIntegrityError, AuditLog, AuditRecord,
    AuthenticatedContext, ContextError, FileAuditError, FileAuditLog, InMemoryAuditLog,
    verify_audit_chain,
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

#[tokio::test]
async fn durable_audit_reopens_verifies_and_continues_the_chain() {
    let directory = tempfile::tempdir().expect("create audit fixture directory");
    let path = directory.path().join("audit.jsonl");
    let audit = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect("create durable audit");
    audit
        .append(decision("invoke-one", true))
        .await
        .expect("first durable append succeeds");
    audit
        .append(decision("invoke-two", false))
        .await
        .expect("second durable append succeeds");
    let checkpoint = audit.checkpoint().await;
    assert_eq!(checkpoint.0, 2);
    assert!(checkpoint.1.is_some());
    assert!(audit.contains_checkpoint(0, None).await);
    assert!(audit.contains_checkpoint(2, checkpoint.1.as_deref()).await);
    let first = serde_json::from_str::<AuditRecord>(
        fs::read_to_string(&path)
            .expect("read synchronized audit")
            .lines()
            .next()
            .expect("first record exists"),
    )
    .expect("first record decodes");
    assert!(audit.contains_checkpoint(1, Some(&first.record_hash)).await);
    assert!(!audit.contains_checkpoint(1, checkpoint.1.as_deref()).await);
    assert!(!audit.contains_checkpoint(3, None).await);
    let error = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect_err("a second writer must not share the audit file");
    assert!(matches!(error, FileAuditError::Lock { .. }));
    drop(audit);

    let audit = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect("existing chain verifies");
    assert_eq!(audit.checkpoint().await, checkpoint);
    assert_eq!(
        audit
            .replay_ids()
            .await
            .iter()
            .map(InvocationId::as_str)
            .collect::<Vec<_>>(),
        ["invoke-one", "invoke-two"]
    );
    audit
        .append(decision("invoke-three", true))
        .await
        .expect("append continues verified chain");
    drop(audit);

    let records = fs::read_to_string(&path)
        .expect("read durable fixture")
        .lines()
        .map(|line| serde_json::from_str::<AuditRecord>(line).expect("valid durable record"))
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 3);
    verify_audit_chain(&records).expect("reopened chain remains valid");

    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&path)
            .expect("audit metadata")
            .permissions()
            .mode()
            & 0o077,
        0
    );
}

#[tokio::test]
async fn durable_audit_rejects_mutation_and_partial_records() {
    let directory = tempfile::tempdir().expect("create audit fixture directory");
    let path = directory.path().join("audit.jsonl");
    let audit = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect("create durable audit");
    audit
        .append(decision("invoke-one", true))
        .await
        .expect("durable append succeeds");
    drop(audit);

    let original = fs::read_to_string(&path).expect("read durable fixture");
    let mutated = original.replace("\"allowed\":true", "\"allowed\":false");
    assert_ne!(mutated, original);
    fs::write(&path, mutated).expect("tamper with durable fixture");
    let error = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect_err("semantic mutation must fail verification");
    assert!(matches!(
        error,
        FileAuditError::Integrity {
            source: AuditIntegrityError::RecordHash { index: 0 },
            ..
        }
    ));

    fs::write(&path, format!("{original}{{\"partial\":")).expect("write partial durable fixture");
    let error = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect_err("partial final record must fail closed");
    assert!(matches!(
        error,
        FileAuditError::UnterminatedRecord { line: 2 }
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn durable_audit_rejects_non_private_permissions() {
    let directory = tempfile::tempdir().expect("create audit fixture directory");
    let path = directory.path().join("audit.jsonl");
    fs::write(&path, []).expect("create audit fixture");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
        .expect("set insecure fixture permissions");
    let error = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect_err("group/world-readable audit must fail");
    assert!(matches!(error, FileAuditError::InsecureFile));

    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .expect("restore private fixture permissions");
    let hard_link = directory.path().join("audit-hard-link.jsonl");
    fs::hard_link(&path, &hard_link).expect("create hard-link fixture");
    let error = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect_err("multiply linked audit must fail");
    assert!(matches!(error, FileAuditError::InsecureFile));

    fs::remove_file(&hard_link).expect("remove hard-link fixture");
    let symlink_path = directory.path().join("audit-symlink.jsonl");
    symlink(&path, &symlink_path).expect("create symlink fixture");
    let error = FileAuditLog::open(&symlink_path, 4, 16 * 1024)
        .await
        .expect_err("audit symlink must not be followed");
    assert!(matches!(error, FileAuditError::Io { .. }));
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
