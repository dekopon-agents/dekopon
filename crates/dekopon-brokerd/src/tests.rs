use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _, path::Path};

use dekopon_broker::ConstraintSet;
use dekopon_capability::{EffectKind, ExecutionConstraints, Idempotency};
use dekopon_core::{ProviderId, RiskLevel};
use serde_json::json;

use super::{config, current_uid, server, socket};

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
        credential_by_agent: BTreeMap::new(),
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

    for scope in [
        json!({
            "breadth": "exactConversation", "kind": "slack", "transport": "slack",
            "channel": "c0123abc", "conversation": "c0123abc:01712345678.1"
        }),
        json!({
            "breadth": "exactChannel", "kind": "discord", "transport": "discord",
            "channel": "00123"
        }),
        json!({
            "breadth": "exactConversation", "kind": "telegram", "transport": "telegram",
            "channel": "-1001", "conversation": "-1001:topic:00"
        }),
        json!({
            "breadth": "transportWide", "kind": "local", "transport": "dev"
        }),
        json!({
            "breadth": "exactChannel", "kind": "slack", "transport": "slack",
            "channel": format!("c{}", "x".repeat(256))
        }),
    ] {
        let mut invalid = document.clone();
        invalid["identities"][1]["attestor"]["chatScopes"] = json!([scope.clone()]);
        write_config(&path, &invalid);
        let error = config::load(&path, uid)
            .await
            .expect_err("noncanonical exact chat scope must fail at startup");
        assert!(
            matches!(error, config::ConfigError::Attestor { .. }),
            "scope {scope} produced {error}"
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

/// Builds a minimal valid configuration document whose `providers` list is caller-supplied.
fn provider_config(uid: u32, providers: serde_json::Value) -> serde_json::Value {
    json!({
        "apiVersion": config::CONFIG_API_VERSION,
        "socketPath": "broker.sock",
        "auditPath": "audit.jsonl",
        "checkpointPath": "checkpoint.json",
        "checkpointLockPath": "checkpoint.lock",
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "providers": providers,
        "identities": [{
            "uid": uid,
            "principal": "caller",
            "actor": {"type": "agent", "agent": "brokerd-test"}
        }],
    })
}

/// A directory entry loads every `*.wasm` directly inside it, in filename order.
///
/// The sort is the point. The registry builds its capability route table in load order, so
/// readdir order would make two runs over one directory disagree about which provider claimed a
/// duplicate capability.
#[tokio::test]
async fn a_provider_directory_expands_in_filename_order() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let providers = directory.path().join("providers");
    fs::create_dir(&providers).expect("create provider directory");
    fs::set_permissions(&providers, fs::Permissions::from_mode(0o755))
        .expect("secure provider directory");

    // Written in an order that is neither sorted nor reverse-sorted.
    for name in ["middle.wasm", "alpha.wasm", "zulu.wasm"] {
        fs::write(providers.join(name), b"component fixture").expect("write component fixture");
    }
    // Neither of these is a component: one has the wrong extension, one is a directory.
    fs::write(providers.join("notes.txt"), b"not a component").expect("write decoy");
    fs::create_dir(providers.join("nested.wasm")).expect("create decoy directory");

    write_config(&path, &provider_config(uid, json!(["providers"])));
    let resolved = config::load(&path, uid)
        .await
        .expect("directory config loads");
    let canonical = fs::canonicalize(&providers).expect("canonical provider directory");
    assert_eq!(
        resolved.providers,
        [
            canonical.join("alpha.wasm"),
            canonical.join("middle.wasm"),
            canonical.join("zulu.wasm"),
        ]
    );
}

/// A directory anyone can write to is a directory anyone can add a provider to, and a provider is
/// code the broker compiles and runs.
///
/// The ownership half of the same check cannot be exercised without a second UID, and it is the
/// adjacent condition in the same expression as the mode check this proves.
#[tokio::test]
async fn a_group_writable_provider_directory_refuses_to_load() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let providers = directory.path().join("providers");
    fs::create_dir(&providers).expect("create provider directory");
    fs::write(providers.join("echo.wasm"), b"component fixture").expect("write component fixture");
    write_config(&path, &provider_config(uid, json!(["providers"])));

    fs::set_permissions(&providers, fs::Permissions::from_mode(0o775))
        .expect("loosen provider directory");
    let error = config::load(&path, uid)
        .await
        .expect_err("a group-writable provider directory refuses to load");
    assert!(
        matches!(error, config::ConfigError::InsecureProviderDirectory { .. }),
        "{error:?}"
    );

    // The same directory, tightened, loads. This is what proves the refusal was about the mode.
    fs::set_permissions(&providers, fs::Permissions::from_mode(0o755))
        .expect("secure provider directory");
    config::load(&path, uid)
        .await
        .expect("a private provider directory loads");
}

