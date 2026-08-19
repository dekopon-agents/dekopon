//! Slack Socket Mode: an outbound WebSocket instead of an inbound webhook.
//!
//! Socket Mode exists so a daemon behind NAT needs no public HTTP endpoint. The protocol's one
//! sharp edge is redelivery: Slack expects an acknowledgment within roughly three seconds and
//! resends the envelope otherwise. A Dekopon session takes far longer than that, so **the ack is
//! sent before any processing begins** and a bounded ring of seen message identifiers absorbs the
//! redeliveries that happen anyway across a reconnect.

use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use dekopon_core::{ExternalSubject, Redacted};
use futures_util::{SinkExt as _, StreamExt as _, future::BoxFuture};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

use crate::{
    asset::{AssetSourceRef, PendingAsset},
    transport::{
        AssetFetcher, ChatReplier, ChatTransport, ConversationKind, InboundMessage, ReplyTarget,
        TransportError, TransportIdentity, bound_inbound, floor_boundary,
    },
};

/// Redeliveries this transport remembers across reconnects.
const DEDUP_CAPACITY: usize = 1024;
/// Message subtypes that are a person making a new request rather than an event about a message.
///
/// An allowlist rather than a deny list: a subtype Slack introduces later is dropped until someone
/// decides it is a request, which is the same default-deny posture the single `subtype` check had.
/// What that check got wrong was treating *every* subtype as an event about a message. Three are
/// not. `file_share` is the one that matters — an upload with a comment is a subtyped message, so
/// asking a question with a screenshot attached produced no answer at all. `thread_broadcast` is a
/// thread reply the sender also sent to the channel, and `me_message` is `/me`; both are ordinary
/// text a person typed.
const REQUEST_SUBTYPES: [&str; 3] = ["file_share", "me_message", "thread_broadcast"];
/// Attachments taken from one message.
const MAX_ATTACHMENTS: usize = 10;
/// Ceiling on one file name inside an attachment note.
const MAX_ATTACHMENT_NAME_BYTES: usize = 128;
/// Ceiling on reconnect backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// First reconnect delay; doubles up to [`MAX_BACKOFF`].
const BASE_BACKOFF: Duration = Duration::from_millis(500);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One Slack workspace connection.
pub(crate) struct SlackTransport {
    name: String,
    endpoint: String,
    app_token: Redacted<String>,
    http: reqwest::Client,
    replier: Arc<SlackReplier>,
    socket: Option<Socket>,
    identity: TransportIdentity,
    team_id: Option<String>,
    seen: Dedup,
    pending: VecDeque<InboundMessage>,
    failures: u32,
}

impl SlackTransport {
    /// Takes credential *values*, which the caller has already resolved from named environment
    /// variables. Keeping `std::env` out of the transport is what lets a test construct one.
    pub(crate) fn new(
        name: String,
        endpoint: String,
        app_token: String,
        bot_token: String,
    ) -> Result<Self, TransportError> {
        let http = client()?;
        Ok(Self {
            name,
            endpoint: endpoint.clone(),
            app_token: Redacted::new(app_token),
            http: http.clone(),
            replier: Arc::new(SlackReplier {
                endpoint,
                bot_token: Redacted::new(bot_token),
                http,
            }),
            socket: None,
            identity: TransportIdentity::default(),
            team_id: None,
            seen: Dedup::new(DEDUP_CAPACITY),
            pending: VecDeque::new(),
            failures: 0,
        })
    }

    /// Confirms the bot token and learns the bot's own user and team identifiers.
    async fn auth_test(&self) -> Result<(String, String), TransportError> {
        let body = post_form(
            &self.http,
            &format!("{}/api/auth.test", self.endpoint),
            self.replier.bot_token.expose(),
        )
        .await?;
        let user_id = body["user_id"].as_str().ok_or(TransportError::Response)?;
        let team_id = body["team_id"].as_str().ok_or(TransportError::Response)?;
        Ok((user_id.to_owned(), team_id.to_owned()))
    }

