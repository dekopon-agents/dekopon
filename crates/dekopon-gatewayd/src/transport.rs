//! The subject is trusted only as service-authenticated routing metadata, never as content; message
//! text is always untrusted and bounded before reaching a model.

use std::{
    collections::{HashSet, VecDeque},
    fmt,
    ops::Range,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use dekopon_agent::attachment::GeneratedImage;
use dekopon_agent::wake::WakeId;
use dekopon_broker_protocol::{ChatTransportKind, Conversation, Trigger};
use dekopon_core::ExternalSubject;
use dekopon_model::ModelText;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    asset::{AssetSourceRef, PendingAsset},
    progress::ProgressText,
};
use futures_util::future::BoxFuture;
use thiserror::Error;

pub(crate) mod discord;
pub(crate) mod github;
mod hydration;
pub(crate) mod local;
pub(crate) mod recovery;
pub(crate) mod slack;
pub(crate) mod telegram;
pub(crate) mod webhook;
pub(crate) mod whatsapp;

pub(crate) const MAX_INBOUND_TEXT_BYTES: usize = 16 * 1024;
/// Outbound text is bounded because chat services reject or silently mangle oversized posts.
pub(crate) const MAX_OUTBOUND_TEXT_BYTES: usize = 8 * 1024;
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const BASE_RECONNECT_DELAY: Duration = Duration::from_millis(500);
const RECONNECT_JITTER_MS: u64 = 250;
const MAX_RECONNECT_DOUBLINGS: u32 = 7;

#[derive(Clone, Debug)]
pub(crate) struct InboundMessage {
    pub transport: String,
    pub transport_kind: ChatTransportKind,
    pub subject: ExternalSubject,
    /// Slack omits thread_ts on the message that opens a thread but includes it on every reply, so
    /// this is derived from the bot's own reply, not raw thread_ts.
    pub conversation: Conversation,
    /// This is checked against the separately attested chat scope so a Slack timestamp cannot be
    /// replayed as a Discord snowflake; it is not used for redelivery rejection.
    pub message_id: MessageId,
    /// Reference lines naming attachments are appended by the session, not the transport, because
    /// AssetStore assigns the numbers and a transport minting its own would collide.
    pub text: String,
    pub assets: Vec<PendingAsset>,
    pub asset_overflow: bool,
    /// Discord sets addressed to Some(false) deliberately from its authenticated mentions array so
    /// presentation text cannot override it; other transports use None and match identifier or
    /// handle syntax instead.
    pub addressed: Option<bool>,
    /// Claims are recorded only after fresh broker authorization, and inherited is true only when
    /// the same authenticated sender continues that exact thread; no model text can create this
    /// state.
    pub thread_continuation: Option<ThreadContinuation>,
    pub reply: ReplyTarget,
    pub liveness: Option<LivenessTarget>,
    pub receive_span: tracing::Span,
    pub received_at: tokio::time::Instant,
    pub native_group: Option<String>,
    pub constituents: Vec<tracing::Span>,
    pub folded: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MessageId {
    Native(String),
    Follow(String),
    Wake {
        id: WakeId,
        notice: String,
    },
    Job {
        id: dekopon_shell::JobId,
        agent: dekopon_core::AgentId,
    },
}

impl MessageId {
    pub(crate) const fn trigger(&self) -> Trigger {
        match self {
            Self::Native(_) | Self::Follow(_) => Trigger::Message,
            Self::Wake { .. } => Trigger::Wake,
            Self::Job { .. } => Trigger::Job,
        }
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Native(id) | Self::Follow(id) => formatter.write_str(id),
            Self::Wake { id, .. } => write!(formatter, "wake-{id}"),
            Self::Job { id, .. } => write!(formatter, "job-{id}"),
        }
    }
}

