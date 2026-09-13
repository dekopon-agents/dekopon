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

use async_trait::async_trait;
use dekopon_agent::attachment::GeneratedImage;
use dekopon_broker_protocol::ChatTransportKind;
use dekopon_core::ExternalSubject;
use dekopon_model::ModelText;
use serde_json::Value;

use crate::{
    asset::{AssetSourceRef, PendingAsset},
    progress::ProgressText,
};
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
#[derive(Clone, Debug)]
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
    /// Authenticated service-native coordinates for best-effort in-flight liveness.
    ///
    /// Absent for transports or messages with no configured liveness surface. These values come
    /// only from the transport envelope and are never model-controlled.
    pub liveness: Option<LivenessTarget>,
    /// The span the transport opened when this message arrived, and the root of its trace.
    ///
    /// Built by [`receive_span`] before the payload was parsed, so the acknowledgment, the
    /// signature check, and the routing decision are already inside it. [`crate::session::run_session`]
    /// takes it, parents `gateway.message` under it, and drops it — which is what keeps
    /// `transport.receive` measuring receipt and dispatch rather than the whole session it started.
    pub receive_span: tracing::Span,
}

/// Opens the trace one inbound message rides, at the moment its transport received it.
///
/// This is the trace root goal 2 of `docs/design.md#constitution` asks for: the acknowledgment, the
/// parse, the routing decision, `gateway.message`, the model turn, every broker invocation, and
/// every provider call hang from it, so an operator reconstructs one message from receipt onward
/// rather than from the point routing had already succeeded. `message.id` is recorded once the
/// payload has been parsed far enough to carry one; a receipt that routes nothing closes this span
/// without one, which is the trace that answers "why did the bot not reply".
///
/// `drop.reason` is declared here rather than by the transport that records it, because
/// [`tracing::Span::record`] on a field the span never declared is a silent no-op: every reader's
/// drop reason would vanish with nothing to say it had. One declaration is also what keeps the
/// reason comparable across transports — the low-cardinality word for why a receipt routed
/// nothing (`self-authored`, `content-withheld`, `duplicate`, …), never the payload it came from.
///
/// No credential, sender, or message text goes on it. The sender stays on the payload-gated
/// `gateway.message.received` record.
pub(crate) fn receive_span(kind: ChatTransportKind) -> tracing::Span {
    tracing::info_span!(
        "transport.receive",
        transport.kind = %kind,
        message.id = tracing::field::Empty,
        drop.reason = tracing::field::Empty,
    )
}

/// One event produced by a chat transport.
#[derive(Clone, Debug)]
pub(crate) enum TransportEvent {
    /// A user message eligible for ordinary routing.
    Message(Box<InboundMessage>),
    /// An authenticated request to stop one active run, from any of the ways a service offers.
    CancelRequested(CancelRequest),
}

/// Authenticated request to cancel one running session.
///
/// The subject is the canonical rendering of the authenticated sender rather than the typed
/// [`ExternalSubject`], because every origin — a native Stop envelope, a button press, a stop word
/// in the thread — proves the same thing about it and the registry compares it against the
/// canonical form of the session's own subject. Only the subject that started a session may stop
/// it; another person's request is acknowledged and ignored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CancelRequest {
    pub transport: String,
    pub conversation_id: String,
    pub subject: String,
    pub via: dekopon_agent::CancelVia,
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

/// Service-native destination for everything a waiting person is shown while a session runs.
///
/// One target covers typing, native status, reactions, the progress message, the streamed answer,
/// and the cancel button, because they all address the same conversation on the same service and a
/// second coordinate set would be a second thing to keep in agreement. Every field comes from the
/// authenticated transport envelope; no model text reaches one.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum LivenessTarget {
    Slack {
        channel_id: String,
        thread_ts: String,
        message_ts: String,
        initiator_user_id: String,
    },
    Discord {
        channel_id: String,
        message_id: String,
    },
    Telegram {
        chat_id: i64,
        message_thread_id: Option<i64>,
        message_id: i64,
    },
    WhatsApp {
        recipient: String,
        inbound_message_id: String,
    },
    Local {
        connection: u64,
    },
}

/// A message the gateway posted and may edit, stream into, finalize, or delete.
///
/// Held only in the session's own policy task and gone when it ends: a restarted gateway forgets
/// every progress message it was editing, which is the no-durable-state rule applied to
/// presentation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct MessageRef {
    pub target: LivenessTarget,
    pub id: String,
}

