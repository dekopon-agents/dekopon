use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _, path::Path};

use dekopon_broker::{CapabilityRoute, ConstraintSet};
use dekopon_capability::{EffectKind, ExecutionConstraints};
use dekopon_core::{ProviderId, RiskLevel};
use serde_json::json;
use sha2::{Digest as _, Sha256};

use super::{config, current_uid, socket};

const POLICIES: &str = r#"
@id("chat-agent-session")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"chat-agent")
when { context.via == "gateway" };

@id("chat-agent-upper")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"cli-probe.upper",
       resource == Dekopon::Provider::"cli-probe")
when { context.via == "gateway"
    && context.agent == "chat-agent" };
"#;

fn probe_capabilities() -> serde_json::Value {
    json!({
        "cli-probe": {
            "constraints": {"timeoutMs": 30_000, "maxOutputBytes": 1_048_576},
            "capabilities": {"cli-probe.upper": {}}
        }
    })
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

fn attested_document(uid: u32) -> serde_json::Value {
    json!({
        "apiVersion": config::CONFIG_API_VERSION,
        "socketPath": "broker.sock",
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "policiesPath": "policies.cedar",
        "providers": ["cli-probe.wasm"],
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
            },
            {
                "uid": uid + 2,
                "principal": "console",
                "actor": {"type": "service", "principal": "console"}
            }
        ],
        "principals": {
            "cpetersen": {"subjects": ["slack.t0123abc.u9xyz"]}
        },
        "capabilities": probe_capabilities()
    })
}

#[tokio::test]
async fn policy_and_constraint_configuration_is_resolved_and_owner_only() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let policies = directory.path().join("policies.cedar");
    fs::write(
        directory.path().join("cli-probe.wasm"),
        b"component fixture",
    )
    .expect("write provider path fixture");
    write_owner_only(&policies, POLICIES.as_bytes());

    let document = attested_document(uid);
    write_config(&path, &document);
    let resolved = config::load(&path, uid)
        .await
        .expect("a complete attested path resolves");
    assert_eq!(resolved.principals.len(), 1);
    assert_eq!(
        resolved.principals[&"cpetersen".parse().expect("principal")].subjects[0].canonical(),
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
    assert_eq!(
        resolved.capabilities[&"cli-probe".parse::<ProviderId>().expect("provider")]
            .capabilities
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["cli-probe.upper"]
    );

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

    write_config(&path, &document);
    fs::set_permissions(&policies, fs::Permissions::from_mode(0o666))
        .expect("loosen policy fixture");
    let error = config::load(&path, uid)
        .await
        .expect_err("a group/world-writable policy file must fail closed");
    assert!(matches!(error, config::ConfigError::InsecureFile { .. }));
    fs::set_permissions(&policies, fs::Permissions::from_mode(0o600))
        .expect("restore policy fixture");

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

#[tokio::test]
async fn managed_provider_configuration_is_strict_and_network_free() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create managed-provider fixture");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private fixture directory");
    let path = directory.path().join("broker.yaml");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );

    let component = fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("examples/providers/cli-probe-provider.wasm"),
    )
    .expect("checked cli-probe component");
    let digest = Sha256::digest(&component)
        .iter()
        .fold(String::new(), |mut text, byte| {
            use std::fmt::Write as _;
            write!(&mut text, "{byte:02x}").expect("writing to a String cannot fail");
            text
        });
    let store = directory.path().join("store");
    let blobs = store.join("blobs");
    let sha = blobs.join("sha256");
    for directory in [&store, &blobs, &sha] {
        fs::create_dir(directory).expect("create store directory");
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .expect("private store directory");
    }
    let blob = sha.join(format!("{digest}.wasm"));
    write_owner_only(&blob, &component);
    let lock = format!(
        "apiVersion: dekopon.dev/provider-lock/v1alpha1\nproviders:\n  - source: ghcr.io/example/cli-probe:1.0.0\n    resolvedVersion: 1.0.0\n    manifestDigest: sha256:{}\n    componentDigest: sha256:{digest}\n    componentBytes: {}\n    providerId: cli-probe\n",
        "1".repeat(64),
        component.len()
    );
    write_owner_only(
        &directory.path().join("providers.lock.yaml"),
        lock.as_bytes(),
    );

    let mut document = attested_document(uid);
    document
        .as_object_mut()
        .expect("config object")
        .remove("providers");
    document["providerSet"] = json!({
        "lockPath": "providers.lock.yaml",
        "storePath": "store"
    });
    write_config(&path, &document);
    let resolved = config::load(&path, uid)
        .await
        .expect("managed provider lock resolves locally");
    assert_eq!(
        resolved.providers,
        vec![fs::canonicalize(&blob).expect("canonical blob")]
    );
    assert_eq!(resolved.locked_providers.as_ref().map(Vec::len), Some(1));
    assert_eq!(
        resolved.host_options.cwasm_dir,
        Some(store.canonicalize().expect("canonical store").join("cwasm"))
    );

    let mut runnable = document.clone();
    runnable["identities"] = json!([document["identities"][0].clone()]);
    write_config(&path, &runnable);
    super::run(&path, async {})
        .await
        .expect("daemon populates the default mapped cache before binding");
    super::run(&path, async {})
        .await
        .expect("warm daemon loads mapped artifacts");

    runnable["compileOnLoad"] = json!(true);
    write_config(&path, &runnable);
    let bypass = config::load(&path, uid)
        .await
        .expect("explicit bypass config");
    assert_eq!(bypass.host_options.cwasm_dir, None);

    let mut retired = document.clone();
    retired["compileCachePath"] = json!("old-cache");
    write_config(&path, &retired);
    let error = config::load(&path, uid)
        .await
        .expect_err("retired cache setting is not ignored");
    assert!(
        format!("{error:?}").contains("compileCachePath"),
        "{error:?}"
    );

    let mut mixed = document.clone();
    mixed["providers"] = json!(["cli-probe.wasm"]);
    write_config(&path, &mixed);
    let error = config::load(&path, uid)
        .await
        .expect_err("legacy and managed provider sources are mutually exclusive");
    assert!(matches!(error, config::ConfigError::MixedProviderSources));

    write_config(&path, &document);
    let second = directory.path().join("blob-hard-link.wasm");
    fs::hard_link(&blob, &second).expect("hard-link blob");
    let error = config::load(&path, uid)
        .await
        .expect_err("a locked blob with another link is not trusted startup input");
    assert!(matches!(error, config::ConfigError::ProviderLock { .. }));
}

