//! Telegram long polling, where the poll *is* the wakeup and the offset *is* the acknowledgment.
//!
//! `getUpdates` blocks server-side for up to fifty seconds and returns as soon as anything arrives,
//! so waiting costs one idle connection rather than a poll loop. Advancing `offset` past an update
//! is what tells Telegram it was handled; there is no separate ack and therefore no ack-before-work
//! problem the way Socket Mode has one.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use dekopon_broker_protocol::ChatTransportKind;
use dekopon_core::{ExternalSubject, Redacted};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::{
    asset::{AssetSourceRef, PendingAsset},
    config::ActivityMode,
    transport::{
        ActivityTarget, AssetFetcher, ChatActivity, ChatReplier, ChatTransport, ConversationKind,
        DeliveryReceipt, InboundMessage, ReplyTarget, TransportError, TransportEvent,
        TransportIdentity, bound_inbound, floor_boundary,
    },
};

/// Ceiling on one attachment's file name.
const MAX_ATTACHMENT_NAME_BYTES: usize = 128;

/// Server-side wait per poll, in seconds. Telegram's own ceiling is fifty.
const POLL_SECONDS: u64 = 50;
/// Client deadline, generously above the server wait so a normal empty poll is not an error.
const POLL_TIMEOUT: Duration = Duration::from_secs(POLL_SECONDS + 20);
const ACTIVITY_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const ACTIVITY_REFRESH_INTERVAL: Duration = Duration::from_secs(4);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const BASE_BACKOFF: Duration = Duration::from_millis(500);

pub(crate) struct TelegramTransport {
    name: String,
    endpoint: String,
    token: Redacted<String>,
    http: reqwest::Client,
    replier: Arc<TelegramReplier>,
    offset: i64,
    pending: VecDeque<InboundMessage>,
    failures: u32,
    activity: ActivityMode,
}

impl TelegramTransport {
    /// Takes the bot token *value*; the caller resolves it from the named environment variable.
    pub(crate) fn new(
        name: String,
        endpoint: String,
        token: String,
        activity: ActivityMode,
    ) -> Result<Self, TransportError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(POLL_TIMEOUT)
            .build()
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        Ok(Self {
            name,
            endpoint: endpoint.clone(),
            token: Redacted::new(token.clone()),
            http: http.clone(),
            replier: Arc::new(TelegramReplier {
                endpoint,
                token: Redacted::new(token),
                http,
                activity_cooldown_until: std::sync::Mutex::new(None),
            }),
            offset: 0,
            pending: VecDeque::new(),
            failures: 0,
            activity,
        })
    }

    fn method(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.endpoint, self.token.expose())
    }

    async fn poll(&mut self) -> Result<(), TransportError> {
        let url = format!(
            "{}?timeout={POLL_SECONDS}&offset={}",
            self.method("getUpdates"),
            self.offset
        );
        let body = decode(
            self.http
                .get(url)
                .send()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?,
        )
        .await?;
        let updates = body["result"]
            .as_array()
            .ok_or(TransportError::Response)?
            .clone();
        for update in updates {
            let Some(update_id) = update["update_id"].as_i64() else {
                continue;
            };
            // Advance first, unconditionally. An update this daemon chooses not to route still has
            // to be acknowledged, or the next poll returns it forever.
            self.offset = self.offset.max(update_id + 1);
            if let Some(message) = self.routable(&update["message"])? {
                self.pending.push_back(message);
            }
        }
        self.failures = 0;
        Ok(())
    }

    fn routable(&self, message: &Value) -> Result<Option<InboundMessage>, TransportError> {
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
        // durable memory, and transient activity together.
        let message_thread_id = message["message_thread_id"].as_i64();
        if message_thread_id.is_some_and(|id| id <= 0) {
            return Err(TransportError::Response);
        }
        let conversation_id = message_thread_id.map_or_else(
            || chat_id.to_string(),
            |topic| format!("{chat_id}:topic:{topic}"),
        );

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
            reply: ReplyTarget::Telegram {
                chat_id,
                reply_to,
                message_thread_id,
            },
            activity: (self.activity == ActivityMode::Native).then_some(ActivityTarget::Telegram {
                chat_id,
                message_thread_id,
            }),
        }))
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

    fn backoff(&self) -> Duration {
        let step = BASE_BACKOFF.saturating_mul(1_u32 << self.failures.min(7));
        step.min(MAX_BACKOFF)
            .saturating_add(Duration::from_millis(u64::from(std::process::id() % 250)))
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
                    .get(self.method("getMe"))
                    .send()
                    .await
                    .map_err(|source| TransportError::Request(Box::new(source)))?,
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
                if let Some(message) = self.pending.pop_front() {
                    return Ok(TransportEvent::Message(Box::new(message)));
                }
                if let Err(error) = self.poll().await {
                    self.failures = self.failures.saturating_add(1);
                    tracing::warn!(
                        event = "gateway_transport_poll_failed",
                        transport = %self.name,
                        category = error.category()
                    );
                    tokio::time::sleep(self.backoff()).await;
                }
            }
        })
    }

    fn replier(&self) -> Arc<dyn ChatReplier> {
        Arc::clone(&self.replier) as Arc<dyn ChatReplier>
    }

    fn asset_fetcher(&self) -> Option<Arc<dyn AssetFetcher>> {
        Some(Arc::clone(&self.replier) as Arc<dyn AssetFetcher>)
    }

    fn activity(&self) -> Option<Arc<dyn ChatActivity>> {
        (self.activity == ActivityMode::Native)
            .then(|| Arc::clone(&self.replier) as Arc<dyn ChatActivity>)
    }
}

