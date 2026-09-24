//! The envelope is acknowledged before processing begins because Slack resends after about three
//! seconds while a session runs much longer; a bounded ring absorbs the redeliveries.

use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
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
    time::{Instant, timeout},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
use tracing::{Instrument as _, Span};

use crate::{
    asset::{AssetSourceRef, PendingAsset},
    config::{LivenessSettings, SLACK_ENDPOINT, SlackExperience, SlackLivenessFallback},
    progress::ProgressText,
    transport::{
        AckToken, AssetFetcher, CancelButton, CancelPress, CancelRequest, ChatDriver, ChatHistory,
        ChatTransport, InboundMessage, InboundReaction, LivenessTarget, MessageRef, NativeStatus,
        OutboundReply, PastMessage, ProgressLimits, ProgressMessage, ReplyTarget, SeenIds, Status,
        StreamLimits, StreamedText, TextStream, ThreadClaim, ThreadContinuation, ThreadOwnership,
        TransportError, TransportEvent, TransportIdentity, asset_buffer, bound_inbound,
        credential_client, floor_boundary, receive_span, record_conversation, reserve_for_chunk,
    },
};

const DEDUP_CAPACITY: usize = 1024;
const OWNED_THREAD_CAPACITY: usize = 1024;
/// file_share, thread_broadcast, and me_message count as new requests because each carries ordinary
/// user-typed text or an attachment, unlike edits, deletions, or joins.
const REQUEST_SUBTYPES: [&str; 3] = ["file_share", "me_message", "thread_broadcast"];
const MAX_ATTACHMENTS: usize = 10;
const MAX_ATTACHMENT_NAME_BYTES: usize = 128;
const SLACK_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// A liveness call must never inherit the final reply/file client's general 30-second wait.
const LIVENESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Slack pings roughly every 30 seconds, so 90 seconds of silence means a connection died without
/// TCP reporting it; the same deadline also bounds opening a socket.
const LIVENESS_DEADLINE: Duration = Duration::from_secs(90);
const LIVENESS_REACTION: &str = "tangerine";
const PROGRESS_MAX_CHARS: usize = 4_000;
const PROGRESS_MIN_EDIT_INTERVAL: Duration = Duration::from_secs(3);
const STREAM_MIN_INTERVAL: Duration = Duration::from_secs(1);
const STREAM_MAX_CHARS: usize = 12_000;
const MAX_PROGRESS_COOLDOWN: Duration = Duration::from_secs(60);
const DEFAULT_PROGRESS_COOLDOWN: Duration = Duration::from_secs(5);
const CANCEL_ACTION_ID: &str = "dekopon-cancel";
const MAX_TRACKED_REACTIONS: usize = 256;
const MAX_TRACKED_CHANNEL_KINDS: usize = 512;
const MAX_TRACKED_STREAMS: usize = 64;
const STREAM_TRUNCATION_MARKER: &str = "…";
const HISTORY_PAGE_LIMIT: usize = 200;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(crate) struct SlackTransport {
    name: String,
    endpoint: String,
    app_token: Redacted<String>,
    http: reqwest::Client,
    replier: Arc<SlackReplier>,
    socket: Option<Socket>,
    identity: TransportIdentity,
    team_id: Option<String>,
    seen: SeenIds,
    channel_kinds: Tracked<SlackChannelKind>,
    pending: VecDeque<TransportEvent>,
    experience: SlackExperience,
    deadline: Duration,
    thread_ownership: Arc<SlackThreadOwnership>,
}

impl SlackTransport {
    pub(crate) fn new(
        name: String,
        endpoint: String,
        app_token: String,
        bot_token: String,
        experience: SlackExperience,
        liveness: LivenessSettings,
    ) -> Result<Self, TransportError> {
        let http = credential_client(SLACK_REQUEST_TIMEOUT)
            .build()
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        let thread_ownership = Arc::new(SlackThreadOwnership::new(OWNED_THREAD_CAPACITY));
        Ok(Self {
            name,
            endpoint: endpoint.clone(),
            app_token: Redacted::new(app_token),
            http: http.clone(),
            replier: Arc::new(SlackReplier {
                endpoint,
                bot_token: Redacted::new(bot_token),
                http,
                experience,
                classic_reaction: liveness.classic_fallback == SlackLivenessFallback::Reaction,
                agent_status_available: AtomicBool::new(true),
                reaction_available: AtomicBool::new(true),
                added_reactions: Mutex::new(Tracked::new(MAX_TRACKED_REACTIONS)),
                progress_cooldown_until: Mutex::new(None),
                streams: Mutex::new(Tracked::new(MAX_TRACKED_STREAMS)),
                bot_user: OnceLock::new(),
            }),
            socket: None,
            identity: TransportIdentity::default(),
            team_id: None,
            seen: SeenIds::new(DEDUP_CAPACITY),
            channel_kinds: Tracked::new(MAX_TRACKED_CHANNEL_KINDS),
            pending: VecDeque::new(),
            experience,
            deadline: LIVENESS_DEADLINE,
            thread_ownership,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

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

    async fn open(&mut self) -> Result<(), TransportError> {
        let body = post_form(
            &self.http,
            &format!("{}/api/apps.connections.open", self.endpoint),
            self.app_token.expose(),
        )
        .await?;
        let url = body["url"].as_str().ok_or(TransportError::Response)?;
        // The handshake and greeting share one deadline because a URL that accepts a connection but
        // never completes TLS or never greets would otherwise hang this transport indefinitely.
        let socket = match timeout(self.deadline, open_socket(url)).await {
            Ok(socket) => socket?,
            Err(_) => {
                tracing::warn!(
                    event = "gateway_transport_silent",
                    transport = %self.name,
                    phase = "open"
                );
                return Err(TransportError::Closed);
            }
        };
        self.socket = Some(socket);
        Ok(())
    }

    async fn pump(&mut self) -> Result<(), TransportError> {
        let deadline = self.deadline;
        let socket = self.socket.as_mut().ok_or(TransportError::Closed)?;
        let frame = match timeout(deadline, socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => text,
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_)))) => {
                return Ok(());
            }
            Ok(Some(Ok(Message::Frame(_)))) => return Ok(()),
            Ok(Some(Ok(Message::Close(_))) | None) => return Err(TransportError::Closed),
            Ok(Some(Err(source))) => return Err(TransportError::Request(Box::new(source))),
            Err(_) => {
                tracing::warn!(
                    event = "gateway_transport_silent",
                    transport = %self.name,
                    phase = "read"
                );
                return Err(TransportError::Closed);
            }
        };
        let frame =
            serde_json::from_str::<Value>(&frame).map_err(TransportError::MalformedResponse)?;

        let received = match frame["envelope_id"].as_str() {
            Some(envelope) => {
                let received = receive_span(ChatTransportKind::Slack);
                let ack = json!({ "envelope_id": envelope }).to_string();
                socket
                    .send(Message::text(ack))
                    .instrument(received.clone())
                    .await
                    .map_err(|source| TransportError::Request(Box::new(source)))?;
                received
            }
            None => Span::none(),
        };

