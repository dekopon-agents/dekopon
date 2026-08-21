//! Slack Socket Mode: an outbound WebSocket instead of an inbound webhook.
//!
//! Socket Mode exists so a daemon behind NAT needs no public HTTP endpoint. The protocol's one
//! sharp edge is redelivery: Slack expects an acknowledgment within roughly three seconds and
//! resends the envelope otherwise. A Dekopon session takes far longer than that, so **the ack is
//! sent before any processing begins** and a bounded ring of seen message identifiers absorbs the
//! redeliveries that happen anyway across a reconnect.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use dekopon_broker_protocol::ChatTransportKind;
use dekopon_core::{ExternalSubject, Redacted};
use dekopon_model::image::GeneratedImage;
use futures_util::{SinkExt as _, StreamExt as _, future::BoxFuture};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

use crate::{
    asset::{AssetSourceRef, PendingAsset},
    config::{
        ActivityMode, SLACK_ENDPOINT, SlackActivityConfig, SlackActivityFallback, SlackExperience,
    },
    transport::{
        ActivityTarget, AssetFetcher, ChatActivity, ChatReplier, ChatTransport, ConversationKind,
        DeliveryReceipt, InboundMessage, OutboundReply, ReplyTarget, SessionStop, ThreadClaim,
        ThreadContinuation, ThreadOwnership, TransportError, TransportEvent, TransportIdentity,
        bound_inbound, floor_boundary,
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
/// Ceiling on reconnect backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// First reconnect delay; doubles up to [`MAX_BACKOFF`].
const BASE_BACKOFF: Duration = Duration::from_millis(500);
/// Activity must never inherit the final reply/file client's general 30-second wait.
const ACTIVITY_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Fixed gateway-owned reaction used by classic/free-workspace fallback.
const ACTIVITY_REACTION: &str = "tangerine";

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
    pending: VecDeque<TransportEvent>,
    failures: u32,
    experience: SlackExperience,
    activity: SlackActivityConfig,
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
        activity: SlackActivityConfig,
    ) -> Result<Self, TransportError> {
        let http = client()?;
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
                fallback: activity.classic_fallback,
                agent_status_available: AtomicBool::new(true),
                reaction_available: AtomicBool::new(true),
                active_activity: Mutex::new(HashMap::new()),
            }),
            socket: None,
            identity: TransportIdentity::default(),
            team_id: None,
            seen: Dedup::new(DEDUP_CAPACITY),
            pending: VecDeque::new(),
            failures: 0,
            experience,
            activity,
            thread_ownership,
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
        let frame =
            serde_json::from_str::<Value>(&frame).map_err(TransportError::MalformedResponse)?;

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
        if self.experience == SlackExperience::Agent
            && event["type"].as_str() == Some("agent_session_stopped")
        {
            if let Some(stopped) = self.session_stopped(&team, event)? {
                self.pending
                    .push_back(TransportEvent::SessionStopped(stopped));
            }
        } else if let Some(message) = self.routable(&team, event)? {
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
            activity: (self.activity.mode == ActivityMode::Native).then(|| ActivityTarget::Slack {
                channel_id: channel.to_owned(),
                thread_ts: root_ts,
                message_ts: ts.to_owned(),
                initiator_user_id: user.to_owned(),
            }),
        }))
    }

    fn session_stopped(
        &self,
        team: &str,
        event: &Value,
    ) -> Result<Option<SessionStop>, TransportError> {
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
        Ok(Some(SessionStop {
            transport: self.name.clone(),
            conversation_id: format!("{channel}:{thread_ts}"),
            subject: ExternalSubject::slack(team, user).map_err(TransportError::Subject)?,
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

    fn next(&mut self) -> BoxFuture<'_, Result<TransportEvent, TransportError>> {
        Box::pin(async move {
            loop {
                if let Some(event) = self.pending.pop_front() {
                    return Ok(event);
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

    fn activity(&self) -> Option<Arc<dyn ChatActivity>> {
        (self.activity.mode == ActivityMode::Native)
            .then(|| Arc::clone(&self.replier) as Arc<dyn ChatActivity>)
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
    fallback: SlackActivityFallback,
    /// Permanently disabled after Slack says this installation cannot use Agent sessions.
    agent_status_available: AtomicBool,
    /// Permanently disabled after Slack says this bot lacks reaction authority.
    reaction_available: AtomicBool,
    /// What this generation may have created, so cleanup never removes a pre-existing reaction.
    active_activity: Mutex<HashMap<ActivityTarget, SlackActivityAttempt>>,
}

#[derive(Clone, Copy, Debug, Default)]
struct SlackActivityAttempt {
    agent_status: bool,
    reaction: bool,
}

impl ChatReplier for SlackReplier {
    fn reply(
        &self,
        target: ReplyTarget,
        reply: OutboundReply,
    ) -> BoxFuture<'_, Result<DeliveryReceipt, TransportError>> {
        Box::pin(async move {
            let ReplyTarget::Slack { channel, thread_ts } = target else {
                return Err(TransportError::Response);
            };
            let OutboundReply { text, image } = reply;
            if let Some(image) = image {
                return self
                    .upload_generated_image(channel, thread_ts, text, image)
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
            let expected_channel = channel.clone();
            let mut body = json!({
                "channel": channel,
                "text": text,
                "blocks": [{ "type": "markdown", "text": text }],
            });
            if let Some(thread_ts) = thread_ts {
                body["thread_ts"] = Value::String(thread_ts);
            }
            #[allow(
                clippy::map_err_ignore,
                reason = "serializing a serde_json::Value cannot fail: it holds no non-string map \
                          keys and serde_json::Number rejects non-finite floats"
            )]
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
            let body = check_ok(response).await?;
            let response_channel = body["channel"].as_str().ok_or(TransportError::Response)?;
            let timestamp = body["ts"].as_str().ok_or(TransportError::Response)?;
            if response_channel != expected_channel || !canonical_timestamp(timestamp) {
                return Err(TransportError::Response);
            }
            Ok(DeliveryReceipt::new(format!(
                "{}:{timestamp}",
                response_channel.to_ascii_lowercase()
            )))
        })
    }
}

impl SlackReplier {
    /// Uses Slack's current external-upload flow. The service-selected upload URL receives only
    /// image bytes; the bot token returns to the fixed Web API origin for completion.
    async fn upload_generated_image(
        &self,
        channel: String,
        thread_ts: Option<String>,
        text: String,
        image: GeneratedImage,
    ) -> Result<DeliveryReceipt, TransportError> {
        let length = image.bytes().len().to_string();
        let described = check_ok(
            self.http
                .post(format!("{}/api/files.getUploadURLExternal", self.endpoint))
                .header(
                    "authorization",
                    format!("Bearer {}", self.bot_token.expose()),
                )
                .form(&[("filename", image.filename()), ("length", length.as_str())])
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
            "files": [{"id": file_id, "title": "Generated image"}],
            "channel_id": channel,
        });
        if !text.is_empty() {
            body["initial_comment"] = Value::String(text);
        }
        if let Some(thread_ts) = thread_ts {
            body["thread_ts"] = Value::String(thread_ts);
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
        Ok(DeliveryReceipt::new(format!("slack-file:{file_id}")))
    }
}

impl ChatActivity for SlackReplier {
    fn show(&self, target: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            let ActivityTarget::Slack {
                channel_id,
                thread_ts,
                message_ts,
                initiator_user_id,
            } = &target
            else {
                return Err(TransportError::Response);
            };

            let mut agent_error = None;
            if self.experience == SlackExperience::Agent
                && self.agent_status_available.load(Ordering::Acquire)
            {
                self.update_attempt(&target, |attempt| attempt.agent_status = true);
                match self
                    .set_agent_status(channel_id, thread_ts, "processing", Some(initiator_user_id))
                    .await
                {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        if permanent_agent_error(&error) {
                            self.update_attempt(&target, |attempt| attempt.agent_status = false);
                            if self.agent_status_available.swap(false, Ordering::AcqRel) {
                                tracing::warn!(
                                    event = "gateway_activity_degraded",
                                    transport = "slack",
                                    surface = "agent-status"
                                );
                            }
                        }
                        agent_error = Some(error);
                    }
                }
            }

            if self.fallback == SlackActivityFallback::Reaction
                && self.reaction_available.load(Ordering::Acquire)
            {
                match self
                    .set_reaction(channel_id, message_ts, "reactions.add")
                    .await
                {
                    Ok(()) => {
                        // Cleanup ownership requires a confirmed successful add. A lost response
                        // may leave a harmless marker, but can never authorize this generation to
                        // remove a reaction the bot already had.
                        self.update_attempt(&target, |attempt| attempt.reaction = true);
                        return Ok(());
                    }
                    Err(TransportError::Service { code }) if code == "already_reacted" => {
                        return Ok(());
                    }
                    Err(error) => {
                        if permanent_reaction_error(&error)
                            && self.reaction_available.swap(false, Ordering::AcqRel)
                        {
                            tracing::warn!(
                                event = "gateway_activity_degraded",
                                transport = "slack",
                                surface = "reaction"
                            );
                        }
                        return Err(error);
                    }
                }
            }

            match agent_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }

    fn hide(&self, target: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            let ActivityTarget::Slack {
                channel_id,
                thread_ts,
                message_ts,
                ..
            } = &target
            else {
                return Err(TransportError::Response);
            };
            let attempt = self
                .active_activity
                .lock()
                .expect("Slack activity registry")
                .remove(&target)
                .unwrap_or_default();
            let mut first_error = None;
            if attempt.agent_status
                && let Err(error) = self
                    .set_agent_status(channel_id, thread_ts, "active", None)
                    .await
            {
                first_error = Some(error);
            }
            if attempt.reaction
                && let Err(error) = self
                    .set_reaction(channel_id, message_ts, "reactions.remove")
                    .await
                && !matches!(&error, TransportError::Service { code } if code == "no_reaction")
                && first_error.is_none()
            {
                first_error = Some(error);
            }
            match first_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }

    fn refresh_interval(&self) -> Option<Duration> {
        None
    }
}

impl SlackReplier {
    fn update_attempt(
        &self,
        target: &ActivityTarget,
        update: impl FnOnce(&mut SlackActivityAttempt),
    ) {
        let mut active = self
            .active_activity
            .lock()
            .expect("Slack activity registry");
        update(active.entry(target.clone()).or_default());
    }

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
        self.post_activity_json("agents.sessions.setStatus", &body)
            .await
    }

    async fn set_reaction(
        &self,
        channel: &str,
        timestamp: &str,
        method: &str,
    ) -> Result<(), TransportError> {
        self.post_activity_json(
            method,
            &json!({
                "channel": channel,
                "timestamp": timestamp,
                "name": ACTIVITY_REACTION,
            }),
        )
        .await
    }

    async fn post_activity_json(&self, method: &str, body: &Value) -> Result<(), TransportError> {
        #[allow(
            clippy::map_err_ignore,
            reason = "serializing a serde_json::Value cannot fail: it holds no non-string map keys \
                      and serde_json::Number rejects non-finite floats"
        )]
        let response = self
            .http
            .post(format!("{}/api/{method}", self.endpoint))
            .header(
                "authorization",
                format!("Bearer {}", self.bot_token.expose()),
            )
            .header("content-type", "application/json; charset=utf-8")
            .body(serde_json::to_vec(body).map_err(|_| TransportError::Response)?)
            .timeout(ACTIVITY_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        check_ok(response).await.map(|_| ())
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