/// Resolving a `file_id` and downloading the bytes, which the Bot API splits into two calls.
///
/// Telegram hands out a handle rather than a URL. `getFile` turns it into a path valid for roughly
/// an hour, and the bytes live under a different prefix — `/file/bot<token>/<path>` rather than
/// `/bot<token>/<method>`. Both carry the token in the URL, which is the Bot API's own design and
/// the reason this transport never logs one.
impl AssetFetcher for TelegramReplier {
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
                    .get(format!(
                        "{}/bot{}/getFile",
                        self.endpoint,
                        self.token.expose()
                    ))
                    .query(&[("file_id", file_id.as_str())])
                    .send()
                    .await
                    .map_err(|source| TransportError::Request(Box::new(source)))?,
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
                .map_err(|source| TransportError::Request(Box::new(source)))?;
            if !response.status().is_success() {
                return Err(TransportError::Service {
                    code: response.status().as_u16().to_string(),
                });
            }
            // Streamed against the ceiling rather than buffered and measured afterwards, for the
            // same reason the Slack path is: a declared length is not a bound.
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?
            {
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

pub(crate) struct TelegramReplier {
    endpoint: String,
    token: Redacted<String>,
    http: reqwest::Client,
    activity_cooldown_until: std::sync::Mutex<Option<tokio::time::Instant>>,
}

impl ChatActivity for TelegramReplier {
    fn show(&self, target: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            let ActivityTarget::Telegram {
                chat_id,
                message_thread_id,
            } = target
            else {
                return Err(TransportError::Response);
            };
            let now = tokio::time::Instant::now();
            if self
                .activity_cooldown_until
                .lock()
                .expect("Telegram activity cooldown")
                .is_some_and(|until| until > now)
            {
                return Ok(());
            }
            let mut body = json!({ "chat_id": chat_id, "action": "typing" });
            if let Some(message_thread_id) = message_thread_id {
                body["message_thread_id"] = json!(message_thread_id);
            }
            let response = self
                .http
                .post(format!(
                    "{}/bot{}/sendChatAction",
                    self.endpoint,
                    self.token.expose()
                ))
                .header("content-type", "application/json")
                .body(serde_json::to_vec(&body).map_err(|_| TransportError::Response)?)
                .timeout(ACTIVITY_REQUEST_TIMEOUT)
                .send()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?;
            let bytes = response
                .bytes()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?;
            let body =
                serde_json::from_slice::<Value>(&bytes).map_err(|_| TransportError::Response)?;
            if body["ok"] == Value::Bool(true) {
                *self
                    .activity_cooldown_until
                    .lock()
                    .expect("Telegram activity cooldown") = None;
                return Ok(());
            }
            if let Some(seconds) = body["parameters"]["retry_after"].as_u64() {
                *self
                    .activity_cooldown_until
                    .lock()
                    .expect("Telegram activity cooldown") =
                    Some(tokio::time::Instant::now() + Duration::from_secs(seconds.min(300)));
                return Err(TransportError::Service {
                    code: "retry-after".to_owned(),
                });
            }
            Err(TransportError::Service {
                code: "chat-action-rejected".to_owned(),
            })
        })
    }