        if frame["type"].as_str() == Some("interactive") {
            return self
                .cancel_pressed(&frame)
                .instrument(received.clone())
                .await;
        }
        self.accept(&frame, &received)
            .instrument(received.clone())
            .await
    }

    async fn cancel_pressed(&mut self, frame: &Value) -> Result<(), TransportError> {
        let Some((press, request)) = self.cancel_press(frame)? else {
            return Ok(());
        };
        let replier = Arc::clone(&self.replier);
        let Some(button) = ChatDriver::cancel_button(&*replier) else {
            return Ok(());
        };
        if let Err(error) = button.ack(&press).await {
            tracing::warn!(
                event = "gateway_progress_degraded",
                transport = %self.name,
                surface = "cancel-ack",
                category = error.category()
            );
        }
        self.pending
            .push_back(TransportEvent::CancelRequested(request));
        Ok(())
    }

    fn cancel_press(
        &self,
        frame: &Value,
    ) -> Result<Option<(CancelPress, CancelRequest)>, TransportError> {
        let payload = &frame["payload"];
        if payload["type"].as_str() != Some("block_actions") {
            return Ok(None);
        }
        let Some(envelope_id) = frame["envelope_id"].as_str() else {
            return Ok(None);
        };
        let Some(conversation_id) = payload["actions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|action| action["action_id"].as_str() == Some(CANCEL_ACTION_ID))
            .and_then(|action| action["value"].as_str())
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        let (Some(channel), Some(user), Some(message_ts)) = (
            payload["channel"]["id"].as_str(),
            payload["user"]["id"].as_str(),
            payload["message"]["ts"].as_str(),
        ) else {
            return Ok(None);
        };
        let Some(team) = payload["team"]["id"].as_str().or(self.team_id.as_deref()) else {
            return Ok(None);
        };
        let thread_ts = payload["message"]["thread_ts"]
            .as_str()
            .unwrap_or(message_ts);
        let subject = ExternalSubject::slack(team, user)
            .map_err(TransportError::Subject)?
            .to_string();
        Ok(Some((
            CancelPress {
                target: LivenessTarget::Slack {
                    channel_id: channel.to_owned(),
                    thread_ts: thread_ts.to_owned(),
                    message_ts: message_ts.to_owned(),
                    initiator_user_id: user.to_owned(),
                    conversation_id: conversation_id.to_owned(),
                },
                subject: subject.clone(),
                ack: AckToken::Slack {
                    envelope_id: envelope_id.to_owned(),
                },
            },
            CancelRequest {
                transport: self.name.clone(),
                conversation_id: conversation_id.to_owned(),
                subject,
                via: CancelVia::Button,
            },
        )))
    }

    async fn accept(&mut self, frame: &Value, received: &Span) -> Result<(), TransportError> {
        match frame["type"].as_str() {
            Some("disconnect") => {
                self.socket = None;
                return Err(TransportError::Closed);
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
        if self.experience == SlackExperience::Agent
            && event["type"].as_str() == Some("agent_session_stopped")
        {
            if let Some(stopped) = self.session_stopped(&team, event)? {
                self.pending
                    .push_back(TransportEvent::CancelRequested(stopped));
            }
        } else if let Some(message) = self.routable(&team, event, received).await? {
            received.record("message.id", message.message_id.as_str());
            self.pending
                .push_back(TransportEvent::Message(Box::new(message)));
        }
        Ok(())
    }

    async fn routable(
        &mut self,
        team: &str,
        event: &Value,
        received: &Span,
    ) -> Result<Option<InboundMessage>, TransportError> {
        if !matches!(event["type"].as_str(), Some("message" | "app_mention")) {
            return Ok(None);
        }
        if !event["bot_id"].is_null() {
            return Ok(None);
        }
        let Some(user) = event["user"].as_str() else {
            return Ok(None);
        };
        if self.identity.user_id.as_deref() == Some(user) {
            return Ok(None);
        }
        if let Some(subtype) = event["subtype"].as_str()
            && !REQUEST_SUBTYPES.contains(&subtype)
        {
            return Ok(None);
        }
        let (Some(channel), Some(ts)) = (event["channel"].as_str(), event["ts"].as_str()) else {
            return Ok(None);
        };
        let text = bound_inbound(event["text"].as_str().unwrap_or_default());
        let assets = pending_assets(&event["files"]);
        if text.trim().is_empty() && assets.is_empty() {
            return Ok(None);
        }
        // Slack sets thread_ts equal to the message's own ts on a post threaded later; treating
        // that as a reply would self-parent it.
        let thread_ts = event["thread_ts"]
            .as_str()
            .filter(|thread| *thread != ts)
            .map(str::to_owned);
        let root_ts = thread_ts.clone().unwrap_or_else(|| ts.to_owned());
        // Keep the mention-text fallback: the plain message event can win Slack's dedup race before
        // the authoritative app_mention event arrives.
        let explicitly_addressed =
            event["type"].as_str() == Some("app_mention") || self.identity.is_addressed(&text);
        if !self.seen.insert(format!("{channel}:{ts}")) {
            return Ok(None);
        }
        let Some(posted_in) = self.channel_kind(channel, event, received).await else {
            return Ok(None);
        };
        let kind = posted_in.conversation_kind(thread_ts.is_some());
        let is_shared = kind != ConversationKind::DirectMessage;
        let thread_continuation = match (is_shared, self.experience) {
            (true, SlackExperience::Agent) => {
                let claim = ThreadClaim::Slack {
                    team_id: team.to_owned(),
                    channel_id: channel.to_owned(),
                    thread_ts: root_ts.clone(),
                    user_id: user.to_owned(),
                };
                let inherited = !explicitly_addressed && self.thread_ownership.owns(&claim);
                if !explicitly_addressed && !inherited {
                    return Ok(None);
                }
                Some(ThreadContinuation { claim, inherited })
            }
            (true, SlackExperience::Classic) if !explicitly_addressed => {
                return Ok(None);
            }
            _ => None,
        };
        let reply_thread = match (kind, self.experience) {
            (ConversationKind::DirectMessage, SlackExperience::Classic) => None,
            _ => Some(root_ts.clone()),
        };
        // The joined thread must come from reply_thread, not thread_ts, since Slack omits thread_ts
        // on a thread's own opening message, which would misfile that first turn if read directly.
        let conversation = Conversation {
            kind,
            container: Some(team.to_ascii_lowercase()),
            id: channel.to_ascii_lowercase(),
            thread: reply_thread.clone(),
        };
        record_conversation(received, &conversation);
        // The conversation key is minted once and carried rather than re-derived, since a Stop
        // button spelling it a second way would name no session at all.
        let conversation_id = conversation.key();

        Ok(Some(InboundMessage {
            transport: self.name.clone(),
            transport_kind: ChatTransportKind::Slack,
            subject: ExternalSubject::slack(team, user).map_err(TransportError::Subject)?,
            conversation,
            message_id: ts.to_owned(),
            text,
            assets,
            addressed: is_shared.then_some(explicitly_addressed),
            thread_continuation,
            reply: ReplyTarget::Slack {
                channel: channel.to_owned(),
                thread_ts: reply_thread,
            },
            liveness: Some(LivenessTarget::Slack {
                channel_id: channel.to_owned(),
                thread_ts: root_ts,
                message_ts: ts.to_owned(),
                initiator_user_id: user.to_owned(),
                conversation_id,
            }),
            receive_span: received.clone(),
            received_at: tokio::time::Instant::now(),
            native_group: None,
            constituents: Vec::new(),
            late_photos: None,
            asset_overflow: event["files"]
                .as_array()
                .is_some_and(|files| files.len() > MAX_ATTACHMENTS),
        }))
    }

    /// app_mention carries no channel_type, so assuming channel there would mint a direct or group
    /// message as a channel that no route or grant would then match.
    async fn channel_kind(
        &mut self,
        channel: &str,
        event: &Value,
        received: &Span,
    ) -> Option<SlackChannelKind> {
        if let Some(channel_type) = event["channel_type"].as_str() {
            return Some(SlackChannelKind::of_channel_type(channel_type));
        }
        if let Some(kind) = self.channel_kinds.get(channel).copied() {
            return Some(kind);
        }
        let unresolved = |cause: &'static str| -> Option<SlackChannelKind> {
            received.record("drop.reason", "conversation-unresolved");
            tracing::debug!(
                event = "gateway_conversation_unresolved",
                transport = "slack",
                cause
            );
            None
        };
        if channel.is_empty() || !channel.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return unresolved("channel-id");
        }
        if self.replier.progress_cooldown().is_some() {
            return unresolved("rate-limited");
        }
        let response = self
            .http
            .get(format!(
                "{}/api/conversations.info?channel={channel}",
                self.endpoint
            ))
            .header(
                "authorization",
                format!("Bearer {}", self.replier.bot_token.expose()),
            )
            .timeout(LIVENESS_REQUEST_TIMEOUT)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(source) => {
                tracing::debug!(
                    event = "gateway_conversation_unresolved",
                    transport = "slack",
                    cause = "request",
                    cause_type = %source
                );
                received.record("drop.reason", "conversation-unresolved");
                return None;
            }
        };
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            self.replier.begin_cooldown(retry_after(response.headers()));
            return unresolved("rate-limited");
        }
        let body = match check_ok(response).await {
            Ok(body) => body,
            Err(error) => {
                tracing::debug!(
                    event = "gateway_conversation_unresolved",
                    transport = "slack",
                    cause = "response",
                    cause_type = %error
                );
                received.record("drop.reason", "conversation-unresolved");
                return None;
            }
        };
        let Some(kind) = SlackChannelKind::of_info(&body["channel"]) else {
            return unresolved("channel-flags");
        };
        self.channel_kinds.insert(channel.to_owned(), kind);
        Some(kind)
    }

    fn session_stopped(
        &self,
        team: &str,
        event: &Value,
    ) -> Result<Option<CancelRequest>, TransportError> {
        let (Some(channel), Some(thread_ts), Some(user)) = (
            event["channel"]
                .as_str()
                .or_else(|| event["channel_id"].as_str()),
            event["thread_ts"].as_str(),
            event["user"].as_str().or_else(|| event["user_id"].as_str()),
        ) else {
            return Ok(None);
        };
        let conversation = Conversation {
            kind: ConversationKind::Thread,
            container: Some(team.to_ascii_lowercase()),
            id: channel.to_ascii_lowercase(),
            thread: Some(thread_ts.to_owned()),
        };
        Ok(Some(CancelRequest {
            transport: self.name.clone(),
            conversation_id: conversation.key(),
            subject: ExternalSubject::slack(team, user)
                .map_err(TransportError::Subject)?
                .to_string(),
            via: CancelVia::NativeStop,
        }))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlackChannelKind {
    Im,
    Mpim,
    Channel,
}

impl SlackChannelKind {
    fn of_channel_type(value: &str) -> Self {
        match value {
            "im" => Self::Im,
            "mpim" => Self::Mpim,
            _ => Self::Channel,
        }
    }

    fn of_info(channel: &Value) -> Option<Self> {
        if channel["is_im"].as_bool() == Some(true) {
            return Some(Self::Im);
        }
        if channel["is_mpim"].as_bool() == Some(true) {
            return Some(Self::Mpim);
        }
        (channel["is_channel"].as_bool() == Some(true)
            || channel["is_group"].as_bool() == Some(true))
        .then_some(Self::Channel)
    }

    const fn conversation_kind(self, threaded: bool) -> ConversationKind {
        match (self, threaded) {
            (Self::Im, _) => ConversationKind::DirectMessage,
            (Self::Mpim, false) => ConversationKind::GroupDirectMessage,
            (Self::Channel, false) => ConversationKind::Channel,
            (Self::Mpim | Self::Channel, true) => ConversationKind::Thread,
        }
    }
}

async fn open_socket(url: &str) -> Result<Socket, TransportError> {
    let (mut socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|source| TransportError::Request(Box::new(source)))?;
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                let frame = serde_json::from_str::<Value>(&text)
                    .map_err(TransportError::MalformedResponse)?;
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
    Ok(socket)
}

impl ChatTransport for SlackTransport {
    fn name(&self) -> &str {
        &self.name
    }

    fn connect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>> {
        Box::pin(async move {
            let (user_id, team_id) = self.auth_test().await?;
            self.replier.bot_user.get_or_init(|| user_id.clone());
            self.identity = TransportIdentity {
                user_id: Some(user_id),
                handle: None,
            };
            self.team_id = Some(team_id);
            self.open().await?;
            Ok(self.identity.clone())
        })
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
                if let Err(error) = self.pump().await {
                    self.socket = None;
                    return Err(error);
                }
            }
        })
    }

    fn driver(&self) -> Arc<dyn ChatDriver> {
        Arc::clone(&self.replier) as Arc<dyn ChatDriver>
    }

    fn asset_fetcher(&self) -> Option<Arc<dyn AssetFetcher>> {
        Some(Arc::clone(&self.replier) as Arc<dyn AssetFetcher>)
    }

    fn thread_ownership(&self) -> Option<Arc<dyn ThreadOwnership>> {
        (self.experience == SlackExperience::Agent)
            .then(|| Arc::clone(&self.thread_ownership) as Arc<dyn ThreadOwnership>)
    }
}

pub(crate) struct SlackReplier {
    endpoint: String,
    bot_token: Redacted<String>,
    http: reqwest::Client,
    experience: SlackExperience,
    classic_reaction: bool,
    agent_status_available: AtomicBool,
    reaction_available: AtomicBool,
    added_reactions: Mutex<Tracked<()>>,
    progress_cooldown_until: Mutex<Option<Instant>>,
    streams: Mutex<Tracked<StreamState>>,
    bot_user: OnceLock<String>,
}

#[async_trait]
impl ChatDriver for SlackReplier {
    async fn reply(
        &self,
        target: &ReplyTarget,
        reply: OutboundReply,
    ) -> Result<(), TransportError> {
        let OutboundReply { text, images } = reply;
        super::hydration::validate_types(&images, super::hydration::AcceptedTypes::Files)?;
        let images = super::hydration::ImageQueue::new(images);
        let ReplyTarget::Slack { channel, thread_ts } = target else {
            return Err(TransportError::Response);
        };
        if !images.is_empty() {
            return self
                .upload_attachments(channel.clone(), thread_ts.clone(), text, images)
                .await;
        }
        let mut body = json!({
            "channel": channel,
            "text": text,
            "blocks": [{ "type": "markdown", "text": text }],
        });
        if let Some(thread_ts) = thread_ts {
            body["thread_ts"] = Value::String(thread_ts.clone());
        }
        #[allow(
            clippy::map_err_ignore,
            reason = "serializing a serde_json::Value cannot fail: it holds no non-string map \
                      keys and serde_json::Number rejects non-finite floats"
        )]
        let encoded = serde_json::to_vec(&body).map_err(|_| TransportError::Response)?;
        let post = || {
            self.http
                .post(format!("{}/api/chat.postMessage", self.endpoint))
                .header(
                    "authorization",
                    format!("Bearer {}", self.bot_token.expose()),
                )
                .header("content-type", "application/json; charset=utf-8")
                .body(encoded.clone())
                .send()
        };
        let mut response = post()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let backoff = retry_after(response.headers()).as_secs();
            tracing::warn!(
                event = "gateway_reply_rate_limited",
                transport = "slack",
                retry_after_seconds = backoff
            );
            drop(response);
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            response = post()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?;
        }
        let body = check_ok(response).await?;
        let response_channel = body["channel"].as_str().ok_or(TransportError::Response)?;
        let timestamp = body["ts"].as_str().ok_or(TransportError::Response)?;
        if response_channel != channel.as_str() || !canonical_timestamp(timestamp) {
            return Err(TransportError::Response);
        }
        Ok(())
    }

    fn status(&self) -> Option<&dyn NativeStatus> {
        (self.experience == SlackExperience::Agent
            && self.agent_status_available.load(Ordering::Acquire))
        .then_some(self as &dyn NativeStatus)
    }

    fn progress(&self) -> Option<&dyn ProgressMessage> {
        Some(self)
    }

    fn stream(&self) -> Option<&dyn TextStream> {
        Some(self)
    }

    fn reaction(&self) -> Option<&dyn InboundReaction> {
        let without_native_status = self.experience == SlackExperience::Classic
            || !self.agent_status_available.load(Ordering::Acquire);
        (self.classic_reaction
            && without_native_status
            && self.reaction_available.load(Ordering::Acquire))
        .then_some(self as &dyn InboundReaction)
    }

    fn cancel_button(&self) -> Option<&dyn CancelButton> {
        (self.experience == SlackExperience::Classic).then_some(self as &dyn CancelButton)
    }

    fn history(&self) -> Option<&dyn ChatHistory> {
        Some(self)
    }
}

