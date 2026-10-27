//! Terminal lifecycle and the event loop that joins keys to sessions.
//!
//! Two things here are worth more care than they look. The terminal is restored on *every* exit
//! path including a panic, because a panic inside a raw-mode alternate screen leaves the operator
//! with a shell that no longer echoes. And `tracing` never reaches stdout while this runs, because
//! this process owns the screen and one stray log line corrupts a frame.

use std::{
    io::{self, Stdout},
    panic,
    sync::Arc,
};

use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use dekopon_agent::{
    ShellRuntime,
    prompt::{History, HistoryLimits, ScriptRuntime},
};
use dekopon_broker_protocol::BrokerClient;
use dekopon_model::model::ChatModel;
use dekopon_process::CallBudget;
use dekopon_process::CancelSignal;
use futures_util::StreamExt as _;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::Instrument as _;

use crate::{
    app::{App, Mode, Notice, Pane, ShellEntry},
    record::{RecordingProgress, SessionEvent},
    report::ErrorReport,
    session::{
        AgentSession, ConsoleOptions, LegHandle, SessionError, StopFlag, build_model,
        open_agent_with_assets, session_channel,
    },
    ui,
};

/// Restores the terminal on drop, whatever the reason for the drop.
///
/// A guard rather than a call at the end of `run`: an error return, a `?`, and a panic all have to
/// leave the terminal usable, and only a destructor covers all three.
struct TerminalGuard;

impl TerminalGuard {
    /// Enters the alternate screen and installs a panic hook that leaves it.
    fn enter() -> io::Result<(Self, Terminal<CrosstermBackend<Stdout>>)> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        // No mouse capture. Every mouse event this loop received was discarded, and capturing them
        // took the terminal's own text selection and scrollback away from the operator to do it —
        // which is exactly what an operator wants after revealing a field.
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            if let Err(restore_error) = disable_raw_mode() {
                eprintln!("warning: raw mode cleanup failed: {restore_error}");
            }
            return Err(error);
        }
        let terminal = match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => terminal,
            Err(error) => {
                restore();
                return Err(error);
            }
        };

        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            // Restore first, then let the default hook print: a backtrace drawn into the alternate
            // screen vanishes with it, which is how a panic becomes a silent exit.
            restore();
            previous(info);
        }));

        Ok((Self, terminal))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Puts the terminal back the way it was found.
///
/// Both failures are reported to standard error rather than through `tracing`: this runs while the
/// screen is being given back, including from a panic hook, and a terminal left in raw mode is
/// something the operator has to fix with `reset` — so the reason has to reach them somewhere that
/// survives the subscriber.
fn restore() {
    if let Err(error) = disable_raw_mode() {
        eprintln!("warning: could not leave raw mode ({error}); run `reset` to restore your shell");
    }
    if let Err(error) = execute!(io::stdout(), LeaveAlternateScreen) {
        eprintln!("warning: could not leave the alternate screen ({error}); run `reset`");
    }
}

/// One turn in flight, and what the console needs back when it finishes.
pub(crate) struct RunningTurn {
    pub(crate) events: UnboundedReceiver<SessionEvent>,
    pub(crate) handle: tokio::task::JoinHandle<Result<History, SessionError>>,
    pub(crate) shell: bool,
}

/// Runs the console until the operator quits.
///
/// # Errors
///
/// Returns a terminal failure. Every other failure — an unreachable broker, a refused hop, a
/// session that died — is drawn rather than returned, because the console is still usable after
/// each of them.
pub async fn run(
    mut app: App,
    client: BrokerClient,
    mut options: ConsoleOptions,
) -> Result<(), ConsoleExit> {
    let (guard, mut terminal) = TerminalGuard::enter().map_err(ConsoleExit::Terminal)?;

    let stop = StopFlag::default();
    let mut keys = EventStream::new();
    let mut running: Option<RunningTurn> = None;
    let mut history = History::new(options.history_limits);
    if options.initial_agent.is_some() {
        dispatch(
            &mut app,
            Action::Enter,
            &client,
            &mut options,
            &mut running,
            &mut history,
            &stop,
        )
        .await;
    }

    let result = drive_event_loop(
        &mut terminal,
        &mut app,
        &client,
        &mut options,
        &mut keys,
        &mut running,
        &mut history,
        &stop,
    )
    .await;
    drop(terminal);
    drop(guard);
    // Back on the ordinary screen, the last failure lands in scrollback where it can be copied.
    if let Some(report) = &app.last_error {
        eprintln!("last console error:\n{report}");
    }
    result
}

