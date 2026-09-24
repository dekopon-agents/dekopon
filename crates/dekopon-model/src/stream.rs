use std::ops::ControlFlow;

use serde_json::Value;

use crate::error::InferenceError;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelText(String);

impl ModelText {
    pub(crate) fn from_model(text: String) -> Self {
        Self(text)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn push(&mut self, delta: &Self) {
        self.0.push_str(&delta.0);
    }

    /// Truncates by character count, not bytes, since slicing UTF-8 at an arbitrary byte offset
    /// panics if it lands inside a multi-byte character.
    #[must_use]
    pub fn truncated(&self, max_chars: usize) -> Self {
        match self.0.char_indices().nth(max_chars) {
            Some((end, _)) => Self(self.0[..end].to_owned()),
            None => self.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum TurnEvent {
    TextDelta(ModelText),
    /// Index is the call's position in the finished turn's tool_calls because that is the only
    /// numbering both the chat-completions and Responses wire formats agree on.
    ToolCallStarted {
        index: u32,
    },
}

pub fn events_from_transcript(body: &str) -> Result<Vec<TurnEvent>, InferenceError> {
    let mut events = Vec::new();
    let mut collect = |event: TurnEvent| -> ControlFlow<()> {
        events.push(event);
        ControlFlow::Continue(())
    };
    if is_responses_transcript(body) {
        crate::codex::replay_transcript(body, &mut collect)?;
    } else {
        crate::openai::replay_transcript(
            crate::diagnostic::DiagnosticSecrets::default(),
            body,
            &mut collect,
        )?;
    }
    Ok(events)
}

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
        let error = events_from_transcript("data: {\"choices\": [\n\n")
            .expect_err("a truncated JSON payload is not a stream");

        assert!(matches!(
            error,
            crate::error::InferenceError::Protocol(crate::error::ProtocolFailure::Decode(source))
                if source.is_eof()
        ));
    }

    #[test]
    fn truncation_cuts_on_a_character_boundary_rather_than_a_byte_one() {
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
