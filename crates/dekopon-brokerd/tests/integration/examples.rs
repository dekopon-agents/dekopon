#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::{collections::BTreeMap, path::PathBuf};

use dekopon_broker::{ConstraintSet, PolicyEngine, PolicyWorld};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_brokerd::{BrokerdConfig, capabilities};
use dekopon_capability::EffectKind;
use dekopon_core::{AgentId, CapabilityId, PrincipalId, ProviderId, RiskLevel};
use dekopon_policy::{PolicyContext, PolicyRequest, PolicyTarget};
use serde::Deserialize;

const GRANTED: [&str; 2] = ["http-probe.conditional-write", "http-probe.fetch"];
const UNGRANTED: [&str; 1] = ["http-probe.purge"];
const PRINCIPAL: &str = "alice";
const GATEWAY: &str = "dekopon-gatewayd";
const AGENT: &str = "conditional-writer";

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn example(name: &str) -> PathBuf {
    repository_root()
        .join("examples/conditional-write")
        .join(name)
}

fn read(name: &str) -> String {
    let path = example(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} reads: {error}", path.display()))
}

fn config() -> BrokerdConfig {
    serde_yaml::from_str::<BrokerdConfig>(&read("broker.yaml"))
        .expect("the example broker configuration decodes under the broker's own strict decoder")
}

