//! What one prompt session remembers of the sessions before it.
//!
//! [`run_prompt`](super::run_prompt) builds a message vector, grows it across turns, and drops it
//! on the way out. That is correct for a one-shot CLI invocation and wrong for a chat transport,
//! where the operator's next message is a continuation rather than a new conversation. This module
//! is the thing that survives between calls.
//!
//! It stores *turns*, not messages, and it stores them as text. Both choices exist to delete
//! whole classes of request-shaping bug rather than to document them; [`ConversationTurn`] names
//! each one.

use dekopon_model::model::{AssistantTurn, ModelMessage, assistant_message};

/// Completed exchanges a session replays before the oldest are trimmed.
///
/// Chosen so a long chat thread stays in context while a very long one still stops growing. The
/// byte bound below is the one that usually binds; this bound is what keeps a thread of one-word
/// exchanges from accumulating turns forever under it.
pub const DEFAULT_MAX_TURNS: usize = 16;

/// Bytes of replayed conversation a session carries by default.
///
/// Roughly eight thousand tokens of prose, which fits comfortably inside every model this
/// workspace targets while still covering a substantial thread. Compaction is what makes a bound
/// this small usable: a single script's output may be 256 KiB
/// (`dekopon_shell::limits::DEFAULT_MAX_OUTPUT_BYTES`), so an uncompacted transcript of one
/// eight-step message can exceed a megabyte on its own and would blow through this within one
/// exchange.
pub const DEFAULT_MAX_BYTES: usize = 32 * 1024;

/// Bounds on how much earlier conversation a session replays.
///
/// Both bounds are in units this workspace can measure *before* it builds a request. There is no
/// tokenizer here, and token counts arrive only after a call has been billed, in
/// `dekopon_model::model::ModelUsage` — so a token bound could only ever be enforced one request
/// too late. Bytes are known up front and are what the request costs to send, which makes bytes
/// the bound that actually binds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryLimits {
    /// Completed exchanges to keep, oldest trimmed first.
    pub max_turns: usize,
    /// Total bytes of remembered prompts and answers to keep, oldest trimmed first.
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

/// One exchange as it is remembered: what the operator asked, and what the agent answered back.
///
/// This is text rather than a pair of [`ModelMessage`]s, and that is the whole design. Roles are
/// assigned at replay from the field a value sits in, so three failures a stored message vector
/// invites cannot be expressed here at all:
///
/// - **A remembered system message.** The ChatGPT backend joins every `system` message it is
///   handed into one `instructions` string. History that kept the system prompt, replayed by a
///   caller that also prepends the current one, would send an agent its own instructions
///   concatenated with themselves — growing by one copy per exchange, mutating the front of every
///   request, and raising no error anywhere. There is no field here a system message could sit in,
///   so the system prompt is necessarily supplied fresh on each call. That also means an operator
///   can edit an agent's instructions and have the edit take effect immediately, without
///   rewriting stored conversations.
/// - **An orphaned tool result.** A `tool` message whose `tool_call_id` has no preceding assistant
///   `tool_calls`, or an assistant `tool_calls` with no matching results, is a 400 on both
///   backends. Trimming a flat message vector produces exactly that whenever the cut lands inside
///   a turn. Here the intermediate traffic is never stored (see [`History::record`]) and a cut can
///   only land between turns.
/// - **Provider replay state outliving its session.** An assistant message can carry opaque
///   provider items — the ChatGPT path's encrypted reasoning among them — which are only safe to
///   replay next to the items they belong to. Remembering the answer as text keeps that state
///   inside the single session that produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationTurn {
    user: String,
    answer: Option<String>,
}

impl ConversationTurn {
    /// Records an exchange the agent answered.
    #[must_use]
    pub fn completed(user: impl Into<String>, answer: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            answer: Some(answer.into()),
        }
    }

    /// Records an exchange that ended without an answer.
    ///
    /// A session that exhausts its step budget, loses its model connection, or refuses a
    /// malformed tool call still consumed the operator's message. Dropping the turn would erase
    /// it: the operator would see a failure, retry with a follow-up, and the agent would have no
    /// idea what the follow-up referred to. Replaying a prompt with no answer beside it is a
    /// truthful record of that, and consecutive user messages are well-formed on both backends.
    #[must_use]
    pub fn unanswered(user: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            answer: None,
        }
    }

    /// Returns the operator's prompt.
    #[must_use]
    pub fn user(&self) -> &str {
        &self.user
    }

    /// Returns the agent's final answer, absent when the session never produced one.
    #[must_use]
    pub fn answer(&self) -> Option<&str> {
        self.answer.as_deref()
    }

    /// Reports whether this exchange reached an answer.
    #[must_use]
    pub fn is_answered(&self) -> bool {
        self.answer.is_some()
    }

    /// Bytes this exchange contributes to [`HistoryLimits::max_bytes`].
    ///
    /// The remembered text, not the encoded request: request framing is the backend's business and
    /// differs per transport, while this number is stable, cheap, and proportional to the thing
    /// that actually grows.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.user
            .len()
            .saturating_add(self.answer.as_ref().map_or(0, String::len))
    }

    /// Renders this exchange as the messages a later session replays.
    ///
    /// The empty `replay_items` here is load-bearing, not incidental. The ChatGPT backend gives an
    /// assistant message two mutually exclusive serializations: a message carrying replay items is
    /// emitted as *only* those items, and its `content` and `tool_calls` are discarded. An
    /// assistant message reconstructed from remembered text must therefore carry no replay items,
    /// or the remembered answer would vanish from the request without an error. The field is not
    /// readable from outside `dekopon-model`, so this end of the contract is held by never putting
    /// anything in it: history stores text and reconstructs the text-only shape.
    fn replay_into(&self, messages: &mut Vec<ModelMessage>) {
        messages.push(ModelMessage::user(&self.user));
        if let Some(answer) = &self.answer {
            // The remembered answer is by construction the turn that ended *without* tool calls,
            // so this reconstructs the only assistant message shape history ever holds.
            messages.push(assistant_message(&AssistantTurn {
                content: Some(answer.clone()),
                tool_calls: Vec::new(),
                usage: None,
                replay_items: Vec::new(),
            }));
        }
    }
}

