//! Strict, versioned, owner-controlled gateway configuration.
//!
//! Every hygiene check `dekopon-brokerd` applies to its own configuration applies here for the
//! same reason: this file names the agents a chat message may reach and the environment variables
//! that hold chat and model credentials, so a world-writable or symlinked copy of it is a way to
//! redirect the daemon rather than a cosmetic problem.
//!
//! Secrets themselves are deliberately absent. Transports and models name *environment variables*,
//! never values, following the precedent `dekopon-telemetry` set for OTLP ingest credentials.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use dekopon_agent::prompt::HistoryLimits;
use dekopon_broker_protocol::{
    BrokerSocketDiscovery, DEFAULT_IO_TIMEOUT, DEFAULT_MAX_FRAME_BYTES, FrameLimits, ProtocolError,
    ResolvedBrokerSocket,
};
use dekopon_core::{AgentId, CapabilityId, FileHygieneError, FileTier, read_trusted_file};
use dekopon_telemetry::{ExporterSettings, TelemetryError, Transport};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    progress::{KeepAlive, ProgressDetail, Templates},
    session::{FAILURE_REPLY, STOPPED_REPLY},
};

/// Exact configuration schema this daemon accepts.
pub const CONFIG_API_VERSION: &str = "dekopon.dev/dekopond/v1alpha1";
/// Hard ceiling on the configuration file, read before any allocation.
pub const HARD_MAX_CONFIG_BYTES: usize = 1024 * 1024;
/// Default concurrent sessions across every transport.
pub const DEFAULT_MAX_CONCURRENT_SESSIONS: usize = 4;
/// Default model turns one routed message may drive.
pub const DEFAULT_MAX_STEPS: u32 = 8;
/// Default capability invocations one routed message may drive.
pub const DEFAULT_MAX_CAPABILITY_CALLS: u32 = 16;
/// Default grace given to in-flight sessions at shutdown.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(120);
/// Default life of an untouched persistent conversation.
///
/// Fifteen minutes resolves toward the person rather than an undocumented provider-cache lifetime.
/// A bot that forgot after a brief lull is the failure people report; the cost control is the
/// window below, not a best-effort cache. See `docs/inference.md`.
pub const DEFAULT_CONVERSATION_IDLE_TIMEOUT: Duration = Duration::from_secs(900);
/// Default exchanges a persistent route replays into the next prompt.
pub const DEFAULT_CONVERSATION_MAX_TURNS: usize = 12;
/// Default bytes of replayed conversation a persistent route carries.
pub const DEFAULT_CONVERSATION_MAX_BYTES: usize = 64 * 1024;
/// Default conversations this process tracks at once.
pub const DEFAULT_MAX_CONVERSATIONS: usize = 1024;
/// What a person types to stop a running session when an operator names no list.
///
/// Two words rather than one because a person who wants a run to stop tries the obvious thing and
/// then the other obvious thing, and neither should be answered by the agent instead.
pub const DEFAULT_STOP_WORDS: [&str; 2] = ["stop", "cancel"];
/// The only non-loopback Slack origin this daemon will talk to.
pub const SLACK_ENDPOINT: &str = "https://slack.com";
/// The only non-loopback Discord REST origin this daemon will talk to.
pub const DISCORD_ENDPOINT: &str = "https://discord.com";
/// The only non-loopback Telegram origin this daemon will talk to.
pub const TELEGRAM_ENDPOINT: &str = "https://api.telegram.org";
/// The only non-loopback Meta Graph API origin this daemon will send WhatsApp replies to.
pub const WHATSAPP_GRAPH_ENDPOINT: &str = "https://graph.facebook.com";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum ConfigApiVersion {
    #[serde(rename = "dekopon.dev/dekopond/v1alpha1")]
    V1Alpha1,
}

/// Whether a transport publishes native in-flight liveness while an authorized session runs.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum LivenessMode {
    /// Preserve the transport's reply-only behavior: nothing is typed, posted, edited, or reacted.
    #[default]
    Off,
    /// Use whatever the service natively offers, with transport-specific fallback where configured.
    Native,
}

/// Which Slack conversation model the installed app exposes.
///
/// This is explicit because Agent mode changes DM threading and conversation identity. A failed
/// cosmetic status call must never switch those semantics underneath a live conversation.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SlackExperience {
    /// Conventional App Home messages and channel mentions.
    #[default]
    Classic,
    /// Slack's paid/admin-gated Agent messaging experience and thread-scoped sessions.
    Agent,
}

/// Visible fallback when Slack's Agent session status is unavailable.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SlackLivenessFallback {
    /// Degrade to the final reply only.
    #[default]
    None,
    /// Add and later remove Dekopon's fixed `:tangerine:` reaction.
    Reaction,
}

/// Whether a transport posts one editable message saying what the session is doing.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum ProgressSurface {
    /// Typing, status, and the reaction only; nothing is posted.
    #[default]
    Off,
    /// One message, posted late and edited in place, that becomes the answer at the end.
    Message,
}

/// When a running session says it is still alive.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KeepAliveConfig {
    /// Offsets from the moment the session started, in seconds.
    #[serde(default = "default_keep_alive_at")]
    pub at_seconds: Vec<u64>,
    /// Period between ticks once those offsets are spent.
    #[serde(default = "default_keep_alive_every")]
    pub every_seconds: u64,
    /// Ticks one session may write, because a run that never ends must not write forever.
    #[serde(default = "default_keep_alive_max")]
    pub max: u32,
}

impl Default for KeepAliveConfig {
    fn default() -> Self {
        Self {
            at_seconds: default_keep_alive_at(),
            every_seconds: default_keep_alive_every(),
            max: default_keep_alive_max(),
        }
    }
}

fn default_keep_alive_at() -> Vec<u64> {
    crate::progress::DEFAULT_KEEP_ALIVE_AT.to_vec()
}

const fn default_keep_alive_every() -> u64 {
    crate::progress::DEFAULT_KEEP_ALIVE_EVERY
}

const fn default_keep_alive_max() -> u32 {
    crate::progress::DEFAULT_KEEP_ALIVE_MAX
}

/// What this daemon shows on one transport while a session runs.
///
/// One block for every transport, because the surfaces are one vocabulary and the differences
/// between services are which of them a driver implements. A setting a transport cannot honor is a
/// startup refusal naming it rather than a field that silently does nothing.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LivenessConfig {
    /// Whether anything at all is published while an authorized session runs.
    #[serde(default)]
    pub mode: LivenessMode,
    /// Used by classic Slack apps and when Agent status is unavailable for that installation.
    #[serde(default)]
    pub classic_fallback: SlackLivenessFallback,
    /// Whether one editable progress message is posted.
    #[serde(default)]
    pub progress: ProgressSurface,
    /// Whether the model's answer is streamed into the surface as it is written.
    #[serde(default)]
    pub stream: bool,
    /// Whether the surface carries the service's own stop control.
    #[serde(default)]
    pub cancel_button: bool,
    #[serde(default)]
    pub keep_alive: KeepAliveConfig,
    /// Operator wording; an absent field keeps this daemon's own sentence.
    #[serde(default)]
    pub templates: TemplateOverrides,
}

/// The authored `templates:` block.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TemplateOverrides {
    pub working: Option<String>,
    pub tool: Option<String>,
    pub keep_alive: Option<String>,
    pub stopped: Option<String>,
    pub failed: Option<String>,
}

/// The half of one transport's liveness block a driver needs, cheap enough to copy per session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LivenessSettings {
    pub mode: LivenessMode,
    pub classic_fallback: SlackLivenessFallback,
    pub progress: ProgressSurface,
    pub stream: bool,
    pub cancel_button: bool,
}

impl LivenessConfig {
    /// The driver-facing settings, without the policy's own timing and wording.
    #[must_use]
    pub const fn settings(&self) -> LivenessSettings {
        LivenessSettings {
            mode: self.mode,
            classic_fallback: self.classic_fallback,
            progress: self.progress,
            stream: self.stream,
            cancel_button: self.cancel_button,
        }
    }
}

/// One transport's liveness settings after validation.
#[derive(Debug)]
pub(crate) struct ResolvedLiveness {
    pub settings: LivenessSettings,
    pub keep_alive: KeepAlive,
    pub templates: Templates,
}

