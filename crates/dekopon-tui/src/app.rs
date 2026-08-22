//! The console's state machine, kept clear of every drawing call.
//!
//! Everything that decides what the console *is* lives here and is driven by two inputs: key
//! events and [`SessionEvent`]s. Nothing here touches a terminal, so the whole behaviour of the
//! console is testable by feeding it those two and reading the state back.

use dekopon_core::AgentId;
use dekopon_protocol::Agent;

use crate::{record::SessionEvent, session::AgentSession, transcript::Transcript};

/// Which pane has focus.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Pane {
    /// The catalog's agents.
    #[default]
    Agents,
    /// One agent's declared and effective capability surfaces.
    Detail,
    /// The conversation and its tool-call tree.
    Turns,
    /// A prompt bound to the agent's own capability seam.
    Shell,
}

impl Pane {
    /// Panes in cycling order.
    pub const ORDER: [Self; 4] = [Self::Agents, Self::Detail, Self::Turns, Self::Shell];

    /// Short title drawn in the tab bar.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Detail => "capabilities",
            Self::Turns => "turns",
            Self::Shell => "shell",
        }
    }

    /// The next pane in the cycle.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Agents => Self::Detail,
            Self::Detail => Self::Turns,
            Self::Turns => Self::Shell,
            Self::Shell => Self::Agents,
        }
    }

    /// The previous pane in the cycle.
    #[must_use]
    pub const fn previous(self) -> Self {
        match self {
            Self::Agents => Self::Shell,
            Self::Detail => Self::Agents,
            Self::Turns => Self::Detail,
            Self::Shell => Self::Turns,
        }
    }
}

/// Where the console is in its own lifecycle.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Mode {
    /// Browsing; keys are commands.
    #[default]
    Browsing,
    /// Typing into the composer; keys are text.
    Composing,
    /// The keybinding overlay is up.
    Help,
}

/// One line of console-visible history in the shell pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellEntry {
    /// The line that was submitted.
    pub input: String,
    /// The interpreter's combined output.
    pub output: String,
    /// Its exit code.
    pub exit_code: u8,
}

/// A message shown on the status line until the next action replaces it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notice {
    /// What to say.
    pub text: String,
    /// Whether it reports a refusal rather than an outcome.
    pub is_refusal: bool,
}

impl Notice {
    /// An ordinary informational notice.
    #[must_use]
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_refusal: false,
        }
    }

    /// A notice reporting that the console refused to do something.
    #[must_use]
    pub fn refusal(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_refusal: true,
        }
    }
}

/// The whole console.
pub struct App {
    /// Agents from the validated catalog, in catalog order.
    pub agents: Vec<Agent>,
    /// Index of the highlighted agent.
    pub selected_agent: usize,
    /// The agent currently hopped into, if any.
    pub session: Option<AgentSession>,
    /// The console's own full-fidelity record of the current agent's conversation.
    pub transcript: Transcript,
    /// Lines run in the shell pane, oldest first.
    pub shell_history: Vec<ShellEntry>,
    /// The composer's contents.
    pub composer: String,
    /// Which pane has focus.
    pub pane: Pane,
    /// Which mode the console is in.
    pub mode: Mode,
    /// The last thing worth saying on the status line.
    pub notice: Option<Notice>,
    /// Whether a turn is in flight.
    pub busy: bool,
    /// Whether the console should exit at the next redraw.
    pub should_quit: bool,
    /// Which capability node is expanded, as `(turn, script, call)`.
    pub expanded_call: Option<(usize, usize, usize)>,
    /// Redacted fields the operator has deliberately revealed, by dotted path.
    pub revealed: Vec<String>,
    /// The resolved credential path, shown so it is never a guess.
    pub credential_path: String,
    /// The resolved broker socket, shown for the same reason.
    pub socket_path: String,
    /// The canonical subject sessions propose on behalf of.
    pub subject: String,
}

impl App {
    /// Builds a console over one catalog.
    #[must_use]
    pub fn new(
        agents: Vec<Agent>,
        subject: String,
        socket_path: String,
        credential_path: String,
    ) -> Self {
        Self {
            agents,
            selected_agent: 0,
            session: None,
            transcript: Transcript::default(),
            shell_history: Vec::new(),
            composer: String::new(),
            pane: Pane::default(),
            mode: Mode::default(),
            notice: None,
            busy: false,
            should_quit: false,
            expanded_call: None,
            revealed: Vec::new(),
            credential_path,
            socket_path,
            subject,
        }
    }

    /// The highlighted agent, if the catalog has any.
    #[must_use]
    pub fn highlighted(&self) -> Option<&Agent> {
        self.agents.get(self.selected_agent)
    }

