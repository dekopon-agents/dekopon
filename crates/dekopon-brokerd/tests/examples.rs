//! The `examples/conditional-write/` walkthrough, held against the real machinery it documents.
//!
//! The example is a configuration a reader copies and runs, and its whole claim is that five
//! narrow GitHub capabilities are reachable by one person, through one gateway, driving one agent
//! — and that nothing else is. Prose cannot keep that true. These tests load the checked-in `gh`
//! component, build the same declared world `dekopon-brokerd` builds at startup, compile the
//! example's Cedar file against it, and assert the decision table both directions.
//!
//! The configuration half is checked with the broker's own `BrokerdConfig` decoder rather than a
//! hand-written struct, so a renamed or newly required field breaks here too. What is deliberately
//! *not* done is running the loaded configuration through the full service: that would need a
//! temporary directory, mode-0600 fixtures, a rewritten socket path, and a copy of the credentials
//! file, which is a lot of scaffolding to prove something `tests/server.rs` already proves. The
//! drift this file exists to catch is the example disagreeing with the manifest or the policy
//! grammar, and that needs no filesystem at all.

#![cfg(unix)]

use std::{collections::BTreeMap, path::PathBuf};

use dekopon_broker::{PolicyEngine, PolicyWorld};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_brokerd::BrokerdConfig;
use dekopon_capability::{EffectKind, Idempotency};
use dekopon_core::{AgentId, CapabilityId, PrincipalId, ProviderId, RiskLevel};
use dekopon_policy::{PolicyContext, PolicyRequest, PolicyTarget};
use serde::Deserialize;

/// The five Tier-1 capabilities the example grants, in the order its files list them.
const GRANTED: [&str; 2] = ["http-probe.fetch", "http-probe.conditional-write"];
/// A capability the probe manifest exposes and the example deliberately does not grant.
const UNGRANTED: &str = "http-probe.purge";
const PRINCIPAL: &str = "cpetersen";
const GATEWAY: &str = "dekopond-gateway";
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
    .expect("the checked-in gh component loads")
}

/// Builds the world exactly as `dekopon-brokerd::run` does: principals from the configuration's
/// peer identities and subject mappings, capability routes from the loaded manifests.
fn world(config: &BrokerdConfig, registry: &BrokerProviderRegistry) -> PolicyWorld {
    PolicyWorld::new(
        config
            .identities
            .iter()
            .map(|identity| identity.principal.clone())
            .chain(
                config
                    .identity_mappings
                    .iter()
                    .map(|mapping| mapping.principal.clone()),
            ),
        registry
            .capabilities()
            .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
    )
    .expect("the example's declared world is coherent")
}

fn capability(name: &str) -> CapabilityId {
    name.parse().expect("valid capability identifier")
}

fn attested(via: Option<&str>, agent: Option<&str>) -> PolicyContext {
    PolicyContext {
        via: via.map(str::to_owned),
        subject: Some("slack.t0123abcd.u0123abcd".to_owned()),
        agent: agent.map(str::to_owned),
    }
}

