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
fn a_bare_invocation_off_a_terminal_stays_a_usage_error() {
    // The console needs a terminal to draw on and a terminal to read keys from. A test harness has
    // neither, and neither does a script — which is the case that must never become a process
    // hanging on raw-mode input that will never arrive.
    let output = binary().output().expect("CLI process starts");

    assert_eq!(
        output.status.code(),
        Some(2),
        "exit 2 is the documented usage code and predates the console"
    );
    assert!(
        stderr(&output).contains("subcommand is required"),
        "the refusal must say what was missing: {}",
        stderr(&output)
    );
}

#[test]
fn the_console_refuses_without_a_subject_rather_than_guessing_one() {
    let output = binary()
        .args(["console"])
        .env_remove("DEKOPON_CONSOLE_SUBJECT")
        .output()
        .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(1));
    let stderr = stderr(&output);
    assert!(stderr.contains("--subject"), "got: {stderr}");
    assert!(stderr.contains("DEKOPON_CONSOLE_SUBJECT"), "got: {stderr}");
}

#[test]
fn the_console_rejects_a_subject_no_service_could_issue() {
    let output = binary()
        .args(["console", "--subject", "dev.console.xavier"])
        .output()
        .expect("CLI process starts");

    // Five services exist and `dev` is not one of them, so this fails at the command line rather
    // than as a broker refusal several steps later.
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn top_level_and_nested_help_are_generated() {
    let top = binary().arg("--help").output().expect("CLI process starts");
    assert_eq!(top.status.code(), Some(0));
    // `[COMMAND]` rather than `<COMMAND>`: a bare `dekopon` on a terminal opens the console, so the
    // subcommand is genuinely optional and the generated usage line says so.
    assert!(stdout(&top).contains("Usage: dekopon [OPTIONS] [COMMAND]"));
    assert!(stdout(&top).contains("--no-color"));
    assert!(stdout(&top).contains("console"));

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

/// One deterministic credential file, written in the field order the export re-serializes, so the
/// exported document is byte-identical to the fixture.
const CREDENTIAL_FIXTURE: &str = concat!(
    r#"{"version":1,"access":"access-token-fixture","refresh":"refresh-token-fixture","#,
    r#""expiresAt":1700000000,"accountId":"acct-fixture"}"#,
    "\n"
);

/// The same document, base64-encoded, as the emitted Secret must carry it.
const CREDENTIAL_FIXTURE_BASE64: &str = concat!(
    "eyJ2ZXJzaW9uIjoxLCJhY2Nlc3MiOiJhY2Nlc3MtdG9rZW4tZml4dHVyZSIsInJlZnJlc2giOiJy",
    "ZWZyZXNoLXRva2VuLWZpeHR1cmUiLCJleHBpcmVzQXQiOjE3MDAwMDAwMDAsImFjY291bnRJZCI6",
    "ImFjY3QtZml4dHVyZSJ9Cg=="
);

fn credential_fixture(directory: &std::path::Path, contents: &str) -> PathBuf {
    let path = directory.join("chatgpt-auth.json");
    std::fs::write(&path, contents).expect("write credential fixture");
    path
}

fn export(auth_file: &std::path::Path, arguments: &[&str]) -> Output {
    binary()
        .args(["auth", "chatgpt", "export", "--no-color", "--auth-file"])
        .arg(auth_file)
        .args(arguments)
        .output()
        .expect("CLI process starts")
}

/// The Secret manifest is applied by `kubectl` and diffed by hand, so its bytes are the contract.
/// The comment header is part of it: a manifest saved to a file outlives the terminal that warned.
#[test]
fn chatgpt_export_emits_an_exact_secret_manifest() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);

    let output = export(&auth_file, &["--expose-credential"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "# Exported by `dekopon auth chatgpt export`. This manifest carries a live ChatGPT access token and\n\
             # a rotating refresh token; base64 here is Kubernetes' encoding for `data`, not encryption.\n\
             #\n\
             # The refresh token rotates: whichever process refreshes next invalidates this copy. Seed it once\n\
             # into a writable directory, never overwrite a newer credential file with it, and re-export after\n\
             # a deliberate rotation.\n\
             apiVersion: v1\n\
             kind: Secret\n\
             metadata:\n  \
               name: dekopon-chatgpt-auth\n  \
               labels:\n    \
                 app.kubernetes.io/component: chatgpt-credential\n    \
                 app.kubernetes.io/managed-by: dekopon-auth-export\n    \
                 app.kubernetes.io/name: dekopon\n\
             type: Opaque\n\
             data:\n  \
               chatgpt-auth.json: {CREDENTIAL_FIXTURE_BASE64}\n"
        )
    );
}