#[tokio::test]
async fn every_peer_uid_a_private_socket_parent_excludes_is_named_at_startup() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private fixture directory");
    let path = directory.path().join("broker.yaml");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );
    let component = fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("examples/providers/cli-probe-provider.wasm"),
    )
    .expect("checked cli-probe component");
    write_owner_only(&directory.path().join("cli-probe.wasm"), &component);
    let socket_parent = directory.path().join("run");
    fs::create_dir(&socket_parent).expect("create socket parent");
    fs::set_permissions(&socket_parent, fs::Permissions::from_mode(0o700))
        .expect("private socket parent");

    let mut document = attested_document(uid);
    document["socketPath"] = json!("run/broker.sock");
    write_config(&path, &document);
    let error = super::run(&path, async {})
        .await
        .expect_err("an owner-only socket cannot admit the gateway or the console peer");
    let super::BrokerdError::UnreachablePeerUids { configured, server } = &error else {
        panic!("the refusal must name the peers it is about: {error}");
    };
    assert_eq!(*configured, vec![uid + 1, uid + 2]);
    assert_eq!(*server, uid);
    let message = error.to_string();
    for named in [uid + 1, uid + 2, uid] {
        assert!(
            message.contains(&named.to_string()),
            "UID {named} is missing from the refusal: {message}"
        );
    }

    fs::set_permissions(&socket_parent, fs::Permissions::from_mode(0o710))
        .expect("IPC socket parent");
    super::run(&path, async {})
        .await
        .expect("a group-traversable socket parent admits every configured peer");
}

