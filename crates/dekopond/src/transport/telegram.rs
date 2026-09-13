//! Telegram long polling, where the poll *is* the wakeup and the offset *is* the acknowledgment.
//!
//! `getUpdates` blocks server-side for up to fifty seconds and returns as soon as anything arrives,
//! so waiting costs one idle connection rather than a poll loop. Advancing `offset` past an update
//! is what tells Telegram it was handled; there is no separate ack and therefore no ack-before-work
//! problem the way Socket Mode has one.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use async_trait::async_trait;
use dekopon_agent::{CancelVia, attachment::GeneratedImage};
use dekopon_broker_protocol::ChatTransportKind;
use dekopon_core::{ExternalSubject, Redacted};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tracing::{Instrument as _, Span};

use crate::{
    asset::{AssetSourceRef, PendingAsset},
    config::{LivenessMode, LivenessSettings},
    progress::ProgressText,
    transport::{
        AckToken, AssetFetcher, CancelButton, CancelPress, CancelRequest, ChatDriver,
        ChatTransport, ConversationKind, InboundMessage, InboundReaction, LivenessTarget,
        MessageRef, OutboundReply, ProgressLimits, ProgressMessage, ReplyTarget, StreamLimits,
        StreamedText, TextStream, TextUnit, TransportError, TransportEvent, TransportIdentity,
        TypingLease, bound_inbound, credential_client, floor_boundary, receive_span,
        reconnect_delay, retry_after_from_body, split_message,
    },
};

/// Ceiling on one attachment's file name.
const MAX_ATTACHMENT_NAME_BYTES: usize = 128;
/// Telegram's `sendMessage` text ceiling, which the Bot API counts in UTF-16 code units.
///
/// Below the gateway's own 8 KiB outbound bound, so a normal long answer is two or three posts
/// rather than a rejected one. Splitting is shared with Discord's 2,000-unit ceiling through
/// [`crate::transport::split_message`], newline preference and all.
const MAX_MESSAGE_CHARS: usize = 4_096;
/// Ceiling on one streamed render, in Unicode scalar values.
///
/// Not [`MAX_MESSAGE_CHARS`], because the two count different things: the policy spends
/// [`StreamLimits::max_chars`] as a `char` count, while [`MAX_MESSAGE_CHARS`] is UTF-16 code
/// units and a scalar outside the BMP costs two of them. The ninety-six units of headroom pay for
/// that difference: a cut render is 4,000 scalars plus the one-scalar marker, which is inside
/// Telegram's ceiling while at most ninety-five of those scalars are astral.
///
/// The worst case past that is worth naming rather than implying. A render the policy cut is
/// still marked, because [`bounded`] brings it back under the ceiling and the marker is appended
/// after that. A render the policy did *not* cut — 4,000 scalars or fewer, but more than 4,096
/// units of them — is cut by [`bounded`] alone, and that cut goes out unmarked.
const MAX_STREAM_CHARS: usize = 4_000;
/// Telegram's `sendPhoto` caption ceiling.
const MAX_PHOTO_CAPTION_CHARS: usize = 1_024;

/// Server-side wait per poll, in seconds. Telegram's own ceiling is fifty.
const POLL_SECONDS: u64 = 50;
/// Client deadline, generously above the server wait so a normal empty poll is not an error.
const POLL_TIMEOUT: Duration = Duration::from_secs(POLL_SECONDS + 20);
/// Deadline on one cosmetic call. Liveness decorates an answer; it never delays one.
const LIVENESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the `typing` action is re-sent. Telegram expires it after five seconds and offers no
/// way to clear it early, so the lease is renewed under that and the final answer ends it.
const TYPING_RENEW_INTERVAL: Duration = Duration::from_secs(4);
/// Ceiling on a server-directed liveness cooldown.
const MAX_LIVENESS_COOLDOWN: Duration = Duration::from_secs(300);
/// Floor between two edits of one message.
///
/// The Bot API's per-chat flood limit tolerates roughly one message every three seconds
/// indefinitely; the policy coalesces to this rather than learning it as a `retry_after`.
const MIN_EDIT_INTERVAL: Duration = Duration::from_secs(3);
/// The reaction that says an inbound message was seen.
///
/// Not the `:tangerine:` Slack wears: `setMessageReaction` accepts only emoji from the server's own
/// available-reactions list for the chat, and the tangerine is not on it. A reaction Telegram
/// refuses is cosmetic, and its refusal surfaces as the Bot API's own description.
const WORKING_REACTION: &str = "👀";
/// What a cancel button puts in `callback_data`, ahead of the conversation identifier.
const CANCEL_CALLBACK_PREFIX: &str = "stop:";
/// The Bot API description for an edit whose text and markup already match what is on screen.
const NOT_MODIFIED: &str = "message is not modified";
/// What a stream render ends in when the answer outgrew the message it is being written into.
///
/// Telegram has no "show more" and an edited message simply stops, so a cut is invisible without
/// this. One scalar rather than a sentence because the room it needs is room the answer gives up:
/// it is counted by [`MAX_STREAM_CHARS`] and, on a render already at the Bot API's ceiling, taken
/// back out of the text.
const TRUNCATION_MARKER: &str = "\u{2026}";

pub(crate) struct TelegramTransport {
    name: String,
    http: reqwest::Client,
    /// Also the reader's URL builder: the bot token lives in every Bot API path, and one place
    /// that formats it is one place that calls [`Redacted::expose`].
    driver: Arc<TelegramDriver>,
    offset: i64,
    pending: VecDeque<TransportEvent>,
    failures: u32,
    /// `liveness.mode: native` — an inbound message carries coordinates for transient signals.
    ///
    /// One decision rather than the whole block: what the Bot API can render is fixed, and
    /// progress, stream, cancel button, keep-alive, and templates are read by the policy that
    /// drives the driver. Withholding the coordinates is how `off` keeps this transport
    /// reply-only.
    native: bool,
}

/// What one polled update turned into.
///
/// Both payloads are boxed for the same reason: an update is read one at a time and neither of
/// these belongs on the stack of the poll loop.
enum Routed {
    /// Ordinary routable traffic.
    Message(Box<InboundMessage>),
    /// A cancel button press.
    Cancel(Box<StopPress>),
}

/// One press, split in two because the halves go to different places: the acknowledgment Telegram
/// is waiting for leaves from the reader, and the request the gateway acts on leaves through the
/// channel after it.
struct StopPress {
    press: CancelPress,
    request: CancelRequest,
}

/// The conversation one chat and optional forum topic belong to.
///
/// One definition because three callers have to agree or turns are misfiled: the inbound message,
/// the cancel button's `callback_data`, and the press that checks that untrusted payload against
/// the envelope it arrived in.
fn conversation_id(chat_id: i64, message_thread_id: Option<i64>) -> String {
    message_thread_id.map_or_else(
        || chat_id.to_string(),
        |topic| format!("{chat_id}:topic:{topic}"),
    )
}

impl TelegramTransport {
    /// Takes the bot token *value*; the caller resolves it from the named environment variable.
    pub(crate) fn new(
        name: String,
        endpoint: String,
        token: String,
        liveness: LivenessSettings,
    ) -> Result<Self, TransportError> {
        let http = credential_client(POLL_TIMEOUT)
            .build()
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        Ok(Self {
            name,
            http: http.clone(),
            driver: Arc::new(TelegramDriver {
                endpoint,
                token: Redacted::new(token),
                http,
                liveness_cooldown_until: std::sync::Mutex::new(None),
            }),
            offset: 0,
            pending: VecDeque::new(),
            failures: 0,
            native: liveness.mode == LivenessMode::Native,
        })
    }

    /// Runs one long-poll cycle and hands back what it failed with.
    ///
    /// Test-only: the daemon reaches [`Self::poll`] through [`Self::next`], which logs the
    /// category and retries, so a poll failure is otherwise never returned to anything that
    /// could render it.
    #[cfg(test)]
    pub(crate) async fn poll_once(&mut self) -> Result<(), TransportError> {
        self.poll().await
    }

