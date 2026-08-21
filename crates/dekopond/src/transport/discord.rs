//! Discord Gateway v10: an outbound event WebSocket paired with REST replies.
//!
//! The transport requests only `GUILD_MESSAGES` and `DIRECT_MESSAGES`. Discord exposes message
//! content and attachments without the privileged Message Content intent in direct messages and in
//! guild messages that mention the bot, which is exactly the surface this gateway routes. The
//! structured `mentions` array decides whether a guild message is addressed; model-visible text is
//! never trusted to make that decision.

use std::{
    collections::{HashSet, VecDeque, hash_map::RandomState},
    hash::{BuildHasher as _, Hasher as _},
    sync::Arc,
    time::Duration,
};

use dekopon_broker_protocol::ChatTransportKind;
use dekopon_core::{ExternalSubject, Redacted};
use dekopon_model::image::GeneratedImage;
use futures_util::{SinkExt as _, StreamExt as _, future::BoxFuture};
use serde_json::{Value, json};
use tokio::{net::TcpStream, sync::Mutex, time::Instant};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

use crate::{
    asset::{AssetSourceRef, PendingAsset},
    config::{ActivityMode, DISCORD_ENDPOINT},
    transport::{
        ActivityTarget, AssetFetcher, ChatActivity, ChatReplier, ChatTransport, ConversationKind,
        DeliveryReceipt, InboundMessage, OutboundReply, ReplyTarget, TransportError,
        TransportEvent, TransportIdentity, bound_inbound, floor_boundary,
    },
};

/// Discord API version used by both REST and Gateway.
const API_VERSION: u8 = 10;
/// `GUILD_MESSAGES | DIRECT_MESSAGES`; Message Content is deliberately absent.
const INTENTS: u64 = (1 << 9) | (1 << 12);
/// Recent message identifiers retained across reconnect/resume redelivery.
const DEDUP_CAPACITY: usize = 1024;
/// Discord permits at most ten attachments on one message.
const MAX_ATTACHMENTS: usize = 10;
/// Ceiling on one sender-controlled attachment filename.
const MAX_ATTACHMENT_NAME_BYTES: usize = 128;
/// Discord's Create Message content ceiling, enforced as UTF-16 code units.
const MAX_MESSAGE_CHARS: usize = 2_000;
/// The single-shard identify bucket permits one Identify every five seconds.
const IDENTIFY_INTERVAL: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const BASE_BACKOFF: Duration = Duration::from_millis(500);
const REST_TIMEOUT: Duration = Duration::from_secs(30);
const ACTIVITY_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const ACTIVITY_REFRESH_INTERVAL: Duration = Duration::from_secs(8);
const MAX_ACTIVITY_COOLDOWN: Duration = Duration::from_secs(300);
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(30);

const DISCORD_CDN_HOSTS: [&str; 2] = ["cdn.discordapp.com", "media.discordapp.net"];
const FATAL_GATEWAY_CLOSE_CODES: [u16; 6] = [4004, 4010, 4011, 4012, 4013, 4014];

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One Discord bot connection.
pub(crate) struct DiscordTransport {
    name: String,
    endpoint: String,
    token: Redacted<String>,
    http: reqwest::Client,
    replier: Arc<DiscordReplier>,
    gateway_url: Option<String>,
    session_starts: Option<SessionStarts>,
    socket: Option<Socket>,
    identity: TransportIdentity,
    sequence: Option<u64>,
    session_id: Option<String>,
    resume_gateway_url: Option<String>,
    heartbeat_interval: Option<Duration>,
    next_heartbeat: Option<Instant>,
    heartbeat_acked: bool,
    last_identify: Option<Instant>,
    seen: Dedup,
    pending: VecDeque<InboundMessage>,
    failures: u32,
    activity: ActivityMode,
}

#[derive(Clone, Copy)]
struct SessionStarts {
    remaining: u64,
    reset_at: Instant,
}

#[derive(Debug)]
enum PumpResult {
    Idle,
    Ready,
    Message(Box<InboundMessage>),
}

impl DiscordTransport {
    /// Takes the bot-token value after the caller resolved its configured environment variable.
    pub(crate) fn new(
        name: String,
        endpoint: String,
        token: String,
        activity: ActivityMode,
    ) -> Result<Self, TransportError> {
        let http = client()?;
        let production = endpoint == DISCORD_ENDPOINT;
        Ok(Self {
            name,
            endpoint: endpoint.clone(),
            token: Redacted::new(token.clone()),
            http: http.clone(),
            replier: Arc::new(DiscordReplier {
                endpoint,
                token: Redacted::new(token),
                http,
                production,
                rest_lock: Mutex::new(()),
                activity_cooldown_until: std::sync::Mutex::new(None),
            }),
            gateway_url: None,
            session_starts: None,
            socket: None,
            identity: TransportIdentity::default(),
            sequence: None,
            session_id: None,
            resume_gateway_url: None,
            heartbeat_interval: None,
            next_heartbeat: None,
            heartbeat_acked: true,
            last_identify: None,
            seen: Dedup::new(DEDUP_CAPACITY),
            pending: VecDeque::new(),
            failures: 0,
            activity,
        })
    }