fn host_limits_document(max_total_memory_bytes: Option<u64>) -> serde_json::Value {
    let defaults = dekopon_broker_host::BrokerHostLimits::default();
    let mut limits = json!({
        "maxMemoryBytes": defaults.max_memory_bytes,
        "maxTableElements": defaults.max_table_elements,
        "maxInstances": defaults.max_instances,
        "maxTables": defaults.max_tables,
        "maxMemories": defaults.max_memories,
        "maxInputBytes": defaults.max_input_bytes,
        "maxOutputBytes": defaults.max_output_bytes,
        "maxHttpRequests": defaults.max_http_requests,
        "maxHttpRequestBytes": defaults.max_http_request_bytes,
        "maxHttpResponseBytes": defaults.max_http_response_bytes,
        "maxHttpHeaders": defaults.max_http_headers,
        "maxHttpHeaderBytes": defaults.max_http_header_bytes,
        "fuel": defaults.fuel,
        "maxTimeoutMs": u64::try_from(defaults.max_timeout.as_millis()).unwrap_or(u64::MAX),
    });
    if let Some(maximum) = max_total_memory_bytes {
        limits["maxTotalMemoryBytes"] = json!(maximum);
    }
    limits
}

/// Per-store limits bound one invocation; the connection ceiling decides how many exist at once.
/// Configuration is where that product becomes a stated number rather than an OOM kill.
#[tokio::test]
async fn concurrent_guest_memory_budget_is_resolved_and_validated() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let policies = directory.path().join("policies.cedar");
    fs::write(directory.path().join("echo.wasm"), b"component fixture")
        .expect("write provider path fixture");
    write_owner_only(&policies, POLICIES.as_bytes());

    let mut document = attested_document(uid);
    document["compileCachePath"] = json!("compile-cache");
    document["hostLimits"] = host_limits_document(Some(256 * 1024 * 1024));
    write_config(&path, &document);
    let resolved = config::load(&path, uid)
        .await
        .expect("an aggregate ceiling above one store loads");
    assert_eq!(
        resolved.host_options.compile_cache_dir.as_deref(),
        Some(
            directory
                .path()
                .canonicalize()
                .expect("canonical fixture directory")
                .join("compile-cache")
                .as_path()
        )
    );
    assert_eq!(
        resolved.host_options.max_total_memory_bytes,
        Some(256 * 1024 * 1024)
    );
    assert_eq!(
        resolved.worst_case_guest_memory_bytes,
        resolved.server_limits.max_connections * resolved.host_limits.max_memory_bytes
    );

    // A ceiling below one store could never admit a single invocation.
    document["hostLimits"] = host_limits_document(Some(
        u64::try_from(dekopon_broker_host::BrokerHostLimits::default().max_memory_bytes)
            .expect("default fits u64")
            - 1,
    ));
    write_config(&path, &document);
    let error = config::load(&path, uid)
        .await
        .expect_err("an unusable aggregate ceiling refuses to load");
    assert!(
        matches!(error, config::ConfigError::InvalidHostLimits),
        "{error:?}"
    );
}

