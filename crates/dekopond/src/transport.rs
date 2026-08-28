//! The chat-service boundary: waiting for a message, and answering one.
//!
//! Everything a transport produces is untrusted except the subject, and the subject is trusted only
//! in the narrow sense that the *service* authenticated it — it is canonical routing metadata that
//! the broker alone maps to a principal. Message text is untrusted end to end and is bounded before
//! it reaches a model.

use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use dekopon_broker_protocol::ChatTransportKind;
use dekopon_core::ExternalSubject;
use dekopon_model::image::GeneratedImage;
use serde_json::Value;

use crate::asset::{AssetSourceRef, PendingAsset};
use futures_util::future::BoxFuture;
use thiserror::Error;

pub(crate) mod discord;
pub(crate) mod local;
pub(crate) mod slack;
pub(crate) mod telegram;
pub(crate) mod whatsapp;

/// Inbound chat text is bounded before prompting, because a chat service's own message ceiling is
/// not a bound this daemon chose.
pub(crate) const MAX_INBOUND_TEXT_BYTES: usize = 16 * 1024;
/// Outbound answers are bounded because a model writes them and chat services reject or silently
/// mangle oversized posts.
pub(crate) const MAX_OUTBOUND_TEXT_BYTES: usize = 8 * 1024;
/// Ceiling on reconnect backoff.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);
/// First reconnect delay; doubles up to [`MAX_RECONNECT_DELAY`].
const BASE_RECONNECT_DELAY: Duration = Duration::from_millis(500);
/// Upper bound on the jitter added to a reconnect delay.
const RECONNECT_JITTER_MS: u64 = 250;
/// How many doublings a delay may accumulate, which is what reaches the ceiling from the base.
const MAX_RECONNECT_DOUBLINGS: u32 = 7;

/// One authenticated inbound chat message.
///
/// Redelivery is already rejected before one of these is built, inside the transport that knows what
/// a redelivery looks like: Slack's [`SeenIds`] ring keyed on `channel:ts`, Discord's on the message
/// snowflake, WhatsApp's bounded claim set on the `wamid`, and Telegram's advancing `offset`, which
/// is the acknowledgment. [`Self::message_id`] therefore exists for the delivered-turn attestation
/// rather than for that question.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InboundMessage {
    /// The configured transport name this arrived on.
    pub transport: String,
    /// Transport family that authenticated this message.
    pub transport_kind: ChatTransportKind,
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
    /// Service-native message identifier of the turn being answered.
    ///
    /// Read by [`crate::session::delivery_identity`], which is the only downstream consumer: it
    /// turns this into the typed [`dekopon_broker_protocol::DeliveryIdentity`] the broker checks
    /// against the separately attested chat scope, so a Slack timestamp cannot be replayed as a
    /// Discord snowflake. Redelivery rejection is *not* what this is for — each transport does that
    /// itself, before building the message.
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
    /// text cannot override the authenticated structure. Slack supplies the authenticated
    /// `app_mention` event type (with mention syntax as a defensive fallback). Other transports
    /// use `None` and the routing loop applies their identifier/handle syntax through
    /// [`TransportIdentity::is_addressed`]. Direct messages ignore this field.
    pub addressed: Option<bool>,
    /// Slack Agent thread ownership carried from authenticated transport state.
    ///
    /// An explicitly addressed message proposes a claim that the session records only after fresh
    /// broker authorization. `inherited` is true only when the same authenticated sender later
    /// speaks in that exact claimed thread without mentioning the bot. No model text can create or
    /// select this state.
    pub thread_continuation: Option<ThreadContinuation>,
    /// Whatever the transport needs to answer this message.
    pub reply: ReplyTarget,
    /// Authenticated service-native coordinates for best-effort in-flight activity.
    ///
    /// Absent for transports or messages with no configured activity surface. These values come
    /// only from the transport envelope and are never model-controlled.
    pub activity: Option<ActivityTarget>,
}

/// One event produced by a chat transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TransportEvent {
    /// A user message eligible for ordinary routing.
    Message(Box<InboundMessage>),
    /// A user asked the service's native Agent/session UI to stop one active run.
    SessionStopped(SessionStop),
}

/// Authenticated request to stop one native chat session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionStop {
    pub transport: String,
    pub conversation_id: String,
    pub subject: ExternalSubject,
}

/// One authenticated service-native thread claim.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ThreadClaim {
    Slack {
        team_id: String,
        channel_id: String,
        thread_ts: String,
        user_id: String,
    },
}

/// How one Slack Agent channel message entered routing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ThreadContinuation {
    pub claim: ThreadClaim,
    /// `true` only when a prior freshly authorized message claimed this exact sender/thread.
    pub inherited: bool,
}

