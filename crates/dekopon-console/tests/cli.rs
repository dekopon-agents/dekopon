//! Command-line tests over the real binary.
//!
//! Every one of these runs without a broker, because the flags they pin are resolved before the
//! console connects to anything. They were untested for the whole time this lived inside `dekopon`,
//! which is how `--api-key-env` could have stopped requiring `--endpoint` without anyone noticing.

use std::{path::Path, process::Command};

use tempfile::TempDir;

/// A catalog with one agent, so catalog loading is never the reason a test fails.
const CATALOG: &str = r"apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: reviewer
spec:
  description: A fixture agent
  enabled: true
";

fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dekopon-console"));
    // The subject is the one setting with no default, and it reads the environment. A developer
    // machine that exports it would otherwise change what these tests assert.
    command.env_remove("DEKOPON_CONSOLE_SUBJECT");
    command.env_remove("DEKOPON_BROKER_UID");
    command
}

fn catalog() -> (TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("create a catalog fixture");
    let path = directory.path().join("dekopon.yaml");
    std::fs::write(&path, CATALOG).expect("write the catalog fixture");
    (directory, path)
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn help_lists_every_flag_a_deployment_might_need() {
    let output = binary()
        .arg("--help")
        .output()
        .expect("the console binary starts");

    assert_eq!(output.status.code(), Some(0));
    let help = stdout(&output);
    for flag in [
        "--model",
        "--endpoint",
        "--api-key-env",
        "--auth-file",
        "--max-steps",
        "--max-capability-calls",
        "--subject",
        "--agent",
        "--profile",
        "--profiles",
        "--idle",
        "--telemetry",
        "--socket",
        "--server-uid",
        "--structured",
    ] {
        assert!(help.contains(flag), "{flag} is undocumented: {help}");
    }
}

#[test]
fn structured_requires_both_streams_non_tty_and_reaches_catalog() {
    let output = binary()
        .args([
            "--structured",
            "--subject",
            "slack.t0123abc.u9xyz",
            "--agent",
            "reviewer",
            "--config",
            "/nonexistent/catalog.yaml",
        ])
        .output()
        .expect("binary starts");
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("catalog") || stderr(&output).contains("No such file"),
        "{}",
        stderr(&output)
    );
    assert!(!stderr(&output).contains("TTY"));
}