/// An empty directory is almost certainly a mount that did not happen or a build that did not run,
/// so it gets its own error rather than the generic "no providers".
#[tokio::test]
async fn an_empty_provider_directory_is_named_in_its_own_error() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let providers = directory.path().join("providers");
    fs::create_dir(&providers).expect("create provider directory");
    fs::set_permissions(&providers, fs::Permissions::from_mode(0o755))
        .expect("secure provider directory");
    write_config(&path, &provider_config(uid, json!(["providers"])));

    let error = config::load(&path, uid)
        .await
        .expect_err("an empty provider directory refuses to load");
    assert!(
        matches!(error, config::ConfigError::EmptyProviderDirectory { .. }),
        "{error:?}"
    );
}

/// Files and directories mix in one list, and a component reachable two ways is still one provider.
#[tokio::test]
async fn file_and_directory_entries_mix_and_still_deduplicate() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let providers = directory.path().join("providers");
    fs::create_dir(&providers).expect("create provider directory");
    fs::set_permissions(&providers, fs::Permissions::from_mode(0o755))
        .expect("secure provider directory");
    fs::write(providers.join("echo.wasm"), b"component fixture").expect("write component fixture");
    fs::write(directory.path().join("solo.wasm"), b"component fixture").expect("write solo");

    write_config(
        &path,
        &provider_config(uid, json!(["solo.wasm", "providers"])),
    );
    let resolved = config::load(&path, uid).await.expect("mixed config loads");
    assert_eq!(resolved.providers.len(), 2);

    // The same component named directly and reached through the directory is one provider, and
    // naming it twice is the configuration mistake `DuplicateProviderPath` exists for.
    write_config(
        &path,
        &provider_config(uid, json!(["providers/echo.wasm", "providers"])),
    );
    let error = config::load(&path, uid)
        .await
        .expect_err("one component reached two ways is a duplicate");
    assert!(
        matches!(error, config::ConfigError::DuplicateProviderPath { .. }),
        "{error:?}"
    );
}

/// The pre-expansion bound limits what the file may say; this one limits what it resolves to.
#[tokio::test]
async fn a_directory_expanding_past_the_provider_ceiling_refuses_to_load() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let providers = directory.path().join("providers");
    fs::create_dir(&providers).expect("create provider directory");
    fs::set_permissions(&providers, fs::Permissions::from_mode(0o755))
        .expect("secure provider directory");
    for index in 0..=config::HARD_MAX_PROVIDERS {
        fs::write(
            providers.join(format!("component-{index:03}.wasm")),
            b"component fixture",
        )
        .expect("write component fixture");
    }

    // One entry in the file, so the pre-expansion check passes and only the post-expansion one
    // can catch this.
    write_config(&path, &provider_config(uid, json!(["providers"])));
    let error = config::load(&path, uid)
        .await
        .expect_err("expanding past the ceiling refuses to load");
    assert!(
        matches!(error, config::ConfigError::TooManyProviders { .. }),
        "{error:?}"
    );
}

#[test]
fn generic_durable_storage_outer_spans_have_no_identity_or_capability_fields() {
    tracing::subscriber::with_default(tracing_subscriber::registry(), || {
        let span = server::storage_invocation_span(
            &"generic-durable-sentinel".parse().expect("invocation"),
            &"generic-durable-trace".parse().expect("trace"),
        );
        let fields = span
            .metadata()
            .expect("storage span metadata")
            .fields()
            .iter()
            .map(|field| field.name())
            .collect::<Vec<_>>();
        assert_eq!(fields, ["invocation", "trace"]);
        for forbidden in ["capability", "provider", "subject", "agent"] {
            assert!(!fields.contains(&forbidden));
        }
    });
}

