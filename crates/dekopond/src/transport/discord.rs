//! Discord Gateway v10: an outbound event WebSocket paired with REST replies.
//!
//! The transport requests only `GUILD_MESSAGES` and `DIRECT_MESSAGES`. Discord exposes message
//! content and attachments without the privileged Message Content intent in direct messages and in
//! guild messages that mention the bot, which is exactly the surface this gateway routes. The
//! structured `mentions` array decides whether a guild message is addressed; model-visible text is
//! never trusted to make that decision.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use async_trait::async_trait;
use dekopon_agent::CancelVia;
use dekopon_agent::attachment::GeneratedImage;
use dekopon_broker_protocol::ChatTransportKind;
use dekopon_core::{ExternalSubject, Redacted};
use futures_util::{SinkExt as _, StreamExt as _, future::BoxFuture};
use serde_json::{Value, json};
use tokio::{net::TcpStream, sync::Mutex, time::Instant};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
use tracing::{Instrument as _, Span};

use crate::{
    asset::{AssetSourceRef, PendingAsset},
    config::{DISCORD_ENDPOINT, LivenessMode, LivenessSettings},
    progress::ProgressText,
    transport::{
        AckToken, AssetFetcher, CancelButton, CancelPress, CancelRequest, ChatDriver,
        ChatTransport, ConversationKind, InboundMessage, InboundReaction, LivenessTarget,
        MessageRef, OutboundReply, ProgressLimits, ProgressMessage, ReplyTarget, SeenIds,
        StreamLimits, StreamedText, TextStream, TextUnit, TransportError, TransportEvent,
        TransportIdentity, TypingLease, bound_inbound, credential_client, floor_boundary,
        jitter_below, receive_span, reconnect_delay, retry_after_from_body, split_message,
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
const REST_TIMEOUT: Duration = Duration::from_secs(30);
/// Deadline on every cosmetic call: typing, a reaction, a progress edit, a press acknowledgment.
///
/// Shorter than Discord's three-second interaction deadline on purpose, so an acknowledgment that
/// is going to fail fails while the press can still be answered another way.
const LIVENESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Renewal interval inside Discord's ten-second typing lease.
const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(8);
/// Ceiling on a cosmetic 429 cooldown; past this the surface simply stays quiet.
const MAX_LIVENESS_COOLDOWN: Duration = Duration::from_secs(300);
/// Floor between two edits of one message, per the design's per-driver budget.
const MIN_EDIT_INTERVAL: Duration = Duration::from_secs(2);
/// Ceiling on streamed text, leaving the 2,000-character message room for a trailer.
const MAX_STREAM_CHARS: usize = 1_900;
/// What a streamed message shows where the policy cut the model's text.
const TRUNCATION_MARKER: char = '…';
/// The gateway's own reaction, the same tangerine the Slack fallback uses.
const LIVENESS_REACTION: &str = "🍊";
/// What a pressed cancel button says while the policy is winning the cancellation race.
///
/// Fixed rather than an operator template: it is written by the transport reader, which has no
/// session and no configuration, inside the three seconds Discord allows. The operator's own
/// stopped line follows from the policy, which owns the templates and the terminal write.
const STOPPING_ACK_TEXT: &str = "Stopping…";
/// Prefix on the cancel button's `custom_id`, which carries the conversation it stops.
const CANCEL_CUSTOM_ID_PREFIX: &str = "stop:";
/// Discord's ceiling on an interaction token, which this transport puts in a URL path.
const MAX_INTERACTION_TOKEN_BYTES: usize = 256;
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(30);
/// How many published rate-limit deadlines one Create Message waits out before sending anyway.
const MAX_REST_COOLDOWN_WAITS: u8 = 2;

const DISCORD_CDN_HOSTS: [&str; 2] = ["cdn.discordapp.com", "media.discordapp.net"];
const FATAL_GATEWAY_CLOSE_CODES: [u16; 6] = [4004, 4010, 4011, 4012, 4013, 4014];

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One Discord bot connection.
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
    pending: VecDeque<TransportEvent>,
    failures: u32,
    /// `liveness.mode: native` — an inbound message carries coordinates for transient signals.
    ///
    /// One decision rather than the whole block: what Discord can render is fixed, and progress,
    /// stream, cancel button, keep-alive, and templates are read by the policy that drives the
    /// driver. Withholding the coordinates is how `off` keeps this transport reply-only.
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
    /// Takes the bot-token value after the caller resolved its configured environment variable.
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
            pending: VecDeque::new(),
            failures: 0,
            native: liveness.mode == LivenessMode::Native,
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
        // Discord requires a random first-heartbeat jitter in [0, interval), so a fleet that
        // connected together does not heartbeat in lockstep.
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
                    // One span per inbound message event, opened before the payload is read, so
                    // the loop-prevention and addressing decisions that may drop it are inside the
                    // event's own trace rather than nowhere. Heartbeats and session control open
                    // none: they carry no message and route nothing.
                    "MESSAGE_CREATE" => {
                        let received = receive_span(ChatTransportKind::Discord);
                        let routed = received.in_scope(|| self.routable(&frame["d"], &received))?;
                        Ok(match routed {
                            Some(message) => {
                                received.record("message.id", message.message_id.as_str());
                                PumpResult::Event(TransportEvent::Message(Box::new(message)))
                            }
                            None => PumpResult::Idle,
                        })
                    }
                    // A press of this gateway's own cancel button. The acknowledgment happens
                    // here, in the reader, before the event is handed to the routing loop: that
                    // hand-off waits on a 64-slot queue, and Discord gives an interaction three
                    // seconds before it shows the presser "This interaction failed".
                    "INTERACTION_CREATE" => {
                        let received = receive_span(ChatTransportKind::Discord);
                        let press =
                            received.in_scope(|| self.cancel_press(&frame["d"], &received))?;
                        let Some((press, request)) = press else {
                            return Ok(PumpResult::Idle);
                        };
                        let driver = Arc::clone(&self.driver);
                        if let Err(error) = driver.ack(&press).instrument(received).await {
                            // Still routed. A refused acknowledgment costs the presser the
                            // immediate "Stopping…" on the button, not the cancellation itself.
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

    fn routable(
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
            // The message identifier rides along because the reaction goes on the message being
            // answered rather than on the channel. Which surfaces are then used is the policy's
            // decision from the rest of the block, not a second gate here.
            liveness: self.native.then(|| LivenessTarget::Discord {
                channel_id: channel_id.to_owned(),
                message_id: message_id.to_owned(),
            }),
            receive_span: received.clone(),
        }))
    }

    /// Reads one component interaction as a press of this gateway's own cancel button.
    ///
    /// Everything comes from the interaction envelope: the presser from `member.user` in a guild
    /// or `user` in a direct message, the conversation from the `custom_id` this transport wrote
    /// on the button itself, and the acknowledgment coordinates from the interaction. Whether the
    /// presser is the person whose run this is remains the routing loop's comparison — only it
    /// knows which subject registered the session on that conversation — and anyone else's press
    /// is acknowledged and then ignored there.
    ///
    /// `Ok(None)` is a press this gateway did not put on screen, or an envelope missing a field
    /// the acknowledgment needs; each records why on the receive span.
    fn cancel_press(
        &self,
        interaction: &Value,
        received: &Span,
    ) -> Result<Option<(CancelPress, CancelRequest)>, TransportError> {
        // Interaction type 3 is a message component, and component type 2 is a button. Anything
        // else is an application command or a modal this gateway never registered.
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
            .filter(|conversation| is_snowflake(conversation))
        else {
            received.record("drop.reason", "not-a-cancel-button");
            return Ok(None);
        };
        // A guild interaction carries the presser under `member`; a direct message has no member
        // object and carries the same user at the top level.
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
        // The button lives on a message in the conversation it stops. A press arriving from
        // anywhere else is a copy of the component, and stopping a run from another channel on
        // the strength of it is not something this transport does.
        if interaction["channel_id"].as_str() != Some(conversation_id) {
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
                channel_id: conversation_id.to_owned(),
                message_id: message_id.to_owned(),
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
                if let Some(event) = self.pending.pop_front() {
                    return Ok(event);
                }
                if self.socket.is_none() {
                    tokio::time::sleep(reconnect_delay(self.failures)).await;
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
                    Ok(PumpResult::Event(event)) => {
                        return Ok(event);
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

    fn driver(&self) -> Arc<dyn ChatDriver> {
        Arc::clone(&self.driver) as Arc<dyn ChatDriver>
    }

    fn asset_fetcher(&self) -> Option<Arc<dyn AssetFetcher>> {
        Some(Arc::clone(&self.driver) as Arc<dyn AssetFetcher>)
    }
}

/// REST and CDN half shared by all in-flight sessions on one Discord transport.
pub(crate) struct DiscordDriver {
    endpoint: String,
    token: Redacted<String>,
    http: reqwest::Client,
    production: bool,
    /// Serializes REST requests so reactive rate-limit waits cannot race each other.
    ///
    /// Held across one request and nothing else. Sleeping under it made one throttled or multi-chunk
    /// reply block every other session's answer, and a model waiting on `fetch_chat_asset`, for as
    /// long as [`MAX_RATE_LIMIT_WAIT`].
    rest_lock: Mutex<()>,
    /// When Discord's last 429 said Create Message may be tried again.
    ///
    /// Published rather than only slept on, so a second reply waits out the same deadline instead of
    /// spending its own single retry rediscovering it.
    rest_cooldown_until: std::sync::Mutex<Option<Instant>>,
    /// When Discord's last 429 on a cosmetic call said this transport may show something again.
    ///
    /// Shared by typing, the reaction, the progress message, and the streamed answer, because they
    /// are one bot against one set of buckets: a 429 earned by an edit is a reason for the next
    /// pulse to stay quiet too. None of them sleeps under the final-reply lock or delays an answer;
    /// the cancel acknowledgment is the one call that ignores this, because a press has three
    /// seconds and is not cosmetic.
    liveness_cooldown_until: std::sync::Mutex<Option<Instant>>,
}

#[async_trait]
impl ChatDriver for DiscordDriver {
    async fn reply(
        &self,
        target: &ReplyTarget,
        reply: OutboundReply,
    ) -> Result<(), TransportError> {
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
        let OutboundReply { text, mut images } = reply;
        let mut accepted = false;
        // The REST lock is taken per request rather than per reply. One answer's chunks still
        // arrive in order because this loop awaits each one, and no second answer can be posting
        // into the same conversation at the same time — admission control serializes a
        // conversation against itself. What the reply-wide lock did add was making every other
        // session, and every attachment refresh, wait out this reply's rate limit.
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
            // Every attachment rides the first post, which is where a person reads the answer
            // and where Discord shows them together as one message.
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

    // `status` keeps the default `None`: Discord has no durable working/idle state of its own.
    // Typing is the lease, and the progress message is what says more than that.

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

#[async_trait]
impl TypingLease for DiscordDriver {
    fn renew_every(&self) -> Duration {
        TYPING_REFRESH_INTERVAL
    }

    async fn renew(&self, target: &LivenessTarget) -> Result<(), TransportError> {
        let (channel_id, _) = discord_coordinates(target)?;
        if self.cooling_down() {
            // A pulse suppressed by Discord's own retry deadline is the cooldown working, not a
            // failure for the policy to count against the lease.
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
        let (channel_id, message_id) = discord_coordinates(target)?;
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
        let (channel_id, _) = discord_coordinates(&message.target)?;
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
            // Cumulative edits: the answer grows in the message the first delta posted, which is
            // also the message `finalize` turns into the answer.
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
        // Type 7 is UPDATE_MESSAGE: one call that answers the interaction and rewrites the message
        // it was on, so the button is gone with the acknowledgment. A deferred update would need a
        // second call to remove it, and between the two a second press is possible.
        let body = json!({
            "type": 7,
            "data": {
                "content": STOPPING_ACK_TEXT,
                "components": [],
                "allowed_mentions": allowed_mentions_none(),
            }
        });
        // Deliberately not a cosmetic call: it takes no cooldown, because a press has three
        // seconds and a 429 earned by typing must not swallow the one thing the presser is
        // waiting for. The interaction token authenticates this request, so the bot token stays
        // off it.
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
    /// Whether Discord's last cosmetic 429 is still asking this transport to stay quiet.
    fn cooling_down(&self) -> bool {
        self.liveness_cooldown_until
            .lock()
            .expect("Discord liveness cooldown")
            .is_some_and(|until| until > Instant::now())
    }

    /// Sends one cosmetic request: this transport's own short deadline, no REST lock, and a 429
    /// published as a cooldown every other cosmetic call waits out.
    ///
    /// Deliberately not `send_rest`: that lock exists to serialize Create Message against itself,
    /// and a progress edit taking it would make every other session's answer wait behind a
    /// cosmetic call — the exact thing the lock was narrowed to avoid.
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

    /// Sends one cosmetic request whose success carries no body worth reading.
    async fn liveness_empty(&self, request: reqwest::RequestBuilder) -> Result<(), TransportError> {
        let response = self.liveness_send(request).await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(TransportError::Service {
            code: format!("http-{}", response.status().as_u16()),
        })
    }

    /// Posts one message this driver will edit later, answering with the reference to edit it by.
    async fn post_liveness_message(
        &self,
        target: &LivenessTarget,
        content: &str,
        cancel: bool,
    ) -> Result<MessageRef, TransportError> {
        let (channel_id, inbound_message_id) = discord_coordinates(target)?;
        let mut body = liveness_body(content, cancel, channel_id);
        // Tied to the question it answers, exactly as the reply is: in a busy channel a floating
        // status line says nothing about which message it belongs to.
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

    /// Rewrites one message this driver posted, mentions suppressed on every edit.
    async fn edit_liveness_message(
        &self,
        message: &MessageRef,
        content: &str,
        cancel: bool,
    ) -> Result<(), TransportError> {
        let (channel_id, _) = discord_coordinates(&message.target)?;
        if !is_snowflake(&message.id) {
            return Err(TransportError::Response);
        }
        let body = liveness_body(content, cancel, channel_id);
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

    /// Turns a progress or streamed message into the answer in place.
    ///
    /// `Err` is not a delivery failure: it tells the policy to delete this message and use
    /// [`ChatDriver::reply`] instead. The two things this cannot do in place say so by code — an
    /// answer past Discord's ceiling, which would have to become several messages, and one
    /// carrying attachments, which only a fresh Create Message can upload.
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
        // Bound to a local first: a `let ... else` drops the temporaries in its own initializer,
        // so the chunk this borrows has to outlive the statement that matched it.
        let chunks = split_message(&reply.text, MAX_MESSAGE_CHARS, TextUnit::Utf16);
        let [content] = chunks.as_slice() else {
            return Err(TransportError::Service {
                code: "answer-too-long".to_owned(),
            });
        };
        // The stop control goes with the answer: the run it would have cancelled is over.
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
                // The wait itself happens in `post_message`, outside the REST lock.
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
        images: Vec<GeneratedImage>,
    ) -> Result<(), TransportError> {
        let url = format!(
            "{}/api/v{API_VERSION}/channels/{channel_id}/messages",
            self.endpoint
        );
        let media_type = images
            .first()
            .ok_or(TransportError::Response)?
            .media_type()
            .to_owned();
        let attachments = images
            .iter()
            .enumerate()
            .map(|(index, image)| image.filename(index))
            .collect::<Vec<_>>();
        let bytes = images
            .into_iter()
            .map(GeneratedImage::into_bytes)
            .collect::<Vec<_>>();
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
            let mut form = reqwest::multipart::Form::new().text("payload_json", payload.clone());
            for (index, (filename, bytes)) in attachments.iter().zip(&bytes).enumerate() {
                #[allow(
                    clippy::map_err_ignore,
                    reason = "mime_str only rejects strings that are not a media type, and \
                              GeneratedImage::media_type returns a fixed IANA type"
                )]
                let part = reqwest::multipart::Part::bytes(bytes.clone())
                    .file_name(filename.clone())
                    .mime_str(&media_type)
                    .map_err(|_| TransportError::Response)?;
                form = form.part(format!("files[{index}]"), part);
            }
            // Attachments go out through the same lock discipline as a text chunk: one more Create
            // Message against the same route, and a reply that posts files is exactly the reply most
            // worth not holding a shared lock through.
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
                // The wait itself happens in `send_rest`, outside the REST lock.
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

    /// Sends one JSON Create Message, serialized against this transport's other REST calls.
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

    /// Sends one prepared REST request, serialized against this transport's other REST calls.
    ///
    /// The lock covers the request and nothing else. Any rate-limit wait is served *before* it is
    /// taken, so a throttled reply holds up neither another session's answer nor a mid-prompt
    /// attachment refresh.
    async fn send_rest(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, TransportError> {
        // Bounded rather than a wait loop: at most one deadline is outstanding when the call
        // arrives, and at most one more can be published while it queues on the lock. Past that it
        // sends and lets Discord answer, because a reply parked forever is not an improvement on a
        // reply that is refused.
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

    /// Publishes Discord's own retry deadline so every later request waits it out before the lock.
    fn publish_rest_cooldown(&self, wait: Duration) {
        *self
            .rest_cooldown_until
            .lock()
            .expect("Discord REST cooldown") = Some(Instant::now() + wait);
    }

    /// How long Discord's last 429 still asks Create Message to wait, if at all.
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
        // Serialized with the reply path, but only for the request: reading the body back is this
        // session's own business and no other caller's rate limit.
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

/// The channel and the message one liveness target names, refusing another service's coordinates.
///
/// The message identifier is the inbound message on an inbound target and the gateway's own
/// message on a press, which is why both are validated here and neither is assumed.
fn discord_coordinates(target: &LivenessTarget) -> Result<(&str, &str), TransportError> {
    let LivenessTarget::Discord {
        channel_id,
        message_id,
    } = target
    else {
        return Err(TransportError::Response);
    };
    if !is_snowflake(channel_id) || !is_snowflake(message_id) {
        return Err(TransportError::Response);
    }
    Ok((channel_id, message_id))
}

/// The body of every message this gateway posts or edits for liveness.
fn liveness_body(content: &str, cancel: bool, conversation_id: &str) -> Value {
    json!({
        "content": content,
        // On every edit, not only the first post: Discord re-notifies a channel when an edit
        // introduces a mention, and neither a progress line nor streamed model text is a reason
        // to ping anyone.
        "allowed_mentions": allowed_mentions_none(),
        "components": cancel_components(cancel, conversation_id),
    })
}

/// Mentions nothing, whatever the text turns out to contain.
fn allowed_mentions_none() -> Value {
    json!({
        "parse": [],
        "users": [],
        "roles": [],
        "replied_user": false,
    })
}

/// The stop control, or the empty list that removes one a previous edit left behind.
///
/// Style 4 is Danger, which is what a destructive control looks like on Discord. The `custom_id`
/// carries the conversation the run belongs to, because an interaction arrives with the message
/// it was on and nothing else this gateway wrote: the reader reads the conversation back out of
/// it, and the routing loop decides whether the presser is the person who may stop that run.
///
/// Nothing truncates the identifier and nothing needs to: a Discord conversation is the channel,
/// [`discord_coordinates`] has already refused anything that is not a snowflake, and the prefix
/// plus the widest `u64` is a quarter of Discord's hundred-byte ceiling.
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

/// The cumulative text as the message shows it: a cut the policy made says so with a marker.
///
/// Rendering the cut is the driver's because only the driver knows what its surface has room for.
/// The marker goes on before [`one_message`] counts, so Discord's ceiling still decides what is
/// sent and the extra character cannot push a full message over it.
fn shown_text(text: &StreamedText) -> String {
    let mut shown = text.text.as_str().to_owned();
    if text.truncated {
        shown.push(TRUNCATION_MARKER);
    }
    shown
}

/// What Discord will take of one progress or streamed text, counted the way Discord counts.
///
/// The policy bounds this text in characters and Discord's ceiling is UTF-16 code units, so astral
/// text can still arrive over it. The first chunk is the part that fits; the rest is dropped
/// because a progress surface is a summary and the answer carries the whole text. `None` is
/// unreachable for any input `split_message` accepts, and is refused rather than guessed at.
fn one_message(text: &str) -> Option<String> {
    split_message(text, MAX_MESSAGE_CHARS, TextUnit::Utf16)
        .into_iter()
        .next()
}

/// Serializes one request body.
#[allow(
    clippy::map_err_ignore,
    reason = "serializing a serde_json::Value cannot fail: it holds no non-string map keys and \
              serde_json::Number rejects non-finite floats"
)]
fn encode(body: &Value) -> Result<Vec<u8>, TransportError> {
    serde_json::to_vec(body).map_err(|_| TransportError::Response)
}

/// Percent-encodes one URL path segment, which is how Discord takes a Unicode reaction.
///
/// Deliberately not `form_urlencoded`: that spelling encodes a space as `+`, which in a path
/// segment is a literal plus rather than a space.
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

/// Whether a service-issued interaction token can be interpolated into a request path unchanged.
///
/// The token authenticates the acknowledgment rather than being this daemon's own credential, but
/// it is still service-supplied text that goes into a URL: a value carrying a slash, a query, a
/// fragment, or whitespace would address an endpoint other than the callback it names.
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
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::{
        CANCEL_CUSTOM_ID_PREFIX, DiscordDriver, DiscordTransport, MAX_MESSAGE_CHARS, SessionStarts,
        TextUnit, allowed_asset_url, cancel_components, client, gateway_url, is_fatal,
        is_interaction_token, percent_encoded, split_message,
    };
    use crate::{
        config::{LivenessMode, LivenessSettings},
        progress::ProgressText,
        transport::{
            AckToken, CancelPress, ChatDriver as _, LivenessTarget, MessageRef, OutboundReply,
            StreamedText, TransportError, TransportIdentity, receive_span,
        },
    };

    /// Discord's ceiling on a component `custom_id`, which the button's identifier fits by
    /// construction rather than by a check.
    const MAX_CUSTOM_ID_BYTES: usize = 100;

    /// What one request reached the loopback service as.
    #[derive(Debug)]
    struct Recorded {
        method: String,
        path: String,
        head: String,
        body: Value,
    }

    /// A loopback stand-in for Discord: answers each request with the next canned response, in
    /// order, and hands back everything it was sent.
    ///
    /// Mocked on loopback rather than against the service, which is also the only endpoint
    /// [`crate::config`] will accept outside production.
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

    /// A transport that publishes liveness, which is what every test here is about.
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

    /// An endpoint nothing listens on: the tests that use it assert no request is ever made.
    const UNREACHABLE: &str = "http://127.0.0.1:1";

    fn driver(endpoint: &str) -> DiscordDriver {
        DiscordDriver {
            endpoint: endpoint.to_owned(),
            token: Redacted::new("bot-secret".to_owned()),
            http: client().expect("the shared client builds"),
            production: false,
            rest_lock: Mutex::new(()),
            rest_cooldown_until: std::sync::Mutex::new(None),
            liveness_cooldown_until: std::sync::Mutex::new(None),
        }
    }

    fn target() -> LivenessTarget {
        LivenessTarget::Discord {
            channel_id: "100".to_owned(),
            message_id: "200".to_owned(),
        }
    }

    fn progress_message() -> MessageRef {
        MessageRef {
            target: target(),
            id: "555".to_owned(),
        }
    }

    /// Operator-authored progress text, which only the policy module otherwise renders.
    fn progress_text(text: &str) -> ProgressText {
        ProgressText::for_test(text)
    }

    /// The cumulative text of a recorded stream, taken through the model crate's own parser
    /// because that parser is the only thing that builds model text from bytes.
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
        assert!(transport.prepare_identify().await.is_err());
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

    /// The reaction goes on the inbound message as a percent-encoded path segment, and the typing
    /// pulse is a plain POST under the transport's own short deadline. Both carry the bot token
    /// and neither takes the reply path's REST lock.
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

    /// The progress message is one post that answers the question it belongs to, then edits of
    /// that same message, then a deletion. Every one of them suppresses mentions, because Discord
    /// re-notifies a channel when an edit introduces one, and carries the danger-style stop button
    /// whose `custom_id` names the conversation the run belongs to.
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

    /// A streamed answer is the same message growing: the first delta posts it and every later one
    /// edits it, so the person reads one message rather than a wall of fragments.
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

    /// A cut the policy made is rendered rather than hidden: the marker is the only thing on the
    /// message that says the answer on screen is not all of it.
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

    /// A finished answer replaces the message in place and takes the stop button with it. The two
    /// answers that cannot land in place name which way they did not fit, because that is what
    /// tells the policy to delete and post instead of reporting a failed delivery.
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

    /// The acknowledgment is one type 7 UPDATE_MESSAGE carrying the stopping line and no
    /// components, so the button is gone with the answer to the interaction and a second press
    /// cannot arrive. The interaction token authenticates it, so the bot token stays off it.
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

    /// An acknowledgment token from another service, or one whose value would leave the callback
    /// path, is refused before any request is built.
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

    /// A press becomes a cancel request only when it is this gateway's own button, pressed on the
    /// conversation its `custom_id` names. The presser's identity rides the request in canonical
    /// form; whether that subject is the one whose run this is belongs to the routing loop.
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
            },
            "the press names the message the button was on"
        );

        // A direct message carries the presser at the top level instead of under a member.
        let direct = press(json!({ "member": null, "user": { "id": "42" } }));
        assert!(
            transport
                .cancel_press(&direct, &span)
                .expect("a well-formed envelope")
                .is_some()
        );

        for ignored in [
            // An application command, not a component.
            press(json!({ "type": 2 })),
            // Some other component this gateway never wrote.
            press(json!({ "data": { "component_type": 2, "custom_id": "vote:100" } })),
            // A copy of the component pressed somewhere other than the conversation it stops.
            press(json!({ "channel_id": "101" })),
            // An envelope with nothing to acknowledge with.
            press(json!({ "token": null })),
            // The bot's own identity, which is not a person pressing anything.
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

    /// The `custom_id` the button is built with stays inside Discord's ceiling for the widest
    /// identifier the service can mint, which is why nothing along the way truncates it.
    #[test]
    fn a_stop_buttons_custom_id_fits_discords_ceiling() {
        let components = cancel_components(true, &u64::MAX.to_string());
        let widest = components[0]["components"][0]["custom_id"]
            .as_str()
            .expect("the button carries a custom_id");
        assert!(widest.starts_with(CANCEL_CUSTOM_ID_PREFIX), "{widest}");
        assert!(widest.len() <= MAX_CUSTOM_ID_BYTES, "{widest}");
        assert_eq!(percent_encoded("🍊"), "%F0%9F%8D%8A");
        assert_eq!(percent_encoded("a-b_c.d~e"), "a-b_c.d~e");
    }

    /// Every message this transport drops says why on its own receive span, because a silent drop
    /// is a triage with nothing to read: the answer to "why did the bot not reply" has to be in
    /// the trace the receipt already opened.
    #[test]
    fn every_dropped_message_records_why_on_its_receive_span() {
        let mut transport = transport("elote");
        transport.identity = TransportIdentity {
            user_id: Some("999".to_owned()),
            handle: None,
        };
        let capture = CaptureLayer::workspace();

        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(capture.clone()),
            || {
                let mut routable = |message: &Value| {
                    let span = receive_span(ChatTransportKind::Discord);
                    span.in_scope(|| transport.routable(message, &span))
                        .expect("a drop is not a transport failure")
                };
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
                    assert!(routable(&dropped).is_none(), "{dropped} routed");
                    assert_reason(&capture, reason);
                }
                assert!(
                    routable(&message(json!({}))).is_some(),
                    "an ordinary message routes"
                );
                capture.clear();
                assert!(
                    routable(&message(json!({}))).is_none(),
                    "the same identifier twice is the redelivery a resume replays"
                );
                assert_reason(&capture, "duplicate");
            },
        );
    }

    /// One Discord message event, with the fields a test varies overridden.
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

    /// A routed message carries the coordinates every liveness surface needs: the channel to post
    /// in and the message to react to. `liveness.mode: off` withholds them, which is how this
    /// transport stays exactly as reply-only as it was.
    #[test]
    fn a_routed_message_carries_its_liveness_coordinates() {
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
        let routed = span
            .in_scope(|| publishing.routable(&message(json!({})), &span))
            .expect("a routable message")
            .expect("the message routes");
        assert_eq!(
            routed.liveness,
            Some(LivenessTarget::Discord {
                channel_id: "100".to_owned(),
                message_id: "200".to_owned(),
            })
        );

        let span = receive_span(ChatTransportKind::Discord);
        let routed = span
            .in_scope(|| quiet.routable(&message(json!({})), &span))
            .expect("a routable message")
            .expect("the message still routes");
        assert!(
            routed.liveness.is_none(),
            "liveness off leaves nothing for the policy to render on"
        );
    }
}
