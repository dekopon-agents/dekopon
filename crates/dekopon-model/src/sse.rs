//! Shared chunk-safe Server-Sent Events framing for Codex and chat-completions consumers.
//!
//! All async adapters feed chunks through [`read_async_stream`]; offline transcript replay
//! uses [`decode_transcript`] over the same [`SseFramer`]. All paths enforce [`MAX_STREAM_BYTES`],
//! join multi-line `data:`, recognize `[DONE]` and deliver a final event without a blank line.
//! Consumers interpret events and control early termination through callbacks. Cancellation drops
//! the response body, including when a socket produces no events.

/// Bound on how much of one streaming response is read.
///
/// The larger of the two bounds these transports used separately (the Codex path's 16 MiB `take`
/// and the chat-completions path's 10 MiB `read_json` default), because a bound that is correct
/// for one wire format and not the other would be a per-backend limit again. A response this size
/// is already far past anything a chat surface can show; the point is that a socket that never
/// stops talking cannot grow the process.
pub(crate) const MAX_STREAM_BYTES: u64 = 16 * 1024 * 1024;

/// The sentinel both wire formats end a turn with.
const DONE: &str = "[DONE]";

/// One event read off the stream.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum SseEvent<'a> {
    /// The event's `data` payload, with multi-line `data:` fields joined by newlines.
    Data(&'a str),
    /// `data: [DONE]`: the server said the turn is over. Nothing after it is read.
    Done,
}

/// What one round of reading produced, without borrowing the buffer it produced it into.
enum Filled {
    /// `data` holds one event's payload.
    Payload,
    /// The `[DONE]` sentinel arrived.
    Done,
    /// The body ended.
    Eof,
}

/// Chunk framing shared by async HTTP and in-memory transcript replay.
#[derive(Default)]
pub(crate) struct SseFramer {
    buffer: Vec<u8>,
    cursor: usize,
    scanned: usize,
    data: String,
    bytes: u64,
    eof: bool,
    ended: bool,
}

impl SseFramer {
    fn push(&mut self, chunk: &[u8]) -> Result<(), crate::error::InferenceError> {
        self.bytes = self.bytes.saturating_add(chunk.len() as u64);
        tracing::Span::current().record("response.bytes", self.bytes);
        if self.bytes > MAX_STREAM_BYTES {
            return Err(crate::error::ProtocolFailure::StreamTooLarge.into());
        }
        if self.cursor > self.buffer.len() / 2 {
            self.buffer.drain(..self.cursor);
            self.scanned -= self.cursor;
            self.cursor = 0;
        }
        self.buffer.extend_from_slice(chunk);
        Ok(())
    }

    fn fill(&mut self) -> Result<Option<Filled>, crate::error::InferenceError> {
        loop {
            if self.ended {
                return Ok(Some(Filled::Eof));
            }
            let end = match self.buffer[self.scanned..]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                Some(index) => self.scanned + index + 1,
                None if self.eof => self.buffer.len(),
                None => {
                    self.scanned = self.buffer.len();
                    return Ok(None);
                }
            };
            let line = std::str::from_utf8(&self.buffer[self.cursor..end])
                .map_err(crate::error::ProtocolFailure::Utf8)?
                .trim_end_matches(['\r', '\n']);
            if end == self.cursor {
                self.ended = true;
            }
            self.cursor = end;
            self.scanned = end;
            if line.is_empty() {
                if self.data.trim().is_empty() {
                    self.data.clear();
                    continue;
                }
                if self.data.trim() == DONE {
                    self.ended = true;
                    return Ok(Some(Filled::Done));
                }
                return Ok(Some(Filled::Payload));
            }
            if let Some(field) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(field.trim_start());
            }
        }
    }
}

