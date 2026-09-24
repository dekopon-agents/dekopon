//! Requesting only the GUILD_MESSAGES and DIRECT_MESSAGES intents avoids needing Discord's
//! privileged Message Content intent, since addressing is still decided by the authenticated
//! mentions array, never message text.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use dekopon_agent::CancelVia;
use dekopon_broker_protocol::{ChatTransportKind, Conversation, ConversationKind};
use dekopon_core::{ExternalSubject, Redacted};
use futures_util::{SinkExt as _, StreamExt as _, future::BoxFuture};
use serde_json::{Value, json};
use tokio::{
    net::TcpStream,
    sync::Mutex,
    time::{Instant, timeout},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
use tracing::{Instrument as _, Span};

use crate::{
    asset::{AssetSourceRef, PendingAsset},
    config::{DISCORD_ENDPOINT, LivenessMode, LivenessSettings},
    progress::ProgressText,
    transport::{
        AckToken, AssetFetcher, CancelButton, CancelPress, CancelRequest, ChatDriver, ChatHistory,
        ChatTransport, InboundMessage, InboundReaction, LivenessTarget, MessageRef, OutboundReply,
        PastMessage, ProgressLimits, ProgressMessage, ReplyTarget, SeenIds, StreamLimits,
        StreamedText, TextStream, TextUnit, TransportError, TransportEvent, TransportIdentity,
        TypingLease, asset_buffer, bound_inbound, credential_client, floor_boundary, jitter_below,
        receive_span, record_conversation, reserve_for_chunk, retry_after_from_body, split_message,
    },
};

const API_VERSION: u8 = 10;
const INTENTS: u64 = (1 << 9) | (1 << 12);
const DEDUP_CAPACITY: usize = 1024;
const CHANNEL_SHAPE_CAPACITY: usize = 512;
const CHANNEL_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_ATTACHMENTS: usize = 10;
const MAX_ATTACHMENT_NAME_BYTES: usize = 128;
const MAX_MESSAGE_CHARS: usize = 2_000;
const IDENTIFY_INTERVAL: Duration = Duration::from_secs(5);
const REST_TIMEOUT: Duration = Duration::from_secs(30);
const LIVENESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(8);
const MAX_LIVENESS_COOLDOWN: Duration = Duration::from_secs(300);
const MIN_EDIT_INTERVAL: Duration = Duration::from_secs(2);
const MAX_STREAM_CHARS: usize = 1_900;
const TRUNCATION_MARKER: char = '…';
const LIVENESS_REACTION: &str = "🍊";
const STOPPING_ACK_TEXT: &str = "Stopping…";
const CANCEL_CUSTOM_ID_PREFIX: &str = "stop:";
const MAX_INTERACTION_TOKEN_BYTES: usize = 256;
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(30);
const MAX_REST_COOLDOWN_WAITS: u8 = 2;
const MAX_HISTORY_MESSAGES: usize = 100;
const DISCORD_EPOCH_MILLIS: u64 = 1_420_070_400_000;

const DISCORD_CDN_HOSTS: [&str; 2] = ["cdn.discordapp.com", "media.discordapp.net"];
const FATAL_GATEWAY_CLOSE_CODES: [u16; 6] = [4004, 4010, 4011, 4012, 4013, 4014];

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(crate) struct DiscordTransport {
    name: String,
    endpoint: String,
    token: Redacted<String>,
    http: reqwest::Client,
    driver: Arc<DiscordDriver>,
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
    seen: SeenIds,
    channels: ChannelShapes,
    pending: VecDeque<TransportEvent>,
    native: bool,
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
    Event(TransportEvent),
}

impl DiscordTransport {
    pub(crate) fn new(
        name: String,
        endpoint: String,
        token: String,
        liveness: LivenessSettings,
    ) -> Result<Self, TransportError> {
        let http = client()?;
        let production = endpoint == DISCORD_ENDPOINT;
        Ok(Self {
            name,
            endpoint: endpoint.clone(),
            token: Redacted::new(token.clone()),
            http: http.clone(),
            driver: Arc::new(DiscordDriver {
                endpoint,
                token: Redacted::new(token),
                http,
                production,
                rest_lock: Mutex::new(()),
                rest_cooldown_until: std::sync::Mutex::new(None),
                liveness_cooldown_until: std::sync::Mutex::new(None),
                bot_user: OnceLock::new(),
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
            seen: SeenIds::new(DEDUP_CAPACITY),
            channels: ChannelShapes::new(CHANNEL_SHAPE_CAPACITY),
            pending: VecDeque::new(),
            native: liveness.mode == LivenessMode::Native,
        })
    }

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
        // Validate the gateway URL before retaining it, since the bot token is sent to it on
        // Identify and Resume.
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

    async fn open(&mut self) -> Result<(), TransportError> {
        if self.gateway_url.is_none() || self.session_starts.is_none() {
            self.discover().await?;
        }
        let resuming = self.session_id.is_some() && self.sequence.is_some();
        if !resuming {
            self.clear_session();
            // Identify throttling is checked before opening the socket so a throttled attempt never
            // leaves a live connection without a heartbeat loop.
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
                    return Ok(());
                }
                PumpResult::Event(event) => self.pending.push_back(event),
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
        let jitter = Duration::from_millis(jitter_below(milliseconds));
        self.heartbeat_interval = Some(interval);
        self.next_heartbeat = Some(Instant::now() + jitter);
        self.heartbeat_acked = true;
        Ok(())
    }

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
                        self.resume_gateway_url =
                            Some(gateway_url(resume, self.endpoint == DISCORD_ENDPOINT)?);
                        self.session_id = Some(session_id.to_owned());
                        self.driver.bot_user.get_or_init(|| user_id.to_owned());
                        self.identity = TransportIdentity {
                            user_id: Some(user_id.to_owned()),
                            // Discord mentions are identifier-based and the structured mentions
                            // array is authoritative; a mutable display name is never used as a
                            // fallback.
                            handle: None,
                        };
                        Ok(PumpResult::Ready)
                    }
                    "RESUMED" => Ok(PumpResult::Ready),
                    "MESSAGE_CREATE" => {
                        let received = receive_span(ChatTransportKind::Discord);
                        let routed = self
                            .routable(&frame["d"], &received)
                            .instrument(received.clone())
                            .await?;
                        Ok(match routed {
                            Some(message) => {
                                received.record("message.id", message.message_id.as_str());
                                PumpResult::Event(TransportEvent::Message(Box::new(message)))
                            }
                            None => PumpResult::Idle,
                        })
                    }
                    // The cancel-button acknowledgment must happen before the event reaches the
                    // routing loop, since that handoff blocks on a bounded queue and Discord fails
                    // the interaction after three seconds.
                    "INTERACTION_CREATE" => {
                        let received = receive_span(ChatTransportKind::Discord);
                        let press =
                            received.in_scope(|| self.cancel_press(&frame["d"], &received))?;
                        let Some((press, request)) = press else {
                            return Ok(PumpResult::Idle);
                        };
                        let driver = Arc::clone(&self.driver);
                        if let Err(error) = driver.ack(&press).instrument(received).await {
                            tracing::debug!(
                                event = "gateway_cancel_ack_failed",
                                transport = %self.name,
                                category = error.category()
                            );
                        }
                        Ok(PumpResult::Event(TransportEvent::CancelRequested(request)))
                    }
                    _ => Ok(PumpResult::Idle),
                }
            }
            1 => {
                self.send_heartbeat().await?;
                if let Some(interval) = self.heartbeat_interval {
                    self.next_heartbeat = Some(Instant::now() + interval);
                }
                Ok(PumpResult::Idle)
            }
            7 => Err(TransportError::Closed),
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

    async fn routable(
        &mut self,
        message: &Value,
        received: &Span,
    ) -> Result<Option<InboundMessage>, TransportError> {
        let message_type = message["type"].as_u64().unwrap_or_default();
        if !matches!(message_type, 0 | 19) {
            received.record("drop.reason", "message-type");
            return Ok(None);
        }
        let author = &message["author"];
        if author["bot"].as_bool() == Some(true) || !message["webhook_id"].is_null() {
            received.record("drop.reason", "bot-authored");
            return Ok(None);
        }
        let (Some(user_id), Some(channel_id), Some(message_id)) = (
            author["id"].as_str(),
            message["channel_id"].as_str(),
            message["id"].as_str(),
        ) else {
            received.record("drop.reason", "malformed-envelope");
            return Ok(None);
        };
        if !is_snowflake(user_id) || !is_snowflake(channel_id) || !is_snowflake(message_id) {
            received.record("drop.reason", "malformed-envelope");
            return Ok(None);
        }
        if self.identity.user_id.as_deref() == Some(user_id) {
            received.record("drop.reason", "self-authored");
            return Ok(None);
        }
        let text = bound_inbound(message["content"].as_str().unwrap_or_default());
        let assets = pending_assets(
            &message["attachments"],
            &self.driver,
            channel_id,
            message_id,
        );
        if text.trim().is_empty() && assets.is_empty() {
            received.record("drop.reason", "content-withheld");
            return Ok(None);
        }
        if !self.seen.insert(message_id.to_owned()) {
            received.record("drop.reason", "duplicate");
            return Ok(None);
        }

        let guild = message["guild_id"].as_str().filter(|id| is_snowflake(id));
        let direct = message["guild_id"].is_null();
        let addressed = direct
            || message["mentions"].as_array().is_some_and(|mentions| {
                mentions
                    .iter()
                    .any(|mention| mention["id"].as_str() == self.identity.user_id.as_deref())
            });
        let conversation = match (direct, guild) {
            (true, _) => Conversation {
                kind: ConversationKind::DirectMessage,
                container: None,
                id: channel_id.to_owned(),
                thread: None,
            },
            (false, Some(guild)) => {
                let Some(shape) = self.channel_shape(channel_id).await else {
                    received.record("drop.reason", "conversation-unresolved");
                    return Ok(None);
                };
                match shape {
                    ChannelShape::Channel => Conversation {
                        kind: ConversationKind::Channel,
                        container: Some(guild.to_owned()),
                        id: channel_id.to_owned(),
                        thread: None,
                    },
                    ChannelShape::Thread { parent } => Conversation {
                        kind: ConversationKind::Thread,
                        container: Some(guild.to_owned()),
                        id: parent,
                        thread: Some(channel_id.to_owned()),
                    },
                }
            }
            (false, None) => {
                received.record("drop.reason", "conversation-unresolved");
                return Ok(None);
            }
        };
        record_conversation(received, &conversation);
        // The conversation id is minted once here and carried onto the button rather than
        // re-derived, because a thread's channel id differs from its keyed parent-thread identity.
        let conversation_id = conversation.key();

        Ok(Some(InboundMessage {
            transport: self.name.clone(),
            transport_kind: ChatTransportKind::Discord,
            subject: ExternalSubject::discord(user_id).map_err(TransportError::Subject)?,
            conversation,
            message_id: message_id.to_owned(),
            text,
            assets,
            addressed: Some(addressed),
            thread_continuation: None,
            reply: ReplyTarget::Discord {
                channel_id: channel_id.to_owned(),
                reply_to: (!direct).then(|| message_id.to_owned()),
            },
            liveness: self.native.then(|| LivenessTarget::Discord {
                channel_id: channel_id.to_owned(),
                message_id: message_id.to_owned(),
                conversation_id,
            }),
            receive_span: received.clone(),
            received_at: tokio::time::Instant::now(),
            native_group: None,
            constituents: Vec::new(),
            late_photos: None,
            asset_overflow: message["attachments"]
                .as_array()
                .is_some_and(|files| files.len() > MAX_ATTACHMENTS),
        }))
    }

    /// This never takes the driver's rest_lock, because that lock serializes replies and a 429 here
    /// must not stall the reader that also sends heartbeats.
    async fn channel_shape(&mut self, channel_id: &str) -> Option<ChannelShape> {
        if let Some(shape) = self.channels.get(channel_id) {
            return Some(shape);
        }
        if self.driver.rest_cooldown().is_some() {
            tracing::debug!(
                event = "gateway_conversation_unresolved",
                transport = %self.name,
                cause = "rest-cooldown"
            );
            return None;
        }
        let response = timeout(
            CHANNEL_LOOKUP_TIMEOUT,
            self.http
                .get(format!(
                    "{}/api/v{API_VERSION}/channels/{channel_id}",
                    self.endpoint
                ))
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bot {}", self.token.expose()),
                )
                .send(),
        )
        .await;
        let bytes = match response {
            Ok(Ok(response)) if response.status().is_success() => response.bytes().await,
            Ok(Ok(response)) => {
                tracing::debug!(
                    event = "gateway_conversation_unresolved",
                    transport = %self.name,
                    cause = "status",
                    status = response.status().as_u16()
                );
                return None;
            }
            Ok(Err(source)) => {
                tracing::debug!(
                    event = "gateway_conversation_unresolved",
                    transport = %self.name,
                    cause = "request",
                    cause_type = %source
                );
                return None;
            }
            Err(_elapsed) => {
                tracing::debug!(
                    event = "gateway_conversation_unresolved",
                    transport = %self.name,
                    cause = "timeout"
                );
                return None;
            }
        };
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(source) => {
                tracing::debug!(
                    event = "gateway_conversation_unresolved",
                    transport = %self.name,
                    cause = "body",
                    cause_type = %source
                );
                return None;
            }
        };
        let body = match serde_json::from_slice::<Value>(&bytes) {
            Ok(body) => body,
            Err(source) => {
                tracing::debug!(
                    event = "gateway_conversation_unresolved",
                    transport = %self.name,
                    cause = "body",
                    cause_type = %source
                );
                return None;
            }
        };
        let Some(shape) = ChannelShape::of(&body) else {
            tracing::debug!(
                event = "gateway_conversation_unresolved",
                transport = %self.name,
                cause = "channel-type"
            );
            return None;
        };
        self.channels.insert(channel_id.to_owned(), shape.clone());
        Some(shape)
    }

    fn cancel_press(
        &self,
        interaction: &Value,
        received: &Span,
    ) -> Result<Option<(CancelPress, CancelRequest)>, TransportError> {
        // Interaction type 3 is a message component and component type 2 is a button; anything else
        // is an application command or modal this gateway never registered.
        if interaction["type"].as_u64() != Some(3)
            || interaction["data"]["component_type"].as_u64() != Some(2)
        {
            received.record("drop.reason", "not-a-component-press");
            return Ok(None);
        }
        let custom_id = interaction["data"]["custom_id"]
            .as_str()
            .unwrap_or_default();
        let Some(conversation_id) = custom_id
            .strip_prefix(CANCEL_CUSTOM_ID_PREFIX)
            .filter(|conversation| is_conversation_key(conversation))
        else {
            received.record("drop.reason", "not-a-cancel-button");
            return Ok(None);
        };
        let presser = if interaction["member"].is_null() {
            &interaction["user"]
        } else {
            &interaction["member"]["user"]
        };
        let (Some(user_id), Some(interaction_id), Some(token), Some(message_id)) = (
            presser["id"].as_str(),
            interaction["id"].as_str(),
            interaction["token"].as_str(),
            interaction["message"]["id"].as_str(),
        ) else {
            received.record("drop.reason", "malformed-envelope");
            return Ok(None);
        };
        if !is_snowflake(user_id)
            || !is_snowflake(interaction_id)
            || !is_snowflake(message_id)
            || !is_interaction_token(token)
        {
            received.record("drop.reason", "malformed-envelope");
            return Ok(None);
        }
        let channel_id = key_channel(conversation_id);
        if interaction["channel_id"].as_str() != Some(channel_id) {
            received.record("drop.reason", "conversation-mismatch");
            return Ok(None);
        }
        if self.identity.user_id.as_deref() == Some(user_id) {
            received.record("drop.reason", "self-authored");
            return Ok(None);
        }
        received.record("message.id", message_id);
        let subject = ExternalSubject::discord(user_id).map_err(TransportError::Subject)?;
        let press = CancelPress {
            target: LivenessTarget::Discord {
                channel_id: channel_id.to_owned(),
                message_id: message_id.to_owned(),
                conversation_id: conversation_id.to_owned(),
            },
            subject: subject.canonical(),
            ack: AckToken::Discord {
                interaction_id: interaction_id.to_owned(),
                interaction_token: token.to_owned(),
            },
        };
        let request = CancelRequest {
            transport: self.name.clone(),
            conversation_id: conversation_id.to_owned(),
            subject: subject.canonical(),
            via: CancelVia::Button,
        };
        Ok(Some((press, request)))
    }

    fn clear_session(&mut self) {
        self.sequence = None;
        self.session_id = None;
        self.resume_gateway_url = None;
    }
}

