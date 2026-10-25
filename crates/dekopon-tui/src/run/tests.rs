use std::{io::IsTerminal as _, time::Duration};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use dekopon_agent::prompt::{ConversationTurn, History};
use dekopon_broker_protocol::{BrokerClient, FrameLimits};
use dekopon_process::Limits;
use dekopon_protocol::{Agent, AgentKind, AgentSpec, ApiVersion, ObjectMeta};
use dekopon_shell::{
    CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandProposal, CommandRun,
    Interpreter, Streams,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};

use super::{Action, RunningTurn, TerminalGuard, dispatch, drive_event_loop, on_key};
use crate::{
    app::{App, Mode, Pane},
    profile::OperatorProfile,
    record::{CallOutcome, CapabilityCall, RecordingInvoker, Sequence, SessionEvent},
    session::{ConsoleOptions, StopFlag},
};

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

fn console() -> App {
    let agent = Agent {
        api_version: ApiVersion::V1Alpha1,
        kind: AgentKind::Agent,
        metadata: ObjectMeta::named("ville-github"),
        spec: AgentSpec {
            description: "a fixture".to_owned(),
            enabled: true,
            instructions: None,
            skills: Vec::new(),
            model_class: None,
            instructions_file: None,
        },
        status: None,
    };
    App::new(
        vec![agent],
        "slack.t0123abc.u9xyz".to_owned(),
        "/run/dekopon/broker.sock".to_owned(),
        "/config/dekopon/chatgpt-auth.console.json".to_owned(),
    )
}