#[async_trait]
impl ChatHistory for SlackReplier {
    async fn recent(
        &self,
        conversation: &Conversation,
        before: &str,
        limit: usize,
    ) -> Result<Vec<PastMessage>, TransportError> {
        let bot = self.bot_user.get().ok_or(TransportError::Closed)?;
        let channel = conversation.id.to_ascii_uppercase();
        if channel.is_empty() || !channel.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(TransportError::Response);
        }
        let cutoff = slack_instant(before).ok_or(TransportError::Response)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let Some(thread) = conversation.thread.as_deref() else {
            let page_limit = limit.min(HISTORY_PAGE_LIMIT).to_string();
            let page = self
                .history_page(
                    "conversations.history",
                    &[
                        ("channel", channel.as_str()),
                        ("latest", before),
                        ("inclusive", "false"),
                        ("limit", page_limit.as_str()),
                    ],
                )
                .await?;
            let mut recalled = past_messages(&page, bot, cutoff)?;
            recalled.reverse();
            return Ok(recalled);
        };
        if !canonical_timestamp(thread) {
            return Err(TransportError::Response);
        }
        let page_limit = HISTORY_PAGE_LIMIT.to_string();
        let mut recalled = VecDeque::with_capacity(limit.min(HISTORY_PAGE_LIMIT));
        let mut cursor = String::new();
        loop {
            let mut query = vec![
                ("channel", channel.as_str()),
                ("ts", thread),
                ("latest", before),
                ("inclusive", "false"),
                ("limit", page_limit.as_str()),
            ];
            if !cursor.is_empty() {
                query.push(("cursor", cursor.as_str()));
            }
            let page = self.history_page("conversations.replies", &query).await?;
            // Slack repeats the thread root at the head of every page.
            for message in past_messages(&page, bot, cutoff)? {
                if recalled
                    .back()
                    .is_some_and(|last: &PastMessage| message.at <= last.at)
                {
                    continue;
                }
                recalled.push_back(message);
                if recalled.len() > limit {
                    recalled.pop_front();
                }
            }
            cursor = page["response_metadata"]["next_cursor"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if cursor.is_empty() {
                return Ok(recalled.into());
            }
        }
    }
}

#[async_trait]
impl NativeStatus for SlackReplier {
    async fn set(&self, target: &LivenessTarget, status: Status) -> Result<(), TransportError> {
        let LivenessTarget::Slack {
            channel_id,
            thread_ts,
            initiator_user_id,
            ..
        } = target
        else {
            return Err(TransportError::Response);
        };
        if !self.agent_status_available.load(Ordering::Acquire) {
            return Err(TransportError::Service {
                code: "feature_disabled".to_owned(),
            });
        }
        let (state, initiator) = match status {
            Status::Working => ("processing", Some(initiator_user_id.as_str())),
            Status::Idle => ("active", None),
        };
        let result = self
            .set_agent_status(channel_id, thread_ts, state, initiator)
            .await;
        if let Err(error) = &result
            && permanent_agent_error(error)
            && self.agent_status_available.swap(false, Ordering::AcqRel)
        {
            tracing::warn!(
                event = "gateway_progress_degraded",
                transport = "slack",
                surface = "agent-status"
            );
        }
        result
    }
}

#[async_trait]
impl InboundReaction for SlackReplier {
    async fn set(&self, target: &LivenessTarget, present: bool) -> Result<(), TransportError> {
        let LivenessTarget::Slack {
            channel_id,
            message_ts,
            ..
        } = target
        else {
            return Err(TransportError::Response);
        };
        let key = format!("{channel_id}:{message_ts}");
        if !present {
            if self
                .added_reactions
                .lock()
                .expect("Slack reaction registry")
                .take(&key)
                .is_none()
            {
                return Ok(());
            }
            return match self
                .set_reaction(channel_id, message_ts, "reactions.remove")
                .await
            {
                Err(TransportError::Service { code }) if code == "no_reaction" => Ok(()),
                result => result,
            };
        }
        if !self.reaction_available.load(Ordering::Acquire) {
            return Err(TransportError::Service {
                code: "missing_scope".to_owned(),
            });
        }
        match self
            .set_reaction(channel_id, message_ts, "reactions.add")
            .await
        {
            Ok(()) => {
                self.added_reactions
                    .lock()
                    .expect("Slack reaction registry")
                    .insert(key, ());
                Ok(())
            }
            Err(TransportError::Service { code }) if code == "already_reacted" => Ok(()),
            Err(error) => {
                if permanent_reaction_error(&error)
                    && self.reaction_available.swap(false, Ordering::AcqRel)
                {
                    tracing::warn!(
                        event = "gateway_progress_degraded",
                        transport = "slack",
                        surface = "reaction"
                    );
                }
                Err(error)
            }
        }
    }
}

#[async_trait]
impl ProgressMessage for SlackReplier {
    fn limits(&self) -> ProgressLimits {
        ProgressLimits {
            max_chars: PROGRESS_MAX_CHARS,
            min_edit_interval: PROGRESS_MIN_EDIT_INTERVAL,
        }
    }

    async fn post(
        &self,
        target: &LivenessTarget,
        text: &ProgressText,
        cancel: bool,
    ) -> Result<MessageRef, TransportError> {
        let LivenessTarget::Slack {
            channel_id,
            thread_ts,
            conversation_id,
            ..
        } = target
        else {
            return Err(TransportError::Response);
        };
        let body = json!({
            "channel": channel_id,
            "thread_ts": thread_ts,
            "text": text.as_str(),
            "blocks": self.progress_blocks(text.as_str(), cancel, conversation_id),
        });
        let body = self.liveness_call("chat.postMessage", &body).await?;
        let (Some(posted_channel), Some(timestamp)) =
            (body["channel"].as_str(), body["ts"].as_str())
        else {
            return Err(TransportError::Response);
        };
        if posted_channel != channel_id || !canonical_timestamp(timestamp) {
            return Err(TransportError::Response);
        }
        Ok(MessageRef {
            target: target.clone(),
            id: timestamp.to_owned(),
        })
    }

    async fn edit(
        &self,
        message: &MessageRef,
        text: &ProgressText,
        cancel: bool,
    ) -> Result<(), TransportError> {
        let LivenessTarget::Slack {
            channel_id,
            conversation_id,
            ..
        } = &message.target
        else {
            return Err(TransportError::Response);
        };
        let body = json!({
            "channel": channel_id,
            "ts": message.id,
            "text": text.as_str(),
            "blocks": self.progress_blocks(text.as_str(), cancel, conversation_id),
        });
        self.liveness_call("chat.update", &body).await.map(|_| ())
    }

    async fn delete(&self, message: &MessageRef) -> Result<(), TransportError> {
        let LivenessTarget::Slack { channel_id, .. } = &message.target else {
            return Err(TransportError::Response);
        };
        self.liveness_call(
            "chat.delete",
            &json!({ "channel": channel_id, "ts": message.id }),
        )
        .await
        .map(|_| ())
    }

    async fn finalize(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError> {
        let LivenessTarget::Slack { channel_id, .. } = &message.target else {
            return Err(TransportError::Response);
        };
        if !reply.images.is_empty() {
            return Err(TransportError::Service {
                code: "answer-has-attachments".to_owned(),
            });
        }
        if reply.text.chars().count() > PROGRESS_MAX_CHARS {
            return Err(TransportError::Service {
                code: "answer-too-long".to_owned(),
            });
        }
        let body = json!({
            "channel": channel_id,
            "ts": message.id,
            "text": reply.text,
            "blocks": [{ "type": "markdown", "text": reply.text }],
        });
        self.liveness_call("chat.update", &body).await.map(|_| ())
    }
}

#[async_trait]
impl TextStream for SlackReplier {
    fn limits(&self) -> StreamLimits {
        StreamLimits {
            min_interval: STREAM_MIN_INTERVAL,
            max_chars: STREAM_MAX_CHARS,
        }
    }

    async fn show(
        &self,
        target: &LivenessTarget,
        message: Option<&MessageRef>,
        text: &StreamedText,
        _cancel: bool,
    ) -> Result<MessageRef, TransportError> {
        let LivenessTarget::Slack {
            channel_id,
            thread_ts,
            initiator_user_id,
            ..
        } = target
        else {
            return Err(TransportError::Response);
        };
        let whole = text.text.as_str();
        let Some(message) = message else {
            let body = json!({
                "channel": channel_id,
                "thread_ts": thread_ts,
                "recipient_user_id": initiator_user_id,
                "markdown_text": streamed_markdown(whole, text.truncated),
            });
            let body = self.liveness_call("chat.startStream", &body).await?;
            let Some(timestamp) = body["ts"].as_str() else {
                return Err(TransportError::Response);
            };
            if !canonical_timestamp(timestamp) {
                return Err(TransportError::Response);
            }
            self.streams.lock().expect("Slack stream registry").insert(
                timestamp.to_owned(),
                StreamState {
                    appended: whole.len(),
                    marked: text.truncated,
                },
            );
            return Ok(MessageRef {
                target: target.clone(),
                id: timestamp.to_owned(),
            });
        };
        let state = self
            .streams
            .lock()
            .expect("Slack stream registry")
            .get(&message.id)
            .copied()
            .unwrap_or_default();
        let appended = state.appended;
        let delta = if appended <= whole.len() && whole.is_char_boundary(appended) {
            &whole[appended..]
        } else {
            whole
        };
        let mark = text.truncated && !state.marked;
        if delta.is_empty() && !mark {
            return Ok(message.clone());
        }
        let body = json!({
            "channel": channel_id,
            "ts": message.id,
            "markdown_text": streamed_markdown(delta, mark),
        });
        self.liveness_call("chat.appendStream", &body).await?;
        self.streams.lock().expect("Slack stream registry").insert(
            message.id.clone(),
            StreamState {
                appended: whole.len(),
                marked: state.marked || text.truncated,
            },
        );
        Ok(message.clone())
    }

    async fn finalize(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError> {
        let LivenessTarget::Slack { channel_id, .. } = &message.target else {
            return Err(TransportError::Response);
        };
        if !reply.images.is_empty() {
            return Err(TransportError::Service {
                code: "answer-has-attachments".to_owned(),
            });
        }
        let mut body = json!({
            "channel": channel_id,
            "ts": message.id,
            "markdown_text": reply.text,
        });
        if self.experience == SlackExperience::Agent {
            body["session_status"] = Value::String("active".to_owned());
        }
        let result = self
            .liveness_call("chat.stopStream", &body)
            .await
            .map(|_| ());
        let _state = self
            .streams
            .lock()
            .expect("Slack stream registry")
            .take(&message.id);
        result
    }
}

#[async_trait]
impl CancelButton for SlackReplier {
    async fn ack(&self, press: &CancelPress) -> Result<(), TransportError> {
        let AckToken::Slack { envelope_id } = &press.ack else {
            return Err(TransportError::Response);
        };
        if envelope_id.is_empty() {
            return Err(TransportError::Response);
        }
        Ok(())
    }
}

impl SlackReplier {
    async fn upload_attachments(
        &self,
        channel: String,
        thread_ts: Option<String>,
        text: String,
        mut images: super::hydration::ImageQueue,
    ) -> Result<(), TransportError> {
        let mut accepted = false;
        while let Some(read) = images.next().await {
            let (index, image) = match read {
                Ok(read) => read,
                Err(_) if accepted => return Err(TransportError::PartialDelivery),
                Err(error) => return Err(error),
            };
            let comment = (index == 0)
                .then_some(text.as_str())
                .filter(|text| !text.is_empty());
            match self
                .upload_attachment(&channel, thread_ts.as_deref(), comment, image)
                .await
            {
                Ok(()) => accepted = true,
                Err(_) if accepted => return Err(TransportError::PartialDelivery),
                Err(error) => return Err(error),
            }
        }
        accepted.then_some(()).ok_or(TransportError::Response)
    }