/// A bounded window of earlier exchanges, oldest trimmed first.
///
/// A `History` carries its own bounds rather than reading them from
/// [`PromptLimits`](super::PromptLimits): the two answer different questions — how much work one
/// session may do, versus how much of the past a conversation drags along — and they belong to
/// different owners, since the conversation outlives any single session's limits.
///
/// A window is transport-neutral, which is a deliberate property rather than a coincidence.
/// Provider replay state does not survive a transport change — the ChatGPT path's encrypted
/// reasoning is `#[serde(skip)]` and simply disappears if the same message is sent to an
/// OpenAI-compatible chat-completions endpoint, with no error and no warning. A conversation that
/// remembered messages would therefore degrade silently the moment an operator repointed an agent
/// at a different model. Remembering text means the same window replays identically on either
/// backend, and that a model change costs the agent nothing but its own reasoning traces.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct History {
    turns: Vec<ConversationTurn>,
    limits: HistoryLimits,
}

impl History {
    /// Creates an empty history under the given bounds.
    #[must_use]
    pub fn new(limits: HistoryLimits) -> Self {
        Self {
            turns: Vec::new(),
            limits,
        }
    }

    /// Rebuilds a history from stored exchanges, applying the bounds as it goes.
    ///
    /// Bounds are applied on the way in rather than trusted from the caller, so a window restored
    /// from somewhere durable is the same size as one that grew here.
    #[must_use]
    pub fn from_turns(
        limits: HistoryLimits,
        turns: impl IntoIterator<Item = ConversationTurn>,
    ) -> Self {
        let mut history = Self::new(limits);
        for turn in turns {
            history.record(turn);
        }
        history
    }

    /// Returns the bounds this history trims to.
    #[must_use]
    pub fn limits(&self) -> HistoryLimits {
        self.limits
    }

    /// Returns the remembered exchanges, oldest first.
    #[must_use]
    pub fn turns(&self) -> &[ConversationTurn] {
        &self.turns
    }

    /// Returns the number of remembered exchanges.
    #[must_use]
    pub fn len(&self) -> usize {
        self.turns.len()
    }

    /// Reports whether anything is remembered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    /// Returns the bytes currently held against [`HistoryLimits::max_bytes`].
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.turns
            .iter()
            .map(ConversationTurn::bytes)
            .fold(0, usize::saturating_add)
    }

    /// Remembers one exchange, then trims the oldest until the window fits.
    ///
    /// Only the prompt and the answer are kept; the assistant turns carrying `tool_calls` and the
    /// `tool` results answering them are already gone by the time a [`ConversationTurn`] exists.
    /// That compaction is the cost control, not tidiness — a single script's output can be 256 KiB
    /// and one eight-step message's raw transcript can reach a megabyte or two, so replaying
    /// transcripts is precisely what would make a remembered conversation slow and expensive. The
    /// model is being given the conversation, not the recording of how it was carried out.
    ///
    /// Dropping the intermediate traffic is also what makes trimming safe: the assistant message
    /// holding the `tool_calls` and the results answering it leave together, so neither half can
    /// be left behind to fail the next request.
    pub fn record(&mut self, turn: ConversationTurn) {
        self.turns.push(turn);
        self.trim();
    }

    /// Appends the remembered conversation to a session's message vector.
    pub(super) fn replay_into(&self, messages: &mut Vec<ModelMessage>) {
        for turn in &self.turns {
            turn.replay_into(messages);
        }
    }

    /// Drops whole oldest exchanges until both bounds hold.
    ///
    /// Whole exchanges, always: half a turn is the orphan shape this type exists to prevent, so an
    /// exchange too large to fit on its own leaves an empty window rather than a partial one.
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
