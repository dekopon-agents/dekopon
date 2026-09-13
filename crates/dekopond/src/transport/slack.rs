//! Slack Socket Mode: an outbound WebSocket instead of an inbound webhook.
//!
//! Socket Mode exists so a daemon behind NAT needs no public HTTP endpoint. The protocol's one
//! sharp edge is redelivery: Slack expects an acknowledgment within roughly three seconds and
//! resends the envelope otherwise. A Dekopon session takes far longer than that, so **the ack is
//! sent before any processing begins** and a bounded ring of seen message identifiers absorbs the
//! redeliveries that happen anyway across a reconnect.

use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use dekopon_agent::{CancelVia, attachment::GeneratedImage};
use dekopon_broker_protocol::ChatTransportKind;
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
        AckToken, AssetFetcher, CancelButton, CancelPress, CancelRequest, ChatDriver,
        ChatTransport, ConversationKind, InboundMessage, InboundReaction, LivenessTarget,
        MessageRef, NativeStatus, OutboundReply, ProgressLimits, ProgressMessage, ReplyTarget,
        SeenIds, Status, StreamLimits, StreamedText, TextStream, ThreadClaim, ThreadContinuation,
        ThreadOwnership, TransportError, TransportEvent, TransportIdentity, bound_inbound,
        credential_client, floor_boundary, receive_span, reconnect_delay,
    },
};

/// Redeliveries this transport remembers across reconnects.
const DEDUP_CAPACITY: usize = 1024;
/// Freshly authorized sender/thread claims retained by one Agent transport.
///
/// Bounded independently from conversation history: this registry decides only whether a message
/// may wake a session, never what the session remembers or what the broker authorizes.
const OWNED_THREAD_CAPACITY: usize = 1024;
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
/// The general deadline every Web API call and file transfer this transport makes shares.
const SLACK_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// A liveness call must never inherit the final reply/file client's general 30-second wait.
const LIVENESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a socket may say nothing before it is treated as dead.
///
/// Slack pings a healthy Socket Mode connection about every 30 seconds and never lets one go quiet
/// for this long, so silence past the deadline is a path that has gone away without TCP saying so —
/// a NAT table dropping the flow, or a partition with no RST. Without it `socket.next()` waits
/// forever and the workspace goes silent with no log line, no failed request, and nothing for a
/// probe to see. The same deadline bounds opening a socket, because a connection that negotiates
/// but never greets is the same wedge one round earlier.
const LIVENESS_DEADLINE: Duration = Duration::from_secs(90);
/// Fixed gateway-owned reaction used by classic/free-workspace fallback.
const LIVENESS_REACTION: &str = "tangerine";
/// Slack's ceiling on the text of one `chat.update`.
const PROGRESS_MAX_CHARS: usize = 4_000;
/// Slack's documented floor between two edits of the same message.
const PROGRESS_MIN_EDIT_INTERVAL: Duration = Duration::from_secs(3);
/// How often a stream may take an append; Slack renders the message itself between them.
const STREAM_MIN_INTERVAL: Duration = Duration::from_secs(1);
/// Ceiling on the `markdown_text` of one streaming call.
const STREAM_MAX_CHARS: usize = 12_000;
/// Longest 429 wait this transport will honor before it stops suppressing progress calls.
const MAX_PROGRESS_COOLDOWN: Duration = Duration::from_secs(60);
/// Wait taken when a 429 names none, matching the reply path's own floor.
const DEFAULT_PROGRESS_COOLDOWN: Duration = Duration::from_secs(5);
/// Fixed `action_id` of this gateway's own Stop button, and the only one a press may carry.
const CANCEL_ACTION_ID: &str = "dekopon-cancel";
/// Inbound messages whose reaction ownership this transport remembers at once.
const MAX_TRACKED_REACTIONS: usize = 256;
/// Open streams whose appended length this transport remembers at once.
const MAX_TRACKED_STREAMS: usize = 64;
/// Fixed marker a stream carries once the policy has cut the answer to [`STREAM_MAX_CHARS`].
///
/// Only the streaming view wears it: `chat.stopStream` closes the message with the answer the reply
/// path bounds, so the ellipsis says there is more text than fits while the stream is open and is
/// gone from the finished message.
const STREAM_TRUNCATION_MARKER: &str = "…";

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
    seen: SeenIds,
    pending: VecDeque<TransportEvent>,
    failures: u32,
    experience: SlackExperience,
    deadline: Duration,
    thread_ownership: Arc<SlackThreadOwnership>,
}