#[expect(
    clippy::too_many_arguments,
    reason = "the loop owns the current terminal, inputs and cancellable session state together"
)]
async fn drive_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    client: &BrokerClient,
    options: &mut ConsoleOptions,
    keys: &mut EventStream,
    running: &mut Option<RunningTurn>,
    history: &mut History,
    stop: &StopFlag,
) -> Result<(), ConsoleExit> {
    loop {
        if let Ok(size) = terminal.size() {
            app.shell_viewport = (size.width.saturating_sub(2), size.height.saturating_sub(11));
        }
        if let Err(error) = terminal.draw(|frame| ui::draw(frame, app)) {
            stop_running(running, stop).await;
            return Err(ConsoleExit::Terminal(error));
        }
        if app.should_quit {
            stop_running(running, stop).await;
            return Ok(());
        }

        // One select over both inputs, so a churning turn keeps redrawing while keys stay live —
        // which is what makes Esc able to stop it.
        tokio::select! {
            key = keys.next() => match key {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    if let Some(action) = on_key(app, key, stop) {
                        dispatch(app, action, client, options, running,
                                 history, stop).await;
                    }
                }
                Some(Ok(Event::Resize(width, height))) => {
                    app.shell_viewport = (width.saturating_sub(2), height.saturating_sub(11));
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => { stop_running(running, stop).await; return Err(ConsoleExit::Terminal(error)); },
                None => { app.should_quit = true; },
            },
            event = recv(running) => {
                match event {
                    Some(SessionEvent::Progress(message)) => app.notice = Some(Notice::info(message)),
                    Some(SessionEvent::ShellFinished(entry)) => app.finish_shell(entry),
                    Some(event) => app.on_session_event(event),
                    None => finish_turn(app, running, history, stop).await,
                }
            }
        }
    }
}

/// Join the blocking turn before releasing its broker leg and restoring the terminal.
async fn stop_running(running: &mut Option<RunningTurn>, stop: &StopFlag) {
    if let Some(turn) = running.take() {
        stop.request();
        if let Err(error) = turn.handle.await {
            tracing::warn!(%error, "stopped console task did not join cleanly");
        }
    }
}

/// Waits for the next session event, or for there to be no session.
///
/// Pending forever when nothing is running, so `select!` simply never takes this branch rather than
/// spinning on an immediately-ready `None`.
async fn recv(running: &mut Option<RunningTurn>) -> Option<SessionEvent> {
    match running {
        Some(turn) => turn.events.recv().await,
        None => std::future::pending().await,
    }
}

/// Collects a finished turn's history back and reports how it ended.
async fn finish_turn(
    app: &mut App,
    running: &mut Option<RunningTurn>,
    history: &mut History,
    stop: &StopFlag,
) {
    let Some(turn) = running.take() else {
        return;
    };
    let joined = turn.handle.await;
    settle_turn(app, turn.shell, joined, history, stop);
}

