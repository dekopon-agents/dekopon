//! The chat-service boundary: waiting for a message, and answering one.
//!
//! Everything a transport produces is untrusted except the subject, and the subject is trusted only
//! in the narrow sense that the *service* authenticated it — it is canonical routing metadata that
//! the broker alone maps to a principal. Message text is untrusted end to end and is bounded before
//! it reaches a model.

use std::sync::Arc;

use dekopon_core::ExternalSubject;

use crate::asset::{AssetSourceRef, PendingAsset};
use futures_util::future::BoxFuture;
use thiserror::Error;

pub(crate) mod discord;
pub(crate) mod local;
pub(crate) mod slack;
pub(crate) mod telegram;

/// Inbound chat text is bounded before prompting, because a chat service's own message ceiling is
/// not a bound this daemon chose.
pub(crate) const MAX_INBOUND_TEXT_BYTES: usize = 16 * 1024;
/// Outbound answers are bounded because a model writes them and chat services reject or silently
/// mangle oversized posts.
pub(crate) const MAX_OUTBOUND_TEXT_BYTES: usize = 8 * 1024;

/// One authenticated inbound chat message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InboundMessage {
    /// The configured transport name this arrived on.
    pub transport: String,
    /// The sender, taken from the authenticated transport payload and nowhere else.
    pub subject: ExternalSubject,
    /// Service-native conversation identifier.
    pub channel: String,
    /// Service-native thread identifier, when the conversation has threads.
    pub thread: Option<String>,
    /// Stable identity of the conversation this message belongs to, unique within its transport.
    ///
    /// Deliberately not `(channel, thread)`. On Slack a message that *starts* a thread carries no
    /// `thread_ts`, while the bot's answer to it opens a thread rooted at that message — so every
    /// later turn does carry one. Anything keyed on [`Self::thread`] therefore files the opening
    /// question under a different key than the replies inside the thread it started, orphaning the
    /// first turn of every threaded conversation. This field is the thread the *answer* joins, which
    /// is the same value for all of them.
    ///
    /// Each transport derives it, because only a transport holds the service-native pieces it takes
    /// — Slack's per-message `ts` is one of them, and it is gone by the time a message is routed.
    ///
    /// This is not the admission key. Admission serializes a conversation against itself on
    /// `(transport, channel, thread)` and is unchanged; this identity exists for per-conversation
    /// state that has to survive across turns.
    pub conversation_id: String,
    /// Service-native message identifier, used only to reject redeliveries.
    pub message_id: String,
    /// Untrusted message text, already bounded to [`MAX_INBOUND_TEXT_BYTES`].
    ///
    /// The sender's own words only. The reference lines naming attachments are appended by the
    /// session, because the numbers in them are assigned by [`crate::asset::AssetStore`] and a
    /// transport that minted its own would collide with the one beside it.
    pub text: String,
    /// What the sender attached, described but not yet numbered or fetched.
    pub assets: Vec<PendingAsset>,
    /// Whether this is a one-to-one conversation or a shared channel.
    pub conversation: ConversationKind,
    /// Whether authenticated structured transport metadata says the bot was addressed.
    ///
    /// Discord supplies `Some` from its `mentions` array, including `Some(false)` so presentation
    /// text cannot override the authenticated structure. Other transports use `None` and the
    /// routing loop applies their identifier/handle syntax through
    /// [`TransportIdentity::is_addressed`]. Direct messages ignore this field.
    pub addressed: Option<bool>,
    /// Whatever the transport needs to answer this message.
    pub reply: ReplyTarget,
}

/// Whether a message arrived in a private conversation or a shared one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConversationKind {
    /// A one-to-one conversation; every message is addressed to the bot.
    DirectMessage,
    /// A shared channel, where an unaddressed message is ambient traffic.
    Channel(String),
}

/// Everything a transport needs to answer one message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReplyTarget {
    Slack {
        channel: String,
        thread_ts: Option<String>,
    },
    Discord {
        channel_id: String,
        reply_to: Option<String>,
    },
    Telegram {
        chat_id: i64,
        reply_to: Option<i64>,
    },
    /// The development transport answers on the connection the request arrived on.
    Local {
        connection: u64,
    },
}

/// Who the bot is on one service, resolved at connect time for self-filtering and @-mentions.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TransportIdentity {
    /// Service-native user identifier of the bot itself, when the service has one.
    pub user_id: Option<String>,
    /// Service-native handle (`@name`) of the bot itself, when the service has one.
    pub handle: Option<String>,
}

impl TransportIdentity {
    /// Whether a message addresses the bot by identifier or handle.
    ///
    /// Deliberately one fallback implementation for every service. Slack renders a mention as
    /// `<@U0123ABC>`, Discord can render `<@123>` or the legacy nickname form `<@!123>`, and
    /// Telegram uses `@botname`. Keeping those forms in one place stops a channel route from firing
    /// on ambient traffic on only one transport. Discord normally uses its structured mention bit
    /// instead.
    pub fn is_addressed(&self, text: &str) -> bool {
        if let Some(user_id) = &self.user_id
            && (text.contains(&format!("<@{user_id}>")) || text.contains(&format!("<@!{user_id}>")))
        {
            return true;
        }
        if let Some(handle) = &self.handle {
            let mention = format!("@{handle}");
            if text
                .to_ascii_lowercase()
                .contains(&mention.to_ascii_lowercase())
            {
                return true;
            }
        }
        false
    }
}