pub(crate) async fn read_async_stream<S, B>(
    stream: S,
    control: &crate::control::TurnControl,
    on_event: &mut (
             impl FnMut(SseEvent<'_>) -> Result<std::ops::ControlFlow<()>, crate::error::InferenceError>
             + Send
         ),
) -> Result<(), crate::error::InferenceError>
where
    S: futures_util::Stream<Item = Result<B, reqwest::Error>> + Send,
    B: AsRef<[u8]> + Send,
{
    use futures_util::StreamExt as _;
    futures_util::pin_mut!(stream);
    let mut framer = SseFramer::default();
    loop {
        match framer.fill()? {
            Some(Filled::Eof) => return Ok(()),
            Some(Filled::Done) => {
                let _flow = on_event(SseEvent::Done)?;
                return Ok(());
            }
            Some(Filled::Payload) => {
                if on_event(SseEvent::Data(framer.data.trim()))?.is_break() {
                    return Ok(());
                }
                framer.data.clear();
            }
            None => match control.run(stream.next()).await? {
                Some(Ok(chunk)) => framer.push(chunk.as_ref())?,
                Some(Err(source)) => {
                    return Err(crate::http::http_failure(
                        crate::error::FailurePhase::ReadingBody,
                        Some(200),
                        source,
                    ));
                }
                None => framer.eof = true,
            },
        }
    }
}

pub(crate) fn decode_transcript(
    body: &str,
    on_event: &mut impl FnMut(
        SseEvent<'_>,
    ) -> Result<std::ops::ControlFlow<()>, crate::error::InferenceError>,
) -> Result<(), crate::error::InferenceError> {
    let mut framer = SseFramer::default();
    framer.push(body.as_bytes())?;
    framer.eof = true;
    while let Some(filled) = framer.fill()? {
        match filled {
            Filled::Eof => break,
            Filled::Done => {
                let _flow = on_event(SseEvent::Done)?;
                break;
            }
            Filled::Payload => {
                if on_event(SseEvent::Data(framer.data.trim()))?.is_break() {
                    break;
                }
                framer.data.clear();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MAX_STREAM_BYTES, SseEvent};

    async fn read_sized_stream(bytes: usize) -> Result<(), crate::error::InferenceError> {
        let suffix = "\ndata: {}\n\n";
        let body = format!(":{}{}", "x".repeat(bytes - 1 - suffix.len()), suffix);
        let control = crate::control::TurnControl::new(
            tokio::sync::watch::channel(false).1,
            std::time::Duration::from_secs(2),
        )
        .unwrap();
        let stream = futures_util::stream::iter([Ok::<_, reqwest::Error>(body.into_bytes())]);
        let mut payloads = 0;
        super::read_async_stream(stream, &control, &mut |event| {
            if let SseEvent::Data(data) = event {
                assert_eq!(data, "{}");
                payloads += 1;
            }
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .await?;
        assert_eq!(payloads, 1);
        Ok(())
    }

    #[tokio::test]
    async fn an_async_stream_one_byte_over_the_ceiling_is_refused() {
        assert!(matches!(
            read_sized_stream(usize::try_from(MAX_STREAM_BYTES).unwrap() + 1).await,
            Err(crate::error::InferenceError::Protocol(
                crate::error::ProtocolFailure::StreamTooLarge
            ))
        ));
    }

    #[tokio::test]
    async fn asynchronous_and_transcript_framing_agree_at_every_byte_boundary() {
        let body = ": ignored\r\nevent: message\r\ndata: {\r\ndata: \"text\":\"🍊\"}\r\n\r\ndata: [DONE]\n\ndata: ignored\n\n";
        let mut expected = Vec::new();
        super::decode_transcript(body, &mut |event| {
            expected.push(match event {
                SseEvent::Data(data) => data.to_owned(),
                SseEvent::Done => "DONE".to_owned(),
            });
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .unwrap();
        let mut actual = Vec::new();
        let control = crate::control::TurnControl::new(
            tokio::sync::watch::channel(false).1,
            std::time::Duration::from_secs(2),
        )
        .unwrap();
        super::read_async_stream(
            futures_util::stream::iter(body.as_bytes().chunks(1).map(Ok::<_, reqwest::Error>)),
            &control,
            &mut |event| {
                actual.push(match event {
                    SseEvent::Data(data) => data.to_owned(),
                    SseEvent::Done => "DONE".to_owned(),
                });
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .await
        .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(actual, ["{\n\"text\":\"🍊\"}", "DONE"]);
    }

    #[tokio::test]
    async fn a_long_data_line_split_across_small_chunks_is_framed_once() {
        let payload = "x".repeat(4 * 1024 * 1024);
        let body = format!("data: {payload}\n\n");
        let control = crate::control::TurnControl::new(
            tokio::sync::watch::channel(false).1,
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        let mut received = 0;
        super::read_async_stream(
            futures_util::stream::iter(
                body.as_bytes()
                    .chunks(16 * 1024)
                    .map(Ok::<_, reqwest::Error>),
            ),
            &control,
            &mut |event| {
                if let SseEvent::Data(data) = event {
                    assert_eq!(data, payload);
                    received += 1;
                }
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .await
        .unwrap();
        assert_eq!(received, 1);
    }

    /// Every payload the reader produced, and how the stream finished.
    fn read(body: &str) -> (Vec<String>, Option<SseEvent<'static>>) {
        let mut payloads = Vec::new();
        let mut finish = None;
        super::decode_transcript(body, &mut |event| {
            match event {
                SseEvent::Data(data) => payloads.push(data.to_owned()),
                SseEvent::Done => finish = Some(SseEvent::Done),
            }
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .expect("a well-formed stream");
        (payloads, finish)
    }

    #[test]
    fn multi_line_data_fields_are_joined_with_newlines() {
        // The SSE framing rule, and not a theoretical one: a chunk large enough to be split across
        // two `data:` lines is still one JSON document, and joining the halves with anything but a
        // newline — or not joining them at all — turns it into two malformed ones.
        let (payloads, finish) = read(concat!(
            "data: {\"first\": 1,\n",
            "data: \"second\": 2}\n",
            "\n",
            "data: [DONE]\n\n",
        ));

        assert_eq!(payloads, vec!["{\"first\": 1,\n\"second\": 2}"]);
        assert_eq!(finish, Some(SseEvent::Done));
    }

    #[test]
    fn heartbeats_comments_and_event_names_are_not_payloads() {
        let (payloads, finish) = read(concat!(
            ": keep-alive\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"hi\"}\n\n",
            "data:\n\n",
            "\n",
            "data: {\"delta\":\"!\"}\n\n",
        ));

        assert_eq!(payloads, vec!["{\"delta\":\"hi\"}", "{\"delta\":\"!\"}"]);
        assert_eq!(finish, None, "the body ended without a [DONE]");
    }

    #[test]
    fn a_final_event_without_its_blank_line_still_arrives() {
        // Half the recorded transcripts in this crate end this way, and so does a server that
        // flushes its last chunk and closes.
        let (payloads, _) = read("data: {\"delta\":\"hi\"}\r\ndata: {\"delta\":\"!\"}");

        assert_eq!(payloads, vec!["{\"delta\":\"hi\"}\n{\"delta\":\"!\"}"]);
    }

    #[test]
    fn nothing_after_done_is_read() {
        let (payloads, finish) = read(concat!(
            "data: {\"delta\":\"hi\"}\n\n",
            "data: [DONE]\n\n",
            "data: {\"delta\":\" and more\"}\n\n",
        ));

        assert_eq!(payloads, vec!["{\"delta\":\"hi\"}"]);
        assert_eq!(finish, Some(SseEvent::Done));

        assert_eq!(
            read("data: [DONE]\n\ndata: {}\n\n"),
            (Vec::new(), Some(SseEvent::Done))
        );
    }

    #[test]
    fn a_stream_that_never_stops_is_refused_by_byte_count() {
        // A peer-claimed length is a limit to enforce. The line is well formed and the payload is
        // never returned, so the failure names the bound rather than surfacing as a parse error on
        // a truncated document.
        let flood = format!(
            "data: {}\n\n",
            "x".repeat(usize::try_from(MAX_STREAM_BYTES).expect("bound fits this platform") + 1)
        );

        let error =
            super::decode_transcript(&flood, &mut |_| Ok(std::ops::ControlFlow::Continue(())))
                .expect_err("an unbounded stream must be refused");

        assert!(
            matches!(
                error,
                crate::error::InferenceError::Protocol(
                    crate::error::ProtocolFailure::StreamTooLarge
                )
            ),
            "{error:?}"
        );
        assert!(
            error.to_string().contains(&MAX_STREAM_BYTES.to_string()),
            "the refusal must name the bound: {error}"
        );
    }
}
