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
const PRINCIPAL: &str = "cpetersen";
const GATEWAY: &str = "dekopon-gatewayd";
const AGENT: &str = "xaviers-conditional-writer";

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
        vec!["boss-may-prompt-conditional-writer".to_owned()],
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
