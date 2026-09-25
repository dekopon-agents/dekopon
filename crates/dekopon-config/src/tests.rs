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
        standalone_agent("zebra"),
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
    let document = standalone_agent("reviewer");
    let input = format!("{document}---\n{document}");
    let error =
        LocalCatalog::from_str("duplicate.yaml", &input).expect_err("duplicate agent must fail");

    assert!(matches!(
        problems(&error),
        [CatalogProblem::DuplicateResource { .. }]
    ));
    assert!(error.to_string().contains("first declared"));
}

#[test]
fn every_unsupported_kind_is_reported_by_name() {
    let input = r#"apiVersion: dekopon.dev/v1alpha1
kind: Provider
metadata:
  name: github
spec:
  description: Test provider
---
apiVersion: dekopon.dev/v1alpha1
kind: Capability
metadata:
  name: github.pull-request.read
spec:
  description: Test capability
"#;
    let error = LocalCatalog::from_str("unsupported.yaml", input)
        .expect_err("a catalog naming unsupported kinds must fail");

    assert!(
        matches!(
            problems(&error),
            [
                CatalogProblem::UnsupportedKind { kind: first, .. },
                CatalogProblem::UnsupportedKind { kind: second, .. }
            ] if first == "Provider" && second == "Capability"
        ),
        "{error}"
    );
    let rendered = error.to_string();
    assert!(
        rendered.contains(r#"unsupported resource kind "Provider""#),
        "{rendered}"
    );
    assert!(
        rendered.contains(r#"unsupported resource kind "Capability""#),
        "{rendered}"
    );
}

#[test]
fn rejects_unknown_fields() {
    let input = standalone_agent("reviewer").replacen(
        "description: Test agent",
        "description: Test agent\n  unknownSetting: true",
        1,
    );
    let error =
        LocalCatalog::from_str("unknown.yaml", &input).expect_err("unknown field must fail");

    assert!(matches!(problems(&error), [CatalogProblem::Decode { .. }]));
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn every_problem_in_a_catalog_is_reported_at_once() {
    let input = r#"apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: reviewer
spec:
  description: Test agent
---
apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: reviewer
spec:
  description: Duplicate agent
---
apiVersion: dekopon.dev/v1alpha1
kind: Capability
metadata:
  name: github.pull-request.read
spec:
  description: Test capability
---
apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: Reviewer.Two
spec:
  description: Test agent
"#;
    let error = LocalCatalog::from_str("many.yaml", input).expect_err("three problems must fail");
    let rendered = error.to_string();

    assert!(
        rendered.contains("3 validation problems found:"),
        "{rendered}"
    );
    assert!(
        rendered
            .contains(r#"duplicate Agent "reviewer" at document 2; first declared at document 1"#),
        "{rendered}"
    );
    assert!(
        rendered.contains(r#"document 3: unsupported resource kind "Capability""#),
        "{rendered}"
    );
    assert!(
        rendered.contains(r#"invalid Agent name "Reviewer.Two""#),
        "{rendered}"
    );
}

#[test]
fn a_future_api_version_gets_the_dedicated_message() {
    let input = standalone_agent("reviewer").replace("v1alpha1", "v1alpha2");
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

#[test]
fn an_unreadable_candidate_fails_instead_of_falling_through() {
    let root = tempdir().expect("temporary directory");
    let xdg = root.path().join("xdg");
    let home = root.path().join("home");
    let current = root.path().join("project");

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
    fs::write(path, standalone_agent("reviewer")).expect("fixture config");
}

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

#[test]
fn every_unmountable_skill_is_reported_in_one_refusal() {
    let root = tempdir().expect("temporary directory");
    write_skill(root.path(), "pull-request-review");
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

#[test]
fn a_catalog_directory_loads_every_file_and_refuses_an_agent_named_twice() {
    let root = tempdir().expect("temporary directory");
    fs::write(root.path().join("alpha.yaml"), standalone_agent("alpha")).expect("first agent file");
    fs::write(root.path().join("bravo.yaml"), standalone_agent("bravo"))
        .expect("second agent file");

    let catalog = LocalCatalog::load(root.path()).expect("a directory of distinct agents loads");
    let names = catalog
        .agents()
        .map(|agent| agent.metadata.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["alpha", "bravo"]);

    fs::write(root.path().join("charlie.yaml"), standalone_agent("alpha"))
        .expect("duplicate agent file");
    let error =
        LocalCatalog::load(root.path()).expect_err("an agent named in two files is a duplicate");

    assert!(
        matches!(problems(&error), [CatalogProblem::DuplicateResource { .. }]),
        "{error}"
    );
    assert!(error.to_string().contains("first declared"), "{error}");
}

#[test]
fn instructions_file_is_read_beside_the_catalog_and_excludes_inline_instructions() {
    let root = tempdir().expect("temporary directory");
    fs::write(
        root.path().join("instructions.md"),
        "Read the pull request and comment once.\n",
    )
    .expect("instructions file");
    let catalog_path = root.path().join("dekopon.yaml");
    fs::write(
        &catalog_path,
        "apiVersion: dekopon.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: reviewer\nspec:\n  \
         description: Test agent\n  instructionsFile: instructions.md\nstatus: Ready\n",
    )
    .expect("catalog naming instructionsFile");

    let catalog =
        LocalCatalog::load(&catalog_path).expect("instructionsFile reads into instructions");
    let reviewer = catalog
        .agent(&"reviewer".parse().expect("valid agent id"))
        .expect("the reviewer agent exists");
    assert_eq!(
        reviewer.spec.instructions.as_deref(),
        Some("Read the pull request and comment once.\n")
    );

    fs::write(
        &catalog_path,
        "apiVersion: dekopon.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: reviewer\nspec:\n  \
         description: Test agent\n  instructions: Inline orders.\n  instructionsFile: instructions.md\n\
         status: Ready\n",
    )
    .expect("catalog naming both instructions and instructionsFile");

    let error = LocalCatalog::load(&catalog_path)
        .expect_err("an agent naming both instructions and instructionsFile must fail");
    assert!(
        matches!(
            problems(&error),
            [CatalogProblem::InstructionsTwice { agent }] if agent == "reviewer"
        ),
        "{error}"
    );
}

#[test]
fn instructions_file_is_read_through_a_config_map_symlink() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let data = directory.path().join("..data");
    std::fs::create_dir(&data).expect("create ..data");
    std::fs::write(data.join("reviewer.md"), "Review carefully.").expect("write instructions");
    std::os::unix::fs::symlink("..data/reviewer.md", directory.path().join("reviewer.md"))
        .expect("link instructions like a ConfigMap volume");
    std::fs::write(
        directory.path().join("reviewer.yaml"),
        "apiVersion: dekopon.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: reviewer\nspec:\n  description: Reviews\n  instructionsFile: reviewer.md\n",
    )
    .expect("write agent");
    let catalog =
        LocalCatalog::load(directory.path()).expect("a symlinked instructions file loads");
    let agent = catalog
        .agent(&"reviewer".parse().expect("agent id"))
        .expect("reviewer is loaded");
    assert_eq!(
        agent.spec.instructions.as_deref(),
        Some("Review carefully.")
    );
}