    /// Discovers the current Gateway and the identify allowance for this bot.
    async fn discover(&mut self) -> Result<(), TransportError> {
        let response = self
            .http
            .get(format!("{}/api/v{API_VERSION}/gateway/bot", self.endpoint))
            .header("authorization", format!("Bot {}", self.token.expose()))
            .send()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        let body = decode(response).await?;
        let gateway = body["url"].as_str().ok_or(TransportError::Response)?;
        // Validate before retaining a URL that will receive the bot token in Identify/Resume.
        let gateway = gateway_url(gateway, self.endpoint == DISCORD_ENDPOINT)?;
        let limit = &body["session_start_limit"];
        let remaining = limit["remaining"]
            .as_u64()
            .ok_or(TransportError::Response)?;
        let reset_after = limit["reset_after"]
            .as_u64()
            .ok_or(TransportError::Response)?;
        let max_concurrency = limit["max_concurrency"]
            .as_u64()
            .ok_or(TransportError::Response)?;
        if max_concurrency == 0 {
            return Err(TransportError::Response);
        }
        self.gateway_url = Some(gateway);
        self.session_starts = Some(SessionStarts {
            remaining,
            reset_at: Instant::now() + Duration::from_millis(reset_after),
        });
        Ok(())
    }

    /// Opens one Gateway socket and completes Identify or Resume through READY/RESUMED.
    async fn open(&mut self) -> Result<(), TransportError> {
        if self.gateway_url.is_none() || self.session_starts.is_none() {
            self.discover().await?;
        }
        let resuming = self.session_id.is_some() && self.sequence.is_some();
        if !resuming {
            self.clear_session();
            // Wait/check before opening a socket so identify throttling cannot leave a live Gateway
            // connection sitting without its heartbeat loop. The allowance itself is consumed only
            // immediately before opcode 2 is sent, after TCP/TLS and Hello have succeeded.
            self.prepare_identify().await?;
        }
        let raw_url = if resuming {
            self.resume_gateway_url
                .as_deref()
                .or(self.gateway_url.as_deref())
        } else {
            self.gateway_url.as_deref()
        }
        .ok_or(TransportError::Response)?;
        let url = gateway_url(raw_url, self.endpoint == DISCORD_ENDPOINT)?;
        let (socket, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        self.socket = Some(socket);
        self.heartbeat_interval = None;
        self.next_heartbeat = None;
        self.heartbeat_acked = true;

        // Discord sends Hello first. No heartbeat deadline exists until it tells us the interval.
        loop {
            let frame = self.read_payload().await?;
            if frame["op"].as_u64() == Some(10) {
                self.configure_heartbeat(&frame["d"])?;
                break;
            }
        }

        if resuming {
            let payload = json!({
                "op": 6,
                "d": {
                    "token": self.token.expose(),
                    "session_id": self.session_id,
                    "seq": self.sequence,
                }
            });
            self.send(payload).await?;
        } else {
            self.consume_identify()?;
            let payload = json!({
                "op": 2,
                "d": {
                    "token": self.token.expose(),
                    "intents": INTENTS,
                    "properties": {
                        "os": std::env::consts::OS,
                        "browser": "dekopond",
                        "device": "dekopond",
                    }
                }
            });
            self.send(payload).await?;
        }

        loop {
            match self.pump().await? {
                PumpResult::Ready => {
                    self.failures = 0;
                    return Ok(());
                }
                PumpResult::Message(message) => self.pending.push_back(*message),
                PumpResult::Idle => {}
            }
        }
    }

    async fn prepare_identify(&mut self) -> Result<(), TransportError> {
        let now = Instant::now();
        if self
            .session_starts
            .is_some_and(|limit| now >= limit.reset_at)
        {
            self.discover().await?;
        }
        if self.session_starts.is_none_or(|limit| limit.remaining == 0) {
            return Err(TransportError::Service {
                code: "session-start-limit-exhausted".to_owned(),
            });
        }
        if let Some(previous) = self.last_identify {
            let next = previous + IDENTIFY_INTERVAL;
            if next > now {
                tokio::time::sleep_until(next).await;
            }
        }
        Ok(())
    }

    fn consume_identify(&mut self) -> Result<(), TransportError> {
        let Some(limit) = &mut self.session_starts else {
            return Err(TransportError::Response);
        };
        if limit.remaining == 0 {
            return Err(TransportError::Service {
                code: "session-start-limit-exhausted".to_owned(),
            });
        }
        limit.remaining -= 1;
        self.last_identify = Some(Instant::now());
        Ok(())
    }

    fn configure_heartbeat(&mut self, hello: &Value) -> Result<(), TransportError> {
        let milliseconds = hello["heartbeat_interval"]
            .as_u64()
            .ok_or(TransportError::Response)?;
        if milliseconds == 0 {
            return Err(TransportError::Response);
        }
        let interval = Duration::from_millis(milliseconds);
        // Discord requires a random first-heartbeat jitter in [0, interval). RandomState is
        // OS-seeded and already underlies the workspace's opaque identifier minting, so this adds
        // no RNG dependency and does not synchronize a fleet restarted at once.
        let jitter = Duration::from_millis(jitter_below(milliseconds));
        self.heartbeat_interval = Some(interval);
        self.next_heartbeat = Some(Instant::now() + jitter);
        self.heartbeat_acked = true;
        Ok(())
    }

    /// Reads one JSON payload, sending scheduled heartbeats while the socket is otherwise idle.
    async fn read_payload(&mut self) -> Result<Value, TransportError> {
        loop {
            let frame = if let Some(deadline) = self.next_heartbeat {
                let result = {
                    let socket = self.socket.as_mut().ok_or(TransportError::Closed)?;
                    tokio::time::timeout_at(deadline, socket.next()).await
                };
                match result {
                    Ok(frame) => frame,
                    Err(_) => {
                        if !self.heartbeat_acked {
                            return Err(TransportError::Closed);
                        }
                        self.send_heartbeat().await?;
                        if let Some(interval) = self.heartbeat_interval {
                            self.next_heartbeat = Some(Instant::now() + interval);
                        }
                        continue;
                    }
                }
            } else {
                self.socket
                    .as_mut()
                    .ok_or(TransportError::Closed)?
                    .next()
                    .await
            };

            match frame {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str::<Value>(&text)
                        .map_err(TransportError::MalformedResponse);
                }
                Some(Ok(Message::Ping(payload))) => {
                    self.socket
                        .as_mut()
                        .ok_or(TransportError::Closed)?
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|source| TransportError::Request(Box::new(source)))?;
                }
                Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Binary(_))) => return Err(TransportError::Response),
                Some(Ok(Message::Close(frame))) => {
                    if let Some(frame) = frame {
                        let code = u16::from(frame.code);
                        if matches!(code, 4007 | 4009) {
                            self.clear_session();
                        }
                        if FATAL_GATEWAY_CLOSE_CODES.contains(&code) {
                            return Err(TransportError::Service {
                                code: format!("gateway-close-{code}"),
                            });
                        }
                    }
                    return Err(TransportError::Closed);
                }
                Some(Err(source)) => {
                    return Err(TransportError::Request(Box::new(source)));
                }
                None => return Err(TransportError::Closed),
            }
        }
    }

    async fn pump(&mut self) -> Result<PumpResult, TransportError> {
        let frame = self.read_payload().await?;
        match frame["op"].as_u64().ok_or(TransportError::Response)? {
            0 => {
                if let Some(sequence) = frame["s"].as_u64() {
                    self.sequence = Some(sequence);
                }
                let event = frame["t"].as_str().ok_or(TransportError::Response)?;
                match event {
                    "READY" => {
                        let data = &frame["d"];
                        let session_id = data["session_id"]
                            .as_str()
                            .ok_or(TransportError::Response)?;
                        let resume = data["resume_gateway_url"]
                            .as_str()
                            .ok_or(TransportError::Response)?;
                        let user_id = data["user"]["id"]
                            .as_str()
                            .ok_or(TransportError::Response)?;
                        if !is_snowflake(user_id) {
                            return Err(TransportError::Response);
                        }
                        // Validate before this URL can receive a future Resume token.
                        self.resume_gateway_url =
                            Some(gateway_url(resume, self.endpoint == DISCORD_ENDPOINT)?);
                        self.session_id = Some(session_id.to_owned());
                        self.identity = TransportIdentity {
                            user_id: Some(user_id.to_owned()),
                            // Discord mentions are identifier-based and the structured mentions
                            // array is authoritative. A mutable display name is not a fallback.
                            handle: None,
                        };
                        Ok(PumpResult::Ready)
                    }
                    "RESUMED" => Ok(PumpResult::Ready),
                    "MESSAGE_CREATE" => self.routable(&frame["d"]).map(|message| {
                        message.map_or(PumpResult::Idle, |message| {
                            PumpResult::Message(Box::new(message))
                        })
                    }),
                    _ => Ok(PumpResult::Idle),
                }
            }
            // Discord may ask for an immediate heartbeat independently of the regular cadence.
            1 => {
                self.send_heartbeat().await?;
                if let Some(interval) = self.heartbeat_interval {
                    self.next_heartbeat = Some(Instant::now() + interval);
                }
                Ok(PumpResult::Idle)
            }
            // Reconnect and attempt Resume on the new connection.
            7 => Err(TransportError::Closed),
            // Invalid Session says whether Resume is still meaningful. Either way Discord requires
            // a randomized 1–5 second delay before the next handshake.
            9 => {
                if frame["d"].as_bool() != Some(true) {
                    self.clear_session();
                }
                let seconds = 1 + jitter_below(5);
                tokio::time::sleep(Duration::from_secs(seconds)).await;
                Err(TransportError::Closed)
            }
            10 => {
                self.configure_heartbeat(&frame["d"])?;
                Ok(PumpResult::Idle)
            }
            11 => {
                self.heartbeat_acked = true;
                Ok(PumpResult::Idle)
            }
            _ => Ok(PumpResult::Idle),
        }
    }

    async fn send_heartbeat(&mut self) -> Result<(), TransportError> {
        self.send(json!({ "op": 1, "d": self.sequence })).await?;
        self.heartbeat_acked = false;
        Ok(())
    }

    async fn send(&mut self, value: Value) -> Result<(), TransportError> {
        #[allow(
            clippy::map_err_ignore,
            reason = "serializing a serde_json::Value cannot fail: it holds no non-string map keys \
                      and serde_json::Number rejects non-finite floats"
        )]
        let encoded = serde_json::to_string(&value).map_err(|_| TransportError::Response)?;
        self.socket
            .as_mut()
            .ok_or(TransportError::Closed)?
            .send(Message::text(encoded))
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))
    }

    fn routable(&mut self, message: &Value) -> Result<Option<InboundMessage>, TransportError> {
        let message_type = message["type"].as_u64().unwrap_or_default();
        if !matches!(message_type, 0 | 19) {
            return Ok(None);
        }
        let author = &message["author"];
        if author["bot"].as_bool() == Some(true) || !message["webhook_id"].is_null() {
            return Ok(None);
        }
        let (Some(user_id), Some(channel_id), Some(message_id)) = (
            author["id"].as_str(),
            message["channel_id"].as_str(),
            message["id"].as_str(),
        ) else {
            return Ok(None);
        };
        if !is_snowflake(user_id) || !is_snowflake(channel_id) || !is_snowflake(message_id) {
            return Ok(None);
        }
        if self.identity.user_id.as_deref() == Some(user_id) {
            return Ok(None);
        }
        let text = bound_inbound(message["content"].as_str().unwrap_or_default());
        let assets = pending_assets(
            &message["attachments"],
            &self.replier,
            channel_id,
            message_id,
        );
        if text.trim().is_empty() && assets.is_empty() {
            return Ok(None);
        }
        if !self.seen.insert(message_id.to_owned()) {
            return Ok(None);
        }

        let direct = message["guild_id"].is_null();
        let addressed = direct
            || message["mentions"].as_array().is_some_and(|mentions| {
                mentions
                    .iter()
                    .any(|mention| mention["id"].as_str() == self.identity.user_id.as_deref())
            });
        let conversation = if direct {
            ConversationKind::DirectMessage
        } else {
            // A Discord thread is itself a channel. Its channel id therefore remains both the route
            // key and the conversation id; a catch-all route naturally covers transient threads.
            ConversationKind::Channel(channel_id.to_owned())
        };

        Ok(Some(InboundMessage {
            transport: self.name.clone(),
            transport_kind: ChatTransportKind::Discord,
            subject: ExternalSubject::discord(user_id).map_err(TransportError::Subject)?,
            channel: channel_id.to_owned(),
            thread: None,
            conversation_id: channel_id.to_owned(),
            message_id: message_id.to_owned(),
            text,
            assets,
            conversation,
            addressed: Some(addressed),
            thread_continuation: None,
            reply: ReplyTarget::Discord {
                channel_id: channel_id.to_owned(),
                reply_to: (!direct).then(|| message_id.to_owned()),
            },
            activity: (self.activity == ActivityMode::Native).then(|| ActivityTarget::Discord {
                channel_id: channel_id.to_owned(),
            }),
        }))
    }

    fn clear_session(&mut self) {
        self.sequence = None;
        self.session_id = None;
        self.resume_gateway_url = None;
    }

    fn backoff(&self) -> Duration {
        let step = BASE_BACKOFF.saturating_mul(1_u32 << self.failures.min(7));
        let capped = step.min(MAX_BACKOFF);
        capped.saturating_add(Duration::from_millis(jitter_below(250)))
    }
}