impl ChatTransport for DiscordTransport {
    fn name(&self) -> &str {
        &self.name
    }

    fn connect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>> {
        Box::pin(async move {
            self.socket = None;
            if self.gateway_url.is_none() {
                self.discover().await?;
            }
            if let Err(error) = self.open().await {
                self.socket = None;
                return Err(error);
            }
            Ok(self.identity.clone())
        })
    }

    fn retryable(&self, error: &TransportError) -> bool {
        !is_fatal(error)
    }

    fn reconnect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>> {
        Box::pin(async move {
            self.socket = None;
            self.open().await?;
            Ok(self.identity.clone())
        })
    }

    fn next(&mut self) -> BoxFuture<'_, Result<TransportEvent, TransportError>> {
        Box::pin(async move {
            loop {
                if let Some(event) = self.pending.pop_front() {
                    return Ok(event);
                }
                match self.pump().await {
                    Ok(PumpResult::Event(event)) => return Ok(event),
                    Ok(PumpResult::Ready | PumpResult::Idle) => {}
                    Err(error) => {
                        self.socket = None;
                        return Err(error);
                    }
                }
            }
        })
    }

    fn driver(&self) -> Arc<dyn ChatDriver> {
        Arc::clone(&self.driver) as Arc<dyn ChatDriver>
    }

    fn asset_fetcher(&self) -> Option<Arc<dyn AssetFetcher>> {
        Some(Arc::clone(&self.driver) as Arc<dyn AssetFetcher>)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ChannelShape {
    Channel,
    Thread { parent: String },
}

impl ChannelShape {
    fn of(channel: &Value) -> Option<Self> {
        match channel["type"].as_u64()? {
            0 | 2 | 5 => Some(Self::Channel),
            10..=12 => channel["parent_id"]
                .as_str()
                .filter(|parent| is_snowflake(parent))
                .map(|parent| Self::Thread {
                    parent: parent.to_owned(),
                }),
            _ => None,
        }
    }
}

struct ChannelShapes {
    order: VecDeque<String>,
    shapes: HashMap<String, ChannelShape>,
    capacity: usize,
}

impl ChannelShapes {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            shapes: HashMap::with_capacity(capacity),
            capacity,
        }
    }

    fn get(&self, channel_id: &str) -> Option<ChannelShape> {
        self.shapes.get(channel_id).cloned()
    }

    fn insert(&mut self, channel_id: String, shape: ChannelShape) {
        if self.shapes.insert(channel_id.clone(), shape).is_none() {
            self.order.push_back(channel_id);
            if self.order.len() > self.capacity
                && let Some(evicted) = self.order.pop_front()
            {
                self.shapes.remove(&evicted);
            }
        }
    }
}