    /// The identifier of the highlighted agent, parsed as the broker will read it.
    ///
    /// Parsed rather than cloned because `ObjectMeta::name` is a `String` the catalog loader has
    /// already validated; the leg wants the typed identifier and the conversion cannot fail here.
    #[must_use]
    pub fn highlighted_id(&self) -> Option<AgentId> {
        self.highlighted()
            .and_then(|agent| agent.metadata.name.parse::<AgentId>().ok())
    }

    /// Moves the agent highlight, saturating rather than wrapping.
    ///
    /// Saturating on purpose: a list that jumps from the last row to the first is one an operator
    /// scrolls past without noticing they have gone round.
    pub fn move_selection(&mut self, delta: isize) {
        if self.agents.is_empty() {
            return;
        }
        let last = self.agents.len() - 1;
        let next = self.selected_agent as isize + delta;
        self.selected_agent = next.clamp(0, last as isize) as usize;
    }

    /// Records a hop into one agent, replacing whatever was open.
    ///
    /// The transcript is reset with the session because it belongs to *that* conversation: showing
    /// one agent's tool calls under another agent's name would misattribute every one of them.
    pub fn enter(&mut self, session: AgentSession) {
        let agent = session.agent.clone();
        self.notice = Some(if session.is_empty() {
            Notice::refusal(format!(
                "policy grants {} nothing through {agent}; the broker answered, it just said no",
                self.subject
            ))
        } else {
            Notice::info(format!(
                "{agent}: {} capabilities granted",
                session.effective.len()
            ))
        });
        self.session = Some(session);
        self.transcript = Transcript::default();
        self.shell_history.clear();
        self.expanded_call = None;
        self.revealed.clear();
        self.pane = Pane::Detail;
    }

    /// Submits the composer as a turn, or explains why it could not.
    ///
    /// Returns the prompt to run, so the caller can spawn the session; `None` means nothing was
    /// submitted and [`Self::notice`] says why.
    pub fn submit_turn(&mut self) -> Option<String> {
        if self.session.is_none() {
            self.notice = Some(Notice::refusal("hop into an agent first"));
            return None;
        }
        if self.busy {
            // One session in flight, said out loud. Queueing silently would let an operator type
            // three prompts and watch one answer arrive.
            self.notice = Some(Notice::refusal(
                "a turn is already running; press Esc to stop it",
            ));
            return None;
        }
        let prompt = self.composer.trim().to_owned();
        if prompt.is_empty() {
            self.notice = Some(Notice::refusal("nothing to send"));
            return None;
        }
        self.composer.clear();
        self.mode = Mode::Browsing;
        self.busy = true;
        self.transcript.open(prompt.clone());
        self.notice = None;
        Some(prompt)
    }

    /// Folds one session event into the open turn.
    pub fn on_session_event(&mut self, event: SessionEvent) {
        let finished = matches!(event, SessionEvent::Finished(_));
        self.transcript.absorb(event);
        if finished {
            self.busy = false;
        }
    }

    /// Records that a turn's session task has fully returned, with the model's live history size.
    pub fn on_session_complete(&mut self, remembered: usize) {
        self.busy = false;
        self.transcript.mark_replay_window(remembered);
        if let Some(turn) = self
            .transcript
            .turns()
            .last()
            .filter(|turn| turn.status.is_running())
        {
            let ordinal = turn.ordinal;
            self.notice = Some(Notice::refusal(format!(
                "turn {ordinal} ended without reporting an outcome"
            )));
        }
    }

    /// Requests a cooperative stop, and says what that does and does not undo.
    pub fn request_stop(&mut self) -> bool {
        if !self.busy {
            return false;
        }
        if let Some(turn) = self.transcript.running_mut() {
            turn.stop_requested = true;
        }
        // Never "cancelled": a provider request the broker already accepted still finishes, and an
        // operator who reads this as a rollback will not check the audit chain.
        self.notice = Some(Notice::info(
            "stopping at the next boundary; calls already sent to the broker still complete",
        ));
        true
    }

    /// Toggles the expansion of one capability node.
    pub fn toggle_call(&mut self, target: (usize, usize, usize)) {
        self.expanded_call = if self.expanded_call == Some(target) {
            None
        } else {
            Some(target)
        };
    }

    /// Reveals one redacted field, by dotted path.
    ///
    /// Per field and per keystroke rather than a mode: revealing is a deliberate act that puts a
    /// secret into terminal scrollback, which is exactly the posture `auth chatgpt export` takes.
    pub fn reveal(&mut self, path: String) {
        if !self.revealed.contains(&path) {
            self.notice = Some(Notice::info(format!(
                "revealed {path}; it is in your scrollback now"
            )));
            self.revealed.push(path);
        }
    }

    /// Whether one dotted path has been deliberately revealed.
    #[must_use]
    pub fn is_revealed(&self, path: &str) -> bool {
        self.revealed.iter().any(|revealed| revealed == path)
    }

    /// Records a shell line and its result.
    pub fn push_shell(&mut self, entry: ShellEntry) {
        self.shell_history.push(entry);
    }
}

#[cfg(test)]
mod tests;