impl Default for ResolvedLiveness {
    /// Reply-only, with this daemon's own sentences.
    ///
    /// Reached only by a session whose transport name is not in the resolved map, which startup
    /// makes unreachable by building one entry per configured transport; it is the shape that
    /// cannot show anything rather than a second set of defaults, so a future path that lost the
    /// lookup degrades to today's behavior instead of inventing one.
    fn default() -> Self {
        // The shipped templates carry only placeholders their own fields render, so the problem
        // list is empty by construction; a configuration's overrides are what validation is for.
        let (templates, _) =
            Templates::resolve(&TemplateOverrides::default(), STOPPED_REPLY, FAILURE_REPLY);
        Self {
            settings: LivenessSettings::default(),
            keep_alive: KeepAlive::default(),
            templates,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DekopondConfig {
    pub api_version: ConfigApiVersion,
    /// The `dekopon-config` catalog whose agents routes may name.
    pub catalog_path: PathBuf,
    #[serde(default)]
    pub broker: BrokerConfig,
    pub transports: Vec<TransportConfig>,
    pub models: Vec<ModelConfig>,
    pub routes: Vec<RouteConfig>,
    /// What a person types to stop the session running in their conversation.
    ///
    /// An operator list rather than a fixed one: the people talking to a deployment do not all
    /// speak English, and a word that means "stop" to them is the one that has to work. Matched
    /// exactly, case-insensitively, after the bot mention and trailing punctuation are stripped,
    /// so a stop word never swallows a sentence that merely contains it.
    #[serde(default)]
    pub stop_words: Option<Vec<String>>,
    #[serde(default)]
    pub sessions: SessionsConfig,
    /// Grace given to in-flight sessions before they are aborted at shutdown.
    #[serde(default)]
    pub shutdown_grace_ms: Option<u64>,
    /// Optional OTLP export. Absent means the daemon exports no telemetry.
    #[serde(default)]
    pub telemetry: Option<TelemetryConfig>,
}

/// How to reach `dekopon-brokerd`. Every field defaults to the documented discovery behavior.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrokerConfig {
    #[serde(default)]
    pub socket_path: Option<PathBuf>,
    #[serde(default)]
    pub server_uid: Option<u32>,
    #[serde(default)]
    pub max_frame_bytes: Option<usize>,
    #[serde(default)]
    pub io_timeout_ms: Option<u64>,
}

/// One chat service this daemon waits on.
///
/// Internally tagged on `kind` so a transport reads as one flat block, and strict on both halves:
/// an unknown `kind` and an unknown field inside a known one are both decode failures.
#[derive(Clone, Debug, Deserialize)]
#[serde(
    tag = "kind",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TransportConfig {
    /// Slack Socket Mode: an app-level token opens a WebSocket, a bot token answers.
    SlackSocketMode {
        name: String,
        app_token_env: String,
        bot_token_env: String,
        /// Conversation and lifecycle model configured on the installed Slack app.
        #[serde(default)]
        experience: SlackExperience,
        /// What a running session shows, and its explicit classic/free-workspace fallback.
        #[serde(default)]
        liveness: LivenessConfig,
        /// Overridable only to `https://slack.com` or a literal loopback HTTP URL, for tests.
        #[serde(default)]
        endpoint: Option<String>,
    },
    /// Discord Gateway: an outbound WebSocket carries messages and REST posts answers.
    DiscordGateway {
        name: String,
        bot_token_env: String,
        /// What a running session shows on this transport.
        #[serde(default)]
        liveness: LivenessConfig,
        /// Overridable only to `https://discord.com` or a literal loopback HTTP URL.
        #[serde(default)]
        endpoint: Option<String>,
    },
    /// Meta WhatsApp Cloud API: a signed public webhook receives text and Graph API sends replies.
    WhatsappCloudApi {
        name: String,
        app_secret_env: String,
        verify_token_env: String,
        access_token_env: String,
        /// Plain HTTP listener behind operator-owned TLS termination.
        bind: SocketAddr,
        callback_path: String,
        waba_id: String,
        phone_number_id: String,
        graph_api_version: String,
        /// What a running session shows; WhatsApp has typing and nothing else.
        #[serde(default)]
        liveness: LivenessConfig,
        /// Overridable only to the pinned production origin or literal loopback HTTP for tests.
        #[serde(default)]
        graph_endpoint: Option<String>,
    },
    /// Telegram long polling: the poll is the wakeup and advancing the offset is the ack.
    TelegramLongPoll {
        name: String,
        bot_token_env: String,
        /// What a running session shows on this transport.
        #[serde(default)]
        liveness: LivenessConfig,
        /// Overridable only to `https://api.telegram.org` or a literal loopback HTTP URL.
        #[serde(default)]
        endpoint: Option<String>,
    },
    /// A development transport on an owner-only Unix socket that trusts its local caller.
    ///
    /// The reference driver: it implements every surface, which is what lets the integration tests
    /// read a whole session's progress off one line stream.
    Local {
        name: String,
        socket_path: PathBuf,
        #[serde(default)]
        liveness: LivenessConfig,
    },
}

impl TransportConfig {
    /// The operator-chosen name routes refer to.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::SlackSocketMode { name, .. }
            | Self::DiscordGateway { name, .. }
            | Self::WhatsappCloudApi { name, .. }
            | Self::TelegramLongPoll { name, .. }
            | Self::Local { name, .. } => name,
        }
    }

    /// Stable low-cardinality label for lifecycle logs.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::SlackSocketMode { .. } => "slackSocketMode",
            Self::DiscordGateway { .. } => "discordGateway",
            Self::WhatsappCloudApi { .. } => "whatsappCloudApi",
            Self::TelegramLongPoll { .. } => "telegramLongPoll",
            Self::Local { .. } => "local",
        }
    }
}

/// One model endpoint a route may select.
#[derive(Clone, Debug, Deserialize)]
#[serde(
    tag = "kind",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ModelConfig {
    /// Any OpenAI-compatible chat-completions endpoint.
    OpenaiCompatible {
        name: String,
        endpoint: String,
        model: String,
        #[serde(default)]
        api_key_env: Option<String>,
        timeout_ms: u64,
        /// Whether this endpoint is asked to stream its answer.
        ///
        /// On by default, because streaming is what puts the answer on screen while it is being
        /// written and what lets a stop interrupt a turn instead of waiting out `timeoutMs`. The
        /// field exists for the endpoint that claims chat-completions compatibility and gets
        /// `stream: true` wrong — a proxy that buffers the whole SSE body, a server that drops
        /// `usage` — where the repair is one line of configuration rather than a second client.
        /// `kind: chatgptSubscription` has no such field: that backend streams and cannot be
        /// asked not to, so writing one there is the unknown-field refusal every other typo gets.
        #[serde(default = "default_model_stream")]
        stream: bool,
        /// Model classes this endpoint satisfies, matched against an agent's `modelClass`.
        #[serde(default)]
        classes: Vec<String>,
        /// What this endpoint can be shown besides text.
        ///
        /// Defaults to nothing. An OpenAI-compatible endpoint is very often a small local model
        /// that will either error or hallucinate when handed an image, and the default has to be
        /// the one that is safe on the endpoint an operator did not think about.
        #[serde(default)]
        modalities: Vec<Modality>,
    },
    /// OpenAI's Codex Responses endpoint using Dekopon's own device-flow credential file.
    ChatgptSubscription {
        name: String,
        model: String,
        #[serde(default)]
        auth_file: Option<PathBuf>,
        timeout_ms: u64,
        #[serde(default)]
        classes: Vec<String>,
        /// What this endpoint can be shown besides text.
        ///
        /// Still opt-in rather than assumed. Every current Codex model reads images, but a
        /// configuration that silently gained a capability when a default changed underneath it is
        /// the thing this file's strict decoding exists to prevent.
        #[serde(default)]
        modalities: Vec<Modality>,
    },
}

/// Streaming is the default: an endpoint that cannot do it is the exception an operator names.
const fn default_model_stream() -> bool {
    true
}

/// Something a model can be shown that is not text.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Modality {
    /// The model accepts images as message content.
    Image,
}

impl ModelConfig {
    /// The operator-chosen name routes refer to.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::OpenaiCompatible { name, .. } | Self::ChatgptSubscription { name, .. } => name,
        }
    }

    /// Whether this endpoint may be shown an image.
    #[must_use]
    pub fn accepts_images(&self) -> bool {
        match self {
            Self::OpenaiCompatible { modalities, .. }
            | Self::ChatgptSubscription { modalities, .. } => modalities.contains(&Modality::Image),
        }
    }

    /// The classes this endpoint declares it can serve.
    #[must_use]
    pub fn classes(&self) -> &[String] {
        match self {
            Self::OpenaiCompatible { classes, .. } | Self::ChatgptSubscription { classes, .. } => {
                classes
            }
        }
    }

    fn timeout_ms(&self) -> u64 {
        match self {
            Self::OpenaiCompatible { timeout_ms, .. }
            | Self::ChatgptSubscription { timeout_ms, .. } => *timeout_ms,
        }
    }
}

