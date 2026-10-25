//! Bounded line transport over the same shell dispatcher as the interactive console.
use std::{
    fs::File,
    io::{self, Read as _},
    time::{Duration, Instant},
};

use dekopon_agent::prompt::History;
use dekopon_broker_protocol::BrokerClient;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncRead, BufReader},
    task::JoinError,
};

use crate::{
    App, ConsoleExit,
    record::SessionEvent,
    redact::sanitize_line,
    run::{Action, RunningTurn, dispatch, settle_turn},
    session::{ConsoleOptions, SessionError, StopFlag},
};

const MAX_LINE: usize = 16 * 1024;
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_COMMANDS: usize = 64;
const CANCEL_GRACE: Duration = Duration::from_secs(5);

fn nonce() -> io::Result<String> {
    let mut bytes = [0u8; 24];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn write_result(out: &mut impl io::Write, nonce: &str, status: &str, text: &str) -> io::Result<()> {
    writeln!(out, "RESULT {nonce} {status}")?;
    let prefixes = [
        format!("READY {nonce}"),
        format!("RESULT {nonce}"),
        format!("END {nonce}"),
    ];
    let mut remaining = MAX_OUTPUT;
    for line in text.split('\n') {
        if remaining == 0 {
            break;
        }
        let safe = sanitize_line(line);
        let escaped = if prefixes.iter().any(|prefix| safe.starts_with(prefix)) {
            format!("> {safe}")
        } else {
            safe
        };
        let clipped = escaped.chars().take(remaining).collect::<String>();
        remaining -= clipped.chars().count();
        writeln!(out, "{clipped}")?;
    }
    if remaining == 0 {
        writeln!(out, "[output truncated]")?;
    }
    writeln!(out, "END {nonce}")?;
    out.flush()
}

async fn read_line<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unterminated command",
            ));
        }
        let count = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1);
        if bytes.len() + count > MAX_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "command exceeds 16 KiB",
            ));
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
    }
}

/// Drains the turn's events to channel close, then joins it without giving up the handle.
async fn complete(
    app: &mut App,
    turn: &mut RunningTurn,
) -> Result<Result<History, SessionError>, JoinError> {
    while let Some(event) = turn.events.recv().await {
        if let SessionEvent::ShellFinished(entry) = event {
            app.finish_shell(entry);
        }
    }
    (&mut turn.handle).await
}

/// Stops a timed-out turn through the broker cancel, aborting it only once the grace runs out.
async fn cancel_turn(running: &mut Option<RunningTurn>, stop: &StopFlag, grace: Duration) {
    let Some(turn) = running.as_mut() else {
        return;
    };
    stop.request();
    if tokio::time::timeout(grace, &mut turn.handle).await.is_err() {
        turn.handle.abort();
        if let Err(error) = (&mut turn.handle).await {
            tracing::debug!(%error, "timed-out structured command aborted");
        }
    }
    *running = None;
    stop.reset();
}

/// Run the selected agent and accept at most 64 sequential commands. No input is echoed.
/// READY is emitted only after a successful hop; END follows event drain and task join.
/// # Errors
/// Returns bounded input, output or session failures.
pub async fn run(
    app: App,
    client: BrokerClient,
    options: ConsoleOptions,
) -> Result<(), ConsoleExit> {
    run_with_limits(
        app,
        client,
        options,
        BufReader::new(tokio::io::stdin()),
        io::stdout().lock(),
        Duration::from_secs(120),
        CANCEL_GRACE,
    )
    .await
}

