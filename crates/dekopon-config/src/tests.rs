use std::{fs, path::Path};

use tempfile::tempdir;

use super::{ConfigError, DiscoveryContext, LocalCatalog};

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

    assert!(matches!(error, ConfigError::DuplicateResource { .. }));
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

    assert!(matches!(error, ConfigError::MissingCapability { .. }));
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

    assert!(matches!(error, ConfigError::MissingProvider { .. }));
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

    assert!(matches!(error, ConfigError::Decode { .. }));
    assert!(error.to_string().contains("unknown field"));
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

fn write_config(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture directory");
    }
    fs::write(path, valid_documents("reviewer")).expect("fixture config");
}