#[tokio::test]
async fn storage_root_rejects_future_socket_and_broker_file_collisions() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("configuration fixture");
    let path = directory.path().join("broker.yaml");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );
    write_owner_only(&directory.path().join("echo.wasm"), b"component fixture");
    write_owner_only(&directory.path().join("storage-key.yaml"), b"key fixture");
    fs::create_dir(directory.path().join("provider-storage")).expect("storage root");

    let mut document = attested_document(uid);
    document["socketPath"] = json!("provider-storage/broker.sock");
    let mut storage = serde_json::to_value(dekopon_storage_host::StorageLimits::default())
        .expect("storage limits serialize");
    storage.as_object_mut().expect("storage object").extend([
        ("rootPath".to_owned(), json!("provider-storage")),
        ("namespaceKeyPath".to_owned(), json!("storage-key.yaml")),
    ]);
    document["storage"] = storage;
    write_config(&path, &document);
    assert!(matches!(
        config::load(&path, uid).await,
        Err(config::ConfigError::StorageStateCollision)
    ));

    write_owner_only(
        &directory.path().join("provider-storage/inside.wasm"),
        b"component fixture",
    );
    let mut document = attested_document(uid);
    document["providers"] = json!(["provider-storage/inside.wasm"]);
    let mut storage = serde_json::to_value(dekopon_storage_host::StorageLimits::default())
        .expect("storage limits serialize");
    storage.as_object_mut().expect("storage object").extend([
        ("rootPath".to_owned(), json!("provider-storage")),
        ("namespaceKeyPath".to_owned(), json!("storage-key.yaml")),
    ]);
    document["storage"] = storage;
    write_config(&path, &document);
    assert!(matches!(
        config::load(&path, uid).await,
        Err(config::ConfigError::StorageStateCollision)
    ));
}

#[tokio::test]
async fn configured_storage_ancestor_symlinks_are_not_canonicalized_away() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("configuration fixture");
    let path = directory.path().join("broker.yaml");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );
    write_owner_only(&directory.path().join("echo.wasm"), b"component fixture");
    write_owner_only(&directory.path().join("storage-key.yaml"), b"key fixture");
    let actual = directory.path().join("actual-storage-parent");
    fs::create_dir(&actual).expect("actual parent");
    fs::set_permissions(&actual, fs::Permissions::from_mode(0o700)).expect("parent mode");
    std::os::unix::fs::symlink(&actual, directory.path().join("storage-parent"))
        .expect("ancestor symlink");

    let mut document = attested_document(uid);
    let mut storage = serde_json::to_value(dekopon_storage_host::StorageLimits::default())
        .expect("storage limits serialize");
    storage.as_object_mut().expect("storage object").extend([
        (
            "rootPath".to_owned(),
            json!("storage-parent/provider-storage"),
        ),
        ("namespaceKeyPath".to_owned(), json!("storage-key.yaml")),
    ]);
    document["storage"] = storage;
    write_config(&path, &document);
    assert!(matches!(
        config::load(&path, uid).await,
        Err(config::ConfigError::StoragePath { .. })
    ));
    assert!(!actual.join("provider-storage").exists());
}

/// A refused deployment has to name the bound it broke. Both of these sections are validated by
/// another crate that already says which field and which ceiling, and the operator reading a
/// failed start is the only audience for that.
#[tokio::test]
async fn refused_storage_and_frame_bounds_keep_the_field_that_refused_them() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("configuration fixture");
    let path = directory.path().join("broker.yaml");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );
    write_owner_only(&directory.path().join("echo.wasm"), b"component fixture");
    write_owner_only(&directory.path().join("storage-key.yaml"), b"key fixture");
    fs::create_dir(directory.path().join("provider-storage")).expect("storage root");

    let mut document = attested_document(uid);
    let mut storage = serde_json::to_value(dekopon_storage_host::StorageLimits::default())
        .expect("storage limits serialize");
    storage.as_object_mut().expect("storage object").extend([
        ("rootPath".to_owned(), json!("provider-storage")),
        ("namespaceKeyPath".to_owned(), json!("storage-key.yaml")),
        ("maxFileBytes".to_owned(), json!(0)),
    ]);
    document["storage"] = storage;
    write_config(&path, &document);
    let error = config::load(&path, uid)
        .await
        .expect_err("a zero storage ceiling is not a deployable configuration");
    let config::ConfigError::InvalidStorage {
        source: dekopon_storage_host::StorageConfigError::Zero { field },
    } = error
    else {
        panic!("the refused storage field must survive into the configuration error: {error}");
    };
    assert_eq!(field, "maxFileBytes");

    let mut document = attested_document(uid);
    document["serverLimits"] = json!({
        "maxFrameBytes": dekopon_broker_protocol::DEFAULT_MAX_FRAME_BYTES,
        "ioTimeoutMs": 0,
        "maxConnections": config::DEFAULT_MAX_CONNECTIONS,
        "auditMaxRecords": dekopon_broker::DEFAULT_MAX_AUDIT_RECORDS,
        "auditMaxLineBytes": dekopon_broker::DEFAULT_MAX_AUDIT_LINE_BYTES,
        "shutdownGraceMs": 120_000
    });
    write_config(&path, &document);
    let error = config::load(&path, uid)
        .await
        .expect_err("a zero frame I/O timeout is not a deployable configuration");
    assert!(
        matches!(
            error,
            config::ConfigError::InvalidFrameLimits {
                source: dekopon_broker_protocol::ProtocolError::ZeroTimeout
            }
        ),
        "a zero I/O timeout must be distinguishable from an out-of-range frame ceiling: {error}"
    );
}