#[tokio::test]
async fn attestor_grants_and_subject_mappings_are_strictly_validated() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    fs::write(
        directory.path().join("cli-probe.wasm"),
        b"component fixture",
    )
    .expect("write provider path fixture");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );
    let document = attested_document(uid);

    let mut duplicated = document.clone();
    duplicated["principals"] = json!({
        "cpetersen": {"subjects": ["slack.t0123abc.u9xyz", "discord.1"]},
        "someone-else": {"subjects": ["slack.t0123abc.u9xyz", "discord.1"]}
    });
    write_config(&path, &duplicated);
    let error = config::load(&path, uid)
        .await
        .expect_err("one subject must not name two principals");
    assert!(matches!(
        error,
        config::ConfigError::DuplicateSubjects { subjects }
            if subjects == ["discord.1", "slack.t0123abc.u9xyz"]
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
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "providers": ["cli-probe.wasm"],
        "identities": [{
            "uid": uid,
            "principal": "caller",
            "actor": {"type": "agent", "agent": "brokerd-test"}
        }],
        "policiesPath": "policies.cedar",
        "capabilities": probe_capabilities()
    });
    write_config(&path, &document);
    fs::write(
        directory.path().join("cli-probe.wasm"),
        b"component fixture",
    )
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
    assert_eq!(
        resolved.providers,
        [canonical_directory.join("cli-probe.wasm")]
    );

    let mut conflicting = document.clone();
    conflicting["policiesPath"] = json!("broker.yaml");
    write_config(&path, &conflicting);
    assert!(matches!(
        config::load(&path, uid).await,
        Err(config::ConfigError::ConflictingPaths)
    ));

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
async fn plaintext_hosts_are_validated_at_startup() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let document = json!({
        "apiVersion": config::CONFIG_API_VERSION,
        "socketPath": "broker.sock",
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "providers": ["cli-probe.wasm"],
        "identities": [{
            "uid": uid,
            "principal": "caller",
            "actor": {"type": "agent", "agent": "brokerd-test"}
        }],
        "policiesPath": "policies.cedar",
        "capabilities": probe_capabilities()
    });
    fs::write(
        directory.path().join("cli-probe.wasm"),
        b"component fixture",
    )
    .expect("write provider path fixture");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );

    write_config(&path, &document);
    let resolved = config::load(&path, uid)
        .await
        .expect("a config without an http section loads");
    assert!(resolved.plaintext_hosts.is_empty());
    assert!(resolved.host_options.plaintext_hosts.is_empty());

    let mut allowed = document.clone();
    allowed["http"] = json!({"plaintextHosts": ["RPi.LAN", "openobserve.openobserve.svc"]});
    write_config(&path, &allowed);
    let resolved = config::load(&path, uid)
        .await
        .expect("a config naming plaintext hosts loads");
    assert_eq!(
        resolved.plaintext_hosts.iter().collect::<Vec<_>>(),
        ["openobserve.openobserve.svc", "rpi.lan"]
    );
    assert!(resolved.host_options.plaintext_hosts.contains("rpi.lan"));

    let ca_file = directory.path().join("root.pem");
    fs::write(
        &ca_file,
        include_bytes!("../../dekopon-http-host/tests/fixtures/private-root.pem"),
    )
    .expect("write public test root");
    let mut private = document.clone();
    private["http"] = json!({
        "extraCABundles": [ca_file],
        "nonPublicHttps": ["openobserve-tls.openobserve.svc.cluster.local:5443"]
    });
    write_config(&path, &private);
    let resolved = config::load(&path, uid)
        .await
        .expect("valid independent HTTPS trust and egress settings");
    assert_eq!(resolved.host_options.extra_ca_bundles.len(), 1);
    assert_eq!(resolved.host_options.non_public_https.len(), 1);
    private["providerSettings"] = json!({"openobserve": {
        "url": "https://openobserve-tls.openobserve.svc.cluster.local:5443/openobserve",
        "org": "default", "stream": "dekopon"
    }});
    write_config(&path, &private);
    let resolved = config::load(&path, uid)
        .await
        .expect("owner provider settings");
    assert_eq!(resolved.host_options.provider_settings.len(), 1);
    assert!(
        resolved
            .host_options
            .provider_settings
            .values()
            .next()
            .unwrap()
            .contains("dekopon")
    );
    private["providerSettings"]["openobserve"] = json!("invalid scalar");
    write_config(&path, &private);
    assert!(matches!(
        config::load(&path, uid).await,
        Err(config::ConfigError::InvalidProviderSettings)
    ));
    private["providerSettings"]["openobserve"] = json!({"url": "https://openobserve-tls.openobserve.svc.cluster.local:5443/openobserve", "org": "default", "stream": "dekopon"});
    private["http"]["nonPublicHttps"][0] =
        json!("openobserve-tls.openobserve.svc.cluster.local:5443/path");
    write_config(&path, &private);
    assert!(matches!(
        config::load(&path, uid).await,
        Err(config::ConfigError::InvalidHttpsConfiguration)
    ));
    private["http"]["nonPublicHttps"][0] =
        json!("openobserve-tls.openobserve.svc.cluster.local:5443");
    fs::write(&ca_file, b"not pem").expect("replace public test root");
    write_config(&path, &private);
    assert!(matches!(
        config::load(&path, uid).await,
        Err(config::ConfigError::InvalidHttpsConfiguration)
    ));

    for entry in [
        "http://rpi.lan",
        "rpi.lan:5080",
        "rpi.lan/ingest",
        "*.lan",
        "",
    ] {
        let mut invalid = document.clone();
        invalid["http"] = json!({"plaintextHosts": [entry]});
        write_config(&path, &invalid);
        let error = config::load(&path, uid)
            .await
            .expect_err("an entry that is not a bare hostname must refuse startup");
        assert!(
            matches!(error, config::ConfigError::InvalidPlaintextHost { .. }),
            "{entry}: {error}"
        );
        assert!(
            error.to_string().contains("http.plaintextHosts"),
            "the refusal names the field: {error}"
        );
    }

    let mut typo = document;
    typo["http"] = json!({"plainTextHosts": ["rpi.lan"]});
    write_config(&path, &typo);
    let error = config::load(&path, uid)
        .await
        .expect_err("a misspelled field inside http is unknown");
    assert!(
        matches!(&error, config::ConfigError::Decode { source }
            if source.to_string().contains("unknown field")
                && source.to_string().contains("plainTextHosts")),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn an_audit_path_in_config_is_refused() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    fs::write(
        directory.path().join("cli-probe.wasm"),
        b"component fixture",
    )
    .expect("write provider path fixture");
    let mut with_path = provider_config(uid, json!(["cli-probe.wasm"]));
    with_path["auditPath"] = json!("audit.jsonl");
    let mut with_line_bound = provider_config(uid, json!(["cli-probe.wasm"]));
    with_line_bound["serverLimits"] = json!({
        "maxFrameBytes": dekopon_broker_protocol::DEFAULT_MAX_FRAME_BYTES,
        "ioTimeoutMs": 30_000,
        "maxConnections": config::DEFAULT_MAX_CONNECTIONS,
        "auditMaxLineBytes": 65_536,
        "shutdownGraceMs": 120_000
    });
    for (document, field) in [
        (with_path, "auditPath"),
        (with_line_bound, "auditMaxLineBytes"),
    ] {
        write_config(&path, &document);
        let error = config::load(&path, uid)
            .await
            .expect_err("an audit sink field is unknown");
        assert!(
            matches!(&error, config::ConfigError::Decode { source }
                if source.to_string().contains("unknown field")
                    && source.to_string().contains(field)),
            "{field}: {error}"
        );
    }
}

