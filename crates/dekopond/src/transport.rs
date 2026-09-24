//! The subject is trusted only as service-authenticated routing metadata, never as content; message
//! text is always untrusted and bounded before reaching a model.

use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use dekopon_agent::attachment::GeneratedImage;
use dekopon_broker_protocol::{ChatTransportKind, Conversation};
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
mod hydration;
pub(crate) mod local;
pub(crate) mod recovery;
pub(crate) mod slack;
pub(crate) mod telegram;
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
    pub message_id: String,
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
    pub late_photos: Option<crate::session::LatePhotoReceipt>,
}

/// drop.reason is declared on this span rather than by the transport that records it, because
/// recording a field a span never declared is a silent no-op.
pub(crate) fn receive_span(kind: ChatTransportKind) -> tracing::Span {
    tracing::info_span!(
        "transport.receive",
        transport.kind = %kind,
        message.id = tracing::field::Empty,
        drop.reason = tracing::field::Empty,
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

#[derive(Clone, Debug)]
pub(crate) struct StreamedText {
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
    ) -> Result<(), TransportError>;
}

#[async_trait]
pub(crate) trait InboundReaction: Send + Sync {
    async fn set(&self, target: &LivenessTarget, present: bool) -> Result<(), TransportError>;
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

/// max_units is counted in the given unit because chat services disagree on what they count, and a
/// chunk measured in the wrong unit is rejected whole rather than trimmed.
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

    pub(crate) fn remove(&mut self, key: &str) {
        if self.seen.remove(key) {
            self.order.retain(|candidate| candidate != key);
        }
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
}