pub(crate) struct DiscordDriver {
    endpoint: String,
    token: Redacted<String>,
    http: reqwest::Client,
    production: bool,
    /// This lock is held across one request only, because sleeping under it for a whole reply
    /// blocked every other session's answer and any in-flight attachment fetch.
    rest_lock: Mutex<()>,
    rest_cooldown_until: std::sync::Mutex<Option<Instant>>,
    liveness_cooldown_until: std::sync::Mutex<Option<Instant>>,
    bot_user: OnceLock<String>,
}

#[async_trait]
impl ChatDriver for DiscordDriver {
    async fn reply(
        &self,
        target: &ReplyTarget,
        reply: OutboundReply,
    ) -> Result<(), TransportError> {
        let OutboundReply { text, images } = reply;
        super::hydration::validate_types(&images, super::hydration::AcceptedTypes::Files)?;
        let mut images = super::hydration::ImageQueue::new(images);
        let ReplyTarget::Discord {
            channel_id,
            reply_to,
        } = target
        else {
            return Err(TransportError::Response);
        };
        if !is_snowflake(channel_id) || reply_to.as_deref().is_some_and(|id| !is_snowflake(id)) {
            return Err(TransportError::Response);
        }
        let mut accepted = false;
        for (index, chunk) in split_message(&text, MAX_MESSAGE_CHARS, TextUnit::Utf16)
            .into_iter()
            .enumerate()
        {
            let mut body = json!({
                "content": chunk,
                "allowed_mentions": allowed_mentions_none(),
            });
            if index == 0
                && let Some(message_id) = reply_to
            {
                body["message_reference"] = json!({
                    "message_id": message_id,
                    "fail_if_not_exists": false,
                });
            }
            let result = if images.is_empty() {
                self.create_message(channel_id, &body).await
            } else {
                self.create_message_with_images(channel_id, &body, std::mem::take(&mut images))
                    .await
            };
            match result {
                Ok(()) => accepted = true,
                Err(_) if accepted => return Err(TransportError::PartialDelivery),
                Err(error) => return Err(error),
            }
        }
        accepted.then_some(()).ok_or(TransportError::Response)
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

    fn history(&self) -> Option<&dyn ChatHistory> {
        Some(self)
    }
}

#[async_trait]
impl ChatHistory for DiscordDriver {
    async fn recent(
        &self,
        conversation: &Conversation,
        before: &str,
        limit: usize,
    ) -> Result<Vec<PastMessage>, TransportError> {
        let bot = self.bot_user.get().ok_or(TransportError::Closed)?;
        let channel_id = conversation.api_channel(ChatTransportKind::Discord);
        if !is_snowflake(channel_id) || !is_snowflake(before) {
            return Err(TransportError::Response);
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = limit.min(MAX_HISTORY_MESSAGES).to_string();
        let response = self
            .send_rest(
                self.http
                    .get(format!(
                        "{}/api/v{API_VERSION}/channels/{channel_id}/messages",
                        self.endpoint
                    ))
                    .query(&[("before", before), ("limit", limit.as_str())])
                    .header("authorization", format!("Bot {}", self.token.expose())),
            )
            .await?;
        if response.status().as_u16() == 429 {
            let bytes = response
                .bytes()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?;
            let body = serde_json::from_slice::<Value>(&bytes)
                .map_err(TransportError::MalformedResponse)?;
            if let Some(retry) = retry_after_from_body(&body, MAX_RATE_LIMIT_WAIT) {
                self.publish_rest_cooldown(retry.wait);
            }
            return Err(TransportError::Service {
                code: "http-429".to_owned(),
            });
        }
        let body = decode(response).await?;
        let messages = body.as_array().ok_or(TransportError::Response)?;
        Ok(messages
            .iter()
            .rev()
            .filter_map(|message| self.past_message(message, bot))
            .collect())
    }
}

impl DiscordDriver {
    fn past_message(&self, message: &Value, bot: &str) -> Option<PastMessage> {
        if !matches!(message["type"].as_u64().unwrap_or_default(), 0 | 19) {
            return None;
        }
        let author = message["author"]["id"]
            .as_str()
            .filter(|id| is_snowflake(id))?;
        let channel_id = message["channel_id"]
            .as_str()
            .filter(|id| is_snowflake(id))?;
        let message_id = message["id"].as_str().filter(|id| is_snowflake(id))?;
        let text = bound_inbound(message["content"].as_str().unwrap_or_default());
        let assets = pending_assets(&message["attachments"], self, channel_id, message_id);
        if text.trim().is_empty() && assets.is_empty() {
            return None;
        }
        Some(PastMessage {
            from_bot: author == bot,
            author: author.to_owned(),
            text,
            assets,
            at: snowflake_instant(message_id)?,
        })
    }
}

fn snowflake_instant(id: &str) -> Option<SystemTime> {
    let millis = (id.parse::<u64>().ok()? >> 22).checked_add(DISCORD_EPOCH_MILLIS)?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(millis))
}

#[async_trait]
impl TypingLease for DiscordDriver {
    fn renew_every(&self) -> Duration {
        TYPING_REFRESH_INTERVAL
    }

    async fn renew(&self, target: &LivenessTarget) -> Result<(), TransportError> {
        let (channel_id, _, _) = discord_coordinates(target)?;
        if self.cooling_down() {
            return Ok(());
        }
        self.liveness_empty(self.http.post(format!(
            "{}/api/v{API_VERSION}/channels/{channel_id}/typing",
            self.endpoint
        )))
        .await
    }
}

#[async_trait]
impl InboundReaction for DiscordDriver {
    async fn set(&self, target: &LivenessTarget, present: bool) -> Result<(), TransportError> {
        let (channel_id, message_id, _) = discord_coordinates(target)?;
        let url = format!(
            "{}/api/v{API_VERSION}/channels/{channel_id}/messages/{message_id}/reactions/{}/@me",
            self.endpoint,
            percent_encoded(LIVENESS_REACTION)
        );
        let request = if present {
            self.http.put(url)
        } else {
            self.http.delete(url)
        };
        self.liveness_empty(request).await
    }
}

#[async_trait]
impl ProgressMessage for DiscordDriver {
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
        let content = one_message(text.as_str()).ok_or(TransportError::Response)?;
        self.post_liveness_message(target, &content, cancel).await
    }

    async fn edit(
        &self,
        message: &MessageRef,
        text: &ProgressText,
        cancel: bool,
    ) -> Result<(), TransportError> {
        let content = one_message(text.as_str()).ok_or(TransportError::Response)?;
        self.edit_liveness_message(message, &content, cancel).await
    }