#[tokio::test]
async fn telemetry_section_is_optional_and_strict() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let base = json!({
        "apiVersion": config::CONFIG_API_VERSION,
        "socketPath": "broker.sock",
        "brokerPrincipal": "broker-test",
        "policyRevision": "policy-test",
        "providers": ["cli-probe.wasm"],
        "identities": [{
            "uid": uid,
            "principal": "caller",
            "actor": {"type": "agent", "agent": "brokerd-test"}
        }],
        "policiesPath": "policies.cedar",
        "capabilities": probe_capabilities()
    });
    fs::write(
        directory.path().join("cli-probe.wasm"),
        b"component fixture",
    )
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
        "exportTimeoutMs": 5000
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
    for broken in [
        json!({"endpoint": "http://rpi.localdomain", "transport": "grpc"}),
        json!({
            "endpoint": "http://rpi.localdomain",
            "transport": "thrift",
            "serviceName": "dekopon-brokerd",
            "exportTimeoutMs": 5000
        }),
        json!({
            "endpoint": "http://rpi.localdomain",
            "transport": "http",
            "serviceName": "dekopon-brokerd",
            "exportTimeoutMs": 0
        }),
        json!({
            "endpoint": "  ",
            "transport": "http",
            "serviceName": "dekopon-brokerd",
            "exportTimeoutMs": 5000
        }),
        json!({
            "endpoint": "http://rpi.localdomain",
            "transport": "grpc",
            "serviceName": "dekopon-brokerd",
            "exportTimeoutMs": 5000,
            "telemetryPayloads": false
        }),
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

fn provider_config(uid: u32, providers: serde_json::Value) -> serde_json::Value {
    json!({
        "apiVersion": config::CONFIG_API_VERSION,
        "socketPath": "broker.sock",
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

#[tokio::test]
async fn a_provider_directory_expands_in_filename_order() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let providers = directory.path().join("providers");
    fs::create_dir(&providers).expect("create provider directory");
    fs::set_permissions(&providers, fs::Permissions::from_mode(0o755))
        .expect("secure provider directory");

    for name in ["middle.wasm", "alpha.wasm", "zulu.wasm"] {
        fs::write(providers.join(name), b"component fixture").expect("write component fixture");
    }
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

#[tokio::test]
async fn a_group_writable_provider_directory_refuses_to_load() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let providers = directory.path().join("providers");
    fs::create_dir(&providers).expect("create provider directory");
    fs::write(providers.join("cli-probe.wasm"), b"component fixture")
        .expect("write component fixture");
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

#[tokio::test]
async fn the_aggregate_memory_ceiling_defaults_to_256_mib() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let policies = directory.path().join("policies.cedar");
    fs::write(
        directory.path().join("cli-probe.wasm"),
        b"component fixture",
    )
    .expect("write provider path fixture");
    write_owner_only(&policies, POLICIES.as_bytes());

    let mut document = attested_document(uid);
    write_config(&path, &document);
    let resolved = config::load(&path, uid)
        .await
        .expect("an absent host-limits block loads");
    assert_eq!(
        resolved.host_options.max_total_memory_bytes,
        Some(dekopon_broker_host::DEFAULT_MAX_TOTAL_MEMORY_BYTES)
    );
    assert_eq!(
        dekopon_broker_host::DEFAULT_MAX_TOTAL_MEMORY_BYTES,
        256 * 1024 * 1024,
        "the documented default and the constant are one fact"
    );

    document["hostLimits"] = host_limits_document(None);
    write_config(&path, &document);
    let resolved = config::load(&path, uid)
        .await
        .expect("a complete block without the aggregate ceiling loads");
    assert_eq!(
        resolved.host_options.max_total_memory_bytes,
        Some(dekopon_broker_host::DEFAULT_MAX_TOTAL_MEMORY_BYTES)
    );

    document["hostLimits"] = json!({ "maxTotalMemoryBytes": serde_json::Value::Null });
    write_config(&path, &document);
    let resolved = config::load(&path, uid)
        .await
        .expect("an explicitly null aggregate ceiling loads");
    assert_eq!(resolved.host_options.max_total_memory_bytes, None);
}

#[tokio::test]
async fn concurrent_guest_memory_budget_is_resolved_and_validated() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let policies = directory.path().join("policies.cedar");
    fs::write(
        directory.path().join("cli-probe.wasm"),
        b"component fixture",
    )
    .expect("write provider path fixture");
    write_owner_only(&policies, POLICIES.as_bytes());

    let mut document = attested_document(uid);
    document["compileOnLoad"] = json!(true);
    document["hostLimits"] = host_limits_document(Some(256 * 1024 * 1024));
    write_config(&path, &document);
    let resolved = config::load(&path, uid)
        .await
        .expect("an aggregate ceiling above one store loads");
    assert_eq!(resolved.host_options.cwasm_dir, None);
    assert_eq!(
        resolved.host_options.max_total_memory_bytes,
        Some(256 * 1024 * 1024)
    );
    assert_eq!(
        resolved.worst_case_guest_memory_bytes,
        resolved.server_limits.max_connections * resolved.host_limits.max_memory_bytes
    );

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

#[tokio::test]
async fn a_partial_limits_block_takes_the_absent_block_defaults_and_is_still_validated() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let policies = directory.path().join("policies.cedar");
    fs::write(
        directory.path().join("cli-probe.wasm"),
        b"component fixture",
    )
    .expect("write provider path fixture");
    write_owner_only(&policies, POLICIES.as_bytes());

    let defaults = dekopon_broker_host::BrokerHostLimits::default();
    let mut document = attested_document(uid);
    document["hostLimits"] = json!({"maxTotalMemoryBytes": 256 * 1024 * 1024});
    document["brokerLimits"] = json!({"maxConstraintSets": 2_048});
    write_config(&path, &document);
    let resolved = config::load(&path, uid)
        .await
        .expect("a partial limits block loads on the absent-block defaults");
    assert_eq!(resolved.host_limits, defaults);
    assert_eq!(
        resolved.host_options.max_total_memory_bytes,
        Some(256 * 1024 * 1024)
    );
    assert_eq!(resolved.broker_limits.max_constraint_sets, 2_048);

    document["hostLimits"] = json!({"maxTotalMemoryBytes": defaults.max_memory_bytes - 1});
    write_config(&path, &document);
    let error = config::load(&path, uid)
        .await
        .expect_err("an aggregate ceiling below the defaulted per-store ceiling refuses");
    assert!(
        matches!(error, config::ConfigError::InvalidHostLimits),
        "{error:?}"
    );

    document["hostLimits"] = json!({"maxTotalMemoryBytes": 256 * 1024 * 1024, "typo": 1});
    write_config(&path, &document);
    let error = config::load(&path, uid)
        .await
        .expect_err("an unknown host-limit field is still refused");
    assert!(
        matches!(error, config::ConfigError::Decode { .. }),
        "{error:?}"
    );
}

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

#[tokio::test]
async fn file_and_directory_entries_mix_and_still_deduplicate() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("create configuration fixture");
    let path = directory.path().join("broker.yaml");
    let providers = directory.path().join("providers");
    fs::create_dir(&providers).expect("create provider directory");
    fs::set_permissions(&providers, fs::Permissions::from_mode(0o755))
        .expect("secure provider directory");
    fs::write(providers.join("cli-probe.wasm"), b"component fixture")
        .expect("write component fixture");
    fs::write(directory.path().join("solo.wasm"), b"component fixture").expect("write solo");

    write_config(
        &path,
        &provider_config(uid, json!(["solo.wasm", "providers"])),
    );
    let resolved = config::load(&path, uid).await.expect("mixed config loads");
    assert_eq!(resolved.providers.len(), 2);

    write_config(
        &path,
        &provider_config(uid, json!(["providers/cli-probe.wasm", "providers"])),
    );
    let error = config::load(&path, uid)
        .await
        .expect_err("one component reached two ways is a duplicate");
    assert!(
        matches!(error, config::ConfigError::DuplicateProviderPath { .. }),
        "{error:?}"
    );
}

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

    write_config(&path, &provider_config(uid, json!(["providers"])));
    let error = config::load(&path, uid)
        .await
        .expect_err("expanding past the ceiling refuses to load");
    assert!(
        matches!(error, config::ConfigError::TooManyProviders { .. }),
        "{error:?}"
    );
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
    write_owner_only(
        &directory.path().join("cli-probe.wasm"),
        b"component fixture",
    );
    fs::create_dir(directory.path().join("provider-storage")).expect("storage root");

    let mut document = attested_document(uid);
    document["socketPath"] = json!("provider-storage/broker.sock");
    let mut storage = serde_json::to_value(dekopon_storage_host::StorageLimits::default())
        .expect("storage limits serialize");
    storage
        .as_object_mut()
        .expect("storage object")
        .extend([("rootPath".to_owned(), json!("provider-storage"))]);
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
    storage
        .as_object_mut()
        .expect("storage object")
        .extend([("rootPath".to_owned(), json!("provider-storage"))]);
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
    write_owner_only(
        &directory.path().join("cli-probe.wasm"),
        b"component fixture",
    );
    let actual = directory.path().join("actual-storage-parent");
    fs::create_dir(&actual).expect("actual parent");
    fs::set_permissions(&actual, fs::Permissions::from_mode(0o700)).expect("parent mode");
    std::os::unix::fs::symlink(&actual, directory.path().join("storage-parent"))
        .expect("ancestor symlink");

    let mut document = attested_document(uid);
    let mut storage = serde_json::to_value(dekopon_storage_host::StorageLimits::default())
        .expect("storage limits serialize");
    storage.as_object_mut().expect("storage object").extend([(
        "rootPath".to_owned(),
        json!("storage-parent/provider-storage"),
    )]);
    document["storage"] = storage;
    write_config(&path, &document);
    assert!(matches!(
        config::load(&path, uid).await,
        Err(config::ConfigError::StoragePath { .. })
    ));
    assert!(!actual.join("provider-storage").exists());
}