impl ChatTransport for DiscordTransport {
    fn name(&self) -> &str {
        &self.name
    }

    fn connect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>> {
        Box::pin(async move {
            self.discover().await?;
            if let Err(error) = self.open().await {
                self.socket = None;
                return Err(error);
            }
            Ok(self.identity.clone())
        })
    }

    fn next(&mut self) -> BoxFuture<'_, Result<TransportEvent, TransportError>> {
        Box::pin(async move {
            loop {
                if let Some(message) = self.pending.pop_front() {
                    return Ok(TransportEvent::Message(Box::new(message)));
                }
                if self.socket.is_none() {
                    tokio::time::sleep(self.backoff()).await;
                    if let Err(error) = self.open().await {
                        self.socket = None;
                        if is_fatal(&error) {
                            return Err(error);
                        }
                        self.failures = self.failures.saturating_add(1);
                        tracing::warn!(
                            event = "gateway_transport_reconnect_failed",
                            transport = %self.name,
                            category = error.category()
                        );
                        continue;
                    }
                }
                match self.pump().await {
                    Ok(PumpResult::Message(message)) => {
                        return Ok(TransportEvent::Message(message));
                    }
                    Ok(PumpResult::Ready | PumpResult::Idle) => {}
                    Err(error) => {
                        self.socket = None;
                        if is_fatal(&error) {
                            return Err(error);
                        }
                        self.failures = self.failures.saturating_add(1);
                        tracing::warn!(
                            event = "gateway_transport_disconnected",
                            transport = %self.name,
                            category = error.category()
                        );
                    }
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

/// REST and CDN half shared by all in-flight sessions on one Discord transport.
pub(crate) struct DiscordReplier {
    endpoint: String,
    token: Redacted<String>,
    http: reqwest::Client,
    production: bool,
    /// Serializes Create Message calls so reactive rate-limit waits cannot race each other.
    rest_lock: Mutex<()>,
    /// Typing is cosmetic: a 429 suppresses later pulses until Discord's own retry deadline rather
    /// than sleeping under the final-reply lock or delaying the answer.
    activity_cooldown_until: std::sync::Mutex<Option<Instant>>,
}

impl ChatReplier for DiscordReplier {
    fn reply(
        &self,
        target: ReplyTarget,
        reply: OutboundReply,
    ) -> BoxFuture<'_, Result<DeliveryReceipt, TransportError>> {
        Box::pin(async move {
            let ReplyTarget::Discord {
                channel_id,
                reply_to,
            } = target
            else {
                return Err(TransportError::Response);
            };
            if !is_snowflake(&channel_id) || reply_to.as_deref().is_some_and(|id| !is_snowflake(id))
            {
                return Err(TransportError::Response);
            }
            let OutboundReply { text, mut image } = reply;
            let _guard = self.rest_lock.lock().await;
            let mut accepted = 0_usize;
            let mut last_id = None;
            for (index, chunk) in split_message(&text).into_iter().enumerate() {
                let mut body = json!({
                    "content": chunk,
                    "allowed_mentions": {
                        "parse": [],
                        "users": [],
                        "roles": [],
                        "replied_user": false,
                    }
                });
                if index == 0
                    && let Some(message_id) = &reply_to
                {
                    body["message_reference"] = json!({
                        "message_id": message_id,
                        "fail_if_not_exists": false,
                    });
                }
                let result = match image.take() {
                    Some(image) => {
                        self.create_message_with_image(&channel_id, &body, image)
                            .await
                    }
                    None => self.create_message(&channel_id, &body).await,
                };
                match result {
                    Ok(id) => {
                        accepted += 1;
                        last_id = Some(id);
                    }
                    Err(_) if accepted > 0 => return Err(TransportError::PartialDelivery),
                    Err(error) => return Err(error),
                }
            }
            Ok(DeliveryReceipt::new(
                last_id.ok_or(TransportError::Response)?,
            ))
        })
    }
}

impl ChatActivity for DiscordReplier {
    fn show(&self, target: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            let ActivityTarget::Discord { channel_id } = target else {
                return Err(TransportError::Response);
            };
            if !is_snowflake(&channel_id) {
                return Err(TransportError::Response);
            }
            let now = Instant::now();
            if self
                .activity_cooldown_until
                .lock()
                .expect("Discord activity cooldown")
                .is_some_and(|until| until > now)
            {
                return Ok(());
            }
            let response = self
                .http
                .post(format!(
                    "{}/api/v{API_VERSION}/channels/{channel_id}/typing",
                    self.endpoint
                ))
                .header("authorization", format!("Bot {}", self.token.expose()))
                .timeout(ACTIVITY_REQUEST_TIMEOUT)
                .send()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?;
            if response.status().as_u16() == 429 {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|source| TransportError::Request(Box::new(source)))?;
                let body = serde_json::from_slice::<Value>(&bytes)
                    .map_err(TransportError::MalformedResponse)?;
                let seconds = body["retry_after"]
                    .as_f64()
                    .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                    .ok_or(TransportError::Response)?;
                let wait =
                    Duration::from_secs_f64(seconds.min(MAX_ACTIVITY_COOLDOWN.as_secs_f64()));
                *self
                    .activity_cooldown_until
                    .lock()
                    .expect("Discord activity cooldown") = Some(Instant::now() + wait);
                return Err(TransportError::Service {
                    code: "http-429".to_owned(),
                });
            }
            if !response.status().is_success() {
                return Err(TransportError::Service {
                    code: format!("http-{}", response.status().as_u16()),
                });
            }
            *self
                .activity_cooldown_until
                .lock()
                .expect("Discord activity cooldown") = None;
            Ok(())
        })
    }

    fn hide(&self, target: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            if !matches!(target, ActivityTarget::Discord { .. }) {
                return Err(TransportError::Response);
            }
            // Discord exposes no explicit clear. Stopping renewal leaves at most the remainder of
            // the ten-second native lease, and sending the final message clears it sooner.
            Ok(())
        })
    }

    fn refresh_interval(&self) -> Option<Duration> {
        Some(ACTIVITY_REFRESH_INTERVAL)
    }
}

