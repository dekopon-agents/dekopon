//! The one Server-Sent Events reader both model transports use.
//!
//! Both backends answer `text/event-stream`: the Codex Responses endpoint always, and an
//! OpenAI-compatible endpoint whenever the request asked for it. They disagree about what the
//! events *mean* and agree completely about how they arrive, so the framing — the byte bound, the
//! multi-line `data:` join, the `[DONE]` sentinel, the final event with no trailing blank line —
//! lives here once instead of twice. The two accumulators stay with the wire formats they parse.
//!
//! Reading is a pull loop rather than a callback: the caller owns the decision to stop, and
//! dropping the reader is what cancellation does — it drops the underlying body, which closes the
//! connection instead of returning it to the pool for a response nobody is reading.

use std::io::{self, BufRead as _, BufReader, Read};

use thiserror::Error;

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

/// Why reading a stream stopped before its caller was finished with it.
#[derive(Debug, Error)]
pub(crate) enum SseError {
    /// The stream exceeded [`MAX_STREAM_BYTES`].
    #[error("response stream exceeded {MAX_STREAM_BYTES} bytes")]
    TooLarge,
    /// The socket failed mid-stream.
    #[error("could not read the response stream")]
    Read {
        /// The underlying failure, kept so the caller can report which read died.
        #[source]
        source: io::Error,
    },
}

/// A bounded reader over one `text/event-stream` body.
pub(crate) struct SseReader<R> {
    reader: BufReader<io::Take<R>>,
    /// One line buffer for the whole stream. A long answer is thousands of one-token `data:`
    /// lines, and a fresh `String` per line is a heap allocation per token.
    line: String,
    /// The payload of the event being assembled.
    data: String,
    bytes_read: u64,
    ended: bool,
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

impl<R: Read> SseReader<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            // One past the bound, so the counter below reports the overrun rather than the reader
            // silently reporting a short, truncated stream at exactly the limit.
            reader: BufReader::new(reader.take(MAX_STREAM_BYTES.saturating_add(1))),
            line: String::new(),
            data: String::new(),
            bytes_read: 0,
            ended: false,
        }
    }

    /// Reads the next event, or `None` once the body ended.
    ///
    /// An event whose `data` is blank — a heartbeat, a comment line, the blank line between two
    /// events — is consumed rather than returned, so a caller's loop sees only payloads.
    pub(crate) fn next_event(&mut self) -> Result<Option<SseEvent<'_>>, SseError> {
        match self.fill()? {
            Filled::Eof => Ok(None),
            Filled::Done => Ok(Some(SseEvent::Done)),
            Filled::Payload => Ok(Some(SseEvent::Data(self.data.trim()))),
        }
    }

    fn fill(&mut self) -> Result<Filled, SseError> {
        loop {
            if self.ended {
                return Ok(Filled::Eof);
            }
            self.data.clear();
            loop {
                self.line.clear();
                let length = self
                    .reader
                    .read_line(&mut self.line)
                    .map_err(|source| SseError::Read { source })?;
                if length == 0 {
                    // A body that ends without its final blank line still delivered the event it
                    // was in the middle of; `ended` makes the next call answer `Eof`.
                    self.ended = true;
                    break;
                }
                self.bytes_read = self.bytes_read.saturating_add(length as u64);
                if self.bytes_read > MAX_STREAM_BYTES {
                    return Err(SseError::TooLarge);
                }
                let line = self.line.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    break;
                }
                if let Some(field) = line.strip_prefix("data:") {
                    if !self.data.is_empty() {
                        self.data.push('\n');
                    }
                    self.data.push_str(field.trim_start());
                }
            }
            let data = self.data.trim();
            if data.is_empty() {
                continue;
            }
            if data == DONE {
                self.ended = true;
                return Ok(Filled::Done);
            }
            return Ok(Filled::Payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_STREAM_BYTES, SseError, SseEvent, SseReader};

    /// Every payload the reader produced, and how the stream finished.
    fn read(body: &str) -> (Vec<String>, Option<SseEvent<'static>>) {
        let mut reader = SseReader::new(body.as_bytes());
        let mut payloads = Vec::new();
        loop {
            let Some(event) = reader.next_event().expect("a well-formed stream") else {
                return (payloads, None);
            };
            match event {
                SseEvent::Data(data) => payloads.push(data.to_owned()),
                SseEvent::Done => return (payloads, Some(SseEvent::Done)),
            }
        }
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

        let mut reader = SseReader::new("data: [DONE]\n\ndata: {}\n\n".as_bytes());
        assert_eq!(
            reader.next_event().expect("done arrives"),
            Some(SseEvent::Done)
        );
        assert_eq!(
            reader.next_event().expect("the stream is over"),
            None,
            "reading past [DONE] must not resume the turn"
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

        let error = SseReader::new(flood.as_bytes())
            .next_event()
            .expect_err("an unbounded stream must be refused");

        assert!(matches!(error, SseError::TooLarge), "{error:?}");
        assert!(
            error.to_string().contains(&MAX_STREAM_BYTES.to_string()),
            "the refusal must name the bound: {error}"
        );
    }
}