    async fn delete(&self, message: &MessageRef) -> Result<(), TransportError> {
        let (channel_id, _, _) = discord_coordinates(&message.target)?;
        if !is_snowflake(&message.id) {
            return Err(TransportError::Response);
        }
        self.liveness_empty(self.http.delete(format!(
            "{}/api/v{API_VERSION}/channels/{channel_id}/messages/{}",
            self.endpoint, message.id
        )))
        .await
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
impl TextStream for DiscordDriver {
    fn limits(&self) -> StreamLimits {
        StreamLimits {
            min_interval: MIN_EDIT_INTERVAL,
            max_chars: MAX_STREAM_CHARS,
        }
    }

    async fn show(
        &self,
        target: &LivenessTarget,
        message: Option<&MessageRef>,
        text: &StreamedText,
        cancel: bool,
    ) -> Result<MessageRef, TransportError> {
        let content = one_message(&shown_text(text)).ok_or(TransportError::Response)?;
        match message {
            Some(message) => {
                self.edit_liveness_message(message, &content, cancel)
                    .await?;
                Ok(message.clone())
            }
            None => self.post_liveness_message(target, &content, cancel).await,
        }
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
impl CancelButton for DiscordDriver {
    async fn ack(&self, press: &CancelPress) -> Result<(), TransportError> {
        let LivenessTarget::Discord { .. } = &press.target else {
            return Err(TransportError::Response);
        };
        let AckToken::Discord {
            interaction_id,
            interaction_token,
        } = &press.ack
        else {
            return Err(TransportError::Response);
        };
        if !is_snowflake(interaction_id) || !is_interaction_token(interaction_token) {
            return Err(TransportError::Response);
        }
        // Type 7, UPDATE_MESSAGE, answers the interaction and rewrites the message in one call, so
        // a deferred update cannot leave a window for a second press.
        let body = json!({
            "type": 7,
            "data": {
                "content": STOPPING_ACK_TEXT,
                "components": [],
                "allowed_mentions": allowed_mentions_none(),
            }
        });
        let response = self
            .http
            .post(format!(
                "{}/api/v{API_VERSION}/interactions/{interaction_id}/{interaction_token}/callback",
                self.endpoint
            ))
            .header("content-type", "application/json")
            .body(encode(&body)?)
            .timeout(LIVENESS_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(TransportError::Service {
            code: format!("http-{}", response.status().as_u16()),
        })
    }
}

impl DiscordDriver {
    fn cooling_down(&self) -> bool {
        self.liveness_cooldown_until
            .lock()
            .expect("Discord liveness cooldown")
            .is_some_and(|until| until > Instant::now())
    }

    async fn liveness_send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, TransportError> {
        if self.cooling_down() {
            return Err(TransportError::Service {
                code: "liveness-cooldown".to_owned(),
            });
        }
        let response = request
            .header("authorization", format!("Bot {}", self.token.expose()))
            .timeout(LIVENESS_REQUEST_TIMEOUT)
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
            let retry = retry_after_from_body(&body, MAX_LIVENESS_COOLDOWN)
                .ok_or(TransportError::Response)?;
            *self
                .liveness_cooldown_until
                .lock()
                .expect("Discord liveness cooldown") = Some(Instant::now() + retry.wait);
            return Err(TransportError::Service {
                code: "http-429".to_owned(),
            });
        }
        if response.status().is_success() {
            *self
                .liveness_cooldown_until
                .lock()
                .expect("Discord liveness cooldown") = None;
        }
        Ok(response)
    }

    async fn liveness_empty(&self, request: reqwest::RequestBuilder) -> Result<(), TransportError> {
        let response = self.liveness_send(request).await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(TransportError::Service {
            code: format!("http-{}", response.status().as_u16()),
        })
    }

    async fn post_liveness_message(
        &self,
        target: &LivenessTarget,
        content: &str,
        cancel: bool,
    ) -> Result<MessageRef, TransportError> {
        let (channel_id, inbound_message_id, conversation_id) = discord_coordinates(target)?;
        let mut body = liveness_body(content, cancel, conversation_id);
        body["message_reference"] = json!({
            "message_id": inbound_message_id,
            "fail_if_not_exists": false,
        });
        let response = decode(
            self.liveness_send(
                self.http
                    .post(format!(
                        "{}/api/v{API_VERSION}/channels/{channel_id}/messages",
                        self.endpoint
                    ))
                    .header("content-type", "application/json")
                    .body(encode(&body)?),
            )
            .await?,
        )
        .await?;
        let id = response["id"]
            .as_str()
            .filter(|id| is_snowflake(id))
            .ok_or(TransportError::Response)?;
        if response["channel_id"].as_str() != Some(channel_id) {
            return Err(TransportError::Response);
        }
        Ok(MessageRef {
            target: target.clone(),
            id: id.to_owned(),
        })
    }

    async fn edit_liveness_message(
        &self,
        message: &MessageRef,
        content: &str,
        cancel: bool,
    ) -> Result<(), TransportError> {
        let (channel_id, _, conversation_id) = discord_coordinates(&message.target)?;
        if !is_snowflake(&message.id) {
            return Err(TransportError::Response);
        }
        let body = liveness_body(content, cancel, conversation_id);
        self.liveness_empty(
            self.http
                .patch(format!(
                    "{}/api/v{API_VERSION}/channels/{channel_id}/messages/{}",
                    self.endpoint, message.id
                ))
                .header("content-type", "application/json")
                .body(encode(&body)?),
        )
        .await
    }

    async fn finalize_in_place(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError> {
        if !reply.images.is_empty() {
            return Err(TransportError::Service {
                code: "answer-has-attachments".to_owned(),
            });
        }
        let chunks = split_message(&reply.text, MAX_MESSAGE_CHARS, TextUnit::Utf16);
        let [content] = chunks.as_slice() else {
            return Err(TransportError::Service {
                code: "answer-too-long".to_owned(),
            });
        };
        self.edit_liveness_message(message, content, false).await
    }

    async fn create_message(&self, channel_id: &str, body: &Value) -> Result<(), TransportError> {
        let url = format!(
            "{}/api/v{API_VERSION}/channels/{channel_id}/messages",
            self.endpoint
        );
        let encoded = encode(body)?;
        let mut retried = false;
        loop {
            let response = self.post_message(&url, &encoded).await?;
            if response.status().as_u16() == 429 {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|source| TransportError::Request(Box::new(source)))?;
                let body = serde_json::from_slice::<Value>(&bytes)
                    .map_err(TransportError::MalformedResponse)?;
                let retry = retry_after_from_body(&body, MAX_RATE_LIMIT_WAIT)
                    .ok_or(TransportError::Response)?;
                if retried || retry.capped {
                    return Err(TransportError::Service {
                        code: "http-429".to_owned(),
                    });
                }
                self.publish_rest_cooldown(retry.wait);
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
            return Ok(());
        }
    }

    async fn create_message_with_images(
        &self,
        channel_id: &str,
        body: &Value,
        mut images: super::hydration::ImageQueue,
    ) -> Result<(), TransportError> {
        let url = format!(
            "{}/api/v{API_VERSION}/channels/{channel_id}/messages",
            self.endpoint
        );
        let attachments = images.filenames();
        let mut payload = body.clone();
        payload["attachments"] = Value::Array(
            attachments
                .iter()
                .enumerate()
                .map(|(index, filename)| {
                    json!({
                        "id": index,
                        "filename": filename,
                        "description": "Attachment",
                    })
                })
                .collect(),
        );
        #[allow(
            clippy::map_err_ignore,
            reason = "serializing a serde_json::Value cannot fail: it holds no non-string map keys \
                      and serde_json::Number rejects non-finite floats"
        )]
        let payload = serde_json::to_string(&payload).map_err(|_| TransportError::Response)?;
        let mut retried = false;
        loop {
            let read = images.read_all().await?;
            let mut form = reqwest::multipart::Form::new().text("payload_json", payload.clone());
            for (index, image) in read.into_iter().enumerate() {
                #[allow(
                    clippy::map_err_ignore,
                    reason = "mime_str only rejects strings that are not a media type, and \
                              GeneratedImage::media_type returns a fixed IANA type"
                )]
                let part = reqwest::multipart::Part::bytes(image.bytes)
                    .file_name(image.filename)
                    .mime_str(&image.media_type)
                    .map_err(|_| TransportError::Response)?;
                form = form.part(format!("files[{index}]"), part);
            }
            let response = self
                .send_rest(
                    self.http
                        .post(&url)
                        .header("authorization", format!("Bot {}", self.token.expose()))
                        .multipart(form),
                )
                .await?;
            if response.status().as_u16() == 429 {
                let response_bytes = response
                    .bytes()
                    .await
                    .map_err(|source| TransportError::Request(Box::new(source)))?;
                let body = serde_json::from_slice::<Value>(&response_bytes)
                    .map_err(TransportError::MalformedResponse)?;
                let retry = retry_after_from_body(&body, MAX_RATE_LIMIT_WAIT)
                    .ok_or(TransportError::Response)?;
                if retried || retry.capped {
                    return Err(TransportError::Service {
                        code: "http-429".to_owned(),
                    });
                }
                self.publish_rest_cooldown(retry.wait);
                retried = true;
                continue;
            }
            let response = decode(response).await?;
            let response_id = response["id"].as_str().ok_or(TransportError::Response)?;
            let response_channel = response["channel_id"]
                .as_str()
                .ok_or(TransportError::Response)?;
            let accepted_images = response["attachments"].as_array().is_some_and(|accepted| {
                accepted.len() == attachments.len()
                    && accepted.iter().zip(&attachments).all(|(value, filename)| {
                        value["id"].as_str().is_some_and(is_snowflake)
                            && value["filename"].as_str() == Some(filename.as_str())
                    })
            });
            if !is_snowflake(response_id) || response_channel != channel_id || !accepted_images {
                return Err(TransportError::Response);
            }
            return Ok(());
        }
    }

    async fn post_message(
        &self,
        url: &str,
        encoded: &[u8],
    ) -> Result<reqwest::Response, TransportError> {
        self.send_rest(
            self.http
                .post(url)
                .header("authorization", format!("Bot {}", self.token.expose()))
                .header("content-type", "application/json")
                .body(encoded.to_vec()),
        )
        .await
    }

    async fn send_rest(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, TransportError> {
        for _ in 0..MAX_REST_COOLDOWN_WAITS {
            let Some(wait) = self.rest_cooldown() else {
                break;
            };
            tokio::time::sleep(wait).await;
        }
        let _guard = self.rest_lock.lock().await;
        request
            .send()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))
    }

