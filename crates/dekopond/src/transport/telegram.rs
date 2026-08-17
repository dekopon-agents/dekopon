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

use crate::transport::{
    ChatReplier, ChatTransport, ConversationKind, InboundMessage, ReplyTarget, TransportError,
    TransportIdentity, bound_inbound,
};

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
        let (Some(from), Some(chat), Some(text), Some(message_id)) = (
            message["from"].as_object(),
            message["chat"].as_object(),
            message["text"].as_str(),
            message["message_id"].as_i64(),
        ) else {
            return Ok(None);
        };
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
            conversation,
            reply: ReplyTarget::Telegram { chat_id, reply_to },
        }))
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