    /// Opens one Socket Mode connection and waits for Slack's `hello`.
    async fn open(&mut self) -> Result<(), TransportError> {
        let body = post_form(
            &self.http,
            &format!("{}/api/apps.connections.open", self.endpoint),
            self.app_token.expose(),
        )
        .await?;
        let url = body["url"].as_str().ok_or(TransportError::Response)?;
        let (mut socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        // Slack always greets before it delivers. Waiting for it here means a socket that
        // negotiated but is not actually usable fails inside `open`, where the backoff lives,
        // rather than looking like an empty conversation.
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let frame = serde_json::from_str::<Value>(&text)
                        .map_err(|_| TransportError::Response)?;
                    if frame["type"] == "hello" {
                        break;
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_))) => {}
                Some(Ok(Message::Close(_) | Message::Frame(_))) => {
                    return Err(TransportError::Closed);
                }
                None => return Err(TransportError::Closed),
                Some(Err(source)) => return Err(TransportError::Request(Box::new(source))),
            }
        }
        self.socket = Some(socket);
        self.failures = 0;
        Ok(())
    }

    /// Reads one frame, acknowledging an events envelope before anything else happens to it.
    async fn pump(&mut self) -> Result<(), TransportError> {
        let socket = self.socket.as_mut().ok_or(TransportError::Closed)?;
        let frame = match socket.next().await {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_))) => return Ok(()),
            Some(Ok(Message::Frame(_))) => return Ok(()),
            Some(Ok(Message::Close(_))) | None => return Err(TransportError::Closed),
            Some(Err(source)) => return Err(TransportError::Request(Box::new(source))),
        };
        let frame = serde_json::from_str::<Value>(&frame).map_err(|_| TransportError::Response)?;

        if let Some(envelope) = frame["envelope_id"].as_str() {
            // Before parsing, before routing, before any model call. Slack resends in about three
            // seconds and a session runs for far longer, so acknowledging afterwards guarantees
            // duplicates rather than merely risking them.
            let ack = json!({ "envelope_id": envelope }).to_string();
            socket
                .send(Message::text(ack))
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?;
        }

        match frame["type"].as_str() {
            // Slack rotates sockets on its own schedule; a disconnect is routine, not a failure.
            Some("disconnect") => {
                self.socket = None;
                return Ok(());
            }
            Some("events_api") => {}
            _ => return Ok(()),
        }

        let payload = &frame["payload"];
        let team = payload["team_id"]
            .as_str()
            .or(self.team_id.as_deref())
            .ok_or(TransportError::Response)?
            .to_owned();
        let event = &payload["event"];
        if let Some(message) = self.routable(&team, event)? {
            self.pending.push_back(message);
        }
        Ok(())
    }

    /// Turns one Slack event into a routable message, or `None` when it is not ours to answer.
    fn routable(
        &mut self,
        team: &str,
        event: &Value,
    ) -> Result<Option<InboundMessage>, TransportError> {
        if !matches!(event["type"].as_str(), Some("message" | "app_mention")) {
            return Ok(None);
        }
        // Loop prevention, and it has to be both checks. `bot_id` catches other apps; the user
        // comparison catches this bot's own posts, which arrive without a `bot_id` when the app
        // posts as itself.
        if !event["bot_id"].is_null() {
            return Ok(None);
        }
        let Some(user) = event["user"].as_str() else {
            return Ok(None);
        };
        if self.identity.user_id.as_deref() == Some(user) {
            return Ok(None);
        }
        // Edits, deletions, and joins arrive as subtyped messages; none of them is a new request.
        // The three in `REQUEST_SUBTYPES` are.
        if let Some(subtype) = event["subtype"].as_str()
            && !REQUEST_SUBTYPES.contains(&subtype)
        {
            return Ok(None);
        }
        let (Some(channel), Some(ts)) = (event["channel"].as_str(), event["ts"].as_str()) else {
            return Ok(None);
        };
        // Text is optional rather than required because an upload posted with no comment carries
        // none, and the attachment is then the whole message. A message with neither text nor a
        // file is not a request and is dropped just below.
        let text = bound_inbound(event["text"].as_str().unwrap_or_default());
        let assets = pending_assets(&event["files"]);
        if text.trim().is_empty() && assets.is_empty() {
            return Ok(None);
        }
        if !self.seen.insert(format!("{channel}:{ts}")) {
            return Ok(None);
        }

        let thread_ts = event["thread_ts"].as_str().map(str::to_owned);
        let conversation = if event["channel_type"].as_str() == Some("im") {
            ConversationKind::DirectMessage
        } else {
            ConversationKind::Channel(channel.to_owned())
        };
        // A channel answer joins the thread it was asked in, starting one on the inbound message
        // when there is none; a DM has no threads to join and answering in one would hide the
        // reply behind a disclosure triangle.
        let reply_thread = match &conversation {
            ConversationKind::DirectMessage => None,
            ConversationKind::Channel(_) => {
                Some(thread_ts.clone().unwrap_or_else(|| ts.to_owned()))
            }
        };
        // The conversation is the thread the answer joins, never `thread_ts`. Slack omits
        // `thread_ts` on the message that *starts* a thread and sends it on every reply inside one,
        // so the first turn and the answers to it disagree about `thread` even though they are the
        // same exchange. Deriving the identity from `reply_thread` — the value the bot actually
        // replies into — is what keeps turn one attached to the thread it opened. Do not
        // "simplify" this back to `thread_ts`; that is the bug.
        //
        // Prefixed with the channel because a Slack `ts` is only unique within its channel, and
        // this identity has to stand on its own once it leaves the transport. A direct message has
        // no thread to join, so the whole conversation is the DM channel.
        let conversation_id = match &reply_thread {
            Some(thread) => format!("{channel}:{thread}"),
            None => channel.to_owned(),
        };

        Ok(Some(InboundMessage {
            transport: self.name.clone(),
            subject: ExternalSubject::slack(team, user).map_err(TransportError::Subject)?,
            channel: channel.to_owned(),
            thread: thread_ts,
            conversation_id,
            message_id: ts.to_owned(),
            text,
            assets,
            conversation,
            reply: ReplyTarget::Slack {
                channel: channel.to_owned(),
                thread_ts: reply_thread,
            },
        }))
    }

    /// Exponential backoff with a fixed ceiling, jittered by the process identifier.
    ///
    /// Nothing in this workspace generates randomness and one reconnect loop is not worth a
    /// dependency for it, so the jitter is derived rather than random. It is enough to keep a fleet
    /// of daemons restarted together from lining up on the same retry instant.
    fn backoff(&self) -> Duration {
        let step = BASE_BACKOFF.saturating_mul(1_u32 << self.failures.min(7));
        let capped = step.min(MAX_BACKOFF);
        let jitter = u64::from(std::process::id() % 250);
        capped.saturating_add(Duration::from_millis(jitter))
    }
}

