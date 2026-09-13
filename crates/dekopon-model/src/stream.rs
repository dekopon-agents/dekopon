//! What a streaming turn reports while it is still arriving.
//!
//! Two types, and the split between them is the point. [`ModelText`] is the only container in the
//! workspace that model-authored visible text travels in, and only this crate can put bytes into
//! one: a progress surface, a trace field, or a chat message that wants to show model text has to
//! name the type, and nothing else — a prompt, a tool-call argument, a reasoning summary, a tool
//! result — can arrive wearing it. [`TurnEvent`] carries that text plus counters and nothing else.

use std::ops::ControlFlow;

use serde_json::Value;

use crate::model::ModelError;

/// Model-authored visible answer text.
///
/// Constructed from bytes only by this crate's SSE accumulators, from the visible-text deltas of a
/// response. A consumer may concatenate, measure, and shorten what it already holds; it cannot
/// manufacture one from a string it chose. That is the whole reason this is a newtype rather than
/// a `String`: the rule that reasoning, arguments, and tool output never reach a chat surface is
/// enforced by the type rather than by review.
///
/// Unbounded on purpose. The caller of [`ChatModel::complete`](crate::model::ChatModel::complete)
/// owns the bound, because how much text may be shown depends on the surface it is going to —
/// Discord's 2,000 characters, Slack's rather more, an audit record's own limit — and a bound
/// baked in here would be the wrong one everywhere.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelText(String);

impl ModelText {
    /// Wraps text the model produced. The SSE accumulators are the only callers.
    pub(crate) fn from_model(text: String) -> Self {
        Self(text)
    }

    /// Borrows the text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Length in bytes, which is what a byte bound on an outbound message measures.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether any text has arrived yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Appends a delta, which is how a caller keeps the cumulative text of a turn.
    pub fn push(&mut self, delta: &Self) {
        self.0.push_str(&delta.0);
    }

    /// The first `max_chars` characters, or the whole text when it is shorter.
    ///
    /// Characters rather than bytes, and cut on a character boundary: the callers are chat
    /// surfaces with character limits, and slicing UTF-8 by byte count panics on the first
    /// multi-byte character that straddles the cut.
    #[must_use]
    pub fn truncated(&self, max_chars: usize) -> Self {
        match self.0.char_indices().nth(max_chars) {
            Some((end, _)) => Self(self.0[..end].to_owned()),
            None => self.clone(),
        }
    }
}

/// One event from a turn that is still arriving, delivered on the calling thread, in order.
///
/// Deliberately small. Everything a surface needs to say "something is happening" is here, and
/// everything else a response contains — reasoning, tool-call arguments, usage — is not, because
/// a variant is the only way for it to travel.
#[derive(Clone, Debug)]
pub enum TurnEvent {
    /// A fragment of visible answer text. Never reasoning, never tool-call arguments.
    TextDelta(ModelText),
    /// A tool call started arriving; its arguments come with the finished turn.
    ///
    /// `index` is the call's position in the finished turn's `tool_calls`, which is the one
    /// definition both transports can answer: the chat-completions wire numbers its fragments and
    /// the Responses wire does not.
    ToolCallStarted {
        /// Position of this call in the finished turn.
        index: u32,
    },
}

/// Decodes a recorded event-stream body into the events that turn reports while it arrives.
///
/// The only way a crate outside this one obtains [`TurnEvent`]s carrying real [`ModelText`], and
/// deliberately the only way: a fixture replays a recorded transcript through the same parser that
/// reads live responses, so a test double can neither invent visible text nor drift from what a
/// backend actually sends. Both wire formats are accepted — a Responses event names its `type` and
/// a chat-completions chunk does not, which is the one difference that needs no guessing.
///
/// It is `pub` for that reason alone and has no production caller: a running daemon reads a live
/// body through [`ChatModel::complete`](crate::model::ChatModel::complete), never a recorded one.
/// Its callers are this workspace's fixtures — `dekopon-test-support`'s scripted stream model, and
/// the transport, prompt-loop, and progress tests that each need one real `ModelText`. The only
/// alternative is a constructor turning an arbitrary string into model text, which is the hole
/// [`ModelText`] exists to close, so the seam stays here beside the parser that fills it.
///
/// # Errors
///
/// When the body is not a stream the matching parser accepts. For a recorded transcript that means
/// the transcript is wrong, not the code reading it.
pub fn events_from_transcript(body: &str) -> Result<Vec<TurnEvent>, ModelError> {
    let mut events = Vec::new();
    let mut collect = |event: TurnEvent| -> ControlFlow<()> {
        events.push(event);
        ControlFlow::Continue(())
    };
    if is_responses_transcript(body) {
        crate::chatgpt::parse_sse(body.as_bytes(), &mut collect)
            .map_err(|error| ModelError::Response(error.to_string()))?;
    } else {
        crate::model::read_chat_stream(body.as_bytes(), &mut collect)?;
    }
    Ok(events)
}

