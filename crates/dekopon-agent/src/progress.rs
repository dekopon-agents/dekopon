//! What a person waiting on one chat message may be told while a session runs.
//!
//! The prompt loop and the broker leg already know when a model turn starts, when text arrives,
//! when a command word runs, and how a session ended; until now that knowledge reached only the
//! trace. [`ProgressSink`] is the third synchronous observer on
//! [`SessionInputs`](crate::prompt::SessionInputs), beside
//! [`ModelUsageObserver`](crate::prompt::ModelUsageObserver) and
//! [`CancellationProbe`](crate::prompt::CancellationProbe): the loop hands it a
//! [`ProgressEvent`] at each seam and an embedding gateway decides what, if anything, to render.
//!
//! The vocabulary is metadata by construction, with one deliberate exception.
//! [`ProgressEvent::TextDelta`] carries model-authored answer text — the same class of text the
//! final reply carries — and it travels in [`dekopon_model::ModelText`], which only the model
//! client can build from bytes. The only other string is [`CommandWord`], the provider-authored
//! command word the trace already carries as `shell.command.name`, bounded here because it is
//! rendered to a person. A prompt, a shell argument, a tool result, a provider result, and
//! attachment bytes have no field to travel in.
//!
//! Emission is synchronous and on the loop's own blocking thread, so an implementation must never
//! block: record, hand off, and return.

use std::time::Duration;

use dekopon_model::{ModelText, model::ModelError};

use crate::prompt::PromptError;

/// Cumulative streamed text one turn forwards to a sink before it stops.
///
/// The same 8 KiB an embedding gateway bounds an outbound answer to: a progress surface never
/// needs more text than the answer will carry, and the loop must not hold a rendering task's
/// channel open for a model that decided to emit a megabyte. Deltas past it are still accumulated
/// for the trace; they are simply not shown.
pub(crate) const STREAMED_TEXT_BOUND_BYTES: usize = 8 * 1024;

/// Longest command word a progress surface is asked to render, in characters.
///
/// A command word is provider-authored (it arrives with the capability snapshot), so it is
/// untrusted text on its way to a person's screen even though it is not a credential path.
const MAX_COMMAND_WORD_CHARS: usize = 32;

/// One thing that happened inside a running session.
///
/// Produced by the prompt loop and the broker leg, except [`ProgressEvent::Started`], which the
/// embedding gateway emits because it owns the grant, and [`ProgressEvent::KeepAlive`], which a
/// gateway's own clock synthesizes when nothing has happened.
#[derive(Clone, Debug)]
pub enum ProgressEvent {
    /// A fresh grant was received and spending starts now. Once per session.
    Started {
        /// The agent answering, as the gateway names it.
        agent: String,
        /// Model turns this session may spend.
        max_steps: u32,
    },
    /// A model request is in flight. `turn` is 1-based.
    ModelTurn {
        /// This request's 1-based turn number.
        turn: u32,
        /// The session's turn ceiling.
        of: u32,
    },
    /// One streamed fragment of the model's visible text for `turn`.
    ///
    /// Never reasoning, never tool-call arguments. `text` is the fragment; `cumulative_chars`
    /// counts the characters of the turn's text so far, including this fragment, so a driver can
    /// stop at its own ceiling without accumulating first.
    TextDelta {
        /// The 1-based turn producing the text.
        turn: u32,
        /// The fragment itself.
        text: ModelText,
        /// Characters of this turn's text so far, this fragment included.
        cumulative_chars: usize,
    },
    /// The model answered `turn`. `tool_calls == 0` means the answer is final.
    Answered {
        /// The 1-based turn that answered.
        turn: u32,
        /// Tool calls this turn requested.
        tool_calls: u32,
        /// How long the model request took.
        duration: Duration,
        /// Time to the first streamed fragment, absent when the response did not stream.
        first_delta: Option<Duration>,
    },
    /// The session is about to run one command word or capability through the broker leg.
    ToolStarted {
        /// The capability identifier or provider command word.
        word: CommandWord,
        /// How many arguments it was given: an argv length, or a JSON input's field count.
        argument_count: u32,
        /// Capability invocations this session has proposed, including this one when it is one.
        calls_used: u32,
        /// The session's capability-call ceiling.
        calls_max: u32,
    },
    /// That command word or capability answered.
    ToolFinished {
        /// The capability identifier or provider command word.
        word: CommandWord,
        /// What the broker, the provider, or the session's own cancellation decided.
        outcome: ToolOutcome,
        /// How long the round trip took.
        duration: Duration,
    },
    /// A capability result carried an attachment this reply accepted.
    Attachment {
        /// Position in the reply being assembled, as
        /// [`GeneratedImage::filename`](crate::attachment::GeneratedImage::filename) numbers it.
        index: u32,
        /// IANA media type, fixed by validation rather than by what the provider claimed.
        media_type: String,
        /// Decoded size.
        bytes: u64,
    },
    /// Nothing changed. Synthesized by a gateway's clock, never by the loop.
    KeepAlive {
        /// Time since the session started.
        elapsed: Duration,
        /// This keep-alive's 1-based number.
        count: u32,
    },
    /// The session stopped at a cooperative boundary.
    Cancelled {
        /// What asked for the stop.
        by: CancelSource,
    },
    /// The session broke.
    Failed {
        /// What a person can act on, derived from [`PromptError`].
        class: FailureClass,
    },
    /// The session finished and its answer is being delivered.
    Finished {
        /// Whether there is an answer to deliver.
        outcome: SessionOutcome,
        /// How long the loop ran.
        elapsed: Duration,
        /// Model turns spent.
        turns: u32,
        /// Tool calls the model requested across every turn.
        tool_calls: u32,
    },
}