    async fn poll(&mut self) -> Result<(), TransportError> {
        let url = format!(
            "{}?timeout={POLL_SECONDS}&offset={}",
            self.driver.url("getUpdates"),
            self.offset
        );
        let body = decode(self.http.get(url).send().await.map_err(request_failed)?).await?;
        let updates = body["result"]
            .as_array()
            .ok_or(TransportError::Response)?
            .clone();
        for update in updates {
            let Some(update_id) = update["update_id"].as_i64() else {
                continue;
            };
            // One span per poll item, opened before the update is read, so the offset advance that
            // acknowledges it and the decision not to route it are both inside the item's trace.
            let received = receive_span(ChatTransportKind::Telegram);
            let routed = received.in_scope(|| {
                // Advance first, unconditionally. An update this daemon chooses not to route still
                // has to be acknowledged, or the next poll returns it forever.
                self.offset = self.offset.max(update_id + 1);
                if update["callback_query"].is_object() {
                    return Ok(self.pressed(&update["callback_query"], &received));
                }
                self.routable(&update["message"], &received)
                    .map(|message| message.map(|message| Routed::Message(Box::new(message))))
            })?;
            match routed {
                Some(Routed::Message(message)) => {
                    received.record("message.id", message.message_id.as_str());
                    self.pending.push_back(TransportEvent::Message(message));
                }
                Some(Routed::Cancel(pressed)) => {
                    let StopPress { press, request } = *pressed;
                    // Answered here, inside the reader, before the request reaches the channel:
                    // Telegram spins the button's progress circle on the presser's client until
                    // this call lands, and the run being stopped takes as long as it takes to
                    // notice. An acknowledgment that waited for either would be a stuck button.
                    let driver = Arc::clone(&self.driver);
                    if let Err(error) = driver.ack(&press).instrument(received.clone()).await {
                        tracing::warn!(
                            event = "gateway_transport_ack_failed",
                            transport = %self.name,
                            category = error.category()
                        );
                    }
                    self.pending
                        .push_back(TransportEvent::CancelRequested(request));
                }
                None => {}
            }
        }
        self.failures = 0;
        Ok(())
    }

    fn routable(
        &self,
        message: &Value,
        received: &Span,
    ) -> Result<Option<InboundMessage>, TransportError> {
        let (Some(from), Some(chat), Some(message_id)) = (
            message["from"].as_object(),
            message["chat"].as_object(),
            message["message_id"].as_i64(),
        ) else {
            return Ok(None);
        };
        // A message carrying a photo or a document puts its words in `caption`; only a plain
        // message has `text`. Reading just `text` is what made an upload invisible here.
        let text = message["text"]
            .as_str()
            .or_else(|| message["caption"].as_str())
            .unwrap_or_default();
        let assets = Self::pending_assets(message);
        if text.trim().is_empty() && assets.is_empty() {
            return Ok(None);
        }
        // Loop prevention: a bot's own posts and every other bot's come back marked.
        if from.get("is_bot").and_then(Value::as_bool) == Some(true) {
            return Ok(None);
        }
        let Some(user) = from.get("id").and_then(Value::as_i64) else {
            return Ok(None);
        };
        let Some(chat_id) = chat.get("id").and_then(Value::as_i64) else {
            return Ok(None);
        };
        // Every private chat is a direct message; a group is a channel, and the daemon separately
        // requires the bot to be addressed there.
        let conversation = match chat.get("type").and_then(Value::as_str) {
            Some("private") => ConversationKind::DirectMessage,
            _ => ConversationKind::Channel(chat_id.to_string()),
        };
        let reply_to = match conversation {
            ConversationKind::DirectMessage => None,
            ConversationKind::Channel(_) => Some(message_id),
        };
        // Plain chats remain one conversation. Forum topics and private-chat topic mode carry a
        // positive service-native thread identifier, which must scope history, admission, replies,
        // durable memory, and transient liveness together.
        let message_thread_id = message["message_thread_id"].as_i64();
        if message_thread_id.is_some_and(|id| id <= 0) {
            return Err(TransportError::Response);
        }
        let conversation_id = conversation_id(chat_id, message_thread_id);

        Ok(Some(InboundMessage {
            transport: self.name.clone(),
            transport_kind: ChatTransportKind::Telegram,
            subject: ExternalSubject::telegram(&user.to_string())
                .map_err(TransportError::Subject)?,
            channel: chat_id.to_string(),
            thread: message_thread_id.map(|id| id.to_string()),
            conversation_id,
            message_id: message_id.to_string(),
            text: bound_inbound(text),
            assets,
            conversation,
            // Telegram's message text carries `@handle`, so the shared fallback checks it.
            addressed: None,
            thread_continuation: None,
            reply: ReplyTarget::Telegram {
                chat_id,
                reply_to,
                message_thread_id,
            },
            liveness: self.native.then_some(LivenessTarget::Telegram {
                chat_id,
                message_thread_id,
                message_id,
            }),
            receive_span: received.clone(),
        }))
    }

    /// Reads one `callback_query`, which is the only inline-keyboard press this bot draws.
    ///
    /// `callback_data` comes back verbatim from whoever pressed the button, so it is untrusted and
    /// says only which conversation the press *claims*. The presser comes from the callback
    /// envelope, which Telegram authenticated, and the claim is accepted only when it matches the
    /// chat and topic that same envelope names. Whether that presser may stop this conversation's
    /// run is the gateway's question rather than this reader's: it holds the subject that asked.
    fn pressed(&self, callback: &Value, received: &Span) -> Option<Routed> {
        let ignored = |reason: &'static str| -> Option<Routed> {
            tracing::debug!(
                event = "gateway_transport_press_ignored",
                transport = %self.name,
                reason
            );
            None
        };
        let (Some(callback_query_id), Some(from), Some(message)) = (
            callback["id"].as_str(),
            callback["from"].as_object(),
            callback["message"].as_object(),
        ) else {
            return ignored("malformed");
        };
        // Loop prevention, as on the message path: a bot's own presses come back marked.
        if from.get("is_bot").and_then(Value::as_bool) == Some(true) {
            return ignored("bot");
        }
        let (Some(user), Some(chat_id), Some(message_id), Some(data)) = (
            from.get("id").and_then(Value::as_i64),
            message
                .get("chat")
                .and_then(Value::as_object)
                .and_then(|chat| chat.get("id"))
                .and_then(Value::as_i64),
            message.get("message_id").and_then(Value::as_i64),
            callback["data"].as_str(),
        ) else {
            return ignored("malformed");
        };
        let Some(claimed) = data.strip_prefix(CANCEL_CALLBACK_PREFIX) else {
            return ignored("malformed");
        };
        let message_thread_id = message.get("message_thread_id").and_then(Value::as_i64);
        if message_thread_id.is_some_and(|id| id <= 0) {
            return ignored("malformed");
        }
        let conversation_id = conversation_id(chat_id, message_thread_id);
        if claimed != conversation_id {
            return ignored("conversation-mismatch");
        }
        let Ok(subject) = ExternalSubject::telegram(&user.to_string()) else {
            return ignored("subject");
        };
        received.record("message.id", message_id.to_string().as_str());
        Some(Routed::Cancel(Box::new(StopPress {
            press: CancelPress {
                target: LivenessTarget::Telegram {
                    chat_id,
                    message_thread_id,
                    message_id,
                },
                subject: subject.canonical(),
                ack: AckToken::Telegram {
                    callback_query_id: callback_query_id.to_owned(),
                },
            },
            request: CancelRequest {
                transport: self.name.clone(),
                conversation_id,
                subject: subject.canonical(),
                via: CancelVia::Button,
            },
        })))
    }

    /// Describes the photo or document on one message so the session can number it.
    ///
    /// A photo arrives as an array of the same image at several sizes, smallest first. The largest is
    /// the one worth showing a model — the small ones are thumbnails, and a model asked to read text in
    /// a screenshot cannot read a 90-pixel-wide copy of it.
    ///
    /// Telegram reports no media type for a photo, so one is inferred: the Bot API re-encodes every
    /// photo to JPEG, while a file sent as a *document* keeps its own bytes and its own declared type.
    fn pending_assets(message: &Value) -> Vec<PendingAsset> {
        if let Some(photo) = message["photo"].as_array()
            && let Some(largest) = photo
                .iter()
                .max_by_key(|size| size["file_size"].as_u64().unwrap_or_default())
            && let Some(file_id) = largest["file_id"].as_str()
        {
            return vec![PendingAsset {
                name: "photo.jpg".to_owned(),
                mime: "image/jpeg".to_owned(),
                size: largest["file_size"].as_u64().unwrap_or_default(),
                source: Some(AssetSourceRef::Telegram {
                    file_id: file_id.to_owned(),
                }),
            }];
        }
        let document = &message["document"];
        if let Some(file_id) = document["file_id"].as_str() {
            let name = document["file_name"].as_str().unwrap_or("attachment");
            return vec![PendingAsset {
                name: name[..floor_boundary(name, MAX_ATTACHMENT_NAME_BYTES)].to_owned(),
                mime: document["mime_type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                size: document["file_size"].as_u64().unwrap_or_default(),
                source: Some(AssetSourceRef::Telegram {
                    file_id: file_id.to_owned(),
                }),
            }];
        }
        Vec::new()
    }
}