#[tokio::test]
async fn refused_storage_and_frame_bounds_keep_the_field_that_refused_them() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("configuration fixture");
    let path = directory.path().join("broker.yaml");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );
    write_owner_only(
        &directory.path().join("cli-probe.wasm"),
        b"component fixture",
    );
    fs::create_dir(directory.path().join("provider-storage")).expect("storage root");

    let mut document = attested_document(uid);
    let mut storage = serde_json::to_value(dekopon_storage_host::StorageLimits::default())
        .expect("storage limits serialize");
    storage.as_object_mut().expect("storage object").extend([
        ("rootPath".to_owned(), json!("provider-storage")),
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
    write_owner_only(
        &directory.path().join("cli-probe.wasm"),
        b"component fixture",
    );
    fs::create_dir(directory.path().join("provider-storage")).expect("storage root");

    let mut document = attested_document(uid);
    let mut storage = serde_json::to_value(dekopon_storage_host::StorageLimits::default())
        .expect("storage limits serialize");
    storage
        .as_object_mut()
        .expect("storage object")
        .extend([("rootPath".to_owned(), json!("provider-storage"))]);
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

#[test]
fn an_old_config_naming_namespace_key_path_is_refused() {
    let mut document = attested_document(current_uid());
    let mut storage = serde_json::to_value(dekopon_storage_host::StorageLimits::default())
        .expect("storage limits serialize");
    storage.as_object_mut().expect("limits object").extend([
        (
            "rootPath".to_owned(),
            json!("/var/lib/dekopon-provider-storage"),
        ),
        (
            "namespaceKeyPath".to_owned(),
            json!("/etc/dekopon-storage-key/storage-key.yaml"),
        ),
    ]);
    document["storage"] = storage;
    let refused = serde_json::from_value::<config::BrokerdConfig>(document)
        .expect_err("a retired storage field is refused");
    assert!(
        refused.to_string().contains("namespaceKeyPath"),
        "{refused}"
    );
}

#[tokio::test]
async fn the_startup_frame_check_covers_more_than_the_direct_peers() {
    use std::sync::Arc;

    use dekopon_broker::{
        AuthenticatedContext, Broker, BrokerLimits, ConstraintCatalog, CredentialStore,
        IdentityDirectory, InMemoryAuditLog, PolicyEngine, PolicyWorld,
    };
    use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
    use dekopon_broker_protocol::{Attestation, ResponseEnvelope};
    use dekopon_core::{Actor, AgentId, CapabilityId, PrincipalId};

    use super::{BrokerdError, MappedPeer, validate_capability_responses};

    let probe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/providers/cli-probe-provider.wasm");
    let registry = BrokerProviderRegistry::load([probe], BrokerHostLimits::default())
        .await
        .expect("load cli-probe fixture");
    let capability = "cli-probe.upper"
        .parse::<CapabilityId>()
        .expect("valid capability fixture");
    let world = PolicyWorld::new(
        ["gateway", "cpetersen"].map(|name| name.parse::<PrincipalId>().expect("valid principal")),
        [(
            capability.clone(),
            "cli-probe".parse().expect("valid provider fixture"),
        )],
    )
    .expect("declared world builds");
    let catalog = ConstraintCatalog::new([(
        capability,
        ConstraintSet {
            route: CapabilityRoute::Generic,
            provider: "cli-probe".parse().expect("valid provider fixture"),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Low,
            credential: None,
            constraints: ExecutionConstraints::default(),
        },
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

    let (capabilities, words) = broker.capability_ceiling();
    assert!(
        !capabilities.is_empty(),
        "the ceiling must see what policy grants an attested principal"
    );
    assert!(
        broker
            .capability_surface(
                &gateway,
                Some(&dekopon_broker::AttestorGrant {
                    namespaces: Some(vec!["slack.t0123abc".to_owned()]),
                }),
                Some(&Attestation::for_subject(
                    "slack.t0123abc.u9xyz".parse().expect("canonical subject"),
                    "chat-agent".parse::<AgentId>().expect("valid agent"),
                )),
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

#[tokio::test]
async fn ipc_group_socket_keeps_private_paths_private_and_replaces_only_safe_stale_sockets() {
    use dekopon_broker_protocol::{BrokerClient, ClientError, FrameLimits};
    use std::os::unix::fs::MetadataExt as _;

    let uid = current_uid();
    let directory = tempfile::tempdir().expect("IPC fixture");
    let path = directory.path().join("broker.sock");
    for mode in [0o700, 0o710, 0o750, 0o2710] {
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(mode)).unwrap();
        let (listener, mut guard) = socket::bind(&path, uid).await.expect("safe IPC parent");
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.uid(), uid);
        assert_eq!(
            metadata.permissions().mode() & 0o7777,
            if mode == 0o700 { 0o600 } else { 0o660 }
        );
        if mode != 0o700 {
            assert_eq!(
                metadata.gid(),
                fs::metadata(directory.path()).unwrap().gid()
            );
            assert!(
                socket::validate_private_parent(&path, uid).is_err(),
                "cache parents stay private"
            );
        }
        assert!(matches!(
            socket::bind(&path, uid).await,
            Err(super::SocketError::AlreadyRunning { .. })
        ));
        drop(listener);
        let parked = directory.path().join("parked.sock");
        fs::rename(&path, &parked).unwrap();
        guard.cleanup().unwrap();
        fs::rename(&parked, &path).unwrap();
        let (listener, mut replacement) = socket::bind(&path, uid)
            .await
            .expect("safe stale IPC socket");
        drop(listener);
        replacement.cleanup().unwrap();
    }
    for mode in [0o770, 0o730, 0o740, 0o711, 0o751, 0o777, 0o1770] {
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(mode)).unwrap();
        assert!(
            matches!(
                socket::bind(&path, uid).await,
                Err(super::SocketError::InsecureParent { .. })
            ),
            "unsafe parent {mode:o}"
        );
    }
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o710)).unwrap();
    assert!(socket::bind(&path, uid.wrapping_add(1)).await.is_err());
    let alias = directory.path().join("alias");
    std::os::unix::fs::symlink(directory.path(), &alias).unwrap();
    assert!(socket::bind(&alias.join("broker.sock"), uid).await.is_err());
    fs::remove_file(&alias).unwrap();
    fs::write(&path, "not a socket").unwrap();
    assert!(socket::bind(&path, uid).await.is_err());
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("missing", &path).unwrap();
    assert!(socket::bind(&path, uid).await.is_err());
    fs::remove_file(&path).unwrap();

    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    for mode in [0o666, 0o661, 0o670, 0o760, 0o1660] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        assert!(matches!(
            socket::bind(&path, uid).await,
            Err(super::SocketError::InsecureSocket { .. })
        ));
        let client = BrokerClient::new(&path, uid, FrameLimits::default()).unwrap();
        assert!(matches!(
            client.capabilities().await,
            Err(ClientError::UnsafeSocket)
        ));
    }

    fs::set_permissions(&path, fs::Permissions::from_mode(0o660)).unwrap();
    let limits = FrameLimits {
        max_frame_bytes: 64 * 1024,
        io_timeout: std::time::Duration::from_millis(200),
    };
    for mode in [0o770, 0o730, 0o740, 0o711, 0o751, 0o777, 0o1770, 0o700] {
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(mode)).unwrap();
        assert!(
            socket::bind(&path, uid).await.is_err(),
            "the broker bound under parent {mode:o}"
        );
        let client = BrokerClient::new(&path, uid, limits).unwrap();
        assert!(
            matches!(client.capabilities().await, Err(ClientError::UnsafeSocket)),
            "the client trusted parent {mode:o}"
        );
    }
    for mode in [0o710, 0o750, 0o2710] {
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(mode)).unwrap();
        let client = BrokerClient::new(&path, uid, limits).unwrap();
        assert!(
            !matches!(client.capabilities().await, Err(ClientError::UnsafeSocket)),
            "the client refused parent {mode:o} the broker binds under"
        );
    }
    drop(listener);
    fs::remove_file(path).unwrap();
}