/// A capability identifier or provider command word, bounded for display.
///
/// Provider-authored: the word arrives with the broker's capability snapshot, which is why only
/// the broker leg constructs one. The bound is a rendering bound, not a validation one — a word
/// past it is shown truncated with a marker rather than refused, because refusing here would turn
/// a cosmetic surface into a session failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandWord(String);

impl CommandWord {
    /// Bounds one provider-authored word for a progress surface.
    pub(crate) fn new(word: &str) -> Self {
        let mut bounded: String = word.chars().take(MAX_COMMAND_WORD_CHARS).collect();
        if word.chars().nth(MAX_COMMAND_WORD_CHARS).is_some() {
            bounded.push('…');
        }
        Self(bounded)
    }

    /// The bounded word.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How one command word or capability call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    /// The provider answered or the broker authorized and ran it.
    Succeeded,
    /// Authorization refused it, or the gateway refused it before the broker saw it.
    Denied,
    /// It ran and errored, or never reached an answer.
    Failed,
    /// The session was stopped underneath it.
    Cancelled,
}

/// What a person can act on when a session breaks.
///
/// Exactly the classes [`FailureClass::of`] produces. A class no [`PromptError`] maps to would be
/// a case every surface has to render and no session can ever reach, which is why a wall-clock
/// bound and a provider failure are absent: a route's `maxDurationMs` is the embedder cancelling
/// the session, and arrives as [`CancelSource::Budget`] rather than as a break, while a broker or
/// provider failure reaches the loop as a script exit status the model reads and answers itself.
///
/// `of` names every error variant, so a new error kind is a compile error there rather than a
/// silent [`FailureClass::Internal`] here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureClass {
    /// The model turn ceiling was reached without a final answer.
    StepBudget,
    /// The model failed, or produced something the loop cannot run.
    Model,
    /// The embedding surface asked for something impossible.
    Internal,
}

impl FailureClass {
    /// Classifies one broken session, or answers `None` for a cancellation.
    ///
    /// Cancellation is a terminal *outcome*, not a failure: the loop reports it as
    /// [`ProgressEvent::Cancelled`] carrying who asked, and an interrupted model stream is a
    /// cancellation rather than a model error however the transport reported it.
    #[must_use]
    pub fn of(error: &PromptError) -> Option<Self> {
        match error {
            PromptError::Cancelled | PromptError::Model(ModelError::Interrupted) => None,
            PromptError::MaxSteps { .. } => Some(Self::StepBudget),
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
            | PromptError::UnreportedCapabilityWork
            | PromptError::EmptyAnswer => Some(Self::Model),
        }
    }
}

/// Whether a finished session has an answer to deliver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionOutcome {
    /// The model answered and the answer is being delivered.
    Answered,
    /// An optional continuation declined; nothing is posted.
    Declined,
}

/// The affordance a person used to stop a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelVia {
    /// A chat service's own Stop control.
    NativeStop,
    /// A button the gateway put on its progress message.
    Button,
    /// A stop word replied into the conversation.
    StopReply,
}

/// Which bound a budget cancellation spent.
///
/// One bound, because one bound cancels: a route's `maxDurationMs`. Running out of model turns is
/// not a cancellation — the loop breaks and reports [`FailureClass::StepBudget`] — and the
/// capability-call ceiling is a shell budget error the script reads and the model recovers from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetLimit {
    /// Wall clock since the session started.
    WallClock,
}

/// What asked for a stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelSource {
    /// The authenticated sender of the message, through one of the affordances.
    User {
        /// Which affordance they used.
        via: CancelVia,
    },
    /// The embedder itself: a shutdown, or a stop it did not attribute further.
    Operator,
    /// A route limit elapsed.
    Budget {
        /// Which limit.
        limit: BudgetLimit,
    },
}

/// Receives one [`ProgressEvent`] per seam, synchronously, on the session's own thread.
///
/// Synchronous on purpose: the prompt loop and the model client run on a blocking task, and a
/// progress surface must never be able to slow an answer down. An implementation records what it
/// needs and hands the rest to something else — the gateway's adapter writes a trace record and
/// `try_send`s into a bounded channel — and never blocks, awaits, or fails.
pub trait ProgressSink: Send + Sync {
    /// Reports one event. Must not block.
    fn emit(&self, event: ProgressEvent);
}

#[cfg(test)]
mod tests {
    use dekopon_model::model::ModelError;

    use super::{CommandWord, FailureClass, MAX_COMMAND_WORD_CHARS};
    use crate::prompt::PromptError;

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
            FailureClass::of(&PromptError::Model(ModelError::Interrupted)),
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
            FailureClass::of(&PromptError::Model(ModelError::NoChoices)),
            Some(FailureClass::Model)
        );
        assert_eq!(
            FailureClass::of(&PromptError::ZeroSteps),
            Some(FailureClass::Internal),
            "a zero-step session is the embedder asking for something impossible"
        );
    }
}
