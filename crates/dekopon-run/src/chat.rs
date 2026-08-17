//! A line-oriented client for `dekopond`'s local development transport.
//!
//! # What this is, and what it is not
//!
//! This is a socket client and nothing else. It loads no component, links no host imports, runs no
//! model, and executes no tool loop: it writes one JSON line to a Unix socket and prints the line
//! that comes back. Every part of a session that matters — routing, attestation, authorization, the
//! model call — happens inside the already-running gateway, on exactly the path a Slack message
//! takes. That is why `chat` can exist beside the direct provider path without touching it: it
//! holds no provider authority to hold.
//!
//! # Trust
//!
//! The gateway's local transport trusts its caller to declare a subject, which is why it is a
//! development transport rather than a production one. That grants nothing here. The subject this
//! client sends is only a claim; the broker still needs an attestor grant covering its namespace
//! and an owner-controlled mapping before it resolves to a principal, so a session reaches exactly
//! the authority the owner already configured for the subject it names. The socket's `0600` mode is
//! what keeps it reachable only by the owner's UID.
//!
//! # One message in flight
//!
//! The local protocol carries no correlation identifier, so a reply is matched to its request by
//! ordering alone. This client therefore never pipelines: it sends one line, waits for exactly one
//! reply, prints it, and only then reads the next line of input.

use std::{
    io::{self, BufRead as _, BufReader, Read as _, Write as _},
    path::PathBuf,
};

use dekopon_agent::IdSequence;
use dekopon_core::{ExternalSubject, IdentifierError};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Longest line the gateway's local transport reads, including its terminating newline.
///
/// Mirrored from the transport rather than imported: this crate must not depend on the gateway.
/// A request over this bound is refused here, because the gateway's own reaction to one is to
/// close the connection without a diagnostic, which would surface as an unexplained hang-up.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// The prefix minted conversation identifiers carry, so an operator can recognize one on sight.
const CONVERSATION_PREFIX: &str = "chat";

/// One request line, in exactly the shape the transport deserializes.
///
/// `channel` is always sent explicitly even though the transport defaults it: the conversation
/// identity is the whole point of a session, and a defaulted field would silently merge every
/// session on the host into one.
#[derive(Debug, Serialize)]
struct LocalRequest<'a> {
    subject: &'a ExternalSubject,
    channel: &'a str,
    text: &'a str,
}

/// One reply line.
///
/// Strict on unknown fields, matching the transport's own strictness about requests: the gateway
/// writes exactly this object, so anything else is a sign the socket is not the one we think.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalReply {
    reply: String,
}

/// Everything one interactive gateway session needs.
pub(crate) struct ChatSession {
    /// The gateway's owner-only local-transport socket.
    gateway: PathBuf,
    /// The canonical external subject every request in this session claims.
    subject: ExternalSubject,
    /// The conversation identity every request carries as its `channel`.
    conversation: String,
    /// Whether the identifier was minted here, and so has to be announced to be resumable.
    announce: bool,
}

impl ChatSession {
    /// Resolves the conversation identity a session will use for its whole lifetime.
    ///
    /// An absent `--conversation` is minted rather than derived from the process: a PID recycles
    /// and every invocation is a new process, so an identity that has to survive a restart to be
    /// resumable cannot come from one.
    pub(crate) fn new(
        gateway: PathBuf,
        subject: ExternalSubject,
        conversation: Option<String>,
    ) -> Result<Self, ChatError> {
        let (conversation, announce) = match conversation {
            Some(conversation) => (conversation, false),
            None => (mint_conversation()?, true),
        };
        Ok(Self {
            gateway,
            subject,
            conversation,
            announce,
        })
    }
}

/// Derives a fresh conversation identifier.
///
/// Reuses the session identifier space `dekopon-agent` already derives for broker traces, for the
/// same reason it exists there: a randomly seeded hash over the process identifier and a
/// nanosecond wall-clock reading collides far less readily than either input alone.
fn mint_conversation() -> Result<String, ChatError> {
    IdSequence::new(CONVERSATION_PREFIX)
        .map(|sequence| sequence.trace().to_string())
        .map_err(ChatError::Conversation)
}