impl SlackTransport {
    /// Takes credential *values*, which the caller has already resolved from named environment
    /// variables. Keeping `std::env` out of the transport is what lets a test construct one.
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
            }),
            socket: None,
            identity: TransportIdentity::default(),
            team_id: None,
            seen: SeenIds::new(DEDUP_CAPACITY),
            pending: VecDeque::new(),
            failures: 0,
            experience,
            deadline: LIVENESS_DEADLINE,
            thread_ownership,
        })
    }

    /// Shortens the liveness deadline so a test can prove a wedged socket is abandoned.
    #[cfg(test)]
    pub(crate) fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
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
        // The handshake and the greeting share one deadline. Neither has one of its own, so a URL
        // that accepts a connection and then stops — or never completes TLS at all — would park
        // this transport in `connect` or in its reconnect loop with nothing to observe.
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
        self.failures = 0;
        Ok(())
    }

    /// Reads one frame, acknowledging an events envelope before anything else happens to it.
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
            // Reported as closed so the reconnect this transport already owns picks it up. The
            // socket is dropped by `next`, which is what stops a half-open connection from
            // holding the only path three workspaces have to this daemon.
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

        // One span per envelope, opened before the acknowledgment and before the payload is read,
        // so the ack, the routing decision, and everything the message goes on to cause share one
        // trace. `hello` and `disconnect` carry no envelope: they acknowledge nothing and route
        // nothing, so they open no trace.
        let received = match frame["envelope_id"].as_str() {
            Some(envelope) => {
                let received = receive_span(ChatTransportKind::Slack);
                // Before parsing, before routing, before any model call. Slack resends in about
                // three seconds and a session runs for far longer, so acknowledging afterwards
                // guarantees duplicates rather than merely risking them.
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

        // A button press arrives on its own envelope kind. The acknowledgment above is the whole
        // of what Slack's three-second interactive deadline asks for, and it has already been sent,
        // so the press is routed from here rather than from the synchronous event path.
        if frame["type"].as_str() == Some("interactive") {
            return self
                .cancel_pressed(&frame)
                .instrument(received.clone())
                .await;
        }
        received.in_scope(|| self.accept(&frame, &received))
    }

    /// Turns one acknowledged interactive envelope into an acknowledged, routable cancel request.
    ///
    /// [`CancelButton::ack`] is still the path a press takes even though Slack's acknowledgment is
    /// the envelope one [`Self::pump`] already sent: the reader keeps the same shape on every
    /// transport, and an experience whose driver offers no button routes no press at all.
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

    /// Reads one `block_actions` payload into the press to acknowledge and the request to route.
    ///
    /// `None` for any interactive payload that is not a press of this gateway's own Stop button:
    /// another app's action identifier, a button with no value, or a payload missing what a session
    /// is named by. The value is the conversation identity this transport minted when it posted the
    /// message, so the press names its session without the reader re-deriving that identity from a
    /// payload shaped unlike a message event.
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
        // The progress message is a reply inside the thread it reports on, so its own `thread_ts`
        // is that thread; a press on a top-level message names itself.
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

    /// Turns one acknowledged envelope into a pending event, inside its receive span.
    ///
    /// Split out of [`Self::pump`] only so the span wraps every branch of it: nothing here awaits,
    /// so `in_scope` is what enters it.
    fn accept(&mut self, frame: &Value, received: &Span) -> Result<(), TransportError> {
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
        if self.experience == SlackExperience::Agent
            && event["type"].as_str() == Some("agent_session_stopped")
        {
            if let Some(stopped) = self.session_stopped(&team, event)? {
                self.pending
                    .push_back(TransportEvent::CancelRequested(stopped));
            }
        } else if let Some(message) = self.routable(&team, event, received)? {
            received.record("message.id", message.message_id.as_str());
            self.pending
                .push_back(TransportEvent::Message(Box::new(message)));
        }
        Ok(())
    }

    /// Turns one Slack event into a routable message, or `None` when it is not ours to answer.
    fn routable(
        &mut self,
        team: &str,
        event: &Value,
        received: &Span,
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
        let thread_ts = event["thread_ts"].as_str().map(str::to_owned);
        let root_ts = thread_ts.clone().unwrap_or_else(|| ts.to_owned());
        let conversation = if event["channel_type"].as_str() == Some("im") {
            ConversationKind::DirectMessage
        } else {
            ConversationKind::Channel(channel.to_owned())
        };
        // `message.channels`/`message.groups` expose ambient traffic to an Agent installation so
        // an owned thread can continue without another mention. Drop everything else here, before
        // it reaches routing, authorization, payload telemetry, or a model. An app_mention event is
        // authenticated structured evidence; mention syntax is retained as a defensive fallback
        // because the parallel message event may win the dedup race.
        let explicitly_addressed =
            event["type"].as_str() == Some("app_mention") || self.identity.is_addressed(&text);
        let thread_continuation = match (&conversation, self.experience) {
            (ConversationKind::Channel(_), SlackExperience::Agent) => {
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
            (ConversationKind::Channel(_), SlackExperience::Classic) if !explicitly_addressed => {
                return Ok(None);
            }
            _ => None,
        };
        if !self.seen.insert(format!("{channel}:{ts}")) {
            return Ok(None);
        }
        // Agent sessions are thread-scoped even in DMs. Classic DMs deliberately retain today's
        // top-level reply and whole-DM conversation behavior; a cosmetic API result never decides
        // which model the installed app exposes.
        let is_channel = matches!(&conversation, ConversationKind::Channel(_));
        let reply_thread = match (&conversation, self.experience) {
            (ConversationKind::DirectMessage, SlackExperience::Classic) => None,
            (ConversationKind::DirectMessage | ConversationKind::Channel(_), _) => {
                Some(root_ts.clone())
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
        // this identity has to stand on its own once it leaves the transport. A classic direct
        // message has no thread to join and uses the DM channel; Agent mode intentionally uses the
        // root thread for one Slack session per task.
        let conversation_id = match &reply_thread {
            Some(thread) => format!("{channel}:{thread}"),
            None => channel.to_owned(),
        };

        Ok(Some(InboundMessage {
            transport: self.name.clone(),
            transport_kind: ChatTransportKind::Slack,
            subject: ExternalSubject::slack(team, user).map_err(TransportError::Subject)?,
            channel: channel.to_owned(),
            thread: match self.experience {
                SlackExperience::Agent => Some(root_ts.clone()),
                SlackExperience::Classic => thread_ts,
            },
            conversation_id,
            message_id: ts.to_owned(),
            text,
            assets,
            conversation,
            addressed: is_channel.then_some(explicitly_addressed),
            thread_continuation,
            reply: ReplyTarget::Slack {
                channel: channel.to_owned(),
                thread_ts: reply_thread,
            },
            // Always present: whether anything is shown is the policy's decision from the
            // transport's own `liveness` configuration, not a second gate in the reader. The thread
            // carries the Agent status and the progress message; the message timestamp carries the
            // reaction, which goes on the message being answered.
            liveness: Some(LivenessTarget::Slack {
                channel_id: channel.to_owned(),
                thread_ts: root_ts,
                message_ts: ts.to_owned(),
                initiator_user_id: user.to_owned(),
            }),
            receive_span: received.clone(),
        }))
    }

    fn session_stopped(
        &self,
        team: &str,
        event: &Value,
    ) -> Result<Option<CancelRequest>, TransportError> {
        // Slack's event reference currently names these `channel` and `user`. Accept the `_id`
        // spellings as authenticated-envelope aliases as well so an SDK/schema rollout cannot turn
        // the mandatory Stop control into a silently ignored event.
        let (Some(channel), Some(thread_ts), Some(user)) = (
            event["channel"]
                .as_str()
                .or_else(|| event["channel_id"].as_str()),
            event["thread_ts"].as_str(),
            event["user"].as_str().or_else(|| event["user_id"].as_str()),
        ) else {
            return Ok(None);
        };
        Ok(Some(CancelRequest {
            transport: self.name.clone(),
            conversation_id: format!("{channel}:{thread_ts}"),
            subject: ExternalSubject::slack(team, user)
                .map_err(TransportError::Subject)?
                .to_string(),
            via: CancelVia::NativeStop,
        }))
    }
}

/// Negotiates one Socket Mode connection and waits for Slack's `hello`.
///
/// Free rather than a method so the caller can put one deadline around the whole thing. Slack
/// always greets before it delivers, so waiting for it here means a socket that negotiated but is
/// not actually usable fails inside `open`, where the backoff lives, rather than looking like an
/// empty conversation.
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
            self.identity = TransportIdentity {
                user_id: Some(user_id),
                handle: None,
            };
            self.team_id = Some(team_id);
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
                if self.socket.is_none() {
                    tokio::time::sleep(reconnect_delay(self.failures)).await;
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

/// The bot-token half of a Slack transport.
pub(crate) struct SlackReplier {
    endpoint: String,
    bot_token: Redacted<String>,
    http: reqwest::Client,
    experience: SlackExperience,
    /// Whether `liveness.classicFallback` asked for the fixed reaction.
    classic_reaction: bool,
    /// Permanently disabled after Slack says this installation cannot use Agent sessions.
    agent_status_available: AtomicBool,
    /// Permanently disabled after Slack says this bot lacks reaction authority.
    reaction_available: AtomicBool,
    /// Inbound messages this generation marked, so cleanup never removes a pre-existing reaction.
    added_reactions: Mutex<Tracked<()>>,
    /// When Slack's last 429 says this app may make another progress call.
    ///
    /// Tier 3 is counted per app per workspace across every session, so one deadline shared by all
    /// of them — not a per-session budget — is what stops a throttled workspace being asked again.
    /// Nothing sleeps on it: a progress call is never retried, so a suppressed one fails and the
    /// policy renders nothing that tick.
    progress_cooldown_until: Mutex<Option<Instant>>,
    /// What this transport has already sent to each open stream.
    streams: Mutex<Tracked<StreamState>>,
}

#[async_trait]
impl ChatDriver for SlackReplier {
    async fn reply(
        &self,
        target: &ReplyTarget,
        reply: OutboundReply,
    ) -> Result<(), TransportError> {
        let ReplyTarget::Slack { channel, thread_ts } = target else {
            return Err(TransportError::Response);
        };
        let OutboundReply { text, images } = reply;
        if !images.is_empty() {
            return self
                .upload_attachments(channel.clone(), thread_ts.clone(), text, images)
                .await;
        }
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
            // Only an explicit 429 is retried, once; every other result uses the
            // existing response validation and failure path.
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

    /// Slack's Agent experience owns the spinner and the Stop control beside it; a classic app has
    /// no such state to set, which is what the reaction below stands in for.
    fn status(&self) -> Option<&dyn NativeStatus> {
        (self.experience == SlackExperience::Agent
            && self.agent_status_available.load(Ordering::Acquire))
        .then_some(self as &dyn NativeStatus)
    }

    /// Both experiences post, edit, and delete an ordinary message; only the classic one hangs a
    /// Stop button on it.
    fn progress(&self) -> Option<&dyn ProgressMessage> {
        Some(self)
    }

    /// `chat.startStream` works in a thread on both experiences.
    fn stream(&self) -> Option<&dyn TextStream> {
        Some(self)
    }

    /// The fallback, and only the fallback: whatever `liveness.classicFallback` asked for, offered
    /// to a classic app — which has no native status at all — and to an Agent installation only
    /// once Slack has permanently refused it one. That is the same condition `status()` answers on,
    /// so the two objects are never live at once and nothing puts `:tangerine:` beside a working
    /// native spinner.
    fn reaction(&self) -> Option<&dyn InboundReaction> {
        let without_native_status = self.experience == SlackExperience::Classic
            || !self.agent_status_available.load(Ordering::Acquire);
        (self.classic_reaction
            && without_native_status
            && self.reaction_available.load(Ordering::Acquire))
        .then_some(self as &dyn InboundReaction)
    }

    /// Classic apps only: the Agent experience renders Slack's own Stop control, and a second
    /// button beside it would be two ways to say one thing.
    fn cancel_button(&self) -> Option<&dyn CancelButton> {
        (self.experience == SlackExperience::Classic).then_some(self as &dyn CancelButton)
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
        // The initiator rides `processing` because Slack shows the spinner and the Stop control to
        // that person; clearing the status is about the session, not about who asked for it.
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
            // Removal requires a confirmed add by this generation. A lost response may leave a
            // harmless marker, but can never authorize removing a reaction the bot already had.
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

    /// Posts the progress message as a reply in the thread it reports on, never in the channel.
    ///
    /// Always threaded, including in a classic direct message, where today's answer is posted at
    /// the top level: a progress message that is finalized in place puts that answer in the thread
    /// under the question instead. That is the price of editing one message rather than posting
    /// two, and it is the same surface Slack's own streaming requires.
    async fn post(
        &self,
        target: &LivenessTarget,
        text: &ProgressText,
        cancel: bool,
    ) -> Result<MessageRef, TransportError> {
        let LivenessTarget::Slack {
            channel_id,
            thread_ts,
            ..
        } = target
        else {
            return Err(TransportError::Response);
        };
        let body = json!({
            "channel": channel_id,
            "thread_ts": thread_ts,
            "text": text.as_str(),
            "blocks": self.progress_blocks(text.as_str(), cancel, channel_id, thread_ts),
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
            thread_ts,
            ..
        } = &message.target
        else {
            return Err(TransportError::Response);
        };
        let body = json!({
            "channel": channel_id,
            "ts": message.id,
            "text": text.as_str(),
            "blocks": self.progress_blocks(text.as_str(), cancel, channel_id, thread_ts),
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

    /// Rewrites the progress message as the answer, which also drops the Stop button with it.
    async fn finalize(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError> {
        let LivenessTarget::Slack { channel_id, .. } = &message.target else {
            return Err(TransportError::Response);
        };
        if !reply.images.is_empty() {
            // Attachments enter a Slack conversation through the upload flow, which posts a message
            // of its own: an answer carrying one cannot become this message. Refusing here is what
            // makes the policy delete this message and reply, so the text and the images arrive
            // together.
            return Err(TransportError::Service {
                code: "answer-has-attachments".to_owned(),
            });
        }
        if reply.text.chars().count() > PROGRESS_MAX_CHARS {
            // An answer past `chat.update`'s ceiling would be refused by Slack after a round trip
            // and silently truncated by nothing: the reply path is what delivers it whole.
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

    /// Opens the stream on the first call and appends only what is new on every later one.
    ///
    /// The one thing this adds to model text is [`STREAM_TRUNCATION_MARKER`], on the show where the
    /// policy first says it cut the answer to fit.
    ///
    /// The Stop affordance on a stream is Slack's own: the Agent experience renders it beside the
    /// streamed message, and a classic stream takes no Block Kit elements at all, so `cancel` has
    /// nothing to add here. A classic session that streams is stopped by a stop word.
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
            // `chat.startStream` names the thread and the person waiting: Slack renders the stream
            // as a reply in that thread, with its own Stop control for them.
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
        // The policy hands cumulative text that grows by appending and Slack takes the new part
        // only. Anything else — a shorter text, an offset that is no longer a character boundary —
        // is a turn's text replaced rather than extended, and is appended whole rather than
        // silently dropped.
        let delta = if appended <= whole.len() && whole.is_char_boundary(appended) {
            &whole[appended..]
        } else {
            whole
        };
        // The marker is the one thing this driver adds to model text, and only the first cut show
        // adds it: past the ceiling the policy hands the same bounded prefix every tick, and a
        // stream that repeated the ellipsis would read as text arriving when none is.
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

    /// Closes the stream with the whole answer, which is what leaves it on screen as the reply.
    async fn finalize(
        &self,
        message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError> {
        let LivenessTarget::Slack { channel_id, .. } = &message.target else {
            return Err(TransportError::Response);
        };
        if !reply.images.is_empty() {
            // As for the progress message: an upload posts a message of its own, so the policy
            // deletes this one and replies rather than closing the stream with half the answer.
            // Length needs no check here — `MAX_OUTBOUND_TEXT_BYTES` is well inside `stopStream`'s
            // own ceiling, so the only answer this refuses is one carrying an attachment.
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
        // Closed either way: Slack keeps a stream it refused to stop open for its own timeout, and
        // this transport has nothing left to append to it.
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
    /// Nothing is sent: Socket Mode acknowledges a press with the envelope identifier, and the
    /// reader sends that acknowledgment before it parses the payload — well inside Slack's
    /// three-second deadline. There is no interaction response to post.
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
    /// Uses Slack's current external-upload flow, once per attachment.
    ///
    /// The service-selected upload URL receives only image bytes; the bot token returns to the fixed
    /// Web API origin for completion. The answer text rides the first upload's `initial_comment`, so
    /// a reply with several attachments still posts one comment rather than repeating itself.
    async fn upload_attachments(
        &self,
        channel: String,
        thread_ts: Option<String>,
        text: String,
        images: Vec<GeneratedImage>,
    ) -> Result<(), TransportError> {
        let mut accepted = false;
        for (index, image) in images.into_iter().enumerate() {
            let comment = (index == 0)
                .then_some(text.as_str())
                .filter(|text| !text.is_empty());
            match self
                .upload_attachment(&channel, thread_ts.as_deref(), comment, image, index)
                .await
            {
                Ok(()) => accepted = true,
                // The first attachment is already in the conversation, so this is a reply that
                // arrived in part rather than one that never arrived.
                Err(_) if accepted => return Err(TransportError::PartialDelivery),
                Err(error) => return Err(error),
            }
        }
        accepted.then_some(()).ok_or(TransportError::Response)
    }

    /// Uploads exactly one attachment and completes it into the conversation.
    async fn upload_attachment(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        initial_comment: Option<&str>,
        image: GeneratedImage,
        index: usize,
    ) -> Result<(), TransportError> {
        let filename = image.filename(index);
        let length = image.bytes().len().to_string();
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
            .header("content-type", image.media_type())
            .body(image.into_bytes())
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
    /// Sets Slack's own Agent session status for one thread.
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

    /// Adds or removes this gateway's fixed marker on one inbound message.
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

    /// The progress message's blocks, with a Stop button only where a press has somewhere to go.
    ///
    /// The button's value is the conversation the press stops, so the reader names a session from
    /// the payload it is handed rather than re-deriving that identity from an interactive payload
    /// shaped unlike the message event the session was opened by.
    fn progress_blocks(
        &self,
        text: &str,
        cancel: bool,
        channel_id: &str,
        thread_ts: &str,
    ) -> Value {
        let mut blocks = vec![json!({ "type": "markdown", "text": text })];
        if cancel && self.experience == SlackExperience::Classic {
            blocks.push(json!({
                "type": "actions",
                "elements": [{
                    "type": "button",
                    "action_id": CANCEL_ACTION_ID,
                    "style": "danger",
                    "text": { "type": "plain_text", "text": "Stop" },
                    "value": self.conversation_id(channel_id, thread_ts),
                }],
            }));
        }
        Value::Array(blocks)
    }

    /// The conversation identity [`SlackTransport::routable`] minted for this thread.
    ///
    /// A classic direct message is answered at the top level and is identified by its DM channel
    /// alone; every other Slack conversation is `<channel>:<thread>`. Slack conversation identifiers
    /// carry their kind in the first character, so a `D` channel is that direct message. The unit
    /// test `the_stop_button_carries_the_conversation_routing_minted` pins the two rules together.
    fn conversation_id(&self, channel_id: &str, thread_ts: &str) -> String {
        if self.experience == SlackExperience::Classic && channel_id.starts_with(['D', 'd']) {
            return channel_id.to_owned();
        }
        format!("{channel_id}:{thread_ts}")
    }

    /// One progress-class Web API call: a short deadline, no retry, one shared 429 cooldown.
    ///
    /// Every transient call goes through here — status, reaction, progress message, stream — because
    /// Slack counts them against one app-wide, workspace-wide tier. A retry would spend the next
    /// session's allowance on a message nobody is waiting for, so a throttled call publishes the
    /// deadline Slack named and fails.
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
            let wait = retry_after(response.headers());
            *self
                .progress_cooldown_until
                .lock()
                .expect("Slack progress cooldown") = Some(Instant::now() + wait);
            return Err(TransportError::Service {
                code: "ratelimited".to_owned(),
            });
        }
        check_ok(response).await
    }

    /// How long Slack's last 429 still asks this app to leave progress calls alone, if at all.
    fn progress_cooldown(&self) -> Option<Duration> {
        let until = (*self
            .progress_cooldown_until
            .lock()
            .expect("Slack progress cooldown"))?;
        until.checked_duration_since(Instant::now())
    }
}

/// Reads Slack's `Retry-After` seconds, bounded, with the documented default when it names none.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Duration {
    let seconds = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(DEFAULT_PROGRESS_COOLDOWN, Duration::from_secs);
    seconds.min(MAX_PROGRESS_COOLDOWN)
}

/// The markdown one streaming call carries: the new text, plus the cut marker when this show is the
/// one that reached the ceiling.
fn streamed_markdown(text: &str, mark: bool) -> String {
    if mark {
        format!("{text}{STREAM_TRUNCATION_MARKER}")
    } else {
        text.to_owned()
    }
}

/// What one open stream has already been told.
#[derive(Clone, Copy, Debug, Default)]
struct StreamState {
    /// Bytes of cumulative text already appended, so an append carries only what is new.
    appended: usize,
    /// Whether this stream has already said its text was cut, so it says so once and not per tick.
    marked: bool,
}

/// A bounded, most-recently-touched registry of per-message state keyed by a service identifier.
///
/// Both of its uses outlive a single call and neither has a moment where every entry is certainly
/// finished: a reaction is cleared when its session ends, a stream when it is closed, and a session
/// that dies without either leaves one behind. The capacity is what bounds that, and evicting the
/// oldest entry is right for both — it forgets that this generation added a reaction (so the marker
/// stays rather than removing a person's) and that a stream had been appended to (so the next
/// append repeats visible text rather than dropping it).
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

    /// Records or replaces one key, evicting the least recently recorded past the capacity.
    fn insert(&mut self, key: String, value: T) {
        self.entries.retain(|(candidate, _)| candidate != &key);
        self.entries.push_back((key, value));
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    /// Removes one key, answering what it held so a caller can act only on state it recorded.
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

/// Bounded Agent-thread ownership fed only by freshly authorized sessions.
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

    /// Claims or refreshes one sender/thread and evicts the least recently authorized claim.
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
/// [`credential_client`] refuses redirects globally, which is the right default for an API call
/// carrying a bearer token — a redirect there would forward the credential to whatever host
/// answered.
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

/// Whether Slack's service-selected upload URL is safe to receive generated bytes.
///
/// No credential is attached either way. Origin binding still matters because generated chat
/// content should not be sent to an arbitrary host named by a malformed service response.
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
    use dekopon_core::Redacted;
    use dekopon_model::ModelText;
    use dekopon_test_support::OPENAI_CHAT_COMPLETIONS_TWO_DELTAS;
    use serde_json::{Value, json};
    use tracing::Span;

    use super::{
        CANCEL_ACTION_ID, MAX_TRACKED_REACTIONS, MAX_TRACKED_STREAMS, PROGRESS_MAX_CHARS,
        SLACK_REQUEST_TIMEOUT, SlackReplier, SlackTransport, Tracked,
    };
    use crate::{
        config::{LivenessSettings, SlackExperience, SlackLivenessFallback, TemplateOverrides},
        progress::{ProgressDetail, ProgressText, Templates},
        transport::{
            AckToken, CancelButton, CancelPress, ChatDriver, InboundReaction, LivenessTarget,
            MessageRef, NativeStatus, OutboundReply, ProgressMessage, Status, StreamedText,
            TextStream, TransportError, TransportEvent, credential_client,
        },
    };

    const TEAM: &str = "t0123abc";
    const CHANNEL: &str = "c0123abc";
    const DIRECT_CHANNEL: &str = "d0123abc";
    const USER: &str = "u9xyz";
    const INBOUND_TS: &str = "1700000000.000001";
    const POSTED_TS: &str = "1700000000.000100";
    const STREAM_TS: &str = "1700000000.000200";
    /// An endpoint nothing listens on: the tests that use it assert a request is never made.
    const UNREACHABLE: &str = "http://127.0.0.1:1";

    fn target() -> LivenessTarget {
        LivenessTarget::Slack {
            channel_id: CHANNEL.to_owned(),
            thread_ts: INBOUND_TS.to_owned(),
            message_ts: INBOUND_TS.to_owned(),
            initiator_user_id: USER.to_owned(),
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

    /// The two lines the policy's own renderer produces from the shipped defaults.
    ///
    /// Rendered rather than invented: [`ProgressText`] has no constructor a driver can reach, which
    /// is the whole point of the type, and a test asserting on a string of its own would be
    /// asserting about something that never goes out.
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

    /// The cumulative text of a recorded stream.
    ///
    /// Taken from a transcript through the model crate's own parser because that parser is the only
    /// thing that constructs [`ModelText`] from bytes — which is also what keeps this fixture from
    /// drifting away from what a backend really sends.
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

    /// Cumulative text the policy has cut to the stream's ceiling.
    fn cut(text: ModelText) -> StreamedText {
        StreamedText {
            text,
            truncated: true,
        }
    }

    /// The body of every call to `path`, in the order they were made.
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

    /// Answers every Web API call this transport makes with the fields it reads back.
    fn accepting(path: &str, body: &Value) -> (u16, Value) {
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

    /// A loopback Slack, recording the path and JSON body of every call in order.
    ///
    /// Hand-rolled and on a real socket: what these tests pin is the bytes that leave the process,
    /// which a hand-written client stub would assert about itself instead.
    struct SlackMock {
        base: String,
        calls: Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl SlackMock {
        fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().expect("mock call log").clone()
        }

        /// The body of the one call to `path`, which must have happened exactly once.
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
                    let body = serde_json::from_str::<Value>(&body)
                        .expect("every call this transport makes carries a JSON body");
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

    /// Reads one HTTP request's path and body, or `None` when the peer gave up on it.
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
            json!(format!("{CHANNEL}:{INBOUND_TS}")),
            "the button names the conversation a press stops"
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
                    "value": format!("{CHANNEL}:{INBOUND_TS}"),
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
        assert_eq!(request.conversation_id, format!("{CHANNEL}:{INBOUND_TS}"));
        assert_eq!(request.subject, format!("slack.{TEAM}.{USER}"));
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
            subject: format!("slack.{TEAM}.{USER}"),
            ack: AckToken::Local,
        };

        let error = CancelButton::ack(&replier, &press)
            .await
            .expect_err("a token minted by another transport is a routing mistake");

        assert!(matches!(&error, TransportError::Response), "{error:?}");
    }

    #[tokio::test]
    async fn the_stop_button_carries_the_conversation_routing_minted() {
        let mut transport = transport(UNREACHABLE, SlackExperience::Classic);
        let mention = json!({
            "type": "app_mention",
            "channel": CHANNEL,
            "channel_type": "channel",
            "user": USER,
            "ts": INBOUND_TS,
            "text": "<@b0123abc> how are things?",
        });
        let direct = json!({
            "type": "message",
            "channel": DIRECT_CHANNEL,
            "channel_type": "im",
            "user": USER,
            "ts": "1700000000.000002",
            "text": "how are things?",
        });

        let in_channel = transport
            .routable(TEAM, &mention, &Span::none())
            .expect("the mention is readable")
            .expect("the mention is routable");
        let in_direct_message = transport
            .routable(TEAM, &direct, &Span::none())
            .expect("the direct message is readable")
            .expect("the direct message is routable");

        // The identity in the button and the identity the session is registered under are one rule,
        // in one place: a press that named anything else would stop nothing.
        assert_eq!(
            in_channel.conversation_id,
            transport.replier.conversation_id(CHANNEL, INBOUND_TS)
        );
        assert_eq!(
            in_direct_message.conversation_id,
            transport
                .replier
                .conversation_id(DIRECT_CHANNEL, "1700000000.000002"),
            "a classic direct message is answered at the top level and named by its channel"
        );
        let blocks = transport
            .replier
            .progress_blocks("Working on it…", true, CHANNEL, INBOUND_TS);
        assert_eq!(
            blocks[1]["elements"][0]["value"],
            json!(in_channel.conversation_id)
        );
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