/// What a route may deliver from the attachments an authorized capability produced.
///
/// Provider bytes reaching a chat conversation is new reach, so an owner grants it per route rather
/// than a provider declaring it. Absence is the default and means a capability result's reserved
/// `attachments` key is stripped and refused, which is what keeps a newly granted capability from
/// silently starting to post files.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProviderAttachmentsConfig {
    /// Attachments one reply may carry, across every capability call the session makes.
    pub max_per_reply: u8,
}

/// Which conversations on a transport a route claims.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(
    tag = "kind",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RouteMatch {
    /// One-to-one conversations with the bot.
    ///
    /// A struct variant with no fields rather than a unit variant, for the reason
    /// [`ConversationConfig::OneShot`] is one: serde's internally tagged *unit* variants accept and
    /// discard every key beside the tag, so `kind: directMessage` with a `channel` beside it would
    /// decode cleanly and throw the channel away — leaving an operator believing they scoped a route
    /// that in fact claims every direct message on the transport. An empty struct variant under
    /// `deny_unknown_fields` makes that a startup failure with the field name in it.
    DirectMessage {},
    /// Channels the bot is summoned in: one named channel, or **any** of them when `channel` is
    /// absent.
    ///
    /// The channel is optional because the alternative is one route per channel, enumerated by
    /// service-native identifier and re-edited every time somebody creates a channel — a bot that
    /// goes silent in the new channel until an operator notices and redeploys. An absent `channel`
    /// says "wherever I am invited", which is the membership the chat service already controls.
    ///
    /// Widening *where* widens nothing about *who*. The bot must still be @-mentioned to be woken
    /// at all, and every session still opens an attested broker leg that refuses a sender the owner
    /// never mapped, before any model call. A catch-all route reaches exactly the people a named
    /// one did.
    Channel {
        /// The one channel this route claims, or every channel when absent.
        #[serde(default)]
        channel: Option<String>,
    },
}

/// Bounds one routed message's session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RouteLimits {
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    #[serde(default = "default_max_capability_calls")]
    pub max_capability_calls: u32,
    /// Wall-clock bound on one session, counted from the moment the agent starts working.
    ///
    /// Optional because most deployments are bounded well enough by steps and calls; absent means
    /// no wall-clock bound. It is counted from `Started` rather than from receipt, because waiting
    /// for an admission slot is not the agent taking too long. Zero is refused: the way to run
    /// nothing is to disable the route.
    #[serde(default)]
    pub max_duration_ms: Option<u64>,
}

impl Default for RouteLimits {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
            max_capability_calls: DEFAULT_MAX_CAPABILITY_CALLS,
            max_duration_ms: None,
        }
    }
}

const fn default_max_steps() -> u32 {
    DEFAULT_MAX_STEPS
}

const fn default_max_capability_calls() -> u32 {
    DEFAULT_MAX_CAPABILITY_CALLS
}

/// Who may share one persistent transcript replay window.
///
/// This value comes only from trusted route configuration. A transport kind, conversation kind,
/// inbound message, or model response can never select it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum ConversationScope {
    /// Keep one history per authenticated transport subject.
    #[default]
    PrivateConversation,
    /// Share one history among authenticated subjects in the exact routed conversation.
    SharedConversation,
}

/// What a route remembers between one message and the next.
///
/// Tagged on `mode` in the house style, and strict on both halves, because the failure worth
/// preventing is a *silent* one: a persistent-only setting written next to `mode: oneShot` can
/// never take effect, and a setting that can never take effect is far more likely a mode typo than
/// an intention. Rejecting it at decode is what turns that into a startup failure with a field name
/// in it rather than a bot that quietly forgets everything.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(
    tag = "mode",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ConversationConfig {
    /// Every message is an independent session that starts from an empty prompt.
    ///
    /// A struct variant with no fields rather than a unit variant, deliberately. serde's internally
    /// tagged *unit* variants accept and discard every key beside the tag, so `mode: oneShot` with
    /// an `idleTimeoutMs` beside it would decode cleanly and do nothing — exactly the silence this
    /// enum exists to prevent. An empty struct variant under `deny_unknown_fields` rejects it.
    OneShot {},
    /// A bounded private or intentionally shared history is replayed ahead of each new message.
    Persistent {
        /// Who shares the replay window; private per authenticated subject when omitted.
        #[serde(default)]
        scope: ConversationScope,
        /// How long an untouched conversation survives.
        #[serde(default = "default_idle_timeout_ms")]
        idle_timeout_ms: u64,
        /// Exchanges the replayed window holds, oldest dropped first.
        #[serde(default = "default_conversation_max_turns")]
        max_turns: usize,
        /// Bytes the replayed window holds, oldest dropped first.
        #[serde(default = "default_conversation_max_bytes")]
        max_bytes: usize,
    },
}

impl Default for ConversationConfig {
    fn default() -> Self {
        Self::OneShot {}
    }
}

const fn default_idle_timeout_ms() -> u64 {
    DEFAULT_CONVERSATION_IDLE_TIMEOUT.as_secs() * 1_000
}

const fn default_conversation_max_turns() -> usize {
    DEFAULT_CONVERSATION_MAX_TURNS
}

const fn default_conversation_max_bytes() -> usize {
    DEFAULT_CONVERSATION_MAX_BYTES
}

/// One transport-and-conversation to agent binding.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RouteConfig {
    pub transport: String,
    #[serde(rename = "match")]
    pub r#match: RouteMatch,
    pub agent: AgentId,
    /// Overrides model-class selection for this route.
    #[serde(default)]
    pub model: Option<String>,
    /// Lets this route deliver the attachments an authorized capability produced.
    #[serde(default)]
    pub provider_attachments: Option<ProviderAttachmentsConfig>,
    /// Capabilities whose input may name a chat attachment as `chat-asset:<N>`.
    ///
    /// A capability absent from this list keeps such a string verbatim, and the provider decides
    /// what to do with it. Listing one is what lets a sender's attachment bytes reach it.
    #[serde(default)]
    pub chat_asset_inputs: Vec<CapabilityId>,
    /// Offers the `suggest_improvement` tool on this route's sessions.
    ///
    /// Opt-in because the suggestion record carries model-authored text to the telemetry sink
    /// whether or not payload telemetry is on; enabling it is that consent. It grants nothing: a
    /// suggestion is a tagged log record an operator reads, never a change the daemon applies.
    #[serde(default)]
    pub improvement_suggestions: bool,
    #[serde(default)]
    pub limits: RouteLimits,
    /// How much this route's progress surface says.
    ///
    /// Per route because the same event stream serves a family Discord and an operations channel.
    /// Crate-visible: it selects a rendering, and nothing outside this daemon renders.
    #[serde(default)]
    pub(crate) progress_detail: ProgressDetail,
    /// What this route remembers between messages; `oneShot` unless an operator says otherwise.
    #[serde(default)]
    pub conversation: ConversationConfig,
}

/// A persistent route's bounds, with `idleTimeoutMs` already resolved to a [`Duration`].
///
/// Both window bounds apply together, oldest exchanges dropping first until each holds. Two bounds
/// because they fail differently: twelve one-line exchanges and twelve paragraph-length ones are the
/// same number of turns and very different prompts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversationWindow {
    /// Who shares the replay window, resolved from trusted route configuration.
    pub scope: ConversationScope,
    /// How long an untouched conversation survives before a lookup drops it.
    pub idle_timeout: Duration,
    /// What the replayed window holds.
    pub limits: HistoryLimits,
}

/// What a route remembers, after validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationPolicy {
    /// No history: every message is an independent session, which is every route's default.
    OneShot,
    /// A bounded private or intentionally shared history, replayed ahead of each new message.
    Persistent(ConversationWindow),
}

impl ConversationPolicy {
    /// The window this route replays, or `None` when it remembers nothing.
    #[must_use]
    pub const fn window(self) -> Option<ConversationWindow> {
        match self {
            Self::OneShot => None,
            Self::Persistent(window) => Some(window),
        }
    }
}

/// One route after its agent, model, and conversation settings were validated.
#[derive(Clone, Debug)]
pub struct ResolvedRoute {
    pub transport: String,
    pub r#match: RouteMatch,
    pub agent: AgentId,
    /// Overrides model-class selection for this route.
    pub model: Option<String>,
    /// Attachments one reply on this route may carry; zero for a route that delivers none.
    pub provider_attachments: u8,
    /// Capabilities whose input may name a chat attachment as `chat-asset:<N>`.
    pub chat_asset_inputs: Vec<CapabilityId>,
    /// Whether this route's sessions may record improvement suggestions.
    pub improvement_suggestions: bool,
    pub limits: RouteLimits,
    /// How much this route's progress surface says.
    pub(crate) progress_detail: ProgressDetail,
    pub conversation: ConversationPolicy,
}

