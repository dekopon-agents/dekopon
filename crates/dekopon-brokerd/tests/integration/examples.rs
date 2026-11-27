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

#[test]
fn broker_diff_compares_managed_sets_without_resolving_providers() {
    let temporary = tempfile::tempdir().expect("fixture root");
    let previous = temporary.path().join("previous.yaml");
    let proposed = temporary.path().join("proposed.yaml");
    let secret_map = temporary.path().join("secret-map.yaml");
    let config = "apiVersion: dekopon.dev/brokerd/v1alpha1\n\
                  socketPath: broker.sock\n\
                  providerSet: {lockPath: providers.lock.yaml, storePath: store}\n\
                  secretMapPath: secret-map.yaml\n\
                  strict: true\n";
    write_broker_fixture(&previous, config);
    write_broker_fixture(&proposed, &config.replace("strict: true", "strict: false"));
    write_broker_fixture(&secret_map, &read("secret-map.yaml.example"));

    let swap = broker_diff(&proposed, &previous, &[]);
    assert_eq!(swap.status.code(), Some(0), "{swap:?}");
    assert_eq!(swap.stdout, b"swap\tstrict\n");
    assert!(!temporary.path().join("store").exists());

    write_broker_fixture(&secret_map, "secrets: [");
    let refusal = broker_diff(&proposed, &previous, &[]);
    assert_eq!(refusal.status.code(), Some(1), "{refusal:?}");
    assert!(refusal.stdout.is_empty());
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
    config["storage"] = serde_json::to_value(dekopon_storage_host::StorageLimits::default())
        .expect("default storage limits");
    config["storage"]["rootPath"] = serde_json::json!("state");
    write_broker_fixture(&previous, &config.to_string());
    config["strict"] = serde_json::json!(false);
    write_broker_fixture(&proposed, &config.to_string());
    let scratch = temporary.path().join("scratch");
    write_broker_fixture(&scratch, "scratch cannot contain directories");

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