/// Service-native destination for transient in-flight activity.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ActivityTarget {
    Slack {
        channel_id: String,
        thread_ts: String,
        message_ts: String,
        initiator_user_id: String,
    },
    Discord {
        channel_id: String,
    },
    Telegram {
        chat_id: i64,
        message_thread_id: Option<i64>,
    },
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
        message_thread_id: Option<i64>,
    },
    WhatsApp {
        recipient: String,
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

    /// Waits for the next routable message or native session-control event, reconnecting internally
    /// as needed.
    fn next(&mut self) -> BoxFuture<'_, Result<TransportEvent, TransportError>>;

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

    /// Best-effort native activity for authorized sessions on this transport.
    ///
    /// Defaulted to absent so the local development transport and future transports preserve their
    /// reply-only behavior without implementing a cosmetic surface.
    fn activity(&self) -> Option<Arc<dyn ChatActivity>> {
        None
    }

    /// Bounded transport-owned registry for authenticated thread continuation.
    ///
    /// Defaulted to absent because only Slack's Agent experience currently owns threaded channel
    /// sessions. A claim is made by the session after fresh authorization, never by the transport
    /// reader merely seeing an event.
    fn thread_ownership(&self) -> Option<Arc<dyn ThreadOwnership>> {
        None
    }
}

/// Transport-owned, authorization-fed thread continuation state.
///
/// The transport reader consults this state to distinguish one sender's claimed Agent thread from
/// ambient channel history. The session mutates it only after a fresh broker answer.
pub(crate) trait ThreadOwnership: Send + Sync {
    fn claim(&self, claim: ThreadClaim);
    fn revoke(&self, claim: &ThreadClaim);
}

/// Transport-owned renderer for the service's native in-flight activity.
///
/// The shared coordinator owns lifecycle and refresh timing; implementations own credentials,
/// exact endpoints, fallback, and per-installation degradation. Failures are cosmetic and never
/// become session failures.
pub(crate) trait ChatActivity: Send + Sync {
    /// Starts or renews activity for one authenticated target under a short driver-owned deadline.
    ///
    /// The coordinator deliberately retains an issued call across sealing so later cleanup cannot
    /// be reordered ahead of bytes already sent to the service.
    fn show(&self, target: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>>;

    /// Clears activity where the service supports it, or performs a no-op for expiring signals.
    fn hide(&self, target: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>>;

    /// Renewal interval for expiring signals; `None` for durable state transitions such as Slack.
    fn refresh_interval(&self) -> Option<Duration>;
}

/// One complete terminal chat reply.
///
/// Text and generated bytes travel as separate typed fields. The image's own `Debug` is
/// metadata-only, so formatting this value cannot place PNG bytes in a log.
#[derive(Debug)]
pub(crate) struct OutboundReply {
    pub text: String,
    pub image: Option<GeneratedImage>,
}

impl OutboundReply {
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            image: None,
        }
    }

    pub(crate) fn with_image(text: impl Into<String>, image: GeneratedImage) -> Self {
        Self {
            text: text.into(),
            image: Some(image),
        }
    }
}

/// The answering half of a transport, shared by every in-flight session on it.
pub(crate) trait ChatReplier: Send + Sync {
    fn reply(
        &self,
        target: ReplyTarget,
        reply: OutboundReply,
    ) -> BoxFuture<'_, Result<DeliveryReceipt, TransportError>>;
}

/// Opaque proof that one complete bounded answer reached service/kernel transport acceptance.
///
/// It is intentionally non-serializable and fully redacted. It does not claim human receipt.
pub(crate) struct DeliveryReceipt {
    acceptance: String,
}

impl DeliveryReceipt {
    pub(crate) fn new(acceptance: impl Into<String>) -> Self {
        Self {
            acceptance: acceptance.into(),
        }
    }

    #[must_use]
    pub(crate) fn accepted(&self) -> bool {
        !self.acceptance.is_empty()
    }
}