#[test]
fn assets_config_is_optional_but_present_sections_require_both_keys_and_reject_unknowns() {
    for document in [
        json!({"rootPath": "assets"}),
        json!({"maxInFlightBytes": 1}),
        json!({"rootPath": "assets", "maxInFlightBytes": 1, "enabled": true}),
    ] {
        assert!(serde_json::from_value::<config::AssetsConfig>(document).is_err());
    }
    let parsed: config::AssetsConfig =
        serde_json::from_value(json!({"rootPath": "assets", "maxInFlightBytes": 0})).unwrap();
    assert_eq!(parsed.max_in_flight_bytes, 0);
    assert_eq!(parsed.root_path, Path::new("assets"));
}

#[tokio::test]
async fn assets_paths_are_resolved_and_refuse_overlap_in_both_directions() {
    let uid = current_uid();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("broker.yaml");
    write_owner_only(
        &directory.path().join("policies.cedar"),
        POLICIES.as_bytes(),
    );
    write_owner_only(
        &directory.path().join("cli-probe.wasm"),
        b"component fixture",
    );
    let mut document = attested_document(uid);
    document["assets"] = json!({"rootPath": "assets", "maxInFlightBytes": 100});
    write_config(&path, &document);
    let resolved = config::load(&path, uid).await.unwrap();
    assert_eq!(
        resolved.assets.unwrap().root_path,
        directory.path().canonicalize().unwrap().join("assets")
    );
    for assets in [
        ".",
        "broker.sock/child",
        "cli-probe.wasm/child",
        "policies.cedar/child",
    ] {
        document["assets"]["rootPath"] = json!(assets);
        write_config(&path, &document);
        assert!(
            matches!(
                config::load(&path, uid).await,
                Err(config::ConfigError::AssetsStateCollision
                    | config::ConfigError::AssetsPath { .. })
            ),
            "{assets}"
        );
    }
    document["assets"]["rootPath"] = json!("assets");
    fs::create_dir(directory.path().join("assets")).unwrap();
    document["socketPath"] = json!("assets/broker.sock");
    write_config(&path, &document);
    assert!(matches!(
        config::load(&path, uid).await,
        Err(config::ConfigError::AssetsStateCollision)
    ));
}