impl DiscordReplier {
    async fn create_message(
        &self,
        channel_id: &str,
        body: &Value,
    ) -> Result<String, TransportError> {
        let url = format!(
            "{}/api/v{API_VERSION}/channels/{channel_id}/messages",
            self.endpoint
        );
        #[allow(
            clippy::map_err_ignore,
            reason = "serializing a serde_json::Value cannot fail: it holds no non-string map keys \
                      and serde_json::Number rejects non-finite floats"
        )]
        let encoded = serde_json::to_vec(body).map_err(|_| TransportError::Response)?;
        let mut retried = false;
        loop {
            let response = self
                .http
                .post(&url)
                .header("authorization", format!("Bot {}", self.token.expose()))
                .header("content-type", "application/json")
                .body(encoded.clone())
                .send()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?;
            if response.status().as_u16() == 429 {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|source| TransportError::Request(Box::new(source)))?;
                let body = serde_json::from_slice::<Value>(&bytes)
                    .map_err(TransportError::MalformedResponse)?;
                let seconds = body["retry_after"]
                    .as_f64()
                    .ok_or(TransportError::Response)?;
                if retried
                    || !seconds.is_finite()
                    || seconds < 0.0
                    || seconds > MAX_RATE_LIMIT_WAIT.as_secs_f64()
                {
                    return Err(TransportError::Service {
                        code: "http-429".to_owned(),
                    });
                }
                let wait = Duration::from_secs_f64(seconds);
                tokio::time::sleep(wait).await;
                retried = true;
                continue;
            }
            let response = decode(response).await?;
            let response_id = response["id"].as_str().ok_or(TransportError::Response)?;
            let response_channel = response["channel_id"]
                .as_str()
                .ok_or(TransportError::Response)?;
            if !is_snowflake(response_id) || response_channel != channel_id {
                return Err(TransportError::Response);
            }
            return Ok(response_id.to_owned());
        }
    }

    async fn create_message_with_image(
        &self,
        channel_id: &str,
        body: &Value,
        image: GeneratedImage,
    ) -> Result<String, TransportError> {
        let url = format!(
            "{}/api/v{API_VERSION}/channels/{channel_id}/messages",
            self.endpoint
        );
        let filename = image.filename().to_owned();
        let media_type = image.media_type().to_owned();
        let bytes = image.into_bytes();
        let mut payload = body.clone();
        payload["attachments"] = json!([{
            "id": 0,
            "filename": filename,
            "description": "Generated image",
        }]);
        #[allow(
            clippy::map_err_ignore,
            reason = "serializing a serde_json::Value cannot fail: it holds no non-string map keys \
                      and serde_json::Number rejects non-finite floats"
        )]
        let payload = serde_json::to_string(&payload).map_err(|_| TransportError::Response)?;
        let mut retried = false;
        loop {
            #[allow(
                clippy::map_err_ignore,
                reason = "mime_str only rejects strings that are not a media type, and \
                          GeneratedImage::media_type returns a fixed IANA type"
            )]
            let part = reqwest::multipart::Part::bytes(bytes.clone())
                .file_name(filename.clone())
                .mime_str(&media_type)
                .map_err(|_| TransportError::Response)?;
            let form = reqwest::multipart::Form::new()
                .text("payload_json", payload.clone())
                .part("files[0]", part);
            let response = self
                .http
                .post(&url)
                .header("authorization", format!("Bot {}", self.token.expose()))
                .multipart(form)
                .send()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?;
            if response.status().as_u16() == 429 {
                let response_bytes = response
                    .bytes()
                    .await
                    .map_err(|source| TransportError::Request(Box::new(source)))?;
                let body = serde_json::from_slice::<Value>(&response_bytes)
                    .map_err(TransportError::MalformedResponse)?;
                let seconds = body["retry_after"]
                    .as_f64()
                    .ok_or(TransportError::Response)?;
                if retried
                    || !seconds.is_finite()
                    || seconds < 0.0
                    || seconds > MAX_RATE_LIMIT_WAIT.as_secs_f64()
                {
                    return Err(TransportError::Service {
                        code: "http-429".to_owned(),
                    });
                }
                tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
                retried = true;
                continue;
            }
            let response = decode(response).await?;
            let response_id = response["id"].as_str().ok_or(TransportError::Response)?;
            let response_channel = response["channel_id"]
                .as_str()
                .ok_or(TransportError::Response)?;
            let accepted_image = response["attachments"]
                .as_array()
                .is_some_and(|attachments| {
                    attachments.len() == 1
                        && attachments[0]["id"].as_str().is_some_and(is_snowflake)
                        && attachments[0]["filename"].as_str() == Some(filename.as_str())
                });
            if !is_snowflake(response_id) || response_channel != channel_id || !accepted_image {
                return Err(TransportError::Response);
            }
            return Ok(response_id.to_owned());
        }
    }

    fn allows_asset_url(&self, raw: &str) -> bool {
        allowed_asset_url(raw, self.production)
    }
}