/// Whether a recorded body is a Responses stream, decided by its first payload.
///
/// A Responses event is `{"type": "response.…"}`; a chat-completions chunk has no top-level
/// `type` at all. A first payload that is not JSON on its own line answers "chat completions", and
/// the real parser then reports what is actually wrong with it.
fn is_responses_transcript(body: &str) -> bool {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .find(|payload| !payload.is_empty() && *payload != "[DONE]")
        .is_some_and(|payload| {
            serde_json::from_str::<Value>(payload)
                .is_ok_and(|event| event.get("type").is_some_and(Value::is_string))
        })
}

#[cfg(test)]
mod tests {
    use super::{ModelText, TurnEvent, events_from_transcript};

    #[test]
    fn cumulative_text_is_the_concatenation_of_its_deltas() {
        let mut cumulative = ModelText::default();
        assert!(cumulative.is_empty());

        for delta in ["Look ", "at ", "that."] {
            cumulative.push(&ModelText::from_model(delta.to_owned()));
        }

        assert_eq!(cumulative.as_str(), "Look at that.");
        assert_eq!(cumulative.len(), 13);
        assert!(!cumulative.is_empty());
    }

    /// Every event a transcript reported, rendered so a test can assert on the sequence.
    fn recorded(events: &[TurnEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                TurnEvent::TextDelta(text) => format!("text:{}", text.as_str()),
                TurnEvent::ToolCallStarted { index } => format!("call:{index}"),
            })
            .collect()
    }

    #[test]
    fn a_recorded_transcript_replays_through_whichever_parser_wrote_it() {
        // Both wire formats reach the same two events, which is what lets one fixture drive a
        // test against either backend. The chat-completions chunk with `content: null` and the
        // Responses reasoning item are both here because both are silent: an event arrives only
        // for text a person could be shown.
        let chat = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Echoed \"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello.\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let responses = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Echoed \"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello.\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
        );

        for (backend, transcript) in [("chat completions", chat), ("responses", responses)] {
            let events = events_from_transcript(transcript)
                .unwrap_or_else(|error| panic!("{backend}: {error}"));

            assert_eq!(
                recorded(&events),
                vec!["text:Echoed ", "text:hello."],
                "{backend}"
            );
        }
    }

    #[test]
    fn a_transcript_that_is_not_a_stream_says_so_rather_than_replaying_nothing() {
        // Failure path, and the one a fixture author actually hits: an edited transcript that no
        // longer parses must name the problem instead of yielding an empty, silently wrong script.
        let error = events_from_transcript("data: {\"choices\": [\n\n")
            .expect_err("a truncated JSON payload is not a stream");

        assert!(
            error.to_string().contains("invalid stream chunk"),
            "the failure must name what could not be read: {error}"
        );
    }

    #[test]
    fn truncation_cuts_on_a_character_boundary_rather_than_a_byte_one() {
        // Four characters, eight bytes: `a`, a two-byte `π`, a four-byte emoji, `b`. A byte
        // slice at 2 lands inside the `π` and panics, and a chat surface bounding a streamed
        // message by its character limit is exactly where that would happen.
        let text = ModelText::from_model("aπ😀b".to_owned());
        assert_eq!(text.len(), 8, "the byte length is not the character count");

        assert_eq!(text.truncated(0).as_str(), "");
        assert_eq!(text.truncated(1).as_str(), "a");
        assert_eq!(text.truncated(2).as_str(), "aπ");
        assert_eq!(
            text.truncated(2).len(),
            3,
            "two characters are three bytes here, so a byte bound would have cut elsewhere"
        );
        assert_eq!(text.truncated(3).as_str(), "aπ😀");
        assert_eq!(text.truncated(4).as_str(), "aπ😀b");
        assert_eq!(
            text.truncated(4_000).as_str(),
            "aπ😀b",
            "a bound larger than the text keeps all of it"
        );
    }
}