#[test]
fn terminal_guard_restores_after_normal_exit_and_panic_when_run_under_a_pty() {
    if !std::io::stdout().is_terminal() {
        return;
    }
    {
        let (_guard, _terminal) = TerminalGuard::enter().expect("enter raw alternate screen");
        assert!(crossterm::terminal::is_raw_mode_enabled().unwrap());
    }
    assert!(
        !crossterm::terminal::is_raw_mode_enabled().unwrap(),
        "normal exit restores raw mode"
    );
    let early_error = || -> std::io::Result<()> {
        let (_guard, _terminal) = TerminalGuard::enter()?;
        Err(std::io::Error::other("injected draw failure"))
    };
    assert!(early_error().is_err());
    assert!(
        !crossterm::terminal::is_raw_mode_enabled().unwrap(),
        "error return restores raw mode"
    );
    let caught = std::panic::catch_unwind(|| {
        let (_guard, _terminal) = TerminalGuard::enter().expect("enter raw alternate screen");
        panic!("injected terminal panic");
    });
    assert!(caught.is_err());
    assert!(
        !crossterm::terminal::is_raw_mode_enabled().unwrap(),
        "panic restores raw mode"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the existing end-to-end witness keeps setup, operation and assertions together"
)]
fn cancelled_running_event_loop_restores_pty_after_join() {
    use std::{
        io::{Read as _, Write as _},
        process::{Command, Stdio},
        sync::atomic::AtomicBool,
        time::Instant,
    };
    if std::env::var_os("DEKOPON_TEST_PTY_CHILD").is_some() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (guard, mut terminal) = TerminalGuard::enter().unwrap();
            let mut app = console();
            app.busy = true;
            app.transcript.open("in flight".into());
            let mut options =
                ConsoleOptions::new("slack.t0123abc.u9xyz".parse().unwrap(), "unused".into());
            let mut history = History::new(options.history_limits);
            let stop = StopFlag::default();
            let mut keys = crossterm::event::EventStream::new();
            let (sender, receiver) = crate::session::session_channel();
            let finished = Arc::new(AtomicBool::new(false));
            let done = Arc::clone(&finished);
            let probe = stop.clone();
            let handle = tokio::spawn(async move {
                while !probe.is_requested() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                done.store(true, Ordering::SeqCst);
                eprintln!("CANCEL_HANDLED");
                drop(sender);
                Ok(History::default())
            });
            let mut running = Some(RunningTurn {
                events: receiver,
                handle,
                shell: false,
            });
            let dir = tempfile::tempdir().unwrap();
            let client = BrokerClient::new(
                dir.path().join("absent.sock"),
                rustix::process::geteuid().as_raw(),
                FrameLimits::default(),
            )
            .unwrap();
            drive_event_loop(
                &mut terminal,
                &mut app,
                &client,
                &mut options,
                &mut keys,
                &mut running,
                &mut history,
                &stop,
            )
            .await
            .unwrap();
            assert!(
                finished.load(Ordering::SeqCst),
                "cancelled work was not joined"
            );
            assert!(running.is_none());
            assert!(app.transcript.turns()[0].stop_requested);
            drop(guard);
            assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
            eprintln!("PTY_RESTORED");
        });
        return;
    }
    let capture = tempfile::tempdir().unwrap();
    let transcript = capture.path().join("pty.log");
    let mut command = Command::new("script");
    #[cfg(target_os = "macos")]
    command
        .args([
            "-q",
            transcript.to_str().unwrap(),
            "sh",
            "-c",
            "stty cols 80 rows 24; exec \"$@\"",
            "sh",
        ])
        .arg(std::env::current_exe().unwrap())
        .args([
            "run::tests::cancelled_running_event_loop_restores_pty_after_join",
            "--exact",
            "--nocapture",
        ]);
    #[cfg(target_os = "linux")]
    {
        let executable = std::env::current_exe()
            .unwrap()
            .display()
            .to_string()
            .replace('\'', "'\\''");
        let shell = format!(
            "stty cols 80 rows 24; exec '{executable}' run::tests::cancelled_running_event_loop_restores_pty_after_join --exact --nocapture"
        );
        command.args(["-q", "-c", &shell, transcript.to_str().unwrap()]);
    }
    let mut child = command
        .env("DEKOPON_TEST_PTY_CHILD", "1")
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
    let deadline = Instant::now() + Duration::from_secs(20);
    fn wait_for(
        child: &mut std::process::Child,
        output: &Arc<Mutex<Vec<u8>>>,
        deadline: Instant,
        needle: &str,
    ) {
        loop {
            let text = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
            if text.contains(needle) {
                return;
            }
            if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
                drop(child.kill());
                drop(child.wait());
                panic!("PTY fixture did not emit {needle}: {text}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    wait_for(&mut child, &output, deadline, "LIVE");
    child.stdin.as_mut().unwrap().write_all(b"\x1b").unwrap();
    wait_for(&mut child, &output, deadline, "CANCEL_HANDLED");
    child.stdin.as_mut().unwrap().write_all(b"q").unwrap();
    wait_for(&mut child, &output, deadline, "PTY_RESTORED");
    let status = child.wait().unwrap();
    reader.join().unwrap();
    assert!(status.success(), "PTY child failed: {status}");
    let text = output.lock().unwrap();
    assert!(text.windows(8).any(|part| part == b"\x1b[?1049h"));
    assert!(text.windows(8).any(|part| part == b"\x1b[?1049l"));
}

#[test]
fn control_c_quits_from_any_mode() {
    for mode in [Mode::Browsing, Mode::Composing, Mode::Help] {
        let mut app = console();
        app.mode = mode.clone();
        let key = KeyEvent {
            modifiers: KeyModifiers::CONTROL,
            ..press(KeyCode::Char('c'))
        };
        assert!(on_key(&mut app, key, &StopFlag::default()).is_none());
        assert!(app.should_quit, "ctrl-c must work in {mode:?}");
    }
}

#[test]
fn any_key_dismisses_the_overlay_without_acting() {
    let mut app = console();
    app.mode = Mode::Help;
    assert!(on_key(&mut app, press(KeyCode::Char('q')), &StopFlag::default()).is_none());
    assert_eq!(app.mode, Mode::Browsing);
    assert!(
        !app.should_quit,
        "dismissing the overlay must not also quit"
    );
}

#[test]
fn tab_cycles_panes_in_both_directions() {
    let mut app = console();
    let stop = StopFlag::default();
    on_key(&mut app, press(KeyCode::Tab), &stop);
    assert_eq!(app.pane, Pane::Shell);
    on_key(&mut app, press(KeyCode::BackTab), &stop);
    assert_eq!(app.pane, Pane::Agents);
}

#[test]
fn enter_on_the_agent_list_asks_to_hop() {
    let mut app = console();
    assert_eq!(
        on_key(&mut app, press(KeyCode::Enter), &StopFlag::default()),
        Some(Action::Enter)
    );
}

#[test]
fn composing_preserves_a_refused_shell_line_without_an_agent() {
    let mut app = console();
    let stop = StopFlag::default();
    app.pane = Pane::Shell;
    on_key(&mut app, press(KeyCode::Char('i')), &stop);
    assert_eq!(app.mode, Mode::Composing);

    for character in "cap --list".chars() {
        on_key(&mut app, press(KeyCode::Char(character)), &stop);
    }
    assert_eq!(app.composer, "cap --list");

    assert_eq!(on_key(&mut app, press(KeyCode::Enter), &stop), None);
    assert_eq!(app.composer, "cap --list");
    assert!(app.notice.as_ref().is_some_and(|notice| notice.is_refusal));
    assert_eq!(app.mode, Mode::Composing);
}

#[test]
fn escape_while_composing_discards_rather_than_stopping_a_turn() {
    let mut app = console();
    let stop = StopFlag::default();
    app.pane = Pane::Shell;
    app.mode = Mode::Composing;
    app.composer = "half a thought".to_owned();

    on_key(&mut app, press(KeyCode::Esc), &stop);
    assert_eq!(app.mode, Mode::Browsing);
    assert!(app.composer.is_empty());
    assert!(
        !stop.is_requested(),
        "leaving the composer must not stop the running turn"
    );
}

#[test]
fn escape_while_browsing_stops_a_running_turn() {
    let mut app = console();
    let stop = StopFlag::default();
    app.busy = true;
    app.transcript.open("ask".to_owned());

    on_key(&mut app, press(KeyCode::Esc), &stop);
    assert!(stop.is_requested());
    assert!(app.transcript.turns()[0].stop_requested);
}

/// A controlled broker-command seam: first invocation remains in flight until released by the
/// harness. A cancelled next proposal is refused before it reaches the simulated broker.
struct BlockingCommands {
    started: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    stop: StopFlag,
    accepted: Arc<AtomicUsize>,
}

impl BlockingCommands {
    fn parse_command(
        &self,
        word: &str,
        argv: &[String],
        _: bool,
    ) -> Option<Result<dekopon_shell::CommandProposal, CommandRun>> {
        if word != "probe" {
            return None;
        }
        Some(Ok(dekopon_shell::CommandProposal {
            capability: format!("probe.{}", argv.first()?),
            input: json!({}),
            secret_use: None,
            report: None,
        }))
    }
}

impl CapabilityInvoker for BlockingCommands {
    fn granted(&self) -> Vec<String> {
        vec!["probe.first".into(), "probe.second".into()]
    }
    fn is_granted(&self, capability: &str) -> bool {
        matches!(capability, "probe.first" | "probe.second")
    }
    fn command_words(&self) -> Vec<String> {
        vec!["probe".into()]
    }
    fn has_command_word(&self, word: &str) -> bool {
        word == "probe"
    }
    fn describe(&self, _: &str) -> Option<CapabilityDescription> {
        None
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
        _tree: &dekopon_process::TreeContext,
    ) -> CapabilityCallResult {
        let capability = proposal.capability;
        if self.stop.is_requested() {
            return CapabilityCallResult::Denied {
                reason: "session-cancelled".into(),
            };
        }
        self.accepted.fetch_add(1, Ordering::SeqCst);
        if capability == "probe.first" {
            self.started.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
        streams.reply(&json!({"effect": capability}))
    }
}

#[test]
fn esc_during_an_accepted_command_prevents_the_next_broker_effect() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (events_tx, mut events_rx) = crate::session::session_channel();
    let accepted = Arc::new(AtomicUsize::new(0));
    let stop = StopFlag::default();
    let commands = BlockingCommands {
        started: started_tx,
        release: Mutex::new(release_rx),
        stop: stop.clone(),
        accepted: Arc::clone(&accepted),
    };
    let task = std::thread::spawn(move || {
        let invoker = RecordingInvoker::new(commands, events_tx, Sequence::default());
        Interpreter::new(Limits::default()).run("probe first; probe second", &invoker)
    });
    started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("first command reached broker seam");
    let mut app = console();
    app.busy = true;
    app.transcript.open("run two commands".into());
    on_key(&mut app, press(KeyCode::Esc), &stop);
    assert!(stop.is_requested());
    release_tx.send(()).unwrap();
    let result = task.join().unwrap();
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "second effect must not reach broker after stop: {result:?}"
    );
    app.on_session_event(SessionEvent::ScriptStarted {
        sequence: 0,
        script: "probe first; probe second".into(),
    });
    while let Ok(event) = events_rx.try_recv() {
        app.on_session_event(event);
    }
    assert!(app.transcript.turns()[0].stop_requested);
    let calls = app.calls();
    assert_eq!(
        calls.len(),
        2,
        "the refused second proposal remains observable"
    );
    assert!(matches!(
        app.call_at(calls[0]).unwrap().outcome,
        CallOutcome::Succeeded
    ));
    assert!(matches!(
        app.call_at(calls[1]).unwrap().outcome,
        CallOutcome::Denied(_)
    ));
}

#[test]
fn cancellation_does_not_erase_an_effect_the_broker_already_accepted() {
    let mut app = console();
    app.busy = true;
    app.transcript.open("perform action".into());
    let stop = StopFlag::default();
    let (handle, signal) = dekopon_process::CancelSignal::pair();
    stop.bind_broker(handle);
    on_key(&mut app, press(KeyCode::Esc), &stop);
    assert!(signal.is_cancelled(), "Esc reaches the broker leg");
    app.on_session_event(SessionEvent::ScriptStarted {
        sequence: 0,
        script: "probe action".into(),
    });
    app.on_session_event(SessionEvent::Capability(Box::new(CapabilityCall {
        sequence: 1,
        capability: "probe.action".into(),
        input: json!({}),
        outcome: CallOutcome::Succeeded,
        elapsed: Duration::from_millis(5),
    })));
    assert!(app.transcript.turns()[0].stop_requested);
    assert!(
        matches!(
            app.transcript.turns()[0].scripts[0].calls[0].outcome,
            CallOutcome::Succeeded
        ),
        "accepted effects are still visible after a stop request"
    );
}

#[tokio::test]
async fn switching_profiles_clears_the_model_replay_and_visible_history() {
    let first = "slack.t0123abc.u9xyz".parse().unwrap();
    let second = "slack.t0123abc.u8xyz".parse().unwrap();
    let mut options = ConsoleOptions::new(first, "test-model".into());
    options.profiles.push(OperatorProfile {
        name: "second".into(),
        agent: "other".parse().unwrap(),
        subject: second,
        model: None,
        max_steps: None,
        max_capability_calls: None,
    });
    let mut history = History::new(options.history_limits);
    history.record(ConversationTurn::completed(
        "first profile secret",
        "answer",
    ));
    let mut app = console();
    let mut other = app.agents[0].clone();
    other.metadata.name = "other".into();
    app.agents.push(other);
    app.selected_agent = 1;
    app.profile = Some("first".into());
    app.transcript.open("first profile secret".into());
    app.start_shell("first profile shell".into());
    app.fail_shell(crate::ErrorReport::from_message(
        "run shell command",
        "old result",
    ));
    app.busy = false;
    let directory = tempfile::tempdir().unwrap();
    let client = BrokerClient::new(
        directory.path().join("absent.sock"),
        rustix::process::geteuid().as_raw(),
        FrameLimits::default(),
    )
    .unwrap();
    let mut running = None;
    dispatch(
        &mut app,
        Action::Enter,
        &client,
        &mut options,
        &mut running,
        &mut history,
        &StopFlag::default(),
    )
    .await;
    assert_eq!(app.profile.as_deref(), Some("second"));
    assert_eq!(app.subject, "slack.t0123abc.u8xyz");
    assert_eq!(app.model_source, crate::session::ModelSource::Default);
    assert_eq!(app.model, crate::session::DEFAULT_MODEL);
    assert!(
        history.is_empty(),
        "new profile must not replay the old subject's turns"
    );
    assert!(app.transcript.turns().is_empty());
    assert!(app.shell_history.is_empty());
    assert!(
        app.session.is_none(),
        "absent broker must refuse, not open a stale leg"
    );
}

#[test]
fn escape_stops_a_manual_shell_command_without_an_open_model_turn() {
    let mut app = console();
    let stop = StopFlag::default();
    app.busy = true;
    on_key(&mut app, press(KeyCode::Esc), &stop);
    assert!(stop.is_requested());
    assert!(
        app.notice
            .as_ref()
            .is_some_and(|notice| notice.text.contains("not") || notice.text.contains("complete"))
    );
}

#[test]
fn escape_with_nothing_running_requests_nothing() {
    let mut app = console();
    let stop = StopFlag::default();
    on_key(&mut app, press(KeyCode::Esc), &stop);
    assert!(!stop.is_requested());
}

#[test]
fn backspace_removes_one_character() {
    let mut app = console();
    let stop = StopFlag::default();
    app.mode = Mode::Composing;
    app.composer = "abc".to_owned();
    on_key(&mut app, press(KeyCode::Backspace), &stop);
    assert_eq!(app.composer, "ab");
}

#[test]
fn composing_is_only_offered_where_there_is_something_to_type_into() {
    let mut app = console();
    let stop = StopFlag::default();
    app.pane = Pane::Agents;
    on_key(&mut app, press(KeyCode::Char('i')), &stop);
    assert_eq!(
        app.mode,
        Mode::Browsing,
        "the agent list has no composer, so `i` must not open one"
    );
}

#[tokio::test]
async fn profile_switches_preserve_cli_override_and_update_model_provenance() {
    use crate::session::{DEFAULT_MODEL, ModelSource};
    let mut options = ConsoleOptions::new(
        "slack.t0123abc.u9xyz".parse().unwrap(),
        DEFAULT_MODEL.into(),
    );
    options.profiles.push(OperatorProfile {
        name: "authored".into(),
        agent: "reviewer".parse().unwrap(),
        subject: options.subject.clone(),
        model: Some("profile-model".into()),
        max_steps: None,
        max_capability_calls: None,
    });
    let mut app = console();
    options.profiles[0].agent = app.agents[0].metadata.name.parse().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let client = BrokerClient::new(
        directory.path().join("absent.sock"),
        rustix::process::geteuid().as_raw(),
        FrameLimits::default(),
    )
    .unwrap();
    let mut history = History::new(options.history_limits);
    let mut running = None;
    for (cli, profile, expected, source) in [
        (
            Some("cli-model"),
            Some("profile-model"),
            "cli-model",
            ModelSource::CliOverride,
        ),
        (
            Some("cli-model"),
            Some("other-profile-model"),
            "cli-model",
            ModelSource::CliOverride,
        ),
        (
            None,
            Some("other-profile-model"),
            "other-profile-model",
            ModelSource::Profile,
        ),
        (None, None, DEFAULT_MODEL, ModelSource::Default),
    ] {
        options.model_override = cli.map(str::to_owned);
        options.profiles[0].model = profile.map(str::to_owned);
        dispatch(
            &mut app,
            Action::Enter,
            &client,
            &mut options,
            &mut running,
            &mut history,
            &StopFlag::default(),
        )
        .await;
        assert_eq!((app.model.as_str(), app.model_source), (expected, source));
        assert_eq!(
            (options.model.as_str(), options.model_source),
            (expected, source)
        );
    }
}

#[test]
fn shell_paging_works_while_composing_and_tab_remains_reachable() {
    let mut app = console();
    let stop = StopFlag::default();
    app.pane = Pane::Shell;
    app.mode = Mode::Composing;
    app.shell_viewport = (10, 3);
    app.composer = "typed command".into();
    for index in 0..8 {
        app.start_shell(format!("command {index} long"));
        app.fail_shell(crate::ErrorReport::from_message(
            "run shell command",
            "output of several visual rows",
        ));
    }
    on_key(&mut app, press(KeyCode::PageUp), &stop);
    assert!(app.shell_top.is_some());
    on_key(&mut app, press(KeyCode::Home), &stop);
    assert_eq!(app.shell_top, Some(0));
    on_key(&mut app, press(KeyCode::PageDown), &stop);
    assert!(app.shell_top.unwrap_or_default() > 0);
    on_key(&mut app, press(KeyCode::End), &stop);
    assert_eq!(app.shell_top, None);
    assert_eq!(app.composer, "typed command");
    on_key(&mut app, press(KeyCode::Tab), &stop);
    assert_eq!(app.pane, Pane::Detail);
}

#[test]
fn busy_shell_refuses_second_submission_and_escape_requests_stop_while_typing() {
    let mut app = console();
    let stop = StopFlag::default();
    app.pane = Pane::Shell;
    app.mode = Mode::Composing;
    app.start_shell("first".into());
    app.composer = "second".into();
    assert_eq!(on_key(&mut app, press(KeyCode::Enter), &stop), None);
    assert_eq!(app.shell_history.len(), 1);
    assert_eq!(app.composer, "second");
    on_key(&mut app, press(KeyCode::Esc), &stop);
    assert!(stop.is_requested());
    assert_eq!(app.mode, Mode::Composing);
}

/// A socket that answers each connection with the next canned frame, as brokerd would.
fn fake_broker(responses: Vec<Value>) -> (tempfile::TempDir, BrokerClient) {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.path().join("b.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    tokio::spawn(async move {
        for response in responses {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = dekopon_broker_protocol::DescriptorStream::new(stream);
            let (_request, _descriptors) = stream
                .read_frame::<Value>(FrameLimits::default())
                .await
                .unwrap();
            stream
                .write_frame(&response, &[], FrameLimits::default())
                .await
                .unwrap();
        }
    });
    let client = BrokerClient::new(
        &path,
        rustix::process::geteuid().as_raw(),
        FrameLimits::default(),
    )
    .unwrap();
    (directory, client)
}