async fn probe_registry() -> BrokerProviderRegistry {
    BrokerProviderRegistry::load(
        [repository_root().join("examples/providers/http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("the checked-in http-probe component loads")
}

fn world(config: &BrokerdConfig, registry: &BrokerProviderRegistry) -> PolicyWorld {
    PolicyWorld::new(
        config
            .identities
            .iter()
            .map(|identity| identity.principal.clone())
            .chain(config.principals.keys().cloned()),
        registry
            .capabilities()
            .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
    )
    .expect("the example's declared world is coherent")
}

fn constraint_sets(
    config: &BrokerdConfig,
    registry: &BrokerProviderRegistry,
) -> BTreeMap<CapabilityId, ConstraintSet> {
    let (sets, problems) = capabilities::constraint_sets(&config.capabilities, |provider| {
        let declared = registry
            .capabilities()
            .filter(|(owner, _)| *owner == provider)
            .map(|(_, capability)| capabilities::ManifestCapability {
                id: &capability.id,
                effect: capability.effect,
                risk: capability.risk,
            })
            .collect::<Vec<_>>();
        (!declared.is_empty()).then_some(declared)
    });
    assert_eq!(
        problems,
        [],
        "the example's capabilities resolve against the loaded manifest"
    );
    sets
}

fn capability(name: &str) -> CapabilityId {
    name.parse().expect("valid capability identifier")
}

fn attested(via: Option<&str>, agent: Option<&str>) -> PolicyContext {
    PolicyContext {
        via: via.map(str::to_owned),
        subject: Some("drn:slack:user:t0123abcd:u0123abcd".to_owned()),
        agent: agent.map(str::to_owned),
        ..PolicyContext::default()
    }
}

fn capability_request(
    sets: &BTreeMap<CapabilityId, ConstraintSet>,
    name: &str,
    context: PolicyContext,
) -> PolicyRequest {
    let id = capability(name);
    let set = sets
        .get(&id)
        .unwrap_or_else(|| panic!("{name} has a constraint set"));
    PolicyRequest {
        principal: PRINCIPAL.parse::<PrincipalId>().expect("valid principal"),
        target: PolicyTarget::Capability {
            capability: id,
            provider: set.provider.clone(),
            effect: set.effect,
            risk: set.risk,
        },
        context,
    }
}

fn prompt_request(agent: &str, context: PolicyContext) -> PolicyRequest {
    PolicyRequest {
        principal: PRINCIPAL.parse::<PrincipalId>().expect("valid principal"),
        target: PolicyTarget::AgentPrompt {
            agent: agent.parse::<AgentId>().expect("valid agent identifier"),
        },
        context,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_example_policy_compiles_against_the_checked_in_probe_provider() {
    let config = config();
    let registry = probe_registry().await;
    let world = world(&config, &registry);
    let sets = constraint_sets(&config, &registry);

    let policy = PolicyEngine::new(&read("policies.cedar"), &world)
        .expect("the example policy validates against the world its own configuration declares");
    assert_eq!(policy.policy_count(), 2, "two statements, two questions");

    for referenced in policy.referenced_capabilities() {
        assert!(
            sets.contains_key(referenced),
            "policy permits {referenced}, which has no constraint set in broker.yaml"
        );
    }

    let mut granted = policy
        .referenced_capabilities()
        .map(|capability| capability.as_str().to_owned())
        .collect::<Vec<_>>();
    granted.sort_unstable();
    let mut expected = GRANTED.map(str::to_owned).to_vec();
    expected.sort_unstable();
    assert_eq!(granted, expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sender_may_write_through_the_gateway_and_nothing_else_may() {
    let config = config();
    let registry = probe_registry().await;
    let world = world(&config, &registry);
    let policy = PolicyEngine::new(&read("policies.cedar"), &world).expect("example policy loads");
    let sets = &constraint_sets(&config, &registry);

    let session = policy.authorize(prompt_request(AGENT, attested(Some(GATEWAY), Some(AGENT))));
    assert!(session.allowed, "the mapped sender may drive the agent");
    assert_eq!(
        session.determining_policy_ids,
        vec!["alice-may-prompt-conditional-writer".to_owned()],
        "the audit record names the rule a reader can find in policies.cedar"
    );

    for name in GRANTED {
        let decision = policy.authorize(capability_request(
            sets,
            name,
            attested(Some(GATEWAY), Some(AGENT)),
        ));
        assert!(decision.allowed, "{name} is granted");
        assert_eq!(
            decision.determining_policy_ids,
            vec!["conditional-writer-surface".to_owned()],
            "{name}"
        );
        assert!(!decision.errors_present, "{name}");
    }

    for (case, context) in [
        ("no via", attested(None, Some(AGENT))),
        (
            "a different gateway",
            attested(Some("other-gateway"), Some(AGENT)),
        ),
        ("no agent", attested(Some(GATEWAY), None)),
        (
            "a different agent",
            attested(Some(GATEWAY), Some("some-other-agent")),
        ),
    ] {
        for name in GRANTED {
            let decision = policy.authorize(capability_request(sets, name, context.clone()));
            assert!(!decision.allowed, "{name} must be denied with {case}");
            assert!(
                decision.determining_policy_ids.is_empty(),
                "{name} / {case}"
            );
        }
    }
    for (case, agent, context) in [
        ("no via", AGENT, attested(None, Some(AGENT))),
        (
            "a different agent",
            "some-other-agent",
            attested(Some(GATEWAY), Some("some-other-agent")),
        ),
    ] {
        let decision = policy.authorize(prompt_request(agent, context));
        assert!(!decision.allowed, "agent.prompt must be denied with {case}");
    }

    for name in UNGRANTED {
        let id = capability(name);
        assert!(
            !sets.contains_key(&id),
            "{name} must stay unconstrained in the example"
        );
        let decision = policy.authorize(PolicyRequest {
            principal: PRINCIPAL.parse::<PrincipalId>().expect("valid principal"),
            target: PolicyTarget::Capability {
                capability: id,
                provider: "http-probe".parse::<ProviderId>().expect("valid provider"),
                effect: EffectKind::ExternalWrite,
                risk: RiskLevel::High,
            },
            context: attested(Some(GATEWAY), Some(AGENT)),
        });
        assert!(!decision.allowed, "{name} is not part of this example");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_example_constraint_set_matches_the_manifest_it_will_be_checked_against() {
    let config = config();
    let registry = probe_registry().await;
    let manifest = registry
        .capabilities()
        .map(|(provider, capability)| (capability.id.clone(), (provider.clone(), capability)))
        .collect::<BTreeMap<_, _>>();

    let sets = constraint_sets(&config, &registry);
    let declared = sets
        .keys()
        .map(|capability| capability.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        declared,
        {
            let mut expected = GRANTED.to_vec();
            expected.sort_unstable();
            expected
        },
        "the example declares exactly the read-and-conditional-write slice"
    );

    for (id, set) in &sets {
        let (provider, capability) = manifest
            .get(id)
            .unwrap_or_else(|| panic!("{id} exists in the loaded http-probe manifest"));
        assert_eq!(set.effect, capability.effect, "{id} effect");
        assert_eq!(set.risk, capability.risk, "{id} risk");
        assert_eq!(&set.provider, provider, "{id} provider route");

        let http = set
            .constraints
            .http
            .as_ref()
            .unwrap_or_else(|| panic!("{id} grants HTTP authority"));
        assert_eq!(
            http.allowed_hosts,
            vec!["api.example.com".to_owned()],
            "{id}"
        );
        assert!(
            !http.allow_plaintext_loopback,
            "{id} talks to the upstream API only"
        );
        assert_eq!(
            set.default_secret_use
                .as_ref()
                .map(|default| default.drn.as_str()),
            Some(SECRET_DRN),
            "{id} defaults to the broker-held token; the upstream needs it even to read"
        );
        let (methods, requests) = if capability.effect == EffectKind::ExternalWrite {
            (vec!["GET".to_owned(), "POST".to_owned()], 2)
        } else {
            (vec!["GET".to_owned()], 1)
        };
        assert_eq!(http.allowed_methods, methods, "{id}");
        assert_eq!(http.max_requests, requests, "{id}");
    }
}

const SECRET_DRN: &str = "drn:secret:example/api-token";

#[tokio::test(flavor = "multi_thread")]
async fn the_secret_map_example_binds_the_default_once_per_capability_within_its_destinations() {
    #[derive(Deserialize)]
    struct File {
        secrets: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        drn: String,
        destinations: Vec<String>,
        bindings: Vec<Binding>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Binding {
        capabilities: Vec<String>,
        allowed_hosts: Vec<String>,
    }

    let file = serde_yaml::from_str::<File>(&read("secret-map.yaml.example"))
        .expect("the secret map example decodes");
    let [entry] = file.secrets.as_slice() else {
        panic!("the example maps one secret");
    };
    assert_eq!(entry.drn, SECRET_DRN);

    let sets = constraint_sets(&config(), &probe_registry().await);
    for (id, set) in &sets {
        let bound = entry
            .bindings
            .iter()
            .filter(|binding| binding.capabilities.iter().any(|name| name == id.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            bound.len(),
            1,
            "{id} needs exactly one binding for its default"
        );
        let http = set.constraints.http.as_ref().expect("HTTP authority");
        for host in &bound[0].allowed_hosts {
            assert!(http.allowed_hosts.contains(host), "{id} binds {host}");
            assert!(entry.destinations.contains(host), "{host} is a destination");
        }
    }
}

#[test]
fn the_relative_paths_in_the_example_resolve_from_its_own_directory() {
    let config = config();
    assert_eq!(
        config.providers,
        vec![PathBuf::from("../providers/http-probe-provider.wasm")]
    );
    assert_eq!(
        config.policies_path,
        Some(PathBuf::from("policies.cedar")),
        "sibling of broker.yaml"
    );
    assert_eq!(
        config.secret_map_path,
        Some(PathBuf::from("secret-map.yaml")),
        "the copied file, not the .example"
    );
    for relative in [
        config.providers[0].clone(),
        config.policies_path.clone().expect("policies path"),
    ] {
        let resolved = example("").join(&relative);
        assert!(
            resolved.exists(),
            "{} does not resolve to a checked-in file",
            resolved.display()
        );
    }

    let identity = config.identities.first().expect("one peer identity");
    let grant = identity.attestor.as_ref().expect("the gateway may attest");
    let (principal, entry) = config.principals.iter().next().expect("one principal");
    let subject = entry.subjects.first().expect("one subject");
    assert!(
        grant
            .namespaces
            .iter()
            .flatten()
            .any(|namespace| namespace.covers(subject)),
        "{subject} sits outside the gateway's attestor namespaces"
    );
    assert_eq!(principal.as_str(), PRINCIPAL);
    assert_eq!(identity.principal.as_str(), GATEWAY);
}

fn write_broker_fixture(path: &std::path::Path, contents: &str) {
    use std::{fs, os::unix::fs::PermissionsExt as _};

    fs::write(path, contents).expect("write fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("secure fixture");
}

fn broker_diff(
    proposed: &std::path::Path,
    previous: &std::path::Path,
    options: &[&str],
) -> std::process::Output {
    broker_diff_command(proposed, previous)
        .args(options)
        .output()
        .expect("run broker diff")
}

fn broker_diff_command(
    proposed: &std::path::Path,
    previous: &std::path::Path,
) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_dekopon-brokerd"));
    command
        .arg("check")
        .arg(proposed)
        .arg("--diff")
        .arg(previous);
    command
}

fn copy_broker_diff_example(root: &std::path::Path) -> PathBuf {
    let config_dir = root.join("conditional-write");
    let providers = root.join("providers");
    std::fs::create_dir_all(&config_dir).expect("create example config directory");
    std::fs::create_dir(&providers).expect("create example provider directory");
    std::fs::copy(
        repository_root().join("examples/providers/http-probe-provider.wasm"),
        providers.join("http-probe-provider.wasm"),
    )
    .expect("copy example provider");
    write_broker_fixture(&config_dir.join("policies.cedar"), &read("policies.cedar"));
    write_broker_fixture(
        &config_dir.join("secret-map.yaml"),
        &read("secret-map.yaml.example"),
    );
    let config = config_dir.join("broker.yaml");
    write_broker_fixture(&config, &read("broker.yaml"));
    config
}

#[test]
fn broker_diff_compares_relocated_provider_copies() {
    let temporary = tempfile::tempdir().expect("fixture root");
    let previous = copy_broker_diff_example(&temporary.path().join("previous"));
    let proposed = copy_broker_diff_example(&temporary.path().join("proposed"));

    let unchanged = broker_diff(&proposed, &previous, &[]);
    assert_eq!(unchanged.status.code(), Some(0), "{unchanged:?}");
    assert_eq!(unchanged.stdout, b"ok\n");

    write_broker_fixture(
        &proposed,
        &read("broker.yaml").replace("strict: true", "strict: false"),
    );
    let swap = broker_diff(&proposed, &previous, &[]);
    assert_eq!(swap.status.code(), Some(0), "{swap:?}");
    assert_eq!(swap.stdout, b"swap\tstrict\n");
}

fn write_managed_diff_lock(root: &std::path::Path, providers: &[&str]) {
    use dekopon_brokerd::ProviderLockApiVersion;
    use serde_json::json;
    use sha2::{Digest as _, Sha256};
    use std::{fs, os::unix::fs::PermissionsExt as _};

    let store = root.join("store");
    let blobs = store.join("blobs");
    let sha256 = blobs.join("sha256");
    for directory in [&store, &blobs, &sha256] {
        fs::create_dir(directory).expect("create store directory");
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .expect("secure store directory");
    }
    let providers: Vec<_> = providers
        .iter()
        .map(|provider| {
            let component = fs::read(
                repository_root().join(format!("examples/providers/{provider}-provider.wasm")),
            )
            .expect("read example provider");
            let digest: String = Sha256::digest(&component)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            fs::write(sha256.join(format!("{digest}.wasm")), &component)
                .expect("write locked blob");
            json!({
                "source": format!("ghcr.io/example/{provider}:1.0.0"),
                "resolvedVersion": "1.0.0",
                "manifestDigest": format!("sha256:{}", "1".repeat(64)),
                "componentDigest": format!("sha256:{digest}"),
                "componentBytes": component.len(),
                "providerId": provider,
            })
        })
        .collect();
    let lock = json!({
        "apiVersion": ProviderLockApiVersion::V1Alpha1,
        "providers": providers,
    });
    write_broker_fixture(
        &root.join("providers.lock.yaml"),
        &serde_yaml::to_string(&lock).expect("serialize fixture lock"),
    );
}

fn managed_broker_diff_example(root: &std::path::Path, directory: bool) -> PathBuf {
    let config = managed_broker_diff_config(root, directory);
    write_managed_diff_lock(root, &["http-probe"]);
    config
}

fn managed_broker_diff_config(root: &std::path::Path, directory: bool) -> PathBuf {
    let config = copy_broker_diff_example(root);
    std::fs::rename(
        config.with_file_name("secret-map.yaml"),
        config.with_file_name("secret-map.yaml.example"),
    )
    .expect("keep the secret map out of the fragment scan");
    let mut document: serde_json::Value =
        serde_yaml::from_str(&read("broker.yaml")).expect("example broker config");
    let fields = document.as_object_mut().expect("config object");
    fields.remove("providers");
    if directory {
        fields.remove("policiesPath");
    }
    document["providerSet"] = serde_json::json!({
        "lockPath": "../providers.lock.yaml", "storePath": "../store"
    });
    document["secretMapPath"] = serde_json::json!("secret-map.yaml.example");
    write_broker_fixture(&config, &document.to_string());
    config
}

#[test]
fn broker_diff_reports_skipped_validation_for_absent_managed_artifacts() {
    use serde_json::{Value, json};

    let temporary = tempfile::tempdir().expect("fixture root");
    for (name, directory) in [("single", false), ("directory", true)] {
        let root = temporary.path().join(name);
        let previous_root = root.join("previous");
        let proposed_root = root.join("proposed");
        let previous_config = managed_broker_diff_config(&previous_root, directory);
        let proposed_config = managed_broker_diff_config(&proposed_root, directory);
        let input = |config: &std::path::Path| {
            if directory {
                config.parent().expect("config directory").to_path_buf()
            } else {
                config.to_path_buf()
            }
        };
        let previous = input(&previous_config);
        let proposed = input(&proposed_config);
        let original = std::fs::read_to_string(&proposed_config).expect("proposed config");
        for (document, swap, row) in [
            (original.clone(), json!([]), "ok"),
            (
                original.replace("\"strict\":true", "\"strict\":false"),
                json!(["strict"]),
                "swap\tstrict",
            ),
        ] {
            write_broker_fixture(&proposed_config, &document);
            let output = broker_diff(&proposed, &previous, &[]);
            assert_eq!(output.status.code(), Some(0), "{name}: {output:?}");
            assert_eq!(
                String::from_utf8(output.stdout).expect("diff table"),
                format!(
                    "{row}\nmanaged_set_validation_skipped\tprevious (local lock and store absent)\nmanaged_set_validation_skipped\tproposed (local lock and store absent)\n"
                )
            );
            let output = broker_diff(&proposed, &previous, &["--output", "json"]);
            assert_eq!(output.status.code(), Some(0), "{name}: {output:?}");
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).expect("diff JSON"),
                json!({
                    "restart_required": [],
                    "swap": swap,
                    "managed_set_validation_skipped": ["previous", "proposed"],
                })
            );
        }
        assert!(!previous_root.join("providers.lock.yaml").exists());
        assert!(!previous_root.join("store").exists());
        assert!(!proposed_root.join("providers.lock.yaml").exists());
        assert!(!proposed_root.join("store").exists());

        write_managed_diff_lock(&previous_root, &["http-probe"]);
        let mixed = broker_diff(&proposed, &previous, &["--output", "json"]);
        assert_eq!(mixed.status.code(), Some(0), "{name}: {mixed:?}");
        assert_eq!(
            serde_json::from_slice::<Value>(&mixed.stdout).expect("mixed diff JSON"),
            json!({
                "restart_required": [],
                "swap": ["strict"],
                "managed_set_validation_skipped": ["proposed"],
            })
        );

        write_broker_fixture(&proposed_config.with_file_name("policies.cedar"), "permit(");
        let invalid = broker_diff(&proposed, &proposed, &["--output", "json"]);
        assert_eq!(invalid.status.code(), Some(1), "{name}: {invalid:?}");
        assert!(invalid.stdout.is_empty());
    }
}

#[tokio::test]
async fn broker_diff_validates_policy_without_managed_artifacts() {
    use dekopon_broker::PolicyBuildError;
    use dekopon_brokerd::BrokerdError;
    use dekopon_core::IdentifierError;

    let temporary = tempfile::tempdir().expect("fixture root");
    let previous = managed_broker_diff_config(&temporary.path().join("previous"), false);
    let proposed = managed_broker_diff_config(&temporary.path().join("proposed"), false);
    let policy_path = proposed
        .with_file_name("policies.cedar")
        .canonicalize()
        .expect("canonical policy path");

    for (policy, expected) in [
        (
            r#"@id("invalid policy id") permit(principal, action, resource);"#,
            PolicyBuildError::InvalidPolicyId {
                policy: "policy0".to_owned(),
            },
        ),
        (
            r#"@id("subject-fence") forbid(principal, action, resource)
               when { context has subject && context.subject == "drn:slack:user:T0123:U0456" };"#,
            PolicyBuildError::NonCanonicalDrn {
                policy: "subject-fence".to_owned(),
                value: "drn:slack:user:T0123:U0456".to_owned(),
            },
        ),
        (
            r#"@id("dekopon-broker-default-secret-use") forbid(principal, action, resource);"#,
            PolicyBuildError::ReservedPolicyId {
                policy: "dekopon-broker-default-secret-use".to_owned(),
            },
        ),
        (
            r#"@id("request-permit") permit(principal, action, resource)
               when { context.request.method == "POST" };"#,
            PolicyBuildError::InvalidRequestShapes {
                policies: vec!["request-permit".to_owned()],
            },
        ),
        (
            r#"@id("unknown-principal")
               permit(principal == Dekopon::Principal::"nobody", action, resource);"#,
            PolicyBuildError::UnknownPrincipal {
                policy: "unknown-principal".to_owned(),
                principal: "nobody".to_owned(),
            },
        ),
        (
            r#"@id("malformed-principal")
               permit(principal == Dekopon::Principal::"Ops Team", action, resource);"#,
            PolicyBuildError::MalformedPrincipal {
                policy: "malformed-principal".to_owned(),
                principal: "Ops Team".to_owned(),
                source: IdentifierError::InvalidStart {
                    kind: "principal",
                    character: 'O',
                },
            },
        ),
        (
            r#"@id("unknown-group")
               permit(principal in Dekopon::Group::"operators", action, resource);"#,
            PolicyBuildError::UnknownGroup {
                policy: "unknown-group".to_owned(),
                group: "operators".to_owned(),
            },
        ),
        (
            r#"@id("unknown-type")
               permit(principal == Dekopon::Unsupported::"alice", action, resource);"#,
            PolicyBuildError::UnknownEntityType {
                policy: "unknown-type".to_owned(),
                entity_type: "Dekopon::Unsupported".to_owned(),
            },
        ),
    ] {
        write_broker_fixture(&policy_path, policy);
        let output = broker_diff(&proposed, &previous, &["--output", "json"]);
        assert_eq!(output.status.code(), Some(1), "{expected:?}: {output:?}");
        assert!(output.stdout.is_empty());

        let error = dekopon_brokerd::check_diff(&proposed, &previous)
            .await
            .expect_err("invalid policy must fail with both managed artifacts absent");
        let BrokerdError::Policy {
            path: Some(path),
            source,
        } = error
        else {
            panic!("expected a policy error, got {error:?}");
        };
        assert_eq!(path.as_ref(), policy_path);
        assert_eq!(source, expected);
    }

    write_broker_fixture(&policy_path, &read("policies.cedar"));
    let valid = broker_diff(&proposed, &previous, &["--output", "json"]);
    assert_eq!(valid.status.code(), Some(0), "{valid:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&valid.stdout).expect("diff JSON"),
        serde_json::json!({
            "restart_required": [],
            "swap": [],
            "managed_set_validation_skipped": ["previous", "proposed"],
        })
    );
}

#[tokio::test]
async fn broker_diff_compares_managed_sets_from_local_artifacts() {
    use dekopon_brokerd::{BrokerdError, ConfigError};

    let temporary = tempfile::tempdir().expect("fixture root");
    let previous_root = temporary.path().join("previous");
    let proposed_root = temporary.path().join("proposed");
    let previous = managed_broker_diff_example(&previous_root, false);
    let proposed = managed_broker_diff_example(&proposed_root, false);
    let original = std::fs::read_to_string(&proposed).expect("proposed config");
    write_broker_fixture(
        &proposed,
        &original.replace("\"strict\":true", "\"strict\":false"),
    );

    let scratch = temporary.path().join("scratch");
    write_broker_fixture(&scratch, "scratch cannot contain directories");
    let output = broker_diff_command(&proposed, &previous)
        .env("TMPDIR", &scratch)
        .output()
        .expect("run offline managed diff");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, b"swap\tstrict\n");
    for root in [&previous_root, &proposed_root] {
        assert!(!root.join("store/cwasm").exists());
        assert!(!root.join("store/.provider-manager.lock").exists());
    }

    for missing in ["providers.lock.yaml", "store"] {
        let artifact = proposed_root.join(missing);
        let saved = proposed_root.join("saved-artifact");
        std::fs::rename(&artifact, &saved).expect("hide one local artifact");
        let error = dekopon_brokerd::check_diff(&proposed, &previous)
            .await
            .expect_err("diff requires both or neither artifact");
        assert!(matches!(
            error,
            BrokerdError::Config(ConfigError::ResolvePath { path, source })
                if path.ends_with(missing) && source.kind() == std::io::ErrorKind::NotFound
        ));
        let refusal = broker_diff(&proposed, &previous, &["--output", "json"]);
        assert_eq!(refusal.status.code(), Some(1), "{refusal:?}");
        assert!(refusal.stdout.is_empty());
        assert!(!artifact.exists());
        std::fs::rename(&saved, &artifact).expect("restore the local artifact");
    }
}

#[tokio::test]
async fn broker_diff_refuses_present_but_broken_managed_artifacts() {
    use dekopon_brokerd::{BrokerdError, ConfigError, ProviderManagerError};
    use std::{
        fs,
        os::unix::fs::{PermissionsExt as _, symlink},
    };

    let temporary = tempfile::tempdir().expect("fixture root");
    let proposed_root = temporary.path().join("broken");
    let previous = managed_broker_diff_config(&temporary.path().join("absent"), false);
    let proposed = managed_broker_diff_config(&proposed_root, false);
    let lock = proposed_root.join("providers.lock.yaml");
    let store = proposed_root.join("store");
    symlink("missing-lock", &lock).expect("dangling lock entry");
    symlink("missing-store", &store).expect("dangling store entry");

    let error = dekopon_brokerd::check_diff(&proposed, &previous)
        .await
        .expect_err("dangling entries count as present");
    assert!(matches!(
        error,
        BrokerdError::Config(ConfigError::ResolvePath { path, source })
            if path.ends_with("providers.lock.yaml") && source.kind() == std::io::ErrorKind::NotFound
    ));
    let dangling = broker_diff(&proposed, &previous, &["--output", "json"]);
    assert_eq!(dangling.status.code(), Some(1), "{dangling:?}");
    assert!(dangling.stdout.is_empty());

    fs::remove_file(&lock).expect("remove lock symlink");
    fs::remove_file(&store).expect("remove store symlink");
    write_managed_diff_lock(&proposed_root, &["http-probe"]);
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o666)).expect("insecure lock mode");
    let insecure = dekopon_brokerd::check_diff(&proposed, &previous)
        .await
        .expect_err("present insecure lock must fail");
    assert!(matches!(
        insecure,
        BrokerdError::Config(ConfigError::ProviderLock {
            source: ProviderManagerError::FileSecurity { .. },
            ..
        })
    ));
    let insecure = broker_diff(&proposed, &previous, &["--output", "json"]);
    assert_eq!(insecure.status.code(), Some(1), "{insecure:?}");
    assert!(insecure.stdout.is_empty());

    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).expect("restore lock mode");
    write_broker_fixture(&lock, "providers: [");
    let corrupt = dekopon_brokerd::check_diff(&proposed, &previous)
        .await
        .expect_err("present corrupt lock must fail");
    assert!(matches!(
        corrupt,
        BrokerdError::Config(ConfigError::ProviderLock {
            source: ProviderManagerError::Configuration { .. },
            ..
        })
    ));
    let corrupt = broker_diff(&proposed, &previous, &["--output", "json"]);
    assert_eq!(corrupt.status.code(), Some(1), "{corrupt:?}");
    assert!(corrupt.stdout.is_empty());
}

#[test]
fn broker_diff_sanitizes_invalid_local_provider_lock_diagnostics() {
    let temporary = tempfile::tempdir().expect("fixture root");
    let config = managed_broker_diff_example(temporary.path(), false);
    let lock = temporary.path().join("providers.lock.yaml");
    let mut document: serde_json::Value =
        serde_yaml::from_str(&std::fs::read_to_string(&lock).expect("read provider lock"))
            .expect("decode provider lock");
    let marker = "private-lock-fixture-text";
    document["providers"][0]["source"] =
        serde_json::json!(format!("https://{marker}@example.com/repository:1.0.0"));
    write_broker_fixture(&lock, &document.to_string());

    let output = broker_diff(&config, &config, &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("providers.lock.yaml"));
    assert!(diagnostic.contains("expected a bounded OCI reference rather than a URL"));
    assert!(!diagnostic.contains(marker));
}

#[tokio::test]
async fn broker_diff_rejects_managed_policy_and_constraint_errors_in_both_input_forms() {
    use dekopon_broker::PolicyBuildError;
    use dekopon_brokerd::{BrokerdError, capabilities::CapabilityProblem};

    let temporary = tempfile::tempdir().expect("fixture root");
    for (name, directory) in [("single", false), ("directory", true)] {
        let root = temporary.path().join(name);
        let previous = managed_broker_diff_example(&root.join("previous"), directory);
        let invalid = managed_broker_diff_example(&root.join("invalid"), directory);
        let policy = invalid.with_file_name("policies.cedar");
        let path = |config: &std::path::Path| {
            if directory {
                config.parent().expect("config directory").to_path_buf()
            } else {
                config.to_path_buf()
            }
        };
        let (previous, invalid_input) = (path(&previous), path(&invalid));
        let pairs = [
            (&invalid_input, &previous),
            (&previous, &invalid_input),
            (&invalid_input, &invalid_input),
        ];
        let marker = "private-managed-policy-fixture-text";
        write_broker_fixture(&policy, &format!("permit({marker}"));
        for (proposed, baseline) in pairs {
            let output = broker_diff(proposed, baseline, &["--output", "json"]);
            assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");
            assert!(output.stdout.is_empty());
            let diagnostic = String::from_utf8_lossy(&output.stderr);
            assert!(diagnostic.contains("could not be parsed (byte offsets "));
            assert!(!diagnostic.contains(marker));
        }
        let error = dekopon_brokerd::check_diff(&invalid_input, &previous)
            .await
            .expect_err("malformed managed policy is a load failure");
        assert!(matches!(
            error,
            BrokerdError::Policy {
                source: PolicyBuildError::Parse { .. },
                ..
            }
        ));

        write_broker_fixture(&policy, &read("policies.cedar"));
        let mut document: serde_json::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&invalid).expect("invalid config"))
                .expect("example config");
        document["capabilities"]["http-probe"]["capabilities"]["http-probe.fetch"]["constraints"]
            .as_object_mut()
            .expect("fetch constraints")
            .remove("timeoutMs")
            .expect("fixture has required timeout");
        write_broker_fixture(&invalid, &document.to_string());
        for (proposed, baseline) in pairs {
            let output = broker_diff(proposed, baseline, &["--output", "json"]);
            assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");
            assert!(output.stdout.is_empty());
        }
        let error = dekopon_brokerd::check_diff(&invalid_input, &previous)
            .await
            .expect_err("missing managed constraint is a load failure");
        assert!(matches!(
            error,
            BrokerdError::Capabilities { problems }
                if problems.iter().any(|problem| matches!(
                    problem,
                    CapabilityProblem::MissingField { capability, field: "timeoutMs", .. }
                        if capability.as_str() == "http-probe.fetch"
                ))
        ));
    }
}

#[test]
fn broker_diff_leaves_storage_and_scratch_paths_untouched() {
    let temporary = tempfile::tempdir().expect("fixture root");
    let previous = copy_broker_diff_example(temporary.path());
    let proposed = previous.with_file_name("proposed.yaml");
    std::fs::copy(
        repository_root().join("examples/providers/storage-probe-provider.wasm"),
        temporary
            .path()
            .join("providers/storage-probe-provider.wasm"),
    )
    .expect("copy storage provider");
    let mut config: serde_json::Value =
        serde_yaml::from_str(&read("broker.yaml")).expect("example broker config");
    config["providers"]
        .as_array_mut()
        .expect("provider paths")
        .push(serde_json::json!(
            "../providers/storage-probe-provider.wasm"
        ));
    config["capabilities"]["storage-probe"] = serde_json::json!({
        "constraints": {
            "timeoutMs": 30_000,
            "storage": {"interface": "durable-files", "access": "read-write", "scope": "agent"}
        },
        "capabilities": {"storage-probe.run": {}}
    });
    let mut storage = serde_json::to_value(dekopon_storage_host::StorageLimits::default())
        .expect("default storage limits");
    storage["rootPath"] = serde_json::json!("state");
    let scratch = temporary.path().join("scratch");
    write_broker_fixture(&scratch, "scratch cannot contain directories");

    for managed in [false, true] {
        if managed {
            write_managed_diff_lock(temporary.path(), &["http-probe", "storage-probe"]);
            config
                .as_object_mut()
                .expect("config object")
                .remove("providers");
            config["providerSet"] = serde_json::json!({
                "lockPath": "../providers.lock.yaml", "storePath": "../store"
            });
        }
        config["storage"] = storage.clone();
        config["strict"] = serde_json::json!(true);
        write_broker_fixture(&previous, &config.to_string());
        config["strict"] = serde_json::json!(false);
        write_broker_fixture(&proposed, &config.to_string());

        let output = broker_diff_command(&proposed, &previous)
            .env("TMPDIR", &scratch)
            .output()
            .expect("run storage diff");
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert_eq!(output.stdout, b"swap\tstrict\n");
        assert!(!previous.with_file_name("state").exists());
        assert_eq!(
            std::fs::read_to_string(&scratch).expect("scratch marker"),
            "scratch cannot contain directories"
        );

        config["storage"]["maxRootBytes"] = serde_json::json!(3);
        config["storage"]["maxNamespaceBytes"] = serde_json::json!(2);
        config["storage"]["maxFileBytes"] = serde_json::json!(1);
        write_broker_fixture(&proposed, &config.to_string());
        let invalid = broker_diff_command(&proposed, &previous)
            .env("TMPDIR", &scratch)
            .output()
            .expect("run diff with invalid storage limits");
        assert_eq!(invalid.status.code(), Some(1), "{invalid:?}");
        assert!(invalid.stdout.is_empty());
        assert!(!previous.with_file_name("state").exists());
    }
}

#[test]
fn broker_diff_refuses_unparseable_policy_in_either_input() {
    let temporary = tempfile::tempdir().expect("fixture root");
    let previous = copy_broker_diff_example(&temporary.path().join("previous"));
    let invalid = copy_broker_diff_example(&temporary.path().join("invalid"));
    let policy_path = invalid.with_file_name("policies.cedar");
    let marker = "private-policy-fixture-text";
    write_broker_fixture(&policy_path, &format!("permit({marker}"));
    let policy_path = policy_path.canonicalize().expect("canonical policy path");

    for (proposed, baseline) in [
        (&invalid, &previous),
        (&previous, &invalid),
        (&invalid, &invalid),
    ] {
        let output = broker_diff(proposed, baseline, &["--output", "json"]);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
        let diagnostic = String::from_utf8(output.stderr).expect("policy diagnostic");
        assert!(diagnostic.contains(policy_path.to_str().expect("policy path")));
        assert!(diagnostic.contains("could not be parsed (byte offsets "));
        assert!(!diagnostic.contains(marker));
    }
}

#[test]
fn broker_diff_refuses_invalid_wasm_in_either_input() {
    let temporary = tempfile::tempdir().expect("fixture root");
    let previous = copy_broker_diff_example(&temporary.path().join("previous"));
    let invalid = copy_broker_diff_example(&temporary.path().join("invalid"));
    let wasm_path = temporary
        .path()
        .join("invalid/providers/http-probe-provider.wasm");
    let marker = "private-wasm-fixture-text";
    write_broker_fixture(&wasm_path, marker);
    let wasm_path = wasm_path.canonicalize().expect("canonical Wasm path");

    for (proposed, baseline) in [
        (&invalid, &previous),
        (&previous, &invalid),
        (&invalid, &invalid),
    ] {
        let output = broker_diff(proposed, baseline, &["--output", "json"]);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
        let diagnostic = String::from_utf8(output.stderr).expect("Wasm diagnostic");
        assert!(diagnostic.contains(wasm_path.to_str().expect("Wasm path")));
        assert!(diagnostic.contains("could not compile"));
        assert!(!diagnostic.contains(marker));
    }
}

#[test]
fn broker_diff_reports_key_policy_and_secret_map_dispositions_in_directories() {
    use serde_json::{Value, json};
    use std::{fs, os::unix::fs::PermissionsExt as _};

    let temporary = tempfile::tempdir().expect("fixture root");
    let previous = temporary.path().join("previous.d");
    let proposed = temporary.path().join("proposed.d");
    let config = read("broker.yaml")
        .replace("policiesPath: policies.cedar\n", "")
        .replace(
            "secretMapPath: secret-map.yaml",
            "secretMapPath: secret-map.yaml.example",
        )
        .replace(
            "../providers/http-probe-provider.wasm",
            &repository_root()
                .join("examples/providers/http-probe-provider.wasm")
                .display()
                .to_string(),
        );
    let policies = read("policies.cedar");
    let secret_map = read("secret-map.yaml.example").replace(
        "/home/you/.config/dekopon/api-token",
        &temporary
            .path()
            .join("absent-credential")
            .display()
            .to_string(),
    );
    for root in [&previous, &proposed] {
        fs::create_dir(root).expect("create config directory");
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))
            .expect("secure config directory");
        write_broker_fixture(&root.join("broker.yaml"), &config);
        write_broker_fixture(&root.join("policies.cedar"), &policies);
        write_broker_fixture(&root.join("secret-map.yaml.example"), &secret_map);
    }

    let unchanged = broker_diff(&proposed, &previous, &[]);
    assert_eq!(unchanged.status.code(), Some(0), "{unchanged:?}");
    assert_eq!(unchanged.stdout, b"ok\n");

    let swap = config.replace("strict: true", "strict: false");
    write_broker_fixture(&proposed.join("broker.yaml"), &swap);
    write_broker_fixture(
        &proposed.join("policies.cedar"),
        &policies.replace("conditional-writer-surface", "changed-policy-name"),
    );
    write_broker_fixture(
        &proposed.join("secret-map.yaml.example"),
        &secret_map.replace("mapRevision: example-1", "mapRevision: example-2"),
    );
    let output = broker_diff(&proposed, &previous, &["--output", "json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).expect("diff JSON"),
        json!({"restart_required": [], "swap": ["policies", "secretMapReferences", "strict"]})
    );

    write_broker_fixture(
        &proposed.join("broker.yaml"),
        &config.replace("/home/you/.local/run/dekopon/broker.sock", "other.sock"),
    );
    let output = broker_diff(&proposed, &previous, &["--output", "json"]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).expect("diff JSON"),
        json!({"restart_required": ["socketPath"], "swap": ["policies", "secretMapReferences"]})
    );
}

#[test]
fn broker_diff_loads_single_files_and_refuses_an_unloadable_input() {
    let temporary = tempfile::tempdir().expect("fixture root");
    let previous = temporary.path().join("previous.yaml");
    let proposed = temporary.path().join("proposed.yaml");
    let invalid = temporary.path().join("invalid.yaml");
    let broker = read("broker.yaml").replace(
        "../providers/http-probe-provider.wasm",
        &repository_root()
            .join("examples/providers/http-probe-provider.wasm")
            .display()
            .to_string(),
    );
    write_broker_fixture(
        &temporary.path().join("policies.cedar"),
        &read("policies.cedar"),
    );
    write_broker_fixture(
        &temporary.path().join("secret-map.yaml"),
        &read("secret-map.yaml.example"),
    );
    write_broker_fixture(&previous, &broker);
    write_broker_fixture(&proposed, &broker.replace("strict: true", "strict: false"));
    write_broker_fixture(&invalid, "apiVersion: deliberately-invalid-version\n");

    let output = broker_diff(&proposed, &previous, &[]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, b"swap\tstrict\n");
    for (current, baseline) in [(&proposed, &invalid), (&invalid, &previous)] {
        let output = broker_diff(current, baseline, &[]);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
    }
    for options in [&["--materialized"][..], &["--store", "store"]] {
        assert_eq!(
            broker_diff(&proposed, &previous, options).status.code(),
            Some(2),
            "{options:?}"
        );
    }
}
