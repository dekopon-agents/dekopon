use dekopon_model::model::{AssistantTurn, ModelMessage, assistant_message};

pub const DEFAULT_MAX_TURNS: usize = 16;

pub const DEFAULT_MAX_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryLimits {
    pub max_turns: usize,
    pub max_bytes: usize,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        Self {
            max_turns: DEFAULT_MAX_TURNS,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// Stored as plain text, not ModelMessages, because a remembered system message, an orphaned tool
/// result, or provider-specific replay state would each corrupt a later request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationTurn {
    user: String,
    answer: Option<String>,
}

impl ConversationTurn {
    #[must_use]
    pub fn completed(user: impl Into<String>, answer: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            answer: Some(answer.into()),
        }
    }

    #[must_use]
    pub fn unanswered(user: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            answer: None,
        }
    }

    #[must_use]
    pub fn user(&self) -> &str {
        &self.user
    }

    #[must_use]
    pub fn answer(&self) -> Option<&str> {
        self.answer.as_deref()
    }

    #[must_use]
    pub fn is_answered(&self) -> bool {
        self.answer.is_some()
    }

    #[must_use]
    pub fn bytes(&self) -> usize {
        self.user
            .len()
            .saturating_add(self.answer.as_ref().map_or(0, String::len))
    }

    fn replay_into(&self, messages: &mut Vec<ModelMessage>) {
        messages.push(ModelMessage::user(&self.user));
        if let Some(answer) = &self.answer {
            messages.push(assistant_message(&AssistantTurn::new(
                Some(answer.clone()),
                Vec::new(),
                None,
            )));
        }
    }
}

/// Kept transport-neutral as text because provider-specific replay state, like ChatGPT's encrypted
/// reasoning, silently disappears with no error when replayed against a different backend.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct History {
    turns: Vec<ConversationTurn>,
    limits: HistoryLimits,
}

impl History {
    #[must_use]
    pub fn new(limits: HistoryLimits) -> Self {
        Self {
            turns: Vec::new(),
            limits,
        }
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn from_turns(
        limits: HistoryLimits,
        turns: impl IntoIterator<Item = ConversationTurn>,
    ) -> Self {
        let mut history = Self::new(limits);
        for turn in turns {
            history.record(turn);
        }
        history
    }

    #[must_use]
    pub fn limits(&self) -> HistoryLimits {
        self.limits
    }

    #[must_use]
    pub fn turns(&self) -> &[ConversationTurn] {
        &self.turns
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.turns.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    #[must_use]
    pub fn bytes(&self) -> usize {
        self.turns
            .iter()
            .map(ConversationTurn::bytes)
            .fold(0, usize::saturating_add)
    }

    /// Only the prompt and answer are kept, not the tool calls between them, so trimming can never
    /// orphan half of a remembered exchange.
    pub fn record(&mut self, turn: ConversationTurn) {
        self.turns.push(turn);
        self.trim();
    }

    pub(super) fn replay_into(&self, messages: &mut Vec<ModelMessage>) {
        for turn in &self.turns {
            turn.replay_into(messages);
        }
    }

    fn trim(&mut self) {
        if self.turns.len() > self.limits.max_turns {
            let excess = self.turns.len() - self.limits.max_turns;
            self.turns.drain(..excess);
        }

        let mut bytes = self.bytes();
        let mut dropped = 0;
        for turn in &self.turns {
            if bytes <= self.limits.max_bytes {
                break;
            }
            bytes = bytes.saturating_sub(turn.bytes());
            dropped += 1;
        }
        self.turns.drain(..dropped);
    }
}
