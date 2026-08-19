//! Telegram long polling, where the poll *is* the wakeup and the offset *is* the acknowledgment.
//!
//! `getUpdates` blocks server-side for up to fifty seconds and returns as soon as anything arrives,
//! so waiting costs one idle connection rather than a poll loop. Advancing `offset` past an update
//! is what tells Telegram it was handled; there is no separate ack and therefore no ack-before-work
//! problem the way Socket Mode has one.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use dekopon_core::{ExternalSubject, Redacted};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::{
    asset::{AssetSourceRef, PendingAsset},
    transport::{
        AssetFetcher, ChatReplier, ChatTransport, ConversationKind, InboundMessage, ReplyTarget,
        TransportError, TransportIdentity, bound_inbound, floor_boundary,
    },
};

/// Ceiling on one attachment's file name.
const MAX_ATTACHMENT_NAME_BYTES: usize = 128;

/// Server-side wait per poll, in seconds. Telegram's own ceiling is fifty.
const POLL_SECONDS: u64 = 50;
/// Client deadline, generously above the server wait so a normal empty poll is not an error.
const POLL_TIMEOUT: Duration = Duration::from_secs(POLL_SECONDS + 20);
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
}

impl TelegramTransport {
    /// Takes the bot token *value*; the caller resolves it from the named environment variable.
    pub(crate) fn new(
        name: String,
        endpoint: String,
        token: String,
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
            }),
            offset: 0,
            pending: VecDeque::new(),
            failures: 0,
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

        // The Bot API has no thread identifier on a plain message, so a conversation *is* its chat:
        // one private chat and one group are each a single continuous exchange. Deriving it here
        // anyway, rather than letting a caller assume the channel, keeps every transport answering
        // the same question in its own terms.
        let conversation_id = chat_id.to_string();

        Ok(Some(InboundMessage {
            transport: self.name.clone(),
            subject: ExternalSubject::telegram(&user.to_string())
                .map_err(TransportError::Subject)?,
            channel: chat_id.to_string(),
            thread: None,
            conversation_id,
            message_id: message_id.to_string(),
            text: bound_inbound(text),
            assets,
            conversation,
            reply: ReplyTarget::Telegram { chat_id, reply_to },
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

    fn next(&mut self) -> BoxFuture<'_, Result<InboundMessage, TransportError>> {
        Box::pin(async move {
            loop {
                if let Some(message) = self.pending.pop_front() {
                    return Ok(message);
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
}

impl ChatReplier for TelegramReplier {
    fn reply(
        &self,
        target: ReplyTarget,
        text: String,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            let ReplyTarget::Telegram { chat_id, reply_to } = target else {
                return Err(TransportError::Response);
            };
            let mut body = json!({ "chat_id": chat_id, "text": text });
            if let Some(reply_to) = reply_to {
                body["reply_to_message_id"] = json!(reply_to);
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
            decode(response).await.map(|_| ())
        })
    }
}

/// Decodes a Bot API response, turning `ok: false` into its stable description.
async fn decode(response: reqwest::Response) -> Result<Value, TransportError> {
    let bytes = response
        .bytes()
        .await
        .map_err(|source| TransportError::Request(Box::new(source)))?;
    let body = serde_json::from_slice::<Value>(&bytes).map_err(|_| TransportError::Response)?;
    if body["ok"] == Value::Bool(true) {
        return Ok(body);
    }
    Err(TransportError::Service {
        code: body["description"]
            .as_str()
            .unwrap_or("unknown")
            .chars()
            .take(64)
            .collect(),
    })
}
