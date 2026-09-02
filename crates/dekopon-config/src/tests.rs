use std::{fs, path::Path};

use tempfile::tempdir;

use super::{CatalogProblem, ConfigError, DiscoveryContext, LocalCatalog};

#[track_caller]
fn problems(error: &ConfigError) -> &[CatalogProblem] {
    match error {
        ConfigError::Invalid { problems, .. } => problems,
        other => panic!("expected a validation report, got {other}"),
    }
}

fn valid_documents(agent_name: &str) -> String {
    format!(
        r#"apiVersion: dekopon.dev/v1alpha1
kind: Provider
metadata:
  name: github
spec:
  description: Test provider
  type: github
  credentialRef: test-credential
---
apiVersion: dekopon.dev/v1alpha1
kind: Capability
metadata:
  name: github.pull-request.read
spec:
  description: Test capability
  provider: github
  effect: read-only
  risk: Low
  idempotency: idempotent
---
apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: {agent_name}
spec:
  description: Test agent
  capabilities:
    - github.pull-request.read
  providers:
    - github
status: Ready
"#
    )
}

fn standalone_agent(name: &str) -> String {
    format!(
        r#"apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: {name}
spec:
  description: Test agent
status: Ready
"#
    )
}

#[test]
fn loads_multiple_documents_and_sorts_resources() {
    let input = format!(
        "{}---\n{}",
        valid_documents("zebra"),
        standalone_agent("alpha")
    );
    let file = tempfile::NamedTempFile::new().expect("temporary config");
    fs::write(file.path(), input).expect("fixture config");

    let catalog = LocalCatalog::load(file.path()).expect("valid catalog");
    let names = catalog
        .agents()
        .map(|agent| agent.metadata.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(names, ["alpha", "zebra"]);
    assert_eq!(catalog.capabilities().len(), 1);
    assert_eq!(catalog.providers().len(), 1);
}

#[test]
fn accepts_json_as_a_yaml_subset() {
    let input = serde_json::json!({
        "apiVersion": "dekopon.dev/v1alpha1",
        "kind": "Agent",
        "metadata": {"name": "reviewer"},
        "spec": {"description": "Test agent"},
        "status": "Ready"
    });
    let input = serde_json::to_string_pretty(&input).expect("fixture serializes as JSON");

    let catalog = LocalCatalog::from_str("config.json", &input).expect("valid JSON input");
    assert_eq!(catalog.agents().len(), 1);
}

#[test]
fn rejects_duplicate_resources() {
    let document = r#"apiVersion: dekopon.dev/v1alpha1
kind: Provider
metadata:
  name: github
spec:
  description: Test provider
  type: github
  credentialRef: test-credential
"#;
    let input = format!("{document}---\n{document}");
    let error =
        LocalCatalog::from_str("duplicate.yaml", &input).expect_err("duplicate provider must fail");

    assert!(matches!(
        problems(&error),
        [CatalogProblem::DuplicateResource { .. }]
    ));
    assert!(error.to_string().contains("first declared"));
}

#[test]
fn rejects_missing_capability_references() {
    let input = r#"apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: reviewer
spec:
  description: Test agent
  capabilities:
    - github.pull-request.read
"#;
    let error =
        LocalCatalog::from_str("missing.yaml", input).expect_err("missing capability must fail");

    assert!(matches!(
        problems(&error),
        [CatalogProblem::MissingCapability { .. }]
    ));
}

#[test]
fn rejects_missing_provider_references() {
    let input = r#"apiVersion: dekopon.dev/v1alpha1
kind: Capability
metadata:
  name: github.pull-request.read
spec:
  description: Test capability
  provider: github
  effect: read-only
  risk: Low
  idempotency: idempotent
"#;
    let error =
        LocalCatalog::from_str("missing.yaml", input).expect_err("missing provider must fail");

    assert!(matches!(
        problems(&error),
        [CatalogProblem::MissingProvider { .. }]
    ));
}

#[test]
fn rejects_unknown_fields() {
    let input = valid_documents("reviewer").replacen(
        "description: Test agent",
        "description: Test agent\n  unknownSetting: true",
        1,
    );
    let error =
        LocalCatalog::from_str("unknown.yaml", &input).expect_err("unknown field must fail");

    assert!(matches!(problems(&error), [CatalogProblem::Decode { .. }]));
    assert!(error.to_string().contains("unknown field"));
}

/// The requirement the whole report shape exists for: one `dekopon validate` run, one fix pass.
#[test]
fn every_problem_in_a_catalog_is_reported_at_once() {
    let input = r#"apiVersion: dekopon.dev/v1alpha1
kind: Provider
metadata:
  name: github
spec:
  description: Test provider
  type: github
  credentialRef: test-credential
---
apiVersion: dekopon.dev/v1alpha1
kind: Provider
metadata:
  name: github
spec:
  description: Duplicate provider
  type: github
  credentialRef: test-credential
---
apiVersion: dekopon.dev/v1alpha1
kind: Capability
metadata:
  name: github.pull-request.read
spec:
  description: Test capability
  provider: elsewhere
  effect: read-only
  risk: Low
  idempotency: idempotent
---
apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: reviewer
spec:
  description: Test agent
  capabilities:
    - github.pull-request.read
    - github.missing
  providers:
    - github
status: Ready
"#;
    let error = LocalCatalog::from_str("many.yaml", input).expect_err("four problems must fail");
    let rendered = error.to_string();

    assert!(
        rendered.contains("4 validation problems found:"),
        "{rendered}"
    );
    assert!(
        rendered
            .contains(r#"duplicate Provider "github" at document 2; first declared at document 1"#),
        "{rendered}"
    );
    assert!(
        rendered.contains(r#"agent "reviewer" references missing capability "github.missing""#),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            r#"agent "reviewer" omits provider "elsewhere", required by capability "github.pull-request.read", from spec.providers"#
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            r#"capability "github.pull-request.read" references missing provider "elsewhere""#
        ),
        "{rendered}"
    );
}

/// A resource that never entered the catalog would otherwise be blamed twice: once for the real
/// failure, once as a reference nothing declares.
#[test]
fn a_dropped_resource_is_reported_without_downstream_reference_noise() {
    let input = r#"apiVersion: dekopon.dev/v1alpha1
kind: Capability
metadata:
  name: Github.Pull-Request.Read
spec:
  description: Test capability
  provider: github
  effect: read-only
  risk: Low
  idempotency: idempotent
---
apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: reviewer
spec:
  description: Test agent
  capabilities:
    - github.pull-request.read
"#;
    let error = LocalCatalog::from_str("dropped.yaml", input).expect_err("invalid name must fail");

    assert!(
        matches!(problems(&error), [CatalogProblem::InvalidName { .. }]),
        "{error}"
    );
}

/// The friendly message exists for exactly this input, and only a pre-decode read reaches it.
#[test]
fn a_future_api_version_gets_the_dedicated_message() {
    let input = valid_documents("reviewer").replace("v1alpha1", "v1alpha2");
    let error = LocalCatalog::from_str("future.yaml", &input).expect_err("v1alpha2 must fail");

    assert!(
        error
            .to_string()
            .contains(r#"unsupported API version "dekopon.dev/v1alpha2""#),
        "{error}"
    );
    assert!(
        problems(&error)
            .iter()
            .all(|problem| matches!(problem, CatalogProblem::UnsupportedApiVersion { .. })),
        "{error}"
    );
}

#[test]
fn agent_providers_must_match_the_providers_its_capabilities_route_to() {
    let unlisted = valid_documents("reviewer").replace("  providers:\n    - github\n", "");
    let error = LocalCatalog::from_str("unlisted.yaml", &unlisted)
        .expect_err("an omitted provider must fail");
    assert!(
        matches!(
            problems(&error),
            [CatalogProblem::UnlistedAgentProvider { .. }]
        ),
        "{error}"
    );

    let unreachable = format!(
        "{}---\n{}",
        valid_documents("reviewer"),
        r#"apiVersion: dekopon.dev/v1alpha1
kind: Provider
metadata:
  name: gitlab
spec:
  description: Unused provider
  type: gitlab
  credentialRef: test-credential
"#
    )
    .replace(
        "  providers:\n    - github\n",
        "  providers:\n    - github\n    - gitlab\n",
    );
    let error = LocalCatalog::from_str("unreachable.yaml", &unreachable)
        .expect_err("a provider no capability routes to must fail");
    assert!(
        matches!(
            problems(&error),
            [CatalogProblem::UnreachableAgentProvider { .. }]
        ),
        "{error}"
    );
}

#[test]
fn discovery_uses_documented_precedence() {
    let root = tempdir().expect("temporary directory");
    let explicit = root.path().join("explicit.yaml");
    let environment = root.path().join("environment.yaml");
    let xdg = root.path().join("xdg");
    let home = root.path().join("home");
    let current = root.path().join("project");

    write_config(&xdg.join("dekopon/config.yaml"));
    write_config(&home.join(".config/dekopon/config.yaml"));
    write_config(&current.join("dekopon.yaml"));

    let defaults = DiscoveryContext::new(
        None,
        None,
        Some(xdg.clone()),
        Some(home.clone()),
        current.clone(),
    );
    assert_eq!(
        defaults.resolve().expect("XDG config exists"),
        xdg.join("dekopon/config.yaml")
    );

    let from_environment = DiscoveryContext::new(
        None,
        Some(environment.clone()),
        Some(xdg),
        Some(home),
        current,
    );
    assert_eq!(
        from_environment
            .resolve()
            .expect("environment is authoritative"),
        environment
    );

    let from_explicit = DiscoveryContext::new(
        Some(explicit.clone()),
        Some(root.path().join("other.yaml")),
        None,
        None,
        root.path().to_path_buf(),
    );
    assert_eq!(
        from_explicit.resolve().expect("explicit is authoritative"),
        explicit
    );
}

/// A candidate that cannot be examined may be hiding the operator's real config, so it must not
/// silently hand the catalog to a lower-precedence file.
#[test]
fn an_unreadable_candidate_fails_instead_of_falling_through() {
    let root = tempdir().expect("temporary directory");
    let xdg = root.path().join("xdg");
    let home = root.path().join("home");
    let current = root.path().join("project");

    // A regular file where a directory belongs makes the candidate below it ENOTDIR rather than
    // absent, which is what the fall-through used to swallow.
    fs::write(&xdg, "not a directory").expect("fixture file");
    write_config(&home.join(".config/dekopon/config.yaml"));
    write_config(&current.join("dekopon.yaml"));

    let context = DiscoveryContext::new(None, None, Some(xdg.clone()), Some(home), current);
    let error = context
        .resolve()
        .expect_err("an unexaminable candidate must fail");

    assert!(matches!(error, ConfigError::Candidate { .. }), "{error}");
    assert!(
        error
            .to_string()
            .contains(&xdg.join("dekopon/config.yaml").display().to_string()),
        "{error}"
    );
}

fn write_config(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture directory");
    }
    fs::write(path, valid_documents("reviewer")).expect("fixture config");
}

/// One well-formed skill directory beside a catalog.
fn write_skill(root: &Path, name: &str) {
    let directory = root.join("skills").join(name);
    fs::create_dir_all(directory.join("references")).expect("skill directory");
    fs::write(
        directory.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: Use when asked about {name}.\n---\n# {name}\n\nDo the thing.\n"
        ),
    )
    .expect("skill file");
    fs::write(directory.join("references/notes.md"), "notes\n").expect("skill resource");
}

fn agent_with_skills(skills: &[&str]) -> String {
    let mounted = skills
        .iter()
        .map(|path| format!("    - {path}\n"))
        .collect::<String>();
    format!(
        r#"apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: reviewer
spec:
  description: Test agent
  skills:
{mounted}status: Ready
"#
    )
}

/// Skills resolve against the catalog file's directory and are read whole at load, so a session
/// never opens a file to show a model one.
#[test]
fn agent_skills_are_loaded_relative_to_the_catalog() {
    let root = tempdir().expect("temporary directory");
    write_skill(root.path(), "pull-request-review");
    write_skill(root.path(), "release-notes");
    let catalog_path = root.path().join("dekopon.yaml");
    fs::write(
        &catalog_path,
        agent_with_skills(&["skills/pull-request-review", "skills/release-notes"]),
    )
    .expect("catalog");

    let catalog = LocalCatalog::load(&catalog_path).expect("catalog with skills loads");
    let reviewer = "reviewer".parse().expect("valid agent id");
    let skills = catalog.agent_skills(&reviewer);

    assert_eq!(
        skills
            .iter()
            .map(|skill| skill.name().as_str())
            .collect::<Vec<_>>(),
        ["pull-request-review", "release-notes"],
        "mount order is the authored order"
    );
    assert_eq!(
        skills[0].description(),
        "Use when asked about pull-request-review."
    );
    assert_eq!(skills[0].resources()[0].path, "references/notes.md");
    let absent = "nobody".parse().expect("valid agent id");
    assert!(catalog.agent_skills(&absent).is_empty());
}

/// Every broken skill is reported, and one bad directory does not hide a second problem.
#[test]
fn every_unmountable_skill_is_reported_in_one_refusal() {
    let root = tempdir().expect("temporary directory");
    write_skill(root.path(), "pull-request-review");
    // A second directory carrying the first skill's name.
    let copy = root.path().join("elsewhere").join("pull-request-review");
    fs::create_dir_all(&copy).expect("copy directory");
    fs::copy(
        root.path().join("skills/pull-request-review/SKILL.md"),
        copy.join("SKILL.md"),
    )
    .expect("copy skill");
    let catalog_path = root.path().join("dekopon.yaml");
    fs::write(
        &catalog_path,
        agent_with_skills(&[
            "skills/pull-request-review",
            "skills/absent",
            "elsewhere/pull-request-review",
        ]),
    )
    .expect("catalog");

    let error = LocalCatalog::load(&catalog_path).expect_err("broken skills refuse the catalog");
    let reported = problems(&error);

    assert_eq!(reported.len(), 2, "{error}");
    assert!(
        matches!(&reported[0], CatalogProblem::Skill { agent, path, .. }
            if agent == "reviewer" && path == "skills/absent"),
        "{error}"
    );
    assert!(
        matches!(&reported[1], CatalogProblem::DuplicateSkill { name, first, duplicate, .. }
            if name == "pull-request-review"
                && first == "skills/pull-request-review"
                && duplicate == "elsewhere/pull-request-review"),
        "{error}"
    );
    assert!(error.to_string().contains("could not be loaded"), "{error}");
}
