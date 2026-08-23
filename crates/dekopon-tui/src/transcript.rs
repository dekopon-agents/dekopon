//! The console's own record of what an agent did, assembled from [`SessionEvent`]s.
//!
//! This is deliberately not `dekopon_agent::prompt::History`. That one is what the *model* sees on
//! the next turn: prompt and answer only, trimmed to a turn and byte window, with every tool call
//! and tool result dropped as it is recorded. This one is what the *operator* sees: every script,
//! every capability, every argument and result, kept whole and never sent anywhere.
//!
//! Holding both and showing the difference is the point. "Why did it forget?" is answered by which
//! turns are still inside the replay window, and nothing else in the system can show that.

use std::time::Duration;

use dekopon_model::model::ModelUsage;

use crate::record::{CapabilityCall, ScriptRun, SessionEvent};

/// How a turn ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnStatus {
    /// Still churning.
    Running,
    /// The model produced a final answer.
    Answered(String),
    /// The model declined to reply, which only a chat continuation can do.
    Suppressed,
    /// The session failed, was cancelled, or exhausted a bound.
    Failed(String),
}

impl TurnStatus {
    /// Whether the turn is still open.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// One script the model wrote, with the capabilities it dispatched inside it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScriptNode {
    /// Session-wide ordinal.
    pub sequence: u64,
    /// The script verbatim.
    pub script: String,
    /// Capability calls made while it ran, in dispatch order.
    pub calls: Vec<CapabilityCall>,
    /// The interpreter's report, absent while the script is still running.
    pub outcome: Option<ScriptRun>,
}

impl ScriptNode {
    /// Whether the interpreter has reported back.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.outcome.is_none()
    }
}

/// Token accounting the provider reported for this turn.
///
/// `None` fields mean the provider reported nothing, which is different from reporting zero and is
/// preserved rather than flattened.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TurnTokens {
    /// Sum of reported input tokens.
    pub input: Option<u64>,
    /// Sum of reported output tokens.
    pub output: Option<u64>,
    /// Model responses that reported no usage at all.
    pub unreported: u32,
}

impl TurnTokens {
    fn absorb(&mut self, usage: Option<ModelUsage>) {
        let Some(usage) = usage else {
            self.unreported = self.unreported.saturating_add(1);
            return;
        };
        if let Some(input) = usage.input_tokens {
            self.input = Some(self.input.unwrap_or_default().saturating_add(input));
        }
        if let Some(output) = usage.output_tokens {
            self.output = Some(self.output.unwrap_or_default().saturating_add(output));
        }
        if usage.input_tokens.is_none() && usage.output_tokens.is_none() {
            self.unreported = self.unreported.saturating_add(1);
        }
    }
}

/// One exchange: what was asked, everything that happened, and how it ended.
#[derive(Clone, Debug)]
pub struct Turn {
    /// Ordinal within this conversation, from one.
    pub ordinal: usize,
    /// Exactly what was typed.
    pub prompt: String,
    /// Scripts the model ran, in order.
    pub scripts: Vec<ScriptNode>,
    /// Accounting across the turn's model responses.
    pub tokens: TurnTokens,
    /// How it ended.
    pub status: TurnStatus,
    /// Whether a stop was requested before it finished.
    pub stop_requested: bool,
    /// Wall-clock time, once it has finished.
    pub elapsed: Option<Duration>,
    /// Whether this turn is still inside the model's replay window.
    ///
    /// Recomputed from the live [`dekopon_agent::prompt::History`] after every turn rather than
    /// stored once: a turn leaves the window because *later* turns pushed it out, so its own
    /// recording cannot know.
    pub in_replay_window: bool,
}

impl Turn {
    /// Opens a turn for a prompt that has just been submitted.
    #[must_use]
    pub fn opened(ordinal: usize, prompt: String) -> Self {
        Self {
            ordinal,
            prompt,
            scripts: Vec::new(),
            tokens: TurnTokens::default(),
            status: TurnStatus::Running,
            stop_requested: false,
            elapsed: None,
            in_replay_window: true,
        }
    }