/// drop.reason is declared on this span rather than by the transport that records it, because
/// recording a field a span never declared is a silent no-op.
pub(crate) fn receive_span(kind: ChatTransportKind) -> tracing::Span {
    tracing::info_span!(
        "transport.receive",
        transport.kind = %kind,
        message.id = tracing::field::Empty,
        drop.reason = tracing::field::Empty,
        mention.roles = tracing::field::Empty,
        conversation.kind = tracing::field::Empty,
        conversation.container = tracing::field::Empty,
        conversation.id = tracing::field::Empty,
        conversation.thread = tracing::field::Empty,
    )
}

pub(crate) fn record_conversation(span: &tracing::Span, conversation: &Conversation) {
    span.record("conversation.kind", conversation.kind.as_str());
    span.record("conversation.id", conversation.id.as_str());
    if let Some(container) = &conversation.container {
        span.record("conversation.container", container.as_str());
    }
    if let Some(thread) = &conversation.thread {
        span.record("conversation.thread", thread.as_str());
    }
}

#[derive(Clone, Debug)]
pub(crate) enum TransportEvent {
    Connected {
        name: String,
        identity: TransportIdentity,
    },
    Message(Box<InboundMessage>),
    CancelRequested(CancelRequest),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CancelRequest {
    pub transport: String,
    pub conversation_id: String,
    pub subject: String,
    pub via: dekopon_agent::CancelVia,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ThreadClaim {
    Slack {
        team_id: String,
        channel_id: String,
        thread_ts: String,
        user_id: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ThreadContinuation {
    pub claim: ThreadClaim,
    pub inherited: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum LivenessTarget {
    Slack {
        channel_id: String,
        thread_ts: String,
        message_ts: String,
        initiator_user_id: String,
        conversation_id: String,
    },
    Discord {
        channel_id: String,
        message_id: String,
        conversation_id: String,
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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct MessageRef {
    pub target: LivenessTarget,
    pub id: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct StreamedText {
    pub generation: u64,
    pub text: ModelText,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Status {
    Working,
    Idle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProgressLimits {
    pub max_chars: usize,
    pub min_edit_interval: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StreamLimits {
    pub min_interval: Duration,
    pub max_chars: usize,
}

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CancelPress {
    pub target: LivenessTarget,
    pub subject: String,
    pub ack: AckToken,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
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
    GithubTrace,
    Local {
        connection: u64,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TransportIdentity {
    pub user_id: Option<String>,
    pub handle: Option<String>,
}

impl TransportIdentity {
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

pub(crate) trait ChatTransport: Send {
    fn name(&self) -> &str;

    fn sibling_driver(&self) -> Option<(String, Arc<dyn ChatDriver>)> {
        None
    }

    fn sibling_fetcher(&self) -> Option<(String, Arc<dyn AssetFetcher>)> {
        None
    }

    fn connect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>>;

    fn reconnect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>> {
        self.connect()
    }

    fn retryable(&self, error: &TransportError) -> bool {
        !matches!(error, TransportError::InsecureSocket { .. })
    }

    fn next(&mut self) -> BoxFuture<'_, Result<TransportEvent, TransportError>>;

    fn driver(&self) -> Arc<dyn ChatDriver>;

    fn asset_fetcher(&self) -> Option<Arc<dyn AssetFetcher>> {
        None
    }

    fn thread_ownership(&self) -> Option<Arc<dyn ThreadOwnership>> {
        None
    }
}

pub(crate) trait ThreadOwnership: Send + Sync {
    fn claim(&self, claim: ThreadClaim);
    fn revoke(&self, claim: &ThreadClaim);
}

#[derive(Debug)]
pub(crate) struct OutboundReply {
    pub text: String,
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

#[async_trait]
pub(crate) trait TypingLease: Send + Sync {
    fn renew_every(&self) -> Duration;
    async fn renew(&self, target: &LivenessTarget) -> Result<(), TransportError>;
}

#[async_trait]
pub(crate) trait NativeStatus: Send + Sync {
    async fn set(&self, target: &LivenessTarget, status: Status) -> Result<(), TransportError>;
}

#[async_trait]
pub(crate) trait StatusText: Send + Sync {
    fn min_interval(&self) -> Duration;
    async fn show(
        &self,
        target: &LivenessTarget,
        text: &ProgressText,
    ) -> Result<(), TransportError>;
    async fn clear(&self, target: &LivenessTarget) -> Result<(), TransportError>;
}

#[derive(Debug)]
pub(crate) enum LinkPreviews {
    Default,
    Disabled,
    Enabled,
}

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
    async fn finalize(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError>;
}

#[async_trait]
pub(crate) trait TextStream: Send + Sync {
    fn limits(&self) -> StreamLimits;
    async fn discard(&self, _message: &MessageRef) -> Result<(), TransportError> {
        Ok(())
    }
    async fn show(
        &self,
        target: &LivenessTarget,
        message: Option<&MessageRef>,
        text: &StreamedText,
        cancel: bool,
    ) -> Result<MessageRef, TransportError>;
    async fn finalize(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
        generation: u64,
    ) -> Result<(), TransportError>;
}

#[async_trait]
pub(crate) trait InboundReaction: Send + Sync {
    async fn set(&self, target: &LivenessTarget, present: bool) -> Result<(), TransportError>;
}

#[async_trait]
pub(crate) trait SteerAck: Send + Sync {
    async fn seen(&self, target: &LivenessTarget) -> Result<(), TransportError>;
}

/// Implementations must acknowledge before the event reaches the bounded inbound channel, since
/// that send can block past the interaction deadline.
#[async_trait]
pub(crate) trait CancelButton: Send + Sync {
    async fn ack(&self, press: &CancelPress) -> Result<(), TransportError>;
}

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

    fn status_text(&self) -> Option<&dyn StatusText> {
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

    fn steer_ack(&self) -> Option<&dyn SteerAck> {
        None
    }

    fn cancel_button(&self) -> Option<&dyn CancelButton> {
        None
    }

    fn history(&self) -> Option<&dyn ChatHistory> {
        None
    }
}

/// Messages the chat service shows in a conversation, oldest first, strictly between `after` and
/// `before`; when more than `limit` fall in that range, the newest `limit` of them.
#[async_trait]
pub(crate) trait ChatHistory: Send + Sync {
    async fn recent(
        &self,
        conversation: &Conversation,
        after: Option<&str>,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PastMessage>, TransportError>;
}

#[derive(Clone, Debug)]
pub(crate) struct PastMessage {
    pub id: String,
    pub from_bot: bool,
    pub author: String,
    pub subject: ExternalSubject,
    pub text: String,
    pub assets: Vec<PendingAsset>,
    pub at: std::time::SystemTime,
}

pub(crate) trait AssetFetcher: Send + Sync {
    fn fetch(
        &self,
        source: &AssetSourceRef,
        max_bytes: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, TransportError>>;
}

pub(crate) fn asset_buffer(declared: Option<u64>, limit: usize) -> Vec<u8> {
    let hint = declared
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0)
        .min(limit);
    Vec::with_capacity(hint)
}

pub(crate) fn compact_json<T>(value: &T, suffix: &[u8]) -> Result<Vec<u8>, serde_json::Error>
where
    T: serde::Serialize + ?Sized,
{
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, value)?;
    let mut buffer = Vec::with_capacity(counter.0.saturating_add(suffix.len()));
    serde_json::to_writer(&mut buffer, value)?;
    buffer.extend_from_slice(suffix);
    Ok(buffer)
}

struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn reserve_for_chunk(buffer: &mut Vec<u8>, chunk: usize, limit: usize) {
    let needed = buffer.len().saturating_add(chunk);
    if needed <= buffer.capacity() {
        return;
    }
    let target = buffer.capacity().saturating_mul(2).min(limit).max(needed);
    buffer.reserve_exact(target - buffer.len());
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("unsupported asset content type; this transport accepts {accepted}")]
    AssetType { accepted: &'static str },
    #[error("{0}")]
    Attachment(#[from] dekopon_model::asset::BlobError),
    #[error("attachment hydration task failed ({reason}); provider already executed")]
    AttachmentTask { reason: &'static str },
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
    #[error(
        "Discord refused the requested gateway intents (close 4014): messageContent: true needs \
         Developer Portal > Bot > Privileged Gateway Intents > Message Content Intent turned on"
    )]
    DisallowedIntents,
    #[error("chat service response was not the expected shape")]
    Response,
    #[error("chat service response was not valid JSON")]
    MalformedResponse(#[source] serde_json::Error),
    #[error("chat service accepted only part of a split answer")]
    PartialDelivery,
    #[error("chat transport connection attempt exceeded its deadline")]
    ConnectTimeout,
    #[error("chat transport recovery exhausted after {failures} failures")]
    RecoveryExhausted {
        failures: u32,
        #[source]
        source: Box<TransportError>,
    },
    #[error("chat transport identity changed during recovery")]
    IdentityChanged,
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
    pub const fn category(&self) -> &'static str {
        match self {
            Self::Attachment(_) => "attachment-storage",
            Self::AttachmentTask { .. } => "attachment-task",
            Self::AssetType { .. } => "asset-type",
            Self::MissingCredential { .. } => "missing-credential",
            Self::EmptyCredential { .. } => "empty-credential",
            Self::NonUtf8Credential { .. } => "non-utf8-credential",
            Self::Request(_) => "request",
            Self::Service { .. } => "service",
            Self::DisallowedIntents => "disallowed-intents",
            Self::Response => "response",
            Self::MalformedResponse(_) => "malformed-response",
            Self::PartialDelivery => "partial-delivery",
            Self::Closed => "closed",
            Self::ConnectTimeout => "connect-timeout",
            Self::RecoveryExhausted { .. } => "recovery-exhausted",
            Self::IdentityChanged => "identity-changed",
            Self::Io(_) => "io",
            Self::InsecureSocket { .. } => "insecure-socket",
            Self::Subject(_) => "subject",
        }
    }
}

pub(crate) fn read_credential(name: &str) -> Result<String, TransportError> {
    credential_from(name, std::env::var_os(name))
}

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

pub(crate) fn credential_value(name: &str, value: String) -> Result<String, TransportError> {
    if value.trim().is_empty() {
        return Err(TransportError::EmptyCredential {
            name: name.to_owned(),
        });
    }
    Ok(value)
}

pub(crate) fn credential_client(timeout: Duration) -> reqwest::ClientBuilder {
    credential_client_from(reqwest::Client::builder(), timeout)
}

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

pub(crate) fn bound_inbound(text: &str) -> String {
    if text.len() <= MAX_INBOUND_TEXT_BYTES {
        return text.to_owned();
    }
    let head = floor_boundary(text, MAX_INBOUND_TEXT_BYTES);
    format!("{}\n[message truncated by the gateway]", &text[..head])
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TextUnit {
    /// Discord and Telegram count UTF-16 code units, not Unicode scalars; counting scalars would
    /// let astral emoji through at double their counted size and reject the whole message.
    Utf16,
    Scalar,
}

impl TextUnit {
    const fn weight(self, character: char) -> usize {
        match self {
            Self::Utf16 => character.len_utf16(),
            Self::Scalar => 1,
        }
    }
}

/// max_units is counted in the given unit because chat services disagree on what they count, and a
/// chunk measured in the wrong unit is rejected whole rather than trimmed.
pub(crate) fn split_message(text: &str, max_units: usize, unit: TextUnit) -> Vec<String> {
    if text.is_empty() {
        return vec!["[empty response]".to_owned()];
    }
    let fences = Fence::scan(text, max_units, unit);
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut reopen: Option<&Fence> = None;
    while start < text.len() {
        let (end, open) = chunk_end(text, start, reopen, &fences, max_units, unit);
        let mut chunk = String::new();
        if let Some(fence) = reopen {
            chunk.push_str(&text[fence.opener.clone()]);
        }
        chunk.push_str(&text[start..end]);
        if let Some(fence) = open {
            if !chunk.ends_with('\n') {
                chunk.push('\n');
            }
            chunk.extend(std::iter::repeat_n(fence.marker, fence.width));
        }
        chunks.push(chunk);
        reopen = open;
        start = end;
    }
    chunks
}

struct Fence {
    opener: Range<usize>,
    closer: usize,
    end: usize,
    marker: char,
    width: usize,
}

impl Fence {
    fn scan(text: &str, max_units: usize, unit: TextUnit) -> Vec<Self> {
        let mut fences = Vec::new();
        let mut open: Option<(Range<usize>, char, usize)> = None;
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            let range = offset..offset + line.len();
            offset = range.end;
            let Some((marker, width, info)) = fence_run(line) else {
                continue;
            };
            match open.take() {
                None => {
                    open = (marker == '~' || !info.contains('`')).then_some((range, marker, width));
                }
                Some((opener, open_marker, open_width))
                    if marker == open_marker
                        && width >= open_width
                        && info.trim_matches([' ', '\t']).is_empty() =>
                {
                    fences.push(Self {
                        opener,
                        closer: range.start,
                        end: range.end,
                        marker,
                        width,
                    });
                }
                Some(still_open) => open = Some(still_open),
            }
        }
        if let Some((opener, marker, width)) = open {
            fences.push(Self {
                opener,
                closer: text.len(),
                end: text.len(),
                marker,
                width,
            });
        }
        // A fence whose reopen and close would take over half a chunk is split as plain text, so
        // every chunk inside it still carries content.
        fences.retain(|fence| {
            2 * (units(&text[fence.opener.clone()], unit) + 1 + fence.width) <= max_units
        });
        fences
    }
}

fn fence_run(line: &str) -> Option<(char, usize, &str)> {
    let line = line.trim_end_matches(['\n', '\r']);
    let body = line.trim_start_matches(' ');
    if line.len() - body.len() > 3 {
        return None;
    }
    let marker = body.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let info = body.trim_start_matches(marker);
    let width = body.len() - info.len();
    (width >= 3).then_some((marker, width, info))
}

fn units(text: &str, unit: TextUnit) -> usize {
    text.chars().map(|character| unit.weight(character)).sum()
}

fn open_fence_at<'f>(fences: &'f [Fence], text: &str, cut: usize) -> Option<&'f Fence> {
    let index = fences.partition_point(|fence| fence.closer < cut);
    fences
        .get(index)
        .filter(|fence| fence.opener.end <= cut && cut < text.len())
}

fn chunk_end<'f>(
    text: &str,
    start: usize,
    reopen: Option<&Fence>,
    fences: &'f [Fence],
    max_units: usize,
    unit: TextUnit,
) -> (usize, Option<&'f Fence>) {
    let mut used = reopen.map_or(0, |fence| units(&text[fence.opener.clone()], unit));
    let mut line_cut = None;
    let mut hard_cut = None;
    let mut characters = text[start..].char_indices();
    loop {
        let (offset, next) = characters
            .next()
            .map_or((text.len() - start, None), |(offset, c)| (offset, Some(c)));
        let cut = start + offset;
        if cut > start {
            let open = open_fence_at(fences, text, cut);
            let after_newline = text[..cut].ends_with('\n');
            let close = open.map_or(0, |fence| fence.width + usize::from(!after_newline));
            if used + close <= max_units {
                hard_cut = Some((cut, open));
                let at_line = match open {
                    None => after_newline && cut - 1 > start,
                    Some(fence) => after_newline && cut > fence.opener.end && cut < fence.closer,
                };
                if at_line || cut == text.len() {
                    line_cut = Some((cut, open));
                }
            }
        }
        let Some(character) = next else { break };
        used += unit.weight(character);
        if used > max_units {
            break;
        }
    }
    let (end, open) = line_cut
        .or(hard_cut)
        .unwrap_or((ceil_boundary(text, start + 1), None));
    if end < text.len()
        && let Some(fence) = fences
            .iter()
            .find(|fence| start < fence.opener.start && fence.opener.start < end && end < fence.end)
        && !text[start..fence.opener.start].trim().is_empty()
        && units(&text[fence.opener.start..fence.end], unit) <= max_units
    {
        return (fence.opener.start, None);
    }
    (end, open)
}

pub(crate) fn reconnect_delay(failures: u32) -> Duration {
    let step = BASE_RECONNECT_DELAY.saturating_mul(1_u32 << failures.min(MAX_RECONNECT_DOUBLINGS));
    step.min(MAX_RECONNECT_DELAY)
        .saturating_add(Duration::from_millis(jitter_below(RECONNECT_JITTER_MS)))
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetryAfter {
    pub wait: Duration,
    pub capped: bool,
}

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

    pub(crate) fn insert_if_absent(
        &mut self,
        key: &str,
        accept: impl FnOnce() -> bool,
    ) -> Result<bool, ()> {
        if self.seen.contains(key) {
            return Ok(false);
        }
        if !accept() {
            return Err(());
        }
        let inserted = self.insert(key.to_owned());
        debug_assert!(inserted);
        Ok(true)
    }

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
}

pub(crate) fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

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
        SeenIds, TextUnit, TransportIdentity, asset_buffer, credential_client_from, is_stop_word,
        jitter_below, reconnect_delay, reserve_for_chunk, retry_after_from_body, split_message,
    };

    #[test]
    fn an_asset_buffer_never_grows_past_the_ceiling_it_is_read_under() {
        let limit = 8 * 1024 * 1024;
        let chunk = 64 * 1024;

        let mut body = asset_buffer(None, limit);
        while body.len() < limit {
            reserve_for_chunk(&mut body, chunk, limit);
            body.extend_from_slice(&vec![0_u8; chunk]);
        }

        assert_eq!(body.len(), limit);
        assert_eq!(
            body.capacity(),
            limit,
            "an undeclared {limit}-byte asset is held in {} bytes",
            body.capacity()
        );
    }

    #[test]
    fn a_declared_length_sizes_the_buffer_and_is_clamped_to_the_ceiling() {
        let limit = 8 * 1024 * 1024;

        assert_eq!(asset_buffer(Some(1_000), limit).capacity(), 1_000);
        assert_eq!(asset_buffer(None, limit).capacity(), 0);
        assert_eq!(
            asset_buffer(Some(u64::MAX), limit).capacity(),
            limit,
            "a declared length past the ceiling reserved more than the read may ever hold"
        );

        let mut body = asset_buffer(Some(1_000), limit);
        reserve_for_chunk(&mut body, 1_000, limit);
        body.extend_from_slice(&[0_u8; 1_000]);
        assert_eq!(body.capacity(), 1_000, "the declared length was not enough");
    }

    const AMBIENT_PROXY: &str = "http://127.0.0.1:9";

    fn proxied_builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(AMBIENT_PROXY).expect("a well-formed proxy uri"))
    }

    #[test]
    fn a_credential_client_ignores_ambient_proxy_configuration() {
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

    #[test]
    fn failed_acceptance_never_consumes_a_recent_delivery_id() {
        let mut seen = SeenIds::new(1024);
        assert!(matches!(seen.insert_if_absent("guid", || false), Err(())));
        assert!(matches!(seen.insert_if_absent("guid", || true), Ok(true)));
        assert!(matches!(
            seen.insert_if_absent("guid", || panic!("duplicate must not enqueue")),
            Ok(false)
        ));
    }

    #[test]
    fn seen_identifiers_evict_oldest_first() {
        let mut seen = SeenIds::new(2);
        assert!(seen.insert("a".to_owned()));
        assert!(seen.insert("b".to_owned()));
        assert!(!seen.insert("a".to_owned()), "a repeat is refused");

        assert!(seen.insert("c".to_owned()), "the ring accepts a third");
        assert!(seen.insert("a".to_owned()), "the oldest was evicted");
        assert!(!seen.insert("c".to_owned()), "the newest was retained");
    }

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

    fn fenced(opener: &str, closer: &str, lines: usize, width: usize) -> String {
        let mut block = format!("{opener}\n");
        for line in 0..lines {
            block.push_str(&format!("{line:04} {}\n", "x".repeat(width)));
        }
        block.push_str(closer);
        block.push('\n');
        block
    }

    fn assert_fits(chunks: &[String], max_units: usize, unit: TextUnit) {
        for chunk in chunks {
            assert!(!chunk.is_empty(), "no service accepts an empty post");
            let units: usize = chunk.chars().map(|c| unit.weight(c)).sum();
            assert!(
                units <= max_units,
                "a chunk of {units} units exceeds {max_units}"
            );
        }
    }

    fn rejoined(chunks: &[String], close: &str, reopen: &str) -> String {
        let last = chunks.len() - 1;
        chunks
            .iter()
            .enumerate()
            .map(|(index, chunk)| {
                let chunk = if index > 0 {
                    chunk
                        .strip_prefix(reopen)
                        .expect("a continuation reopens its fence")
                } else {
                    chunk
                };
                if index < last {
                    chunk.strip_suffix(close).expect("a cut block is closed")
                } else {
                    chunk
                }
            })
            .collect()
    }

    #[test]
    fn a_block_straddling_the_discord_limit_moves_whole_to_the_next_message() {
        let mut text = String::new();
        for (index, lines) in [(0, 8), (1, 8), (2, 16), (3, 9)] {
            text.push_str(&format!("Step {index} explains the next block.\n\n"));
            text.push_str(&fenced(&format!("```rust{index}"), "```", lines, 60));
            text.push('\n');
        }
        let third = text.find("```rust2").expect("third block");
        let third_end = third + text[third..].find("\n```\n").expect("third closer") + 5;
        assert!(
            third < 2_000 && third_end > 2_000,
            "the third block straddles the limit"
        );

        let chunks = split_message(&text, 2_000, TextUnit::Utf16);
        assert_fits(&chunks, 2_000, TextUnit::Utf16);
        assert_eq!(chunks.len(), 2);
        assert!(
            chunks[1].starts_with("```rust2\n"),
            "the block opens the next message"
        );
        assert!(chunks[0].ends_with("Step 2 explains the next block.\n\n"));
        assert_eq!(
            chunks.concat(),
            text,
            "a block that fits is moved, never rewritten"
        );
    }

    #[test]
    fn a_block_longer_than_a_chunk_is_closed_and_reopened_with_its_info_string() {
        let text = format!("Here it is:\n{}Done.\n", fenced("```python", "```", 60, 50));
        assert!(text.len() > 3_000);

        let chunks = split_message(&text, 2_000, TextUnit::Utf16);
        assert_fits(&chunks, 2_000, TextUnit::Utf16);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].starts_with("Here it is:\n```python\n"));
        assert!(
            chunks[0].ends_with("x\n```"),
            "the cut is at a line inside the block"
        );
        assert!(chunks[1].starts_with("```python\n0"));
        assert!(chunks[1].ends_with("x\n```\nDone.\n"));
        assert_eq!(rejoined(&chunks, "```", "```python\n"), text);
    }

    #[test]
    fn a_tilde_fence_is_closed_and_reopened_with_tildes() {
        let text = fenced("~~~ sh", "~~~", 60, 50);
        let chunks = split_message(&text, 2_000, TextUnit::Utf16);
        assert_fits(&chunks, 2_000, TextUnit::Utf16);
        assert!(chunks[0].ends_with("\n~~~"));
        assert!(chunks[1].starts_with("~~~ sh\n"));
        assert_eq!(rejoined(&chunks, "~~~", "~~~ sh\n"), text);
    }

    #[test]
    fn three_backticks_inside_a_four_backtick_fence_do_not_close_it() {
        let mut inner = fenced("```rust", "```", 30, 50);
        inner.push_str(&inner.clone());
        let text = format!("````md\n{inner}````\n");

        let chunks = split_message(&text, 2_000, TextUnit::Utf16);
        assert_fits(&chunks, 2_000, TextUnit::Utf16);
        assert!(chunks.len() > 1);
        assert!(
            chunks[0].ends_with("\n````"),
            "the outer fence is the one closed"
        );
        assert!(chunks[1].starts_with("````md\n"));
        assert_eq!(rejoined(&chunks, "````", "````md\n"), text);
    }

    #[test]
    fn an_unterminated_fence_runs_to_the_end_of_the_text() {
        let mut text = fenced("```", "```", 60, 50);
        text.truncate(text.len() - "```\n".len());

        let chunks = split_message(&text, 2_000, TextUnit::Utf16);
        assert_fits(&chunks, 2_000, TextUnit::Utf16);
        assert!(chunks[0].ends_with("x\n```"));
        assert!(chunks[1].starts_with("```\n"));
        assert!(
            chunks[1].ends_with("x\n"),
            "the last chunk stays as unterminated as the answer"
        );
        assert_eq!(rejoined(&chunks, "```", "```\n"), text);

        let short = format!(
            "{}\n```\n{}",
            "intro ".repeat(300),
            "let x = 1;\n".repeat(30)
        );
        let chunks = split_message(&short, 2_000, TextUnit::Utf16);
        assert_eq!(chunks.len(), 2);
        assert!(
            chunks[1].starts_with("```\n"),
            "an unterminated block that fits moves whole"
        );
    }

    #[test]
    fn crlf_lines_are_cut_after_the_line_feed_and_reopen_with_their_ending() {
        let text = fenced("```rust", "```", 60, 50).replace('\n', "\r\n");
        let chunks = split_message(&text, 2_000, TextUnit::Utf16);
        assert_fits(&chunks, 2_000, TextUnit::Utf16);
        assert!(chunks[0].ends_with("x\r\n```"));
        assert!(chunks[1].starts_with("```rust\r\n0"));
        assert_eq!(rejoined(&chunks, "```", "```rust\r\n"), text);
    }

    #[test]
    fn fence_aware_chunks_respect_the_telegram_and_whatsapp_limit_in_their_units() {
        let text = format!(
            "🦀 intro\n{}",
            fenced("```diagram", "```", 160, 40).replace('x', "─")
        );
        for unit in [TextUnit::Utf16, TextUnit::Scalar] {
            let chunks = split_message(&text, 4_096, unit);
            assert_fits(&chunks, 4_096, unit);
            assert_eq!(chunks.len(), 2);
            assert!(chunks[1].starts_with("```diagram\n"));
            assert_eq!(rejoined(&chunks, "```", "```diagram\n"), text);
        }
    }

    #[test]
    fn a_line_longer_than_a_chunk_is_hard_split_inside_its_fence() {
        let text = format!("```\n{}\n```\n", "y".repeat(5_000));
        let chunks = split_message(&text, 2_000, TextUnit::Utf16);
        assert_fits(&chunks, 2_000, TextUnit::Utf16);
        assert_eq!(chunks.len(), 3);
        assert!(chunks[..2].iter().all(|chunk| chunk.ends_with("y\n```")));
        assert_eq!(rejoined(&chunks, "\n```", "```\n"), text);

        let mixed = format!(
            "```js\n{}\n```\n🦀\n~~~~\n{}\n",
            "🦀".repeat(70),
            "z".repeat(90)
        );
        for max_units in 2..120 {
            for unit in [TextUnit::Utf16, TextUnit::Scalar] {
                assert_fits(&split_message(&mixed, max_units, unit), max_units, unit);
            }
        }
    }
}
