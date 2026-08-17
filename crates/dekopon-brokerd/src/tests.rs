use std::{fs, os::unix::fs::PermissionsExt as _, path::Path};

use dekopon_broker::ConstraintSet;
use dekopon_capability::{EffectKind, ExecutionConstraints, Idempotency};
use dekopon_core::{ProviderId, RiskLevel};
use serde_json::json;

use super::{config, current_uid, socket};

/// The attested workflow policy: `cpetersen` may drive `chat-agent` and reach `echo.echo`, but
/// only through the gateway that vouched for them.
const POLICIES: &str = r#"
@id("chat-agent-session")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"chat-agent")
when { context has via && context.via == "gateway" };

@id("chat-agent-echo")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"echo.echo",
       resource == Dekopon::Provider::"echo")
when { context has via && context.via == "gateway"
    && context has agent && context.agent == "chat-agent" };
"#;

fn constraint_set() -> serde_json::Value {
    serde_json::to_value(ConstraintSet {
        provider: "echo"
            .parse::<ProviderId>()
            .expect("valid provider fixture"),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        idempotency: Idempotency::Idempotent,
        credential: None,
        constraints: ExecutionConstraints::default(),
    })
    .expect("constraint set serializes")
}

fn write_owner_only(path: &Path, contents: &[u8]) {
    fs::write(path, contents).expect("write fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("secure fixture");
}

fn write_config(path: &Path, document: &serde_json::Value) {
    write_owner_only(
        path,
        &serde_json::to_vec(document).expect("config serializes"),
    );
}

/// A configuration whose attested path is complete end to end: a gateway peer holding an attestor
/// grant, a mapping inside that grant, a policy file, and the constraint set the policy's
/// capability needs.
///
/// The gateway takes its own UID because that is the only shape in which `via` is real isolation.
/// `run` separately refuses any configured UID other than the server's, so this remains
/// configuration-level validation of a deployment the socket cannot yet express.
fn attested_document(uid: u32) -> serde_json::Value {
    json!({
        "apiVersion": config::CONFIG_API_VERSION,
        "socketPath": "broker.sock",
        "auditPath": "audit.jsonl",
        "checkpointPath": "checkpoint.json",
        "checkpointLockPath": "checkpoint.lock",
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "policiesPath": "policies.cedar",
        "providers": ["echo.wasm"],
        "identities": [
            {
                "uid": uid,
                "principal": "caller",
                "actor": {"type": "agent", "agent": "brokerd-test"}
            },
            {
                "uid": uid + 1,
                "principal": "gateway",
                "actor": {"type": "service", "principal": "gateway"},
                "attestor": {"namespaces": ["slack.t0123abc"]}
            }
        ],
        "identityMappings": [
            {"subject": "slack.t0123abc.u9xyz", "principal": "cpetersen"}
        ],
        "constraintSets": {"echo.echo": constraint_set()}
    })
}

/// The configuration layer owns two things the policy engine cannot see: that a deployment which
/// declares executable capabilities also declares a policy, and that the policy file is owner-only
/// trusted input read under the same rules as the configuration itself.
#[tokio::test]
async fn policy_and_constraint_configuration_is_resolved_and_owner_only() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let policies = directory.path().join("policies.cedar");
    fs::write(directory.path().join("echo.wasm"), b"component fixture")
        .expect("write provider path fixture");
    write_owner_only(&policies, POLICIES.as_bytes());

    let document = attested_document(uid);
    write_config(&path, &document);
    let resolved = config::load(&path, uid)
        .await
        .expect("a complete attested path resolves");
    assert_eq!(resolved.identity_mappings.len(), 1);
    assert_eq!(
        resolved.identity_mappings[0].subject.canonical(),
        "slack.t0123abc.u9xyz"
    );
    assert!(
        resolved.identities[1].attestor.is_some(),
        "the gateway keeps its owner-configured grant"
    );
    assert_eq!(
        resolved.policies_path.as_deref(),
        Some(
            fs::canonicalize(&policies)
                .expect("canonical policy fixture")
                .as_path()
        )
    );
    assert!(resolved.policies.contains("agent.prompt"));
    assert_eq!(resolved.constraint_sets.len(), 1);

    // Capabilities with no policy would refuse every request while looking configured.
    let mut orphaned = document.clone();
    orphaned
        .as_object_mut()
        .expect("config object")
        .remove("policiesPath");
    write_config(&path, &orphaned);
    let error = config::load(&path, uid)
        .await
        .expect_err("constraint sets without a policy file are a configuration mistake");
    assert!(matches!(error, config::ConfigError::MissingPoliciesPath));

    // A world-readable policy file is authorization input anyone could rewrite.
    write_config(&path, &document);
    fs::set_permissions(&policies, fs::Permissions::from_mode(0o666))
        .expect("loosen policy fixture");
    let error = config::load(&path, uid)
        .await
        .expect_err("a group/world-writable policy file must fail closed");
    assert!(matches!(error, config::ConfigError::InsecureFile { .. }));
    fs::set_permissions(&policies, fs::Permissions::from_mode(0o600))
        .expect("restore policy fixture");

    // And a second hard link is a second writer nobody accounted for.
    let hard_link = directory.path().join("policies-hard-link.cedar");
    fs::hard_link(&policies, &hard_link).expect("create policy hard-link fixture");
    let error = config::load(&path, uid)
        .await
        .expect_err("a multiply linked policy file must fail closed");
    assert!(matches!(error, config::ConfigError::InsecureFile { .. }));
    fs::remove_file(&hard_link).expect("remove policy hard-link fixture");
    config::load(&path, uid)
        .await
        .expect("the restored policy file loads");
}