async fn run_with_limits<R: AsyncRead + Unpin, W: io::Write>(
    mut app: App,
    client: BrokerClient,
    mut options: ConsoleOptions,
    mut input: BufReader<R>,
    mut out: W,
    command_timeout: Duration,
    grace: Duration,
) -> Result<(), ConsoleExit> {
    let deadline = Instant::now() + Duration::from_secs(600);
    let nonce = nonce().map_err(ConsoleExit::Terminal)?;
    let stop = StopFlag::default();
    let mut running: Option<RunningTurn> = None;
    let mut history = History::new(options.history_limits);
    tokio::time::timeout(
        Duration::from_secs(120),
        dispatch(
            &mut app,
            Action::Enter,
            &client,
            &mut options,
            &mut running,
            &mut history,
            &stop,
        ),
    )
    .await
    .map_err(|_elapsed| ConsoleExit::Structured("agent entry deadline exceeded".into()))?;
    if app.session.is_none() {
        return Err(ConsoleExit::Structured("agent entry refused".into()));
    }
    writeln!(out, "READY {nonce}")
        .and_then(|()| out.flush())
        .map_err(ConsoleExit::Terminal)?;
    for _ in 0..MAX_COMMANDS {
        let line = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            read_line(&mut input),
        )
        .await
        .map_err(|_elapsed| ConsoleExit::Structured("session deadline exceeded".into()))?
        .map_err(ConsoleExit::Terminal)?;
        let Some(line) = line else {
            return Ok(());
        };
        let before = app.shell_history.len();
        app.notice = None;
        let command_deadline = Instant::now() + command_timeout;
        let finished = tokio::time::timeout(
            command_deadline
                .saturating_duration_since(Instant::now())
                .min(deadline.saturating_duration_since(Instant::now())),
            async {
                dispatch(
                    &mut app,
                    Action::Shell(line),
                    &client,
                    &mut options,
                    &mut running,
                    &mut history,
                    &stop,
                )
                .await;
                match running.as_mut() {
                    Some(turn) => Some(complete(&mut app, turn).await),
                    None => None,
                }
            },
        )
        .await;
        match finished {
            Ok(Some(joined)) => {
                let shell = running.take().expect("turn").shell;
                settle_turn(&mut app, shell, joined, &mut history, &stop);
            }
            Ok(None) => {}
            Err(_elapsed) => {
                cancel_turn(&mut running, &stop, grace).await;
                write_result(
                    &mut out,
                    &nonce,
                    "timeout",
                    "outcome unknown: the command timed out and may have run",
                )
                .map_err(ConsoleExit::Terminal)?;
                return Err(ConsoleExit::Structured(
                    "command deadline exceeded; outcome uncertain; remaining commands refused"
                        .into(),
                ));
            }
        }
        let entry = app.shell_history.get(before);
        let status = match entry.and_then(|e| e.exit_code) {
            Some(0) => "ok",
            _ => "error",
        };
        let message = entry.map_or_else(
            || {
                app.notice
                    .as_ref()
                    .map_or("command refused", |n| n.text.as_str())
            },
            |e| e.output.as_str(),
        );
        write_result(&mut out, &nonce, status, message).map_err(ConsoleExit::Terminal)?;
    }
    // Accept exactly 64 completed commands, but never silently drop a 65th submitted line.
    // EOF is success; a further newline-terminated command receives its own refusal frame.
    if tokio::time::timeout(
        deadline.saturating_duration_since(Instant::now()),
        read_line(&mut input),
    )
    .await
    .map_err(|_elapsed| ConsoleExit::Structured("session deadline exceeded".into()))?
    .map_err(ConsoleExit::Terminal)?
    .is_some()
    {
        write_result(
            &mut out,
            &nonce,
            "error",
            "command limit exceeded; command not run",
        )
        .map_err(ConsoleExit::Terminal)?;
        return Err(ConsoleExit::Structured(
            "session command limit exceeded".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dekopon_broker_protocol::FrameLimits;
    use dekopon_process::CancelSignal;
    use dekopon_protocol::{Agent, AgentKind, AgentSpec, ApiVersion, ObjectMeta};
    use std::{
        process::{Child, Command, Stdio},
        time::Instant,
    };

    struct BrokerFixture {
        child: Child,
        dir: tempfile::TempDir,
    }

    impl Drop for BrokerFixture {
        fn drop(&mut self) {
            drop(self.child.kill());
            drop(self.child.wait());
        }
    }

    fn broker_fixture() -> Option<BrokerFixture> {
        use std::os::unix::fs::PermissionsExt as _;
        let broker = std::env::var_os("DEKOPON_TEST_BROKERD")?;
        let wasm = std::env::var_os("DEKOPON_TEST_PROBE_WASM")?;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let component = dir.path().join("probe.wasm");
        let policy = dir.path().join("policy.cedar");
        let config = dir.path().join("broker.yaml");
        let socket = dir.path().join("broker.sock");
        std::fs::write(&component, std::fs::read(wasm).unwrap()).unwrap();
        std::fs::write(&policy, r#"@id("console-agent") permit(principal == Dekopon::Principal::"maintainer", action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"reviewer") when { context.via == "dekopon-console" };
@id("console-read") permit(principal == Dekopon::Principal::"maintainer", action == Dekopon::Action::"cli-probe.upper", resource == Dekopon::Provider::"cli-probe") when { context.via == "dekopon-console" && context.agent == "reviewer" };"#).unwrap();
        std::fs::write(&config, format!("apiVersion: dekopon.dev/brokerd/v1alpha1\nsocketPath: {}\npoliciesPath: {}\nproviders: [{}]\nidentities:\n  - uid: {}\n    principal: dekopon-console\n    attestor:\n      namespaces: [slack.t0123abc]\nprincipals:\n  maintainer:\n    subjects: [slack.t0123abc.u9xyz]\ncapabilities:\n  cli-probe:\n    capabilities:\n      cli-probe.upper:\n        constraints: {{timeoutMs: 30000}}\n", socket.display(), policy.display(), component.display(), rustix::process::geteuid().as_raw())).unwrap();
        for path in [&component, &policy, &config] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let log = std::fs::File::create(dir.path().join("broker.log")).unwrap();
        let mut child = Command::new(broker)
            .args(["--config", config.to_str().unwrap()])
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !socket.exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "broker exited {status}: {}",
                    std::fs::read_to_string(dir.path().join("broker.log")).unwrap()
                );
            }
            assert!(Instant::now() < deadline, "broker fixture startup deadline");
            std::thread::sleep(Duration::from_millis(10));
        }
        Some(BrokerFixture { child, dir })
    }

    #[tokio::test]
    async fn deadline_frames_unknown_outcome_and_refuses_next_input() {
        let Some(fixture) = broker_fixture() else {
            eprintln!("broker fixture env absent; deadline integration skipped");
            return;
        };
        let socket = fixture.dir.path().join("broker.sock");
        let subject = "slack.t0123abc.u9xyz".parse().unwrap();
        let mut options = ConsoleOptions::new(subject, "unused".into());
        options.initial_agent = Some("reviewer".parse().unwrap());
        let agent = Agent {
            api_version: ApiVersion::V1Alpha1,
            kind: AgentKind::Agent,
            metadata: ObjectMeta::named("reviewer"),
            spec: AgentSpec {
                description: "fixture".into(),
                enabled: true,
                instructions: None,
                skills: Vec::new(),
                model_class: None,
                instructions_file: None,
            },
            status: None,
        };
        let app = App::new(
            vec![agent],
            "slack.t0123abc.u9xyz".into(),
            socket.display().to_string(),
            "none".into(),
        );
        let client = BrokerClient::new(
            socket,
            rustix::process::geteuid().as_raw(),
            FrameLimits::default(),
        )
        .unwrap();
        let mut output = Vec::new();
        let started = Instant::now();
        let result = run_with_limits(
            app,
            client,
            options,
            BufReader::new(&b"sleep 1\necho should-not-run\n"[..]),
            &mut output,
            Duration::from_millis(20),
            Duration::from_millis(1),
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "grace must not wait for local sleep"
        );
        assert!(
            matches!(result, Err(ConsoleExit::Structured(_))),
            "{result:?}"
        );
        let text = String::from_utf8(output).unwrap();
        let lines: Vec<_> = text.lines().collect();
        let nonce = lines[0].strip_prefix("READY ").unwrap();
        assert_eq!(
            lines,
            [
                format!("READY {nonce}"),
                format!("RESULT {nonce} timeout"),
                "outcome unknown: the command timed out and may have run".to_owned(),
                format!("END {nonce}")
            ]
        );
        assert!(!text.contains("ok") && !text.contains("should-not-run"));
    }

    #[tokio::test]
    async fn timeout_writes_end_then_exits_nonzero_without_dispatching_next_line() {
        const CHILD: &str = "DEKOPON_STRUCTURED_TIMEOUT_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let Some(fixture) = broker_fixture() else {
                panic!("broker fixture required for child");
            };
            let socket = fixture.dir.path().join("broker.sock");
            let subject = "slack.t0123abc.u9xyz".parse().unwrap();
            let mut options = ConsoleOptions::new(subject, "unused".into());
            options.initial_agent = Some("reviewer".parse().unwrap());
            let agent = Agent {
                api_version: ApiVersion::V1Alpha1,
                kind: AgentKind::Agent,
                metadata: ObjectMeta::named("reviewer"),
                spec: AgentSpec {
                    description: "fixture".into(),
                    enabled: true,
                    instructions: None,
                    skills: Vec::new(),
                    model_class: None,
                    instructions_file: None,
                },
                status: None,
            };
            let app = App::new(
                vec![agent],
                "slack.t0123abc.u9xyz".into(),
                socket.display().to_string(),
                "none".into(),
            );
            let client = BrokerClient::new(
                socket,
                rustix::process::geteuid().as_raw(),
                FrameLimits::default(),
            )
            .unwrap();
            // The test harness exits nonzero when the structured Result is an error, just
            // as main maps ConsoleError to ExitCode::FAILURE. No retry of either input.
            run_with_limits(
                app,
                client,
                options,
                BufReader::new(&b"sleep 1\necho should-not-run\n"[..]),
                io::stdout().lock(),
                Duration::from_millis(20),
                Duration::from_millis(1),
            )
            .await
            .unwrap();
            return;
        }
        if std::env::var_os("DEKOPON_TEST_BROKERD").is_none()
            || std::env::var_os("DEKOPON_TEST_PROBE_WASM").is_none()
        {
            eprintln!("broker fixture env absent; subprocess deadline witness skipped");
            return;
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "structured::tests::timeout_writes_end_then_exits_nonzero_without_dispatching_next_line", "--nocapture"])
            .env(CHILD, "1").output().unwrap();
        assert!(
            !output.status.success(),
            "timed-out session must exit nonzero"
        );
        let text = String::from_utf8(output.stdout).unwrap();
        let nonce = text
            .lines()
            .find_map(|line| line.strip_prefix("READY "))
            .expect("READY in subprocess output");
        assert!(text.contains(&format!("RESULT {nonce} timeout\noutcome unknown: the command timed out and may have run\nEND {nonce}\n")), "{text}");
        assert_eq!(
            text.matches(&format!("RESULT {nonce} ")).count(),
            1,
            "second input must be refused: {text}"
        );
        assert!(!text.contains(&format!("RESULT {nonce} ok")));
    }

    #[tokio::test]
    async fn broker_cancel_is_observed_and_owned_task_joins() {
        let stop = StopFlag::default();
        let (cancel, signal) = CancelSignal::pair();
        stop.bind_broker(cancel);
        let (sender, events) = crate::session::session_channel();
        let probe = signal.clone();
        let handle = tokio::spawn(async move {
            while !probe.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            drop(sender);
            Ok(History::default())
        });
        let mut running = Some(RunningTurn {
            events,
            handle,
            shell: true,
        });
        cancel_turn(&mut running, &stop, Duration::from_millis(100)).await;
        assert!(
            signal.is_cancelled(),
            "the bound broker CancelSignal must fire"
        );
        assert!(running.is_none(), "task handle joined before return");
    }

    #[tokio::test]
    async fn ignored_cancel_is_aborted_and_joined_after_grace() {
        const CHILD: &str = "DEKOPON_STRUCTURED_GRACE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "structured::tests::ignored_cancel_is_aborted_and_joined_after_grace",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "ignored cancellation must exit nonzero"
            );
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(text.contains("RESULT abc timeout\noutcome unknown: the command timed out and may have run\nEND abc\n"), "{text}");
            assert!(!text.contains("RESULT abc ok"));
            return;
        }
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let stop = StopFlag::default();
        let (cancel, signal) = CancelSignal::pair();
        stop.bind_broker(cancel);
        let (sender, events) = crate::session::session_channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&dropped);
        let (started, ready) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _guard = Dropped(observed);
            assert!(started.send(()).is_ok());
            let _keep_channel_open = sender;
            std::future::pending::<()>().await;
            #[allow(unreachable_code)]
            Ok(History::default())
        });
        ready.await.unwrap();
        let mut running = Some(RunningTurn {
            events,
            handle,
            shell: true,
        });
        cancel_turn(&mut running, &stop, Duration::from_millis(10)).await;
        assert!(signal.is_cancelled());
        assert!(
            dropped.load(Ordering::SeqCst),
            "abort was awaited, not detached"
        );
        assert!(running.is_none());
        write_result(
            &mut io::stdout().lock(),
            "abc",
            "timeout",
            "outcome unknown: the command timed out and may have run",
        )
        .unwrap();
        fn reject() -> Result<(), ConsoleExit> {
            Err(ConsoleExit::Structured(
                "command deadline exceeded; outcome uncertain; remaining commands refused".into(),
            ))
        }
        reject().expect("timed-out command must return an error");
    }

    #[test]
    fn markers_cannot_be_forged_by_output() {
        let mut out = Vec::new();
        write_result(
            &mut out,
            "abc",
            "error",
            "READY abc\nRESULT abc ok\nEND abc\nREADY other\nhi\tthere\u{1b}[31m",
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\n> READY abc\n> RESULT abc ok\n> END abc\nREADY other\n"));
        assert!(!text.contains('\u{1b}'));
        assert!(text.ends_with("END abc\n"));
    }

    #[test]
    fn bounded_output_never_inserts_a_marker_after_truncation() {
        let mut out = Vec::new();
        write_result(
            &mut out,
            "abc",
            "ok",
            &format!("{}\nEND abc", "x".repeat(MAX_OUTPUT)),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("[output truncated]"));
        assert_eq!(text.matches("END abc\n").count(), 1);
    }
}