impl std::fmt::Debug for DeliveryReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DeliveryReceipt([REDACTED])")
    }
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
    #[error("credential environment variable {name} is not set")]
    MissingCredential { name: String },
    #[error("credential environment variable {name} is set to an empty value")]
    EmptyCredential { name: String },
    #[error("credential environment variable {name} is not UTF-8")]
    NonUtf8Credential { name: String },
    #[error("chat service request failed")]
    Request(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("chat service returned an error: {code}")]
    Service { code: String },
    #[error("chat service response was not the expected shape")]
    Response,
    /// A service response was not JSON at all, as opposed to JSON missing a field the call needs.
    ///
    /// The source is safe to render: every parse behind it targets [`serde_json::Value`], which
    /// accepts any well-formed document, so the only failures reachable here are syntactic. The
    /// message is a byte offset and what the parser expected there — an HTML error page from an
    /// interposed proxy, or a body cut short — and never a field of the payload.
    #[error("chat service response was not valid JSON")]
    MalformedResponse(#[source] serde_json::Error),
    #[error("chat service accepted only part of a split answer")]
    PartialDelivery,
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
            Self::EmptyCredential { .. } => "empty-credential",
            Self::NonUtf8Credential { .. } => "non-utf8-credential",
            Self::Request(_) => "request",
            Self::Service { .. } => "service",
            Self::Response => "response",
            Self::MalformedResponse(_) => "malformed-response",
            Self::PartialDelivery => "partial-delivery",
            Self::Closed => "closed",
            Self::Io(_) => "io",
            Self::InsecureSocket { .. } => "insecure-socket",
            Self::Subject(_) => "subject",
        }
    }
}

/// Reads one credential by variable name, reporting the *name* and never the value.
pub(crate) fn read_credential(name: &str) -> Result<String, TransportError> {
    credential_from(name, std::env::var_os(name))
}

/// Decides what a named credential variable holds, given what the environment holds for it.
///
/// Split from the read so the rule is reachable without a test mutating this process's
/// environment: `set_var` is unsafe in this edition and this workspace forbids unsafe outright.
pub(crate) fn credential_from(
    name: &str,
    value: Option<std::ffi::OsString>,
) -> Result<String, TransportError> {
    let value = value.ok_or_else(|| TransportError::MissingCredential {
        name: name.to_owned(),
    })?;
    #[allow(
        clippy::map_err_ignore,
        reason = "OsString::into_string returns the credential value itself as its error; keeping \
                  it would move the secret into an error this daemon renders"
    )]
    let value = value
        .into_string()
        .map_err(|_| TransportError::NonUtf8Credential {
            name: name.to_owned(),
        })?;
    credential_value(name, value)
}

/// Rejects an exported-but-empty credential, which is a misconfiguration rather than a secret.
///
/// A blank value is not a weak token, it is the absence of one presented as presence: an empty HMAC
/// key verifies signatures anybody can compute, and an empty bearer token is still sent as a header.
/// One definition here rather than per reader, because every chat transport, the model client, and
/// the image generator read an owner-named credential variable through [`read_credential`].
pub(crate) fn credential_value(name: &str, value: String) -> Result<String, TransportError> {
    if value.trim().is_empty() {
        return Err(TransportError::EmptyCredential {
            name: name.to_owned(),
        });
    }
    Ok(value)
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

/// How a chat service counts one message against its own length ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TextUnit {
    /// UTF-16 code units. Discord's 2,000 and Telegram's 4,096 are both UTF-16 ceilings, and
    /// counting scalar values against them would let a chunk of astral emoji through at twice the
    /// declared size — the whole answer rejected, with no partial delivery and nothing to read.
    Utf16,
    /// Unicode scalar values, which is what Meta counts against WhatsApp's 4,096 ceiling.
    Scalar,
}

impl TextUnit {
    /// How much one character costs against a ceiling counted in this unit.
    const fn weight(self, character: char) -> usize {
        match self {
            Self::Utf16 => character.len_utf16(),
            Self::Scalar => 1,
        }
    }
}

/// Splits one answer into chunks a chat service will accept, preferring line boundaries.
///
/// `max_units` is counted in `unit`, because the services do not agree on what they count and a
/// chunk measured in the wrong unit is rejected whole rather than trimmed.
///
/// Not truncation: the gateway's own outbound bound is 8 KiB, above what one Discord, Telegram, or
/// WhatsApp message may carry, so an answer longer than a service ceiling is the ordinary case and
/// dropping its second half would lose the conclusion.
///
/// An empty answer becomes one placeholder chunk, for every service alike: they all refuse an
/// empty post, so the alternative to a placeholder is not an empty message but a delivery failure,
/// and "the model said nothing" is a better thing for a person to see than silence.
pub(crate) fn split_message(text: &str, max_units: usize, unit: TextUnit) -> Vec<String> {
    if text.is_empty() {
        return vec!["[empty response]".to_owned()];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let rest = &text[start..];
        let mut units = 0;
        let mut end = text.len();
        for (index, character) in rest.char_indices() {
            let next = units + unit.weight(character);
            if next > max_units {
                end = start + index;
                break;
            }
            units = next;
        }
        if end < text.len()
            && let Some(newline) = text[start..end].rfind('\n')
            && newline > 0
        {
            end = start + newline + 1;
        }
        chunks.push(text[start..end].to_owned());
        start = end;
    }
    chunks
}