impl ChatTransport for TelegramTransport {
    fn name(&self) -> &str {
        &self.name
    }

    fn connect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>> {
        Box::pin(async move {
            let body = decode(
                self.http
                    .get(self.driver.url("getMe"))
                    .send()
                    .await
                    .map_err(request_failed)?,
            )
            .await?;
            let handle = body["result"]["username"]
                .as_str()
                .ok_or(TransportError::Response)?
                .to_owned();
            Ok(TransportIdentity {
                user_id: None,
                handle: Some(handle),
            })
        })
    }

    fn next(&mut self) -> BoxFuture<'_, Result<TransportEvent, TransportError>> {
        Box::pin(async move {
            loop {
                if let Some(event) = self.pending.pop_front() {
                    return Ok(event);
                }
                if let Err(error) = self.poll().await {
                    self.failures = self.failures.saturating_add(1);
                    tracing::warn!(
                        event = "gateway_transport_poll_failed",
                        transport = %self.name,
                        category = error.category()
                    );
                    tokio::time::sleep(reconnect_delay(self.failures)).await;
                }
            }
        })
    }

    /// One handle for replying and for every liveness surface the Bot API has.
    ///
    /// What the capability objects advertise is what Telegram implements, not what this deployment
    /// asked for: `liveness.mode: off` withholds the target on the inbound message instead, so the
    /// policy has nothing to render against and the transport is reply-only exactly as before.
    fn driver(&self) -> Arc<dyn ChatDriver> {
        Arc::clone(&self.driver) as Arc<dyn ChatDriver>
    }

    fn asset_fetcher(&self) -> Option<Arc<dyn AssetFetcher>> {
        Some(Arc::clone(&self.driver) as Arc<dyn AssetFetcher>)
    }
}

/// Resolving a `file_id` and downloading the bytes, which the Bot API splits into two calls.
///
/// Telegram hands out a handle rather than a URL. `getFile` turns it into a path valid for roughly
/// an hour, and the bytes live under a different prefix — `/file/bot<token>/<path>` rather than
/// `/bot<token>/<method>`. Both carry the token in the URL, which is the Bot API's own design and
/// the reason this transport never logs one.
impl AssetFetcher for TelegramDriver {
    fn fetch(
        &self,
        source: &AssetSourceRef,
        max_bytes: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, TransportError>> {
        let AssetSourceRef::Telegram { file_id } = source else {
            // A reference belonging to another transport is a routing mistake, not a fetch failure.
            return Box::pin(async { Err(TransportError::Response) });
        };
        let file_id = file_id.clone();
        Box::pin(async move {
            let described = decode(
                self.http
                    .get(self.url("getFile"))
                    .query(&[("file_id", file_id.as_str())])
                    .send()
                    .await
                    .map_err(request_failed)?,
            )
            .await?;
            let path = described["result"]["file_path"]
                .as_str()
                .ok_or(TransportError::Response)?;
            let mut response = self
                .http
                .get(format!(
                    "{}/file/bot{}/{path}",
                    self.endpoint,
                    self.token.expose()
                ))
                .send()
                .await
                .map_err(request_failed)?;
            if !response.status().is_success() {
                return Err(TransportError::Service {
                    code: response.status().as_u16().to_string(),
                });
            }
            // Streamed against the ceiling rather than buffered and measured afterwards, for the
            // same reason the Slack path is: a declared length is not a bound.
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(request_failed)? {
                if body.len().saturating_add(chunk.len()) as u64 > max_bytes {
                    return Err(TransportError::Service {
                        code: "asset-too-large".to_owned(),
                    });
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        })
    }
}

/// The answering and rendering half of the transport: replies, attachments, and every liveness
/// surface the Bot API offers.
pub(crate) struct TelegramDriver {
    endpoint: String,
    token: Redacted<String>,
    http: reqwest::Client,
    /// Until when the Bot API's own `retry_after` says this transport must stop calling.
    ///
    /// Cosmetic calls only. A reply is the session's one visible outcome and is never withheld for
    /// a rate limit a progress message earned.
    liveness_cooldown_until: std::sync::Mutex<Option<tokio::time::Instant>>,
}

#[async_trait]
impl TypingLease for TelegramDriver {
    fn renew_every(&self) -> Duration {
        TYPING_RENEW_INTERVAL
    }

    async fn renew(&self, target: &LivenessTarget) -> Result<(), TransportError> {
        let LivenessTarget::Telegram {
            chat_id,
            message_thread_id,
            ..
        } = target
        else {
            return Err(TransportError::Response);
        };
        let mut body = json!({ "chat_id": chat_id, "action": "typing" });
        if let Some(message_thread_id) = message_thread_id {
            body["message_thread_id"] = json!(message_thread_id);
        }
        self.liveness_call("sendChatAction", &body).await.map(drop)
    }
}

#[async_trait]
impl InboundReaction for TelegramDriver {
    /// Telegram replaces the bot's whole reaction set on each call, so an empty list is how one is
    /// taken back.
    async fn set(&self, target: &LivenessTarget, present: bool) -> Result<(), TransportError> {
        let LivenessTarget::Telegram {
            chat_id,
            message_id,
            ..
        } = target
        else {
            return Err(TransportError::Response);
        };
        let reaction = if present {
            json!([{ "type": "emoji", "emoji": WORKING_REACTION }])
        } else {
            json!([])
        };
        self.liveness_call(
            "setMessageReaction",
            &json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "reaction": reaction,
            }),
        )
        .await
        .map(drop)
    }
}

#[async_trait]
impl ProgressMessage for TelegramDriver {
    fn limits(&self) -> ProgressLimits {
        ProgressLimits {
            max_chars: MAX_MESSAGE_CHARS,
            min_edit_interval: MIN_EDIT_INTERVAL,
        }
    }

    async fn post(
        &self,
        target: &LivenessTarget,
        text: &ProgressText,
        cancel: bool,
    ) -> Result<MessageRef, TransportError> {
        self.post_text(target, bounded(text.as_str()), cancel).await
    }

    async fn edit(
        &self,
        message: &MessageRef,
        text: &ProgressText,
        cancel: bool,
    ) -> Result<(), TransportError> {
        self.edit_text(message, bounded(text.as_str()), cancel)
            .await
    }

    async fn delete(&self, message: &MessageRef) -> Result<(), TransportError> {
        let (chat_id, _, message_id) = addressed(message)?;
        self.liveness_call(
            "deleteMessage",
            &json!({ "chat_id": chat_id, "message_id": message_id }),
        )
        .await
        .map(drop)
    }