/// One capability question, classified exactly as the example's constraint sets classify it.
fn capability_request(
    sets: &BTreeMap<CapabilityId, dekopon_broker::ConstraintSet>,
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
            idempotency: set.idempotency,
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

    let policy = PolicyEngine::new(&read("policies.cedar"), &world)
        .expect("the example policy validates against the world its own configuration declares");
    assert_eq!(policy.policy_count(), 2, "two statements, two questions");

    // The startup rule that makes an unreachable grant impossible: every capability the policy can
    // permit has to be executable. The broker refuses to start otherwise, so an example that adds
    // a capability to the policy and forgets its constraint set fails here first.
    for referenced in policy.referenced_capabilities() {
        assert!(
            config.constraint_sets.contains_key(referenced),
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
async fn the_boss_may_review_through_the_gateway_and_nothing_else_may() {
    let config = config();
    let registry = probe_registry().await;
    let world = world(&config, &registry);
    let policy = PolicyEngine::new(&read("policies.cedar"), &world).expect("example policy loads");
    let sets = &config.constraint_sets;

    // Allowed: the whole workflow, under the attested context the gateway actually renders.
    let session = policy.authorize(prompt_request(AGENT, attested(Some(GATEWAY), Some(AGENT))));
    assert!(session.allowed, "the boss may drive the agent");
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

    // Denied, one missing condition at a time.
    //
    // No `via` is the direct-peer case: a process connecting to the broker socket under the same
    // UID, with no gateway vouching for anyone. It matches neither rule, which is the property
    // that keeps adding a gateway from widening a grant that already existed.
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

    // A capability the manifest exposes, the example never grants, and no constraint set covers.
    // Cedar denies it even with every context condition satisfied; the broker would have denied it
    // `unconstrained-capability` before reaching Cedar at all.
    let merge = capability(UNGRANTED);
    assert!(
        !config.constraint_sets.contains_key(&merge),
        "{UNGRANTED} must stay unconstrained in the example"
    );
    let decision = policy.authorize(PolicyRequest {
        principal: PRINCIPAL.parse::<PrincipalId>().expect("valid principal"),
        target: PolicyTarget::Capability {
            capability: merge,
            provider: "http-probe".parse::<ProviderId>().expect("valid provider"),
            effect: EffectKind::ExternalWrite,
            risk: RiskLevel::High,
            idempotency: Idempotency::Conditional,
        },
        context: attested(Some(GATEWAY), Some(AGENT)),
    });
    assert!(!decision.allowed, "{UNGRANTED} is not part of this example");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_example_constraint_set_matches_the_manifest_it_will_be_checked_against() {
    let config = config();
    let registry = probe_registry().await;
    let manifest = registry
        .capabilities()
        .map(|(provider, capability)| (capability.id.clone(), (provider.clone(), capability)))
        .collect::<BTreeMap<_, _>>();

    let declared = config
        .constraint_sets
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
        "the example declares exactly the Tier-1 review slice"
    );

    for (id, set) in &config.constraint_sets {
        let (provider, capability) = manifest
            .get(id)
            .unwrap_or_else(|| panic!("{id} exists in the loaded gh manifest"));
        // These three are what `Broker::new` compares byte for byte against the manifest; a
        // mismatch is a startup refusal, so an example carrying one would never run.
        assert_eq!(set.effect, capability.effect, "{id} effect");
        assert_eq!(set.risk, capability.risk, "{id} risk");
        assert_eq!(set.idempotency, capability.idempotency, "{id} idempotency");
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
        assert!(!http.allow_plaintext_loopback, "{id} talks to GitHub only");
        assert_eq!(
            set.credential.as_deref(),
            Some("api-token"),
            "{id} presents the broker-held credential; private repositories need it even to read"
        );
        // The write pre-reads its pull request, which is exactly one extra GET and the reason the
        // capability is `conditional`. A read that could issue two requests would be a read that
        // could paginate somewhere the example never inspected.
        let (methods, requests) = if capability.effect == EffectKind::ExternalWrite {
            (vec!["GET".to_owned(), "POST".to_owned()], 2)
        } else {
            (vec!["GET".to_owned()], 1)
        };
        assert_eq!(http.allowed_methods, methods, "{id}");
        assert_eq!(http.max_requests, requests, "{id}");
    }
}

/// The destination coverage rule the broker enforces at startup, checked against the credentials
/// file the walkthrough tells the reader to copy.
///
/// The real file is `broker-credentials.yaml`, which is deliberately absent from the repository —
/// it is the one file holding a secret. The `.example` beside it is what a reader edits, so it is
/// the one worth pinning: if it stops naming `api-token`, or stops binding `api.example.com`,
/// following the walkthrough produces a broker that refuses to start.
#[test]
fn the_credentials_example_covers_every_host_the_constraint_sets_allow() {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct File {
        api_version: String,
        credentials: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        name: String,
        kind: String,
        scheme: String,
        destinations: Vec<String>,
        secret: String,
    }

    let file = serde_yaml::from_str::<File>(&read("broker-credentials.yaml.example"))
        .expect("the credentials example decodes");
    assert_eq!(file.api_version, dekopon_brokerd::CREDENTIALS_API_VERSION);
    let entry = file
        .credentials
        .iter()
        .find(|entry| entry.name == "api-token")
        .expect("the example names the credential every constraint set binds");
    assert_eq!(entry.kind, "bearerToken");
    assert_eq!(entry.scheme, "Bearer");

    let config = config();
    for (id, set) in &config.constraint_sets {
        let Some(http) = &set.constraints.http else {
            continue;
        };
        for host in &http.allowed_hosts {
            assert!(
                entry.destinations.contains(host),
                "{id} allows {host}, which the credential is not bound to"
            );
        }
    }

    // A placeholder, and unmistakably one. A checked-in file that could be mistaken for a live
    // token is a leak waiting for a copy-paste.
    assert!(
        entry.secret.starts_with("github_pat_X") && entry.secret.contains("XXXXXXXX"),
        "the checked-in secret must stay an obvious placeholder"
    );
}

/// The paths the walkthrough tells a reader not to edit.
///
/// `dekopon-brokerd` resolves every relative configured path against the configuration file's own
/// canonicalized directory, which is what lets the example point at the checked-in component and
/// its sibling policy and credentials files without a reader rewriting them first.
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
        config.credentials_path,
        Some(PathBuf::from("broker-credentials.yaml")),
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

    // The attested subject the identity mapping names must sit inside the attestor grant's
    // namespace, or the gateway could name it and the broker would refuse the attestation.
    let identity = config.identities.first().expect("one peer identity");
    let grant = identity.attestor.as_ref().expect("the gateway may attest");
    let mapping = config.identity_mappings.first().expect("one mapping");
    let subject = mapping.subject.canonical();
    assert!(
        grant
            .namespaces
            .iter()
            .any(|namespace| subject == *namespace || subject.starts_with(&format!("{namespace}."))),
        "{subject} sits outside the gateway's attestor namespaces"
    );
    assert_eq!(mapping.principal.as_str(), PRINCIPAL);
    assert_eq!(identity.principal.as_str(), GATEWAY);
}