/// The cumulative model text so far, bounded by the policy to the surface's own ceiling.
///
/// Separate from [`ProgressText`] because the two have different provenance and the type is the
/// proof: this one is model-authored and arrives only from the model client's own
/// [`ModelText`], while a [`ProgressText`] is built from operator templates and numbers. Which is
/// also why the cut is reported in a second field instead of written into the text: every
/// character of `text` came from the model, and the marker that says it was cut is the driver's.
#[derive(Clone, Debug)]
pub(crate) struct StreamedText {
    pub text: ModelText,
    /// Whether the policy cut the text to the surface's ceiling.
    ///
    /// A driver that shows this text appends `…` when it is set: the person reading has to be able
    /// to tell a cut sentence from an answer that ends where it ends.
    pub truncated: bool,
}

/// The two states a service's own "the agent is working" indicator has.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Status {
    Working,
    Idle,
}

/// What one transport's editable progress message will accept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProgressLimits {
    pub max_chars: usize,
    pub min_edit_interval: Duration,
}

/// What one transport's streamed answer will accept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StreamLimits {
    pub min_interval: Duration,
    pub max_chars: usize,
}

/// How the transport reader acknowledges a cancel press within the service deadline.
///
/// Every variant is service-issued and single-use: a Discord interaction token, a Telegram
/// callback-query identifier, a Slack socket envelope. None of them is a credential this daemon
/// holds, and none survives the acknowledgment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AckToken {
    Discord {
        interaction_id: String,
        interaction_token: String,
    },
    Telegram {
        callback_query_id: String,
    },
    Slack {
        envelope_id: String,
    },
    Local,
}

/// One authenticated press of a cancel control, before it becomes a [`CancelRequest`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CancelPress {
    pub target: LivenessTarget,
    pub subject: String,
    pub ack: AckToken,
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

    /// Removes every mention form [`Self::is_addressed`] recognizes.
    ///
    /// The two methods share the forms deliberately: a stop word said in a channel arrives as
    /// `<@U0123> stop`, and a stripper that knew a different set of mentions than the addressing
    /// check would make "stop" work in a direct message and silently not in a channel. Matching is
    /// case-sensitive for identifiers and case-insensitive for the handle, exactly as above.
    pub fn strip_mentions(&self, text: &str) -> String {
        let mut stripped = text.to_owned();
        if let Some(user_id) = &self.user_id {
            for form in [format!("<@{user_id}>"), format!("<@!{user_id}>")] {
                stripped = stripped.replace(&form, " ");
            }
        }
        if let Some(handle) = &self.handle {
            stripped = remove_ascii_insensitive(&stripped, &format!("@{handle}"));
        }
        stripped
    }
}

/// Whether one message is exactly a stop word said to this bot.
///
/// Exactly, after the mention forms [`TransportIdentity::is_addressed`] knows and any trailing
/// punctuation are removed: a message that merely *contains* "stop" is a sentence for the agent to
/// read, and cancelling on it would make the bot unusable in a conversation about stopping things.
/// It lives here because what counts as "said to this bot" is the mention grammar above, and one
/// definition of that grammar is what keeps a stop word working the same in a channel and a DM.
pub(crate) fn is_stop_word(
    identity: Option<&TransportIdentity>,
    text: &str,
    stop_words: &[String],
) -> bool {
    let stripped =
        identity.map_or_else(|| text.to_owned(), |identity| identity.strip_mentions(text));
    let candidate = stripped
        .trim()
        .trim_end_matches(|character: char| character.is_ascii_punctuation())
        .trim()
        .to_lowercase();
    !candidate.is_empty() && stop_words.contains(&candidate)
}

