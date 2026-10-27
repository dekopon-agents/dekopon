//! Interoperability with a separately built/released broker process. No model or live provider.
//! Run with DEKOPON_TEST_BROKERD and DEKOPON_TEST_PROBE_WASM pointing to verified artifacts.

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "tests spawn, join and drain freely"
)]

use dekopon_agent::{SessionInvoker, ShellRuntime, prompt::ScriptRuntime as _};
use dekopon_broker_protocol::{BrokerClient, ChatScopeClaim, FrameLimits};
use dekopon_core::{AgentId, ExternalSubject};
use dekopon_core::{SecretDrn, SecretUseProposal};
use dekopon_process::{CallBudget, Limits, TreeContext};
use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, CommandProposal, CommandRun, Interpreter, Streams,
};
use dekopon_tui::{
    App,
    record::{RecordingInvoker, Sequence, SessionEvent},
    session::{AgentSession, ConsoleOptions, LegHandle, NoDirect, connect, open_agent},
};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::Path,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;

#[path = "spawn_component.rs"]
mod spawn_component;

fn sink() -> Streams {
    let (stdout, mut reader) = std::os::unix::net::UnixStream::pair().unwrap();
    // Drain without retaining fixture output; a real stream endpoint is required by the wire.
    drop(std::thread::spawn(move || {
        std::io::copy(&mut reader, &mut std::io::sink()).unwrap();
    }));
    Streams {
        stdin: None,
        stdout: stdout.into(),
    }
}