#[tokio::test]
async fn chat_memory_rejects_a_host_fuel_ceiling_that_cannot_reach_compaction() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("configuration fixture");
    let path = directory.path().join("broker.yaml");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );
    write_owner_only(&directory.path().join("echo.wasm"), b"component fixture");
    write_owner_only(&directory.path().join("storage-key.yaml"), b"key fixture");
    fs::create_dir(directory.path().join("provider-storage")).expect("storage root");

    let mut document = attested_document(uid);
    let mut storage = serde_json::to_value(dekopon_storage_host::StorageLimits::default())
        .expect("storage limits serialize");
    storage.as_object_mut().expect("storage object").extend([
        ("rootPath".to_owned(), json!("provider-storage")),
        ("namespaceKeyPath".to_owned(), json!("storage-key.yaml")),
    ]);
    document["storage"] = storage;
    document["chatMemory"] = serde_json::to_value(dekopon_broker::ChatMemoryConfig {
        continuity_policy: dekopon_storage_host::ContinuityPolicy::AuthorityBound,
        enabled_agents: vec!["reviewer".parse().expect("agent")],
        max_lookback_turns: 200,
        max_recent_turns: 20,
        max_search_results: 20,
        max_query_bytes: 256,
        max_result_bytes: 65_536,
        max_turn_bytes: 32_768,
        max_dedup_records: 16_000,
        max_dedup_bytes: 4_194_304,
        compaction_target_bytes: 8_388_608,
        compaction_threshold_bytes: 12_582_912,
    })
    .expect("memory config serializes");
    let host = dekopon_broker_host::BrokerHostLimits::default();
    document["hostLimits"] = json!({
        "maxMemoryBytes": host.max_memory_bytes,
        "maxTableElements": host.max_table_elements,
        "maxInstances": host.max_instances,
        "maxTables": host.max_tables,
        "maxMemories": host.max_memories,
        "maxInputBytes": host.max_input_bytes,
        "maxOutputBytes": host.max_output_bytes,
        "maxHttpRequests": host.max_http_requests,
        "maxHttpRequestBytes": host.max_http_request_bytes,
        "maxHttpResponseBytes": host.max_http_response_bytes,
        "maxHttpHeaders": host.max_http_headers,
        "maxHttpHeaderBytes": host.max_http_header_bytes,
        "fuel": host.fuel,
        "maxTimeoutMs": u64::try_from(host.max_timeout.as_millis()).expect("timeout")
    });
    write_config(&path, &document);
    config::load(&path, uid)
        .await
        .expect("documented defaults compose before component loading");

    document["hostLimits"]["fuel"] = json!(10_000_000);
    write_config(&path, &document);
    assert!(matches!(
        config::load(&path, uid).await,
        Err(config::ConfigError::InvalidChatMemory)
    ));
}