/// Process-wide session admission bounds.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionsConfig {
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Whether a rejected message gets a short "try again" reply instead of silence.
    #[serde(default = "default_reply_on_busy")]
    pub reply_on_busy: bool,
    /// Conversations this process tracks at once, across every persistent route.
    ///
    /// A memory bound rather than an admission bound: reaching it evicts the least recently used
    /// conversation rather than refusing a message, because a person talking now matters more than
    /// one who stopped an hour ago. It lives here rather than in a route block because it is a
    /// property of the process, and `sessions:` is already where what this daemon costs at once is
    /// configured.
    #[serde(default = "default_max_conversations")]
    pub max_conversations: usize,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            max_concurrent: DEFAULT_MAX_CONCURRENT_SESSIONS,
            reply_on_busy: true,
            max_conversations: DEFAULT_MAX_CONVERSATIONS,
        }
    }
}

const fn default_max_concurrent() -> usize {
    DEFAULT_MAX_CONCURRENT_SESSIONS
}

const fn default_max_conversations() -> usize {
    DEFAULT_MAX_CONVERSATIONS
}

const fn default_reply_on_busy() -> bool {
    true
}

/// Gateway-owned OTLP export settings, identical in shape to the broker's.
///
/// The ingest credential is deliberately absent: the OpenTelemetry SDK reads it from
/// `OTEL_EXPORTER_OTLP_HEADERS`, so no token enters this file, the command line, or a span.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TelemetryConfig {
    pub endpoint: String,
    pub transport: Transport,
    pub service_name: String,
    pub export_timeout_ms: u64,
}

/// Gateway telemetry after validation.
#[derive(Clone, Debug)]
pub struct ResolvedTelemetry {
    pub settings: ExporterSettings,
}

/// Where and how to reach the broker, after discovery defaults were applied.
#[derive(Clone, Debug)]
pub struct ResolvedBroker {
    pub socket_path: PathBuf,
    pub server_uid: u32,
    pub frame: FrameLimits,
}

/// One validated configuration, before the catalog is consulted.
#[derive(Debug)]
pub struct ResolvedConfig {
    pub source: PathBuf,
    pub catalog_path: PathBuf,
    pub broker: ResolvedBroker,
    pub transports: Vec<TransportConfig>,
    pub models: Vec<ModelConfig>,
    pub routes: Vec<ResolvedRoute>,
    /// Each transport's validated liveness settings, by transport name.
    ///
    /// Resolved once here rather than re-derived per session: the templates are validated at
    /// startup, so a placeholder nobody can render is a refusal instead of a line that reads wrong
    /// in a chat window an hour later.
    pub(crate) liveness: BTreeMap<String, Arc<ResolvedLiveness>>,
    /// The words that stop a running session, lowercased.
    pub(crate) stop_words: Vec<String>,
    pub sessions: SessionsConfig,
    pub shutdown_grace: Duration,
    pub telemetry: Option<ResolvedTelemetry>,
}

/// Reads, hygiene-checks, and strictly decodes one gateway configuration.
pub async fn load(
    path: impl AsRef<Path>,
    expected_uid: u32,
) -> Result<ResolvedConfig, ConfigError> {
    let path = absolute(path.as_ref())?;
    // Authored configuration, not a secret: the gateway's own credentials live in the environment
    // and in transport credential files, so the bar here is that nobody else can rewrite it.
    let owned = path.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        read_trusted_file(
            &owned,
            expected_uid,
            FileTier::NotWorldWritable,
            HARD_MAX_CONFIG_BYTES,
        )
    })
    .await
    .map_err(|join| ConfigError::Read {
        path: path.clone(),
        source: io::Error::other(join),
    })?
    .map_err(|error| match error {
        FileHygieneError::NotRegular { path, .. } => ConfigError::NotRegular { path },
        FileHygieneError::TooLarge {
            length, maximum, ..
        } => ConfigError::TooLarge { length, maximum },
        FileHygieneError::Io { path, source } => ConfigError::Read { path, source },
        insecure => ConfigError::InsecureFile {
            path: path.clone(),
            source: insecure,
        },
    })?;
    let config = decode(&bytes)?;
    resolve(
        config,
        path,
        &BrokerSocketDiscovery::from_process(None),
        expected_uid,
    )
}

/// What serde writes when strict decoding meets the block this release replaced.
///
/// The whole match: `deny_unknown_fields` has no case for a key that used to exist, so the name in
/// the decoder's own refusal is the only trace a retired block leaves.
const RETIRED_ACTIVITY_FIELD: &str = "unknown field `activity`";

/// Strictly decodes one gateway configuration, naming the block this release replaced.
///
/// Nothing here reads what an `activity:` block contained, and no field accepts one: the decoder
/// refuses the key, and this maps that refusal onto the sentence that says what to write instead.
/// serde's own sentence lists the fields a transport does accept, which tells an operator that
/// `activity` is not among them and nothing about where it went.
fn decode(document: &[u8]) -> Result<DekopondConfig, ConfigError> {
    serde_yaml::from_slice::<DekopondConfig>(document).map_err(|source| {
        if source.to_string().contains(RETIRED_ACTIVITY_FIELD) {
            ConfigError::RetiredActivityBlock { source }
        } else {
            ConfigError::Decode { source }
        }
    })
}

fn absolute(path: &Path) -> Result<PathBuf, ConfigError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    env::current_dir()
        .map(|directory| directory.join(path))
        .map_err(|source| ConfigError::CurrentDirectory { source })
}

