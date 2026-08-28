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
    collections::BTreeSet,
    env, io,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use dekopon_agent::prompt::HistoryLimits;
use dekopon_broker_protocol::{
    BrokerSocketDiscovery, DEFAULT_IO_TIMEOUT, DEFAULT_MAX_FRAME_BYTES, FrameLimits, ProtocolError,
    ResolvedBrokerSocket,
};
use dekopon_core::{AgentId, FileHygieneError, FileTier, read_trusted_file};
use dekopon_telemetry::{ExporterSettings, TelemetryError, Transport};
use serde::Deserialize;
use thiserror::Error;

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

/// Whether a transport publishes native in-flight activity while an authorized session runs.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum ActivityMode {
    /// Preserve the transport's current reply-only behavior.
    #[default]
    Off,
    /// Use the service's native activity surface, with transport-specific fallback where configured.
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
pub enum SlackActivityFallback {
    /// Degrade to the final reply only.
    #[default]
    None,
    /// Add and later remove Dekopon's fixed `:tangerine:` reaction.
    Reaction,
}

/// In-flight activity settings for Discord and Telegram.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NativeActivityConfig {
    #[serde(default)]
    pub mode: ActivityMode,
}

/// Slack-specific activity settings.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SlackActivityConfig {
    #[serde(default)]
    pub mode: ActivityMode,
    /// Used by classic apps and when Agent status is unavailable for this installation.
    #[serde(default)]
    pub classic_fallback: SlackActivityFallback,
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
    /// Named image-generation backends. Empty unless a route explicitly opts in.
    #[serde(default)]
    pub image_generators: Vec<ImageGeneratorConfig>,
    pub routes: Vec<RouteConfig>,
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
        /// Best-effort native activity and its explicit classic/free-workspace fallback.
        #[serde(default)]
        activity: SlackActivityConfig,
        /// Overridable only to `https://slack.com` or a literal loopback HTTP URL, for tests.
        #[serde(default)]
        endpoint: Option<String>,
    },
    /// Discord Gateway: an outbound WebSocket carries messages and REST posts answers.
    DiscordGateway {
        name: String,
        bot_token_env: String,
        /// Best-effort renewable native typing while an authorized session runs.
        #[serde(default)]
        activity: NativeActivityConfig,
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
        /// Overridable only to the pinned production origin or literal loopback HTTP for tests.
        #[serde(default)]
        graph_endpoint: Option<String>,
    },
    /// Telegram long polling: the poll is the wakeup and advancing the offset is the ack.
    TelegramLongPoll {
        name: String,
        bot_token_env: String,
        /// Best-effort renewable native `typing` action while an authorized session runs.
        #[serde(default)]
        activity: NativeActivityConfig,
        /// Overridable only to `https://api.telegram.org` or a literal loopback HTTP URL.
        #[serde(default)]
        endpoint: Option<String>,
    },
    /// A development transport on an owner-only Unix socket that trusts its local caller.
    Local { name: String, socket_path: PathBuf },
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

/// One explicitly configured image-generation backend.
///
/// The production endpoint is fixed inside `dekopon-model`; authored configuration chooses only
/// the model, credential variable, and deadline. This keeps model output from selecting where a
/// credential or image prompt is sent.
#[derive(Clone, Debug, Deserialize)]
#[serde(
    tag = "kind",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ImageGeneratorConfig {
    /// OpenAI's public Images API returning one inline PNG.
    OpenaiImages {
        name: String,
        model: String,
        api_key_env: String,
        timeout_ms: u64,
    },
}

impl ImageGeneratorConfig {
    /// Operator-chosen name routes refer to.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::OpenaiImages { name, .. } => name,
        }
    }

    /// Configured image model identifier.
    #[must_use]
    pub fn model(&self) -> &str {
        match self {
            Self::OpenaiImages { model, .. } => model,
        }
    }

    /// Environment variable containing the model credential.
    #[must_use]
    pub fn api_key_env(&self) -> &str {
        match self {
            Self::OpenaiImages { api_key_env, .. } => api_key_env,
        }
    }

    /// Whole-request deadline in milliseconds.
    #[must_use]
    pub const fn timeout_ms(&self) -> u64 {
        match self {
            Self::OpenaiImages { timeout_ms, .. } => *timeout_ms,
        }
    }
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
}

