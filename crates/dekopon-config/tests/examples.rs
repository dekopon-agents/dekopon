//! The checked-in example catalogs are load-bearing documentation, so they are held to the same
//! parser the gateway uses.
//!
//! A catalog that stops parsing, or an agent whose capability list drifts from the workflow the
//! example promises, breaks instructions a reader follows literally. Both review examples may
//! propose a comment and neither may approve or merge.

use std::path::{Path, PathBuf};

use dekopon_config::LocalCatalog;

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
    let catalog = load(&example("catalog/dekopon.yaml"));
    let reviewer = catalog
        .agent(&"reviewer".parse().expect("valid agent id"))
        .expect("the reviewer agent exists");

    assert!(reviewer.spec.enabled);
    let skills = catalog.agent_skills(&"reviewer".parse().expect("valid agent id"));
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name().as_str(), "pull-request-review");
    assert_eq!(skills[0].resources().len(), 1);
    assert_eq!(
        skills[0].resources()[0].path,
        "references/risk-checklist.md"
    );
    assert!(!skills[0].body().is_empty());
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
fn the_conditional_write_example_declares_the_slice_the_broker_constrains() {
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
    let instructions = agent
        .spec
        .instructions
        .as_deref()
        .expect("write behavior must be explicit standing orders");
    assert!(
        !instructions.trim().is_empty(),
        "write behavior must be explicit standing orders"
    );
    for command in ["httpprobe fetch", "httpprobe conditional-write"] {
        assert!(
            instructions.contains(command),
            "the standing orders must name `{command}`, the command the provider answers"
        );
    }
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
}