pub(crate) fn resolve(
    config: DekopondConfig,
    source: PathBuf,
    discovery: &BrokerSocketDiscovery,
    current_uid: u32,
) -> Result<ResolvedConfig, ConfigError> {
    let base = source
        .parent()
        .ok_or(ConfigError::MissingParent)?
        .to_path_buf();
    let resolve_path = |path: PathBuf| {
        if path.is_absolute() {
            path
        } else {
            base.join(path)
        }
    };

    // Every semantic problem in this file is collected before any of them is reported, the way
    // `dekopon-config` scans a whole catalog: an operator with three mistakes in one file fixes
    // three and restarts once, instead of rediscovering the next one after every restart. Only a
    // failure that makes the rest unreadable — the file's hygiene, or its decode — stops earlier.
    let mut problems = Vec::new();

    if config.transports.is_empty() {
        problems.push(ConfigProblem::NoTransports);
    }
    if config.models.is_empty() {
        problems.push(ConfigProblem::NoModels);
    }
    if config.routes.is_empty() {
        problems.push(ConfigProblem::NoRoutes);
    }

    // A transport that never reached the name set cannot be named by a route, so the reference
    // checks below would blame the routes pointing at it on top of reporting the real failure.
    // A *duplicate* name is not that case: the first declaration is still in the set and still
    // resolves, which is exactly why `dekopon-config` leaves duplicates out of `drops_resource`.
    let mut transports_incomplete = config.transports.is_empty();
    let mut transport_names = BTreeSet::new();
    // Transports that cannot carry an attachment at all, recorded before validation so the route
    // pairing check below does not pass merely because this transport had a problem of its own.
    let mut text_only_transports = BTreeSet::new();
    let mut transports = Vec::with_capacity(config.transports.len());
    let mut liveness_settings = BTreeMap::new();
    for transport in config.transports {
        let name = transport.name().to_owned();
        if name.trim().is_empty() {
            problems.push(ConfigProblem::UnnamedTransport);
            transports_incomplete = true;
            continue;
        }
        if !transport_names.insert(name.clone()) {
            problems.push(ConfigProblem::DuplicateTransport { name });
            continue;
        }
        if matches!(transport, TransportConfig::WhatsappCloudApi { .. }) {
            text_only_transports.insert(name.clone());
        }
        liveness_settings.insert(
            name.clone(),
            Arc::new(resolve_liveness(&transport, &mut problems)),
        );
        transports.push(match transport {
            TransportConfig::SlackSocketMode {
                name,
                app_token_env,
                bot_token_env,
                experience,
                liveness,
                endpoint,
            } => {
                check_env_name(&app_token_env, &mut problems);
                check_env_name(&bot_token_env, &mut problems);
                let endpoint = checked_endpoint(endpoint, SLACK_ENDPOINT, &mut problems);
                TransportConfig::SlackSocketMode {
                    name,
                    app_token_env,
                    bot_token_env,
                    experience,
                    liveness,
                    endpoint: Some(endpoint),
                }
            }
            TransportConfig::DiscordGateway {
                name,
                bot_token_env,
                liveness,
                endpoint,
            } => {
                check_env_name(&bot_token_env, &mut problems);
                let endpoint = checked_endpoint(endpoint, DISCORD_ENDPOINT, &mut problems);
                TransportConfig::DiscordGateway {
                    name,
                    bot_token_env,
                    liveness,
                    endpoint: Some(endpoint),
                }
            }
            TransportConfig::WhatsappCloudApi {
                name,
                app_secret_env,
                verify_token_env,
                access_token_env,
                bind,
                callback_path,
                waba_id,
                phone_number_id,
                graph_api_version,
                liveness,
                graph_endpoint,
            } => {
                check_env_name(&app_secret_env, &mut problems);
                check_env_name(&verify_token_env, &mut problems);
                check_env_name(&access_token_env, &mut problems);
                if !canonical_positive_decimal(&waba_id)
                    || !canonical_positive_decimal(&phone_number_id)
                {
                    problems.push(ConfigProblem::InvalidWhatsappScope { name: name.clone() });
                }
                if bind.port() == 0 {
                    problems.push(ConfigProblem::InvalidWhatsappBind { name: name.clone() });
                }
                if !valid_whatsapp_callback_path(&callback_path) {
                    problems.push(ConfigProblem::InvalidWhatsappCallback { name: name.clone() });
                }
                if !valid_graph_version(&graph_api_version) {
                    problems
                        .push(ConfigProblem::InvalidWhatsappGraphVersion { name: name.clone() });
                }
                let graph_endpoint =
                    checked_endpoint(graph_endpoint, WHATSAPP_GRAPH_ENDPOINT, &mut problems);
                TransportConfig::WhatsappCloudApi {
                    name,
                    app_secret_env,
                    verify_token_env,
                    access_token_env,
                    bind,
                    callback_path,
                    waba_id,
                    phone_number_id,
                    graph_api_version,
                    liveness,
                    graph_endpoint: Some(graph_endpoint),
                }
            }
            TransportConfig::TelegramLongPoll {
                name,
                bot_token_env,
                liveness,
                endpoint,
            } => {
                check_env_name(&bot_token_env, &mut problems);
                let endpoint = checked_endpoint(endpoint, TELEGRAM_ENDPOINT, &mut problems);
                TransportConfig::TelegramLongPoll {
                    name,
                    bot_token_env,
                    liveness,
                    endpoint: Some(endpoint),
                }
            }
            TransportConfig::Local {
                name,
                socket_path,
                liveness,
            } => TransportConfig::Local {
                name,
                socket_path: resolve_path(socket_path),
                liveness,
            },
        });
    }

    let mut models_incomplete = config.models.is_empty();
    let mut model_names = BTreeSet::new();
    for model in &config.models {
        let name = model.name().to_owned();
        if name.trim().is_empty() {
            problems.push(ConfigProblem::UnnamedModel);
            models_incomplete = true;
            continue;
        }
        if !model_names.insert(name.clone()) {
            problems.push(ConfigProblem::DuplicateModel { name });
            continue;
        }
        if model.timeout_ms() == 0 {
            problems.push(ConfigProblem::InvalidModelTimeout { name });
        }
        if let ModelConfig::OpenaiCompatible {
            api_key_env: Some(variable),
            ..
        } = model
        {
            check_env_name(variable, &mut problems);
        }
    }

    let mut routes = Vec::with_capacity(config.routes.len());
    for route in config.routes {
        if !transports_incomplete && !transport_names.contains(&route.transport) {
            problems.push(ConfigProblem::UnknownRouteTransport {
                transport: route.transport.clone(),
            });
        }
        if let Some(model) = &route.model
            && !models_incomplete
            && !model_names.contains(model)
        {
            problems.push(ConfigProblem::UnknownRouteModel {
                model: model.clone(),
            });
        }
        // A bound of zero is a bound nobody meant to write: the way to deliver no attachments is
        // to leave the block out, and writing one that can never accept anything would make a
        // capability's files vanish with the route looking as though it carried them.
        if route
            .provider_attachments
            .is_some_and(|attachments| attachments.max_per_reply == 0)
        {
            problems.push(ConfigProblem::InvalidProviderAttachments {
                agent: route.agent.to_string(),
            });
        }
        // A provider attachment on a text-only transport would be authorized, paid for, and then
        // dropped on the way out. Refusing the pair at startup is the only place that is legible.
        if route.provider_attachments.is_some() && text_only_transports.contains(&route.transport) {
            problems.push(ConfigProblem::UnsupportedRouteProviderAttachments {
                transport: route.transport.clone(),
            });
        }
        if route.limits.max_steps == 0 || route.limits.max_capability_calls == 0 {
            problems.push(ConfigProblem::InvalidRouteLimits {
                agent: route.agent.to_string(),
            });
        }
        // A bound of zero cancels the session in the same instant it starts, which is a route that
        // can only ever answer `Stopped.`; the way to run nothing is to remove the route.
        if route.limits.max_duration_ms == Some(0) {
            problems.push(ConfigProblem::InvalidRouteDuration {
                agent: route.agent.to_string(),
            });
        }
        // A bound of zero is a bound nobody meant to write, exactly as a zero step budget already
        // is. The other half of this check — a window setting on a `oneShot` route — is a decode
        // failure rather than a check here, because there is no field it could have landed in.
        let conversation = match route.conversation {
            ConversationConfig::OneShot {} => ConversationPolicy::OneShot,
            ConversationConfig::Persistent {
                scope,
                idle_timeout_ms,
                max_turns,
                max_bytes,
            } => {
                if idle_timeout_ms == 0 || max_turns == 0 || max_bytes == 0 {
                    problems.push(ConfigProblem::InvalidConversationBounds {
                        agent: route.agent.to_string(),
                    });
                }
                ConversationPolicy::Persistent(ConversationWindow {
                    scope,
                    idle_timeout: Duration::from_millis(idle_timeout_ms),
                    limits: HistoryLimits {
                        max_turns,
                        max_bytes,
                    },
                })
            }
        };
        routes.push(ResolvedRoute {
            transport: route.transport,
            r#match: route.r#match,
            agent: route.agent,
            model: route.model,
            provider_attachments: route
                .provider_attachments
                .map_or(0, |attachments| attachments.max_per_reply),
            chat_asset_inputs: route.chat_asset_inputs,
            improvement_suggestions: route.improvement_suggestions,
            limits: route.limits,
            progress_detail: route.progress_detail,
            conversation,
        });
    }

    // Lowercased once here so the matcher in `dispatch` compares two values that were normalized
    // the same way, rather than lowercasing the operator's list on every inbound message.
    let stop_words = match config.stop_words {
        Some(words) if words.is_empty() || words.iter().any(|word| word.trim().is_empty()) => {
            problems.push(ConfigProblem::InvalidStopWords);
            Vec::new()
        }
        Some(words) => words
            .iter()
            .map(|word| word.trim().to_lowercase())
            .collect(),
        None => DEFAULT_STOP_WORDS
            .iter()
            .map(|word| (*word).to_owned())
            .collect(),
    };

    if config.sessions.max_concurrent == 0 {
        problems.push(ConfigProblem::InvalidSessionLimits);
    }
    if config.sessions.max_conversations == 0 {
        problems.push(ConfigProblem::InvalidMaxConversations);
    }
    let shutdown_grace = match config.shutdown_grace_ms {
        Some(0) => {
            problems.push(ConfigProblem::InvalidSessionLimits);
            DEFAULT_SHUTDOWN_GRACE
        }
        Some(milliseconds) => Duration::from_millis(milliseconds),
        None => DEFAULT_SHUTDOWN_GRACE,
    };

    let frame = match (FrameLimits {
        max_frame_bytes: config
            .broker
            .max_frame_bytes
            .unwrap_or(DEFAULT_MAX_FRAME_BYTES),
        io_timeout: config
            .broker
            .io_timeout_ms
            .map_or(DEFAULT_IO_TIMEOUT, Duration::from_millis),
    })
    .validate()
    {
        Ok(frame) => Some(frame),
        Err(source) => {
            problems.push(ConfigProblem::BrokerLimits { source });
            None
        }
    };
    let socket_path = match config.broker.socket_path {
        Some(path) => Some(resolve_path(path)),
        None => match discovery.resolve().map(ResolvedBrokerSocket::into_path) {
            Some(path) => Some(path),
            None => {
                problems.push(ConfigProblem::BrokerSocketUnresolved);
                None
            }
        },
    };

    let telemetry = match config.telemetry.as_ref().map(|telemetry| {
        ExporterSettings::new(
            &telemetry.endpoint,
            telemetry.transport,
            &telemetry.service_name,
            "dekopond",
            env!("CARGO_PKG_VERSION"),
            Duration::from_millis(telemetry.export_timeout_ms),
        )
        .map(|settings| ResolvedTelemetry { settings })
    }) {
        None => Some(None),
        Some(Ok(telemetry)) => Some(Some(telemetry)),
        Some(Err(source)) => {
            problems.push(ConfigProblem::Telemetry { source });
            None
        }
    };

    match (frame, socket_path, telemetry) {
        (Some(frame), Some(socket_path), Some(telemetry)) if problems.is_empty() => {
            Ok(ResolvedConfig {
                source,
                catalog_path: resolve_path(config.catalog_path),
                broker: ResolvedBroker {
                    socket_path,
                    server_uid: config.broker.server_uid.unwrap_or(current_uid),
                    frame,
                },
                transports,
                models: config.models,
                routes,
                liveness: liveness_settings,
                stop_words,
                sessions: config.sessions,
                shutdown_grace,
                telemetry,
            })
        }
        // Each of the three above pushed its own problem in place of a value, so every path that
        // lands here carries at least one.
        _ => Err(ConfigError::Invalid {
            path: source,
            problems,
        }),
    }
}