/// Exponential reconnect backoff with a fixed ceiling and random jitter.
///
/// The jitter is what keeps a fleet of daemons restarted together — a rolling deploy, a service
/// outage that dropped every socket at once — from lining up on the same retry instant and
/// arriving as one thundering herd. It is drawn from the OS rather than derived from the process
/// identifier, which a container runtime is free to hand out identically in every pod.
pub(crate) fn reconnect_delay(failures: u32) -> Duration {
    let step = BASE_RECONNECT_DELAY.saturating_mul(1_u32 << failures.min(MAX_RECONNECT_DOUBLINGS));
    step.min(MAX_RECONNECT_DELAY)
        .saturating_add(Duration::from_millis(jitter_below(RECONNECT_JITTER_MS)))
}

/// A random value in `[0, upper)`.
///
/// The modulo bias is immaterial: every caller is spreading retries or heartbeats over a window,
/// not minting an identifier. `0` for an empty range, and for an OS that would not supply entropy —
/// which costs de-synchronization rather than correctness, and says so once per occurrence.
pub(crate) fn jitter_below(upper: u64) -> u64 {
    if upper == 0 {
        return 0;
    }
    let mut bytes = [0_u8; 8];
    if let Err(error) = getrandom::fill(&mut bytes) {
        tracing::warn!(event = "gateway_transport_jitter_unavailable", error = %error);
        return 0;
    }
    u64::from_le_bytes(bytes) % upper
}

/// A server-directed wait read from a rate-limit response body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetryAfter {
    /// How long to wait, never longer than the ceiling the caller passed.
    pub wait: Duration,
    /// Whether the service asked for longer than that ceiling.
    ///
    /// The difference a caller acts on: a wait it is willing to sit out and retry, against one it
    /// will not, which is a rate limit to report rather than absorb.
    pub capped: bool,
}

/// Reads the `retry_after` seconds a rate-limit body names, in seconds, capped at `max`.
///
/// `None` means the body named no wait that can be acted on — the field is absent, is not a
/// number, is not finite, or is negative. That is a malformed rate-limit response rather than a
/// wait, and it is deliberately not the same answer as a wait that is merely too long: one says
/// the service is throttling, the other says the service did not say why it refused.
pub(crate) fn retry_after_from_body(body: &Value, max: Duration) -> Option<RetryAfter> {
    let seconds = body["retry_after"]
        .as_f64()
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)?;
    let ceiling = max.as_secs_f64();
    Some(RetryAfter {
        wait: Duration::from_secs_f64(seconds.min(ceiling)),
        capped: seconds > ceiling,
    })
}

/// Bounded ring of identifiers a transport has already accepted.
///
/// Bounded because it must survive reconnects without becoming a slow leak on a busy workspace,
/// and a ring because the only redeliveries that matter are recent ones.
pub(crate) struct SeenIds {
    order: VecDeque<String>,
    seen: HashSet<String>,
    capacity: usize,
}

impl SeenIds {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            seen: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    /// Records an identifier, reporting `false` when it was already seen.
    pub(crate) fn insert(&mut self, key: String) -> bool {
        if !self.seen.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        if self.order.len() > self.capacity
            && let Some(evicted) = self.order.pop_front()
        {
            self.seen.remove(&evicted);
        }
        true
    }