    async fn finalize(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError> {
        self.finalize_in_place(message, reply).await
    }
}

#[async_trait]
impl TextStream for TelegramDriver {
    fn limits(&self) -> StreamLimits {
        StreamLimits {
            min_interval: MIN_EDIT_INTERVAL,
            max_chars: MAX_STREAM_CHARS,
        }
    }

    /// Telegram has no append. A stream here is one message edited to the whole answer so far,
    /// which is why the policy hands over cumulative text rather than a delta.
    ///
    /// The policy cuts that text to [`StreamLimits::max_chars`] — scalars, which is why the cap
    /// advertised is [`MAX_STREAM_CHARS`] rather than the Bot API's own UTF-16 ceiling — and
    /// reports the cut in [`StreamedText::truncated`] without marking it, because what marks it is
    /// per surface. Here it is [`TRUNCATION_MARKER`] on the end. The headroom normally pays for
    /// it; where a render arrives at the ceiling anyway, the tail of the text gives up the room,
    /// because one unit past it is an edit the Bot API rejects whole.
    async fn show(
        &self,
        target: &LivenessTarget,
        message: Option<&MessageRef>,
        text: &StreamedText,
        cancel: bool,
    ) -> Result<MessageRef, TransportError> {
        let mut rendered = bounded(text.text.as_str());
        if text.truncated {
            // Terminates: the marker is one unit against a ceiling of four thousand and ninety-
            // six, so the condition is false long before the text runs out. A popped scalar frees
            // one unit, or two when it was an astral one.
            let marker = TRUNCATION_MARKER.encode_utf16().count();
            while rendered.encode_utf16().count() + marker > MAX_MESSAGE_CHARS {
                rendered.pop();
            }
            rendered.push_str(TRUNCATION_MARKER);
        }
        let Some(message) = message else {
            return self.post_text(target, rendered, cancel).await;
        };
        self.edit_text(message, rendered, cancel).await?;
        Ok(message.clone())
    }

    async fn finalize(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError> {
        self.finalize_in_place(message, reply).await
    }
}

#[async_trait]
impl CancelButton for TelegramDriver {
    async fn ack(&self, press: &CancelPress) -> Result<(), TransportError> {
        let AckToken::Telegram { callback_query_id } = &press.ack else {
            return Err(TransportError::Response);
        };
        self.liveness_call(
            "answerCallbackQuery",
            &json!({ "callback_query_id": callback_query_id, "text": "Stopping\u{2026}" }),
        )
        .await
        .map(drop)
    }
}

/// The liveness half of the driver: the calls that decorate an answer rather than deliver one.
impl TelegramDriver {
    fn url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.endpoint, self.token.expose())
    }

    /// One Bot API call made for liveness rather than for an answer.
    ///
    /// Three things separate it from the reply path, and all three matter. A two-second deadline
    /// instead of the poll client's seventy, because a progress edit that outlives the answer is
    /// worse than no progress edit. A server-directed cooldown this transport refuses inside,
    /// rather than re-earning a `retry_after` per call — and refuses *as a failure with a cause*,
    /// so the policy counts it and drops the rung instead of believing a call it never made. And
    /// the Bot API's own `description` on the error, which is how `message is not modified` reaches
    /// the one caller that has to read it as success.
    async fn liveness_call(&self, method: &str, body: &Value) -> Result<Value, TransportError> {
        if self
            .liveness_cooldown_until
            .lock()
            .expect("Telegram liveness cooldown")
            .is_some_and(|until| until > tokio::time::Instant::now())
        {
            return Err(TransportError::Service {
                code: "cooldown".to_owned(),
            });
        }
        #[allow(
            clippy::map_err_ignore,
            reason = "serializing a serde_json::Value cannot fail: it holds no non-string map \
                      keys and serde_json::Number rejects non-finite floats"
        )]
        let response = self
            .http
            .post(self.url(method))
            .header("content-type", "application/json")
            .body(serde_json::to_vec(body).map_err(|_| TransportError::Response)?)
            .timeout(LIVENESS_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(request_failed)?;
        let bytes = response.bytes().await.map_err(request_failed)?;
        let body =
            serde_json::from_slice::<Value>(&bytes).map_err(TransportError::MalformedResponse)?;
        if body["ok"] == Value::Bool(true) {
            *self
                .liveness_cooldown_until
                .lock()
                .expect("Telegram liveness cooldown") = None;
            return Ok(body);
        }
        if let Some(retry) = retry_after_from_body(&body["parameters"], MAX_LIVENESS_COOLDOWN) {
            *self
                .liveness_cooldown_until
                .lock()
                .expect("Telegram liveness cooldown") =
                Some(tokio::time::Instant::now() + retry.wait);
            return Err(TransportError::Service {
                code: "retry-after".to_owned(),
            });
        }
        Err(TransportError::Service {
            code: described(&body),
        })
    }

    /// Posts the message a progress or stream surface lives in.
    async fn post_text(
        &self,
        target: &LivenessTarget,
        text: String,
        cancel: bool,
    ) -> Result<MessageRef, TransportError> {
        let LivenessTarget::Telegram {
            chat_id,
            message_thread_id,
            ..
        } = target
        else {
            return Err(TransportError::Response);
        };
        let mut body = json!({
            "chat_id": chat_id,
            "text": text,
            "reply_markup": reply_markup(*chat_id, *message_thread_id, cancel),
        });
        if let Some(message_thread_id) = message_thread_id {
            body["message_thread_id"] = json!(message_thread_id);
        }
        let posted = self.liveness_call("sendMessage", &body).await?;
        let id = posted["result"]["message_id"]
            .as_i64()
            .filter(|id| *id > 0)
            .ok_or(TransportError::Response)?;
        Ok(MessageRef {
            target: target.clone(),
            id: id.to_string(),
        })
    }