    fn publish_rest_cooldown(&self, wait: Duration) {
        *self
            .rest_cooldown_until
            .lock()
            .expect("Discord REST cooldown") = Some(Instant::now() + wait);
    }

    fn rest_cooldown(&self) -> Option<Duration> {
        let until = (*self
            .rest_cooldown_until
            .lock()
            .expect("Discord REST cooldown"))?;
        until.checked_duration_since(Instant::now())
    }

    fn allows_asset_url(&self, raw: &str) -> bool {
        allowed_asset_url(raw, self.production)
    }
}

impl AssetFetcher for DiscordDriver {
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
                // Discord attachment URLs are signed and expire, so the source message is refreshed
                // only on statuses meaning the signature is stale; the bot token never reaches the
                // CDN.
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

impl DiscordDriver {
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
        let limit = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        let mut body = asset_buffer(response.content_length(), limit);
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
            reserve_for_chunk(&mut body, chunk.len(), limit);
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
        let response = {
            let _guard = self.rest_lock.lock().await;
            self.http
                .get(format!(
                    "{}/api/v{API_VERSION}/channels/{channel_id}/messages/{message_id}",
                    self.endpoint
                ))
                .header("authorization", format!("Bot {}", self.token.expose()))
                .send()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?
        };
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
    driver: &DiscordDriver,
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
                .filter(|(id, url)| is_snowflake(id) && driver.allows_asset_url(url));
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
                size: attachment["size"].as_u64(),
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

fn discord_coordinates(target: &LivenessTarget) -> Result<(&str, &str, &str), TransportError> {
    let LivenessTarget::Discord {
        channel_id,
        message_id,
        conversation_id,
    } = target
    else {
        return Err(TransportError::Response);
    };
    if !is_snowflake(channel_id)
        || !is_snowflake(message_id)
        || !is_conversation_key(conversation_id)
    {
        return Err(TransportError::Response);
    }
    Ok((channel_id, message_id, conversation_id))
}

fn is_conversation_key(value: &str) -> bool {
    value.split_once(':').map_or_else(
        || is_snowflake(value),
        |(parent, thread)| is_snowflake(parent) && is_snowflake(thread),
    )
}

/// A Discord thread is itself a channel for REST purposes; always address the thread, never its
/// parent, or calls fail.
fn key_channel(conversation_id: &str) -> &str {
    conversation_id
        .split_once(':')
        .map_or(conversation_id, |(_, thread)| thread)
}

fn liveness_body(content: &str, cancel: bool, conversation_id: &str) -> Value {
    json!({
        "content": content,
        // Discord re-notifies a channel when an edit introduces a mention, so mentions are
        // suppressed on every edit, not only the first post.
        "allowed_mentions": allowed_mentions_none(),
        "components": cancel_components(cancel, conversation_id),
    })
}

fn allowed_mentions_none() -> Value {
    json!({
        "parse": [],
        "users": [],
        "roles": [],
        "replied_user": false,
    })
}

fn cancel_components(cancel: bool, conversation_id: &str) -> Value {
    if !cancel {
        return json!([]);
    }
    json!([{
        "type": 1,
        "components": [{
            "type": 2,
            "style": 4,
            "label": "Stop",
            "custom_id": format!("{CANCEL_CUSTOM_ID_PREFIX}{conversation_id}"),
        }],
    }])
}

fn shown_text(text: &StreamedText) -> String {
    let mut shown = text.text.as_str().to_owned();
    if text.truncated {
        shown.push(TRUNCATION_MARKER);
    }
    shown
}

fn one_message(text: &str) -> Option<String> {
    split_message(text, MAX_MESSAGE_CHARS, TextUnit::Utf16)
        .into_iter()
        .next()
}

#[allow(
    clippy::map_err_ignore,
    reason = "serializing a serde_json::Value cannot fail: it holds no non-string map keys and \
              serde_json::Number rejects non-finite floats"
)]
fn encode(body: &Value) -> Result<Vec<u8>, TransportError> {
    serde_json::to_vec(body).map_err(|_| TransportError::Response)
}

/// This deliberately avoids form_urlencoded, which encodes a space as a plus sign, a literal plus
/// rather than a space in a URL path segment.
fn percent_encoded(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn is_interaction_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_INTERACTION_TOKEN_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_graphic() && !matches!(byte, b'/' | b'?' | b'#' | b'%' | b'\\')
        })
}

fn is_snowflake(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|parsed| parsed != 0 && parsed.to_string() == value)
}

fn is_fatal(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::Service { code }
            if code == "session-start-limit-exhausted"
                || code == "http-401"
                || code == "http-403"
                || code
                    .strip_prefix("gateway-close-")
                    .and_then(|code| code.parse::<u16>().ok())
                    .is_some_and(|code| FATAL_GATEWAY_CLOSE_CODES.contains(&code))
    )
}

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