    /// Forgets an identifier, so the next delivery carrying it is accepted again.
    pub(crate) fn remove(&mut self, key: &str) {
        if self.seen.remove(key) {
            self.order.retain(|candidate| candidate != key);
        }
    }
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

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, time::Duration};

    use serde_json::json;

    use super::{
        BASE_RECONNECT_DELAY, MAX_RECONNECT_DELAY, MAX_RECONNECT_DOUBLINGS, RECONNECT_JITTER_MS,
        SeenIds, TextUnit, jitter_below, reconnect_delay, retry_after_from_body, split_message,
    };

    /// The delay every transport now shares: doubling from the base, clamped at seven doublings,
    /// ceilinged, and never longer than the ceiling plus one jitter window. The clamp is what
    /// stops the shift from overflowing rather than a cosmetic bound, so a transport that has
    /// failed twenty times must still land in the same window as one that has failed seven.
    #[test]
    fn a_reconnect_delay_doubles_within_its_ceiling_and_jitter() {
        let ceiling = MAX_RECONNECT_DELAY + Duration::from_millis(RECONNECT_JITTER_MS);
        for failures in [0_u32, 1, 2, 7, 8, 20, u32::MAX] {
            let floor = BASE_RECONNECT_DELAY
                .saturating_mul(1 << failures.min(MAX_RECONNECT_DOUBLINGS))
                .min(MAX_RECONNECT_DELAY);
            let delay = reconnect_delay(failures);
            assert!(
                delay >= floor && delay <= ceiling,
                "{failures} failures produced {delay:?}, outside {floor:?}..={ceiling:?}"
            );
        }
        assert_eq!(
            reconnect_delay(7).min(MAX_RECONNECT_DELAY),
            reconnect_delay(u32::MAX).min(MAX_RECONNECT_DELAY)
        );
    }

    /// Jitter has to be inside its window and has to actually vary. The previous per-transport
    /// spellings derived it from the process identifier, which is fixed for the life of a process
    /// and identical across pods a runtime numbers the same way — a jitter that de-synchronizes
    /// nothing.
    #[test]
    fn jitter_stays_below_its_bound_and_is_not_a_constant() {
        assert_eq!(jitter_below(0), 0);
        assert_eq!(jitter_below(1), 0);
        for _ in 0..256 {
            assert!(jitter_below(RECONNECT_JITTER_MS) < RECONNECT_JITTER_MS);
        }
        let drawn: HashSet<u64> = (0..64).map(|_| jitter_below(u64::MAX)).collect();
        assert!(drawn.len() > 1, "the jitter is the same value every time");
    }

    /// A ring, not a set: the oldest identifier is the one evicted, so a redelivery of a recent
    /// message is still refused after the ring has turned over.
    #[test]
    fn seen_identifiers_evict_oldest_first_and_can_be_released() {
        let mut seen = SeenIds::new(2);
        assert!(seen.insert("a".to_owned()));
        assert!(seen.insert("b".to_owned()));
        assert!(!seen.insert("a".to_owned()), "a repeat is refused");

        assert!(seen.insert("c".to_owned()), "the ring accepts a third");
        assert!(seen.insert("a".to_owned()), "the oldest was evicted");
        assert!(!seen.insert("c".to_owned()), "the newest was retained");

        seen.remove("c");
        assert!(
            seen.insert("c".to_owned()),
            "a released claim is accepted again"
        );
    }

    /// The wait a service directs, separated from the two ways a body fails to name one: nothing
    /// usable at all, and a wait longer than the caller will sit out.
    #[test]
    fn a_retry_after_body_is_read_capped_and_classified() {
        let max = Duration::from_secs(30);
        for body in [
            json!({}),
            json!({ "retry_after": "5" }),
            json!({ "retry_after": null }),
            json!({ "retry_after": -1.0 }),
            json!({ "retry_after": f64::INFINITY }),
        ] {
            assert!(
                retry_after_from_body(&body, max).is_none(),
                "{body} named a usable wait"
            );
        }

        let short = retry_after_from_body(&json!({ "retry_after": 1.5 }), max)
            .expect("a wait inside the ceiling");
        assert_eq!(short.wait, Duration::from_millis(1_500));
        assert!(!short.capped);

        let integer =
            retry_after_from_body(&json!({ "retry_after": 2 }), max).expect("an integer wait");
        assert_eq!(integer.wait, Duration::from_secs(2));

        let long = retry_after_from_body(&json!({ "retry_after": 900.0 }), max)
            .expect("a wait past the ceiling is still a wait");
        assert_eq!(long.wait, max, "the wait is capped rather than honored");
        assert!(long.capped, "the caller cannot tell it was capped");

        let exact = retry_after_from_body(&json!({ "retry_after": 30.0 }), max)
            .expect("the ceiling itself");
        assert!(!exact.capped, "the ceiling itself is not over it");
    }

    /// Same text, two ceilings: an astral scalar costs two UTF-16 code units and one scalar value,
    /// which is the whole reason the unit is a parameter rather than an assumption.
    #[test]
    fn splitting_counts_in_the_unit_the_service_enforces() {
        let text = "🦀".repeat(100);
        assert_eq!(split_message(&text, 100, TextUnit::Scalar).len(), 1);
        assert_eq!(split_message(&text, 100, TextUnit::Utf16).len(), 2);
        assert_eq!(
            split_message(&text, 100, TextUnit::Utf16).concat(),
            text,
            "no scalar is lost at a chunk boundary"
        );

        for unit in [TextUnit::Utf16, TextUnit::Scalar] {
            assert_eq!(
                split_message("", 4_096, unit),
                vec!["[empty response]".to_owned()],
                "every service refuses an empty post, so every unit answers the same way"
            );
        }
    }
}