    /// One `editMessageText`, treating "message is not modified" as the success it describes.
    async fn edit_text(
        &self,
        message: &MessageRef,
        text: String,
        cancel: bool,
    ) -> Result<(), TransportError> {
        let (chat_id, message_thread_id, message_id) = addressed(message)?;
        let body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
            "reply_markup": reply_markup(chat_id, message_thread_id, cancel),
        });
        match self.liveness_call("editMessageText", &body).await {
            Ok(_) => Ok(()),
            // Telegram answers `ok: false` when the text and the markup already match what is on
            // screen. The caller asked for exactly that state and it holds; calling it a failure
            // would trip the policy's breaker on a coalesced re-render of unchanged state.
            Err(TransportError::Service { ref code }) if code.contains(NOT_MODIFIED) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Turns the surface into the answer, where the answer fits inside it.
    ///
    /// `Err` is the policy's signal to delete this message and reply normally. That is the only
    /// path for an answer carrying attachments, which an edit cannot add, and for one past
    /// Telegram's single-message ceiling, which an edit cannot split.
    async fn finalize_in_place(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError> {
        if !reply.images.is_empty() || reply.text.encode_utf16().count() > MAX_MESSAGE_CHARS {
            return Err(TransportError::Service {
                code: "answer-does-not-fit".to_owned(),
            });
        }
        // Through the same splitter `reply` uses, so an empty answer reads the same here as it
        // would in a posted one rather than becoming a text the Bot API refuses.
        self.edit_text(message, bounded(&reply.text), false).await
    }
}

/// The chat, topic, and message one [`MessageRef`] names.
///
/// The one failure is a reference this transport did not mint, which is what
/// [`TransportError::Response`] says: a surface belonging to another chat service reached a
/// Telegram driver.
fn addressed(message: &MessageRef) -> Result<(i64, Option<i64>, i64), TransportError> {
    let LivenessTarget::Telegram {
        chat_id,
        message_thread_id,
        ..
    } = &message.target
    else {
        return Err(TransportError::Response);
    };
    let id = message
        .id
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or(TransportError::Response)?;
    Ok((*chat_id, *message_thread_id, id))
}

/// The inline keyboard one liveness post or edit carries.
///
/// `callback_data` names the conversation and nothing else: Telegram hands it back verbatim from
/// whoever pressed the button, so it is untrusted, and the reader checks it against the envelope
/// the press arrived in. An edit that omits `reply_markup` leaves the keyboard that is already
/// there, so "no button" has to be said out loud — which is what the empty keyboard is for, on the
/// finalize that turns a progress message into an answer as much as on a post that never had one.
///
/// The button's label is the gateway's own fixed word, never a template and never model text.
fn reply_markup(chat_id: i64, message_thread_id: Option<i64>, cancel: bool) -> Value {
    if !cancel {
        return json!({ "inline_keyboard": [] });
    }
    // The Bot API bounds `callback_data` at 64 bytes and rejects the whole post over it, but the
    // widest identifier two `i64`s can spell is well inside that, so there is no branch here to
    // reach: `the_cancel_payload_always_fits_the_bot_api_ceiling` pins the arithmetic instead.
    json!({
        "inline_keyboard": [[{
            "text": "Stop",
            "callback_data": format!(
                "{CANCEL_CALLBACK_PREFIX}{}",
                conversation_id(chat_id, message_thread_id)
            ),
        }]]
    })
}

/// The head of one text at the Bot API's own ceiling, through the splitter the reply path uses so
/// the two cannot disagree about what fits.
///
/// A liveness surface is one message: the rest of an over-long render is dropped rather than posted
/// beside it.
fn bounded(text: &str) -> String {
    split_message(text, MAX_MESSAGE_CHARS, TextUnit::Utf16)
        .into_iter()
        .next()
        .unwrap_or_default()
}

#[async_trait]
impl ChatDriver for TelegramDriver {
    async fn reply(
        &self,
        target: &ReplyTarget,
        reply: OutboundReply,
    ) -> Result<(), TransportError> {
        let ReplyTarget::Telegram {
            chat_id,
            reply_to,
            message_thread_id,
        } = target
        else {
            return Err(TransportError::Response);
        };
        let (chat_id, reply_to, message_thread_id) = (*chat_id, *reply_to, *message_thread_id);
        let OutboundReply { text, images } = reply;
        if !images.is_empty() {
            // One `sendPhoto` per attachment; Telegram has no multi-attachment message that also
            // carries a caption the way a person expects to read it.
            let caption_fits = text.encode_utf16().count() <= MAX_PHOTO_CAPTION_CHARS;
            let mut accepted = false;
            for (index, image) in images.into_iter().enumerate() {
                let caption = (index == 0 && caption_fits).then_some(text.as_str());
                match self
                    .send_photo(chat_id, reply_to, message_thread_id, caption, image, index)
                    .await
                {
                    Ok(()) => accepted = true,
                    // One attachment already reached the chat, so this is a reply that arrived
                    // in part rather than one that never arrived — the same distinction the text
                    // chunk loop below makes.
                    Err(_) if accepted => return Err(TransportError::PartialDelivery),
                    Err(error) => return Err(error),
                }
            }
            if caption_fits {
                return accepted.then_some(()).ok_or(TransportError::Response);
            }
            return self
                .send_text_chunks(chat_id, reply_to, message_thread_id, &text, true)
                .await;
        }
        self.send_text_chunks(chat_id, reply_to, message_thread_id, &text, false)
            .await
    }

    fn typing(&self) -> Option<&dyn TypingLease> {
        Some(self)
    }

    fn progress(&self) -> Option<&dyn ProgressMessage> {
        Some(self)
    }

    fn stream(&self) -> Option<&dyn TextStream> {
        Some(self)
    }

    fn reaction(&self) -> Option<&dyn InboundReaction> {
        Some(self)
    }

    fn cancel_button(&self) -> Option<&dyn CancelButton> {
        Some(self)
    }
}

impl TelegramDriver {
    async fn send_text_chunks(
        &self,
        chat_id: i64,
        reply_to: Option<i64>,
        message_thread_id: Option<i64>,
        text: &str,
        prior_accepted: bool,
    ) -> Result<(), TransportError> {
        // `accepted` starts from the attachments this reply already posted and decides partial
        // delivery; `delivered` is about this call alone and decides whether anything went out.
        let mut accepted = prior_accepted;
        let mut delivered = false;
        for (index, chunk) in split_message(text, MAX_MESSAGE_CHARS, TextUnit::Utf16)
            .into_iter()
            .enumerate()
        {
            let first_text_reply = (!prior_accepted && index == 0)
                .then_some(reply_to)
                .flatten();
            match self
                .send_text(chat_id, first_text_reply, message_thread_id, chunk)
                .await
            {
                Ok(()) => {
                    accepted = true;
                    delivered = true;
                }
                Err(_) if accepted => return Err(TransportError::PartialDelivery),
                Err(error) => return Err(error),
            }
        }
        delivered.then_some(()).ok_or(TransportError::Response)
    }

    async fn send_text(
        &self,
        chat_id: i64,
        reply_to: Option<i64>,
        message_thread_id: Option<i64>,
        text: String,
    ) -> Result<(), TransportError> {
        let mut body = json!({ "chat_id": chat_id, "text": text });
        if let Some(reply_to) = reply_to {
            body["reply_to_message_id"] = json!(reply_to);
        }
        if let Some(message_thread_id) = message_thread_id {
            body["message_thread_id"] = json!(message_thread_id);
        }
        #[allow(
            clippy::map_err_ignore,
            reason = "serializing a serde_json::Value cannot fail: it holds no non-string map keys \
                      and serde_json::Number rejects non-finite floats"
        )]
        let response = self
            .http
            .post(self.url("sendMessage"))
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body).map_err(|_| TransportError::Response)?)
            .send()
            .await
            .map_err(request_failed)?;
        let response = decode(response).await?;
        let result = response["result"]
            .as_object()
            .ok_or(TransportError::Response)?;
        let posted = result
            .get("message_id")
            .and_then(Value::as_i64)
            .is_some_and(|id| id > 0);
        let response_chat = result
            .get("chat")
            .and_then(Value::as_object)
            .and_then(|chat| chat.get("id"))
            .and_then(Value::as_i64)
            .ok_or(TransportError::Response)?;
        let response_thread = result.get("message_thread_id").and_then(Value::as_i64);
        if !posted || response_chat != chat_id || response_thread != message_thread_id {
            return Err(TransportError::Response);
        }
        Ok(())
    }

    async fn send_photo(
        &self,
        chat_id: i64,
        reply_to: Option<i64>,
        message_thread_id: Option<i64>,
        caption: Option<&str>,
        image: GeneratedImage,
        index: usize,
    ) -> Result<(), TransportError> {
        let filename = image.filename(index);
        #[allow(
            clippy::map_err_ignore,
            reason = "mime_str only rejects strings that are not a media type, and this one is the \
                      literal above it"
        )]
        let part = reqwest::multipart::Part::bytes(image.into_bytes())
            .file_name(filename)
            .mime_str("image/png")
            .map_err(|_| TransportError::Response)?;
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part("photo", part);
        if let Some(caption) = caption {
            form = form.text("caption", caption.to_owned());
        }
        if let Some(reply_to) = reply_to {
            form = form.text(
                "reply_parameters",
                json!({"message_id": reply_to}).to_string(),
            );
        }
        if let Some(message_thread_id) = message_thread_id {
            form = form.text("message_thread_id", message_thread_id.to_string());
        }
        let response = self
            .http
            .post(self.url("sendPhoto"))
            .multipart(form)
            .send()
            .await
            .map_err(request_failed)?;
        let response = decode(response).await?;
        let result = response["result"]
            .as_object()
            .ok_or(TransportError::Response)?;
        let posted = result
            .get("message_id")
            .and_then(Value::as_i64)
            .is_some_and(|id| id > 0);
        let response_chat = result
            .get("chat")
            .and_then(Value::as_object)
            .and_then(|chat| chat.get("id"))
            .and_then(Value::as_i64)
            .ok_or(TransportError::Response)?;
        let response_thread = result.get("message_thread_id").and_then(Value::as_i64);
        let accepted_photo = result
            .get("photo")
            .and_then(Value::as_array)
            .is_some_and(|photo| !photo.is_empty());
        if !posted
            || response_chat != chat_id
            || response_thread != message_thread_id
            || !accepted_photo
        {
            return Err(TransportError::Response);
        }
        Ok(())
    }
}

