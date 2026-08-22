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
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use dekopon_agent::prompt::History;
use dekopon_broker_protocol::BrokerClient;
use dekopon_model::model::ChatModel;
use dekopon_shell::Interpreter;
use futures_util::StreamExt as _;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{
    app::{App, Mode, Notice, Pane, ShellEntry},
    record::SessionEvent,
    session::{
        AgentSession, ConsoleOptions, SessionError, StopFlag, build_model, open_agent,
        session_channel,
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
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            // Restore first, then let the default hook print: a backtrace drawn into the alternate
            // screen vanishes with it, which is how a panic becomes a silent exit.
            restore();
            previous(info);
        }));

        Ok((Self, Terminal::new(CrosstermBackend::new(io::stdout()))?))
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
    if let Err(error) = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture) {
        eprintln!("warning: could not leave the alternate screen ({error}); run `reset`");
    }
}

/// One turn in flight, and what the console needs back when it finishes.
struct RunningTurn {
    events: UnboundedReceiver<SessionEvent>,
    handle: tokio::task::JoinHandle<Result<History, SessionError>>,
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
    options: ConsoleOptions,
) -> Result<(), ConsoleExit> {
    let model: Arc<dyn ChatModel + Send + Sync> =
        Arc::from(build_model(&options).map_err(ConsoleExit::Session)?);
    let (_guard, mut terminal) = TerminalGuard::enter().map_err(ConsoleExit::Terminal)?;

    let stop = StopFlag::default();
    let mut keys = EventStream::new();
    let mut running: Option<RunningTurn> = None;
    let mut history = History::new(options.history_limits);

    loop {
        terminal
            .draw(|frame| ui::draw(frame, &app))
            .map_err(ConsoleExit::Terminal)?;
        if app.should_quit {
            return Ok(());
        }

        // One select over both inputs, so a churning turn keeps redrawing while keys stay live —
        // which is what makes Esc able to stop it.
        tokio::select! {
            key = keys.next() => match key {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    if let Some(action) = on_key(&mut app, key, &stop) {
                        dispatch(&mut app, action, &client, &options, &model, &mut running,
                                 &mut history, &stop).await;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(ConsoleExit::Terminal(error)),
                None => return Ok(()),
            },
            event = recv(&mut running) => {
                match event {
                    Some(event) => app.on_session_event(event),
                    None => finish_turn(&mut app, &mut running, &mut history, &stop).await,
                }
            }
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
    match turn.handle.await {
        Ok(Ok(returned)) => *history = returned,
        // The session's own history is lost, so the model's replay window is now a guess. Saying so
        // is better than silently continuing against a window that no longer matches the screen.
        Ok(Err(error)) => app.notice = Some(Notice::refusal(error.to_string())),
        Err(error) => app.notice = Some(Notice::refusal(format!("the session task died: {error}"))),
    }
    stop.reset();
    app.on_session_complete(history.len());
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
pub fn on_key(app: &mut App, key: KeyEvent, stop: &StopFlag) -> Option<Action> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return None;
    }
    if app.mode == Mode::Help {
        app.mode = Mode::Browsing;
        return None;
    }
    if app.mode == Mode::Composing {
        return on_composing_key(app, key);
    }

    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => app.mode = Mode::Help,
        KeyCode::Tab => app.pane = app.pane.next(),
        KeyCode::BackTab => app.pane = app.pane.previous(),
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::Char('i') if matches!(app.pane, Pane::Turns | Pane::Shell) => {
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

fn on_composing_key(app: &mut App, key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Browsing;
            app.composer.clear();
        }
        KeyCode::Backspace => {
            app.composer.pop();
        }
        KeyCode::Char(character) => app.composer.push(character),
        KeyCode::Enter => {
            return match app.pane {
                Pane::Shell => {
                    let line = app.composer.trim().to_owned();
                    app.composer.clear();
                    app.mode = Mode::Browsing;
                    (!line.is_empty()).then_some(Action::Shell(line))
                }
                _ => app.submit_turn().map(Action::Turn),
            };
        }
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests;

#[expect(
    clippy::too_many_arguments,
    reason = "the loop's whole mutable world, threaded through one place rather than gathered into \
              a struct that would exist only to satisfy this lint"
)]
async fn dispatch(
    app: &mut App,
    action: Action,
    client: &BrokerClient,
    options: &ConsoleOptions,
    model: &Arc<dyn ChatModel + Send + Sync>,
    running: &mut Option<RunningTurn>,
    history: &mut History,
    stop: &StopFlag,
) {
    match action {
        Action::Enter => {
            let Some(agent) = app.highlighted_id() else {
                return;
            };
            match open_agent(client.clone(), options.subject.clone(), agent.clone()).await {
                Ok(leg) => {
                    // The replay window belongs to the conversation, and hopping starts a new one.
                    // Carrying the old history across would replay one agent's exchanges into
                    // another agent's prompt.
                    *history = History::new(options.history_limits);
                    app.enter(AgentSession::new(agent, leg, options.history_limits));
                }
                Err(error) => app.notice = Some(Notice::refusal(error.to_string())),
            }
        }
        Action::Turn(prompt) => {
            let Some(session) = app.session.as_ref() else {
                return;
            };
            let (sender, receiver) = session_channel();
            stop.reset();
            let handle = tokio::spawn(crate::session::run_turn(
                Arc::clone(session.leg()),
                Arc::clone(model),
                prompt,
                instructions(app),
                options.clone(),
                std::mem::replace(history, History::new(options.history_limits)),
                stop.clone(),
                sender,
            ));
            *running = Some(RunningTurn {
                events: receiver,
                handle,
            });
        }
        Action::Shell(line) => {
            let Some(session) = app.session.as_ref() else {
                app.notice = Some(Notice::refusal("hop into an agent first"));
                return;
            };
            let leg = Arc::clone(session.leg());
            let limits = options.shell_limits;
            // The interpreter is synchronous and the leg is only valid on a blocking task, exactly
            // as it is inside a session; the shell pane is the same seam, not a lighter one.
            let outcome = tokio::task::spawn_blocking(move || {
                let outcome = Interpreter::new(limits).run(&line, leg.as_ref());
                (line, outcome)
            })
            .await;
            match outcome {
                Ok((input, outcome)) => app.push_shell(ShellEntry {
                    input,
                    output: outcome.output,
                    exit_code: outcome.exit_code.get(),
                }),
                Err(error) => {
                    app.notice = Some(Notice::refusal(format!("the shell task died: {error}")));
                }
            }
        }
    }
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
    /// Setting up the session layer failed before the console could open.
    #[error(transparent)]
    Session(SessionError),
}