/// Validates one transport's `liveness:` block, recording every setting it cannot honor.
///
/// Every setting rather than the first, and a value comes back either way: the caller is
/// collecting a whole file's problems and will refuse the configuration itself.
fn resolve_liveness(
    transport: &TransportConfig,
    problems: &mut Vec<ConfigProblem>,
) -> ResolvedLiveness {
    let name = transport.name().to_owned();
    let (liveness, experience) = match transport {
        TransportConfig::SlackSocketMode {
            liveness,
            experience,
            ..
        } => (liveness, Some(*experience)),
        TransportConfig::DiscordGateway { liveness, .. }
        | TransportConfig::TelegramLongPoll { liveness, .. }
        | TransportConfig::WhatsappCloudApi { liveness, .. }
        | TransportConfig::Local { liveness, .. } => (liveness, None),
    };

    // A surface configured under `mode: off` is a setting that can never take effect, which is far
    // more likely a forgotten `mode:` than an intention.
    if liveness.mode == LivenessMode::Off {
        for surface in [
            (liveness.progress != ProgressSurface::Off).then_some("progress"),
            liveness.stream.then_some("stream"),
            liveness.cancel_button.then_some("cancelButton"),
        ]
        .into_iter()
        .flatten()
        {
            problems.push(ConfigProblem::LivenessSurfaceWithoutMode {
                transport: name.clone(),
                surface,
            });
        }
    }

    if matches!(transport, TransportConfig::WhatsappCloudApi { .. }) {
        for surface in [
            liveness.stream.then_some("stream"),
            liveness.cancel_button.then_some("cancelButton"),
        ]
        .into_iter()
        .flatten()
        {
            problems.push(ConfigProblem::UnsupportedLivenessSurface {
                transport: name.clone(),
                surface,
                reason: "WhatsApp messages cannot be edited and carry no interactive components",
            });
        }
    }
    if experience == Some(SlackExperience::Agent) && liveness.cancel_button {
        problems.push(ConfigProblem::UnsupportedLivenessSurface {
            transport: name.clone(),
            surface: "cancelButton",
            reason: "Slack's Agent experience renders its own Stop control",
        });
    }

    match experience {
        // Carried over unchanged: the fallback is the classic app's only signal, and a native
        // Agent installation that loses status falls back to it.
        Some(experience) => {
            let coherent = match (experience, liveness.mode) {
                (_, LivenessMode::Off) => liveness.classic_fallback == SlackLivenessFallback::None,
                (SlackExperience::Classic, LivenessMode::Native) => {
                    liveness.classic_fallback == SlackLivenessFallback::Reaction
                }
                (SlackExperience::Agent, LivenessMode::Native) => true,
            };
            if !coherent {
                problems.push(ConfigProblem::InvalidSlackLiveness {
                    transport: name.clone(),
                });
            }
        }
        None if liveness.classic_fallback != SlackLivenessFallback::None => {
            problems.push(ConfigProblem::UnsupportedLivenessFallback {
                transport: name.clone(),
            });
        }
        None => {}
    }

    // A zero period is a render loop rather than a keep-alive, and a tick at zero seconds is the
    // post at `Started` this design deliberately does not make.
    if liveness.keep_alive.every_seconds == 0 || liveness.keep_alive.at_seconds.contains(&0) {
        problems.push(ConfigProblem::InvalidKeepAlive {
            transport: name.clone(),
        });
    }

    let (templates, template_problems) =
        Templates::resolve(&liveness.templates, STOPPED_REPLY, FAILURE_REPLY);
    for problem in template_problems {
        problems.push(ConfigProblem::InvalidLivenessTemplate {
            transport: name.clone(),
            field: problem.field.key(),
            placeholder: problem.placeholder,
        });
    }

    ResolvedLiveness {
        settings: liveness.settings(),
        keep_alive: KeepAlive {
            at: liveness
                .keep_alive
                .at_seconds
                .iter()
                .map(|seconds| Duration::from_secs(*seconds))
                .collect(),
            every: Duration::from_secs(liveness.keep_alive.every_seconds),
            max: liveness.keep_alive.max,
        },
        templates,
    }
}

/// Records an invalid environment variable name rather than abandoning the rest of the scan.
fn check_env_name(name: &str, problems: &mut Vec<ConfigProblem>) {
    if let Err(problem) = validate_env_name(name) {
        problems.push(problem);
    }
}

/// Records an unsupported endpoint and keeps scanning under the pinned production origin.
///
/// The substituted value never reaches a socket: a recorded problem is a refusal, and the resolved
/// configuration this would belong to is not returned at all.
fn checked_endpoint(
    endpoint: Option<String>,
    production: &str,
    problems: &mut Vec<ConfigProblem>,
) -> String {
    match validate_endpoint(endpoint, production) {
        Ok(endpoint) => endpoint,
        Err(problem) => {
            problems.push(problem);
            production.to_owned()
        }
    }
}

/// Accepts an environment variable *name*, which is never a secret and is safe to echo.
///
/// The grammar is deliberately narrower than the operating system's: a name with `=` or a NUL is
/// unreachable through `env::var_os` anyway, and one with a space is almost always a typo that
/// would otherwise surface as "this token is missing" at connect time.
fn validate_env_name(name: &str) -> Result<(), ConfigProblem> {
    let mut bytes = name.bytes();
    let valid = match bytes.next() {
        Some(first) if first.is_ascii_alphabetic() || first == b'_' => {
            bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ConfigProblem::InvalidEnvironmentName {
            name: name.to_owned(),
        })
    }
}

/// Accepts the one production origin or a literal loopback HTTP URL.
///
/// Overridability exists so tests can point a transport at a mock, and the loopback restriction is
/// what keeps that from doubling as a way to send a bot token to an arbitrary host. The host is
/// compared after stripping userinfo, so `http://127.0.0.1@evil.test` does not read as loopback.
fn validate_endpoint(endpoint: Option<String>, production: &str) -> Result<String, ConfigProblem> {
    let Some(endpoint) = endpoint else {
        return Ok(production.to_owned());
    };
    let trimmed = endpoint.trim_end_matches('/');
    if trimmed == production {
        return Ok(trimmed.to_owned());
    }
    if let Some(authority) = trimmed.strip_prefix("http://")
        && is_loopback_authority(authority)
    {
        return Ok(trimmed.to_owned());
    }
    Err(ConfigProblem::UnsupportedEndpoint {
        endpoint,
        production: production.to_owned(),
    })
}

fn canonical_positive_decimal(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('0')
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_whatsapp_callback_path(value: &str) -> bool {
    value.len() <= 256
        && value.starts_with('/')
        && !value.ends_with('/')
        && value.split('/').skip(1).all(|segment| {
            !segment.is_empty()
                && segment.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'-' | b'_')
                })
        })
}

fn valid_graph_version(value: &str) -> bool {
    let Some(version) = value.strip_prefix('v') else {
        return false;
    };
    let Some((major, minor)) = version.split_once('.') else {
        return false;
    };
    minor == "0"
        && !major.is_empty()
        && major.len() <= 3
        && !major.starts_with('0')
        && major.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_loopback_authority(authority: &str) -> bool {
    // Anything before `@` is userinfo and anything after `/` is a path; neither is the host the
    // socket would connect to, and both are how a remote authority disguises itself as loopback.
    if authority.contains('@') || authority.contains('/') {
        return false;
    }
    let host = match authority.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((literal, tail)) if tail.is_empty() || tail.starts_with(':') => literal,
            _ => return false,
        },
        None => authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host),
    };
    matches!(host.to_ascii_lowercase().as_str(), "127.0.0.1" | "::1")
}