/// Turns one Bot API failure into a transport error with the credential-bearing URL removed.
///
/// Every call in this transport puts the bot token in its path, and reqwest keeps the request URL
/// on both send and body-read failures, rendering it in the `Display` *and* the `Debug` of its
/// error. `TransportError` is public, `Debug`, and re-exported from a published crate, so an
/// embedder that printed one would print the token. This is the only construction of
/// [`TransportError::Request`] from a Bot API call for exactly that reason; the client-builder
/// failure in [`TelegramTransport::new`] carries no URL and is built inline.
fn request_failed(source: reqwest::Error) -> TransportError {
    TransportError::Request(Box::new(source.without_url()))
}

/// Decodes a Bot API response, turning `ok: false` into its stable description.
async fn decode(response: reqwest::Response) -> Result<Value, TransportError> {
    let status = response.status();
    let bytes = response.bytes().await.map_err(request_failed)?;
    let body =
        serde_json::from_slice::<Value>(&bytes).map_err(TransportError::MalformedResponse)?;
    if status.is_success() && body["ok"] == Value::Bool(true) {
        return Ok(body);
    }
    Err(TransportError::Service {
        code: if status.is_success() {
            described(&body)
        } else {
            format!("http-{}", status.as_u16())
        },
    })
}

/// The Bot API's own `description`, bounded, which is all it says about a rejection.
///
/// One definition because two callers act on it: the reply path turns it into a low-cardinality
/// category, and the liveness path reads it for `message is not modified`, the one description
/// whose meaning is success.
fn described(body: &Value) -> String {
    body["description"]
        .as_str()
        .unwrap_or("unknown")
        .chars()
        .take(64)
        .collect()
}

#[cfg(test)]
mod tests {
    use dekopon_model::ModelText;

    use super::*;

    /// Telegram's `callback_data` ceiling, in bytes, which it enforces by rejecting the whole post.
    ///
    /// Lives here rather than beside [`reply_markup`] because nothing at run time compares against
    /// it: the widest conversation identifier this transport can spell is far inside it, and
    /// [`the_cancel_payload_always_fits_the_bot_api_ceiling`] is what keeps that true.
    const MAX_CALLBACK_DATA_BYTES: usize = 64;

    /// A loopback Bot API that records every call and answers each method from `handler`.
    ///
    /// Hand-rolled and local to this transport: what is under test is the exact request the Bot
    /// API receives, and a real socket carrying real bytes is the only thing that proves one.
    struct BotApi {
        base: String,
        calls: Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    }

    impl BotApi {
        /// Bot API method names, in the order they were called.
        fn methods(&self) -> Vec<String> {
            self.calls
                .lock()
                .expect("mock call log")
                .iter()
                .map(|(method, _)| method.clone())
                .collect()
        }

        /// Every body sent to one method, in order.
        fn bodies(&self, method: &str) -> Vec<Value> {
            self.calls
                .lock()
                .expect("mock call log")
                .iter()
                .filter(|(called, _)| called == method)
                .map(|(_, body)| body.clone())
                .collect()
        }

        /// The body of the one call to `method`.
        fn body(&self, method: &str) -> Value {
            let mut bodies = self.bodies(method);
            assert_eq!(bodies.len(), 1, "{method} was called exactly once");
            bodies.remove(0)
        }
    }