    /// Capability invocations across every script in this turn.
    #[must_use]
    pub fn capability_calls(&self) -> usize {
        self.scripts.iter().map(|script| script.calls.len()).sum()
    }

    /// Capability invocations policy refused.
    ///
    /// Counted separately because it is the number that changes what an operator does next: a turn
    /// with denials is a policy question, and a turn with failures is a provider question.
    #[must_use]
    pub fn denied_calls(&self) -> usize {
        self.scripts
            .iter()
            .flat_map(|script| &script.calls)
            .filter(|call| matches!(call.outcome, crate::record::CallOutcome::Denied(_)))
            .count()
    }

    /// Folds one session event into this turn.
    ///
    /// A capability that arrives with no script open is still recorded, under a synthetic script
    /// node: dropping it would make the console quietly lose a call that really happened, which is
    /// the one thing this view exists to prevent.
    pub fn absorb(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::ScriptStarted { sequence, script } => {
                self.scripts.push(ScriptNode {
                    sequence,
                    script,
                    calls: Vec::new(),
                    outcome: None,
                });
            }
            SessionEvent::Capability(call) => match self
                .scripts
                .iter_mut()
                .rev()
                .find(|script| script.is_running())
            {
                Some(script) => script.calls.push(*call),
                None => self.scripts.push(ScriptNode {
                    sequence: call.sequence,
                    script: String::new(),
                    calls: vec![*call],
                    outcome: None,
                }),
            },
            SessionEvent::ScriptFinished(run) => {
                match self
                    .scripts
                    .iter_mut()
                    .find(|script| script.sequence == run.sequence)
                {
                    Some(script) => script.outcome = Some(*run),
                    None => self.scripts.push(ScriptNode {
                        sequence: run.sequence,
                        script: run.script.clone(),
                        calls: Vec::new(),
                        outcome: Some(*run),
                    }),
                }
            }
            SessionEvent::ModelUsage(usage) => self.tokens.absorb(usage),
            SessionEvent::Finished(outcome) => {
                self.status = match *outcome {
                    Ok(outcome) => match outcome.disposition {
                        dekopon_agent::prompt::ReplyDisposition::Send => {
                            TurnStatus::Answered(outcome.answer)
                        }
                        dekopon_agent::prompt::ReplyDisposition::Suppress => TurnStatus::Suppressed,
                    },
                    Err(error) => TurnStatus::Failed(error),
                };
            }
        }
    }
}

/// Every turn of one agent conversation, in order.
#[derive(Clone, Debug, Default)]
pub struct Transcript {
    turns: Vec<Turn>,
}

impl Transcript {
    /// The turns, oldest first.
    #[must_use]
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// Whether anything has been asked yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    /// Opens a turn and returns its index.
    pub fn open(&mut self, prompt: String) -> usize {
        self.turns.push(Turn::opened(self.turns.len() + 1, prompt));
        self.turns.len() - 1
    }

    /// The turn currently accepting events, if one is open.
    pub fn running_mut(&mut self) -> Option<&mut Turn> {
        self.turns
            .iter_mut()
            .rev()
            .find(|turn| turn.status.is_running())
    }

    /// Folds one event into the open turn, if there is one.
    ///
    /// Events arriving with no open turn are dropped rather than opening one: a turn exists because
    /// somebody typed a prompt, and inventing one would put a turn on screen nobody asked for.
    pub fn absorb(&mut self, event: SessionEvent) {
        if let Some(turn) = self.running_mut() {
            turn.absorb(event);
        }
    }

    /// Marks how far back the model's replay window currently reaches.
    ///
    /// `remembered` is the number of exchanges the live history holds. The window is a suffix, so
    /// the last `remembered` turns are inside it and everything older is not.
    pub fn mark_replay_window(&mut self, remembered: usize) {
        let boundary = self.turns.len().saturating_sub(remembered);
        for (index, turn) in self.turns.iter_mut().enumerate() {
            turn.in_replay_window = index >= boundary;
        }
    }
}

#[cfg(test)]
mod tests;