/// Strict configuration failure.
///
/// Only the ways a file can be unusable before it is understood stop at the first error. Every
/// semantic problem in a file that decoded is reported together through [`ConfigError::Invalid`],
/// which is the shape `dekopon-config` already refuses a catalog with. A file that still carries
/// the retired `activity:` block stops there too: strict decoding refuses the key, and the refusal
/// names the replacement instead of listing the fields a transport does accept.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not determine the current directory")]
    CurrentDirectory {
        #[source]
        source: io::Error,
    },
    #[error("could not read gateway configuration at {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("gateway configuration path is not a regular non-symlink file: {path}")]
    NotRegular { path: PathBuf },
    #[error(
        "gateway configuration must be single-link, owned by the daemon UID, and not group/world writable: {path}"
    )]
    InsecureFile {
        /// The refused path.
        path: PathBuf,
        /// Which hygiene check refused it.
        #[source]
        source: FileHygieneError,
    },
    #[error("gateway configuration is {length} bytes; maximum is {maximum}")]
    TooLarge { length: u64, maximum: usize },
    #[error("gateway configuration is not strict valid YAML/JSON")]
    Decode {
        #[source]
        source: serde_yaml::Error,
    },
    #[error(
        "gateway configuration declares activity:, which this release replaced; write liveness: instead, with classicFallback where the old activity block had it"
    )]
    RetiredActivityBlock {
        /// The decoder's own refusal, which carries where in the file the block was written.
        #[source]
        source: serde_yaml::Error,
    },
    #[error("configured path has no parent")]
    MissingParent,
    /// The file decoded, and every semantic problem found in it is listed here.
    #[error("{path}: {}", render_problems(.problems))]
    Invalid {
        /// The configuration file every problem below was found in.
        path: PathBuf,
        /// Every problem found, in file order.
        problems: Vec<ConfigProblem>,
    },
}

/// One semantic problem in an otherwise decodable gateway configuration.
///
/// The file is scanned to the end before it is refused, so an operator fixing three mistakes
/// restarts the daemon once rather than three times. Problems are reported through
/// [`ConfigError::Invalid`], which owns the source path they all share.
#[derive(Debug, Error)]
pub enum ConfigProblem {
    #[error("gateway configuration must declare at least one transport")]
    NoTransports,
    #[error("gateway configuration must declare at least one model")]
    NoModels,
    #[error("gateway configuration must declare at least one route")]
    NoRoutes,
    #[error("every transport must have a name")]
    UnnamedTransport,
    #[error("transport name {name:?} is declared more than once")]
    DuplicateTransport { name: String },
    #[error("every model must have a name")]
    UnnamedModel,
    #[error("model name {name:?} is declared more than once")]
    DuplicateModel { name: String },
    #[error("model {name:?} must have a timeout greater than zero")]
    InvalidModelTimeout { name: String },
    #[error(
        "Slack transport {transport:?} has a liveness fallback that cannot take effect; off requires fallback none, and a classic app with native liveness requires fallback reaction"
    )]
    InvalidSlackLiveness { transport: String },
    #[error(
        "transport {transport:?} sets liveness.{surface} while liveness.mode is off, where it can never take effect"
    )]
    LivenessSurfaceWithoutMode {
        transport: String,
        surface: &'static str,
    },
    #[error("transport {transport:?} cannot use liveness.{surface}: {reason}")]
    UnsupportedLivenessSurface {
        transport: String,
        surface: &'static str,
        reason: &'static str,
    },
    #[error(
        "transport {transport:?} sets liveness.classicFallback, which only a slackSocketMode transport has"
    )]
    UnsupportedLivenessFallback { transport: String },
    #[error(
        "transport {transport:?} has a liveness.keepAlive bound of zero; everySeconds must be greater than zero and no offset may be zero"
    )]
    InvalidKeepAlive { transport: String },
    #[error(
        "transport {transport:?} liveness template {field} uses {{{placeholder}}}, which it cannot render"
    )]
    InvalidLivenessTemplate {
        transport: String,
        field: &'static str,
        placeholder: String,
    },
    #[error(
        "stopWords must not be empty and no word may be blank; omit the key to keep the default list"
    )]
    InvalidStopWords,
    #[error(
        "route for agent {agent:?} sets limits.maxDurationMs to 0, which cancels every session the instant it starts; omit it for no wall-clock bound"
    )]
    InvalidRouteDuration { agent: String },
    #[error("WhatsApp transport {name:?} must bind an explicit nonzero port")]
    InvalidWhatsappBind { name: String },
    #[error("WhatsApp transport {name:?} must use canonical positive WABA and phone-number IDs")]
    InvalidWhatsappScope { name: String },
    #[error("WhatsApp transport {name:?} has an invalid callback path")]
    InvalidWhatsappCallback { name: String },
    #[error("WhatsApp transport {name:?} must pin a Graph API version such as v23.0")]
    InvalidWhatsappGraphVersion { name: String },
    #[error("route names unknown transport {transport:?}")]
    UnknownRouteTransport { transport: String },
    #[error("route names unknown model {model:?}")]
    UnknownRouteModel { model: String },
    #[error(
        "route for agent {agent:?} declares providerAttachments with maxPerReply 0; omit the block to deliver none"
    )]
    InvalidProviderAttachments { agent: String },
    #[error("transport {transport:?} is text-only and cannot deliver a provider attachment")]
    UnsupportedRouteProviderAttachments { transport: String },
    #[error("route for agent {agent:?} must allow at least one step and one capability call")]
    InvalidRouteLimits { agent: String },
    #[error("session bounds must be greater than zero")]
    InvalidSessionLimits,
    #[error(
        "route for agent {agent:?} declares a persistent conversation with a zero bound; its idle timeout, turn window, and byte window must each be greater than zero"
    )]
    InvalidConversationBounds { agent: String },
    #[error(
        "sessions.maxConversations must be greater than zero; a zero ceiling evicts every conversation immediately and turns a persistent route into an expensive one-shot one"
    )]
    InvalidMaxConversations,
    #[error(
        "{name:?} is not a valid environment variable name; transports and models name variables, never secrets"
    )]
    InvalidEnvironmentName { name: String },
    #[error("endpoint {endpoint:?} must be {production} or a literal loopback http:// URL")]
    UnsupportedEndpoint {
        endpoint: String,
        production: String,
    },
    #[error("broker frame bounds are invalid")]
    BrokerLimits {
        #[source]
        source: ProtocolError,
    },
    #[error(
        "could not determine the broker socket path; set broker.socketPath or DEKOPON_BROKER_SOCKET"
    )]
    BrokerSocketUnresolved,
    #[error("invalid gateway telemetry configuration")]
    Telemetry {
        #[source]
        source: TelemetryError,
    },
}