impl Default for RouteLimits {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
            max_capability_calls: DEFAULT_MAX_CAPABILITY_CALLS,
        }
    }
}

const fn default_max_steps() -> u32 {
    DEFAULT_MAX_STEPS
}

const fn default_max_capability_calls() -> u32 {
    DEFAULT_MAX_CAPABILITY_CALLS
}

/// What a route remembers between one message and the next.
///
/// Tagged on `mode` in the house style, and strict on both halves, because the failure worth
/// preventing is a *silent* one: a window bound written next to `mode: oneShot` can never take
/// effect, and a setting that can never take effect is far more likely a mode typo than an
/// intention. Rejecting it at decode is what turns that into a startup failure with a field name in
/// it rather than a bot that quietly forgets everything.
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
    /// A bounded per-sender history is replayed ahead of each new message.
    Persistent {
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
    /// Explicitly enables the named image generator for this route.
    #[serde(default)]
    pub image_generator: Option<String>,
    #[serde(default)]
    pub limits: RouteLimits,
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
    /// A bounded per-sender history, replayed ahead of each new message.
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
    /// Named image generator, already proved to exist.
    pub image_generator: Option<String>,
    pub limits: RouteLimits,
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
    /// Whether spans and logs carry chat text and canonical subject identifiers.
    pub telemetry_payloads: bool,
}

/// Gateway telemetry after validation.
#[derive(Clone, Debug)]
pub struct ResolvedTelemetry {
    pub settings: ExporterSettings,
    pub telemetry_payloads: bool,
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
    pub image_generators: Vec<ImageGeneratorConfig>,
    pub routes: Vec<ResolvedRoute>,
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
    let config = serde_yaml::from_slice::<DekopondConfig>(&bytes)
        .map_err(|source| ConfigError::Decode { source })?;
    resolve(
        config,
        path,
        &BrokerSocketDiscovery::from_process(None),
        expected_uid,
    )
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

    if config.transports.is_empty() {
        return Err(ConfigError::NoTransports);
    }
    if config.models.is_empty() {
        return Err(ConfigError::NoModels);
    }
    if config.routes.is_empty() {
        return Err(ConfigError::NoRoutes);
    }

    let mut transport_names = BTreeSet::new();
    let mut transports = Vec::with_capacity(config.transports.len());
    for transport in config.transports {
        let name = transport.name().to_owned();
        if name.trim().is_empty() {
            return Err(ConfigError::UnnamedTransport);
        }
        if !transport_names.insert(name.clone()) {
            return Err(ConfigError::DuplicateTransport { name });
        }
        transports.push(match transport {
            TransportConfig::SlackSocketMode {
                name,
                app_token_env,
                bot_token_env,
                experience,
                activity,
                endpoint,
            } => {
                validate_env_name(&app_token_env)?;
                validate_env_name(&bot_token_env)?;
                let activity_is_meaningful = match (experience, activity.mode) {
                    (_, ActivityMode::Off) => {
                        activity.classic_fallback == SlackActivityFallback::None
                    }
                    (SlackExperience::Classic, ActivityMode::Native) => {
                        activity.classic_fallback == SlackActivityFallback::Reaction
                    }
                    (SlackExperience::Agent, ActivityMode::Native) => true,
                };
                if !activity_is_meaningful {
                    return Err(ConfigError::InvalidSlackActivity { name });
                }
                let endpoint = validate_endpoint(endpoint, SLACK_ENDPOINT)?;
                TransportConfig::SlackSocketMode {
                    name,
                    app_token_env,
                    bot_token_env,
                    experience,
                    activity,
                    endpoint: Some(endpoint),
                }
            }
            TransportConfig::DiscordGateway {
                name,
                bot_token_env,
                activity,
                endpoint,
            } => {
                validate_env_name(&bot_token_env)?;
                let endpoint = validate_endpoint(endpoint, DISCORD_ENDPOINT)?;
                TransportConfig::DiscordGateway {
                    name,
                    bot_token_env,
                    activity,
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
                graph_endpoint,
            } => {
                validate_env_name(&app_secret_env)?;
                validate_env_name(&verify_token_env)?;
                validate_env_name(&access_token_env)?;
                if !canonical_positive_decimal(&waba_id)
                    || !canonical_positive_decimal(&phone_number_id)
                {
                    return Err(ConfigError::InvalidWhatsappScope { name });
                }
                if bind.port() == 0 {
                    return Err(ConfigError::InvalidWhatsappBind { name });
                }
                if !valid_whatsapp_callback_path(&callback_path) {
                    return Err(ConfigError::InvalidWhatsappCallback { name });
                }
                if !valid_graph_version(&graph_api_version) {
                    return Err(ConfigError::InvalidWhatsappGraphVersion { name });
                }
                let graph_endpoint = validate_endpoint(graph_endpoint, WHATSAPP_GRAPH_ENDPOINT)?;
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
                    graph_endpoint: Some(graph_endpoint),
                }
            }
            TransportConfig::TelegramLongPoll {
                name,
                bot_token_env,
                activity,
                endpoint,
            } => {
                validate_env_name(&bot_token_env)?;
                let endpoint = validate_endpoint(endpoint, TELEGRAM_ENDPOINT)?;
                TransportConfig::TelegramLongPoll {
                    name,
                    bot_token_env,
                    activity,
                    endpoint: Some(endpoint),
                }
            }
            TransportConfig::Local { name, socket_path } => TransportConfig::Local {
                name,
                socket_path: resolve_path(socket_path),
            },
        });
    }