impl AssetFetcher for DiscordReplier {
    fn fetch(
        &self,
        source: &AssetSourceRef,
        max_bytes: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, TransportError>> {
        let AssetSourceRef::Discord {
            attachment_id,
            channel_id,
            message_id,
            url,
        } = source
        else {
            return Box::pin(async { Err(TransportError::Response) });
        };
        let attachment_id = attachment_id.clone();
        let channel_id = channel_id.clone();
        let message_id = message_id.clone();
        let url = url.clone();
        Box::pin(async move {
            match self.download_asset(&url, max_bytes).await {
                Ok(bytes) => Ok(bytes),
                // Discord attachment URLs are signed and expire. Refresh the source message only
                // on the statuses that can mean the signature is stale; the bot token goes to the
                // pinned REST origin and never to the CDN.
                Err(TransportError::Service { code })
                    if matches!(code.as_str(), "http-401" | "http-403" | "http-404") =>
                {
                    let refreshed = self
                        .refresh_asset_url(&channel_id, &message_id, &attachment_id)
                        .await?;
                    self.download_asset(&refreshed, max_bytes).await
                }
                Err(error) => Err(error),
            }
        })
    }
}

impl DiscordReplier {
    async fn download_asset(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, TransportError> {
        if !self.allows_asset_url(url) {
            return Err(TransportError::Response);
        }
        let mut response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        if !response.status().is_success() {
            return Err(TransportError::Service {
                code: format!("http-{}", response.status().as_u16()),
            });
        }
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
    }

    async fn refresh_asset_url(
        &self,
        channel_id: &str,
        message_id: &str,
        attachment_id: &str,
    ) -> Result<String, TransportError> {
        if !is_snowflake(channel_id) || !is_snowflake(message_id) || !is_snowflake(attachment_id) {
            return Err(TransportError::Response);
        }
        let _guard = self.rest_lock.lock().await;
        let response = self
            .http
            .get(format!(
                "{}/api/v{API_VERSION}/channels/{channel_id}/messages/{message_id}",
                self.endpoint
            ))
            .header("authorization", format!("Bot {}", self.token.expose()))
            .send()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        let body = decode(response).await?;
        let url = body["attachments"]
            .as_array()
            .and_then(|attachments| {
                attachments.iter().find_map(|attachment| {
                    (attachment["id"].as_str() == Some(attachment_id))
                        .then(|| attachment["url"].as_str())
                        .flatten()
                })
            })
            .ok_or(TransportError::Response)?;
        if !self.allows_asset_url(url) {
            return Err(TransportError::Response);
        }
        Ok(url.to_owned())
    }
}

fn pending_assets(
    attachments: &Value,
    replier: &DiscordReplier,
    channel_id: &str,
    message_id: &str,
) -> Vec<PendingAsset> {
    let Some(attachments) = attachments.as_array() else {
        return Vec::new();
    };
    attachments
        .iter()
        .take(MAX_ATTACHMENTS)
        .map(|attachment| {
            let name = attachment["filename"].as_str().unwrap_or("attachment");
            let name = name[..floor_boundary(name, MAX_ATTACHMENT_NAME_BYTES)].to_owned();
            let source = attachment["id"]
                .as_str()
                .zip(attachment["url"].as_str())
                .filter(|(id, url)| is_snowflake(id) && replier.allows_asset_url(url));
            PendingAsset {
                name,
                mime: attachment["content_type"]
                    .as_str()
                    .unwrap_or_default()
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase(),
                size: attachment["size"].as_u64().unwrap_or_default(),
                source: source.map(|(attachment_id, url)| AssetSourceRef::Discord {
                    attachment_id: attachment_id.to_owned(),
                    channel_id: channel_id.to_owned(),
                    message_id: message_id.to_owned(),
                    url: url.to_owned(),
                }),
            }
        })
        .collect()
}

fn split_message(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let rest = &text[start..];
        // Discord enforces this in its UTF-16 implementation. Counting scalar values would let a
        // chunk of 2,000 astral emoji through as 4,000 UTF-16 code units and get the whole answer
        // rejected.
        let mut units = 0;
        let mut end = text.len();
        for (index, character) in rest.char_indices() {
            let next = units + character.len_utf16();
            if next > MAX_MESSAGE_CHARS {
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

fn is_snowflake(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|parsed| parsed != 0 && parsed.to_string() == value)
}

fn jitter_below(upper: u64) -> u64 {
    if upper == 0 {
        return 0;
    }
    let state = RandomState::new();
    let mut hasher = state.build_hasher();
    hasher.write_u32(std::process::id());
    hasher.finish() % upper
}

fn is_fatal(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::Service { code }
            if code == "http-401"
                || code == "http-403"
                || code
                    .strip_prefix("gateway-close-")
                    .and_then(|code| code.parse::<u16>().ok())
                    .is_some_and(|code| FATAL_GATEWAY_CLOSE_CODES.contains(&code))
    )
}

/// Adds the fixed v10 JSON query after proving the service-selected URL cannot receive the token
/// outside Discord (or the explicit loopback test boundary).
fn gateway_url(raw: &str, production: bool) -> Result<String, TransportError> {
    #[allow(
        clippy::map_err_ignore,
        reason = "url::ParseError names a syntax rule in a fixed string, and the three checks \
                  below reject a well-formed but unacceptable gateway URL as the same \
                  TransportError::Response; the distinction an operator acts on is whether \
                  Discord's URL was refused at all"
    )]
    let mut url = reqwest::Url::parse(raw).map_err(|_| TransportError::Response)?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(TransportError::Response);
    }
    let valid = if production {
        let host = url.host_str().unwrap_or_default();
        url.scheme() == "wss"
            && (host == "gateway.discord.gg"
                || (host.starts_with("gateway-") && host.ends_with(".discord.gg")))
    } else {
        url.scheme() == "ws" && is_loopback_host(url.host_str())
    };
    if !valid {
        return Err(TransportError::Response);
    }
    url.query_pairs_mut()
        .clear()
        .append_pair("v", &API_VERSION.to_string())
        .append_pair("encoding", "json");
    Ok(url.into())
}