/// Grants and mappings are owner-controlled identity machinery, so both fail closed on the shapes
/// that would make attribution ambiguous: a namespace that is not a canonical subject prefix, and
/// one subject naming two principals.
#[tokio::test]
async fn attestor_grants_and_subject_mappings_are_strictly_validated() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    fs::write(directory.path().join("echo.wasm"), b"component fixture")
        .expect("write provider path fixture");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );
    let document = attested_document(uid);

    let mut duplicated = document.clone();
    duplicated["identityMappings"] = json!([
        {"subject": "slack.t0123abc.u9xyz", "principal": "cpetersen"},
        {"subject": "slack.t0123abc.u9xyz", "principal": "someone-else"}
    ]);
    write_config(&path, &duplicated);
    let error = config::load(&path, uid)
        .await
        .expect_err("one subject must not name two principals");
    assert!(matches!(
        error,
        config::ConfigError::DuplicateSubject { subject } if subject == "slack.t0123abc.u9xyz"
    ));

    for namespaces in [
        json!(["sms"]),
        json!(["slack.T0123ABC"]),
        json!(["slack..u9xyz"]),
        json!([]),
    ] {
        let mut invalid = document.clone();
        invalid["identities"][1]["attestor"]["namespaces"] = namespaces.clone();
        write_config(&path, &invalid);
        let Err(error) = config::load(&path, uid).await else {
            panic!("accepted attestor namespaces {namespaces}");
        };
        assert!(
            matches!(error, config::ConfigError::Attestor { .. }),
            "namespaces {namespaces} produced {error}"
        );
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
        "policiesPath": "policies.cedar",
        "constraintSets": {"echo.echo": constraint_set()}
    });
    write_config(&path, &document);
    fs::write(directory.path().join("echo.wasm"), b"component fixture")
        .expect("write provider path fixture");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );
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

/// Telemetry is optional, strict when present, and never a place to put a credential.
#[tokio::test]
async fn telemetry_section_is_optional_and_strict() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let base = json!({
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
        "policiesPath": "policies.cedar",
        "constraintSets": {"echo.echo": constraint_set()}
    });
    fs::write(directory.path().join("echo.wasm"), b"component fixture")
        .expect("write provider path fixture");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );

    let write = |document: &serde_json::Value| write_config(&path, document);

    write(&base);
    assert!(
        config::load(&path, uid)
            .await
            .expect("config without telemetry loads")
            .telemetry
            .is_none()
    );

    let mut enabled = base.clone();
    enabled["telemetry"] = json!({
        "endpoint": "http://rpi.localdomain",
        "transport": "grpc",
        "serviceName": "dekopon-brokerd",
        "exportTimeoutMs": 5000,
        "telemetryPayloads": false
    });
    write(&enabled);
    let resolved = config::load(&path, uid)
        .await
        .expect("config with telemetry loads");
    let settings = resolved.telemetry.expect("telemetry resolved");
    assert_eq!(
        settings.settings.transport(),
        dekopon_telemetry::Transport::Grpc
    );
    assert_eq!(
        settings.settings.timeout(),
        std::time::Duration::from_millis(5_000)
    );
    assert!(!settings.telemetry_payloads);

    // A partial section, an unknown transport, and a zero timeout are all rejected rather than
    // quietly defaulted; the section follows the same all-fields-required rule as every other one.
    for broken in [
        json!({"endpoint": "http://rpi.localdomain", "transport": "grpc"}),
        // Every field is required once the section is present, so omitting only `telemetryPayloads`
        // fails rather than defaulting to the quiet setting — an operator who meant to enable it
        // and mistyped the key finds out at startup.
        json!({
            "endpoint": "http://rpi.localdomain",
            "transport": "grpc",
            "serviceName": "dekopon-brokerd",
            "exportTimeoutMs": 5000
        }),
        json!({
            "endpoint": "http://rpi.localdomain",
            "transport": "thrift",
            "serviceName": "dekopon-brokerd",
            "exportTimeoutMs": 5000,
            "telemetryPayloads": false
        }),
        json!({
            "endpoint": "http://rpi.localdomain",
            "transport": "http",
            "serviceName": "dekopon-brokerd",
            "exportTimeoutMs": 0,
            "telemetryPayloads": false
        }),
        json!({
            "endpoint": "  ",
            "transport": "http",
            "serviceName": "dekopon-brokerd",
            "exportTimeoutMs": 5000,
            "telemetryPayloads": false
        }),
        // A credential has no slot here. It belongs in `OTEL_EXPORTER_OTLP_HEADERS`, which the
        // SDK reads directly, so an unknown field is the correct answer rather than a warning.
        json!({
            "endpoint": "http://rpi.localdomain",
            "transport": "http",
            "serviceName": "dekopon-brokerd",
            "exportTimeoutMs": 5000,
            "authorization": "Basic c2VjcmV0"
        }),
    ] {
        let mut invalid = base.clone();
        invalid["telemetry"] = broken.clone();
        write(&invalid);
        assert!(
            config::load(&path, uid).await.is_err(),
            "accepted telemetry section {broken}"
        );
    }
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