/// Runs one session: a line of input becomes a request, and its reply is printed.
///
/// Returns when standard input ends. Blocking throughout, because every step of it is: a reply
/// arrives only after the gateway has finished a whole agent session.
pub(crate) fn run(session: &ChatSession) -> Result<(), ChatError> {
    let stream = std::os::unix::net::UnixStream::connect(&session.gateway).map_err(|source| {
        ChatError::Connect {
            path: session.gateway.clone(),
            source,
        }
    })?;
    if session.announce {
        // Standard error, not standard output: standard output is the reply stream, and a piped
        // consumer must not have to skip a header to read it.
        writeln!(io::stderr(), "conversation: {}", session.conversation)
            .map_err(ChatError::Announce)?;
    }

    let mut requests = &stream;
    let mut replies = BufReader::new(&stream).take(MAX_LINE_BYTES as u64);
    let mut input = io::stdin().lock().take(MAX_LINE_BYTES as u64);
    let mut output = io::stdout().lock();

    loop {
        let line = match read_line(&mut input).map_err(ChatError::ReadInput)? {
            Line::Ended => return Ok(()),
            Line::TooLong => {
                return Err(ChatError::MessageTooLarge {
                    maximum: MAX_LINE_BYTES,
                });
            }
            Line::Ready(line) => line,
        };
        let line = String::from_utf8(line).map_err(ChatError::InputUtf8)?;
        let text = line.trim_end_matches(['\r', '\n']);
        // A blank line asks nothing. Sending it would spend a whole gateway session on empty text
        // and, worse, leave this client waiting for a reply the transport may never produce.
        if text.trim().is_empty() {
            continue;
        }

        let request = serde_json::to_string(&LocalRequest {
            subject: &session.subject,
            channel: &session.conversation,
            text,
        })
        .map_err(ChatError::Serialize)?;
        // The newline the gateway delimits by counts against its bound.
        if request.len() >= MAX_LINE_BYTES {
            return Err(ChatError::MessageTooLarge {
                maximum: MAX_LINE_BYTES,
            });
        }
        send(&mut requests, &request)?;

        let reply = match read_line(&mut replies) {
            Ok(Line::Ended) => return Err(ChatError::GatewayClosed),
            Ok(Line::TooLong) => {
                return Err(ChatError::ReplyTooLarge {
                    maximum: MAX_LINE_BYTES,
                });
            }
            Ok(Line::Ready(line)) => line,
            Err(error) if hung_up(&error) => return Err(ChatError::GatewayClosed),
            Err(error) => return Err(ChatError::ReadReply(error)),
        };
        let reply =
            serde_json::from_slice::<LocalReply>(&reply).map_err(ChatError::MalformedReply)?;

        match print(&mut output, &reply.reply) {
            Ok(()) => {}
            // A consumer that stopped reading ends the session; it is not a failure of it.
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            Err(error) => return Err(ChatError::Print(error)),
        }
    }
}

/// Writes one request line, reporting a peer that has already gone as a hang-up.
///
/// A gateway that closed mid-session is detected by whichever operation reaches it first: the write
/// that fails with `EPIPE` or the read that sees end of stream. Both mean the same thing, so both
/// say the same thing.
fn send(writer: &mut impl io::Write, request: &str) -> Result<(), ChatError> {
    match writer
        .write_all(request.as_bytes())
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush())
    {
        Ok(()) => Ok(()),
        Err(error) if hung_up(&error) => Err(ChatError::GatewayClosed),
        Err(error) => Err(ChatError::Send(error)),
    }
}

/// Prints one reply, adding the newline only when the text does not already end in one.
fn print(output: &mut impl io::Write, reply: &str) -> io::Result<()> {
    output.write_all(reply.as_bytes())?;
    if !reply.ends_with('\n') {
        output.write_all(b"\n")?;
    }
    output.flush()
}

fn hung_up(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
    )
}

/// What one bounded line read produced.
enum Line {
    /// A complete line, or a final unterminated one at end of input.
    Ready(Vec<u8>),
    /// End of input.
    Ended,
    /// A line the bound cut short, so it can never be sent or understood whole.
    TooLong,
}

/// Reads one `\n`-delimited line without allocating past the transport's bound.
///
/// The limit is re-armed per line rather than per stream, exactly as the gateway arms its own: a
/// ceiling has to bound the buffer before it is allocated, and a whole-stream ceiling would end a
/// long session after enough short lines.
fn read_line<R: io::BufRead>(reader: &mut io::Take<R>) -> io::Result<Line> {
    reader.set_limit(MAX_LINE_BYTES as u64);
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line)? == 0 {
        return Ok(Line::Ended);
    }
    // An unterminated line that filled the whole budget was cut short rather than ended. A shorter
    // unterminated one is the last line before end of input, which is a perfectly good message.
    if !line.ends_with(b"\n") && line.len() >= MAX_LINE_BYTES {
        return Ok(Line::TooLong);
    }
    Ok(Line::Ready(line))
}