/// Renders every problem in one refusal, each followed by its own cause chain.
///
/// The chain is walked here rather than inlined into each problem's message because a problem's
/// real reason can sit two links down — a credential variable that is unset, under the transport
/// that named it — and an aggregate printing only top lines would be a list of headlines with the
/// reasons removed. Shared by the gateway's three aggregate refusals so they read alike.
pub(crate) fn render_problems<P: std::error::Error>(problems: &[P]) -> String {
    let mut rendered = format!(
        "{} validation problem{} found:",
        problems.len(),
        if problems.len() == 1 { "" } else { "s" }
    );
    for problem in problems {
        rendered.push_str("\n  - ");
        rendered.push_str(&problem.to_string());
        let mut source = std::error::Error::source(problem);
        while let Some(cause) = source {
            rendered.push_str(": ");
            rendered.push_str(&cause.to_string());
            source = cause.source();
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use dekopon_broker_protocol::BrokerSocketDiscovery;

    use super::{
        ConfigError, LivenessMode, ModelConfig, ProgressDetail, ProgressSurface,
        SlackLivenessFallback, resolve,
    };
    use crate::progress::{DEFAULT_KEEP_ALIVE_MAX, KeepAlive};

    /// Everything a configuration needs before the field under test is added to it.
    const PREAMBLE: &str = "apiVersion: dekopon.dev/dekopond/v1alpha1\n\
         catalogPath: dekopon.yaml\n\
         broker: { socketPath: /run/dekopon/broker.sock, serverUid: 501 }\n\
         models:\n\
         \x20 - name: m\n\
         \x20   kind: openaiCompatible\n\
         \x20   endpoint: http://127.0.0.1:11434/v1\n\
         \x20   model: q\n\
         \x20   timeoutMs: 1000\n\
         \x20   classes: [reasoning]\n";

    fn resolved(document: &str) -> Result<super::ResolvedConfig, ConfigError> {
        let config =
            super::decode(format!("{PREAMBLE}{document}").as_bytes()).expect("the fixture decodes");
        resolve(
            config,
            PathBuf::from("/tmp/dekopond.yaml"),
            &BrokerSocketDiscovery::new(None, None, Some(PathBuf::from("/run/user/501")), None),
            501,
        )
    }

    /// An operator with nine mistakes in one file fixes nine and restarts once. Each of these is a
    /// setting that decodes cleanly and could never take effect, which is exactly the class this
    /// file's strict validation exists to refuse out loud rather than absorb.
    #[test]
    fn every_liveness_conflict_in_one_file_is_reported_together() {
        let error = resolved(
            "transports:\n\
             \x20 - name: slack\n\
             \x20   kind: slackSocketMode\n\
             \x20   appTokenEnv: A\n\
             \x20   botTokenEnv: B\n\
             \x20   experience: agent\n\
             \x20   liveness: { mode: native, cancelButton: true }\n\
             \x20 - name: wa\n\
             \x20   kind: whatsappCloudApi\n\
             \x20   appSecretEnv: C\n\
             \x20   verifyTokenEnv: D\n\
             \x20   accessTokenEnv: E\n\
             \x20   bind: 0.0.0.0:9080\n\
             \x20   callbackPath: /hook\n\
             \x20   wabaId: \"1\"\n\
             \x20   phoneNumberId: \"2\"\n\
             \x20   graphApiVersion: v23.0\n\
             \x20   liveness: { mode: native, stream: true, cancelButton: true }\n\
             \x20 - name: tg\n\
             \x20   kind: telegramLongPoll\n\
             \x20   botTokenEnv: F\n\
             \x20   liveness:\n\
             \x20     mode: \"off\"\n\
             \x20     progress: message\n\
             \x20     classicFallback: reaction\n\
             \x20     keepAlive: { everySeconds: 0 }\n\
             \x20     templates: { working: \"Busy with {word}\" }\n\
             stopWords: []\n\
             routes:\n\
             \x20 - transport: slack\n\
             \x20   match: { kind: directMessage }\n\
             \x20   agent: reviewer\n\
             \x20   limits: { maxDurationMs: 0 }\n",
        )
        .expect_err("a file this broken must not resolve");

        let rendered = error.to_string();
        for expected in [
            "transport \"slack\" cannot use liveness.cancelButton",
            "transport \"wa\" cannot use liveness.stream",
            "transport \"wa\" cannot use liveness.cancelButton",
            "transport \"tg\" sets liveness.progress while liveness.mode is off",
            "transport \"tg\" sets liveness.classicFallback",
            "transport \"tg\" has a liveness.keepAlive bound of zero",
            "liveness template working uses {word}",
            "stopWords must not be empty",
            "limits.maxDurationMs to 0",
        ] {
            assert!(
                rendered.contains(expected),
                "every conflict is reported together; {expected:?} is missing from:\n{rendered}"
            );
        }
    }

    /// A file written for the previous release is told what replaced its block, not which keys a
    /// transport happens to accept.
    ///
    /// Strict decoding is what refuses it — no field of any transport accepts `activity` — so this
    /// is the one refusal that cannot be collected beside the file's other problems, and the
    /// sentence carries the whole migration on its own.
    #[test]
    fn a_retired_activity_block_is_refused_by_name() {
        let document = format!(
            "{PREAMBLE}{}",
            "transports:\n\
             \x20 - name: slack\n\
             \x20   kind: slackSocketMode\n\
             \x20   appTokenEnv: A\n\
             \x20   botTokenEnv: B\n\
             \x20   activity: { mode: native, classicFallback: reaction }\n\
             routes:\n\
             \x20 - transport: slack\n\
             \x20   match: { kind: directMessage }\n\
             \x20   agent: reviewer\n"
        );
        let error = super::decode(document.as_bytes())
            .expect_err("a file that still carries the retired block must not decode");

        let rendered = error.to_string();
        for expected in [
            "declares activity:",
            "write liveness: instead",
            "classicFallback",
        ] {
            assert!(
                rendered.contains(expected),
                "the refusal has to carry the migration; {expected:?} is missing from:\n{rendered}"
            );
        }
    }

    /// The shipped shape, so a deployment that writes the block and nothing else gets the schedule
    /// and the sentences this daemon documents rather than an empty one.
    #[test]
    fn a_liveness_block_resolves_to_the_documented_defaults() {
        let config = resolved(
            "transports:\n\
             \x20 - name: dev\n\
             \x20   kind: local\n\
             \x20   socketPath: dev.sock\n\
             \x20   liveness: { mode: native, progress: message }\n\
             routes:\n\
             \x20 - transport: dev\n\
             \x20   match: { kind: directMessage }\n\
             \x20   agent: reviewer\n",
        )
        .expect("a well-formed configuration resolves");

        assert_eq!(
            config.stop_words,
            vec!["stop".to_owned(), "cancel".to_owned()],
            "the default list is what an operator gets by writing nothing"
        );
        let route = config.routes.first().expect("one route");
        assert_eq!(route.progress_detail, ProgressDetail::Plain);
        assert_eq!(route.limits.max_duration_ms, None);
        let liveness = config.liveness.get("dev").expect("the transport resolved");
        assert_eq!(liveness.settings.mode, LivenessMode::Native);
        assert_eq!(liveness.settings.progress, ProgressSurface::Message);
        assert_eq!(
            liveness.settings.classic_fallback,
            SlackLivenessFallback::None
        );
        assert!(!liveness.settings.stream && !liveness.settings.cancel_button);
        assert_eq!(liveness.keep_alive, KeepAlive::default());
        assert_eq!(liveness.keep_alive.max, DEFAULT_KEEP_ALIVE_MAX);
        assert_eq!(liveness.templates.stopped(), super::STOPPED_REPLY);
        assert_eq!(liveness.templates.failed(), super::FAILURE_REPLY);
    }

    /// Stop words are normalized once, where the list is read, rather than on every inbound
    /// message: the matcher compares two values that were lowercased the same way.
    #[test]
    fn configured_stop_words_are_normalized_once() {
        let config = resolved(
            "transports:\n\
             \x20 - name: dev\n\
             \x20   kind: local\n\
             \x20   socketPath: dev.sock\n\
             stopWords: [\"  STOP \", Basta]\n\
             routes:\n\
             \x20 - transport: dev\n\
             \x20   match: { kind: directMessage }\n\
             \x20   agent: reviewer\n",
        )
        .expect("a well-formed configuration resolves");

        assert_eq!(
            config.stop_words,
            vec!["stop".to_owned(), "basta".to_owned()]
        );
    }

    /// Everything a two-model fixture needs around its `models:` block.
    const HEAD: &str = "apiVersion: dekopon.dev/dekopond/v1alpha1\n\
         catalogPath: dekopon.yaml\n\
         broker: { socketPath: /run/dekopon/broker.sock, serverUid: 501 }\n";
    const TAIL: &str = "transports:\n\
         \x20 - name: dev\n\
         \x20   kind: local\n\
         \x20   socketPath: dev.sock\n\
         routes:\n\
         \x20 - transport: dev\n\
         \x20   match: { kind: directMessage }\n\
         \x20   agent: reviewer\n";

    /// Only the kind that has a choice about streaming carries the switch, and its default is on.
    ///
    /// The default is the half worth pinning: a model block written before streaming existed
    /// decodes into a streaming client, which is what makes `stream:` the repair for one endpoint
    /// that gets `stream: true` wrong rather than a switch every deployment has to find.
    /// `kind: chatgptSubscription` streams and cannot be asked not to, so the field written there
    /// is refused by name instead of being accepted and ignored.
    #[test]
    fn only_an_openai_compatible_model_chooses_whether_it_streams() {
        let document = format!(
            "{HEAD}{}{TAIL}",
            "models:\n\
             \x20 - name: default\n\
             \x20   kind: openaiCompatible\n\
             \x20   endpoint: http://127.0.0.1:11434/v1\n\
             \x20   model: q\n\
             \x20   timeoutMs: 1000\n\
             \x20 - name: buffered-proxy\n\
             \x20   kind: openaiCompatible\n\
             \x20   endpoint: http://127.0.0.1:11435/v1\n\
             \x20   model: q\n\
             \x20   timeoutMs: 1000\n\
             \x20   stream: false\n"
        );
        let config = super::decode(document.as_bytes()).expect("the fixture decodes");

        let streaming: Vec<bool> = config
            .models
            .iter()
            .filter_map(|model| match model {
                ModelConfig::OpenaiCompatible { stream, .. } => Some(*stream),
                ModelConfig::ChatgptSubscription { .. } => None,
            })
            .collect();
        assert_eq!(
            streaming,
            vec![true, false],
            "a model block that says nothing streams; the one that says so does not"
        );

        let subscription = format!(
            "{HEAD}{}{TAIL}",
            "models:\n\
             \x20 - name: codex\n\
             \x20   kind: chatgptSubscription\n\
             \x20   model: gpt-5\n\
             \x20   timeoutMs: 1000\n\
             \x20   stream: false\n"
        );
        let error = super::decode(subscription.as_bytes())
            .expect_err("the subscription backend has no streaming switch to set");
        let cause = std::error::Error::source(&error)
            .map(ToString::to_string)
            .expect("the decoder's own refusal is the cause");
        assert!(
            cause.contains("unknown field `stream`"),
            "the refusal names the field the kind does not have: {cause}"
        );
    }
}