#[tokio::test]
async fn a_configuration_directory_merges_fragments_and_refuses_every_collision() {
    use std::os::unix::fs::PermissionsExt as _;

    let uid = current_uid();
    let root = tempfile::tempdir().expect("create configuration fixture");
    let directory = root.path().join("broker.d");
    fs::create_dir(&directory).expect("create broker.d");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).expect("restrict broker.d");
    fs::write(root.path().join("cli-probe.wasm"), b"component fixture")
        .expect("write provider path fixture");

    let mut host = attested_document(uid);
    let object = host.as_object_mut().expect("config object");
    let principals = object.remove("principals").expect("principals");
    let capabilities = object.remove("capabilities").expect("capabilities");
    object.remove("policiesPath");
    for (key, value) in object.iter_mut() {
        if key == "providers" {
            *value = json!(["../cli-probe.wasm"]);
        }
    }
    write_config(&directory.join("host.yaml"), &host);
    write_config(
        &directory.join("people.yaml"),
        &json!({"apiVersion": config::CONFIG_API_VERSION, "principals": principals}),
    );
    write_config(
        &directory.join("probe.yaml"),
        &json!({"apiVersion": config::CONFIG_API_VERSION, "capabilities": capabilities}),
    );
    write_owner_only(&directory.join("probe.cedar"), POLICIES.as_bytes());

    let resolved = config::load(&directory, uid)
        .await
        .expect("disjoint fragments resolve");
    assert_eq!(resolved.principals.len(), 1);
    assert_eq!(resolved.capabilities.len(), 1);
    assert!(resolved.policies.contains("chat-agent-upper"));

    write_config(
        &directory.join("twice.yaml"),
        &json!({
            "apiVersion": config::CONFIG_API_VERSION,
            "socketPath": "/elsewhere.sock",
            "principals": {"cpetersen": {"subjects": ["slack.t0123abc.uother"]}}
        }),
    );
    let Err(config::ConfigError::Fragments(dekopon_core::fragments::FragmentError::Conflicts {
        conflicts,
    })) = config::load(&directory, uid).await
    else {
        panic!("colliding fragments must refuse startup");
    };
    let keys = conflicts
        .iter()
        .map(|conflict| conflict.key.as_str())
        .collect::<Vec<_>>();
    assert_eq!(keys, ["principals.cpetersen", "socketPath"]);
}