impl ChatTransport for SlackTransport {
    fn name(&self) -> &str {
        &self.name
    }

    fn connect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>> {
        Box::pin(async move {
            let (user_id, team_id) = self.auth_test().await?;
            self.identity = TransportIdentity {
                user_id: Some(user_id),
                handle: None,
            };
            self.team_id = Some(team_id);
            self.open().await?;
            Ok(self.identity.clone())
        })
    }

    fn next(&mut self) -> BoxFuture<'_, Result<InboundMessage, TransportError>> {
        Box::pin(async move {
            loop {
                if let Some(message) = self.pending.pop_front() {
                    return Ok(message);
                }
                if self.socket.is_none() {
                    tokio::time::sleep(self.backoff()).await;
                    if let Err(error) = self.open().await {
                        self.failures = self.failures.saturating_add(1);
                        tracing::warn!(
                            event = "gateway_transport_reconnect_failed",
                            transport = %self.name,
                            category = error.category()
                        );
                    }
                    continue;
                }
                if let Err(error) = self.pump().await {
                    self.socket = None;
                    self.failures = self.failures.saturating_add(1);
                    tracing::warn!(
                        event = "gateway_transport_disconnected",
                        transport = %self.name,
                        category = error.category()
                    );
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

/// The bot-token half of a Slack transport.
pub(crate) struct SlackReplier {
    endpoint: String,
    bot_token: Redacted<String>,
    http: reqwest::Client,
}

impl ChatReplier for SlackReplier {
    fn reply(
        &self,
        target: ReplyTarget,
        text: String,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            let ReplyTarget::Slack { channel, thread_ts } = target else {
                return Err(TransportError::Response);
            };
            // A `markdown` block, so Slack translates the model's CommonMark instead of this
            // process doing it. Slack's `text` field is mrkdwn — a proprietary syntax where bold is
            // `*one asterisk*` — so an answer posted through it arrives with its formatting as
            // literal punctuation. The block exists for exactly this case and renders tables and
            // task lists that mrkdwn cannot express at all.
            //
            // `text` stays as the notification fallback, which is the one place blocks do not
            // render. It carries the answer unchanged rather than a second translation of it.
            let mut body = json!({
                "channel": channel,
                "text": text,
                "blocks": [{ "type": "markdown", "text": text }],
            });
            if let Some(thread_ts) = thread_ts {
                body["thread_ts"] = Value::String(thread_ts);
            }
            let response = self
                .http
                .post(format!("{}/api/chat.postMessage", self.endpoint))
                .header(
                    "authorization",
                    format!("Bearer {}", self.bot_token.expose()),
                )
                .header("content-type", "application/json; charset=utf-8")
                .body(serde_json::to_vec(&body).map_err(|_| TransportError::Response)?)
                .send()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?;
            check_ok(response).await.map(|_| ())
        })
    }
}

/// Bounded ring of seen message identifiers.
///
/// Bounded because it must survive reconnects without becoming a slow leak on a busy workspace,
/// and a ring because the only redeliveries that matter are recent ones.
struct Dedup {
    order: VecDeque<String>,
    seen: HashSet<String>,
    capacity: usize,
}

impl Dedup {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            seen: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    /// Records an identifier, reporting `false` when it was already seen.
    fn insert(&mut self, key: String) -> bool {
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

/// Describes the files on one message so the session can number them.
///
/// Name and media type come from the event and are sender-controlled, so they are untrusted
/// exactly like the message text. A file the app cannot see at all arrives without an id or a URL —
/// Slack withholds both when the token lacks `files:read` on it — and is skipped rather than
/// registered as an asset nothing could resolve.
fn pending_assets(files: &Value) -> Vec<PendingAsset> {
    let Some(files) = files.as_array() else {
        return Vec::new();
    };
    files
        .iter()
        .take(MAX_ATTACHMENTS)
        .map(|file| {
            // `url_private_download` rather than `url_private`: the former serves the bytes, the
            // latter serves Slack's own viewer page for some types. Both are absent, along with the
            // id, when the token has no access to this file — the asset is still described, with no
            // way to resolve it.
            let source = file["id"].as_str().zip(
                file["url_private_download"]
                    .as_str()
                    .or_else(|| file["url_private"].as_str()),
            );
            let name = file["name"].as_str().unwrap_or("attachment");
            let name = name[..floor_boundary(name, MAX_ATTACHMENT_NAME_BYTES)].to_owned();
            PendingAsset {
                name,
                mime: file["mimetype"].as_str().unwrap_or_default().to_owned(),
                size: file["size"].as_u64().unwrap_or_default(),
                source: source.map(|(file_id, url)| AssetSourceRef::Slack {
                    file_id: file_id.to_owned(),
                    url: url.to_owned(),
                }),
            }
        })
        .collect()
}

/// The one redirect hop a Slack file download is allowed to take.
///
/// `client()` refuses redirects globally, which is the right default for an API call carrying a
/// bearer token — a redirect there would forward the credential to whatever host answered.
/// `url_private_download` genuinely does redirect, to Slack's own file host, so this transport
/// follows exactly one hop and only to a host it recognises, re-attaching the token itself rather
/// than letting a redirect policy carry it anywhere.
const SLACK_FILE_HOSTS: [&str; 2] = ["files.slack.com", "slack.com"];

impl AssetFetcher for SlackReplier {
    fn fetch(
        &self,
        source: &AssetSourceRef,
        max_bytes: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, TransportError>> {
        // A reference belonging to another transport is a routing mistake rather than a fetch
        // failure, and the daemon looks a fetcher up by the message's own transport name.
        let AssetSourceRef::Slack { url, .. } = source else {
            return Box::pin(async { Err(TransportError::Response) });
        };
        let url = url.clone();
        Box::pin(async move {
            let mut response = self.get_file(&url).await?;
            // One hop, and only to a Slack file host. Anything else is a redirect this transport
            // will not carry a bot token to.
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|value| value.to_str().ok())
                    .ok_or(TransportError::Response)?
                    .to_owned();
                if !is_slack_file_url(&location) {
                    return Err(TransportError::Response);
                }
                response = self.get_file(&location).await?;
            }
            if !response.status().is_success() {
                return Err(TransportError::Service {
                    code: response.status().as_u16().to_string(),
                });
            }
            // Streamed against the ceiling rather than buffered and measured afterwards. The
            // reported size is sender-influenced metadata and a chunked response need not declare
            // a length at all, so the only bound that holds is the one applied while reading.
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

impl SlackReplier {
    /// One authenticated GET against a Slack file URL, without following redirects.
    async fn get_file(&self, url: &str) -> Result<reqwest::Response, TransportError> {
        self.http
            .get(url)
            .header(
                "authorization",
                format!("Bearer {}", self.bot_token.expose()),
            )
            .send()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))
    }
}

/// Whether a redirect target is a Slack file host this transport will re-authenticate to.
///
/// Compares the host itself rather than a prefix of the URL, so `https://files.slack.com.evil.test`
/// is not mistaken for Slack.
pub(crate) fn is_slack_file_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest
        .split('/')
        .next()
        .unwrap_or_default()
        .split('@')
        .next_back()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default();
    SLACK_FILE_HOSTS
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
}

/// One HTTP client shared across a transport's calls, with redirects refused.
pub(crate) fn client() -> Result<reqwest::Client, TransportError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|source| TransportError::Request(Box::new(source)))
}

/// Posts an empty form with a bearer token, which is what Slack's token-only methods expect.
async fn post_form(
    http: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<Value, TransportError> {
    let response = http
        .post(url)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Vec::new())
        .send()
        .await
        .map_err(|source| TransportError::Request(Box::new(source)))?;
    check_ok(response).await
}

/// Decodes a Slack response, turning `ok: false` into the documented error code.
///
/// The code is Slack's own stable vocabulary (`invalid_auth`, `channel_not_found`), never a token
/// or a message body, so it is safe to log and to carry in an error.
async fn check_ok(response: reqwest::Response) -> Result<Value, TransportError> {
    let bytes = response
        .bytes()
        .await
        .map_err(|source| TransportError::Request(Box::new(source)))?;
    let body = serde_json::from_slice::<Value>(&bytes).map_err(|_| TransportError::Response)?;
    if body["ok"] == Value::Bool(true) {
        return Ok(body);
    }
    Err(TransportError::Service {
        code: body["error"]
            .as_str()
            .unwrap_or("unknown")
            .chars()
            .take(64)
            .collect(),
    })
}