fn client() -> Result<reqwest::Client, TransportError> {
    credential_client(REST_TIMEOUT)
        .user_agent(concat!(
            "dekopond/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/dekopon-agents/dekopon)"
        ))
        .build()
        .map_err(|source| TransportError::Request(Box::new(source)))
}

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

    use dekopon_agent::{CancelVia, attachment::GeneratedImage};
    use dekopon_broker_protocol::ChatTransportKind;
    use dekopon_core::Redacted;
    use dekopon_test_support::CaptureLayer;
    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        sync::Mutex,
        time::Instant,
    };
    use tracing::Instrument as _;
    use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

    use super::{
        CANCEL_CUSTOM_ID_PREFIX, ChannelShape, Conversation, ConversationKind, DiscordDriver,
        DiscordTransport, MAX_HISTORY_MESSAGES, MAX_MESSAGE_CHARS, SessionStarts, TextUnit,
        allowed_asset_url, cancel_components, client, gateway_url, is_fatal, is_interaction_token,
        percent_encoded, split_message,
    };
    use crate::{
        config::{LivenessMode, LivenessSettings},
        progress::ProgressText,
        transport::{
            AckToken, CancelPress, ChatDriver as _, ChatHistory as _, LivenessTarget, MessageRef,
            OutboundReply, StreamedText, TransportError, TransportIdentity, receive_span,
        },
    };

    const MAX_CUSTOM_ID_BYTES: usize = 100;

    #[derive(Debug)]
    struct Recorded {
        method: String,
        path: String,
        head: String,
        body: Value,
    }

    fn loopback(replies: Vec<(u16, String)>) -> (String, tokio::task::JoinHandle<Vec<Recorded>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the stand-in");
        let address = listener.local_addr().expect("the bound address");
        listener
            .set_nonblocking(true)
            .expect("a non-blocking listener");
        let listener = tokio::net::TcpListener::from_std(listener).expect("a tokio listener");
        let server = tokio::spawn(async move {
            let mut recorded = Vec::new();
            for (status, body) in replies {
                let (mut stream, _) = listener.accept().await.expect("one connection per request");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 8192];
                loop {
                    let read = stream.read(&mut buffer).await.expect("read");
                    assert!(read > 0, "a complete request arrives");
                    request.extend_from_slice(&buffer[..read]);
                    let Some(split) = request.windows(4).position(|window| window == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let head_end = split + 4;
                    let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let line = line.to_ascii_lowercase();
                            line.strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or_default();
                    if request.len() < head_end + length {
                        continue;
                    }
                    let mut words = head.split_whitespace();
                    let method = words.next().expect("a method").to_owned();
                    let path = words.next().expect("a path").to_owned();
                    let body = if length == 0 {
                        Value::Null
                    } else {
                        serde_json::from_slice(&request[head_end..head_end + length])
                            .expect("a JSON request body")
                    };
                    recorded.push(Recorded {
                        method,
                        path,
                        head,
                        body,
                    });
                    break;
                }
                let response = format!(
                    "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.expect("write");
            }
            recorded
        });
        (format!("http://{address}"), server)
    }

    fn transport(name: &str) -> DiscordTransport {
        DiscordTransport::new(
            name.to_owned(),
            UNREACHABLE.to_owned(),
            "test-token".to_owned(),
            LivenessSettings {
                mode: LivenessMode::Native,
                ..LivenessSettings::default()
            },
        )
        .expect("transport builds")
    }

    const UNREACHABLE: &str = "http://127.0.0.1:1";
    const BOT_USER: &str = "999";

    fn driver(endpoint: &str) -> DiscordDriver {
        DiscordDriver {
            endpoint: endpoint.to_owned(),
            token: Redacted::new("bot-secret".to_owned()),
            http: client().expect("the shared client builds"),
            production: false,
            rest_lock: Mutex::new(()),
            rest_cooldown_until: std::sync::Mutex::new(None),
            liveness_cooldown_until: std::sync::Mutex::new(None),
            bot_user: std::sync::OnceLock::from(BOT_USER.to_owned()),
        }
    }

    fn target() -> LivenessTarget {
        LivenessTarget::Discord {
            channel_id: "100".to_owned(),
            message_id: "200".to_owned(),
            conversation_id: "100".to_owned(),
        }
    }

    fn progress_message() -> MessageRef {
        MessageRef {
            target: target(),
            id: "555".to_owned(),
        }
    }

    fn progress_text(text: &str) -> ProgressText {
        ProgressText::for_test(text)
    }

    fn streamed() -> StreamedText {
        let events = dekopon_model::events_from_transcript(
            dekopon_test_support::OPENAI_CHAT_COMPLETIONS_TWO_DELTAS,
        )
        .expect("the recorded transcript parses");
        StreamedText {
            text: dekopon_test_support::scripted_text(&events),
            truncated: false,
        }
    }

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
        let mut transport = transport("discord");
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
        let exhausted = transport
            .prepare_identify()
            .await
            .expect_err("Identify budget spent");
        assert!(
            is_fatal(&exhausted),
            "recovery must not retry a spent Identify budget"
        );
    }

    #[test]
    fn long_answers_split_without_losing_text() {
        let answer = format!("{}\n{}", "a".repeat(1_999), "🦀".repeat(2_001));
        let chunks = split_message(&answer, MAX_MESSAGE_CHARS, TextUnit::Utf16);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.encode_utf16().count() <= 2_000)
        );
        assert_eq!(chunks.concat(), answer);
    }

    #[tokio::test]
    async fn typing_and_the_reaction_are_the_calls_discord_documents() {
        let (endpoint, server) = loopback(vec![
            (204, String::new()),
            (204, String::new()),
            (204, String::new()),
        ]);
        let driver = driver(&endpoint);

        driver
            .typing()
            .expect("Discord renews a typing lease")
            .renew(&target())
            .await
            .expect("the pulse is accepted");
        let reaction = driver.reaction().expect("Discord reacts to the question");
        reaction.set(&target(), true).await.expect("added");
        reaction.set(&target(), false).await.expect("removed");

        let recorded = server.await.expect("the stand-in joins");
        assert_eq!(recorded[0].method, "POST");
        assert_eq!(recorded[0].path, "/api/v10/channels/100/typing");
        assert!(
            recorded[0]
                .head
                .to_ascii_lowercase()
                .contains("authorization: bot bot-secret")
        );
        assert_eq!(recorded[1].method, "PUT");
        assert_eq!(
            recorded[1].path,
            "/api/v10/channels/100/messages/200/reactions/%F0%9F%8D%8A/@me"
        );
        assert_eq!(recorded[2].method, "DELETE");
        assert_eq!(recorded[2].path, recorded[1].path);
    }

    #[tokio::test]
    async fn a_progress_message_is_posted_edited_and_deleted() {
        let (endpoint, server) = loopback(vec![
            (200, json!({ "id": "555", "channel_id": "100" }).to_string()),
            (200, "{}".to_owned()),
            (204, String::new()),
        ]);
        let driver = driver(&endpoint);
        let progress = driver.progress().expect("Discord posts progress messages");

        let message = progress
            .post(&target(), &progress_text("Working on it…"), true)
            .await
            .expect("the progress message is posted");
        assert_eq!(message, progress_message());
        progress
            .edit(&message, &progress_text("Running gpt-image…"), true)
            .await
            .expect("the same message is rewritten");
        progress.delete(&message).await.expect("and removed");

        let recorded = server.await.expect("the stand-in joins");
        assert_eq!(recorded[0].method, "POST");
        assert_eq!(recorded[0].path, "/api/v10/channels/100/messages");
        assert_eq!(recorded[0].body["content"], "Working on it…");
        assert_eq!(
            recorded[0].body["message_reference"]["message_id"], "200",
            "the surface answers the question it was asked"
        );
        let button = &recorded[0].body["components"][0]["components"][0];
        assert_eq!(button["type"], 2);
        assert_eq!(button["style"], 4, "a stop control is danger-styled");
        assert_eq!(button["custom_id"], format!("{CANCEL_CUSTOM_ID_PREFIX}100"));
        assert_eq!(recorded[1].method, "PATCH");
        assert_eq!(recorded[1].path, "/api/v10/channels/100/messages/555");
        assert_eq!(recorded[1].body["content"], "Running gpt-image…");
        for request in &recorded[..2] {
            assert_eq!(
                request.body["allowed_mentions"]["parse"],
                json!([]),
                "no edit of gateway text may ping a channel"
            );
        }
        assert_eq!(recorded[2].method, "DELETE");
        assert_eq!(recorded[2].path, "/api/v10/channels/100/messages/555");
    }

    #[tokio::test]
    async fn a_streamed_answer_grows_in_one_message() {
        let (endpoint, server) = loopback(vec![
            (200, json!({ "id": "555", "channel_id": "100" }).to_string()),
            (200, "{}".to_owned()),
        ]);
        let driver = driver(&endpoint);
        let stream = driver
            .stream()
            .expect("Discord streams by cumulative edits");
        let text = streamed();

        let message = stream
            .show(&target(), None, &text, true)
            .await
            .expect("the first delta posts one message");
        let again = stream
            .show(&target(), Some(&message), &text, true)
            .await
            .expect("a later delta edits the same message");
        assert_eq!(again, message, "the answer stays in one message");

        let recorded = server.await.expect("the stand-in joins");
        assert_eq!(recorded[0].method, "POST");
        assert_eq!(recorded[1].method, "PATCH");
        assert_eq!(recorded[1].path, "/api/v10/channels/100/messages/555");
        for request in &recorded {
            assert_eq!(
                request.body["content"],
                text.text.as_str(),
                "the cumulative text is what is on screen"
            );
            assert_eq!(request.body["allowed_mentions"]["parse"], json!([]));
        }
    }

    #[tokio::test]
    async fn a_cut_stream_carries_the_truncation_marker() {
        let (endpoint, server) = loopback(vec![(
            200,
            json!({ "id": "555", "channel_id": "100" }).to_string(),
        )]);
        let driver = driver(&endpoint);
        let text = StreamedText {
            truncated: true,
            ..streamed()
        };

        driver
            .stream()
            .expect("Discord streams by cumulative edits")
            .show(&target(), None, &text, false)
            .await
            .expect("the first delta posts one message");

        let recorded = server.await.expect("the stand-in joins");
        assert_eq!(
            recorded[0].body["content"],
            format!("{}…", text.text.as_str()),
            "the marker is what says the text was cut"
        );
    }

    #[tokio::test]
    async fn an_answer_finalizes_in_place_or_says_why_it_cannot() {
        let (endpoint, server) = loopback(vec![(200, "{}".to_owned())]);
        let driver = driver(&endpoint);
        let progress = driver.progress().expect("Discord posts progress messages");

        progress
            .finalize(&progress_message(), &OutboundReply::text("the answer"))
            .await
            .expect("the answer replaces the progress message");

        let too_long = progress
            .finalize(
                &progress_message(),
                &OutboundReply::text("x".repeat(MAX_MESSAGE_CHARS + 1)),
            )
            .await
            .expect_err("an answer past the ceiling is more than one message");
        assert!(
            matches!(&too_long, TransportError::Service { code } if code == "answer-too-long"),
            "{too_long:?}"
        );

        let png = GeneratedImage::from_png(b"\x89PNG\r\n\x1a\n".to_vec()).expect("a PNG fixture");
        let attached = progress
            .finalize(
                &progress_message(),
                &OutboundReply::with_images("here it is", vec![png]),
            )
            .await
            .expect_err("only a fresh Create Message uploads attachments");
        assert!(
            matches!(&attached, TransportError::Service { code } if code == "answer-has-attachments"),
            "{attached:?}"
        );

        let recorded = server.await.expect("the stand-in joins");
        assert_eq!(
            recorded.len(),
            1,
            "a refusal that cannot land in place sends nothing"
        );
        assert_eq!(recorded[0].method, "PATCH");
        assert_eq!(recorded[0].body["content"], "the answer");
        assert_eq!(
            recorded[0].body["components"],
            json!([]),
            "the stop control goes with the answer"
        );
    }

    #[tokio::test]
    async fn a_press_is_acknowledged_by_rewriting_the_message_it_was_on() {
        let (endpoint, server) = loopback(vec![(204, String::new())]);
        let driver = driver(&endpoint);

        driver
            .cancel_button()
            .expect("Discord has a stop button")
            .ack(&CancelPress {
                target: target(),
                subject: "discord.42".to_owned(),
                ack: AckToken::Discord {
                    interaction_id: "300".to_owned(),
                    interaction_token: "interaction-token-1".to_owned(),
                },
            })
            .await
            .expect("acknowledged inside the deadline");

        let recorded = server.await.expect("the stand-in joins");
        assert_eq!(recorded[0].method, "POST");
        assert_eq!(
            recorded[0].path,
            "/api/v10/interactions/300/interaction-token-1/callback"
        );
        assert_eq!(recorded[0].body["type"], 7);
        assert_eq!(recorded[0].body["data"]["content"], "Stopping…");
        assert_eq!(recorded[0].body["data"]["components"], json!([]));
        assert!(
            !recorded[0]
                .head
                .to_ascii_lowercase()
                .contains("authorization"),
            "the interaction token is the credential this call needs"
        );
    }

    #[tokio::test]
    async fn an_acknowledgment_token_that_is_not_discords_is_refused() {
        let driver = driver(UNREACHABLE);
        let button = driver.cancel_button().expect("Discord has a stop button");

        for ack in [
            AckToken::Local,
            AckToken::Telegram {
                callback_query_id: "9".to_owned(),
            },
            AckToken::Discord {
                interaction_id: "300".to_owned(),
                interaction_token: "../../channels/100/messages".to_owned(),
            },
            AckToken::Discord {
                interaction_id: "not-a-snowflake".to_owned(),
                interaction_token: "interaction-token-1".to_owned(),
            },
        ] {
            let refused = button
                .ack(&CancelPress {
                    target: target(),
                    subject: "discord.42".to_owned(),
                    ack,
                })
                .await
                .expect_err("the acknowledgment is refused");
            assert_eq!(refused.category(), "response");
        }

        assert!(is_interaction_token("aW50ZXJhY3Rpb246MTIz.abc-_~"));
        assert!(!is_interaction_token(""));
        assert!(!is_interaction_token("has space"));
        assert!(!is_interaction_token("has/slash"));
    }

    #[test]
    fn only_this_gateways_button_becomes_a_cancel_request() {
        let mut transport = transport("elote");
        transport.identity = TransportIdentity {
            user_id: Some("999".to_owned()),
            handle: None,
        };
        let press = |overrides: Value| {
            let mut interaction = json!({
                "id": "300",
                "token": "interaction-token-1",
                "type": 3,
                "channel_id": "100",
                "data": { "component_type": 2, "custom_id": "stop:100" },
                "message": { "id": "555" },
                "member": { "user": { "id": "42" } },
            });
            for (key, value) in overrides.as_object().expect("an object of overrides") {
                interaction[key] = value.clone();
            }
            interaction
        };

        let span = receive_span(ChatTransportKind::Discord);
        let (pressed, request) = transport
            .cancel_press(&press(json!({})), &span)
            .expect("a well-formed envelope")
            .expect("this gateway's own button");
        assert_eq!(request.transport, "elote");
        assert_eq!(request.conversation_id, "100");
        assert_eq!(request.subject, "discord.42");
        assert_eq!(request.via, CancelVia::Button);
        assert_eq!(pressed.subject, request.subject);
        assert_eq!(
            pressed.ack,
            AckToken::Discord {
                interaction_id: "300".to_owned(),
                interaction_token: "interaction-token-1".to_owned(),
            }
        );
        assert_eq!(
            pressed.target,
            LivenessTarget::Discord {
                channel_id: "100".to_owned(),
                message_id: "555".to_owned(),
                conversation_id: "100".to_owned(),
            },
            "the press names the message the button was on"
        );

        let direct = press(json!({ "member": null, "user": { "id": "42" } }));
        assert!(
            transport
                .cancel_press(&direct, &span)
                .expect("a well-formed envelope")
                .is_some()
        );

        for ignored in [
            press(json!({ "type": 2 })),
            press(json!({ "data": { "component_type": 2, "custom_id": "vote:100" } })),
            press(json!({ "channel_id": "101" })),
            press(json!({ "token": null })),
            press(json!({ "member": { "user": { "id": "999" } } })),
        ] {
            assert!(
                transport
                    .cancel_press(&ignored, &span)
                    .expect("a refusal is not a transport failure")
                    .is_none(),
                "{ignored} should not stop a run"
            );
        }
    }

    #[tokio::test]
    async fn a_stop_pressed_in_a_thread_names_the_conversation_routing_minted() {
        let mut transport = transport("elote");
        transport.identity = TransportIdentity {
            user_id: Some("999".to_owned()),
            handle: None,
        };
        transport.channels.insert(
            "100".to_owned(),
            ChannelShape::Thread {
                parent: "50".to_owned(),
            },
        );
        let span = receive_span(ChatTransportKind::Discord);
        let routed = transport
            .routable(
                &message(json!({ "guild_id": "300", "mentions": [{ "id": "999" }] })),
                &span,
            )
            .instrument(span.clone())
            .await
            .expect("a routable message")
            .expect("the thread places the message");
        drop(span);
        let target = routed.liveness.clone().expect("a live Discord message");

        let (endpoint, server) = loopback(vec![(
            200,
            json!({ "id": "555", "channel_id": "100" }).to_string(),
        )]);
        driver(&endpoint)
            .progress()
            .expect("Discord posts progress messages")
            .post(&target, &progress_text("Working on it…"), true)
            .await
            .expect("the progress message is posted");
        let recorded = server.await.expect("the stand-in joins");
        let custom_id = recorded[0].body["components"][0]["components"][0]["custom_id"]
            .as_str()
            .expect("the button carries a custom_id")
            .to_owned();

        let span = receive_span(ChatTransportKind::Discord);
        let (_, request) = transport
            .cancel_press(
                &json!({
                    "id": "300",
                    "token": "interaction-token-1",
                    "type": 3,
                    "channel_id": "100",
                    "data": { "component_type": 2, "custom_id": custom_id },
                    "message": { "id": "555" },
                    "member": { "user": { "id": "42" } },
                }),
                &span,
            )
            .expect("a well-formed envelope")
            .expect("this gateway's own button");

        assert_eq!(request.conversation_id, routed.conversation.key());
        assert_eq!(request.conversation_id, "50:100");
        assert_eq!(request.via, CancelVia::Button);
    }

    #[test]
    fn a_stop_buttons_custom_id_fits_discords_ceiling() {
        let components = cancel_components(true, &format!("{0}:{0}", u64::MAX));
        let widest = components[0]["components"][0]["custom_id"]
            .as_str()
            .expect("the button carries a custom_id");
        assert!(widest.starts_with(CANCEL_CUSTOM_ID_PREFIX), "{widest}");
        assert!(widest.len() <= MAX_CUSTOM_ID_BYTES, "{widest}");
        assert_eq!(percent_encoded("🍊"), "%F0%9F%8D%8A");
        assert_eq!(percent_encoded("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[tokio::test]
    async fn every_dropped_message_records_why_on_its_receive_span() {
        let mut transport = transport("elote");
        transport.identity = TransportIdentity {
            user_id: Some("999".to_owned()),
            handle: None,
        };
        let capture = CaptureLayer::workspace();
        let _subscriber = tracing_subscriber::registry()
            .with(capture.clone())
            .set_default();

        for (dropped, reason) in [
            (message(json!({ "type": 6 })), "message-type"),
            (
                message(json!({ "author": { "id": "7", "bot": true } })),
                "bot-authored",
            ),
            (message(json!({ "channel_id": null })), "malformed-envelope"),
            (
                message(json!({ "author": { "id": "999" } })),
                "self-authored",
            ),
            (message(json!({ "content": "  " })), "content-withheld"),
        ] {
            capture.clear();
            let span = receive_span(ChatTransportKind::Discord);
            let routed = transport
                .routable(&dropped, &span)
                .instrument(span.clone())
                .await
                .expect("a drop is not a transport failure");
            assert!(routed.is_none(), "{dropped} routed");
            drop(span);
            assert_reason(&capture, reason);
        }
        let span = receive_span(ChatTransportKind::Discord);
        assert!(
            transport
                .routable(&message(json!({})), &span)
                .instrument(span.clone())
                .await
                .expect("a routable message")
                .is_some(),
            "an ordinary message routes"
        );
        drop(span);
        capture.clear();
        let span = receive_span(ChatTransportKind::Discord);
        assert!(
            transport
                .routable(&message(json!({})), &span)
                .instrument(span.clone())
                .await
                .expect("a routable message")
                .is_none(),
            "the same identifier twice is the redelivery a resume replays"
        );
        drop(span);
        assert_reason(&capture, "duplicate");
    }

    #[tokio::test]
    async fn a_guild_message_that_cannot_be_placed_is_dropped_as_conversation_unresolved() {
        let mut transport = transport("elote");
        transport.identity = TransportIdentity {
            user_id: Some("999".to_owned()),
            handle: None,
        };
        let capture = CaptureLayer::workspace();
        let _subscriber = tracing_subscriber::registry()
            .with(capture.clone())
            .set_default();

        let span = receive_span(ChatTransportKind::Discord);
        let routed = transport
            .routable(
                &message(json!({ "guild_id": "300", "mentions": [{ "id": "999" }] })),
                &span,
            )
            .instrument(span.clone())
            .await
            .expect("a drop is not a transport failure");
        assert!(
            routed.is_none(),
            "a guild message was placed without a lookup"
        );
        drop(span);
        assert_reason(&capture, "conversation-unresolved");

        transport.channels.insert(
            "100".to_owned(),
            ChannelShape::Thread {
                parent: "50".to_owned(),
            },
        );
        let span = receive_span(ChatTransportKind::Discord);
        let routed = transport
            .routable(
                &message(json!({
                    "id": "201",
                    "guild_id": "300",
                    "mentions": [{ "id": "999" }]
                })),
                &span,
            )
            .instrument(span.clone())
            .await
            .expect("a routable message")
            .expect("the cached shape places the message");
        assert_eq!(routed.conversation.kind, ConversationKind::Thread);
        assert_eq!(routed.conversation.container.as_deref(), Some("300"));
        assert_eq!(routed.conversation.id, "50");
        assert_eq!(routed.conversation.thread.as_deref(), Some("100"));
        assert_eq!(routed.conversation.key(), "50:100");
    }

    fn message(overrides: Value) -> Value {
        let mut message = json!({
            "type": 0,
            "id": "200",
            "channel_id": "100",
            "content": "hello",
            "author": { "id": "42" },
            "attachments": [],
        });
        for (key, value) in overrides.as_object().expect("an object of overrides") {
            message[key] = value.clone();
        }
        message
    }

    fn assert_reason(capture: &CaptureLayer, reason: &str) {
        let spans = capture.spans_text();
        assert!(
            spans.contains(&format!("drop.reason=\"{reason}\"")),
            "expected drop.reason={reason} in {spans}"
        );
    }

    #[tokio::test]
    async fn a_routed_message_carries_its_liveness_coordinates() {
        let identity = TransportIdentity {
            user_id: Some("999".to_owned()),
            handle: None,
        };
        let mut publishing = transport("elote");
        publishing.identity = identity.clone();
        let mut quiet = DiscordTransport::new(
            "elote".to_owned(),
            UNREACHABLE.to_owned(),
            "test-token".to_owned(),
            LivenessSettings::default(),
        )
        .expect("transport builds");
        quiet.identity = identity;

        let span = receive_span(ChatTransportKind::Discord);
        let routed = publishing
            .routable(&message(json!({})), &span)
            .instrument(span.clone())
            .await
            .expect("a routable message")
            .expect("the message routes");
        assert_eq!(
            routed.liveness,
            Some(LivenessTarget::Discord {
                channel_id: "100".to_owned(),
                message_id: "200".to_owned(),
                conversation_id: "100".to_owned(),
            })
        );

        let span = receive_span(ChatTransportKind::Discord);
        let routed = quiet
            .routable(&message(json!({})), &span)
            .instrument(span.clone())
            .await
            .expect("a routable message")
            .expect("the message still routes");
        assert!(
            routed.liveness.is_none(),
            "liveness off leaves nothing for the policy to render on"
        );
    }

    fn thread_conversation() -> Conversation {
        Conversation {
            kind: ConversationKind::Thread,
            container: Some("77".to_owned()),
            id: "100".to_owned(),
            thread: Some("300".to_owned()),
        }
    }

    fn direct_conversation() -> Conversation {
        Conversation {
            kind: ConversationKind::DirectMessage,
            container: None,
            id: "300".to_owned(),
            thread: None,
        }
    }

    fn photo() -> Value {
        json!([{
            "id": "61",
            "filename": "cat.png",
            "content_type": "image/png; charset=binary",
            "size": 42,
            "url": "http://127.0.0.1:1/attachments/cat.png",
        }])
    }

    fn posted(id: &str, author: &str, content: &str) -> Value {
        message(
            json!({ "id": id, "channel_id": "300", "author": { "id": author }, "content": content }),
        )
    }

    #[tokio::test]
    async fn a_thread_is_recalled_oldest_first_from_the_messages_before_the_trigger() {
        let mut shared = posted("1100000004194304000", "42", "");
        shared["attachments"] = photo();
        let mut joined = posted("1100000008388608000", "42", "joined");
        joined["type"] = json!(7);
        let newest_first = json!([
            posted("1100000012582912000", BOT_USER, "a cat"),
            joined,
            shared,
            posted("1100000000000000000", "42", "hello"),
        ]);
        let (endpoint, server) = loopback(vec![(200, newest_first.to_string())]);

        let recalled = driver(&endpoint)
            .history()
            .expect("Discord reads its own history")
            .recent(&thread_conversation(), "1100000016777216000", 4)
            .await
            .expect("the history reads");

        let recorded = server.await.expect("the stand-in joins");
        assert_eq!(recorded[0].method, "GET");
        assert_eq!(
            recorded[0].path,
            "/api/v10/channels/300/messages?before=1100000016777216000&limit=4"
        );
        assert!(
            recorded[0]
                .head
                .to_ascii_lowercase()
                .contains("authorization: bot bot-secret")
        );
        let seen = recalled
            .iter()
            .map(|message| {
                (
                    message.from_bot,
                    message.author.as_str(),
                    message.text.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            seen,
            [
                (false, "42", "hello"),
                (false, "42", ""),
                (true, BOT_USER, "a cat")
            ]
        );
        assert!(recalled.windows(2).all(|pair| pair[0].at < pair[1].at));
    }

    #[tokio::test]
    async fn a_recalled_attachment_is_the_asset_the_inbound_path_would_have_built() {
        let mut shared = posted("170", "42", "look");
        shared["attachments"] = photo();
        let (endpoint, server) = loopback(vec![(200, json!([shared.clone()]).to_string())]);
        let mut transport = transport("elote");
        transport.identity = TransportIdentity {
            user_id: Some(BOT_USER.to_owned()),
            handle: None,
        };
        let inbound = transport
            .routable(&shared, &tracing::Span::none())
            .await
            .expect("a routable message")
            .expect("the message routes");

        let recalled = driver(&endpoint)
            .recent(&direct_conversation(), "200", 10)
            .await
            .expect("the history reads");
        server.await.expect("the stand-in joins");

        assert_eq!(recalled.len(), 1);
        assert!(!recalled[0].assets.is_empty());
        assert_eq!(recalled[0].assets, inbound.assets);
    }

    #[tokio::test]
    async fn a_history_read_never_asks_for_more_than_discords_page_ceiling() {
        let (endpoint, server) = loopback(vec![(200, "[]".to_owned()), (200, "[]".to_owned())]);
        let driver = driver(&endpoint);

        for limit in [MAX_HISTORY_MESSAGES, MAX_HISTORY_MESSAGES + 1] {
            driver
                .recent(&direct_conversation(), "200", limit)
                .await
                .expect("the history reads");
        }

        let recorded = server.await.expect("the stand-in joins");
        let ceiling =
            format!("/api/v10/channels/300/messages?before=200&limit={MAX_HISTORY_MESSAGES}");
        assert_eq!(recorded[0].path, ceiling);
        assert_eq!(recorded[1].path, ceiling);
    }

    #[tokio::test]
    async fn a_throttled_history_read_fails_and_publishes_the_rest_cooldown() {
        let (endpoint, server) = loopback(vec![(
            429,
            json!({ "retry_after": 1.5, "global": false }).to_string(),
        )]);
        let driver = driver(&endpoint);

        let throttled = driver.recent(&direct_conversation(), "200", 10).await;
        server.await.expect("the stand-in joins");

        assert!(
            matches!(&throttled, Err(TransportError::Service { code }) if code == "http-429"),
            "{throttled:?}"
        );
        assert!(
            driver.rest_cooldown().is_some(),
            "replies wait out the same limit"
        );
    }

    #[tokio::test]
    async fn a_refused_history_read_is_an_error_naming_discords_code() {
        let (endpoint, server) = loopback(vec![(
            403,
            json!({ "code": 50001, "message": "Missing Access" }).to_string(),
        )]);

        let refused = driver(&endpoint)
            .recent(&direct_conversation(), "200", 10)
            .await;
        server.await.expect("the stand-in joins");

        assert!(
            matches!(&refused, Err(TransportError::Service { code }) if code == "50001"),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn history_is_unreadable_until_the_bot_knows_who_it_is() {
        let mut driver = driver(UNREACHABLE);
        driver.bot_user = std::sync::OnceLock::new();

        let early = driver.recent(&direct_conversation(), "200", 10).await;

        assert!(matches!(early, Err(TransportError::Closed)), "{early:?}");
    }
}