fn refusal_frame() -> Value {
    serde_json::to_value(dekopon_broker_protocol::ResponseEnvelope::error(
        "unauthenticated",
        "attestation refused: no attestor authority for this subject",
    ))
    .unwrap()
}

fn empty_surface_frame() -> Value {
    serde_json::to_value(dekopon_broker_protocol::ResponseEnvelope::capabilities(
        Vec::new(),
        Vec::new(),
        std::collections::BTreeMap::new(),
    ))
    .unwrap()
}

async fn hop(app: &mut App, client: &BrokerClient) {
    let mut options = ConsoleOptions::new(app.subject.parse().unwrap(), "test-model".into());
    let mut history = History::new(options.history_limits);
    dispatch(
        app,
        Action::Enter,
        client,
        &mut options,
        &mut None,
        &mut history,
        &StopFlag::default(),
    )
    .await;
}

/// Every row of one drawn frame, so a test reads what the operator would.
fn drawn(app: &App) -> String {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 40)).unwrap();
    terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    buffer
        .content()
        .chunks(usize::from(buffer.area.width))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_contains_all(text: &str, expected: &[&str]) {
    for fragment in expected {
        assert!(text.contains(fragment), "missing {fragment:?} in:\n{text}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_hop_opens_the_broker_refusal_in_full() {
    let (_directory, client) = fake_broker(vec![refusal_frame()]);
    let mut app = console();
    hop(&mut app, &client).await;

    assert!(app.session.is_none());
    assert_eq!(app.mode, Mode::Error, "a refused hop opens its report");
    let report = app.last_error.as_ref().unwrap().to_string();
    assert_contains_all(
        &report,
        &[
            "open broker session failed",
            "subject: slack.t0123abc.u9xyz",
            "agent: ville-github",
            "scope: subject-only",
            "error: could not open a broker session for this subject and agent: broker returned \
             unauthenticated: attestation refused: no attestor authority for this subject",
            "broker refusal code: unauthenticated",
            "broker refusal message: attestation refused: no attestor authority for this subject",
            "event=broker_capabilities_refused, subject=slack.t0123abc.u9xyz, agent=ville-github",
        ],
    );
    assert_contains_all(
        &drawn(&app),
        &[
            "broker refusal code: unauthenticated",
            "broker refusal message: attestation refused: no attestor authority for this subject",
            "event=broker_capabilities_refused,",
        ],
    );

    let stop = StopFlag::default();
    on_key(&mut app, press(KeyCode::Esc), &stop);
    assert_eq!(app.mode, Mode::Browsing);
    let status = drawn(&app);
    assert!(
        status.contains("attestation refused: no attestor authority for this subject"),
        "the status line keeps the refusal message: {status}"
    );
    on_key(&mut app, press(KeyCode::Char('e')), &stop);
    assert_eq!(app.mode, Mode::Error, "e reopens the last error");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_undecodable_broker_answer_names_the_decode_error() {
    let mut frame = refusal_frame();
    frame["response"] = json!({"type": "bogus"});
    let (_directory, client) = fake_broker(vec![frame]);
    let mut app = console();
    hop(&mut app, &client).await;

    let report = app.last_error.as_ref().unwrap();
    assert_eq!(report.refusal, None, "a decode failure is not a refusal");
    assert_contains_all(
        &report.to_string(),
        &[
            "error: could not open a broker session for this subject and agent: broker response \
             framing failed: broker frame is not valid protocol JSON: unknown variant `bogus`",
            "agent: ville-github",
        ],
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_absent_broker_names_the_transport_cause() {
    let directory = tempfile::tempdir().unwrap();
    let client = BrokerClient::new(
        directory.path().join("absent.sock"),
        rustix::process::geteuid().as_raw(),
        FrameLimits::default(),
    )
    .unwrap();
    let mut app = console();
    hop(&mut app, &client).await;

    let report = app.last_error.as_ref().unwrap().to_string();
    assert_contains_all(
        &report,
        &[
            "error: could not open a broker session for this subject and agent: could not \
             inspect broker socket: ",
            "(os error 2)",
            "broker socket: /run/dekopon/broker.sock",
        ],
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_surface_says_what_the_broker_accepted() {
    let (_directory, client) = fake_broker(vec![empty_surface_frame()]);
    let mut app = console();
    hop(&mut app, &client).await;

    assert!(app.session.is_none());
    assert_eq!(app.mode, Mode::Error);
    assert_contains_all(
        &app.last_error.as_ref().unwrap().to_string(),
        &[
            "error: broker grants no capabilities for this context",
            "permitted agent.prompt",
            "context.agent == \"ville-github\"",
            "subject: slack.t0123abc.u9xyz",
        ],
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_record_uses_existing_frame_and_binds_invocation() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.path().join("b.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let client = BrokerClient::new(
        &path,
        rustix::process::geteuid().as_raw(),
        FrameLimits::default(),
    )
    .unwrap();
    let response = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = dekopon_broker_protocol::DescriptorStream::new(stream);
            let (request, _) = stream
                .read_frame::<Value>(FrameLimits::default())
                .await
                .unwrap();
            stream
                .write_frame(&refusal_frame(), &[], FrameLimits::default())
                .await
                .unwrap();
            requests.push(request);
        }
        requests
    });
    let result = crate::session::smoke_record(
        &client,
        "slack.t0123abc.u9xyz".parse().unwrap(),
        "ville-github".parse().unwrap(),
        "marker".into(),
    )
    .await;
    assert!(result.is_err());
    let second = crate::session::smoke_record(
        &client,
        "slack.t0123abc.u9xyz".parse().unwrap(),
        "ville-github".parse().unwrap(),
        "second".into(),
    )
    .await;
    assert!(second.is_err());
    let requests = response.await.unwrap();
    for request in &requests {
        let wire = request["request"].to_string();
        assert!(wire.contains("recordDeliveredTurn"), "{wire}");
        assert!(wire.contains("console-smoke"), "{wire}");
        assert_eq!(
            request["request"]["attestation"]["invocation"],
            request["request"]["turn"]["id"]
        );
    }
    let turn = |index: usize| &requests[index]["request"]["turn"];
    assert_eq!(turn(0)["user"], "marker");
    assert_eq!(turn(1)["user"], "second");
    assert_ne!(turn(0)["id"], turn(1)["id"]);
    assert_eq!(
        turn(0)["delivery"]["bootNonce"],
        turn(1)["delivery"]["bootNonce"]
    );
    assert_ne!(turn(0)["traceParent"], turn(1)["traceParent"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_record_without_flag_is_local_refusal() {
    let (_directory, client) = fake_broker(vec![empty_surface_frame()]);
    let mut app = console();
    let leg = crate::session::open_agent(
        client.clone(),
        app.subject.parse().unwrap(),
        "ville-github".parse().unwrap(),
        None,
    )
    .await
    .unwrap();
    app.enter(crate::session::AgentSession::new(
        "ville-github".parse().unwrap(),
        leg,
        dekopon_agent::prompt::HistoryLimits::default(),
    ));
    let mut options = ConsoleOptions::new(app.subject.parse().unwrap(), "test-model".into());
    let mut history = History::new(options.history_limits);
    dispatch(
        &mut app,
        Action::Shell(":smoke record 'marker'".into()),
        &client,
        &mut options,
        &mut None,
        &mut history,
        &StopFlag::default(),
    )
    .await;
    assert!(
        app.notice
            .as_ref()
            .unwrap()
            .text
            .contains("--smoke-conversation")
    );
    assert!(app.shell_history.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_shell_command_is_written_in_full_into_the_transcript() {
    let (_directory, client) = fake_broker(vec![empty_surface_frame(), refusal_frame()]);
    let mut app = console();
    let leg = crate::session::open_agent(
        client.clone(),
        app.subject.parse().unwrap(),
        "ville-github".parse().unwrap(),
        None,
    )
    .await
    .unwrap();
    app.enter(crate::session::AgentSession::new(
        "ville-github".parse().unwrap(),
        leg,
        dekopon_agent::prompt::HistoryLimits::default(),
    ));
    app.assets = Some(std::sync::Arc::new(
        crate::session::assets::ConsoleAssets::default(),
    ));
    let mut options = ConsoleOptions::new(app.subject.parse().unwrap(), "test-model".into());
    let mut history = History::new(options.history_limits);
    dispatch(
        &mut app,
        Action::Shell("gh issue list".into()),
        &client,
        &mut options,
        &mut None,
        &mut history,
        &StopFlag::default(),
    )
    .await;

    let entry = app.shell_history.last().unwrap();
    assert_eq!(entry.exit_code, Some(1));
    assert_contains_all(
        &entry.output,
        &[
            "open broker session for a shell command failed",
            "broker returned unauthenticated: attestation refused",
            "broker refusal code: unauthenticated",
            "agent: ville-github",
        ],
    );
    assert!(!app.busy);
    assert_contains_all(
        &drawn(&app),
        &["broker refusal code: unauthenticated", "[exit code: 1]"],
    );
}