    let mut model_names = BTreeSet::new();
    for model in &config.models {
        let name = model.name().to_owned();
        if name.trim().is_empty() {
            return Err(ConfigError::UnnamedModel);
        }
        if !model_names.insert(name.clone()) {
            return Err(ConfigError::DuplicateModel { name });
        }
        if model.timeout_ms() == 0 {
            return Err(ConfigError::InvalidModelTimeout { name });
        }
        if let ModelConfig::OpenaiCompatible {
            api_key_env: Some(variable),
            ..
        } = model
        {
            validate_env_name(variable)?;
        }
    }

    let mut image_generator_names = BTreeSet::new();
    for generator in &config.image_generators {
        let name = generator.name().to_owned();
        if name.trim().is_empty() {
            return Err(ConfigError::UnnamedImageGenerator);
        }
        if !image_generator_names.insert(name.clone()) {
            return Err(ConfigError::DuplicateImageGenerator { name });
        }
        if generator.model().trim().is_empty() {
            return Err(ConfigError::UnnamedImageModel { name });
        }
        if generator.timeout_ms() == 0 {
            return Err(ConfigError::InvalidImageGeneratorTimeout { name });
        }
        validate_env_name(generator.api_key_env())?;
    }

    let mut routes = Vec::with_capacity(config.routes.len());
    for route in config.routes {
        if !transport_names.contains(&route.transport) {
            return Err(ConfigError::UnknownRouteTransport {
                transport: route.transport,
            });
        }
        if let Some(model) = &route.model
            && !model_names.contains(model)
        {
            return Err(ConfigError::UnknownRouteModel {
                model: model.clone(),
            });
        }
        if let Some(generator) = &route.image_generator
            && !image_generator_names.contains(generator)
        {
            return Err(ConfigError::UnknownRouteImageGenerator {
                generator: generator.clone(),
            });
        }
        // A generated image on a text-only transport would be paid for, then dropped on the way
        // out. Refusing the pair at startup is the only place that failure is legible.
        if route.image_generator.is_some()
            && transports.iter().any(|transport| {
                transport.name() == route.transport
                    && matches!(transport, TransportConfig::WhatsappCloudApi { .. })
            })
        {
            return Err(ConfigError::UnsupportedRouteImageGenerator {
                transport: route.transport.clone(),
            });
        }
        if route.limits.max_steps == 0 || route.limits.max_capability_calls == 0 {
            return Err(ConfigError::InvalidRouteLimits {
                agent: route.agent.to_string(),
            });
        }
        // A bound of zero is a bound nobody meant to write, exactly as a zero step budget already
        // is. The other half of this check — a window setting on a `oneShot` route — is a decode
        // failure rather than a check here, because there is no field it could have landed in.
        let conversation = match route.conversation {
            ConversationConfig::OneShot {} => ConversationPolicy::OneShot,
            ConversationConfig::Persistent {
                idle_timeout_ms,
                max_turns,
                max_bytes,
            } => {
                if idle_timeout_ms == 0 || max_turns == 0 || max_bytes == 0 {
                    return Err(ConfigError::InvalidConversationBounds {
                        agent: route.agent.to_string(),
                    });
                }
                ConversationPolicy::Persistent(ConversationWindow {
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
            image_generator: route.image_generator,
            limits: route.limits,
            conversation,
        });
    }

    if config.sessions.max_concurrent == 0 {
        return Err(ConfigError::InvalidSessionLimits);
    }
    if config.sessions.max_conversations == 0 {
        return Err(ConfigError::InvalidMaxConversations);
    }
    let shutdown_grace = match config.shutdown_grace_ms {
        Some(0) => return Err(ConfigError::InvalidSessionLimits),
        Some(milliseconds) => Duration::from_millis(milliseconds),
        None => DEFAULT_SHUTDOWN_GRACE,
    };

    let frame = FrameLimits {
        max_frame_bytes: config
            .broker
            .max_frame_bytes
            .unwrap_or(DEFAULT_MAX_FRAME_BYTES),
        io_timeout: config
            .broker
            .io_timeout_ms
            .map_or(DEFAULT_IO_TIMEOUT, Duration::from_millis),
    }
    .validate()
    .map_err(|source| ConfigError::BrokerLimits { source })?;
    let socket_path = match config.broker.socket_path {
        Some(path) => resolve_path(path),
        None => discovery
            .resolve()
            .map(ResolvedBrokerSocket::into_path)
            .ok_or(ConfigError::BrokerSocketUnresolved)?,
    };
    let broker = ResolvedBroker {
        socket_path,
        server_uid: config.broker.server_uid.unwrap_or(current_uid),
        frame,
    };

    let telemetry = config
        .telemetry
        .as_ref()
        .map(|telemetry| {
            Ok::<_, ConfigError>(ResolvedTelemetry {
                settings: ExporterSettings::new(
                    &telemetry.endpoint,
                    telemetry.transport,
                    &telemetry.service_name,
                    "dekopond",
                    env!("CARGO_PKG_VERSION"),
                    Duration::from_millis(telemetry.export_timeout_ms),
                )
                .map_err(|source| ConfigError::Telemetry { source })?,
                telemetry_payloads: telemetry.telemetry_payloads,
            })
        })
        .transpose()?;

    Ok(ResolvedConfig {
        source,
        catalog_path: resolve_path(config.catalog_path),
        broker,
        transports,
        models: config.models,
        image_generators: config.image_generators,
        routes,
        sessions: config.sessions,
        shutdown_grace,
        telemetry,
    })
}

/// Accepts an environment variable *name*, which is never a secret and is safe to echo.
///
/// The grammar is deliberately narrower than the operating system's: a name with `=` or a NUL is
/// unreachable through `env::var_os` anyway, and one with a space is almost always a typo that
/// would otherwise surface as "this token is missing" at connect time.
fn validate_env_name(name: &str) -> Result<(), ConfigError> {
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
        Err(ConfigError::InvalidEnvironmentName {
            name: name.to_owned(),
        })
    }
}

/// Accepts the one production origin or a literal loopback HTTP URL.
///
/// Overridability exists so tests can point a transport at a mock, and the loopback restriction is
/// what keeps that from doubling as a way to send a bot token to an arbitrary host. The host is
/// compared after stripping userinfo, so `http://127.0.0.1@evil.test` does not read as loopback.
fn validate_endpoint(endpoint: Option<String>, production: &str) -> Result<String, ConfigError> {
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
    Err(ConfigError::UnsupportedEndpoint {
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
    #[error("configured path has no parent")]
    MissingParent,
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
    #[error("every image generator must have a name")]
    UnnamedImageGenerator,
    #[error("image generator name {name:?} is declared more than once")]
    DuplicateImageGenerator { name: String },
    #[error("image generator {name:?} must name a model")]
    UnnamedImageModel { name: String },
    #[error("image generator {name:?} must have a timeout greater than zero")]
    InvalidImageGeneratorTimeout { name: String },
    #[error(
        "Slack transport {name:?} has an activity fallback that cannot take effect; off requires fallback none, and classic native activity requires fallback reaction"
    )]
    InvalidSlackActivity { name: String },
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
    #[error("route names unknown image generator {generator:?}")]
    UnknownRouteImageGenerator { generator: String },
    #[error("transport {transport:?} is text-only and cannot deliver a generated image")]
    UnsupportedRouteImageGenerator { transport: String },
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