#[test]
fn storage_section_is_optional_all_or_nothing_and_strict() {
    let uid = current_uid();
    let mut document = attested_document(uid);
    assert!(serde_json::from_value::<config::BrokerdConfig>(document.clone()).is_ok());

    let mut storage = serde_json::to_value(dekopon_storage_host::StorageLimits::default())
        .expect("storage limits serialize");
    let object = storage.as_object_mut().expect("limits object");
    object.insert(
        "rootPath".to_owned(),
        json!("/var/lib/dekopon-provider-storage"),
    );
    object.insert(
        "namespaceKeyPath".to_owned(),
        json!("/etc/dekopon-storage-key/storage-key.yaml"),
    );
    document["storage"] = storage.clone();
    let decoded = serde_json::from_value::<config::BrokerdConfig>(document.clone())
        .expect("complete strict storage section decodes");
    assert_eq!(
        decoded.storage.expect("storage").limits.max_root_bytes,
        2 * 1024 * 1024 * 1024
    );

    document["storage"]
        .as_object_mut()
        .expect("storage object")
        .remove("maxRootBytes");
    assert!(
        serde_json::from_value::<config::BrokerdConfig>(document).is_err(),
        "presence requires every storage field"
    );
}

/// One transient `accept` failure used to end the privileged daemon, and ending it is the most
/// expensive answer available: the container restarts, every provider recompiles under Cranelift
/// before the socket rebinds, and durable audit state waits through all of it. Descriptor
/// exhaustion — which the unauthenticated `--http-bind` listener can cause on its own — is not a
/// broken listener.
#[test]
fn transient_accept_failures_are_survivable_and_the_rest_are_not() {
    for (errno, kind) in [
        (libc::EMFILE, "process-descriptor-limit"),
        (libc::ENFILE, "system-descriptor-limit"),
        (libc::ENOBUFS, "kernel-memory"),
        (libc::ENOMEM, "kernel-memory"),
        (libc::ECONNABORTED, "connection-aborted"),
        (libc::ECONNRESET, "connection-reset"),
        (libc::EINTR, "interrupted"),
    ] {
        assert_eq!(
            server::retryable_accept_error(&std::io::Error::from_raw_os_error(errno)),
            Some(kind),
            "errno {errno} must not exit the daemon"
        );
    }

    // A listener that is gone, unbound, or not a socket is a real fault: retrying it forever would
    // turn a startup mistake into a silent hang.
    for errno in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK, libc::EOPNOTSUPP] {
        assert_eq!(
            server::retryable_accept_error(&std::io::Error::from_raw_os_error(errno)),
            None,
            "errno {errno} must stay fatal"
        );
    }
    // Not every `io::Error` carries an errno.
    assert_eq!(
        server::retryable_accept_error(&std::io::Error::other("no errno")),
        None
    );
}

/// The shutdown budget is one grace, not one per listener. Both listeners have already stopped
/// accepting when this runs, so a broker drain that spends the whole grace must not then hand the
/// storage GC and the web UI a fresh full grace each — that is how a 120 s `shutdownGraceMs`
/// became a 360 s exit against a 180 s `terminationGracePeriodSeconds`.
#[tokio::test(start_paused = true)]
async fn every_drain_shares_one_grace() {
    let grace = std::time::Duration::from_secs(120);
    let started = tokio::time::Instant::now();
    let report = super::drain_services(
        started + grace,
        async {
            tokio::time::sleep(grace).await;
            Ok(())
        },
        tokio::time::sleep(grace * 3 / 4),
        Some(async {
            tokio::time::sleep(grace * 3 / 4).await;
            Ok(())
        }),
    )
    .await;

    let elapsed = started.elapsed();
    assert!(
        elapsed < grace * 2,
        "three drains that each fit one grace must not take three: {elapsed:?}"
    );
    assert!(report.broker.is_ok());
    assert!(!report.storage_gc_timed_out);
    assert!(!report.web_timed_out);
    assert!(matches!(report.web, Some(Ok(()))));
}