    #[allow(
        clippy::let_underscore_must_use,
        reason = "a mock that cannot finish writing its canned response leaves the driver under \
                  test without one, which is what the calling test already asserts on"
    )]
    fn bot_api<H>(handler: H) -> BotApi
    where
        H: Fn(&str, &Value) -> Value + Send + Sync + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("mock endpoint binds");
        let address = listener.local_addr().expect("mock endpoint address");
        listener
            .set_nonblocking(true)
            .expect("mock endpoint is pollable");
        let listener = tokio::net::TcpListener::from_std(listener).expect("mock endpoint adopts");
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        tokio::spawn(async move {
            let handler = Arc::new(handler);
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = Arc::clone(&handler);
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let mut stream = stream;
                    let Some((path, body)) = read_request(&mut stream).await else {
                        return;
                    };
                    // `/bot<token>/<method>`, with the long poll's query string on the end.
                    let method = path
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .split('?')
                        .next()
                        .unwrap_or_default()
                        .to_owned();
                    let body = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
                    recorded
                        .lock()
                        .expect("mock call log")
                        .push((method.clone(), body.clone()));
                    let encoded = serde_json::to_vec(&(*handler)(&method, &body))
                        .expect("mock response serializes");
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        encoded.len()
                    );
                    use tokio::io::AsyncWriteExt as _;
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&encoded).await;
                    let _ = stream.flush().await;
                });
            }
        });
        BotApi {
            base: format!("http://{address}"),
            calls,
        }
    }

    /// Reads one request, answering with its path and body once both have arrived.
    async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<(String, String)> {
        use tokio::io::AsyncReadExt as _;
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut buffer).await.ok()?;
            if read == 0 {
                return None;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(at) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break at + 4;
            }
        };
        let head = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
        let path = head.lines().next()?.split_whitespace().nth(1)?.to_owned();
        let length = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or_default();
        while bytes.len() < header_end + length {
            let read = stream.read(&mut buffer).await.ok()?;
            if read == 0 {
                return None;
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        Some((
            path,
            String::from_utf8_lossy(&bytes[header_end..header_end + length]).into_owned(),
        ))
    }

    fn driver(base: &str) -> TelegramDriver {
        TelegramDriver {
            endpoint: base.to_owned(),
            token: Redacted::new("test-token".to_owned()),
            http: credential_client(Duration::from_secs(5))
                .build()
                .expect("test client builds"),
            liveness_cooldown_until: std::sync::Mutex::new(None),
        }
    }

    fn transport(base: &str) -> TelegramTransport {
        TelegramTransport::new(
            "family".to_owned(),
            base.to_owned(),
            "test-token".to_owned(),
            LivenessSettings {
                mode: LivenessMode::Native,
                ..LivenessSettings::default()
            },
        )
        .expect("telegram transport builds")
    }

    /// A Bot API success carrying `result`.
    fn accepted(result: Value) -> Value {
        json!({ "ok": true, "result": result })
    }

    /// `sendMessage` answers a posted message; everything else answers a bare success.
    fn posting(method: &str, _: &Value) -> Value {
        if method == "sendMessage" {
            return accepted(json!({ "message_id": 9, "chat": { "id": 42 } }));
        }
        accepted(json!(true))
    }

    fn target() -> LivenessTarget {
        LivenessTarget::Telegram {
            chat_id: 42,
            message_thread_id: None,
            message_id: 7,
        }
    }

    fn topic_target() -> LivenessTarget {
        LivenessTarget::Telegram {
            chat_id: 42,
            message_thread_id: Some(11),
            message_id: 7,
        }
    }

    fn surface() -> MessageRef {
        MessageRef {
            target: target(),
            id: "9".to_owned(),
        }
    }

    /// A recorded stream's cumulative text, repeated until it is longer than one Telegram message.
    ///
    /// Through the model crate's own parser because that parser is the only thing in the workspace
    /// that builds [`ModelText`] from bytes, and repeated rather than handed a `String` of `x`s for
    /// the same reason: a driver cannot be shown text no model wrote.
    fn longer_than_one_message() -> ModelText {
        let events = dekopon_model::events_from_transcript(
            dekopon_test_support::OPENAI_CHAT_COMPLETIONS_TWO_DELTAS,
        )
        .expect("the recorded transcript parses");
        let delta = dekopon_test_support::scripted_text(&events);
        assert!(!delta.is_empty(), "the transcript carries visible text");
        let mut whole = ModelText::default();
        while whole.as_str().chars().count() <= MAX_MESSAGE_CHARS {
            whole.push(&delta);
        }
        whole
    }

    fn one_update(update: Value) -> impl Fn(&str, &Value) -> Value + Send + Sync + 'static {
        move |method, body| {
            if method == "getUpdates" {
                return accepted(json!([update.clone()]));
            }
            posting(method, body)
        }
    }

    /// The presence of a capability object is the whole advertisement; there is no descriptor
    /// beside it that could disagree.
    #[tokio::test]
    async fn telegram_advertises_every_surface_the_bot_api_has() {
        let driver = driver("http://127.0.0.1:1");
        assert!(driver.typing().is_some());
        assert!(driver.progress().is_some());
        assert!(driver.stream().is_some());
        assert!(driver.reaction().is_some());
        assert!(driver.cancel_button().is_some());
        assert!(
            driver.status().is_none(),
            "the Bot API has no durable working state to set"
        );
    }

    #[tokio::test]
    async fn typing_is_a_chat_action_renewed_inside_its_five_second_lease() {
        let api = bot_api(posting);
        let driver = driver(&api.base);
        assert_eq!(driver.renew_every(), Duration::from_secs(4));
        driver
            .renew(&topic_target())
            .await
            .expect("the typing lease renews");
        let body = api.body("sendChatAction");
        assert_eq!(body["chat_id"], 42);
        assert_eq!(body["action"], "typing");
        assert_eq!(
            body["message_thread_id"], 11,
            "a forum topic's action belongs to the topic, not the chat"
        );
    }

    #[tokio::test]
    async fn a_reaction_is_set_on_the_inbound_message_and_taken_back_by_an_empty_list() {
        let api = bot_api(posting);
        let driver = driver(&api.base);
        driver
            .set(&target(), true)
            .await
            .expect("the reaction lands");
        driver
            .set(&target(), false)
            .await
            .expect("the reaction is taken back");
        let bodies = api.bodies("setMessageReaction");
        assert_eq!(bodies[0]["chat_id"], 42);
        assert_eq!(
            bodies[0]["message_id"], 7,
            "the reaction goes on the message being answered"
        );
        assert_eq!(
            bodies[0]["reaction"],
            json!([{ "type": "emoji", "emoji": WORKING_REACTION }])
        );
        assert_eq!(
            bodies[1]["reaction"],
            json!([]),
            "Telegram replaces the whole set, so an empty list is how one is removed"
        );
    }

    #[tokio::test]
    async fn a_progress_message_is_posted_edited_and_deleted() {
        let api = bot_api(posting);
        let driver = driver(&api.base);
        assert_eq!(
            ProgressMessage::limits(&driver),
            ProgressLimits {
                max_chars: MAX_MESSAGE_CHARS,
                min_edit_interval: Duration::from_secs(3),
            }
        );
        let message = driver
            .post_text(&target(), "Working on it…".to_owned(), false)
            .await
            .expect("the progress message posts");
        assert_eq!(message.id, "9");
        assert_eq!(message.target, target());
        driver
            .edit_text(&message, "Running gpt-image…".to_owned(), false)
            .await
            .expect("the progress message edits");
        driver
            .delete(&message)
            .await
            .expect("the progress message deletes");
        assert_eq!(
            api.methods(),
            ["sendMessage", "editMessageText", "deleteMessage"]
        );
        let posted = api.body("sendMessage");
        assert_eq!(posted["chat_id"], 42);
        assert_eq!(posted["text"], "Working on it…");
        let edited = api.body("editMessageText");
        assert_eq!(edited["chat_id"], 42);
        assert_eq!(edited["message_id"], 9);
        assert_eq!(edited["text"], "Running gpt-image…");
        let deleted = api.body("deleteMessage");
        assert_eq!(deleted["chat_id"], 42);
        assert_eq!(deleted["message_id"], 9);
    }

    /// The one Bot API rejection that means the caller got what it asked for.
    #[tokio::test]
    async fn an_edit_that_changes_nothing_is_the_success_it_describes() {
        let api = bot_api(|_, _| {
            json!({
                "ok": false,
                "description":
                    "Bad Request: message is not modified: specified new message content and \
                     reply markup are exactly the same as a current content and reply markup of \
                     the message",
            })
        });
        let driver = driver(&api.base);
        driver
            .edit_text(&surface(), "Working on it…".to_owned(), false)
            .await
            .expect("a re-render of unchanged state is not a failure");
    }

    #[tokio::test]
    async fn a_rejected_edit_surfaces_the_bot_api_description() {
        let api = bot_api(
            |_, _| json!({ "ok": false, "description": "Bad Request: message to edit not found" }),
        );
        let driver = driver(&api.base);
        let error = driver
            .edit_text(&surface(), "Working on it…".to_owned(), false)
            .await
            .expect_err("a deleted message cannot be edited");
        assert!(
            matches!(&error, TransportError::Service { code } if code.contains("message to edit not found")),
            "{error:?}"
        );
    }

    /// A flood limit is a refusal with a cause, so the policy can count it and drop the rung
    /// rather than believe a call that was never sent.
    #[tokio::test]
    async fn a_flood_limit_refuses_every_later_liveness_call_without_sending_one() {
        let api = bot_api(|_, _| json!({ "ok": false, "parameters": { "retry_after": 30 } }));
        let driver = driver(&api.base);
        let first = driver
            .renew(&target())
            .await
            .expect_err("the flood limit fails the call it answered");
        assert!(
            matches!(&first, TransportError::Service { code } if code == "retry-after"),
            "{first:?}"
        );
        let second = driver
            .renew(&target())
            .await
            .expect_err("the cooldown keeps failing");
        assert!(
            matches!(&second, TransportError::Service { code } if code == "cooldown"),
            "{second:?}"
        );
        assert_eq!(
            api.methods(),
            ["sendChatAction"],
            "the second call is refused inside the transport, not by Telegram again"
        );
    }

    #[tokio::test]
    async fn a_streamed_answer_is_one_message_edited_to_the_cumulative_text() {
        let api = bot_api(posting);
        let driver = driver(&api.base);
        assert_eq!(
            TextStream::limits(&driver),
            StreamLimits {
                min_interval: Duration::from_secs(3),
                max_chars: MAX_STREAM_CHARS,
            }
        );
        let streamed = StreamedText {
            text: ModelText::default(),
            truncated: false,
        };
        let message = driver
            .show(&target(), None, &streamed, true)
            .await
            .expect("the stream posts its message");
        let again = driver
            .show(&target(), Some(&message), &streamed, true)
            .await
            .expect("the stream edits the message it posted");
        assert_eq!(
            again, message,
            "one message per session, grown by edits rather than by posts"
        );
        assert_eq!(api.methods(), ["sendMessage", "editMessageText"]);
        assert_eq!(
            api.body("sendMessage")["text"],
            "[empty response]",
            "the Bot API rejects an empty text, so the splitter's floor is what goes out"
        );
    }

    #[tokio::test]
    async fn a_cut_stream_render_says_so_and_still_fits_one_message() {
        let api = bot_api(posting);
        let driver = driver(&api.base);
        // Exactly what the policy hands a driver once the answer outgrows the surface: the head
        // that fits, cut to the ceiling this driver advertised, and the flag saying the rest of it
        // was dropped.
        let whole = longer_than_one_message();
        let streamed = StreamedText {
            text: whole.truncated(MAX_STREAM_CHARS),
            truncated: true,
        };

        let message = driver
            .show(&target(), None, &streamed, false)
            .await
            .expect("the stream posts its message");
        driver
            .show(&target(), Some(&message), &streamed, false)
            .await
            .expect("the stream edits the message it posted");

        let posted = api.body("sendMessage")["text"]
            .as_str()
            .expect("the post carries its text")
            .to_owned();
        let edited = api.body("editMessageText")["text"]
            .as_str()
            .expect("the edit carries its text")
            .to_owned();
        assert!(
            edited.ends_with(TRUNCATION_MARKER),
            "nothing else on this surface distinguishes a cut answer from a finished one"
        );
        assert_eq!(
            posted, edited,
            "the first render of a cut text is marked the same way the next one is"
        );
        assert_eq!(
            edited.chars().count(),
            MAX_STREAM_CHARS + 1,
            "the marker is paid for by the headroom between the scalar cap and the unit ceiling, \
             so no scalar of the head the policy kept is given up for it"
        );
        assert!(
            edited.encode_utf16().count() <= MAX_MESSAGE_CHARS,
            "an edit one unit past the Bot API's ceiling is rejected whole"
        );
        assert!(
            whole
                .as_str()
                .starts_with(edited.trim_end_matches(TRUNCATION_MARKER)),
            "the marker goes after the head that fit, and stands for the text that did not"
        );
    }

    /// The policy counts its cut in scalars, the Bot API counts UTF-16 units, and ninety-six units
    /// of headroom is not every text: a render can still arrive at the ceiling. The marker has to
    /// come out of that text, because an edit one unit over is rejected whole and the reader would
    /// be left with whatever the last accepted render said.
    #[tokio::test]
    async fn a_stream_render_at_the_bot_api_ceiling_takes_its_marker_out_of_the_text() {
        let api = bot_api(posting);
        let driver = driver(&api.base);
        let whole = longer_than_one_message();
        let streamed = StreamedText {
            text: whole.clone(),
            truncated: true,
        };

        driver
            .show(&target(), None, &streamed, false)
            .await
            .expect("the stream posts its message");

        let posted = api.body("sendMessage")["text"]
            .as_str()
            .expect("the post carries its text")
            .to_owned();
        assert_eq!(
            posted.encode_utf16().count(),
            MAX_MESSAGE_CHARS,
            "the marker counts against the ceiling rather than pushing the render past it"
        );
        assert!(posted.ends_with(TRUNCATION_MARKER), "{posted}");
        assert!(
            whole
                .as_str()
                .starts_with(posted.trim_end_matches(TRUNCATION_MARKER)),
            "the marker replaced the last scalar that fit, not text the model never wrote"
        );
    }

    #[tokio::test]
    async fn a_cancel_button_carries_the_conversation_and_is_cleared_when_it_is_not_wanted() {
        let api = bot_api(posting);
        let driver = driver(&api.base);
        let message = driver
            .post_text(&topic_target(), "Working on it…".to_owned(), true)
            .await
            .expect("the progress message posts");
        assert_eq!(
            api.body("sendMessage")["reply_markup"],
            json!({
                "inline_keyboard": [[{ "text": "Stop", "callback_data": "stop:42:topic:11" }]]
            })
        );
        driver
            .edit_text(&message, "Stopped.".to_owned(), false)
            .await
            .expect("the progress message edits");
        assert_eq!(
            api.body("editMessageText")["reply_markup"],
            json!({ "inline_keyboard": [] }),
            "an edit that omitted the markup would leave a dead button on screen"
        );
    }

    #[test]
    fn the_cancel_payload_always_fits_the_bot_api_ceiling() {
        let widest = format!(
            "{CANCEL_CALLBACK_PREFIX}{}",
            conversation_id(i64::MIN, Some(i64::MAX))
        );
        assert!(
            widest.len() <= MAX_CALLBACK_DATA_BYTES,
            "{widest} is {} bytes",
            widest.len()
        );
    }

    #[tokio::test]
    async fn a_cancel_press_is_answered_before_the_request_is_queued() {
        let api = bot_api(one_update(json!({
            "update_id": 100,
            "callback_query": {
                "id": "callback-1",
                "from": { "id": 16034700182_i64, "is_bot": false },
                "message": { "message_id": 9, "chat": { "id": 42, "type": "private" } },
                "data": "stop:42"
            }
        })));
        let mut transport = transport(&api.base);
        transport.poll_once().await.expect("one poll cycle");
        assert_eq!(
            api.methods(),
            ["getUpdates", "answerCallbackQuery"],
            "Telegram spins the button until this lands, so it leaves before anything is queued"
        );
        let answered = api.body("answerCallbackQuery");
        assert_eq!(answered["callback_query_id"], "callback-1");
        let event = transport.next().await.expect("the queued request follows");
        let TransportEvent::CancelRequested(request) = event else {
            panic!("a callback query is a cancel request, not a message");
        };
        assert_eq!(request.transport, "family");
        assert_eq!(request.conversation_id, "42");
        assert_eq!(
            request.subject, "telegram.16034700182",
            "the presser comes from the authenticated envelope, never from the payload"
        );
        assert_eq!(request.via, CancelVia::Button);
    }

    /// `callback_data` is echoed from the presser's own client, so a crafted one must not name a
    /// conversation the envelope does not.
    #[tokio::test]
    async fn a_press_claiming_another_conversation_is_neither_answered_nor_queued() {
        let api = bot_api(one_update(json!({
            "update_id": 100,
            "callback_query": {
                "id": "callback-1",
                "from": { "id": 16034700182_i64, "is_bot": false },
                "message": { "message_id": 9, "chat": { "id": 42, "type": "private" } },
                "data": "stop:99"
            }
        })));
        let mut transport = transport(&api.base);
        transport.poll_once().await.expect("one poll cycle");
        assert_eq!(api.methods(), ["getUpdates"]);
        assert!(transport.pending.is_empty());
        assert_eq!(
            transport.offset, 101,
            "an update this daemon drops is still acknowledged"
        );
    }

    #[tokio::test]
    async fn finalize_turns_the_surface_into_the_answer_and_refuses_what_an_edit_cannot_carry() {
        let api = bot_api(posting);
        let driver = driver(&api.base);
        ProgressMessage::finalize(&driver, &surface(), &OutboundReply::text("the answer"))
            .await
            .expect("the progress message becomes the answer");
        let edited = api.body("editMessageText");
        assert_eq!(edited["text"], "the answer");
        assert_eq!(
            edited["reply_markup"],
            json!({ "inline_keyboard": [] }),
            "the answer keeps no stop button"
        );

        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(b"kitty pixels");
        let with_image = OutboundReply::with_images(
            "here it is",
            vec![GeneratedImage::from_png(png).expect("generated PNG fixture")],
        );
        let refused = ProgressMessage::finalize(&driver, &surface(), &with_image)
            .await
            .expect_err("an edit cannot add a photo");
        assert!(
            matches!(&refused, TransportError::Service { code } if code == "answer-does-not-fit"),
            "{refused:?}"
        );
        let oversized = OutboundReply::text("x".repeat(MAX_MESSAGE_CHARS + 1));
        let refused = ProgressMessage::finalize(&driver, &surface(), &oversized)
            .await
            .expect_err("an edit cannot split an answer past the ceiling");
        assert!(
            matches!(&refused, TransportError::Service { code } if code == "answer-does-not-fit"),
            "{refused:?}"
        );
        assert_eq!(
            api.methods(),
            ["editMessageText"],
            "a refusal the policy will fall back from costs no call"
        );
    }

    /// A reference this transport did not mint names another service's surface.
    #[tokio::test]
    async fn a_surface_belonging_to_another_transport_is_refused() {
        let api = bot_api(posting);
        let driver = driver(&api.base);
        let foreign = MessageRef {
            target: LivenessTarget::Discord {
                channel_id: "1".to_owned(),
                message_id: "2".to_owned(),
            },
            id: "2".to_owned(),
        };
        let error = driver
            .delete(&foreign)
            .await
            .expect_err("a Discord surface is not a Telegram one");
        assert!(matches!(&error, TransportError::Response), "{error:?}");
        assert!(api.methods().is_empty());
    }
}