/// Reports how a joined turn ended and releases its stop binding.
pub(crate) fn settle_turn(
    app: &mut App,
    shell: bool,
    joined: Result<Result<History, SessionError>, tokio::task::JoinError>,
    history: &mut History,
    stop: &StopFlag,
) {
    match joined {
        Ok(Ok(returned)) => *history = returned,
        // The session's own history is lost, so the model's replay window is now a guess. Saying so
        // is better than silently continuing against a window that no longer matches the screen.
        Ok(Err(error)) if shell => {
            app.fail_shell(ErrorReport::from_error("run shell command", &error));
        }
        Err(error) if shell => app.fail_shell(ErrorReport::from_error(
            "run shell command (the shell task died)",
            &error,
        )),
        Ok(Err(error)) => {
            let report = app.report(ErrorReport::from_error("run model turn", &error));
            app.raise(report);
        }
        Err(error) => {
            let report = app.report(ErrorReport::from_error(
                "run model turn (the session task died)",
                &error,
            ));
            app.raise(report);
        }
    }
    stop.reset();
    if shell {
        if app
            .shell_history
            .last()
            .is_some_and(|entry| entry.exit_code.is_none())
        {
            app.fail_shell(ErrorReport::from_message(
                "run shell command",
                "shell ended without reporting an outcome",
            ));
        }
        app.busy = false;
    } else {
        app.on_session_complete(history.len());
    }
}

/// Something the loop must do that needs more than the state machine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Hop into the highlighted agent.
    Enter,
    /// Run one prompt as a turn.
    Turn(String),
    /// Run one line through the interpreter.
    Shell(String),
}

/// Maps one key press onto a state change, and possibly an action.
///
/// Split from the loop so the whole of the console's key handling is testable without a terminal.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled terminal key codes are ignored to preserve console input behavior"
)]
pub fn on_key(app: &mut App, key: KeyEvent, stop: &StopFlag) -> Option<Action> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return None;
    }
    if app.mode == Mode::Help {
        app.mode = Mode::Browsing;
        return None;
    }
    if app.mode == Mode::Error {
        let page = app.shell_viewport.1.max(1);
        let scroll = match key.code {
            KeyCode::Char('j') | KeyCode::Down => app.error_scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => app.error_scroll.saturating_sub(1),
            KeyCode::PageDown => app.error_scroll.saturating_add(page),
            KeyCode::PageUp => app.error_scroll.saturating_sub(page),
            KeyCode::Home => 0,
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | 'e') => {
                app.mode = Mode::Browsing;
                return None;
            }
            _ => return None,
        };
        app.error_scroll = scroll.min(crate::ui::chrome::error_max_scroll(app));
        return None;
    }
    if app.pane == Pane::Shell {
        match key.code {
            KeyCode::PageUp => {
                crate::ui::shell::page(app, -1);
                return None;
            }
            KeyCode::PageDown => {
                crate::ui::shell::page(app, 1);
                return None;
            }
            KeyCode::Home => {
                crate::ui::shell::home(app);
                return None;
            }
            KeyCode::End => {
                app.shell_top = None;
                return None;
            }
            _ => {}
        }
    }
    if app.mode == Mode::Composing {
        return on_composing_key(app, key, stop);
    }

    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => app.mode = Mode::Help,
        KeyCode::Char('e') => app.show_error(),
        KeyCode::Tab => app.pane = app.pane.next(),
        KeyCode::BackTab => app.pane = app.pane.previous(),
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::Char('i') if app.pane == Pane::Shell => {
            app.mode = Mode::Composing;
        }
        KeyCode::Enter if app.pane == Pane::Agents => return Some(Action::Enter),
        // A stop is requested through the state machine first, so the console and the session agree
        // on whether there was anything to stop.
        KeyCode::Esc if app.request_stop() => stop.request(),
        _ => {}
    }
    None
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled terminal key codes are ignored to preserve console input behavior"
)]
fn on_composing_key(app: &mut App, key: KeyEvent, stop: &StopFlag) -> Option<Action> {
    match key.code {
        KeyCode::Esc if app.request_stop() => stop.request(),
        KeyCode::Esc => {
            app.mode = Mode::Browsing;
            app.composer.clear();
        }
        KeyCode::Tab => {
            app.mode = Mode::Browsing;
            app.pane = app.pane.next();
        }
        KeyCode::BackTab => {
            app.mode = Mode::Browsing;
            app.pane = app.pane.previous();
        }
        KeyCode::Backspace => {
            app.composer.pop();
        }
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            app.composer.push(character)
        }
        KeyCode::Enter => {
            return match app.pane {
                Pane::Shell => {
                    if app.busy {
                        app.notice = Some(Notice::refusal(
                            "another command is running; press Esc to stop it",
                        ));
                        return None;
                    }
                    if app.session.is_none() {
                        app.notice = Some(Notice::refusal("hop into an agent first"));
                        return None;
                    }
                    let line = app.composer.trim().to_owned();
                    if line.is_empty() {
                        return None;
                    }
                    app.composer.clear();
                    app.shell_top = None;
                    Some(Action::Shell(line))
                }
                Pane::Agents | Pane::Detail => None,
            };
        }
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests;

fn clear_agent_context(app: &mut App, history: &mut History, limits: HistoryLimits) {
    app.session = None;
    app.assets = None;
    app.broker_trace = None;
    app.transcript = Default::default();
    app.shell_history.clear();
    app.shell_top = None;
    app.composer.clear();
    app.mode = Mode::Browsing;
    *history = History::new(limits);
}

#[expect(
    clippy::too_many_arguments,
    reason = "the dispatcher borrows the existing event loop state for one action"
)]
pub(crate) async fn dispatch(
    app: &mut App,
    action: Action,
    client: &BrokerClient,
    options: &mut ConsoleOptions,
    running: &mut Option<RunningTurn>,
    history: &mut History,
    stop: &StopFlag,
) {
    let span = match &action {
        Action::Enter => tracing::Span::none(),
        Action::Turn(_) => console_turn_span("model"),
        Action::Shell(_) => console_turn_span("shell"),
    };
    dispatch_in_context(app, action, client, options, running, history, stop)
        .instrument(span)
        .await;
}