    async fn upload_attachment(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        initial_comment: Option<&str>,
        image: super::hydration::HydratedImage,
    ) -> Result<(), TransportError> {
        let filename = image.filename;
        let length = image.bytes.len().to_string();
        let described = check_ok(
            self.http
                .post(format!("{}/api/files.getUploadURLExternal", self.endpoint))
                .header(
                    "authorization",
                    format!("Bearer {}", self.bot_token.expose()),
                )
                .form(&[("filename", filename.as_str()), ("length", length.as_str())])
                .send()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?,
        )
        .await?;
        let upload_url = described["upload_url"]
            .as_str()
            .ok_or(TransportError::Response)?;
        let file_id = described["file_id"]
            .as_str()
            .filter(|id| !id.trim().is_empty())
            .ok_or(TransportError::Response)?
            .to_owned();
        if !is_slack_upload_url(upload_url, &self.endpoint) {
            return Err(TransportError::Response);
        }
        let uploaded = self
            .http
            .post(upload_url)
            .header("content-type", image.media_type)
            .body(image.bytes)
            .send()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        if !uploaded.status().is_success() {
            return Err(TransportError::Service {
                code: format!("http-{}", uploaded.status().as_u16()),
            });
        }

        let mut body = json!({
            "files": [{"id": file_id, "title": filename}],
            "channel_id": channel,
        });
        if let Some(initial_comment) = initial_comment {
            body["initial_comment"] = Value::String(initial_comment.to_owned());
        }
        if let Some(thread_ts) = thread_ts {
            body["thread_ts"] = Value::String(thread_ts.to_owned());
        }
        #[allow(
            clippy::map_err_ignore,
            reason = "serializing a serde_json::Value cannot fail: it holds no non-string map keys \
                      and serde_json::Number rejects non-finite floats"
        )]
        let completed = check_ok(
            self.http
                .post(format!(
                    "{}/api/files.completeUploadExternal",
                    self.endpoint
                ))
                .header(
                    "authorization",
                    format!("Bearer {}", self.bot_token.expose()),
                )
                .header("content-type", "application/json; charset=utf-8")
                .body(serde_json::to_vec(&body).map_err(|_| TransportError::Response)?)
                .send()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)))?,
        )
        .await?;
        let accepted = completed["files"]
            .as_array()
            .is_some_and(|files| files.iter().any(|file| file["id"] == file_id));
        if !accepted {
            return Err(TransportError::Response);
        }
        Ok(())
    }
}

impl SlackReplier {
    async fn set_agent_status(
        &self,
        channel_id: &str,
        thread_ts: &str,
        status: &str,
        initiator_user_id: Option<&str>,
    ) -> Result<(), TransportError> {
        let mut body = json!({
            "channel_id": channel_id,
            "thread_ts": thread_ts,
            "status": status,
        });
        if let Some(initiator_user_id) = initiator_user_id {
            body["initiator_user_id"] = Value::String(initiator_user_id.to_owned());
        }
        self.liveness_call("agents.sessions.setStatus", &body)
            .await
            .map(|_| ())
    }

    async fn set_reaction(
        &self,
        channel: &str,
        timestamp: &str,
        method: &str,
    ) -> Result<(), TransportError> {
        self.liveness_call(
            method,
            &json!({
                "channel": channel,
                "timestamp": timestamp,
                "name": LIVENESS_REACTION,
            }),
        )
        .await
        .map(|_| ())
    }

    fn progress_blocks(&self, text: &str, cancel: bool, conversation_id: &str) -> Value {
        let mut blocks = vec![json!({ "type": "markdown", "text": text })];
        if cancel && self.experience == SlackExperience::Classic {
            blocks.push(json!({
                "type": "actions",
                "elements": [{
                    "type": "button",
                    "action_id": CANCEL_ACTION_ID,
                    "style": "danger",
                    "text": { "type": "plain_text", "text": "Stop" },
                    "value": conversation_id,
                }],
            }));
        }
        Value::Array(blocks)
    }

    fn begin_cooldown(&self, wait: Duration) {
        *self
            .progress_cooldown_until
            .lock()
            .expect("Slack progress cooldown") = Some(Instant::now() + wait);
    }

    async fn liveness_call(&self, method: &str, body: &Value) -> Result<Value, TransportError> {
        if self.progress_cooldown().is_some() {
            return Err(TransportError::Service {
                code: "ratelimited".to_owned(),
            });
        }
        #[allow(
            clippy::map_err_ignore,
            reason = "serializing a serde_json::Value cannot fail: it holds no non-string map keys \
                      and serde_json::Number rejects non-finite floats"
        )]
        let encoded = serde_json::to_vec(body).map_err(|_| TransportError::Response)?;
        let response = self
            .http
            .post(format!("{}/api/{method}", self.endpoint))
            .header(
                "authorization",
                format!("Bearer {}", self.bot_token.expose()),
            )
            .header("content-type", "application/json; charset=utf-8")
            .body(encoded)
            .timeout(LIVENESS_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            self.begin_cooldown(retry_after(response.headers()));
            return Err(TransportError::Service {
                code: "ratelimited".to_owned(),
            });
        }
        check_ok(response).await
    }

    async fn history_page(
        &self,
        method: &str,
        query: &[(&str, &str)],
    ) -> Result<Value, TransportError> {
        if self.progress_cooldown().is_some() {
            return Err(TransportError::Service {
                code: "ratelimited".to_owned(),
            });
        }
        let response = self
            .http
            .get(format!("{}/api/{method}", self.endpoint))
            .query(query)
            .header(
                "authorization",
                format!("Bearer {}", self.bot_token.expose()),
            )
            .send()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            self.begin_cooldown(retry_after(response.headers()));
            return Err(TransportError::Service {
                code: "ratelimited".to_owned(),
            });
        }
        check_ok(response).await
    }

    fn progress_cooldown(&self) -> Option<Duration> {
        let until = (*self
            .progress_cooldown_until
            .lock()
            .expect("Slack progress cooldown"))?;
        until.checked_duration_since(Instant::now())
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Duration {
    let seconds = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(DEFAULT_PROGRESS_COOLDOWN, Duration::from_secs);
    seconds.min(MAX_PROGRESS_COOLDOWN)
}

fn streamed_markdown(text: &str, mark: bool) -> String {
    if mark {
        format!("{text}{STREAM_TRUNCATION_MARKER}")
    } else {
        text.to_owned()
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct StreamState {
    appended: usize,
    marked: bool,
}

struct Tracked<T> {
    entries: VecDeque<(String, T)>,
    capacity: usize,
}

impl<T> Tracked<T> {
    fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn get(&self, key: &str) -> Option<&T> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value)
    }

    fn insert(&mut self, key: String, value: T) {
        self.entries.retain(|(candidate, _)| candidate != &key);
        self.entries.push_back((key, value));
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    fn take(&mut self, key: &str) -> Option<T> {
        let index = self
            .entries
            .iter()
            .position(|(candidate, _)| candidate == key)?;
        self.entries.remove(index).map(|(_, value)| value)
    }
}

fn permanent_agent_error(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::Service { code }
            if matches!(
                code.as_str(),
                "feature_disabled"
                    | "missing_scope"
                    | "not_allowed_token_type"
                    | "method_deprecated"
                    | "deprecated_endpoint"
            )
    )
}

fn permanent_reaction_error(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::Service { code }
            if matches!(code.as_str(), "missing_scope" | "not_allowed_token_type")
    )
}

struct SlackThreadOwnership {
    owned: Mutex<OwnedThreads>,
}

impl SlackThreadOwnership {
    fn new(capacity: usize) -> Self {
        Self {
            owned: Mutex::new(OwnedThreads::new(capacity)),
        }
    }

    fn owns(&self, claim: &ThreadClaim) -> bool {
        let key = SlackThreadKey::from_claim(claim);
        self.owned
            .lock()
            .expect("Slack thread ownership registry")
            .contains(&key)
    }
}

impl ThreadOwnership for SlackThreadOwnership {
    fn claim(&self, claim: ThreadClaim) {
        let key = SlackThreadKey::from_claim(&claim);
        self.owned
            .lock()
            .expect("Slack thread ownership registry")
            .claim(key);
    }

    fn revoke(&self, claim: &ThreadClaim) {
        let key = SlackThreadKey::from_claim(claim);
        self.owned
            .lock()
            .expect("Slack thread ownership registry")
            .revoke(&key);
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SlackThreadKey {
    team_id: String,
    channel_id: String,
    thread_ts: String,
    user_id: String,
}

impl SlackThreadKey {
    fn from_claim(claim: &ThreadClaim) -> Self {
        let ThreadClaim::Slack {
            team_id,
            channel_id,
            thread_ts,
            user_id,
        } = claim;
        Self {
            team_id: team_id.to_ascii_lowercase(),
            channel_id: channel_id.to_ascii_lowercase(),
            thread_ts: thread_ts.to_owned(),
            user_id: user_id.to_ascii_lowercase(),
        }
    }
}

struct OwnedThreads {
    order: VecDeque<SlackThreadKey>,
    owned: HashSet<SlackThreadKey>,
    capacity: usize,
}

impl OwnedThreads {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            owned: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    fn contains(&self, key: &SlackThreadKey) -> bool {
        self.owned.contains(key)
    }

    fn claim(&mut self, key: SlackThreadKey) {
        if self.owned.contains(&key) {
            self.order.retain(|candidate| candidate != &key);
        } else {
            self.owned.insert(key.clone());
        }
        self.order.push_back(key);
        while self.order.len() > self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.owned.remove(&evicted);
            }
        }
    }

    fn revoke(&mut self, key: &SlackThreadKey) {
        if self.owned.remove(key) {
            self.order.retain(|candidate| candidate != key);
        }
    }
}

fn pending_assets(files: &Value) -> Vec<PendingAsset> {
    let Some(files) = files.as_array() else {
        return Vec::new();
    };
    files
        .iter()
        .take(MAX_ATTACHMENTS)
        .map(|file| {
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
                size: file["size"].as_u64(),
                source: source.map(|(file_id, url)| AssetSourceRef::Slack {
                    file_id: file_id.to_owned(),
                    url: url.to_owned(),
                }),
            }
        })
        .collect()
}

/// Redirects are normally refused so a bearer token cannot be forwarded elsewhere; since Slack's
/// downloads do redirect, this transport follows exactly one hop to a known host itself.
const SLACK_FILE_HOSTS: [&str; 2] = ["files.slack.com", "slack.com"];

impl AssetFetcher for SlackReplier {
    fn fetch(
        &self,
        source: &AssetSourceRef,
        max_bytes: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, TransportError>> {
        let AssetSourceRef::Slack { url, .. } = source else {
            return Box::pin(async { Err(TransportError::Response) });
        };
        let url = url.clone();
        Box::pin(async move {
            let mut response = self.get_file(&url).await?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|value| value.to_str().ok())
                    .ok_or(TransportError::Response)?
                    .to_owned();
                response = self.get_file(&location).await?;
            }
            if !response.status().is_success() {
                return Err(TransportError::Service {
                    code: response.status().as_u16().to_string(),
                });
            }
            // Response size is sender-controlled and a chunked response need not declare a length
            // at all, so only the cutoff applied while reading each chunk actually bounds it.
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
        })
    }
}