/// `--namespace` is the only shape change the manifest accepts, and it must land in `metadata`
/// rather than anywhere a reader would miss it.
#[test]
fn chatgpt_export_places_the_secret_in_a_namespace() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);

    let output = export(
        &auth_file,
        &[
            "--expose-credential",
            "--namespace",
            "dekopon",
            "--secret-name",
            "chatgpt-seed",
        ],
    );

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("  name: chatgpt-seed\n  namespace: dekopon\n"));
}

/// The raw form is pasted into a password-manager field and later projected back into a file, so
/// it must be exactly the document a login would have written — no wrapper, no re-indentation.
#[test]
fn chatgpt_export_emits_the_exact_credential_document() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);

    let output = export(&auth_file, &["--expose-credential", "--format", "raw"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), CREDENTIAL_FIXTURE);
}

/// Every export must say that the copy it just produced dies at the next refresh, and must say it
/// on standard error so the document stays pipeable.
#[test]
fn chatgpt_export_warns_that_the_exported_copy_rotates() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);

    let output = export(&auth_file, &["--expose-credential", "--format", "raw"]);

    let diagnostics = stderr(&output);
    assert!(diagnostics.contains("rotates"), "{diagnostics}");
    assert!(diagnostics.contains("in the clear"), "{diagnostics}");
    assert!(!stdout(&output).contains("rotates"));
}

/// Printing a credential must be typed out, not defaulted into.
#[test]
fn chatgpt_export_requires_the_credential_acknowledgement() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);

    let output = export(&auth_file, &[]);

    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("--expose-credential"));
    assert!(stdout(&output).is_empty());
}

/// No credential must fail loudly. An empty or half-formed Secret is the failure that survives
/// into a cluster and fails later, somewhere less obvious.
#[test]
fn chatgpt_export_without_a_credential_fails() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = directory.path().join("missing-auth.json");

    let output = export(&auth_file, &["--expose-credential"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("not logged in to ChatGPT"));
    assert!(stdout(&output).is_empty());
}

/// A credential file that is not credential JSON must name the file rather than emit a manifest.
#[test]
fn chatgpt_export_rejects_a_malformed_credential_file() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), "{ not json");

    let output = export(&auth_file, &["--expose-credential"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("could not parse ChatGPT credentials"));
    assert!(stdout(&output).is_empty());
}

/// Valid JSON with empty tokens is the more dangerous malformed case, because it would otherwise
/// produce a structurally perfect Secret carrying nothing.
#[test]
fn chatgpt_export_rejects_an_incomplete_credential_file() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(
        directory.path(),
        r#"{"version":1,"access":"","refresh":"","expiresAt":0,"accountId":""}"#,
    );

    let output = export(&auth_file, &["--expose-credential"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("incomplete"));
    assert!(stdout(&output).is_empty());
}

/// `--quiet` would suppress the document and still exit zero, so a scripted seeding step would
/// store nothing and believe it had succeeded.
#[test]
fn chatgpt_export_refuses_to_be_quiet() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);

    let output = export(&auth_file, &["--expose-credential", "--quiet"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("--quiet"));
}

/// A name the API server would reject must fail before the credential is read, not after it has
/// been printed and piped somewhere.
///
/// The dotted cases are the ones a whole-string character filter lets through: DNS-1123 applies
/// its start/end rule to every label, not just to the first and last character of the name.
#[test]
fn chatgpt_export_rejects_an_invalid_secret_name() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);

    for name in ["Not_A_Name", "a.-b.c", "a-.b", "a..b", "-a", "a-"] {
        let output = export(&auth_file, &["--expose-credential", "--secret-name", name]);

        assert_eq!(output.status.code(), Some(2), "{name}");
        assert!(stdout(&output).is_empty(), "{name}");
    }

    let output = export(
        &auth_file,
        &["--expose-credential", "--secret-name", "a-b.c9.d"],
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout(&output).contains("a-b.c9.d"));
}

/// The command's own help must say that it prints credential material.
#[test]
fn chatgpt_export_help_states_that_it_prints_a_credential() {
    let output = binary()
        .args(["auth", "chatgpt", "export", "--help"])
        .output()
        .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(0));
    let help = stdout(&output);
    assert!(
        help.contains("prints real credential material in the clear"),
        "{help}"
    );
    assert!(help.contains("--expose-credential"), "{help}");
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

#[test]
fn verbose_diagnostics_still_reach_a_redirected_stderr() {
    // The console discards diagnostics only when they would land inside its own frame. A test
    // harness never has a terminal, so this also pins that the discard is conditional rather than
    // a blanket suppression that would silence `-v` everywhere.
    let output = run_example(&["-vv", "get", "agents"]);

    assert_eq!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("loaded validated catalog"),
        "debug diagnostics vanished: {}",
        stderr(&output)
    );
}