/// And the deadline is shared rather than restarted, so a drain that outlives it is reported
/// instead of being given the grace over again.
#[tokio::test(start_paused = true)]
async fn a_drain_past_the_shared_deadline_times_out() {
    let grace = std::time::Duration::from_secs(120);
    let started = tokio::time::Instant::now();
    let report = super::drain_services(
        started + grace,
        async {
            tokio::time::sleep(grace / 2).await;
            Ok(())
        },
        tokio::time::sleep(grace * 4),
        Some(async {
            tokio::time::sleep(grace * 4).await;
            Ok(())
        }),
    )
    .await;

    let elapsed = started.elapsed();
    assert!(elapsed < grace * 2, "{elapsed:?}");
    assert!(report.storage_gc_timed_out);
    assert!(report.web_timed_out);
    assert!(report.web.is_none());
}

/// The startup frame check exists so an oversized capability response fails here rather than on
/// the first session. It used to measure only the direct peers, and in the deployment it is written
/// for the direct peer is the gateway — granted almost nothing. The capability sets that actually
/// reach the wire belong to the attested principals the identity mappings name, on the
/// `capabilitiesFor` path the check skipped, so the oversized response passed startup and then
/// failed `write_frame` on every session open.
#[tokio::test]
async fn the_startup_frame_check_covers_more_than_the_direct_peers() {
    use std::sync::Arc;

    use dekopon_broker::{
        AuthenticatedContext, Broker, BrokerLimits, ConstraintCatalog, CredentialStore,
        IdentityDirectory, InMemoryAuditLog, PolicyEngine, PolicyWorld,
    };
    use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
    use dekopon_broker_protocol::ResponseEnvelope;
    use dekopon_core::{Actor, AgentId, CapabilityId, PrincipalId};

    use super::{BrokerdError, MappedPeer, validate_capability_responses};

    let echo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/providers/echo-provider.wasm");
    let registry = BrokerProviderRegistry::load([echo], BrokerHostLimits::default())
        .await
        .expect("load echo fixture");
    let capability = "echo.echo"
        .parse::<CapabilityId>()
        .expect("valid capability fixture");
    let world = PolicyWorld::new(
        ["gateway", "cpetersen"].map(|name| name.parse::<PrincipalId>().expect("valid principal")),
        [(
            capability.clone(),
            "echo".parse().expect("valid provider fixture"),
        )],
    )
    .expect("declared world builds");
    let catalog = ConstraintCatalog::new([(
        capability,
        serde_json::from_value(constraint_set()).expect("constraint set decodes"),
    )])
    .expect("one capability builds a catalog");
    let broker = Broker::new(
        registry,
        "broker-test".parse().expect("valid broker principal"),
        "policy-test".to_owned(),
        PolicyEngine::new(POLICIES, &world).expect("fixture policy validates"),
        catalog,
        CredentialStore::empty(),
        IdentityDirectory::new([(
            "slack.t0123abc.u9xyz".parse().expect("canonical subject"),
            "cpetersen".parse::<PrincipalId>().expect("valid principal"),
        )])
        .expect("one mapping builds a directory"),
        Arc::new(InMemoryAuditLog::new(8).expect("valid audit bound")),
        BrokerLimits::default(),
    )
    .expect("broker starts");

    // The gateway peer itself: it may attest for others and holds no capability of its own, so its
    // own answer is empty and fits anything.
    let gateway = AuthenticatedContext::new(
        "gateway".parse().expect("valid principal"),
        Actor::Service {
            principal: "gateway".parse().expect("valid principal"),
        },
    )
    .expect("trusted context binds");
    let mut identities = BTreeMap::new();
    identities.insert(
        current_uid(),
        MappedPeer {
            context: gateway.clone(),
            attestor: None,
        },
    );
    let peer_bytes = serde_json::to_vec(&ResponseEnvelope::capabilities(
        broker.capabilities(&gateway),
        broker.command_words(&gateway),
    ))
    .expect("peer response encodes")
    .len();

    // A session's answer under `chat-agent` carries the real capability, so it is strictly larger.
    let (capabilities, words) = broker.capability_ceiling();
    assert!(
        !capabilities.is_empty(),
        "the ceiling must see what policy grants an attested principal"
    );
    assert!(
        broker
            .capabilities_for(
                &gateway,
                Some(&dekopon_broker::AttestorGrant {
                    namespaces: vec!["slack.t0123abc".to_owned()],
                    chat_scopes: Vec::new(),
                }),
                &"slack.t0123abc.u9xyz".parse().expect("canonical subject"),
                &"chat-agent".parse::<AgentId>().expect("valid agent"),
            )
            .expect("the mapped subject is attestable")
            .0
            .len()
            <= capabilities.len(),
        "the ceiling must bound what a real session receives"
    );

    let ceiling_bytes = serde_json::to_vec(&ResponseEnvelope::chat_capabilities(
        capabilities,
        words,
        broker.chat_memory_ceiling(),
    ))
    .expect("ceiling response encodes")
    .len();
    assert!(ceiling_bytes > peer_bytes);

    // A frame that fits every direct peer and nothing else used to pass startup.
    let error = validate_capability_responses(&broker, &identities, peer_bytes)
        .expect_err("a frame that cannot carry a session's answer must refuse to start");
    assert!(
        matches!(error, BrokerdError::CapabilityCeilingTooLarge { length, maximum }
            if length == ceiling_bytes && maximum == peer_bytes),
        "{error}"
    );
    validate_capability_responses(&broker, &identities, ceiling_bytes)
        .expect("a frame that carries the widest answer starts");
}