struct BrokerFixture {
    child: Child,
    _dir: TempDir,
    socket: std::path::PathBuf,
}
impl Drop for BrokerFixture {
    fn drop(&mut self) {
        if let Err(error) = self.child.kill() {
            eprintln!("broker fixture stop: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("broker fixture wait: {error}");
        }
    }
}

fn write_private(path: &Path, contents: impl AsRef<[u8]>) {
    fs::write(path, contents).expect("fixture write");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("fixture mode");
}

async fn broker(mapped: bool) -> Option<BrokerFixture> {
    broker_with_telemetry(mapped, false, false, None).await
}

async fn broker_with_telemetry(
    mapped: bool,
    asset: bool,
    spawn: bool,
    endpoint: Option<&str>,
) -> Option<BrokerFixture> {
    let binary = std::env::var_os("DEKOPON_TEST_BROKERD")?;
    let wasm = if spawn {
        None
    } else {
        Some(std::env::var_os(if asset {
            "DEKOPON_TEST_ASSET_WASM"
        } else {
            "DEKOPON_TEST_PROBE_WASM"
        })?)
    };
    let dir = tempfile::tempdir().expect("fixture directory");
    fs::set_permissions(
        dir.path(),
        fs::Permissions::from_mode(if mapped { 0o700 } else { 0o710 }),
    )
    .expect("directory mode");
    let socket = dir.path().join("broker.sock");
    let component = dir.path().join("probe.wasm");
    write_private(
        &component,
        if let Some(wasm) = wasm {
            fs::read(wasm).expect("fixture component")
        } else {
            fs::read(spawn_component::component().path()).expect("tag v0.33.0 spawn component")
        },
    );
    let policy = dir.path().join("policies.cedar");
    write_private(&policy, br#"
@id("console-agent")
permit(principal == Dekopon::Principal::"maintainer", action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"reviewer")
when { context.via == "dekopon-console" }
unless { context has conversation && context.conversation.id != "d0123abc" };
@id("console-empty")
permit(principal == Dekopon::Principal::"maintainer", action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"empty")
when { context.via == "dekopon-console" };
@id("console-read")
permit(principal == Dekopon::Principal::"maintainer", action == Dekopon::Action::"cli-probe.upper", resource == Dekopon::Provider::"cli-probe")
when { context.via == "dekopon-console" && context.agent == "reviewer" };
@id("console-asset")
permit(principal == Dekopon::Principal::"maintainer", action == Dekopon::Action::"http-probe.purge", resource == Dekopon::Provider::"http-probe")
when { context.via == "dekopon-console" && context.agent == "reviewer" };
"#);
    if spawn {
        use std::io::Write as _;
        writeln!(fs::OpenOptions::new().append(true).open(&policy).unwrap(),
            "@id(\"console-spawn\") permit(principal == Dekopon::Principal::\"maintainer\", action == Dekopon::Action::\"cli-probe.write\", resource == Dekopon::Provider::\"cli-probe\") when {{ context.via == \"dekopon-console\" && context.agent == \"reviewer\" }};"
        ).unwrap();
    }
    let config = dir.path().join("broker.yaml");
    let assets_root = dir
        .path()
        .canonicalize()
        .expect("canonical fixture path")
        .join("assets");
    if asset {
        fs::create_dir(&assets_root).expect("assets directory");
        fs::set_permissions(&assets_root, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let asset_config = if asset {
        format!(
            "assets:\n  rootPath: {}\n  maxInFlightBytes: 1048576\n",
            assets_root.display()
        )
    } else {
        String::new()
    };
    let capability = if asset {
        "http-probe.purge"
    } else {
        "cli-probe.upper"
    };
    let provider = if asset { "http-probe" } else { "cli-probe" };
    let constraints = if asset {
        "{timeoutMs: 30000, asset: {attach: true, send: true, remove: false}}"
    } else {
        "{timeoutMs: 30000}"
    };
    let uid = rustix::process::geteuid().as_raw();
    let spawn_capability = if spawn {
        "      cli-probe.write:\n        constraints: {timeoutMs: 30000}\n"
    } else {
        ""
    };
    write_private(
        &config,
        format!(
            r#"apiVersion: dekopon.dev/brokerd/v1alpha1
{asset_config}socketPath: {}
policiesPath: {}
providers: [{}]
identities:
  - uid: {}
    principal: dekopon-console
    attestor:
      namespaces: [slack.t0123abc]
principals:
  maintainer:
    subjects: [slack.t0123abc.u9xyz]
capabilities:
  {provider}:
    capabilities:
      {capability}:
        constraints: {constraints}
{spawn_capability}"#,
            socket.display(),
            policy.display(),
            component.display(),
            if mapped { uid } else { uid + 1 }
        ),
    );
    if let Some(endpoint) = endpoint {
        use std::io::Write as _;
        let mut config = fs::OpenOptions::new().append(true).open(&config).unwrap();
        writeln!(config, "telemetry:\n  endpoint: {endpoint}\n  transport: http\n  serviceName: console-broker-fixture\n  exportTimeoutMs: 5000").unwrap();
    }
    let log = fs::File::create(dir.path().join("broker.log")).expect("log file");
    let mut child = Command::new(binary)
        .arg("--config")
        .arg(config)
        .stdout(Stdio::from(log.try_clone().expect("clone log")))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("start broker");
    for _ in 0..600 {
        if socket.exists() {
            return Some(BrokerFixture {
                child,
                _dir: dir,
                socket,
            });
        }
        if let Some(status) = child.try_wait().expect("broker status") {
            panic!(
                "broker failed at startup ({status}): {}",
                fs::read_to_string(dir.path().join("broker.log")).expect("broker log")
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "broker did not bind socket: {}",
        fs::read_to_string(dir.path().join("broker.log")).expect("broker log")
    );
}

/// v0.33.0 core brokerd test fixture: its provider invokes spawn/run("printf child").
/// Unlike the direct wire tests, this drives the console's own ShellRuntime and BrokerLeg.
#[tokio::test]
async fn console_shell_runs_provider_child_through_real_broker() {
    let Some(binary) = std::env::var_os("DEKOPON_TEST_BROKERD") else {
        eprintln!("broker fixture env absent; nested integration not exercised");
        return;
    };
    assert!(
        Path::new(&binary).is_file(),
        "broker fixture executable missing"
    );
    let fixture = broker_with_telemetry(true, false, true, None)
        .await
        .expect("broker executable supplied");
    let subject = "slack.t0123abc.u9xyz".parse().unwrap();
    let mut options = ConsoleOptions::new(subject, "unused".into());
    options.socket = Some(fixture.socket.clone());
    options.server_uid = Some(rustix::process::geteuid().as_raw());
    let (client, _) = connect(&options).await.expect("real broker connection");
    let leg = open_agent(client, options.subject, "reviewer".parse().unwrap(), None)
        .await
        .expect("broker authorized the console");
    let outcome = tokio::task::spawn_blocking(move || {
        ShellRuntime {
            invoker: LegHandle(Arc::new(leg)),
            limits: Limits::default(),
            calls: CallBudget::new(4),
        }
        .run_script("probe upper --text parent", None)
    })
    .await
    .expect("console script task");
    assert_eq!(
        outcome.exit_code.get(),
        0,
        "nested script failed: {}",
        outcome.output
    );
    assert!(
        outcome.output.contains("child"),
        "child output lost: {}",
        outcome.output
    );
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the existing end-to-end witness keeps setup, operation and assertions together"
)]
async fn published_client_against_real_broker_allows_only_attested_agent_surface() {
    let Some(fixture) = broker(true).await else {
        eprintln!("broker fixture env absent; integration not exercised");
        return;
    };
    let uid = rustix::process::geteuid().as_raw();
    let subject: ExternalSubject = "slack.t0123abc.u9xyz".parse().expect("canonical subject");
    let mut options = ConsoleOptions::new(subject.clone(), "unused".into());
    options.socket = Some(fixture.socket.clone());
    options.server_uid = Some(uid);
    let (client, _) = connect(&options).await.expect("authenticated peer");
    let allowed = open_agent(
        client.clone(),
        subject.clone(),
        "reviewer".parse().unwrap(),
        None,
    )
    .await
    .expect("agent prompt granted");
    assert_eq!(allowed.effective_capabilities().len(), 1);
    assert_eq!(allowed.effective_capabilities()[0].id, "cli-probe.upper");
    assert!(allowed.command_words().contains(&"probe".to_owned()));
    let second = open_agent(
        client.clone(),
        subject.clone(),
        "reviewer".parse().unwrap(),
        None,
    )
    .await
    .expect("fresh session");
    assert_ne!(
        allowed.session_trace(),
        second.session_trace(),
        "new broker legs do not reuse a session trace"
    );
    let mut app = App::new(
        vec![],
        subject.to_string(),
        fixture.socket.display().to_string(),
        "auth-file".into(),
    );
    app.profile = Some("operator".into());
    app.scope_label = "subject-only".into();
    app.model = "test-model".into();
    app.enter(AgentSession::new(
        "reviewer".parse().unwrap(),
        second,
        Default::default(),
    ));
    assert_eq!(app.pane, dekopon_tui::Pane::Shell);
    assert_eq!(app.mode, dekopon_tui::Mode::Composing);
    assert!(app.composer.is_empty());
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| dekopon_tui::ui::draw(frame, &app))
        .unwrap();
    let rendered: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    for field in [
        "LIVE PROVIDERS",
        "effects are real",
        "model (default): test-model",
        "agent: reviewer",
        "profile: operator",
        "subject: slack.t0123abc.u9xyz",
        "console: dekopon-console",
        "session: subject-only",
    ] {
        assert!(
            rendered.contains(field),
            "80-column entered session hides {field}"
        );
    }
    tokio::task::spawn_blocking(move || {
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let recorded = RecordingInvoker::new(allowed, events, Sequence::default());
        let help = Interpreter::new(Limits::default()).run("probe --help", &recorded);
        assert_eq!(
            help.exit_code.get(),
            0,
            "provider command help: {}",
            help.output
        );
        assert!(help.output.contains("Usage: probe"));
        let read = Interpreter::new(Limits::default()).run("probe upper --text hello", &recorded);
        assert_eq!(
            read.exit_code.get(),
            0,
            "pure fixture read: {}",
            read.output
        );
        assert!(read.output.contains("HELLO"), "{}", read.output);
        let refused =
            Interpreter::new(Limits::default()).run("probe reverse --text hello", &recorded);
        assert_ne!(
            refused.exit_code.get(),
            0,
            "ungranted fixture capability cannot run"
        );
        let drn = SecretUseProposal::HttpBearer {
            secret: "drn:com.xrl:secret:prod:api/token"
                .parse::<SecretDrn>()
                .unwrap(),
        };
        let denied = recorded.invoke(CommandProposal::new("cli-probe.upper", json!({"text":"hi"}), Some(drn)), sink(), &TreeContext::new(Limits::default(), CallBudget::new(4)));
        assert!(
            matches!(denied, CapabilityCallResult::Denied { .. }),
            "secret.use cannot be dropped by a wrapper: {denied:?}"
        );
        assert!(
            std::iter::from_fn(|| receiver.try_recv().ok()).any(|event| matches!(event, SessionEvent::Capability(call) if matches!(call.outcome, dekopon_tui::record::CallOutcome::Denied(_)))),
            "secret-use refusal remains observable"
        );
    })
    .await
    .expect("fixture script task");
    let empty = open_agent(
        client.clone(),
        subject.clone(),
        "empty".parse().unwrap(),
        None,
    )
    .await
    .expect("empty prompt granted");
    assert!(
        empty.effective_capabilities().is_empty(),
        "no model call may follow empty surface"
    );
    let mut empty_app = App::new(
        vec![],
        subject.to_string(),
        fixture.socket.display().to_string(),
        "unopened".into(),
    );
    empty_app.enter(AgentSession::new(
        "empty".parse().unwrap(),
        empty,
        Default::default(),
    ));
    empty_app.composer = "do not call model".into();
    assert!(
        empty_app.submit_turn().is_none(),
        "empty grant gate refuses before model construction"
    );
    assert!(
        open_agent(
            client.clone(),
            subject.clone(),
            "other".parse::<AgentId>().unwrap(),
            None
        )
        .await
        .is_err(),
        "missing agent.prompt refuses"
    );
    assert!(
        open_agent(
            client.clone(),
            "slack.other.u9xyz".parse().unwrap(),
            "reviewer".parse().unwrap(),
            None
        )
        .await
        .is_err(),
        "namespace denied"
    );
    let allowed_scope: ChatScopeClaim = serde_json::from_str(r#"{"transport":"operator","kind":"slack","conversation":{"kind":"directMessage","container":"t0123abc","id":"d0123abc"},"trigger":"message"}"#).expect("typed scope");
    let real_scope_refusal = open_agent(
        client.clone(),
        subject.clone(),
        "reviewer".parse().unwrap(),
        Some(allowed_scope),
    )
    .await
    .err()
    .expect("authenticated console cannot attest a real conversation");
    assert!(
        format!("{real_scope_refusal:?}")
            .contains("dekopon sandbox: console sessions cannot attest a real conversation"),
        "real-scope refusal must be actionable at the broker boundary: {real_scope_refusal:?}"
    );
    let wrong_scope: ChatScopeClaim = serde_json::from_str(r#"{"transport":"operator","kind":"slack","conversation":{"kind":"directMessage","container":"t0123abc","id":"d9999xyz"},"trigger":"message"}"#).expect("typed scope");
    assert!(
        open_agent(
            client,
            subject,
            "reviewer".parse().unwrap(),
            Some(wrong_scope)
        )
        .await
        .is_err(),
        "console cannot attest any real conversation, even one outside its grant"
    );
    let wrong = BrokerClient::new(&fixture.socket, uid + 1, FrameLimits::default())
        .expect("path validated");
    assert!(
        wrong.capabilities().await.is_err(),
        "server UID pin enforced"
    );
}

#[tokio::test]
async fn executed_asset_effect_registers_descriptor_through_real_broker() {
    // Verified v0.33.0 broker and provider component; no model or paid provider call.
    let Some(fixture) = broker_with_telemetry(true, true, false, None).await else {
        eprintln!("asset fixture env absent; integration not exercised");
        return;
    };
    let assets_root = fixture._dir.path().join("assets");
    let subject = "slack.t0123abc.u9xyz".parse().unwrap();
    let mut options = ConsoleOptions::new(subject, "unused".into());
    options.socket = Some(fixture.socket.clone());
    options.server_uid = Some(rustix::process::geteuid().as_raw());
    let (client, _) = connect(&options).await.unwrap();
    let leg = open_agent(client, options.subject, "reviewer".parse().unwrap(), None)
        .await
        .unwrap();
    assert_eq!(leg.effective_capabilities().len(), 1);
    let outcome = tokio::task::spawn_blocking(move || {
        LegHandle(Arc::new(leg)).invoke(
            CommandProposal::new("http-probe.purge", json!({"assetMode": "attach"}), None),
            sink(),
            &TreeContext::new(Limits::default(), CallBudget::new(4)),
        )
    })
    .await
    .unwrap();
    let CapabilityCallResult::SucceededWithStderr(note) = outcome else {
        panic!("asset effect must succeed with a registration note: {outcome:?}");
    };
    assert!(
        note.contains("chat-asset:1"),
        "missing registration id: {note}"
    );
    assert!(note.contains("text/plain"), "missing media type: {note}");
    assert!(note.contains("11 stored bytes"), "wrong byte size: {note}");
    assert!(!note.contains("no asset store"), "old refusal: {note}");
    assert_eq!(
        fs::read_dir(&assets_root).unwrap().count(),
        0,
        "broker assets must be unlinked after output"
    );
}

// The verified http-probe Wasm exposes assetMode in its capability input schema but not in
// its CLI argv parser. This test-only command-word parser supplies that *same* broker-granted
// proposal to ShellRuntime; all effect execution, asset descriptors, registration and stderr
// handling still run through the real broker and the console's LegHandle. It is not evidence
// that an operator can type this command against the stock http-probe provider CLI.
struct AssetFixtureCommand(LegHandle);

impl AssetFixtureCommand {
    fn parse_command(
        &self,
        word: &str,
        argv: &[String],
        stdin_piped: bool,
    ) -> Option<Result<dekopon_shell::CommandProposal, CommandRun>> {
        if word != "asset-fixture" || !argv.is_empty() || stdin_piped {
            return None;
        }
        Some(Ok(dekopon_shell::CommandProposal {
            capability: "http-probe.purge".into(),
            input: json!({"assetMode": "attach"}),
            secret_use: None,
            report: None,
        }))
    }
}

impl CapabilityInvoker for AssetFixtureCommand {
    fn granted(&self) -> Vec<String> {
        self.0.granted()
    }

    fn command_words(&self) -> Vec<String> {
        vec!["asset-fixture".into()]
    }

    fn run(
        &self,
        call: dekopon_shell::RunCall,
        streams: dekopon_shell::Streams,
        tree: &dekopon_process::TreeContext,
    ) -> dekopon_shell::RunResult {
        let argv = call
            .argv
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        match self.parse_command(&call.word, &argv, streams.stdin.is_some()) {
            Some(Ok(proposal)) => {
                let capability = proposal.capability.clone();
                let input = proposal.input.clone();
                let secret_use = proposal.secret_use.clone();
                if !self.is_granted(&capability) {
                    return dekopon_shell::render_command(
                        streams,
                        "",
                        format!(
                            "dekopon-shell: {}: requires capability {}, which is not granted in this session\n",
                            call.word, capability
                        ),
                        127,
                    );
                }
                let result = self.invoke(proposal, streams, tree);
                dekopon_shell::RunResult::Invoked {
                    capability,
                    input,
                    secret_use,
                    result,
                }
            }
            Some(Err(rendered)) => dekopon_shell::render_run(call, Some(rendered), streams),
            None => dekopon_shell::render_run(call, None, streams),
        }
    }

    fn invoke(
        &self,
        proposal: CommandProposal,
        streams: Streams,
        tree: &TreeContext,
    ) -> CapabilityCallResult {
        self.0.invoke(proposal, streams, tree)
    }
}

#[tokio::test]
async fn console_shell_and_model_script_register_real_broker_asset_with_zero_exit() {
    let Some(fixture) = broker_with_telemetry(true, true, false, None).await else {
        eprintln!("asset fixture env absent; shell and model-script integration not exercised");
        return;
    };
    let subject = "slack.t0123abc.u9xyz".parse().unwrap();
    let mut options = ConsoleOptions::new(subject, "unused".into());
    options.socket = Some(fixture.socket.clone());
    options.server_uid = Some(rustix::process::geteuid().as_raw());
    let (client, _) = connect(&options).await.unwrap();
    for model_script in [false, true] {
        let leg = open_agent(
            client.clone(),
            options.subject.clone(),
            "reviewer".parse().unwrap(),
            None,
        )
        .await
        .unwrap();
        let output = tokio::task::spawn_blocking(move || {
            let broker = AssetFixtureCommand(LegHandle(Arc::new(leg)));
            if model_script {
                // run_turn builds this SessionInvoker (NoDirect + LegHandle) for model scripts.
                // The fixture parser wraps only the command-word step; invoke is unchanged.
                ShellRuntime {
                    invoker: SessionInvoker {
                        direct: NoDirect,
                        broker: Some(Box::new(broker)),
                    },
                    limits: Limits::default(),
                    calls: CallBudget::new(4),
                }
                .run_script("asset-fixture", None)
            } else {
                ShellRuntime {
                    invoker: broker,
                    limits: Limits::default(),
                    calls: CallBudget::new(4),
                }
                .run_script("asset-fixture", None)
            }
        })
        .await
        .unwrap();
        assert_eq!(
            output.exit_code.get(),
            0,
            "model_script={model_script}: {}",
            output.output
        );
        assert!(
            output.output.contains("chat-asset:1"),
            "model_script={model_script}: {}",
            output.output
        );
        assert!(
            output.output.contains("text/plain"),
            "model_script={model_script}: {}",
            output.output
        );
        assert!(
            output.output.contains("11 stored bytes"),
            "model_script={model_script}: {}",
            output.output
        );
        assert!(
            !output.output.contains("no asset store"),
            "model_script={model_script}: {}",
            output.output
        );
    }
    assert_eq!(
        fs::read_dir(fixture._dir.path().join("assets"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn real_broker_rejects_unmapped_peer_uid() {
    let Some(fixture) = broker(false).await else {
        eprintln!("broker fixture env absent; integration not exercised");
        return;
    };
    let mut options = ConsoleOptions::new("slack.t0123abc.u9xyz".parse().unwrap(), "unused".into());
    options.socket = Some(fixture.socket.clone());
    options.server_uid = Some(rustix::process::geteuid().as_raw());
    assert!(
        connect(&options).await.is_err(),
        "unmapped peer cannot probe broker"
    );
}

#[path = "support/trace.rs"]
mod trace;
