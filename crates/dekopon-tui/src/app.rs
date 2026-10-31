use dekopon_core::AgentId;
use dekopon_protocol::Agent;

use crate::{
    record::{CapabilityCall, SessionEvent},
    redact::redact,
    report::ErrorReport,
    session::AgentSession,
    transcript::Transcript,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Pane {
    #[default]
    Agents,
    Detail,
    Shell,
}

impl Pane {
    pub const ORDER: [Self; 3] = [Self::Agents, Self::Shell, Self::Detail];

    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Detail => "capabilities",
            Self::Shell => "shell",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Agents => Self::Shell,
            Self::Shell => Self::Detail,
            Self::Detail => Self::Agents,
        }
    }

    #[must_use]
    pub const fn previous(self) -> Self {
        match self {
            Self::Agents => Self::Detail,
            Self::Shell => Self::Agents,
            Self::Detail => Self::Shell,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Mode {
    #[default]
    Browsing,
    Composing,
    Help,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellEntry {
    pub input: String,
    pub output: String,
    pub exit_code: Option<u8>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notice {
    pub text: String,
    pub is_refusal: bool,
}

impl Notice {
    #[must_use]
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_refusal: false,
        }
    }

    #[must_use]
    pub fn refusal(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_refusal: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Payload {
    Input,
    Output,
}

impl Payload {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevealedField {
    pub call: (usize, usize, usize),
    pub payload: Payload,
    pub path: String,
}

impl RevealedField {
    #[must_use]
    pub fn describe(&self) -> String {
        if self.path.is_empty() {
            self.payload.label().to_owned()
        } else {
            format!("{}.{}", self.payload.label(), self.path)
        }
    }
}

pub struct App {
    pub agents: Vec<Agent>,
    pub selected_agent: usize,
    pub session: Option<AgentSession>,
    pub transcript: Transcript,
    pub shell_history: Vec<ShellEntry>,
    pub shell_top: Option<usize>,
    pub shell_viewport: (u16, u16),
    pub composer: String,
    pub pane: Pane,
    pub mode: Mode,
    pub notice: Option<Notice>,
    pub busy: bool,
    pub should_quit: bool,
    pub expanded_call: Option<(usize, usize, usize)>,
    pub selected_call: usize,
    pub revealed: Vec<RevealedField>,
    pub credential_path: String,
    pub socket_path: String,
    pub subject: String,
    pub profile: Option<String>,
    pub scope_label: String,
    pub model: String,
    pub model_source: crate::session::ModelSource,
    pub broker_trace: Option<String>,
    pub assets: Option<std::sync::Arc<crate::session::assets::ConsoleAssets>>,
    pub last_error: Option<ErrorReport>,
    pub error_scroll: u16,
}

impl App {
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
            shell_top: None,
            shell_viewport: (0, 0),
            composer: String::new(),
            pane: Pane::default(),
            mode: Mode::default(),
            notice: None,
            busy: false,
            should_quit: false,
            expanded_call: None,
            selected_call: 0,
            revealed: Vec::new(),
            credential_path,
            socket_path,
            subject,
            profile: None,
            scope_label: "subject-only".into(),
            model: String::new(),
            model_source: crate::session::ModelSource::Default,
            broker_trace: None,
            assets: None,
            last_error: None,
            error_scroll: 0,
        }
    }

    #[must_use]
    pub fn report(&self, report: ErrorReport) -> ErrorReport {
        let agent = self.session.as_ref().map_or_else(
            || {
                self.highlighted_id()
                    .map(|id| id.to_string())
                    .unwrap_or_default()
            },
            |session| session.agent.to_string(),
        );
        let mut report = report
            .with("subject", self.subject.clone())
            .with("agent", agent)
            .with("profile", self.profile.as_deref().unwrap_or("subject-only"))
            .with("scope", self.scope_label.clone())
            .with("broker socket", self.socket_path.clone());
        if let Some(trace) = &self.broker_trace {
            report = report.with("broker trace", trace.clone());
        }
        report
    }

    pub fn record_error(&mut self, report: ErrorReport) {
        tracing::warn!(
            target: "dekopon_tui::error",
            operation = report.operation.as_str(),
            report = %report,
            "console error"
        );
        self.notice = Some(Notice::refusal(report.summary()));
        self.last_error = Some(report);
    }

    pub fn raise(&mut self, report: ErrorReport) {
        self.record_error(report);
        self.show_error();
    }

    pub fn show_error(&mut self) {
        if self.last_error.is_some() {
            self.mode = Mode::Error;
            self.error_scroll = 0;
        } else {
            self.notice = Some(Notice::info("no error to show"));
        }
    }

    #[must_use]
    pub fn highlighted(&self) -> Option<&Agent> {
        self.agents.get(self.selected_agent)
    }

    #[must_use]
    pub fn highlighted_id(&self) -> Option<AgentId> {
        self.highlighted()
            .and_then(|agent| agent.metadata.name.parse::<AgentId>().ok())
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.agents.is_empty() {
            return;
        }
        let last = self.agents.len() - 1;
        let next = self.selected_agent as isize + delta;
        self.selected_agent = next.clamp(0, last as isize) as usize;
    }

    #[must_use]
    pub fn calls(&self) -> Vec<(usize, usize, usize)> {
        let mut targets = Vec::new();
        for (turn_index, turn) in self.transcript.turns().iter().enumerate() {
            for (script_index, script) in turn.scripts.iter().enumerate() {
                for call_index in 0..script.calls.len() {
                    targets.push((turn_index, script_index, call_index));
                }
            }
        }
        targets
    }

    #[must_use]
    pub fn cursor_call(&self) -> Option<(usize, usize, usize)> {
        self.calls().get(self.selected_call).copied()
    }

    #[must_use]
    pub fn call_at(&self, target: (usize, usize, usize)) -> Option<&CapabilityCall> {
        let (turn, script, call) = target;
        self.transcript
            .turns()
            .get(turn)?
            .scripts
            .get(script)?
            .calls
            .get(call)
    }

    pub fn toggle_cursor_call(&mut self) {
        let Some(target) = self.cursor_call() else {
            self.notice = Some(Notice::refusal("no capability call to expand yet"));
            return;
        };
        self.toggle_call(target);
    }

    pub fn reveal_next(&mut self) {
        let Some(target) = self.cursor_call() else {
            self.notice = Some(Notice::refusal("no capability call is selected"));
            return;
        };
        let Some(call) = self.call_at(target) else {
            return;
        };
        let mut candidates = Vec::new();
        for (payload, value) in call.payloads() {
            for redaction in redact(value).redactions {
                candidates.push(RevealedField {
                    call: target,
                    payload,
                    path: redaction.path,
                });
            }
        }
        let Some(next) = candidates
            .into_iter()
            .find(|field| !self.is_revealed(field.call, field.payload, &field.path))
        else {
            self.notice = Some(Notice::info("nothing left to reveal in this call"));
            return;
        };
        self.reveal(next);
    }

    pub fn enter(&mut self, session: AgentSession) {
        let agent = session.agent.clone();
        self.broker_trace = Some(session.leg().session_trace().to_string());
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
        self.shell_top = None;
        self.composer.clear();
        self.mode = Mode::Composing;
        self.expanded_call = None;
        self.selected_call = 0;
        self.revealed.clear();
        self.pane = Pane::Shell;
    }

    pub fn submit_turn(&mut self) -> Option<String> {
        if self.session.is_none() {
            self.notice = Some(Notice::refusal("hop into an agent first"));
            return None;
        }
        if self.session.as_ref().is_some_and(AgentSession::is_empty) {
            self.notice = Some(Notice::refusal(
                "broker grants no capabilities; no model request will be sent",
            ));
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

    pub fn on_session_event(&mut self, event: SessionEvent) {
        let finished = matches!(event, SessionEvent::Finished(_));
        self.transcript.absorb(event);
        if finished {
            self.busy = false;
        }
    }

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

    pub fn toggle_call(&mut self, target: (usize, usize, usize)) {
        self.expanded_call = if self.expanded_call == Some(target) {
            None
        } else {
            Some(target)
        };
    }

    pub fn reveal(&mut self, field: RevealedField) {
        if self.is_revealed(field.call, field.payload, &field.path) {
            return;
        }
        let path = field.describe();
        self.notice = Some(Notice::info(format!(
            "revealed {path}; it is in your scrollback now"
        )));
        self.revealed.push(field);
    }

    #[must_use]
    pub fn is_revealed(&self, call: (usize, usize, usize), payload: Payload, path: &str) -> bool {
        self.revealed.iter().any(|revealed| {
            revealed.call == call && revealed.payload == payload && revealed.path == path
        })
    }

    pub fn start_shell(&mut self, input: String) {
        self.shell_top = None;
        self.busy = true;
        self.notice = None;
        self.shell_history.push(ShellEntry {
            input,
            output: String::new(),
            exit_code: None,
            truncated: false,
        });
    }

    pub fn finish_shell(&mut self, entry: ShellEntry) {
        if let Some(pending) = self
            .shell_history
            .last_mut()
            .filter(|entry| entry.exit_code.is_none())
        {
            *pending = entry;
        }
    }

    pub fn fail_shell(&mut self, report: ErrorReport) {
        let report = self.report(report);
        if let Some(pending) = self
            .shell_history
            .last_mut()
            .filter(|entry| entry.exit_code.is_none())
        {
            pending.output = report.to_string();
            pending.exit_code = Some(1);
        }
        self.record_error(report);
    }
}

#[cfg(test)]
mod tests;