fn allowed_asset_url(raw: &str, production: bool) -> bool {
    let Ok(url) = reqwest::Url::parse(raw) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    if production {
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| DISCORD_CDN_HOSTS.contains(&host))
    } else {
        url.scheme() == "http" && is_loopback_host(url.host_str())
    }
}

fn is_loopback_host(host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

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

fn client() -> Result<reqwest::Client, TransportError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REST_TIMEOUT)
        .user_agent(concat!(
            "dekopond/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/dekopon-agents/dekopon)"
        ))
        .build()
        .map_err(|source| TransportError::Request(Box::new(source)))
}

/// Decodes a Discord REST response while retaining only a numeric API or HTTP error code.
async fn decode(response: reqwest::Response) -> Result<Value, TransportError> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|source| TransportError::Request(Box::new(source)))?;
    let body =
        serde_json::from_slice::<Value>(&bytes).map_err(TransportError::MalformedResponse)?;
    if status.is_success() {
        return Ok(body);
    }
    let code = body["code"].as_i64().filter(|code| *code != 0).map_or_else(
        || format!("http-{}", status.as_u16()),
        |code| code.to_string(),
    );
    Err(TransportError::Service { code })
}

#[cfg(test)]
mod unit_tests {
    use std::time::Duration;

    use super::{
        DiscordTransport, SessionStarts, allowed_asset_url, gateway_url, is_fatal, split_message,
    };
    use crate::{config::ActivityMode, transport::TransportError};
    use tokio::time::Instant;