    fn hide(&self, target: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            if !matches!(target, ActivityTarget::Telegram { .. }) {
                return Err(TransportError::Response);
            }
            // Telegram has no explicit clear. Renewal stops here and the final bot message clears
            // the native action sooner than its five-second lease.
            Ok(())
        })
    }

    fn refresh_interval(&self) -> Option<Duration> {
        Some(ACTIVITY_REFRESH_INTERVAL)
    }
}

impl ChatReplier for TelegramReplier {
    fn reply(
        &self,
        target: ReplyTarget,
        text: String,
    ) -> BoxFuture<'_, Result<DeliveryReceipt, TransportError>> {
        Box::pin(async move {
            let ReplyTarget::Telegram {
                chat_id,
                reply_to,
                message_thread_id,
            } = target
            else {
                return Err(TransportError::Response);
            };
            let mut body = json!({ "chat_id": chat_id, "text": text });
            if let Some(reply_to) = reply_to {
                body["reply_to_message_id"] = json!(reply_to);
            }
            if let Some(message_thread_id) = message_thread_id {
                body["message_thread_id"] = json!(message_thread_id);
            }
            let response = self
                .http
                .post(format!(
                    "{}/bot{}/sendMessage",
                    self.endpoint,
                    self.token.expose()
                ))
                .header("content-type", "application/json")
                .body(serde_json::to_vec(&body).map_err(|_| TransportError::Response)?)
                .send()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?;
            let response = decode(response).await?;
            let result = response["result"]
                .as_object()
                .ok_or(TransportError::Response)?;
            let message_id = result
                .get("message_id")
                .and_then(Value::as_i64)
                .filter(|id| *id > 0)
                .ok_or(TransportError::Response)?;
            let response_chat = result
                .get("chat")
                .and_then(Value::as_object)
                .and_then(|chat| chat.get("id"))
                .and_then(Value::as_i64)
                .ok_or(TransportError::Response)?;
            let response_thread = result.get("message_thread_id").and_then(Value::as_i64);
            if response_chat != chat_id || response_thread != message_thread_id {
                return Err(TransportError::Response);
            }
            Ok(DeliveryReceipt::new(format!("{chat_id}:{message_id}")))
        })
    }
}

/// Decodes a Bot API response, turning `ok: false` into its stable description.
async fn decode(response: reqwest::Response) -> Result<Value, TransportError> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|source| TransportError::Request(Box::new(source)))?;
    let body = serde_json::from_slice::<Value>(&bytes).map_err(|_| TransportError::Response)?;
    if status.is_success() && body["ok"] == Value::Bool(true) {
        return Ok(body);
    }
    Err(TransportError::Service {
        code: if status.is_success() {
            body["description"]
                .as_str()
                .unwrap_or("unknown")
                .chars()
                .take(64)
                .collect()
        } else {
            format!("http-{}", status.as_u16())
        },
    })
}