/// Every way one gateway session can fail.
#[derive(Debug, Error)]
pub(crate) enum ChatError {
    #[error("could not derive a conversation identifier")]
    Conversation(#[source] IdentifierError),
    #[error("could not connect to the gateway socket {}", path.display())]
    Connect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not announce the conversation identifier")]
    Announce(#[source] io::Error),
    #[error("could not read from standard input")]
    ReadInput(#[source] io::Error),
    #[error("standard input is not UTF-8")]
    InputUtf8(#[source] std::string::FromUtf8Error),
    #[error(
        "the message does not fit the gateway's {maximum}-byte line, \
         counting the JSON envelope and the newline it reads by"
    )]
    MessageTooLarge { maximum: usize },
    #[error("could not encode the chat request")]
    Serialize(#[source] serde_json::Error),
    #[error("could not send the chat request")]
    Send(#[source] io::Error),
    #[error("the gateway closed the connection")]
    GatewayClosed,
    #[error("could not read the gateway's reply")]
    ReadReply(#[source] io::Error),
    #[error("the gateway sent a line longer than {maximum} bytes")]
    ReplyTooLarge { maximum: usize },
    #[error("the gateway sent a line that is not a reply")]
    MalformedReply(#[source] serde_json::Error),
    #[error("could not write a reply to standard output")]
    Print(#[source] io::Error),
    #[error("the chat session did not run to completion")]
    Task(#[source] tokio::task::JoinError),
}

impl ChatError {
    /// Stable, low-cardinality failure category for telemetry.
    ///
    /// Kept distinct per failure the way [`crate::AppError`] does, and for the same reason: the
    /// messages carry socket paths and transport diagnostics that stay out of exported telemetry,
    /// so the category is all an operator has to correlate with stderr.
    pub(crate) fn telemetry_kind(&self) -> &'static str {
        match self {
            Self::Conversation(_) => "chat-conversation",
            Self::Connect { .. } => "chat-connect",
            Self::Announce(_) | Self::Print(_) => "chat-output",
            Self::ReadInput(_) => "chat-input-read",
            Self::InputUtf8(_) => "chat-input-utf8",
            Self::MessageTooLarge { .. } => "chat-message-too-large",
            Self::Serialize(_) => "chat-serialize",
            Self::Send(_) => "chat-send",
            Self::GatewayClosed => "chat-gateway-closed",
            Self::ReadReply(_) => "chat-reply-read",
            Self::ReplyTooLarge { .. } => "chat-reply-too-large",
            Self::MalformedReply(_) => "chat-malformed-reply",
            Self::Task(_) => "chat-task",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde_json::json;

    use super::{ChatSession, Line, LocalReply, LocalRequest, MAX_LINE_BYTES, read_line};

    fn reader(bytes: &[u8]) -> std::io::Take<Cursor<Vec<u8>>> {
        std::io::Read::take(Cursor::new(bytes.to_vec()), MAX_LINE_BYTES as u64)
    }

    #[test]
    fn reads_lines_until_input_ends() {
        let mut reader = reader(b"first\nsecond");
        assert!(matches!(read_line(&mut reader), Ok(Line::Ready(line)) if line == b"first\n"));
        assert!(matches!(read_line(&mut reader), Ok(Line::Ready(line)) if line == b"second"));
        assert!(matches!(read_line(&mut reader), Ok(Line::Ended)));
    }

    #[test]
    fn refuses_a_line_the_bound_cut_short() {
        let mut oversize = vec![b'x'; MAX_LINE_BYTES + 16];
        oversize.push(b'\n');
        assert!(matches!(
            read_line(&mut reader(&oversize)),
            Ok(Line::TooLong)
        ));
    }

    #[test]
    fn sends_the_conversation_as_the_transport_channel() {
        let session = ChatSession::new(
            "/run/dekopon/dev.sock".into(),
            "tel.16034700182".parse().expect("valid subject fixture"),
            Some("morning-standup".to_owned()),
        )
        .expect("explicit conversation identifier");
        let request = serde_json::to_value(LocalRequest {
            subject: &session.subject,
            channel: &session.conversation,
            text: "what changed today?",
        })
        .expect("request serializes");

        assert_eq!(
            request,
            json!({
                "subject": "tel.16034700182",
                "channel": "morning-standup",
                "text": "what changed today?",
            })
        );
    }

    #[test]
    fn mints_an_announceable_conversation_when_none_is_given() {
        let session = ChatSession::new(
            "/run/dekopon/dev.sock".into(),
            "tel.16034700182".parse().expect("valid subject fixture"),
            None,
        )
        .expect("minted conversation identifier");

        assert!(session.announce);
        assert!(
            session.conversation.starts_with("chat-"),
            "{}",
            session.conversation
        );
        let other = ChatSession::new(
            "/run/dekopon/dev.sock".into(),
            "tel.16034700182".parse().expect("valid subject fixture"),
            None,
        )
        .expect("minted conversation identifier");
        assert_ne!(session.conversation, other.conversation);
    }

    #[test]
    fn rejects_a_reply_line_that_is_not_a_reply() {
        for line in [
            "not json at all",
            r#"{"unexpected":"shape"}"#,
            r#"{"reply":"ok","extra":1}"#,
        ] {
            assert!(serde_json::from_str::<LocalReply>(line).is_err(), "{line}");
        }
        let reply = serde_json::from_str::<LocalReply>(r#"{"reply":"ok"}"#)
            .expect("a well-formed reply parses");
        assert_eq!(reply.reply, "ok");
    }
}