/// Each submitted turn owns a root, never the preceding turn's context.
pub fn console_turn_span(kind: &'static str) -> tracing::Span {
    tracing::info_span!(parent: None, "console.turn", console.origin = "dekopon-console", console.kind = kind)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the dispatcher borrows the existing event loop state for one action"
)]
#[expect(
    clippy::too_many_lines,
    reason = "one exhaustive action dispatcher preserves the console state transitions"
)]
async fn dispatch_in_context(
    app: &mut App,
    action: Action,
    client: &BrokerClient,
    options: &mut ConsoleOptions,
    running: &mut Option<RunningTurn>,
    history: &mut History,
    stop: &StopFlag,
) {
    match action {
        Action::Enter => {
            if app.busy {
                app.notice = Some(Notice::refusal(
                    "stop the current turn before switching agents",
                ));
                return;
            }
            let Some(agent) = app.highlighted_id() else {
                app.notice = Some(Notice::refusal("the catalog contains no agents"));
                return;
            };
            if options.fixed_profile.as_ref().is_some_and(|name| {
                options
                    .profiles
                    .iter()
                    .any(|p| &p.name == name && p.agent != agent)
            }) {
                app.notice = Some(Notice::refusal(
                    "--profile is bound to its agent; restart with a matching profile",
                ));
                return;
            }
            let matches: Vec<_> = options
                .profiles
                .iter()
                .filter(|p| p.agent == agent)
                .collect();
            if matches.len() > 1 && options.fixed_profile.is_none() {
                app.notice = Some(Notice::refusal(format!(
                    "multiple profiles for {agent}; restart with --profile <NAME>"
                )));
                return;
            }
            if let Some(profile) = options
                .fixed_profile
                .as_ref()
                .and_then(|name| matches.iter().copied().find(|p| &p.name == name))
                .or_else(|| matches.first().copied())
            {
                options.subject = profile.subject.clone();
                (options.model, options.model_source) = crate::session::selected_model(
                    options.model_override.as_deref(),
                    profile.model.as_deref(),
                );
                options.prompt_limits.max_steps =
                    options.steps_override.or(profile.max_steps).unwrap_or(8);
                options.prompt_limits.max_capability_calls = options
                    .calls_override
                    .or(profile.max_capability_calls)
                    .unwrap_or(16);
                app.profile = Some(profile.name.clone());
            } else if !options.profiles.is_empty() {
                app.notice = Some(Notice::refusal(format!(
                    "no authored profile for {agent}; restart without --profiles and with --subject for subject-only use"
                )));
                return;
            }
            options.scope = options.smoke_conversation.then(crate::session::smoke_claim);
            // A hop (including a refused hop) must never leave
            // the prior agent usable under the newly selected subject/model.
            clear_agent_context(app, history, options.history_limits);
            app.subject = options.subject.to_string();
            app.scope_label = options
                .scope
                .as_ref()
                .map(|s| serde_json::to_string(s).expect("typed scope serializes"))
                .unwrap_or_else(|| "subject-only".into());
            app.model = options.model.clone();
            app.model_source = options.model_source;
            let assets = Arc::new(crate::session::assets::ConsoleAssets::default());
            match open_agent_with_assets(
                client.clone(),
                options.subject.clone(),
                agent.clone(),
                options.scope.clone(),
                Arc::clone(&assets),
            )
            .await
            {
                Ok(leg) if leg.effective_capabilities().is_empty() => {
                    let report = empty_surface(app, "open broker session");
                    app.raise(report);
                }
                Ok(leg) => {
                    app.enter(AgentSession::new(agent, leg, options.history_limits));
                    app.assets = Some(assets);
                }
                Err(error) => {
                    let report = session_refusal(app, "open broker session", &error);
                    app.raise(report);
                }
            }
        }
        Action::Turn(prompt) => {
            let Some(session) = app.session.as_ref() else {
                return;
            };
            let Some(assets) = app.assets.clone() else {
                return;
            };
            let leg = match open_agent_with_assets(
                client.clone(),
                options.subject.clone(),
                session.agent.clone(),
                options.scope.clone(),
                assets,
            )
            .await
            {
                Ok(leg) if !leg.effective_capabilities().is_empty() => leg,
                Ok(_) => {
                    let report = empty_surface(app, "open broker session for a model turn");
                    refuse_turn(app, report);
                    return;
                }
                Err(error) => {
                    let report =
                        session_refusal(app, "open broker session for a model turn", &error);
                    refuse_turn(app, report);
                    return;
                }
            };
            app.broker_trace = Some(leg.session_trace().to_string());
            let model: Arc<dyn ChatModel + Send + Sync> = match build_model(options, stop) {
                Ok(model) => Arc::from(model),
                Err(error) => {
                    let report = app.report(ErrorReport::from_error("build model client", &error));
                    refuse_turn(app, report);
                    return;
                }
            };
            let (sender, receiver) = session_channel();
            let progress = Arc::new(RecordingProgress::new(sender.clone()));
            stop.reset();
            let (handle_cancel, signal) = CancelSignal::pair();
            stop.bind_broker(handle_cancel);
            #[expect(
                clippy::disallowed_methods,
                reason = "owner: RunningTurn retained by the event loop and joined on completion or shutdown; bound: one active turn admitted by app.busy and running"
            )]
            let handle = tokio::spawn(
                crate::session::run_turn(
                    leg.with_cancel_signal(signal),
                    model,
                    prompt,
                    instructions(app),
                    app.agents
                        .iter()
                        .find(|agent| agent.metadata.name == session.agent.as_str())
                        .expect("session catalog agent")
                        .clone(),
                    options.clone(),
                    std::mem::replace(history, History::new(options.history_limits)),
                    stop.clone(),
                    progress,
                    sender,
                )
                .in_current_span(),
            );
            *running = Some(RunningTurn {
                events: receiver,
                handle,
                shell: false,
            });
        }
        Action::Shell(line) => {
            let Some(session) = app.session.as_ref() else {
                app.notice = Some(Notice::refusal("hop into an agent first"));
                return;
            };
            if app.busy {
                app.notice = Some(Notice::refusal(
                    "another command or turn is running; press Esc to stop it",
                ));
                return;
            }
            let agent = session.agent.clone();
            if let Some(text) = line.strip_prefix(":smoke record ") {
                if !options.smoke_conversation {
                    app.notice = Some(Notice::refusal(
                        "restart with --smoke-conversation to record a synthetic turn",
                    ));
                    return;
                }
                let Some(text) = text.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) else {
                    app.notice = Some(Notice::refusal("use :smoke record '<text>'"));
                    return;
                };
                let result = if text.len() <= 64 * 1024 {
                    crate::session::smoke_record(
                        client,
                        options.subject.clone(),
                        agent,
                        text.to_owned(),
                    )
                    .await
                    .map_err(|e| e.to_string())
                } else {
                    Err("smoke text exceeds 64 KiB".into())
                };
                let output = match &result {
                    Ok((message, _)) => message.clone(),
                    Err(error) => format!("refused: {error}"),
                };
                app.start_shell(line.clone());
                app.finish_shell(ShellEntry {
                    input: line,
                    output,
                    exit_code: Some(if result.as_ref().is_ok_and(|(_, succeeded)| *succeeded) {
                        0
                    } else {
                        1
                    }),
                    truncated: false,
                });
                app.busy = false;
                return;
            }
            if line.starts_with(":smoke") {
                app.notice = Some(Notice::refusal(
                    "unknown local smoke command; use :smoke record '<text>'",
                ));
                return;
            }
            if let Some(command) = line.strip_prefix(":asset ") {
                let Some(assets) = app.assets.as_ref() else {
                    return;
                };
                let result = command
                    .strip_prefix("attach-base64 --type ")
                    .and_then(|args| args.split_once(' '))
                    .ok_or_else(|| "use :asset attach-base64 --type <mime> <BASE64>".to_owned())
                    .and_then(|(mime, encoded)| assets.attach_base64(mime, encoded));
                let output = match &result {
                    Ok(id) => format!("chat-asset:{id}"),
                    Err(error) => format!("refused: {error}"),
                };
                let label = ":asset attach-base64 [input redacted]".to_owned();
                app.start_shell(label.clone());
                app.finish_shell(ShellEntry {
                    input: label,
                    output,
                    exit_code: Some(if result.is_ok() { 0 } else { 1 }),
                    truncated: false,
                });
                app.busy = false;
                return;
            }
            if line.starts_with(":asset") {
                app.notice = Some(Notice::refusal(
                    "unknown local asset command; use :asset attach-base64 --type <mime> <BASE64>",
                ));
                return;
            }
            app.start_shell(line.clone());
            let Some(assets) = app.assets.clone() else {
                return;
            };
            let leg = match open_agent_with_assets(
                client.clone(),
                options.subject.clone(),
                agent,
                options.scope.clone(),
                assets,
            )
            .await
            {
                Ok(leg) if !leg.effective_capabilities().is_empty() => leg,
                Ok(_) => {
                    let report = empty_surface(app, "open broker session for a shell command");
                    app.fail_shell(report);
                    app.busy = false;
                    return;
                }
                Err(error) => {
                    let report =
                        session_refusal(app, "open broker session for a shell command", &error);
                    app.fail_shell(report);
                    app.busy = false;
                    return;
                }
            };
            app.broker_trace = Some(leg.session_trace().to_string());
            let limits = options.shell_limits;
            let (sender, receiver) = session_channel();
            stop.reset();
            let (handle_cancel, signal) = CancelSignal::pair();
            stop.bind_broker(handle_cancel);
            let previous = std::mem::replace(history, History::new(options.history_limits));
            let span = tracing::Span::current();
            #[expect(clippy::disallowed_methods, reason = "owner: RunningTurn retained by the event loop and joined on completion or shutdown; bound: one active turn admitted by app.busy and running")]
            let handle = tokio::spawn(async move {
                tokio::task::spawn_blocking(move || {
                    let _entered = span.enter();
                    let script = tracing::info_span!("console.script");
                    let _script = script.enter();
                    tracing::info!(target: "dekopon_tui::audit", script = line.as_str(), "console manual script");
                    let outcome = ShellRuntime {
                        invoker: LegHandle(Arc::new(leg.with_cancel_signal(signal))),
                        limits,
                        calls: CallBudget::new(limits.max_capability_calls),
                    }.run_script(&line, None);
                    tracing::info!(target: "dekopon_tui::audit", output = outcome.output.as_str(), exit_code = outcome.exit_code.get(), "console manual output");
                    if sender
                        .send(SessionEvent::ShellFinished(ShellEntry {
                            input: line,
                            output: outcome.output,
                            exit_code: Some(outcome.exit_code.get()),
                            truncated: outcome.truncated,
                        }))
                        .is_err()
                    {
                        tracing::debug!("shell output lost because the terminal closed");
                    }
                    previous
                })
                .await
                .map_err(SessionError::Task)
            }.in_current_span());
            *running = Some(RunningTurn {
                events: receiver,
                handle,
                shell: true,
            });
        }
    }
}