/// One chat service this daemon waits on.
///
/// `next` is driven from one dedicated task per transport rather than from a `select!`, so a
/// transport may keep partially consumed protocol state across calls: the daemon never drops the
/// future. Shutdown aborts the task, which is why a transport must hold nothing that must be
/// flushed to be correct — an acknowledgment is sent before the work it acknowledges begins.
pub(crate) trait ChatTransport: Send {
    /// The configured transport name routes refer to.
    fn name(&self) -> &str;

    /// Authenticates, resolves the bot's own identity, and opens the wakeup path.
    fn connect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>>;

    /// Waits for the next routable message, reconnecting internally as needed.
    fn next(&mut self) -> BoxFuture<'_, Result<InboundMessage, TransportError>>;

    /// A cheaply cloned handle sessions use to answer.
    ///
    /// Replying is separated from the transport itself because a session answers minutes after the
    /// message arrived, while `next` has long since gone back to waiting. Handing sessions a shared
    /// handle is what lets both happen at once without a lock across the wait.
    fn replier(&self) -> Arc<dyn ChatReplier>;

    /// How this transport turns an attachment reference back into bytes, when it can.
    ///
    /// Defaulted to `None` so a transport that never carries attachments says nothing about them.
    fn asset_fetcher(&self) -> Option<Arc<dyn AssetFetcher>> {
        None
    }
}

/// The answering half of a transport, shared by every in-flight session on it.
pub(crate) trait ChatReplier: Send + Sync {
    fn reply(&self, target: ReplyTarget, text: String)
    -> BoxFuture<'_, Result<(), TransportError>>;
}

/// Resolves an attachment reference back into the bytes a model can look at.
///
/// Separate from [`ChatReplier`] because the two are used at different moments by different code:
/// a reply happens once at the end of a session, while a fetch happens mid-loop only if the model
/// decides the answer depends on the file. A transport that carries no attachments implements
/// neither and answers `None` from [`ChatTransport::asset_fetcher`].
///
/// `max_bytes` is enforced by the implementation rather than the caller, because the point is to
/// stop reading a response that is too large rather than to discover afterwards that it was.
pub(crate) trait AssetFetcher: Send + Sync {
    fn fetch(
        &self,
        source: &AssetSourceRef,
        max_bytes: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, TransportError>>;
}

/// Transport-level failure.
///
/// Variants carry service-supplied text only where that text is a documented API error code; none
/// of them carries a credential, and the daemon logs the category rather than the message.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("transport credential environment variable {name} is not set")]
    MissingCredential { name: String },
    #[error("transport credential environment variable {name} is not UTF-8")]
    NonUtf8Credential { name: String },
    #[error("chat service request failed")]
    Request(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("chat service returned an error: {code}")]
    Service { code: String },
    #[error("chat service response was not the expected shape")]
    Response,
    #[error("chat socket closed")]
    Closed,
    #[error("transport input/output failed")]
    Io(#[source] std::io::Error),
    #[error("transport socket path is not private, owner-owned, and single-link: {path}")]
    InsecureSocket { path: String },
    #[error("subject could not be represented canonically")]
    Subject(#[source] dekopon_core::SubjectError),
}

impl TransportError {
    /// Stable low-cardinality category for telemetry, never the underlying message.
    pub const fn category(&self) -> &'static str {
        match self {
            Self::MissingCredential { .. } => "missing-credential",
            Self::NonUtf8Credential { .. } => "non-utf8-credential",
            Self::Request(_) => "request",
            Self::Service { .. } => "service",
            Self::Response => "response",
            Self::Closed => "closed",
            Self::Io(_) => "io",
            Self::InsecureSocket { .. } => "insecure-socket",
            Self::Subject(_) => "subject",
        }
    }
}

/// Reads one credential by variable name, reporting the *name* and never the value.
pub(crate) fn read_credential(name: &str) -> Result<String, TransportError> {
    let value = std::env::var_os(name).ok_or_else(|| TransportError::MissingCredential {
        name: name.to_owned(),
    })?;
    value
        .into_string()
        .map_err(|_| TransportError::NonUtf8Credential {
            name: name.to_owned(),
        })
}

/// Bounds untrusted inbound text, keeping the head and saying so.
///
/// The head rather than the tail: a chat message states its request first and elaborates
/// afterwards, so truncating the end loses the least. The marker is inside the text a model sees
/// because a silently shortened prompt is worse than a visibly shortened one.
pub(crate) fn bound_inbound(text: &str) -> String {
    if text.len() <= MAX_INBOUND_TEXT_BYTES {
        return text.to_owned();
    }
    let head = floor_boundary(text, MAX_INBOUND_TEXT_BYTES);
    format!("{}\n[message truncated by the gateway]", &text[..head])
}

/// Bounds a model-authored answer, keeping both ends.
///
/// Head and tail rather than head alone: an answer's conclusion is usually its last line, and
/// dropping it would leave a reader with the reasoning and none of the result.
pub(crate) fn bound_outbound(text: &str) -> String {
    if text.len() <= MAX_OUTBOUND_TEXT_BYTES {
        return text.to_owned();
    }
    const MARKER: &str = "\n\n[...truncated by the gateway...]\n\n";
    let budget = MAX_OUTBOUND_TEXT_BYTES.saturating_sub(MARKER.len());
    let head = floor_boundary(text, budget / 2);
    let tail = ceil_boundary(text, text.len() - (budget - budget / 2));
    format!("{}{MARKER}{}", &text[..head], &text[tail..])
}

/// Largest character boundary at or below `index`.
pub(crate) fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Smallest character boundary at or above `index`.
fn ceil_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}
