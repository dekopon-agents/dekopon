use std::{
    path::PathBuf,
    process::{Command, Output},
};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dekopon"))
}

fn example_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples/local/dekopon.yaml")
}

fn run_example(arguments: &[&str]) -> Output {
    let mut command = binary();
    command.arg("--config").arg(example_path()).args(arguments);
    command.output().expect("CLI process starts")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

#[test]
fn required_example_commands_are_operational() {
    let cases = [
        (vec!["get", "agents"], "reviewer"),
        (vec!["get", "agents", "-o", "wide"], "PROVIDERS"),
        (
            vec!["get", "agent", "reviewer", "-o", "yaml"],
            "kind: Agent",
        ),
        (vec!["get", "capabilities"], "github.pull-request.comment"),
        (vec!["get", "providers"], "github"),
        (vec!["describe", "agent", "reviewer"], "Capabilities:"),
        (vec!["validate"], "configuration valid"),
        (vec!["config", "view", "-o", "json"], "\"agents\""),
    ];

    for (arguments, expected) in cases {
        let output = run_example(&arguments);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{arguments:?} failed: {}",
            stderr(&output)
        );
        assert!(
            stdout(&output).contains(expected),
            "{arguments:?} did not contain {expected:?}: {}",
            stdout(&output)
        );
    }
}

#[test]
fn lists_agents_in_every_output_format() {
    let table = run_example(&["get", "agents", "-o", "table"]);
    assert_eq!(table.status.code(), Some(0));
    assert!(stdout(&table).starts_with("NAME"));
    assert!(stdout(&table).contains("Disabled"));

    let wide = run_example(&["get", "agents", "-o", "wide"]);
    assert!(stdout(&wide).contains("MODEL"));
    assert!(stdout(&wide).contains("POLICY"));

    let json = run_example(&["get", "agents", "-o", "json"]);
    let value: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("JSON output parses");
    assert_eq!(value["kind"], "AgentList");
    assert_eq!(value["items"].as_array().map(Vec::len), Some(2));

    let yaml = run_example(&["get", "agents", "-o", "yaml"]);
    assert!(stdout(&yaml).contains("kind: AgentList"));

    let names = run_example(&["get", "agents", "-o", "name"]);
    assert_eq!(stdout(&names), "agent/reviewer\nagent/snooper\n");
}

#[test]
fn gets_each_singular_resource_shape() {
    let agent = run_example(&["get", "agent", "reviewer", "-o", "json"]);
    let agent_value: serde_json::Value =
        serde_json::from_slice(&agent.stdout).expect("agent JSON parses");
    assert_eq!(agent_value["kind"], "Agent");

    let capability = run_example(&[
        "get",
        "capability",
        "github.pull-request.comment",
        "-o",
        "yaml",
    ]);
    assert_eq!(capability.status.code(), Some(0));
    assert!(stdout(&capability).contains("effect: external-write"));

    let provider = run_example(&["get", "provider", "github", "-o", "name"]);
    assert_eq!(stdout(&provider), "provider/github\n");
}

#[test]
fn example_encodes_the_review_authority_boundary() {
    let description = run_example(&["describe", "agent", "reviewer"]);
    let rendered = stdout(&description);

    assert!(rendered.contains("github.pull-request.read"));
    assert!(rendered.contains("github.pull-request.comment [external-write"));
    assert!(!rendered.contains("github.pull-request.approve"));
}

#[test]
fn config_view_is_canonical_and_machine_readable() {
    let output = run_example(&["config", "view", "--output", "json"]);
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("config JSON parses");

    assert_eq!(value["apiVersion"], "dekopon.dev/v1alpha1");
    assert_eq!(value["agents"].as_array().map(Vec::len), Some(2));
    assert_eq!(value["capabilities"].as_array().map(Vec::len), Some(3));
    assert_eq!(value["providers"].as_array().map(Vec::len), Some(1));
}

#[test]
fn missing_resource_exits_with_three() {
    let output = run_example(&["get", "agent", "absent"]);

    assert_eq!(output.status.code(), Some(3));
    assert!(stderr(&output).contains("agent \"absent\" not found"));
}

#[test]
fn invalid_configuration_exits_with_one() {
    let file = tempfile::NamedTempFile::new().expect("temporary config");
    std::fs::write(
        file.path(),
        r#"
apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: reviewer
spec:
  description: Invalid fixture
  capabilities:
    - github.missing
"#,
    )
    .expect("write fixture");
    let output = binary()
        .arg("--config")
        .arg(file.path())
        .arg("validate")
        .output()
        .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("references missing capability"));
}

#[test]
fn usage_errors_exit_with_two() {
    let output = binary()
        .args(["get", "agent", "not valid"])
        .output()
        .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("invalid character"));
}

#[test]
fn top_level_and_nested_help_are_generated() {
    let top = binary().arg("--help").output().expect("CLI process starts");
    assert_eq!(top.status.code(), Some(0));
    assert!(stdout(&top).contains("Usage: dekopon [OPTIONS] <COMMAND>"));
    assert!(stdout(&top).contains("--no-color"));

    let nested = binary()
        .args(["get", "--help"])
        .output()
        .expect("CLI process starts");
    assert_eq!(nested.status.code(), Some(0));
    assert!(stdout(&nested).contains("agents"));
    assert!(stdout(&nested).contains("capabilities"));
}

#[test]
fn chatgpt_auth_status_does_not_require_configuration() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = directory.path().join("missing-auth.json");
    let output = binary()
        .current_dir(directory.path())
        .args(["auth", "chatgpt", "status", "--auth-file"])
        .arg(&auth_file)
        .args(["--output", "json"])
        .output()
        .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let status: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("auth status JSON parses");
    assert_eq!(status["account"], "chatgpt");
    assert_eq!(status["signedIn"], false);
    assert_eq!(status["credentialFile"], auth_file.display().to_string());
}

#[test]
fn version_does_not_require_configuration() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let output = binary()
        .current_dir(directory.path())
        .arg("version")
        .output()
        .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout(&output),
        format!("dekopon {}\n", env!("CARGO_PKG_VERSION"))
    );
}
