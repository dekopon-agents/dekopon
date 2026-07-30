use std::{fs, os::unix::fs::PermissionsExt as _};

use dekopon_broker::PolicyRule;
use dekopon_capability::{EffectKind, ExecutionConstraints, Idempotency};
use dekopon_core::{Actor, AgentId, CapabilityId, PrincipalId, ProviderId, RiskLevel};
use serde_json::json;

use super::{config, current_uid, socket};

fn rule() -> PolicyRule {
    PolicyRule {
        principal: "caller"
            .parse::<PrincipalId>()
            .expect("valid principal fixture"),
        actor: Actor::Agent {
            agent: "brokerd-test"
                .parse::<AgentId>()
                .expect("valid agent fixture"),
        },
        capability: "echo.echo"
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        provider: "echo"
            .parse::<ProviderId>()
            .expect("valid provider fixture"),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        idempotency: Idempotency::Idempotent,
        constraints: ExecutionConstraints::default(),
    }
}

#[tokio::test]
async fn strict_configuration_resolves_paths_and_rejects_unknown_fields() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let document = json!({
        "apiVersion": config::CONFIG_API_VERSION,
        "socketPath": "broker.sock",
        "auditPath": "audit.jsonl",
        "checkpointPath": "checkpoint.json",
        "checkpointLockPath": "checkpoint.lock",
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "providers": ["echo.wasm"],
        "identities": [{
            "uid": uid,
            "principal": "caller",
            "actor": {"type": "agent", "agent": "brokerd-test"}
        }],
        "rules": [serde_json::to_value(rule()).expect("rule serializes")]
    });
    fs::write(
        &path,
        serde_json::to_vec(&document).expect("config serializes"),
    )
    .expect("write config fixture");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("secure config fixture");
    fs::write(directory.path().join("echo.wasm"), b"component fixture")
        .expect("write provider path fixture");
    let resolved = config::load(&path, uid).await.expect("strict config loads");
    let canonical_directory =
        fs::canonicalize(directory.path()).expect("canonical fixture directory");
    assert_eq!(
        resolved.socket_path,
        canonical_directory.join("broker.sock")
    );
    assert_eq!(resolved.audit_path, canonical_directory.join("audit.jsonl"));
    assert_eq!(
        resolved.checkpoint_path,
        canonical_directory.join("checkpoint.json")
    );
    assert_eq!(
        resolved.checkpoint_lock_path,
        canonical_directory.join("checkpoint.lock")
    );
    assert_eq!(resolved.providers, [canonical_directory.join("echo.wasm")]);

    let mut conflicting = document.clone();
    conflicting["checkpointLockPath"] = json!("checkpoint.json.tmp");
    fs::write(
        &path,
        serde_json::to_vec(&conflicting).expect("conflict fixture serializes"),
    )
    .expect("replace config fixture");
    assert!(config::load(&path, uid).await.is_err());

    let mut invalid = document;
    invalid["principal"] = json!("payload-forgery");
    fs::write(
        &path,
        serde_json::to_vec(&invalid).expect("invalid fixture serializes"),
    )
    .expect("replace config fixture");
    assert!(config::load(&path, uid).await.is_err());
}

#[tokio::test]
async fn configuration_rejects_symlinks_and_hard_links() {
    use std::os::unix::fs::symlink;

    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration security fixture");
    let target = directory.path().join("target.yaml");
    let link = directory.path().join("link.yaml");
    let hard_link = directory.path().join("hard-link.yaml");
    fs::write(&target, b"{}").expect("write target fixture");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("secure target fixture");
    symlink(&target, &link).expect("create symlink fixture");
    assert!(config::load(&link, uid).await.is_err());
    fs::hard_link(&target, &hard_link).expect("create hard-link fixture");
    assert!(config::load(&target, uid).await.is_err());
}

#[tokio::test]
async fn socket_binding_requires_private_parent_and_refuses_live_replacement() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create socket fixture");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("secure socket parent");
    let path = directory.path().join("broker.sock");
    let (_listener, mut guard) = socket::bind(&path, uid)
        .await
        .expect("bind private broker socket");
    assert!(socket::bind(&path, uid).await.is_err());
    guard.cleanup().expect("remove exact socket inode");
    assert!(!path.exists());
}

#[tokio::test]
async fn stale_socket_is_replaced_but_guard_never_removes_a_new_inode() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create stale socket fixture");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("secure socket parent");
    let path = directory.path().join("broker.sock");
    let (listener, guard) = socket::bind(&path, uid).await.expect("bind first socket");
    drop(listener);
    let parked = directory.path().join("parked.sock");
    fs::rename(&path, &parked).expect("park stale socket around guard cleanup");
    drop(guard);
    fs::rename(&parked, &path).expect("restore stale socket fixture");
    let (listener, mut guard) = socket::bind(&path, uid)
        .await
        .expect("replace safe stale socket");
    fs::remove_file(&path).expect("remove guarded socket before replacement");
    let replacement = tokio::net::UnixListener::bind(&path).expect("bind replacement socket");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .expect("secure replacement socket");
    assert!(guard.cleanup().is_err());
    assert!(path.exists());
    drop(replacement);
    drop(listener);
    fs::remove_file(path).expect("remove replacement fixture");
}