    #[test]
    fn gateway_and_asset_urls_are_origin_bounded() {
        assert!(gateway_url("wss://gateway.discord.gg", true).is_ok());
        assert!(gateway_url("wss://gateway-us-east1-b.discord.gg", true).is_ok());
        assert!(gateway_url("wss://gateway.discord.gg.evil.test", true).is_err());
        assert!(gateway_url("ws://127.0.0.1:9000", false).is_ok());
        assert!(gateway_url("ws://127.0.0.1@evil.test", false).is_err());

        assert!(allowed_asset_url(
            "https://cdn.discordapp.com/attachments/1/2/file.png?ex=1",
            true
        ));
        assert!(!allowed_asset_url(
            "https://cdn.discordapp.com.evil.test/attachments/1/2/file.png",
            true
        ));
        assert!(!allowed_asset_url(
            "https://cdn.discordapp.com@evil.test/attachments/1/2/file.png",
            true
        ));
    }

    #[test]
    fn fatal_gateway_close_codes_stop_instead_of_reconnecting_forever() {
        for code in [4004, 4010, 4011, 4012, 4013, 4014] {
            assert!(is_fatal(&TransportError::Service {
                code: format!("gateway-close-{code}"),
            }));
        }
        assert!(!is_fatal(&TransportError::Service {
            code: "gateway-close-4009".to_owned(),
        }));
    }

    #[tokio::test]
    async fn identify_allowance_is_consumed_only_at_the_send_boundary() {
        let mut transport = DiscordTransport::new(
            "discord".to_owned(),
            "http://127.0.0.1:1".to_owned(),
            "test-token".to_owned(),
            ActivityMode::Off,
        )
        .expect("transport builds");
        transport.session_starts = Some(SessionStarts {
            remaining: 1,
            reset_at: Instant::now() + Duration::from_secs(60),
        });

        transport
            .prepare_identify()
            .await
            .expect("one Identify remains");
        assert_eq!(transport.session_starts.expect("limit").remaining, 1);
        transport
            .consume_identify()
            .expect("the send boundary consumes it");
        assert_eq!(transport.session_starts.expect("limit").remaining, 0);
        assert!(transport.prepare_identify().await.is_err());
    }

    #[test]
    fn long_answers_split_without_losing_text() {
        let answer = format!("{}\n{}", "a".repeat(1_999), "🦀".repeat(2_001));
        let chunks = split_message(&answer);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.encode_utf16().count() <= 2_000)
        );
        assert_eq!(chunks.concat(), answer);
    }
}