/// Closes the transcript when a fresh gate or model setup refuses before inference.
fn refuse_turn(app: &mut App, report: ErrorReport) {
    app.on_session_event(SessionEvent::Finished(Box::new(Err(report.to_string()))));
    app.raise(report);
}

/// A failed `capabilitiesFor`, with the broker's refusal and where its reason is logged.
fn session_refusal(app: &App, operation: &str, error: &SessionError) -> ErrorReport {
    let report = app.report(ErrorReport::from_error(operation, error));
    if !report
        .refusal
        .as_ref()
        .is_some_and(|(code, _)| code == dekopon_broker_protocol::ERROR_UNAUTHENTICATED)
    {
        return report;
    }
    let note = format!(
        "brokerd keeps the refusal reason off the wire so a refused caller cannot probe the subject \
         directory or agent grants. It logs it: a WARN with event=broker_capabilities_refused, \
         subject={}, agent={}, a reason (attestation-denied, unmapped-subject, agent-denied or \
         policy-error) and the determining policy_ids",
        report.fact("subject").unwrap_or_default(),
        report.fact("agent").unwrap_or_default(),
    );
    report.with_note(note)
}

/// The broker answered, and policy grants this context nothing.
fn empty_surface(app: &App, operation: &str) -> ErrorReport {
    let report = app.report(ErrorReport::from_message(
        operation,
        "broker grants no capabilities for this context; no model call or command will be sent",
    ));
    let note = format!(
        "the broker accepted the attestation and permitted agent.prompt, but Cedar authorizes no \
         provider capability for this subject's principal with context.agent == \"{}\" in scope \
         {}. Check the permit policies for that agent, including any context.transport or \
         context.conversation condition",
        report.fact("agent").unwrap_or_default(),
        report.fact("scope").unwrap_or_default(),
    );
    report.with_note(note)
}

/// The agent's standing orders, handed to the model fresh on every turn.
fn instructions(app: &App) -> Option<String> {
    let session = app.session.as_ref()?;
    app.agents
        .iter()
        .find(|agent| agent.metadata.name == session.agent.as_str())
        .and_then(|agent| agent.spec.instructions.clone())
}

/// Why the console stopped.
#[derive(Debug, thiserror::Error)]
pub enum ConsoleExit {
    /// The terminal itself failed.
    #[error("the terminal could not be driven")]
    Terminal(#[source] io::Error),
    /// Structured session could not safely continue.
    #[error("structured console: {0}")]
    Structured(String),
    /// Setting up the session layer failed before the console could open.
    #[error(transparent)]
    Session(SessionError),
}