#[test]
fn default_non_tty_refusal_and_structured_idle_conflict() {
    let (_dir, config) = catalog();
    let output = binary()
        .args(["--subject", "slack.t0123abc.u9xyz", "--config"])
        .arg(config)
        .output()
        .unwrap();
    assert!(stderr(&output).contains("TTY"));
    let output = binary().args(["--structured", "--idle"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn an_api_key_variable_without_an_endpoint_is_a_usage_error() {
    // On its own it names a variable for a request nothing will send. Accepting it would leave an
    // operator convinced they had configured a bearer token for the ChatGPT subscription path.
    let output = binary()
        .args(["--api-key-env", "OPENAI_API_KEY"])
        .output()
        .expect("the console binary starts");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("--endpoint"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_credential_file_and_an_endpoint_are_mutually_exclusive() {
    // They select different model backends. Silently preferring one would resolve a credential the
    // operator did not intend to spend.
    let output = binary()
        .args([
            "--auth-file",
            "/nonexistent/chatgpt-auth.console.json",
            "--endpoint",
            "http://127.0.0.1:8081/v1",
        ])
        .output()
        .expect("the console binary starts");

    assert_eq!(output.status.code(), Some(2));
    let message = stderr(&output);
    assert!(
        message.contains("--auth-file") && message.contains("--endpoint"),
        "the conflict has to name both: {message}"
    );
}

#[test]
fn the_session_bounds_are_numbers_and_are_checked_at_the_command_line() {
    for (flag, too_large) in [("--max-steps", "65"), ("--max-capability-calls", "257")] {
        for value in ["lots", "0", too_large] {
            let output = binary()
                .args([flag, value])
                .output()
                .expect("the console binary starts");
            assert_eq!(
                output.status.code(),
                Some(2),
                "{flag} accepted {value}: {}",
                stderr(&output)
            );
        }
    }
}

#[test]
fn no_subject_refuses_before_it_touches_anything() {
    let output = binary().output().expect("the console binary starts");

    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.contains("--subject"), "{message}");
    assert!(message.contains("DEKOPON_CONSOLE_SUBJECT"), "{message}");
    assert!(
        message.contains("defaultProfile"),
        "the refusal must name the authored identity route: {message}"
    );
}

#[test]
fn a_subject_no_service_could_issue_is_refused_at_the_command_line() {
    let output = binary()
        .args(["--subject", "sms.15550100000"])
        .output()
        .expect("the console binary starts");

    // Exit 2, not a broker refusal several steps later where the operator learns nothing about
    // which part was wrong.
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn every_flag_together_passes_parsing_and_stops_at_the_tty_gate() {
    let (_directory, path) = catalog();
    let socket = Path::new("/nonexistent/dekopon/broker.sock");
    let output = binary()
        .args(["--config".as_ref(), path.as_os_str()])
        .args(["--socket".as_ref(), socket.as_os_str()])
        .args([
            "--subject",
            "slack.t0123abc.u9xyz",
            "--model",
            "gpt-5.6-luna",
            "--endpoint",
            "http://127.0.0.1:8081/v1",
            "--api-key-env",
            "DEKOPON_TEST_MODEL_KEY",
            "--max-steps",
            "3",
            "--max-capability-calls",
            "5",
        ])
        .output()
        .expect("the console binary starts");

    let message = stderr(&output);
    assert_ne!(
        output.status.code(),
        Some(2),
        "every one of these is a supported flag: {message}"
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        message.contains("TTY"),
        "the interactive command must refuse without a terminal before contacting broker: {message}"
    );
}

#[test]
fn explicit_chatgpt_auth_file_passes_parsing_and_stops_at_the_tty_gate() {
    // Without --endpoint the subscription backend is selected. An explicit credential-file flag
    // is valid, but the non-TTY check returns before any credential or broker is opened.
    let (_directory, path) = catalog();
    let socket = Path::new("/nonexistent/dekopon/broker.sock");
    let output = binary()
        .args(["--config".as_ref(), path.as_os_str()])
        .args(["--socket".as_ref(), socket.as_os_str()])
        .args([
            "--subject",
            "slack.t0123abc.u9xyz",
            "--auth-file",
            "/nonexistent/dekopon/chatgpt-auth.console.json",
        ])
        .output()
        .expect("the console binary starts");

    let message = stderr(&output);
    assert_ne!(output.status.code(), Some(2), "{message}");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        message.contains("TTY"),
        "an explicit credential file passes parsing before the interactive TTY gate: {message}"
    );
}

fn profile_fixture(text: &str) -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let (dir, catalog) = catalog();
    let profiles = dir.path().join("profiles.yaml");
    std::fs::write(&profiles, text).expect("profiles fixture");
    (dir, catalog, profiles)
}

const PROFILE: &str = "defaultProfile: family\nprofiles:\n  - name: family\n    agent: reviewer\n    subject: slack.t0123abc.u9xyz\n    model: test-model\n    maxSteps: 2\n    maxCapabilityCalls: 3\n";

#[test]
fn authored_default_profile_is_accepted_without_subject_or_model_credential() {
    let (_dir, catalog, profiles) = profile_fixture(PROFILE);
    let output = binary()
        .args(["--config".as_ref(), catalog.as_os_str()])
        .args(["--profiles".as_ref(), profiles.as_os_str()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("TTY"), "{}", stderr(&output));
}

#[test]
fn explicit_profile_is_bound_to_its_agent() {
    let (_dir, catalog, profiles) = profile_fixture(PROFILE);
    let output = binary()
        .args(["--config".as_ref(), catalog.as_os_str()])
        .args(["--profiles".as_ref(), profiles.as_os_str()])
        .args(["--profile", "family", "--agent", "other"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("conflicts"), "{}", stderr(&output));
}

#[test]
fn explicit_subject_cannot_be_replaced_by_a_later_picker_profile() {
    // Neither profile is selected at startup; without the conflict check a picker hop could
    // silently replace the explicitly supplied subject with either authored identity.
    let profiles_text = "profiles:\n  - name: first\n    agent: reviewer\n    subject: slack.t0123abc.u9xyz\n  - name: second\n    agent: other\n    subject: slack.t0123abc.u8xyz\n";
    let (_dir, catalog, profiles) = profile_fixture(profiles_text);
    let output = binary()
        .args(["--config".as_ref(), catalog.as_os_str()])
        .args(["--profiles".as_ref(), profiles.as_os_str()])
        .args(["--subject", "slack.t0123abc.u7xyz"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("--subject") && message.contains("--profiles"),
        "{message}"
    );
    assert!(
        !message.contains("TTY"),
        "must refuse before opening the picker: {message}"
    );
}

#[test]
fn ambiguous_profiles_never_select_the_first_identity() {
    let duplicate = format!(
        "{}  - name: second\n    agent: reviewer\n    subject: slack.t0123abc.u9xyz\n",
        PROFILE
    );
    let (_dir, catalog, profiles) = profile_fixture(&duplicate);
    let output = binary()
        .args(["--config".as_ref(), catalog.as_os_str()])
        .args(["--profiles".as_ref(), profiles.as_os_str()])
        .args(["--agent", "reviewer"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("multiple profiles"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn unknown_agent_and_profile_names_refuse_before_connecting() {
    let (_dir, catalog, profiles) = profile_fixture(PROFILE);
    let base = || {
        let mut cmd = binary();
        cmd.args(["--config".as_ref(), catalog.as_os_str()]);
        cmd.args(["--profiles".as_ref(), profiles.as_os_str()]);
        cmd
    };
    let unknown = base().args(["--agent", "missing"]).output().unwrap();
    assert!(
        stderr(&unknown).contains("unknown agent"),
        "{}",
        stderr(&unknown)
    );
    let unknown = base().args(["--profile", "missing"]).output().unwrap();
    assert!(
        stderr(&unknown).contains("unknown profile"),
        "{}",
        stderr(&unknown)
    );
}

#[test]
fn malformed_profile_document_and_missing_default_are_refused() {
    for text in [
        "defaultProfile: missing\nprofiles: []\n",
        "profiles:\n  - name: family\n    agent: reviewer\n    subject: slack.t0123abc.u9xyz\n    maxSteps: 0\n",
        "profiles:\n  - name: family\n    agent: reviewer\n    subject: slack.t0123abc.u9xyz\n    scope: {kind: slack, transport: operator, conversation: {kind: directMessage, id: d0123abc}, trigger: message}\n",
        "profiles:\n  - name: family\n    agent: reviewer\n    subject: slack.t0123abc.u9xyz\n    unknownField: true\n",
        "profiles:\n  - name: family\n    agent: reviewer\n    subject: slack.t0123abc.u9xyz\n  - name: family\n    agent: reviewer\n    subject: slack.t0123abc.u9xyz\n",
    ] {
        let (_dir, catalog, profiles) = profile_fixture(text);
        let output = binary()
            .args(["--config".as_ref(), catalog.as_os_str()])
            .args(["--profiles".as_ref(), profiles.as_os_str()])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(
            stderr(&output).contains("profile configuration"),
            "{}",
            stderr(&output)
        );
    }
}

#[test]
fn obsolete_profile_scope_is_refused_even_when_other_fields_are_valid() {
    let text = format!(
        "{PROFILE}    scope: {{kind: local, transport: console-smoke, conversation: {{kind: directMessage, id: console-smoke}}, trigger: message}}\n"
    );
    let (_dir, catalog, profiles) = profile_fixture(&text);
    let output = binary()
        .args(["--config".as_ref(), catalog.as_os_str()])
        .args(["--profiles".as_ref(), profiles.as_os_str()])
        .output()
        .unwrap();
    let message = stderr(&output);
    assert!(message.contains("profile configuration"), "{message}");
    assert!(message.contains("unknown field"), "{message}");
    assert!(!message.contains("TTY"), "{message}");
}

#[test]
fn no_terminal_refuses_direct_selection_without_contacting_broker_or_model() {
    let (_dir, catalog) = catalog();
    let output = binary()
        .args(["--config".as_ref(), catalog.as_os_str()])
        .args([
            "--subject",
            "slack.t0123abc.u9xyz",
            "--agent",
            "reviewer",
            "--endpoint",
            "http://127.0.0.1:9/v1",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("TTY"), "{}", stderr(&output));
}

/// The chart's catalog: a ConfigMap mount, so every file is a symlink into `..data/`, one agent per
/// `*.yaml` with its instructions in a `.md` beside it.
#[test]
fn an_agents_directory_in_dekopon_config_loads_every_agent_and_its_instructions_file() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("..data");
    std::fs::create_dir(&data).unwrap();
    for (name, text) in [
        (
            "lange-family.yaml",
            "apiVersion: dekopon.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: lange-family\nspec:\n  description: Family chat\n  instructionsFile: lange-family.md\n",
        ),
        ("lange-family.md", "Answer the family kindly.\n"),
        ("reviewer.yaml", CATALOG),
    ] {
        std::fs::write(data.join(name), text).unwrap();
        std::os::unix::fs::symlink(Path::new("..data").join(name), dir.path().join(name)).unwrap();
    }

    for agent in ["lange-family", "reviewer"] {
        let output = binary()
            .env("DEKOPON_CONFIG", dir.path())
            .args(["--subject", "slack.t0123abc.u9xyz", "--agent", agent])
            .args(["--endpoint", "http://127.0.0.1:9/v1"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(stderr(&output).contains("TTY"), "{}", stderr(&output));
    }
    let catalog = dekopon_config::LocalCatalog::load(dir.path()).unwrap();
    let family = catalog
        .agents()
        .find(|agent| agent.metadata.name == "lange-family")
        .unwrap();
    assert_eq!(
        family.spec.instructions.as_deref(),
        Some("Answer the family kindly.\n")
    );
}

#[test]
fn an_empty_catalog_refuses_before_terminal_or_model_setup() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty.yaml");
    std::fs::write(&empty, "").unwrap();
    let output = binary()
        .args(["--config".as_ref(), empty.as_os_str()])
        .args(["--subject", "slack.t0123abc.u9xyz"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("configuration contains no resources"),
        "{}",
        stderr(&output)
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct PtyBroker {
    child: std::process::Child,
    _dir: tempfile::TempDir,
    socket: std::path::PathBuf,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for PtyBroker {
    fn drop(&mut self) {
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn pty_broker_fixture() -> Option<PtyBroker> {
    use std::os::unix::fs::PermissionsExt as _;
    let broker = std::env::var_os("DEKOPON_TEST_BROKERD")?;
    let wasm = std::env::var_os("DEKOPON_TEST_PROBE_WASM")?;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let component = dir.path().join("probe.wasm");
    let policy = dir.path().join("policy.cedar");
    let config = dir.path().join("broker.yaml");
    let socket = dir.path().join("broker.sock");
    for (path, bytes) in [
        (&component, std::fs::read(wasm).unwrap()),
        (&policy, br#"@id("console-agent") permit(principal == Dekopon::Principal::"maintainer", action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"reviewer") when { context.via == "dekopon-console" };
@id("console-read") permit(principal == Dekopon::Principal::"maintainer", action == Dekopon::Action::"cli-probe.upper", resource == Dekopon::Provider::"cli-probe") when { context.via == "dekopon-console" && context.agent == "reviewer" };"#.to_vec()),
    ] {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::fs::write(&config, format!("apiVersion: dekopon.dev/brokerd/v1alpha1\nsocketPath: {}\npoliciesPath: {}\nproviders: [{}]\nidentities:\n  - uid: {}\n    principal: dekopon-console\n    attestor:\n      namespaces: [slack.t0123abc]\nprincipals:\n  maintainer:\n    subjects: [slack.t0123abc.u9xyz]\ncapabilities:\n  cli-probe:\n    capabilities:\n      cli-probe.upper:\n        constraints: {{timeoutMs: 30000}}\n", socket.display(), policy.display(), component.display(), rustix::process::geteuid().as_raw())).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let log = std::fs::File::create(dir.path().join("broker.log")).unwrap();
    let mut child = Command::new(broker)
        .arg("--config")
        .arg(&config)
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    for _ in 0..300 {
        if socket.exists() {
            return Some(PtyBroker {
                child,
                _dir: dir,
                socket,
            });
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "broker startup {status}: {}",
                std::fs::read_to_string(dir.path().join("broker.log")).unwrap()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("broker did not bind");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn pty_console(socket: &Path, catalog: &Path, direct: bool) -> String {
    use std::io::{Read as _, Write as _};
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    let capture = tempfile::tempdir().unwrap();
    let transcript = capture.path().join("pty.log");
    let mut cmd = Command::new("script");
    #[cfg(target_os = "macos")]
    cmd.args([
        "-q",
        transcript.to_str().unwrap(),
        "sh",
        "-c",
        "stty cols 80 rows 24; exec \"$@\"",
        "sh",
        env!("CARGO_BIN_EXE_dekopon-console"),
    ]);
    #[cfg(target_os = "linux")]
    {
        let executable = env!("CARGO_BIN_EXE_dekopon-console").replace('\'', "'\\''");
        let command = format!(
            "stty cols 80 rows 24; exec '{executable}'{}",
            if direct { " --agent reviewer" } else { "" }
        );
        cmd.args(["-q", "-c", &command, transcript.to_str().unwrap()]);
    }
    // Normal no-flag startup gets its catalog, socket and subject from discovery; no model
    // request is made by the picker or by opening the exact agent's broker snapshot.
    cmd.env("DEKOPON_CONFIG", catalog)
        .env("DEKOPON_BROKER_SOCKET", socket)
        .env("DEKOPON_CONSOLE_SUBJECT", "slack.t0123abc.u9xyz")
        .env_remove("DEKOPON_CHATGPT_AUTH_FILE");
    #[cfg(target_os = "macos")]
    if direct {
        cmd.args(["--agent", "reviewer"]);
    }
    let mut child = cmd
        .env("TERM", "xterm-256color")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&output);
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut chunk = [0; 4096];
        while let Ok(count) = stdout.read(&mut chunk) {
            if count == 0 {
                break;
            }
            received.lock().unwrap().extend_from_slice(&chunk[..count]);
        }
    });
    let expected = if direct { "reviewer:" } else { "picker" };
    let deadline = Instant::now() + Duration::from_secs(20);
    let observed = loop {
        let text = output.lock().unwrap().clone();
        let shown = String::from_utf8_lossy(&text);
        if shown.contains(expected) {
            break shown.into_owned();
        }
        if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            drop(child.kill());
            drop(child.wait());
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!(
                "PTY console did not display {expected}: {shown}; stderr: {stderr}; log: {}",
                String::from_utf8_lossy(&std::fs::read(&transcript).unwrap_or_default())
            );
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    // Direct agent entry starts in composing mode, where 'q' edits the script rather than quits.
    child.stdin.take().unwrap().write_all(b"\x03").unwrap();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "PTY console exited {status}");
            break;
        }
        if Instant::now() >= deadline {
            drop(child.kill());
            drop(child.wait());
            panic!("console did not quit");
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    reader.join().unwrap();
    observed
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn exact_agent_enters_real_broker_session_while_omitted_agent_stays_in_picker() {
    let Some(fixture) = pty_broker_fixture() else {
        eprintln!("broker fixture env absent; PTY integration not exercised");
        return;
    };
    let (_dir, catalog) = catalog();
    let direct = pty_console(&fixture.socket, &catalog, true);
    assert!(
        direct.contains("reviewer:")
            && direct.contains("capabilities")
            && direct.contains("granted"),
        "direct selection did not open the broker leg: {direct}"
    );
    let picker = pty_console(&fixture.socket, &catalog, false);
    assert!(
        !picker.contains("reviewer:"),
        "bare selection opened a leg without picker action"
    );
}

#[test]
fn structured_refuses_implicit_default_profile_before_broker_connection() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = dir.path().join("agents.yaml");
    let profiles = dir.path().join("profiles.yaml");
    std::fs::write(&catalog, format!("{CATALOG}---\napiVersion: dekopon.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: second\nspec:\n  description: Second agent\n  enabled: true\n")).unwrap();
    std::fs::write(&profiles, "defaultProfile: second-profile\nprofiles:\n  - name: second-profile\n    agent: second\n    subject: slack.t0123abc.u9xyz\n").unwrap();
    let output = binary()
        .args(["--structured", "--config"])
        .arg(catalog)
        .arg("--profiles")
        .arg(profiles)
        .args([
            "--socket",
            "/nonexistent/broker.sock",
            "--endpoint",
            "http://127.0.0.1:9/v1",
        ])
        .output()
        .unwrap();
    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{message}");
    assert!(
        message.contains("--agent") && message.contains("--profile"),
        "{message}"
    );
    assert!(
        !message.contains("connect") && !message.contains("broker.sock"),
        "{message}"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn structured_fixture_accepts_exactly_64_completed_commands() {
    use std::{io::Write as _, process::Stdio};
    let Some(fixture) = pty_broker_fixture() else {
        eprintln!("broker fixture env absent; structured boundary not exercised");
        return;
    };
    let (_dir, config) = catalog();
    let mut child = binary()
        .args([
            "--structured",
            "--agent",
            "reviewer",
            "--subject",
            "slack.t0123abc.u9xyz",
            "--config",
        ])
        .arg(config)
        .arg("--socket")
        .arg(&fixture.socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(":smoke unknown\n".repeat(64).as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let text = stdout(&output);
    let nonce = text.lines().next().unwrap().strip_prefix("READY ").unwrap();
    assert_eq!(
        text.lines()
            .filter(|line| *line == format!("RESULT {nonce} error"))
            .count(),
        64
    );
    assert_eq!(
        text.lines()
            .filter(|line| *line == format!("END {nonce}"))
            .count(),
        64
    );
    assert!(
        output.status.success(),
        "completed frames must close successfully: {}",
        stderr(&output)
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn structured_fixture_refuses_65th_command_with_a_frame() {
    use std::{io::Write as _, process::Stdio};
    let Some(fixture) = pty_broker_fixture() else {
        eprintln!("broker fixture env absent; 65-command boundary not exercised");
        return;
    };
    let (_dir, config) = catalog();
    let mut child = binary()
        .args([
            "--structured",
            "--agent",
            "reviewer",
            "--subject",
            "slack.t0123abc.u9xyz",
            "--config",
        ])
        .arg(config)
        .arg("--socket")
        .arg(&fixture.socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(":smoke unknown\n".repeat(65).as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let text = stdout(&output);
    let nonce = text.lines().next().unwrap().strip_prefix("READY ").unwrap();
    assert_eq!(
        text.lines()
            .filter(|line| *line == format!("RESULT {nonce} error"))
            .count(),
        65
    );
    assert_eq!(
        text.lines()
            .filter(|line| *line == format!("END {nonce}"))
            .count(),
        65
    );
    assert!(
        text.contains("command limit exceeded; command not run"),
        "{text}"
    );
    assert!(!output.status.success(), "65th submitted command must fail");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn structured_fixture_orders_local_commands_and_ends_at_eof() {
    use std::{io::Write as _, process::Stdio};
    let Some(fixture) = pty_broker_fixture() else {
        eprintln!("broker fixture env absent; structured integration not exercised");
        return;
    };
    let (_dir, config) = catalog();
    let mut child = binary()
        .args([
            "--structured",
            "--agent",
            "reviewer",
            "--subject",
            "slack.t0123abc.u9xyz",
            "--config",
        ])
        .arg(config)
        .args(["--socket"])
        .arg(&fixture.socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b":smoke unknown\n:asset unknown\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let lines: Vec<_> = text.lines().collect();
    let nonce = lines[0].strip_prefix("READY ").unwrap();
    assert_eq!(nonce.len(), 48);
    assert_eq!(
        lines
            .iter()
            .filter(|s| **s == format!("RESULT {nonce} error"))
            .count(),
        2
    );
    assert_eq!(
        lines
            .iter()
            .filter(|s| **s == format!("END {nonce}"))
            .count(),
        2
    );
    assert!(!text.contains(":smoke unknown"));
    assert!(!text.contains(":asset unknown"));
}

#[test]
fn idle_starts_without_a_catalog_or_model_and_exits_on_sigterm() {
    let mut child = binary()
        .arg("--idle")
        .args(["--telemetry", "/nonexistent/ignored-by-idle.yaml"])
        .env("DEKOPON_BROKER_SOCKET", "/nonexistent/broker.sock")
        .env("DEKOPON_BROKER_UID", "65532")
        .env("DEKOPON_CONSOLE_SUBJECT", "discord.578258790881951745")
        .env("DEKOPON_CONFIG", "/nonexistent/dekopon.yaml")
        .spawn()
        .expect("idle process starts");
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(
        child.try_wait().unwrap().is_none(),
        "idle must remain alive without config or broker"
    );
    let signal = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(signal.success(), "send SIGTERM");
    let status = child.wait().expect("SIGTERM exits idle");
    assert!(status.success(), "idle must exit gracefully: {status}");
}

#[test]
fn invalid_telemetry_fails_before_startup_without_echoing_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("telemetry.yaml");
    for text in [
        "endpoint: http://user:synthetic-secret@localhost",
        "endpoint: http://localhost\nheaders: synthetic-secret",
        "endpoint: synthetic-secret\ntransport: invalid",
    ] {
        std::fs::write(&config, text).unwrap();
        let output = binary()
            .arg("--telemetry")
            .arg(&config)
            .arg("-vv")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(stdout(&output).is_empty());
        assert!(stderr(&output).contains("telemetry"));
        assert!(!stderr(&output).contains("synthetic-secret"));
    }
}