impl SlackReplier {
    async fn get_file(&self, url: &str) -> Result<reqwest::Response, TransportError> {
        if !is_slack_file_url(url, &self.endpoint) {
            return Err(TransportError::Response);
        }
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

/// The URL is parsed, not prefix-matched, so files.slack.com.evil.test cannot be mistaken for
/// Slack, and the default port is required so no other listener can impersonate one.
pub(crate) fn is_slack_file_url(url: &str, endpoint: &str) -> bool {
    let (Ok(url), Ok(endpoint)) = (reqwest::Url::parse(url), reqwest::Url::parse(endpoint)) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    if endpoint.as_str().trim_end_matches('/') == SLACK_ENDPOINT {
        let host = url.host_str().unwrap_or_default();
        return url.scheme() == "https"
            && url.port().is_none()
            && SLACK_FILE_HOSTS
                .iter()
                .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")));
    }
    url.scheme() == "http"
        && url.origin() == endpoint.origin()
        && matches!(
            url.host_str().map(str::to_ascii_lowercase).as_deref(),
            Some("localhost" | "127.0.0.1" | "::1")
        )
}

pub(crate) fn is_slack_upload_url(url: &str, endpoint: &str) -> bool {
    let (Ok(url), Ok(endpoint)) = (reqwest::Url::parse(url), reqwest::Url::parse(endpoint)) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    if endpoint.as_str().trim_end_matches('/') == SLACK_ENDPOINT {
        let host = url.host_str().unwrap_or_default();
        return url.scheme() == "https"
            && (host == "files.slack.com" || host.ends_with(".files.slack.com"));
    }
    url.scheme() == "http"
        && url.origin() == endpoint.origin()
        && matches!(
            url.host_str().map(str::to_ascii_lowercase).as_deref(),
            Some("localhost" | "127.0.0.1" | "::1")
        )
}

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

fn past_messages(
    page: &Value,
    bot: &str,
    cutoff: SystemTime,
) -> Result<Vec<PastMessage>, TransportError> {
    let messages = page["messages"]
        .as_array()
        .ok_or(TransportError::Response)?;
    Ok(messages
        .iter()
        .filter_map(|message| past_message(message, bot))
        .filter(|message| message.at < cutoff)
        .collect())
}

fn past_message(message: &Value, bot: &str) -> Option<PastMessage> {
    if let Some(subtype) = message["subtype"].as_str()
        && !REQUEST_SUBTYPES.contains(&subtype)
    {
        return None;
    }
    let user = message["user"].as_str()?;
    let at = slack_instant(message["ts"].as_str()?)?;
    let text = bound_inbound(message["text"].as_str().unwrap_or_default());
    let assets = pending_assets(&message["files"]);
    if text.trim().is_empty() && assets.is_empty() {
        return None;
    }
    Some(PastMessage {
        from_bot: user == bot,
        author: user.to_owned(),
        text,
        assets,
        at,
    })
}

fn slack_instant(ts: &str) -> Option<SystemTime> {
    if !canonical_timestamp(ts) {
        return None;
    }
    let (seconds, micros) = ts.split_once('.')?;
    let since_epoch = Duration::new(
        seconds.parse().ok()?,
        micros.parse::<u32>().ok()?.checked_mul(1_000)?,
    );
    SystemTime::UNIX_EPOCH.checked_add(since_epoch)
}

fn canonical_timestamp(value: &str) -> bool {
    value.split_once('.').is_some_and(|(seconds, fraction)| {
        seconds.len() == 10
            && fraction.len() == 6
            && !seconds.starts_with('0')
            && seconds.bytes().all(|byte| byte.is_ascii_digit())
            && fraction.bytes().all(|byte| byte.is_ascii_digit())
    })
}

async fn check_ok(response: reqwest::Response) -> Result<Value, TransportError> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|source| TransportError::Request(Box::new(source)))?;
    let body =
        serde_json::from_slice::<Value>(&bytes).map_err(TransportError::MalformedResponse)?;
    if status.is_success() && body["ok"] == Value::Bool(true) {
        return Ok(body);
    }
    Err(TransportError::Service {
        code: if status.is_success() {
            body["error"]
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

#[cfg(test)]
mod driver_tests {
    use std::sync::{Arc, Mutex, atomic::AtomicBool};

    use dekopon_agent::{CancelVia, attachment::GeneratedImage};
    use dekopon_broker_protocol::ChatTransportKind;
    use dekopon_core::Redacted;
    use dekopon_model::ModelText;
    use dekopon_test_support::{CaptureLayer, OPENAI_CHAT_COMPLETIONS_TWO_DELTAS};
    use serde_json::{Value, json};
    use tracing::{Instrument as _, Span};
    use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

    use super::{
        CANCEL_ACTION_ID, Conversation, ConversationKind, HISTORY_PAGE_LIMIT,
        MAX_TRACKED_REACTIONS, MAX_TRACKED_STREAMS, OnceLock, PROGRESS_MAX_CHARS,
        SLACK_REQUEST_TIMEOUT, SlackReplier, SlackTransport, Tracked,
    };
    use crate::{
        config::{LivenessSettings, SlackExperience, SlackLivenessFallback, TemplateOverrides},
        progress::{ProgressDetail, ProgressText, Templates},
        transport::{
            AckToken, CancelButton, CancelPress, ChatDriver, ChatHistory as _, InboundReaction,
            LivenessTarget, MessageRef, NativeStatus, OutboundReply, ProgressMessage, Status,
            StreamedText, TextStream, TransportError, TransportEvent, credential_client,
            receive_span,
        },
    };

    const TEAM: &str = "T0123ABC";
    const CHANNEL: &str = "C0123ABC";
    const DIRECT_CHANNEL: &str = "D0123ABC";
    const GROUP_CHANNEL: &str = "G0123ABC";
    const USER: &str = "U9XYZ";
    const BOT_USER: &str = "UBOT123";
    const SUBJECT: &str = "slack.t0123abc.u9xyz";
    const CONVERSATION: &str = "c0123abc:1700000000.000001";
    const INBOUND_TS: &str = "1700000000.000001";
    const POSTED_TS: &str = "1700000000.000100";
    const STREAM_TS: &str = "1700000000.000200";
    const UNREACHABLE: &str = "http://127.0.0.1:1";

    fn target() -> LivenessTarget {
        LivenessTarget::Slack {
            channel_id: CHANNEL.to_owned(),
            thread_ts: INBOUND_TS.to_owned(),
            message_ts: INBOUND_TS.to_owned(),
            initiator_user_id: USER.to_owned(),
            conversation_id: CONVERSATION.to_owned(),
        }
    }

    fn progress_message() -> MessageRef {
        MessageRef {
            target: target(),
            id: POSTED_TS.to_owned(),
        }
    }

    fn replier(endpoint: &str, experience: SlackExperience) -> SlackReplier {
        replier_with(endpoint, experience, true)
    }

    fn replier_with(
        endpoint: &str,
        experience: SlackExperience,
        classic_reaction: bool,
    ) -> SlackReplier {
        SlackReplier {
            endpoint: endpoint.to_owned(),
            bot_token: Redacted::new("xoxb-test-bot-token".to_owned()),
            http: credential_client(SLACK_REQUEST_TIMEOUT)
                .build()
                .expect("credential client builds"),
            experience,
            classic_reaction,
            agent_status_available: AtomicBool::new(true),
            reaction_available: AtomicBool::new(true),
            added_reactions: Mutex::new(Tracked::new(MAX_TRACKED_REACTIONS)),
            progress_cooldown_until: Mutex::new(None),
            streams: Mutex::new(Tracked::new(MAX_TRACKED_STREAMS)),
            bot_user: OnceLock::from(BOT_USER.to_owned()),
        }
    }

    fn transport(endpoint: &str, experience: SlackExperience) -> SlackTransport {
        SlackTransport::new(
            "scientist-slack".to_owned(),
            endpoint.to_owned(),
            "xapp-test-app-token".to_owned(),
            "xoxb-test-bot-token".to_owned(),
            experience,
            LivenessSettings {
                classic_fallback: SlackLivenessFallback::Reaction,
                ..LivenessSettings::default()
            },
        )
        .expect("slack transport builds")
    }

    fn rendered() -> (ProgressText, ProgressText) {
        let (templates, problems) = Templates::resolve(
            &TemplateOverrides::default(),
            "Stopped.",
            "Something went wrong; nothing was changed.",
        );
        assert!(
            problems.is_empty(),
            "the sentences this daemon ships render: {problems:?}"
        );
        (
            templates.working(ProgressDetail::Plain, &Default::default()),
            templates.keep_alive(ProgressDetail::Plain, &Default::default()),
        )
    }

    fn recorded_text() -> ModelText {
        let events = dekopon_model::events_from_transcript(OPENAI_CHAT_COMPLETIONS_TWO_DELTAS)
            .expect("the recorded transcript parses");
        dekopon_test_support::scripted_text(&events)
    }

    fn streamed(text: ModelText) -> StreamedText {
        StreamedText {
            text,
            truncated: false,
        }
    }

    fn cut(text: ModelText) -> StreamedText {
        StreamedText {
            text,
            truncated: true,
        }
    }

    fn bodies(mock: &SlackMock, path: &str) -> Vec<Value> {
        mock.calls()
            .into_iter()
            .filter(|(candidate, _)| candidate == path)
            .map(|(_, body)| body)
            .collect()
    }

    fn png() -> GeneratedImage {
        GeneratedImage::from_png(vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
            .expect("a PNG signature is a PNG")
    }

    fn accepting(path: &str, body: &Value) -> (u16, Value) {
        if let Some(channel) = path.strip_prefix("/api/conversations.info?channel=") {
            return (
                200,
                json!({ "ok": true, "channel": {
                    "id": channel,
                    "is_im": channel.starts_with('D'),
                    "is_mpim": channel.starts_with('G'),
                    "is_channel": channel.starts_with('C'),
                    "is_group": false,
                }}),
            );
        }
        match path {
            "/api/chat.postMessage" => (
                200,
                json!({ "ok": true, "channel": body["channel"], "ts": POSTED_TS }),
            ),
            "/api/chat.startStream" => (
                200,
                json!({ "ok": true, "channel": body["channel"], "ts": STREAM_TS }),
            ),
            _ => (200, json!({ "ok": true })),
        }
    }

    struct SlackMock {
        base: String,
        calls: Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl SlackMock {
        fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().expect("mock call log").clone()
        }

        fn body(&self, path: &str) -> Value {
            let mut matching = self
                .calls()
                .into_iter()
                .filter(|(candidate, _)| candidate == path);
            let call = matching
                .next()
                .unwrap_or_else(|| panic!("{path} was called"));
            assert!(matching.next().is_none(), "{path} was called once");
            call.1
        }
    }

    #[allow(
        clippy::let_underscore_must_use,
        reason = "a mock that cannot finish writing its canned response leaves the call under test \
                  without one, which is what the calling test already asserts on"
    )]
    fn spawn_slack_mock<H>(handler: H) -> SlackMock
    where
        H: Fn(&str, &Value) -> (u16, Value) + Send + Sync + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("mock endpoint binds");
        let address = listener.local_addr().expect("mock endpoint address");
        listener
            .set_nonblocking(true)
            .expect("mock endpoint is pollable");
        let listener = tokio::net::TcpListener::from_std(listener).expect("mock endpoint adopts");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        tokio::spawn(async move {
            let handler = Arc::new(handler);
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = Arc::clone(&handler);
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let Some((path, body)) = read_request(&mut stream).await else {
                        return;
                    };
                    let body = if body.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_str::<Value>(&body)
                            .expect("every call with a body carries a JSON one")
                    };
                    recorded
                        .lock()
                        .expect("mock call log")
                        .push((path.clone(), body.clone()));
                    let (status, response) = handler(&path, &body);
                    let payload = serde_json::to_vec(&response).expect("mock response serializes");
                    let mut head = format!(
                        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                        if status == 429 {
                            "Too Many Requests"
                        } else {
                            "OK"
                        },
                        payload.len()
                    );
                    if status == 429 {
                        head.push_str("retry-after: 1\r\n");
                    }
                    head.push_str("\r\n");
                    use tokio::io::AsyncWriteExt as _;
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&payload).await;
                    let _ = stream.flush().await;
                });
            }
        });
        SlackMock {
            base: format!("http://{address}"),
            calls,
        }
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<(String, String)> {
        use tokio::io::AsyncReadExt as _;
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        let head_end = loop {
            if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break index;
            }
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
        };
        let head = String::from_utf8(buffer[..head_end].to_vec()).ok()?;
        let path = head.lines().next()?.split(' ').nth(1)?.to_owned();
        let mut length = 0_usize;
        for line in head.lines().skip(1) {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().ok()?;
            }
        }
        let mut body = buffer[head_end + 4..].to_vec();
        while body.len() < length {
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        Some((path, String::from_utf8(body).ok()?))
    }

    #[tokio::test]
    async fn the_agent_experience_owns_stop_while_the_classic_one_is_given_a_button() {
        let agent = replier(UNREACHABLE, SlackExperience::Agent);
        assert!(ChatDriver::status(&agent).is_some());
        assert!(
            ChatDriver::reaction(&agent).is_none(),
            "the fallback is offered only where the native status is not: an Agent session Slack \
             still accepts the status for carries the spinner alone, never a reaction beside it"
        );
        assert!(
            ChatDriver::reaction(&replier_with(UNREACHABLE, SlackExperience::Classic, false))
                .is_none(),
            "and nothing marks an inbound message when no fallback was configured"
        );
        assert!(
            ChatDriver::cancel_button(&agent).is_none(),
            "Slack renders its own Stop control beside an Agent session"
        );
        assert!(
            ChatDriver::typing(&agent).is_none(),
            "the modern Slack platform has no typing indicator"
        );

        let classic = replier(UNREACHABLE, SlackExperience::Classic);
        assert!(ChatDriver::status(&classic).is_none());
        assert!(ChatDriver::reaction(&classic).is_some());
        assert!(ChatDriver::cancel_button(&classic).is_some());
        assert!(ChatDriver::progress(&classic).is_some());
        assert!(ChatDriver::stream(&classic).is_some());
        let limits = ProgressMessage::limits(&classic);
        assert_eq!(
            limits.max_chars, 4_000,
            "Slack's own ceiling on one message"
        );
        assert_eq!(
            limits.min_edit_interval,
            std::time::Duration::from_secs(3),
            "Slack's documented floor between two edits"
        );
    }

    #[tokio::test]
    async fn the_agent_status_names_the_person_waiting_and_is_cleared_when_the_session_ends() {
        let mock = spawn_slack_mock(accepting);
        let replier = replier(&mock.base, SlackExperience::Agent);

        NativeStatus::set(&replier, &target(), Status::Working)
            .await
            .expect("Slack accepts the working status");
        NativeStatus::set(&replier, &target(), Status::Idle)
            .await
            .expect("Slack accepts the idle status");

        let calls = mock.calls();
        assert!(
            calls
                .iter()
                .all(|(path, _)| path == "/api/agents.sessions.setStatus"),
            "{calls:?}"
        );
        assert_eq!(
            calls[0].1,
            json!({
                "channel_id": CHANNEL,
                "thread_ts": INBOUND_TS,
                "status": "processing",
                "initiator_user_id": USER,
            })
        );
        assert_eq!(
            calls[1].1,
            json!({ "channel_id": CHANNEL, "thread_ts": INBOUND_TS, "status": "active" }),
            "clearing the status is about the session, not about who asked for it"
        );
    }

    #[tokio::test]
    async fn an_installation_without_agent_sessions_degrades_to_the_configured_reaction() {
        let mock = spawn_slack_mock(|path, _| {
            if path.ends_with("setStatus") {
                (200, json!({ "ok": false, "error": "feature_disabled" }))
            } else {
                (200, json!({ "ok": true }))
            }
        });
        let replier = replier(&mock.base, SlackExperience::Agent);
        assert!(
            ChatDriver::status(&replier).is_some(),
            "an Agent installation is asked for its own status first"
        );
        assert!(
            ChatDriver::reaction(&replier).is_none(),
            "and while that status is live the fallback is not offered beside it"
        );

        let error = NativeStatus::set(&replier, &target(), Status::Working)
            .await
            .expect_err("Slack refuses the status");

        assert!(
            matches!(&error, TransportError::Service { code } if code == "feature_disabled"),
            "{error:?}"
        );
        assert!(
            ChatDriver::status(&replier).is_none(),
            "the refusal is permanent for this installation rather than per session"
        );
        assert!(
            ChatDriver::reaction(&replier).is_some(),
            "and the configured fallback is what the policy is left with"
        );
    }

    #[tokio::test]
    async fn the_marker_is_removed_only_by_the_run_that_added_it() {
        let mock = spawn_slack_mock(accepting);
        let replier = replier(&mock.base, SlackExperience::Classic);

        InboundReaction::set(&replier, &target(), true)
            .await
            .expect("Slack accepts the reaction");
        InboundReaction::set(&replier, &target(), false)
            .await
            .expect("Slack accepts the removal");
        InboundReaction::set(&replier, &target(), false)
            .await
            .expect("a second removal is not a failure");

        assert_eq!(
            mock.body("/api/reactions.add"),
            json!({ "channel": CHANNEL, "timestamp": INBOUND_TS, "name": "tangerine" })
        );
        assert_eq!(
            mock.body("/api/reactions.remove"),
            json!({ "channel": CHANNEL, "timestamp": INBOUND_TS, "name": "tangerine" })
        );
        assert_eq!(
            mock.calls().len(),
            2,
            "a marker this run no longer owns is never removed again: {:?}",
            mock.calls()
        );
    }

    #[tokio::test]
    async fn a_classic_progress_message_is_a_thread_reply_carrying_the_stop_button() {
        let mock = spawn_slack_mock(accepting);
        let replier = replier(&mock.base, SlackExperience::Classic);
        let (working, still_working) = rendered();

        let message = ProgressMessage::post(&replier, &target(), &working, true)
            .await
            .expect("Slack accepts the progress message");
        ProgressMessage::edit(&replier, &message, &still_working, true)
            .await
            .expect("Slack accepts the edit");
        ProgressMessage::delete(&replier, &message)
            .await
            .expect("Slack accepts the deletion");

        assert_eq!(message.id, POSTED_TS);
        let posted = mock.body("/api/chat.postMessage");
        assert_eq!(posted["channel"], json!(CHANNEL));
        assert_eq!(
            posted["thread_ts"],
            json!(INBOUND_TS),
            "progress belongs in the thread it reports on, never in the channel"
        );
        assert_eq!(posted["text"], json!(working.as_str()));
        assert_eq!(
            posted["blocks"][0],
            json!({ "type": "markdown", "text": working.as_str() }),
            "the notification fallback and the rendered block say the same thing"
        );
        assert_eq!(
            posted["blocks"][1]["elements"][0]["action_id"],
            json!(CANCEL_ACTION_ID)
        );
        assert_eq!(
            posted["blocks"][1]["elements"][0]["value"],
            json!(CONVERSATION),
            "the button names the conversation a press stops, in the canonical form the registry \
             keys on rather than the original-case channel Slack sent"
        );
        let edited = mock.body("/api/chat.update");
        assert_eq!(edited["channel"], json!(CHANNEL));
        assert_eq!(
            edited["ts"],
            json!(POSTED_TS),
            "the same message, edited in place"
        );
        assert_eq!(edited["text"], json!(still_working.as_str()));
        assert_eq!(
            edited["blocks"],
            json!([
                { "type": "markdown", "text": still_working.as_str() },
                posted["blocks"][1],
            ]),
            "the button survives an edit; only the status line changes"
        );
        assert_eq!(
            mock.body("/api/chat.delete"),
            json!({ "channel": CHANNEL, "ts": POSTED_TS })
        );
    }

    #[tokio::test]
    async fn an_agent_progress_message_carries_no_button_of_this_gateways_own() {
        let mock = spawn_slack_mock(accepting);
        let replier = replier(&mock.base, SlackExperience::Agent);
        let (working, _) = rendered();

        ProgressMessage::post(&replier, &target(), &working, true)
            .await
            .expect("Slack accepts the progress message");

        let posted = mock.body("/api/chat.postMessage");
        assert_eq!(
            posted["blocks"].as_array().expect("blocks").len(),
            1,
            "Slack's own Stop control is the one on an Agent session: {posted}"
        );
    }

    #[tokio::test]
    async fn the_progress_message_becomes_the_answer_in_place() {
        let mock = spawn_slack_mock(accepting);
        let replier = replier(&mock.base, SlackExperience::Classic);

        ProgressMessage::finalize(
            &replier,
            &progress_message(),
            &OutboundReply::text("All good."),
        )
        .await
        .expect("Slack accepts the final edit");

        assert_eq!(
            mock.body("/api/chat.update"),
            json!({
                "channel": CHANNEL,
                "ts": POSTED_TS,
                "text": "All good.",
                "blocks": [{ "type": "markdown", "text": "All good." }],
            }),
            "the answer replaces the status line and the button with it"
        );
    }

    #[tokio::test]
    async fn an_answer_this_message_cannot_hold_is_refused_before_slack_is_asked() {
        let mock = spawn_slack_mock(accepting);
        let replier = replier(&mock.base, SlackExperience::Classic);

        let error = ProgressMessage::finalize(
            &replier,
            &progress_message(),
            &OutboundReply::with_images("Here it is.", vec![png()]),
        )
        .await
        .expect_err("an upload posts its own message, so this one cannot become the answer");

        let too_long = ProgressMessage::finalize(
            &replier,
            &progress_message(),
            &OutboundReply::text("x".repeat(PROGRESS_MAX_CHARS + 1)),
        )
        .await
        .expect_err("an answer past Slack's ceiling for one message cannot become it either");

        assert!(
            matches!(&error, TransportError::Service { code } if code == "answer-has-attachments"),
            "{error:?}"
        );
        assert!(
            matches!(&too_long, TransportError::Service { code } if code == "answer-too-long"),
            "{too_long:?}"
        );
        assert!(
            mock.calls().is_empty(),
            "both refusals are decided here, so the policy deletes and replies without a round trip"
        );
    }

    #[tokio::test]
    async fn a_stream_opens_in_the_thread_and_appends_only_what_is_new() {
        let mock = spawn_slack_mock(accepting);
        let replier = replier(&mock.base, SlackExperience::Agent);
        let whole = recorded_text();
        let first_delta = whole.truncated(7);
        let second_delta = whole.as_str()[first_delta.len()..].to_owned();

        let stream = TextStream::show(
            &replier,
            &target(),
            None,
            &streamed(first_delta.clone()),
            false,
        )
        .await
        .expect("Slack opens the stream");
        let same = TextStream::show(
            &replier,
            &target(),
            Some(&stream),
            &streamed(whole.clone()),
            false,
        )
        .await
        .expect("Slack accepts the append");
        TextStream::show(
            &replier,
            &target(),
            Some(&stream),
            &streamed(whole.clone()),
            false,
        )
        .await
        .expect("unchanged text is not an append");
        TextStream::finalize(&replier, &stream, &OutboundReply::text(whole.as_str()))
            .await
            .expect("Slack closes the stream");

        assert!(
            !second_delta.is_empty(),
            "the recorded transcript carries more than the opening delta"
        );
        assert_eq!(stream.id, STREAM_TS);
        assert_eq!(
            same, stream,
            "one stream is one message for the whole session"
        );
        assert_eq!(
            mock.body("/api/chat.startStream"),
            json!({
                "channel": CHANNEL,
                "thread_ts": INBOUND_TS,
                "recipient_user_id": USER,
                "markdown_text": first_delta.as_str(),
            })
        );
        assert_eq!(
            mock.body("/api/chat.appendStream"),
            json!({ "channel": CHANNEL, "ts": STREAM_TS, "markdown_text": second_delta }),
            "Slack appends, so the driver sends only the new part of cumulative text"
        );
        assert_eq!(
            mock.body("/api/chat.stopStream"),
            json!({
                "channel": CHANNEL,
                "ts": STREAM_TS,
                "markdown_text": whole.as_str(),
                "session_status": "active",
            }),
            "the answer lands in the streamed message rather than beside it"
        );
        assert_eq!(mock.calls().len(), 3, "{:?}", mock.calls());
    }

    #[tokio::test]
    async fn a_cut_stream_appends_the_marker_once() {
        let mock = spawn_slack_mock(accepting);
        let replier = replier(&mock.base, SlackExperience::Agent);
        let whole = recorded_text();
        let opening = whole.truncated(7);
        let rest = whole.as_str()[opening.len()..].to_owned();

        let stream = TextStream::show(&replier, &target(), None, &streamed(opening), false)
            .await
            .expect("Slack opens the stream");
        TextStream::show(
            &replier,
            &target(),
            Some(&stream),
            &cut(whole.clone()),
            false,
        )
        .await
        .expect("Slack takes the append that reached the ceiling");
        TextStream::show(&replier, &target(), Some(&stream), &cut(whole), false)
            .await
            .expect("a bounded prefix that has not grown is not an append");

        assert_eq!(
            bodies(&mock, "/api/chat.appendStream"),
            vec![json!({
                "channel": CHANNEL,
                "ts": STREAM_TS,
                "markdown_text": format!("{rest}…"),
            })],
            "the ellipsis arrives with the text that hit the ceiling and is not repeated after it"
        );
    }

    #[tokio::test]
    async fn a_stream_that_opens_past_the_ceiling_says_so_in_the_opening_call() {
        let mock = spawn_slack_mock(accepting);
        let replier = replier(&mock.base, SlackExperience::Agent);
        let whole = recorded_text();

        let stream = TextStream::show(&replier, &target(), None, &cut(whole.clone()), false)
            .await
            .expect("Slack opens the stream");
        TextStream::show(
            &replier,
            &target(),
            Some(&stream),
            &cut(whole.clone()),
            false,
        )
        .await
        .expect("the same bounded prefix is not an append");

        assert_eq!(
            mock.body("/api/chat.startStream")["markdown_text"],
            json!(format!("{}…", whole.as_str())),
            "a first show already past the ceiling wears the marker from the start"
        );
        assert!(
            bodies(&mock, "/api/chat.appendStream").is_empty(),
            "nothing is appended, marker included, once the opening call carried it"
        );
    }

    #[tokio::test]
    async fn a_throttled_progress_call_is_never_retried_and_suppresses_the_next_one() {
        let mock = spawn_slack_mock(|_, _| (429, json!({ "ok": false, "error": "ratelimited" })));
        let replier = replier(&mock.base, SlackExperience::Classic);
        let (working, still_working) = rendered();

        let throttled = ProgressMessage::edit(&replier, &progress_message(), &working, false)
            .await
            .expect_err("Slack refuses the edit");
        let suppressed =
            ProgressMessage::edit(&replier, &progress_message(), &still_working, false)
                .await
                .expect_err("the cooldown Slack named is honored");

        assert!(
            matches!(&throttled, TransportError::Service { code } if code == "ratelimited"),
            "{throttled:?}"
        );
        assert!(
            matches!(&suppressed, TransportError::Service { code } if code == "ratelimited"),
            "{suppressed:?}"
        );
        assert_eq!(
            mock.calls().len(),
            1,
            "Tier 3 is counted per app per workspace, so the second call is not made at all: {:?}",
            mock.calls()
        );
    }

    #[tokio::test]
    async fn a_stop_press_is_acknowledged_by_the_envelope_and_routed_to_its_conversation() {
        let mock = spawn_slack_mock(accepting);
        let mut transport = transport(&mock.base, SlackExperience::Classic);
        let frame = json!({
            "type": "interactive",
            "envelope_id": "envelope-7",
            "payload": {
                "type": "block_actions",
                "team": { "id": TEAM },
                "user": { "id": USER },
                "channel": { "id": CHANNEL },
                "message": { "ts": POSTED_TS, "thread_ts": INBOUND_TS },
                "actions": [{
                    "action_id": CANCEL_ACTION_ID,
                    "value": CONVERSATION,
                }],
            },
        });

        transport
            .cancel_pressed(&frame)
            .await
            .expect("the press is acknowledged");

        let event = transport
            .pending
            .pop_front()
            .expect("the press is routed as a cancel request");
        let TransportEvent::CancelRequested(request) = event else {
            panic!("a press on the Stop button is routed as a cancel request");
        };
        assert_eq!(request.transport, "scientist-slack");
        assert_eq!(request.conversation_id, CONVERSATION);
        assert_eq!(request.subject, SUBJECT);
        assert_eq!(request.via, CancelVia::Button);
        assert!(
            mock.calls().is_empty(),
            "the envelope acknowledgment is the whole acknowledgment Slack asks for"
        );
    }

    #[tokio::test]
    async fn a_press_on_another_apps_button_routes_nothing() {
        let mut transport = transport(UNREACHABLE, SlackExperience::Classic);
        let frame = json!({
            "type": "interactive",
            "envelope_id": "envelope-8",
            "payload": {
                "type": "block_actions",
                "team": { "id": TEAM },
                "user": { "id": USER },
                "channel": { "id": CHANNEL },
                "message": { "ts": POSTED_TS, "thread_ts": INBOUND_TS },
                "actions": [{ "action_id": "other-app-stop", "value": "whatever" }],
            },
        });

        transport
            .cancel_pressed(&frame)
            .await
            .expect("another app's press is not this transport's failure");

        assert!(transport.pending.is_empty());
    }

    #[tokio::test]
    async fn an_acknowledgment_for_another_transport_reports_the_token_it_was_given() {
        let replier = replier(UNREACHABLE, SlackExperience::Classic);
        let press = CancelPress {
            target: target(),
            subject: SUBJECT.to_owned(),
            ack: AckToken::Local,
        };

        let error = CancelButton::ack(&replier, &press)
            .await
            .expect_err("a token minted by another transport is a routing mistake");

        assert!(matches!(&error, TransportError::Response), "{error:?}");
    }

    #[tokio::test]
    async fn a_pressed_stop_button_names_the_conversation_the_registry_keys_on() {
        let mock = spawn_slack_mock(accepting);
        let mut transport = transport(&mock.base, SlackExperience::Classic);
        let (working, _) = rendered();
        let mention = json!({
            "type": "app_mention",
            "channel": CHANNEL,
            "user": USER,
            "ts": INBOUND_TS,
            "text": "<@B0123ABC> how are things?",
        });

        let message = transport
            .routable(TEAM, &mention, &Span::none())
            .await
            .expect("the mention is readable")
            .expect("the mention is routable");
        let target = message.liveness.clone().expect("a Slack message is live");
        ProgressMessage::post(&*transport.replier, &target, &working, true)
            .await
            .expect("the progress message is posted");
        let value = mock.body("/api/chat.postMessage")["blocks"][1]["elements"][0]["value"].clone();
        let frame = json!({
            "type": "interactive",
            "envelope_id": "envelope-9",
            "payload": {
                "type": "block_actions",
                "team": { "id": TEAM },
                "user": { "id": USER },
                "channel": { "id": CHANNEL },
                "message": { "ts": POSTED_TS, "thread_ts": INBOUND_TS },
                "actions": [{ "action_id": CANCEL_ACTION_ID, "value": value }],
            },
        });
        transport
            .cancel_pressed(&frame)
            .await
            .expect("the press is acknowledged");

        let TransportEvent::CancelRequested(request) = transport
            .pending
            .pop_front()
            .expect("the press is routed as a cancel request")
        else {
            panic!("a press on the Stop button is routed as a cancel request");
        };
        assert_eq!(
            request.conversation_id,
            message.conversation.key(),
            "the value on the button and the key the session is registered under are one value"
        );
        assert_eq!(request.conversation_id, CONVERSATION);
        assert_eq!(request.subject, SUBJECT);
        assert_eq!(request.via, CancelVia::Button);
    }

    #[tokio::test]
    async fn a_native_stop_names_the_conversation_routing_minted() {
        let mock = spawn_slack_mock(accepting);
        let mut transport = transport(&mock.base, SlackExperience::Agent);
        let mention = json!({
            "type": "app_mention",
            "channel": CHANNEL,
            "user": USER,
            "ts": INBOUND_TS,
            "text": "<@B0123ABC> how are things?",
        });

        let message = transport
            .routable(TEAM, &mention, &Span::none())
            .await
            .expect("the mention is readable")
            .expect("the mention is routable");
        let stopped = transport
            .session_stopped(
                TEAM,
                &json!({
                    "type": "agent_session_stopped",
                    "channel": CHANNEL,
                    "thread_ts": INBOUND_TS,
                    "user": USER,
                }),
            )
            .expect("the envelope is readable")
            .expect("a native stop is a cancel request");

        assert_eq!(stopped.conversation_id, message.conversation.key());
        assert_eq!(stopped.via, CancelVia::NativeStop);
        assert_eq!(stopped.subject, SUBJECT);
    }

    #[tokio::test]
    async fn a_mention_resolves_its_kind_through_one_bounded_conversations_info() {
        let mock = spawn_slack_mock(accepting);
        let mut transport = transport(&mock.base, SlackExperience::Classic);
        let mention = |channel: &str, ts: &str| {
            json!({
                "type": "app_mention",
                "channel": channel,
                "user": USER,
                "ts": ts,
                "text": "<@B0123ABC> how are things?",
            })
        };

        let direct = transport
            .routable(TEAM, &mention(DIRECT_CHANNEL, INBOUND_TS), &Span::none())
            .await
            .expect("readable")
            .expect("routable");
        let group = transport
            .routable(TEAM, &mention(GROUP_CHANNEL, POSTED_TS), &Span::none())
            .await
            .expect("readable")
            .expect("routable");
        let public = transport
            .routable(TEAM, &mention(CHANNEL, STREAM_TS), &Span::none())
            .await
            .expect("readable")
            .expect("routable");
        let again = transport
            .routable(TEAM, &mention(CHANNEL, "1700000000.000300"), &Span::none())
            .await
            .expect("readable")
            .expect("routable");

        assert_eq!(direct.conversation.kind, ConversationKind::DirectMessage);
        assert_eq!(
            group.conversation.kind,
            ConversationKind::GroupDirectMessage
        );
        assert_eq!(public.conversation.kind, ConversationKind::Channel);
        assert_eq!(again.conversation.kind, ConversationKind::Channel);
        assert_eq!(
            mock.calls().len(),
            3,
            "one lookup per conversation, not one per message: {:?}",
            mock.calls()
        );
    }

    #[tokio::test]
    async fn a_mention_whose_kind_cannot_be_resolved_is_dropped_naming_its_cause() {
        let capture = CaptureLayer::workspace();
        let _subscriber = tracing_subscriber::registry()
            .with(capture.clone())
            .set_default();
        let mut transport = transport(UNREACHABLE, SlackExperience::Classic);
        let span = receive_span(ChatTransportKind::Slack);

        let routed = transport
            .routable(
                TEAM,
                &json!({
                    "type": "app_mention",
                    "channel": CHANNEL,
                    "user": USER,
                    "ts": INBOUND_TS,
                    "text": "<@B0123ABC> how are things?",
                }),
                &span,
            )
            .instrument(span.clone())
            .await
            .expect("a drop is not a transport failure");
        drop(span);

        assert!(routed.is_none(), "an unplaceable mention is not routed");
        assert!(
            capture
                .spans_text()
                .contains("drop.reason=\"conversation-unresolved\""),
            "{}",
            capture.spans_text()
        );
        assert!(
            capture.saw("cause=\"request\""),
            "the drop names which step failed: {}",
            capture.events_text()
        );
    }

    #[tokio::test]
    async fn a_thread_root_is_the_conversation_it_was_posted_in_rather_than_a_thread_under_itself()
    {
        let mock = spawn_slack_mock(accepting);
        let mut transport = transport(&mock.base, SlackExperience::Classic);
        let root = json!({
            "type": "app_mention",
            "channel": CHANNEL,
            "user": USER,
            "ts": INBOUND_TS,
            "thread_ts": INBOUND_TS,
            "text": "<@B0123ABC> how are things?",
        });
        let reply = json!({
            "type": "app_mention",
            "channel": CHANNEL,
            "user": USER,
            "ts": POSTED_TS,
            "thread_ts": INBOUND_TS,
            "text": "<@B0123ABC> and then?",
        });

        let opened = transport
            .routable(TEAM, &root, &Span::none())
            .await
            .expect("readable")
            .expect("routable");
        let inside = transport
            .routable(TEAM, &reply, &Span::none())
            .await
            .expect("readable")
            .expect("routable");

        assert_eq!(opened.conversation.kind, ConversationKind::Channel);
        assert_eq!(inside.conversation.kind, ConversationKind::Thread);
        assert_eq!(
            opened.conversation.key(),
            inside.conversation.key(),
            "the opening question and the answers to it are one exchange"
        );
    }

    fn direct_conversation() -> Conversation {
        Conversation {
            kind: ConversationKind::DirectMessage,
            container: Some(TEAM.to_ascii_lowercase()),
            id: DIRECT_CHANNEL.to_ascii_lowercase(),
            thread: None,
        }
    }

    fn thread_conversation() -> Conversation {
        Conversation {
            kind: ConversationKind::Thread,
            container: Some(TEAM.to_ascii_lowercase()),
            id: CHANNEL.to_ascii_lowercase(),
            thread: Some(INBOUND_TS.to_owned()),
        }
    }

    fn said(user: &str, ts: &str, text: &str) -> Value {
        json!({ "type": "message", "user": user, "ts": ts, "text": text })
    }

    fn photo() -> Value {
        json!([{
            "id": "F0PHOTO",
            "name": "cat.png",
            "mimetype": "image/png",
            "size": 42,
            "url_private_download": "http://127.0.0.1:1/files/cat.png",
        }])
    }

    #[tokio::test]
    async fn a_direct_message_is_recalled_oldest_first_from_conversations_history() {
        let mock = spawn_slack_mock(|path, _| {
            assert!(path.starts_with("/api/conversations.history?"), "{path}");
            let mut with_photo = said(USER, "1700000000.000003", "look");
            with_photo["subtype"] = json!("file_share");
            with_photo["files"] = photo();
            let mut joined = said(USER, "1700000000.000002", "joined");
            joined["subtype"] = json!("channel_join");
            (
                200,
                json!({ "ok": true, "messages": [
                    said(USER, POSTED_TS, "the trigger itself"),
                    said(BOT_USER, "1700000000.000004", "a cat"),
                    with_photo,
                    joined,
                    said(USER, INBOUND_TS, "hello"),
                ]}),
            )
        });
        let replier = replier(&mock.base, SlackExperience::Classic);

        let recalled = replier
            .history()
            .expect("Slack reads its own history")
            .recent(&direct_conversation(), POSTED_TS, 3)
            .await
            .expect("the history reads");

        let path = &mock.calls()[0].0;
        assert_eq!(
            path,
            &format!(
                "/api/conversations.history?channel={DIRECT_CHANNEL}&latest={POSTED_TS}&inclusive=false&limit=3"
            )
        );
        let texts = recalled
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
            texts,
            [
                (false, USER, "hello"),
                (false, USER, "look"),
                (true, BOT_USER, "a cat"),
            ]
        );
        assert!(recalled.windows(2).all(|pair| pair[0].at < pair[1].at));
    }

    #[tokio::test]
    async fn a_recalled_attachment_is_the_asset_the_inbound_path_would_have_built() {
        let mock = spawn_slack_mock(|path, _| {
            if path.starts_with("/api/conversations.history?") {
                let mut shared = said(USER, INBOUND_TS, "look");
                shared["subtype"] = json!("file_share");
                shared["files"] = photo();
                return (200, json!({ "ok": true, "messages": [shared] }));
            }
            (200, json!({ "ok": true }))
        });
        let mut transport = transport(&mock.base, SlackExperience::Classic);
        let inbound = transport
            .routable(
                TEAM,
                &json!({
                    "type": "message",
                    "subtype": "file_share",
                    "channel": DIRECT_CHANNEL,
                    "channel_type": "im",
                    "user": USER,
                    "ts": INBOUND_TS,
                    "text": "look",
                    "files": photo(),
                }),
                &Span::none(),
            )
            .await
            .expect("readable")
            .expect("routable");

        let recalled = replier(&mock.base, SlackExperience::Classic)
            .recent(&direct_conversation(), POSTED_TS, 10)
            .await
            .expect("the history reads");

        assert_eq!(recalled.len(), 1);
        assert_eq!(recalled[0].assets, inbound.assets);
    }

    #[tokio::test]
    async fn a_thread_keeps_only_the_newest_replies_across_pages_without_repeating_the_root() {
        let root = said(USER, INBOUND_TS, "question");
        let mock = spawn_slack_mock(move |path, _| {
            let page = if path.contains("cursor=") {
                json!({ "ok": true, "messages": [
                    root.clone(),
                    said(USER, "1700000000.000003", "second"),
                    said(BOT_USER, "1700000000.000004", "third"),
                ]})
            } else {
                json!({ "ok": true, "messages": [
                    root.clone(),
                    said(BOT_USER, "1700000000.000002", "first"),
                ], "response_metadata": { "next_cursor": "bmV4dA==" } })
            };
            (200, page)
        });

        let recalled = replier(&mock.base, SlackExperience::Agent)
            .recent(&thread_conversation(), POSTED_TS, 3)
            .await
            .expect("the history reads");

        let paths = mock
            .calls()
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>();
        let first = format!(
            "/api/conversations.replies?channel={CHANNEL}&ts={INBOUND_TS}&latest={POSTED_TS}&inclusive=false&limit={HISTORY_PAGE_LIMIT}"
        );
        assert_eq!(
            paths,
            [first.clone(), format!("{first}&cursor=bmV4dA%3D%3D")]
        );
        let texts = recalled
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(texts, ["first", "second", "third"]);
        assert!(recalled[0].from_bot && !recalled[1].from_bot && recalled[2].from_bot);
    }

    #[tokio::test]
    async fn a_history_read_asks_for_at_most_one_page_of_slacks_recommended_size() {
        let mock = spawn_slack_mock(|_, _| (200, json!({ "ok": true, "messages": [] })));
        let replier = replier(&mock.base, SlackExperience::Classic);

        replier
            .recent(&direct_conversation(), POSTED_TS, HISTORY_PAGE_LIMIT)
            .await
            .expect("the history reads");
        replier
            .recent(&direct_conversation(), POSTED_TS, HISTORY_PAGE_LIMIT + 1)
            .await
            .expect("the history reads");

        let limits = mock
            .calls()
            .into_iter()
            .map(|(path, _)| path.rsplit_once("limit=").expect("a limit").1.to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            limits,
            [
                HISTORY_PAGE_LIMIT.to_string(),
                HISTORY_PAGE_LIMIT.to_string()
            ]
        );
    }

    #[tokio::test]
    async fn a_refused_history_read_is_an_error_naming_slacks_code() {
        let mock =
            spawn_slack_mock(|_, _| (200, json!({ "ok": false, "error": "channel_not_found" })));

        let refused = replier(&mock.base, SlackExperience::Classic)
            .recent(&direct_conversation(), POSTED_TS, 10)
            .await;

        assert!(
            matches!(&refused, Err(TransportError::Service { code }) if code == "channel_not_found"),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn a_throttled_history_read_fails_and_holds_the_shared_cooldown() {
        let mock = spawn_slack_mock(|_, _| (429, json!({ "ok": false, "error": "ratelimited" })));
        let replier = replier(&mock.base, SlackExperience::Classic);

        let throttled = replier.recent(&direct_conversation(), POSTED_TS, 10).await;
        let held = replier.recent(&thread_conversation(), POSTED_TS, 10).await;

        for result in [&throttled, &held] {
            assert!(
                matches!(result, Err(TransportError::Service { code }) if code == "ratelimited"),
                "{result:?}"
            );
        }
        assert_eq!(
            mock.calls().len(),
            1,
            "the cooldown suppresses the next read"
        );
        assert!(replier.progress_cooldown().is_some());
    }

    #[tokio::test]
    async fn history_is_unreadable_until_the_bot_knows_who_it_is() {
        let mut replier = replier(UNREACHABLE, SlackExperience::Classic);
        replier.bot_user = OnceLock::new();

        let early = replier.recent(&direct_conversation(), POSTED_TS, 10).await;

        assert!(matches!(early, Err(TransportError::Closed)), "{early:?}");
    }

    #[test]
    fn a_full_stream_registry_forgets_the_oldest_stream_rather_than_growing() {
        let mut tracked = Tracked::new(2);

        tracked.insert("first".to_owned(), 1_usize);
        tracked.insert("second".to_owned(), 2);
        tracked.insert("first".to_owned(), 3);
        tracked.insert("third".to_owned(), 4);

        assert_eq!(tracked.get("first").copied(), Some(3), "a rewrite replaces");
        assert_eq!(tracked.get("third").copied(), Some(4));
        assert_eq!(
            tracked.get("second"),
            None,
            "the least recently recorded entry is the one evicted"
        );
        assert_eq!(tracked.take("third"), Some(4));
        assert_eq!(tracked.take("third"), None, "removal answers once");
    }
}

#[cfg(test)]
mod owned_thread_tests {
    use super::{OwnedThreads, SlackThreadKey};

    fn key(thread: &str, user: &str) -> SlackThreadKey {
        SlackThreadKey {
            team_id: "t0123abc".to_owned(),
            channel_id: "c0123abc".to_owned(),
            thread_ts: thread.to_owned(),
            user_id: user.to_owned(),
        }
    }

    #[test]
    fn claims_refresh_lru_order_and_revoke_exactly_one_sender_thread() {
        let first = key("1.000001", "u1");
        let second = key("2.000002", "u1");
        let other_sender = key("1.000001", "u2");
        let mut owned = OwnedThreads::new(2);

        owned.claim(first.clone());
        owned.claim(second.clone());
        owned.claim(first.clone());
        owned.claim(other_sender.clone());

        assert!(owned.contains(&first), "a refreshed claim remains owned");
        assert!(owned.contains(&other_sender));
        assert!(
            !owned.contains(&second),
            "the least recently authorized claim is evicted"
        );

        owned.revoke(&first);
        assert!(!owned.contains(&first));
        assert!(
            owned.contains(&other_sender),
            "revocation is exact to one sender/thread"
        );
    }
}
