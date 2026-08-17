//! The checked-in example catalogs are load-bearing documentation, so they are held to the same
//! parser the CLI and the gateway use.
//!
//! A catalog that stops parsing, or an agent whose capability list drifts away from the point the
//! example is making, breaks a walkthrough a reader is following literally. The contrast asserted
//! here is the one the examples exist to draw: `reviewer` may comment and provably cannot approve,
//! `xaviers-rubber-stamper` may approve, and neither fact is a property of the catalog — it is a
//! property of what the broker was configured to permit.

use std::path::{Path, PathBuf};

use dekopon_capability::{EffectKind, Idempotency};
use dekopon_config::LocalCatalog;
use dekopon_core::RiskLevel;

fn example(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples")
        .join(relative)
}

fn load(path: &Path) -> LocalCatalog {
    LocalCatalog::load(path).unwrap_or_else(|error| panic!("{} loads: {error}", path.display()))
}

#[test]
fn the_local_reviewer_example_has_no_approval_authority() {
    let catalog = load(&example("local/dekopon.yaml"));
    let reviewer = catalog
        .agent(&"reviewer".parse().expect("valid agent id"))
        .expect("the reviewer agent exists");

    assert!(reviewer.spec.enabled);
    let capabilities = reviewer
        .spec
        .capabilities
        .iter()
        .map(|capability| capability.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        capabilities,
        vec!["github.pull-request.read", "github.pull-request.comment"],
        "the reviewer deliberately holds no approval capability"
    );
}

#[test]
fn the_rubber_stamper_example_matches_the_gh_provider_manifest() {
    let catalog = load(&example("rubber-stamper/dekopon.yaml"));

    let agent = catalog
        .agent(&"xaviers-rubber-stamper".parse().expect("valid agent id"))
        .expect("the rubber-stamper agent exists");
    assert!(agent.spec.enabled, "a disabled agent routes to nothing");
    assert_eq!(agent.spec.model_class.as_deref(), Some("reasoning"));
    assert!(
        agent
            .spec
            .instructions
            .as_ref()
            .is_some_and(|instructions| !instructions.trim().is_empty()),
        "the standing orders are the whole difference between this agent and a rubber stamp"
    );
    let capabilities = agent
        .spec
        .capabilities
        .iter()
        .map(|capability| capability.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        capabilities,
        vec![
            "gh.content.read",
            "gh.pull-request.list",
            "gh.pull-request.read",
            "gh.pull-request.files",
            "gh.pull-request.approve",
        ],
        "the catalog surface must stay the Tier-1 review slice the broker constrains"
    );

    // The classification a reader compares against the manifest and against broker.yaml. The
    // broker refuses to start when a constraint set disagrees with the manifest; nothing but this
    // test stops the catalog from disagreeing with both.
    let expected = [
        (
            "gh.content.read",
            EffectKind::ReadOnly,
            RiskLevel::Low,
            Idempotency::Idempotent,
        ),
        (
            "gh.pull-request.list",
            EffectKind::ReadOnly,
            RiskLevel::Low,
            Idempotency::Idempotent,
        ),
        (
            "gh.pull-request.read",
            EffectKind::ReadOnly,
            RiskLevel::Low,
            Idempotency::Idempotent,
        ),
        (
            "gh.pull-request.files",
            EffectKind::ReadOnly,
            RiskLevel::Low,
            Idempotency::Idempotent,
        ),
        (
            "gh.pull-request.approve",
            EffectKind::ExternalWrite,
            RiskLevel::High,
            Idempotency::Conditional,
        ),
    ];
    for (name, effect, risk, idempotency) in expected {
        let capability = catalog
            .capability(&name.parse().expect("valid capability id"))
            .unwrap_or_else(|| panic!("{name} is declared"));
        assert_eq!(capability.spec.effect, effect, "{name} effect");
        assert_eq!(capability.spec.risk, risk, "{name} risk");
        assert_eq!(
            capability.spec.idempotency, idempotency,
            "{name} idempotency"
        );
        assert_eq!(
            capability.spec.provider.as_str(),
            "gh",
            "{name} routes to the gh provider"
        );
        assert!(
            !capability.spec.permissions.is_empty(),
            "{name} declares the provider permissions it needs"
        );
    }

    let provider = catalog
        .provider(&"gh".parse().expect("valid provider id"))
        .expect("the gh provider is declared");
    assert_eq!(provider.spec.provider_type, "github");
    // Symbolic, and the same symbolic name the broker's constraint sets bind. The value lives in
    // the broker's credentials file and nowhere else.
    assert_eq!(provider.spec.credential_ref, "github-pat");
}