/// A `dev.*` subject is the one kind no external service authenticated, so a broker admits it only
/// when an operator said so — and says every place the configuration assumed otherwise, at once.
#[tokio::test]
async fn development_subjects_need_an_explicit_opt_in() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    fs::write(directory.path().join("echo.wasm"), b"component fixture")
        .expect("write provider path fixture");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );

    let mut document = attested_document(uid);
    document["identities"][1]["attestor"]["namespaces"] = json!(["slack.t0123abc", "dev.console"]);
    document["identityMappings"] = json!([
        {"subject": "slack.t0123abc.u9xyz", "principal": "cpetersen"},
        {"subject": "dev.console.xavier", "principal": "xavier-console"}
    ]);

    write_config(&path, &document);
    let error = config::load(&path, uid)
        .await
        .expect_err("development identities need the opt-in");
    let config::ConfigError::DevelopmentSubjectsNotAllowed { entries } = &error else {
        panic!("expected the development-subject refusal, got {error:?}");
    };
    // Both, in one report: an operator who fixed the mapping and restarted only to be told about
    // the namespace would read the check as arbitrary rather than their configuration.
    assert_eq!(
        entries.len(),
        2,
        "every offending entry must be named: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.contains("dev.console.xavier")),
        "{entries:?}"
    );
    assert!(
        entries.iter().any(|entry| entry.contains("dev.console")),
        "{entries:?}"
    );
    let message = error.to_string();
    assert!(message.contains("allowDevelopmentSubjects"), "{message}");

    // With the opt-in, the same configuration resolves and both mappings survive.
    document["allowDevelopmentSubjects"] = json!(true);
    write_config(&path, &document);
    let resolved = config::load(&path, uid)
        .await
        .expect("the opt-in admits development identities");
    assert_eq!(resolved.identity_mappings.len(), 2);
}

/// A namespace that merely starts with the same letters is an ordinary namespace.
#[tokio::test]
async fn a_namespace_is_matched_on_segment_boundaries_not_letters() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    fs::write(directory.path().join("echo.wasm"), b"component fixture")
        .expect("write provider path fixture");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );

    let mut document = attested_document(uid);
    document["identities"][1]["attestor"]["namespaces"] = json!(["slack.t0123abc"]);
    write_config(&path, &document);
    assert!(
        config::load(&path, uid).await.is_ok(),
        "an ordinary deployment must not need the development opt-in"
    );
}
