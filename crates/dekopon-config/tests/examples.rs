//! The checked-in example catalogs are load-bearing documentation, so they are held to the same
//! parser the CLI and gateway use.
//!
//! A catalog that stops parsing, or an agent whose capability list drifts from the workflow the
//! example promises, breaks instructions a reader follows literally. Both review examples may
//! propose a comment and neither may approve or merge; the end-to-end example additionally proves
//! that its complete surface agrees with the broker configuration and the provider manifest.

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
fn the_local_reviewer_example_has_comment_but_no_approval_authority() {
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
fn the_conditional_write_example_matches_the_http_probe_provider_manifest() {
    let catalog = load(&example("conditional-write/dekopon.yaml"));

    let agent = catalog
        .agent(
            &"xaviers-conditional-writer"
                .parse()
                .expect("valid agent id"),
        )
        .expect("the conditional writer agent exists");
    assert!(agent.spec.enabled, "a disabled agent routes to nothing");
    assert_eq!(agent.spec.model_class.as_deref(), Some("reasoning"));
    assert!(
        agent
            .spec
            .instructions
            .as_ref()
            .is_some_and(|instructions| !instructions.trim().is_empty()),
        "write behavior must be explicit standing orders"
    );
    let capabilities = agent
        .spec
        .capabilities
        .iter()
        .map(|capability| capability.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        capabilities,
        vec!["http-probe.fetch", "http-probe.conditional-write"],
        "the catalog surface must stay the slice the broker constrains"
    );
    assert!(
        !capabilities.contains(&"http-probe.purge"),
        "the manifest exposes more than this deployment grants, and it must stay that way"
    );

    // The classification a reader compares against the manifest and broker.yaml. The broker
    // refuses startup when a constraint set disagrees with the manifest; this test keeps the
    // unprivileged catalog from disagreeing with both.
    let expected = [
        (
            "http-probe.fetch",
            EffectKind::ReadOnly,
            RiskLevel::Low,
            Idempotency::Idempotent,
        ),
        (
            "http-probe.conditional-write",
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
            "http-probe",
            "{name} routes to the probe provider"
        );
        assert!(
            !capability.spec.permissions.is_empty(),
            "{name} declares the provider permissions it needs"
        );
    }

    let provider = catalog
        .provider(&"http-probe".parse().expect("valid provider id"))
        .expect("the probe provider is declared");
    assert_eq!(provider.spec.provider_type, "http");
    // Symbolic, and the same name the broker's constraint sets bind. The value lives in the
    // broker's credentials file and nowhere else.
    assert_eq!(provider.spec.credential_ref, "api-token");
}
