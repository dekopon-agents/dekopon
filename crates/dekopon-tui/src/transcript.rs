use std::time::Duration;

use dekopon_model_token_governor::ModelUsage;

use crate::record::{CapabilityCall, ScriptRun, SessionEvent};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnStatus {
    Running,
    Answered(String),
    Suppressed,
    Failed(String),
}

impl TurnStatus {
    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScriptNode {
    pub sequence: u64,
    pub script: String,
    pub calls: Vec<CapabilityCall>,
    pub outcome: Option<ScriptRun>,
}

impl ScriptNode {
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.outcome.is_none()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TurnTokens {
    pub input: Option<u64>,
    pub output: Option<u64>,
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

#[derive(Clone, Debug)]
pub struct Turn {
    pub ordinal: usize,
    pub prompt: String,
    pub scripts: Vec<ScriptNode>,
    pub tokens: TurnTokens,
    pub status: TurnStatus,
    pub stop_requested: bool,
    pub elapsed: Option<Duration>,
    pub in_replay_window: bool,
}

impl Turn {
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

    #[must_use]
    pub fn capability_calls(&self) -> usize {
        self.scripts.iter().map(|script| script.calls.len()).sum()
    }

    #[must_use]
    pub fn denied_calls(&self) -> usize {
        self.scripts
            .iter()
            .flat_map(|script| &script.calls)
            .filter(|call| matches!(call.outcome, crate::record::CallOutcome::Denied(_)))
            .count()
    }

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
            SessionEvent::Progress(_) | SessionEvent::ShellFinished(_) => {}
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

#[derive(Clone, Debug, Default)]
pub struct Transcript {
    turns: Vec<Turn>,
}

impl Transcript {
    #[must_use]
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    pub fn open(&mut self, prompt: String) -> usize {
        self.turns.push(Turn::opened(self.turns.len() + 1, prompt));
        self.turns.len() - 1
    }

    pub fn running_mut(&mut self) -> Option<&mut Turn> {
        self.turns
            .iter_mut()
            .rev()
            .find(|turn| turn.status.is_running())
    }

    pub fn absorb(&mut self, event: SessionEvent) {
        if let Some(turn) = self.running_mut() {
            turn.absorb(event);
        }
    }

    pub fn mark_replay_window(&mut self, remembered: usize) {
        let boundary = self.turns.len().saturating_sub(remembered);
        for (index, turn) in self.turns.iter_mut().enumerate() {
            turn.in_replay_window = index >= boundary;
        }
    }
}

#[cfg(test)]
mod tests;