/// Removes every occurrence of `needle`, ignoring ASCII case.
///
/// Index-safe because `to_ascii_lowercase` maps only the 26 ASCII letters, one byte to one byte,
/// so a position found in the lowercased copy is the same position in the original.
fn remove_ascii_insensitive(text: &str, needle: &str) -> String {
    let lowered_text = text.to_ascii_lowercase();
    let lowered_needle = needle.to_ascii_lowercase();
    let mut kept = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(found) = lowered_text[cursor..].find(&lowered_needle) {
        let start = cursor + found;
        kept.push_str(&text[cursor..start]);
        kept.push(' ');
        cursor = start + lowered_needle.len();
    }
    kept.push_str(&text[cursor..]);
    kept
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

    /// A cheaply cloned handle sessions answer and show progress through.
    ///
    /// Replying is separated from the transport itself because a session answers minutes after the
    /// message arrived, while `next` has long since gone back to waiting. Handing sessions a shared
    /// handle is what lets both happen at once without a lock across the wait.
    fn driver(&self) -> Arc<dyn ChatDriver>;

    /// How this transport turns an attachment reference back into bytes, when it can.
    ///
    /// Defaulted to `None` so a transport that never carries attachments says nothing about them.
    fn asset_fetcher(&self) -> Option<Arc<dyn AssetFetcher>> {
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

/// One complete terminal chat reply.
///
/// Text and attachment bytes travel as separate typed fields. Each image's own `Debug` is
/// metadata-only, so formatting this value cannot place PNG bytes in a log.
#[derive(Debug)]
pub(crate) struct OutboundReply {
    pub text: String,
    /// Attachments to post beside the text, in the order the session accepted them. Empty for a
    /// text-only reply, which every transport must deliver byte for byte as it always has.
    pub images: Vec<GeneratedImage>,
}

impl OutboundReply {
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }

    pub(crate) fn with_images(text: impl Into<String>, images: Vec<GeneratedImage>) -> Self {
        Self {
            text: text.into(),
            images,
        }
    }
}

/// The service's own expiring "typing…" lease.
///
/// The renewal interval belongs to the object because only the implementation knows how long its
/// service keeps the indicator alive; the policy renews inside it rather than guessing.
#[async_trait]
pub(crate) trait TypingLease: Send + Sync {
    fn renew_every(&self) -> Duration;
    async fn renew(&self, target: &LivenessTarget) -> Result<(), TransportError>;
}

/// The service's own durable working/idle state, such as a Slack Agent session status.
#[async_trait]
pub(crate) trait NativeStatus: Send + Sync {
    async fn set(&self, target: &LivenessTarget, status: Status) -> Result<(), TransportError>;
}

/// One editable message the gateway posts to say what it is doing.
///
/// `cancel` asks for the transport's own stop control on the message; a driver whose service has
/// no components ignores it, which is why it is a flag here rather than a second method.
#[async_trait]
pub(crate) trait ProgressMessage: Send + Sync {
    fn limits(&self) -> ProgressLimits;
    async fn post(
        &self,
        target: &LivenessTarget,
        text: &ProgressText,
        cancel: bool,
    ) -> Result<MessageRef, TransportError>;
    async fn edit(
        &self,
        message: &MessageRef,
        text: &ProgressText,
        cancel: bool,
    ) -> Result<(), TransportError>;
    async fn delete(&self, message: &MessageRef) -> Result<(), TransportError>;
    /// Turns the progress message into the answer in place.
    ///
    /// `Err` is not a delivery failure to report: it means the policy deletes this message and
    /// falls back to [`ChatDriver::reply`], whose `Ok` is the only acceptance receipt there is.
    async fn finalize(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError>;
}

/// The model's answer as it is written, shown in one message that grows.
#[async_trait]
pub(crate) trait TextStream: Send + Sync {
    fn limits(&self) -> StreamLimits;
    /// Shows cumulative text. `message` is `None` on the first call; the returned reference is the
    /// stream's message — Slack's stream `ts`, or the edited Discord/Telegram message.
    ///
    /// [`StreamedText::truncated`] is the one field an implementation must act on rather than pass
    /// through: when it is set the text was cut to [`StreamLimits::max_chars`] and the driver
    /// appends `…` to whatever it shows.
    async fn show(
        &self,
        target: &LivenessTarget,
        message: Option<&MessageRef>,
        text: &StreamedText,
        cancel: bool,
    ) -> Result<MessageRef, TransportError>;
    /// Turns the streamed message into the answer in place, with the same fallback contract as
    /// [`ProgressMessage::finalize`].
    async fn finalize(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError>;
}

/// The gateway's own fixed reaction on the message being answered.
///
/// The cheapest signal any service offers, and the only one available at t=0 on every transport
/// that has reactions at all.
#[async_trait]
pub(crate) trait InboundReaction: Send + Sync {
    async fn set(&self, target: &LivenessTarget, present: bool) -> Result<(), TransportError>;
}

/// Acknowledges a cancel press inside the service's interaction deadline.
///
/// Called from the transport reader *before* the inbound channel send, because that send blocks on
/// a bounded queue and a three-second interaction deadline does not survive waiting in it.
#[async_trait]
pub(crate) trait CancelButton: Send + Sync {
    async fn ack(&self, press: &CancelPress) -> Result<(), TransportError>;
}

/// The answering half of a transport, shared by every in-flight session on it.
///
/// `Ok` from [`Self::reply`] means one complete bounded answer — every chunk and every attachment —
/// reached service or kernel acceptance. It does not claim human receipt. A reply that arrived in
/// part is [`TransportError::PartialDelivery`], not success.
///
/// Everything beyond replying is a capability object the driver either has or does not.
/// `Some` means implemented: there is no descriptor to keep in agreement with the implementation
/// and no unsupported-primitive error for the policy to construct and never reach. A transport
/// that grows a surface returns `Some` from one more accessor and nothing else changes.
#[async_trait]
pub(crate) trait ChatDriver: Send + Sync {
    async fn reply(&self, target: &ReplyTarget, reply: OutboundReply)
    -> Result<(), TransportError>;

    fn typing(&self) -> Option<&dyn TypingLease> {
        None
    }

    fn status(&self) -> Option<&dyn NativeStatus> {
        None
    }

    fn progress(&self) -> Option<&dyn ProgressMessage> {
        None
    }

    fn stream(&self) -> Option<&dyn TextStream> {
        None
    }

    fn reaction(&self) -> Option<&dyn InboundReaction> {
        None
    }

    fn cancel_button(&self) -> Option<&dyn CancelButton> {
        None
    }
}

/// Resolves an attachment reference back into the bytes a model can look at.
///
/// Separate from [`ChatDriver`] because the two are used at different moments by different code:
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
/// One definition here rather than per reader, because every chat transport and the model client
/// read an owner-named credential variable through [`read_credential`].
pub(crate) fn credential_value(name: &str, value: String) -> Result<String, TransportError> {
    if value.trim().is_empty() {
        return Err(TransportError::EmptyCredential {
            name: name.to_owned(),
        });
    }
    Ok(value)
}

/// Builds the one HTTP client shape every credential-bearing chat transport uses.
///
/// The four transports differ only in their deadline and, for Discord, a user agent, so the stance
/// lives here rather than being restated — and silently diverging — at each of them. None of these
/// settings is reqwest's default:
///
/// - `no_proxy()` overrides the `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY` reqwest reads from the
///   environment by default. Left at the default, an exported proxy variable would carry a Slack
///   app and bot token, a Discord bot token, a Telegram bot token, and a WhatsApp Graph access
///   token — and the message bytes they authenticate — through a host nobody named to Dekopon.
///   `dekopon-http-host` takes this stance for provider HTTP, `dekopon-brokerd` for its secret
///   sources and OCI registries, and `dekopon-model` for model endpoints; a chat token is a
///   credential like any of them.
/// - `redirect(Policy::none())` keeps a credential-bearing request on the endpoint it was
///   addressed to; a followed redirect would hand the bearer token to whatever host answered.
/// - `retry(never())` disables reqwest's default replay of protocol NACKs. Posting a chat message
///   is not idempotent, and automatic retries are a stated non-goal: a failed call is the model's
///   to re-assess.
///
/// The caller builds, so Discord can add its user agent without a second definition of the rest.
pub(crate) fn credential_client(timeout: Duration) -> reqwest::ClientBuilder {
    credential_client_from(reqwest::Client::builder(), timeout)
}

/// Applies that stance to whatever builder the caller started from.
///
/// Production always starts from `reqwest::Client::builder()`, whose default reads the ambient
/// proxy variables. The seam exists for the proxy assertion: a default builder on a proxy-free
/// runner carries no proxy either way, so a builder that had dropped `.no_proxy()` would still
/// look clean and the test would prove nothing. The test starts from a builder that definitely
/// carries a proxy and watches this clear it, with no process environment to mutate and nothing
/// for a concurrent test to race.
fn credential_client_from(
    builder: reqwest::ClientBuilder,
    timeout: Duration,
) -> reqwest::ClientBuilder {
    builder
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .retry(reqwest::retry::never())
        .timeout(timeout)
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
        SeenIds, TextUnit, TransportIdentity, credential_client_from, is_stop_word, jitter_below,
        reconnect_delay, retry_after_from_body, split_message,
    };

    /// The discard port: a proxy that is well formed, never dialled, and obvious in a diff.
    const AMBIENT_PROXY: &str = "http://127.0.0.1:9";

    /// The shape an exported `HTTPS_PROXY=http://127.0.0.1:9` leaves in reqwest's default builder.
    fn proxied_builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(AMBIENT_PROXY).expect("a well-formed proxy uri"))
    }

    /// A chat token is a credential, and the four transports that carry one all build from
    /// [`credential_client_from`]. `reqwest` exposes no getters for a builder's configuration, so
    /// the builder's own `Debug` is the reading: it prints `proxies` only when the proxy list is
    /// non-empty and `redirect_policy` only when the policy is not the default ten-hop limit. The
    /// retry policy is not printed at all, so this test pins the proxy, the redirect and the
    /// deadline and cannot see `retry(never())`.
    #[test]
    fn a_credential_client_ignores_ambient_proxy_configuration() {
        // Not read from the environment: a default builder on a proxy-free runner produces the
        // same empty proxy list whether or not `.no_proxy()` is there, so starting from one would
        // assert nothing. Starting from a builder that carries a proxy is what makes this fail if
        // `.no_proxy()` is dropped — and it mutates no process state, so it races no other test.
        assert!(
            format!("{:?}", proxied_builder()).contains("proxies"),
            "the fixture must carry the proxy this test is about"
        );

        let rendered = format!(
            "{:?}",
            credential_client_from(proxied_builder(), Duration::from_secs(30))
        );

        assert!(
            !rendered.contains("proxies"),
            "chat transports must not inherit an ambient proxy: {rendered}"
        );
        assert!(
            rendered.contains("redirect_policy: Policy(None)"),
            "a credential-bearing request must not follow a redirect: {rendered}"
        );
        assert!(
            rendered.contains("timeout: 30s"),
            "the caller's deadline must survive the shared stance: {rendered}"
        );
    }

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

    fn slack_bot() -> TransportIdentity {
        TransportIdentity {
            user_id: Some("U0123ABC".to_owned()),
            handle: Some("Dekopon".to_owned()),
        }
    }

    fn words() -> Vec<String> {
        vec!["stop".to_owned(), "cancel".to_owned()]
    }

    /// The form a stop word arrives in differs by surface and must not: a direct message carries
    /// the bare word, a channel carries the mention the bot was addressed with, and the reason the
    /// matcher strips mentions at all is that the channel form is the one that used to be
    /// unreachable — every unaddressed channel message is dropped before any matcher could run,
    /// and an addressed one never equals `stop`.
    #[test]
    fn a_stop_word_is_recognized_in_both_the_direct_message_and_channel_forms() {
        let identity = slack_bot();
        for (text, expected, why) in [
            ("stop", true, "the direct-message form"),
            ("Cancel", true, "case is not part of the word"),
            (
                "stop.",
                true,
                "trailing punctuation is not part of the word",
            ),
            ("<@U0123ABC> stop", true, "the Slack channel form"),
            ("<@!U0123ABC> stop!", true, "Discord's legacy nickname form"),
            (
                "@dekopon cancel",
                true,
                "the handle form, case-insensitively",
            ),
            (
                "  stop  ",
                true,
                "surrounding whitespace is not part of the word",
            ),
            (
                "stop the build",
                false,
                "a sentence that contains the word is a question",
            ),
            ("please stop", false, "the word must be the whole message"),
            (
                "<@U0123ABC> stop the build",
                false,
                "addressed, but still a sentence",
            ),
            ("stopping", false, "a longer word is a different word"),
            ("<@U0123ABC>", false, "a bare mention is not a stop word"),
        ] {
            assert_eq!(
                is_stop_word(Some(&identity), text, &words()),
                expected,
                "{text:?} should {} match: {why}",
                if expected { "" } else { "not" }
            );
        }
    }

    /// The operator's list is the whole list: a deployment whose people say `basta` configures it,
    /// and the English defaults stop being special.
    #[test]
    fn only_the_configured_words_stop_a_run() {
        let configured = vec!["basta".to_owned()];
        assert!(is_stop_word(None, "basta", &configured));
        assert!(!is_stop_word(None, "stop", &configured));
        assert!(
            !is_stop_word(None, "stop", &[]),
            "an empty list stops nothing, which is why configuration refuses one"
        );
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
