use std::time::Duration;

use dekopon_model::{ModelText, error::InferenceError};

use crate::prompt::PromptError;

/// Matches the embedding gateway's own outbound-answer bound (8 KiB), since a progress surface
/// never needs more text than the reply itself; deltas past it still reach the trace, just not this
/// sink.
pub(crate) const STREAMED_TEXT_BOUND_BYTES: usize = 8 * 1024;

/// A command word is provider-authored, so it is untrusted text on its way to a person's screen
/// even though it is not itself a credential path.
const MAX_COMMAND_WORD_CHARS: usize = 32;
const MAX_NOTE_CHARS: usize = 100;
const NOTE_FORMAT_CHARACTERS: &[char] = &[
    '\u{200b}', '\u{200c}', '\u{200d}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}',
    '\u{202d}', '\u{202e}', '\u{2060}', '\u{2061}', '\u{2062}', '\u{2063}', '\u{2064}', '\u{2066}',
    '\u{2067}', '\u{2068}', '\u{2069}', '\u{feff}',
];

#[derive(Clone, Debug)]
pub enum ProgressEvent {
    Started {
        agent: String,
        max_steps: u32,
    },
    ModelTurn {
        turn: u32,
        of: u32,
    },
    Steered {
        turn: u32,
    },
    Note {
        text: ProgressNote,
        eta: Option<Duration>,
    },
    TextDelta {
        /// Never reasoning or tool-call arguments, only visible answer text; cumulative_chars lets
        /// a driver enforce its own length ceiling without re-accumulating deltas itself.
        turn: u32,
        text: ModelText,
        cumulative_chars: usize,
    },
    Answered {
        turn: u32,
        tool_calls: u32,
        duration: Duration,
        first_delta: Option<Duration>,
    },
    ToolStarted {
        word: CommandWord,
        argument_count: u32,
        calls_used: u32,
        calls_max: u32,
    },
    ToolFinished {
        word: CommandWord,
        outcome: ToolOutcome,
        duration: Duration,
    },
    Attachment {
        index: u32,
        /// IANA media type, fixed by validation rather than by what the provider claimed.
        media_type: String,
        bytes: u64,
    },
    KeepAlive {
        elapsed: Duration,
        count: u32,
    },
    Cancelled {
        by: CancelSource,
    },
    Failed {
        class: FailureClass,
    },
    Finished {
        outcome: SessionOutcome,
        elapsed: Duration,
        turns: u32,
        tool_calls: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandWord(String);

impl CommandWord {
    pub(crate) fn new(word: &str) -> Self {
        let mut bounded: String = word.chars().take(MAX_COMMAND_WORD_CHARS).collect();
        if word.chars().nth(MAX_COMMAND_WORD_CHARS).is_some() {
            bounded.push('…');
        }
        Self(bounded)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressNote(String);

impl ProgressNote {
    pub(crate) fn new(raw: &str) -> Option<Self> {
        let mut text = String::new();
        let mut count = 0;
        let mut separator = false;
        for character in raw.chars() {
            if matches!(character, '<' | '>' | '@' | '[' | ']' | '`')
                || NOTE_FORMAT_CHARACTERS.contains(&character)
            {
                continue;
            }
            if character.is_whitespace() || character.is_control() {
                separator = !text.is_empty();
                continue;
            }
            if separator && count < MAX_NOTE_CHARS {
                text.push(' ');
                count += 1;
            }
            if count == MAX_NOTE_CHARS {
                text.truncate(text.trim_end().len());
                text.push('…');
                return Some(Self(text));
            }
            text.push(character);
            count += 1;
            separator = false;
        }
        (!text.is_empty()).then_some(Self(text))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    Succeeded,
    Denied,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureClass {
    StepBudget,
    OverBudget,
    Model,
    Internal,
}

impl FailureClass {
    #[must_use]
    pub fn of(error: &PromptError) -> Option<Self> {
        match error {
            PromptError::Cancelled | PromptError::Model(InferenceError::Cancelled) => None,
            PromptError::MaxSteps { .. } => Some(Self::StepBudget),
            PromptError::Model(InferenceError::OverBudget(_)) => Some(Self::OverBudget),
            PromptError::ZeroSteps => Some(Self::Internal),
            PromptError::Model(_)
            | PromptError::UnknownTool(_)
            | PromptError::TooManyToolCalls { .. }
            | PromptError::EmptyToolCallId
            | PromptError::InvalidArguments { .. }
            | PromptError::ArgumentsNotObject { .. }
            | PromptError::AgentConfigArgumentsNotEmpty { .. }
            | PromptError::DeclineReplyArgumentsNotEmpty { .. }
            | PromptError::MissingScript { .. }
            | PromptError::MissingAssetId { .. }
            | PromptError::MissingSkillName { .. }
            | PromptError::UnexpectedSkillArguments { .. }
            | PromptError::InvalidSuggestion { .. }
            | PromptError::InvalidWake { .. }
            | PromptError::UnreportedCapabilityWork
            | PromptError::EmptyAnswer => Some(Self::Model),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionOutcome {
    Answered,
    Declined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelVia {
    NativeStop,
    Button,
    StopReply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetLimit {
    WallClock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelSource {
    User { via: CancelVia },
    Operator,
    Budget { limit: BudgetLimit },
}

/// Synchronous by design so a progress sink can never slow the session down; an implementation must
/// record what it needs, hand the rest off, and never block, await, or fail.
pub trait ProgressSink: Send + Sync {
    fn emit(&self, event: ProgressEvent);
}

#[cfg(test)]
mod tests {
    use dekopon_model::error::InferenceError;

    use super::{CommandWord, FailureClass, MAX_COMMAND_WORD_CHARS, ProgressNote};
    use crate::prompt::PromptError;

    #[test]
    fn a_note_deletes_mention_and_markdown_delimiters() {
        assert_eq!(
            ProgressNote::new("<!channel> ping @here [x](https://e.example)")
                .unwrap()
                .as_str(),
            "!channel ping here x(https://e.example)"
        );
        assert_eq!(ProgressNote::new("`code`").unwrap().as_str(), "code");
    }

    #[test]
    fn a_note_deletes_format_characters() {
        assert_eq!(ProgressNote::new("a\u{202e}b").unwrap().as_str(), "ab");
    }

    #[test]
    fn a_note_collapses_whitespace_and_controls_between_kept_characters() {
        assert_eq!(
            ProgressNote::new("a\n\tb \u{7}  c").unwrap().as_str(),
            "a b c"
        );
    }

    #[test]
    fn a_long_note_is_cut_by_characters_with_a_marker() {
        let note = ProgressNote::new(&"あ".repeat(150)).unwrap();
        assert_eq!(note.as_str().chars().count(), 101);
        assert!(note.as_str().ends_with('…'));
    }

    #[test]
    fn a_note_empty_after_cleaning_is_absent() {
        assert_eq!(ProgressNote::new("  <@>  "), None);
    }

    #[test]
    fn a_command_word_past_the_rendering_bound_is_marked_rather_than_refused() {
        let word = "a".repeat(MAX_COMMAND_WORD_CHARS + 8);

        let bounded = CommandWord::new(&word);

        assert_eq!(
            bounded.as_str().chars().count(),
            MAX_COMMAND_WORD_CHARS + 1,
            "a bounded word keeps the ceiling plus the marker"
        );
        assert!(
            bounded.as_str().ends_with('…'),
            "a truncated word says so: {}",
            bounded.as_str()
        );
    }

    #[test]
    fn a_word_inside_the_bound_is_carried_verbatim() {
        assert_eq!(CommandWord::new("gh").as_str(), "gh");
    }

    #[test]
    fn a_cancellation_is_an_outcome_rather_than_a_failure_class() {
        assert_eq!(FailureClass::of(&PromptError::Cancelled), None);
        assert_eq!(
            FailureClass::of(&PromptError::Model(InferenceError::Cancelled)),
            None,
            "an interrupted stream is the session being stopped, not the model failing"
        );
    }

    #[test]
    fn the_turn_ceiling_and_a_broken_model_are_told_apart() {
        assert_eq!(
            FailureClass::of(&PromptError::MaxSteps { maximum: 8 }),
            Some(FailureClass::StepBudget)
        );
        assert_eq!(
            FailureClass::of(&PromptError::Model(InferenceError::Protocol(
                dekopon_model::error::ProtocolFailure::NoChoices
            ))),
            Some(FailureClass::Model)
        );
        assert_eq!(
            FailureClass::of(&PromptError::ZeroSteps),
            Some(FailureClass::Internal),
            "a zero-step session is the embedder asking for something impossible"
        );
    }

    #[test]
    fn a_budget_refusal_is_not_a_model_failure() {
        let refusal = dekopon_model_token_governor::Refusal {
            agent: "gylmar".parse().unwrap(),
            meter: dekopon_model_token_governor::MeterKind::Credit,
            limit: dekopon_model_token_governor::Tokens(10),
            remaining: 0,
            requested: dekopon_model_token_governor::Tokens(5),
            retry: dekopon_model_token_governor::Retry::Never,
        };
        assert_eq!(
            FailureClass::of(&PromptError::Model(InferenceError::OverBudget(refusal))),
            Some(FailureClass::OverBudget)
        );
    }
}
