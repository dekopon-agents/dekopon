//! This file is checked as strictly as the broker's own config, since it names agent-reachable
//! models and credential env vars, so a writable or symlinked copy could redirect the daemon.

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
    BrokerSocketDiscovery, ChatTransportKind, ConversationKind, ConversationKindMatch,
    ConversationMatch, ConversationMatchProblem, DEFAULT_IO_TIMEOUT, DEFAULT_MAX_FRAME_BYTES,
    FrameLimits, ProtocolError, ResolvedBrokerSocket,
};
use dekopon_core::{
    AgentId, ExternalSubject, FileHygieneError, FileTier,
    fragments::{self, FragmentError, MergeRules},
    read_trusted_file,
};
use dekopon_telemetry::{ExporterSettings, TelemetryError, Transport};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    progress::{KeepAlive, ProgressDetail, Templates},
    session::{FAILURE_REPLY, STOPPED_REPLY},
};

pub const CONFIG_API_VERSION: &str = "dekopon.dev/dekopond/v1alpha1";
pub const HARD_MAX_CONFIG_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_CONCURRENT_SESSIONS: usize = 4;
pub const DEFAULT_MAX_STEPS: u32 = 8;
pub const DEFAULT_MAX_CAPABILITY_CALLS: u32 = 16;
pub const DEFAULT_SCRIPT_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(120);
pub const DEFAULT_CONVERSATION_IDLE_TIMEOUT: Duration = Duration::from_secs(900);
pub const DEFAULT_CONVERSATION_MAX_TURNS: usize = 12;
pub const DEFAULT_CONVERSATION_MAX_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_CONVERSATIONS: usize = 1024;
pub const DEFAULT_STOP_WORDS: [&str; 2] = ["stop", "cancel"];
pub const SLACK_ENDPOINT: &str = "https://slack.com";
pub const DISCORD_ENDPOINT: &str = "https://discord.com";
pub const TELEGRAM_ENDPOINT: &str = "https://api.telegram.org";
pub const WHATSAPP_GRAPH_ENDPOINT: &str = "https://graph.facebook.com";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum ConfigApiVersion {
    #[serde(rename = "dekopon.dev/dekopond/v1alpha1")]
    V1Alpha1,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum LivenessMode {
    #[default]
    Off,
    Native,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SlackExperience {
    #[default]
    Classic,
    Agent,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SlackLivenessFallback {
    #[default]
    None,
    Reaction,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum ProgressSurface {
    #[default]
    Auto,
    Off,
    Message,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KeepAliveConfig {
    #[serde(default = "default_keep_alive_at")]
    pub at_seconds: Vec<u64>,
    #[serde(default = "default_keep_alive_every")]
    pub every_seconds: u64,
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

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LivenessConfig {
    #[serde(default)]
    pub mode: LivenessMode,
    #[serde(default)]
    pub classic_fallback: SlackLivenessFallback,
    #[serde(default)]
    pub progress: ProgressSurface,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub cancel_button: bool,
    #[serde(default)]
    pub keep_alive: KeepAliveConfig,
    #[serde(default)]
    pub templates: TemplateOverrides,
    #[serde(default)]
    pub conversations: BTreeMap<ConversationKind, LivenessOverride>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LivenessOverride {
    pub progress: Option<ProgressSurface>,
    pub stream: Option<bool>,
    pub cancel_button: Option<bool>,
    pub keep_alive: Option<KeepAliveConfig>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TemplateOverrides {
    pub working: Option<String>,
    pub tool: Option<String>,
    pub keep_alive: Option<String>,
    pub stopped: Option<String>,
    pub failed: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LivenessSettings {
    pub mode: LivenessMode,
    pub classic_fallback: SlackLivenessFallback,
    pub progress: ProgressSurface,
    pub stream: bool,
    pub cancel_button: bool,
}

impl LivenessConfig {
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

#[derive(Debug)]
pub(crate) struct ResolvedLiveness {
    pub settings: LivenessSettings,
    pub keep_alive: KeepAlive,
    pub templates: Templates,
    pub conversations: BTreeMap<ConversationKind, LivenessOverride>,
}

impl ResolvedLiveness {
    pub(crate) fn for_kind(&self, kind: ConversationKind) -> (LivenessSettings, KeepAlive) {
        let Some(override_for_kind) = self.conversations.get(&kind) else {
            return (self.settings, self.keep_alive.clone());
        };
        let settings = LivenessSettings {
            progress: override_for_kind.progress.unwrap_or(self.settings.progress),
            stream: override_for_kind.stream.unwrap_or(self.settings.stream),
            cancel_button: override_for_kind
                .cancel_button
                .unwrap_or(self.settings.cancel_button),
            ..self.settings
        };
        let keep_alive = override_for_kind
            .keep_alive
            .as_ref()
            .map_or_else(|| self.keep_alive.clone(), keep_alive_from);
        (settings, keep_alive)
    }
}

fn keep_alive_from(config: &KeepAliveConfig) -> KeepAlive {
    KeepAlive {
        at: config
            .at_seconds
            .iter()
            .map(|seconds| Duration::from_secs(*seconds))
            .collect(),
        every: Duration::from_secs(config.every_seconds),
        max: config.max,
    }
}

impl Default for ResolvedLiveness {
    fn default() -> Self {
        let (templates, _) =
            Templates::resolve(&TemplateOverrides::default(), STOPPED_REPLY, FAILURE_REPLY);
        Self {
            settings: LivenessSettings::default(),
            keep_alive: KeepAlive::default(),
            templates,
            conversations: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DekopondConfig {
    pub api_version: ConfigApiVersion,
    pub catalog_path: PathBuf,
    #[serde(default)]
    pub broker: BrokerConfig,
    pub transports: Vec<TransportConfig>,
    pub models: Vec<ModelConfig>,
    pub routes: Vec<RouteConfig>,
    #[serde(default)]
    pub stop_words: Option<Vec<String>>,
    #[serde(default)]
    pub sessions: SessionsConfig,
    #[serde(default)]
    pub shutdown_grace_ms: Option<u64>,
    #[serde(default)]
    pub telemetry: Option<TelemetryConfig>,
}

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

#[derive(Clone, Debug, Deserialize)]
#[serde(
    tag = "kind",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TransportConfig {
    SlackSocketMode {
        name: String,
        app_token_env: String,
        bot_token_env: String,
        #[serde(default)]
        experience: SlackExperience,
        #[serde(default)]
        liveness: LivenessConfig,
        #[serde(default)]
        endpoint: Option<String>,
    },
    DiscordGateway {
        name: String,
        bot_token_env: String,
        #[serde(default)]
        liveness: LivenessConfig,
        #[serde(default)]
        endpoint: Option<String>,
    },
    WhatsappCloudApi {
        name: String,
        app_secret_env: String,
        verify_token_env: String,
        access_token_env: String,
        bind: SocketAddr,
        callback_path: String,
        waba_id: String,
        phone_number_id: String,
        graph_api_version: String,
        #[serde(
            default = "default_whatsapp_debounce_ms",
            deserialize_with = "deserialize_collection_millis"
        )]
        debounce_ms: u32,
        #[serde(
            default = "default_whatsapp_debounce_max_wait_ms",
            deserialize_with = "deserialize_collection_millis"
        )]
        debounce_max_wait_ms: u32,
        #[serde(default)]
        liveness: LivenessConfig,
        #[serde(default)]
        graph_endpoint: Option<String>,
    },
    TelegramLongPoll {
        name: String,
        bot_token_env: String,
        #[serde(default)]
        liveness: LivenessConfig,
        #[serde(default)]
        endpoint: Option<String>,
    },
    Local {
        name: String,
        socket_path: PathBuf,
        #[serde(default)]
        liveness: LivenessConfig,
    },
}

const fn default_whatsapp_debounce_ms() -> u32 {
    5000
}

const fn default_whatsapp_debounce_max_wait_ms() -> u32 {
    15000
}

fn deserialize_collection_millis<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<u32, D::Error> {
    let millis = u32::deserialize(deserializer)?;
    if std::time::Instant::now()
        .checked_add(Duration::from_millis(u64::from(millis)))
        .is_none()
    {
        return Err(serde::de::Error::custom(
            "collection duration cannot be represented as a timer deadline",
        ));
    }
    Ok(millis)
}

impl TransportConfig {
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

    #[must_use]
    pub const fn chat_kind(&self) -> ChatTransportKind {
        match self {
            Self::SlackSocketMode { .. } => ChatTransportKind::Slack,
            Self::DiscordGateway { .. } => ChatTransportKind::Discord,
            Self::WhatsappCloudApi { .. } => ChatTransportKind::Whatsapp,
            Self::TelegramLongPoll { .. } => ChatTransportKind::Telegram,
            Self::Local { .. } => ChatTransportKind::Local,
        }
    }

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

#[derive(Clone, Debug, Deserialize)]
#[serde(
    tag = "kind",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ModelConfig {
    OpenaiCompatible {
        name: String,
        endpoint: String,
        model: String,
        #[serde(default)]
        api_key_env: Option<String>,
        timeout_ms: u64,
        #[serde(default = "default_model_stream")]
        stream: bool,
        #[serde(default)]
        classes: Vec<String>,
        #[serde(default)]
        modalities: Vec<Modality>,
    },
    Openrouter {
        name: String,
        model: String,
        api_key_env: String,
        timeout_ms: u64,
        #[serde(default)]
        classes: Vec<String>,
        #[serde(default)]
        modalities: Vec<Modality>,
        generation: Option<dekopon_model::openrouter::settings::Generation>,
        reasoning: Option<dekopon_model::openrouter::settings::Reasoning>,
        routing: Option<dekopon_model::openrouter::settings::Routing>,
        cache: Option<dekopon_model::openrouter::settings::Cache>,
    },
    ChatgptSubscription {
        name: String,
        model: String,
        #[serde(default)]
        auth_file: Option<PathBuf>,
        timeout_ms: u64,
        #[serde(default)]
        classes: Vec<String>,
        #[serde(default)]
        modalities: Vec<Modality>,
    },
}

const fn default_model_stream() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Modality {
    Image,
}

impl ModelConfig {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::OpenaiCompatible { name, .. }
            | Self::ChatgptSubscription { name, .. }
            | Self::Openrouter { name, .. } => name,
        }
    }

    #[must_use]
    pub fn accepts_images(&self) -> bool {
        match self {
            Self::OpenaiCompatible { modalities, .. }
            | Self::ChatgptSubscription { modalities, .. }
            | Self::Openrouter { modalities, .. } => modalities.contains(&Modality::Image),
        }
    }

    #[must_use]
    pub fn classes(&self) -> &[String] {
        match self {
            Self::OpenaiCompatible { classes, .. }
            | Self::ChatgptSubscription { classes, .. }
            | Self::Openrouter { classes, .. } => classes,
        }
    }

    fn timeout_ms(&self) -> u64 {
        match self {
            Self::OpenaiCompatible { timeout_ms, .. }
            | Self::ChatgptSubscription { timeout_ms, .. }
            | Self::Openrouter { timeout_ms, .. } => *timeout_ms,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ConversationMatchConfig {
    pub kind: ConversationKindMatch,
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub ids: Option<Vec<String>>,
}

impl ConversationMatchConfig {
    fn selector(&self) -> ConversationMatch {
        ConversationMatch {
            kind: self.kind.clone(),
            container: self.container.clone(),
            ids: self.ids.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RouteLimits {
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    #[serde(default = "default_max_capability_calls")]
    pub max_capability_calls: u32,
    #[serde(default)]
    pub max_duration_ms: Option<u64>,
    #[serde(default)]
    pub script_timeout_ms: Option<u64>,
}

impl Default for RouteLimits {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
            max_capability_calls: DEFAULT_MAX_CAPABILITY_CALLS,
            max_duration_ms: None,
            script_timeout_ms: None,
        }
    }
}

impl RouteLimits {
    #[must_use]
    pub const fn script_timeout(self) -> Duration {
        Duration::from_millis(match self.script_timeout_ms {
            Some(milliseconds) => milliseconds,
            None => DEFAULT_SCRIPT_TIMEOUT_MS,
        })
    }
}

const fn default_max_steps() -> u32 {
    DEFAULT_MAX_STEPS
}

const fn default_max_capability_calls() -> u32 {
    DEFAULT_MAX_CAPABILITY_CALLS
}

/// MemoryScope comes only from trusted route configuration; a transport kind, conversation kind,
/// inbound message, or model response can never select it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum MemoryScope {
    #[default]
    PrivateConversation,
    SharedConversation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(
    tag = "mode",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MemoryConfig {
    OneShot {},
    Persistent {
        #[serde(default)]
        scope: MemoryScope,
        #[serde(default = "default_idle_timeout_ms")]
        idle_timeout_ms: u64,
        #[serde(default = "default_conversation_max_turns")]
        max_turns: usize,
        #[serde(default = "default_conversation_max_bytes")]
        max_bytes: usize,
        #[serde(default)]
        recall: Option<RecallSource>,
        #[serde(default)]
        forget_after_ms: Option<u64>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum RecallSource {
    None,
    Journal,
    Platform,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self::Persistent {
            scope: MemoryScope::default(),
            idle_timeout_ms: default_idle_timeout_ms(),
            max_turns: default_conversation_max_turns(),
            max_bytes: default_conversation_max_bytes(),
            recall: None,
            forget_after_ms: None,
        }
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

// One week matches how long WhatsApp keeps an inbound media id, so recalled photos stay fetchable.
pub const DEFAULT_FORGET_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RouteConfig {
    pub transport: String,
    pub conversation: ConversationMatchConfig,
    /// Subjects only routes which agent hears a message; authority stays with Cedar and the
    /// broker's subject mapping, and it is restricted to direct-message routes since a channel
    /// version would read as an access list.
    #[serde(default)]
    pub subjects: Option<Vec<ExternalSubject>>,
    pub agent: AgentId,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(
        default,
        rename = "providerAttachments",
        deserialize_with = "refuse_provider_attachments"
    )]
    _retired_provider_attachments: (),
    #[serde(
        default,
        rename = "chatAssetInputs",
        deserialize_with = "refuse_chat_asset_inputs"
    )]
    _retired_chat_asset_inputs: (),
    #[serde(default)]
    pub improvement_suggestions: bool,
    /// Turning inspect_agent_config off only removes the structured config dump; the instructions
    /// stay in the system prompt regardless, so this is never a real secrecy gate against a
    /// determined user.
    #[serde(default = "default_true")]
    pub inspect_agent_config: bool,
    #[serde(default)]
    pub limits: RouteLimits,
    #[serde(default)]
    pub(crate) progress_detail: ProgressDetail,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub wakes: bool,
}

const fn default_true() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryWindow {
    pub scope: MemoryScope,
    pub idle_timeout: Duration,
    pub limits: HistoryLimits,
    pub recall: RecallSource,
    pub forget_after: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryPolicy {
    OneShot,
    Persistent(MemoryWindow),
}

impl MemoryPolicy {
    #[must_use]
    pub const fn window(self) -> Option<MemoryWindow> {
        match self {
            Self::OneShot => None,
            Self::Persistent(window) => Some(window),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedRoute {
    pub transport: String,
    pub conversation: ConversationMatch,
    pub subjects: Option<Vec<ExternalSubject>>,
    pub agent: AgentId,
    pub model: Option<String>,
    pub improvement_suggestions: bool,
    pub inspect_agent_config: bool,
    pub limits: RouteLimits,
    pub(crate) progress_detail: ProgressDetail,
    pub memory: MemoryPolicy,
    pub wakes: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WakesConfig {
    pub path: PathBuf,
    #[serde(default = "default_wake_max_per_subject")]
    pub max_per_subject: usize,
    #[serde(default = "default_wake_min_interval_ms")]
    pub min_interval_ms: u64,
    #[serde(default = "default_wake_max_horizon_ms")]
    pub max_horizon_ms: u64,
}

const fn default_wake_max_per_subject() -> usize {
    20
}

const fn default_wake_min_interval_ms() -> u64 {
    5 * 60 * 1_000
}

const fn default_wake_max_horizon_ms() -> u64 {
    30 * 24 * 60 * 60 * 1_000
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WakeBounds {
    pub max_per_subject: usize,
    pub min_interval: Duration,
    pub max_horizon: Duration,
}

#[derive(Clone, Debug)]
pub struct ResolvedWakes {
    pub path: PathBuf,
    pub bounds: WakeBounds,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct JournalConfig {
    pub path: PathBuf,
    pub max_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct ResolvedJournal {
    pub dir: PathBuf,
    pub max_bytes: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionsConfig {
    #[serde(default = "default_asset_retention_bytes")]
    pub asset_retention_bytes: usize,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_reply_on_busy")]
    pub reply_on_busy: bool,
    #[serde(default = "default_max_conversations")]
    pub max_conversations: usize,
    #[serde(default)]
    pub journal: Option<JournalConfig>,
    #[serde(default)]
    pub wakes: Option<WakesConfig>,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            asset_retention_bytes: DEFAULT_ASSET_RETENTION_BYTES,
            max_concurrent: DEFAULT_MAX_CONCURRENT_SESSIONS,
            reply_on_busy: true,
            max_conversations: DEFAULT_MAX_CONVERSATIONS,
            journal: None,
            wakes: None,
        }
    }
}

pub(crate) const DEFAULT_ASSET_RETENTION_BYTES: usize = 256 * 1024 * 1024;
const fn default_asset_retention_bytes() -> usize {
    DEFAULT_ASSET_RETENTION_BYTES
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

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TelemetryConfig {
    pub endpoint: String,
    pub transport: Transport,
    pub service_name: String,
    pub export_timeout_ms: u64,
}

#[derive(Clone, Debug)]
pub struct ResolvedTelemetry {
    pub settings: ExporterSettings,
}

#[derive(Clone, Debug)]
pub struct ResolvedBroker {
    pub socket_path: PathBuf,
    pub server_uid: u32,
    pub frame: FrameLimits,
}

#[derive(Debug)]
pub struct ResolvedConfig {
    pub source: PathBuf,
    pub catalog_path: PathBuf,
    pub broker: ResolvedBroker,
    pub transports: Vec<TransportConfig>,
    pub models: Vec<ModelConfig>,
    pub routes: Vec<ResolvedRoute>,
    pub(crate) liveness: BTreeMap<String, Arc<ResolvedLiveness>>,
    pub(crate) stop_words: Vec<String>,
    pub sessions: SessionsConfig,
    pub journal: Option<ResolvedJournal>,
    pub wakes: Option<ResolvedWakes>,
    pub shutdown_grace: Duration,
    pub telemetry: Option<ResolvedTelemetry>,
}

/// `Check` keeps going past a directory's fragment refusals so they are reported beside whatever
/// the rest of startup finds; every rule is otherwise the one boot applies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadMode {
    Boot,
    Check,
}

pub async fn load(
    path: impl AsRef<Path>,
    expected_uid: u32,
) -> Result<ResolvedConfig, ConfigError> {
    load_in(path, expected_uid, LoadMode::Boot)
        .await
        .map(|(resolved, _)| resolved)
}

pub async fn load_in(
    path: impl AsRef<Path>,
    expected_uid: u32,
    mode: LoadMode,
) -> Result<(ResolvedConfig, Vec<ConfigError>), ConfigError> {
    let path = absolute(path.as_ref())?;
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return load_directory(path, expected_uid, mode).await;
    }
    let config = decode(&read_config_file(path.clone(), expected_uid).await?)?;
    let resolved = resolve(
        config,
        path,
        &BrokerSocketDiscovery::from_process(None),
        expected_uid,
    )?;
    Ok((resolved, Vec::new()))
}

const MERGE_RULES: MergeRules = MergeRules {
    merged_by_name: &[],
    concatenated: &["transports", "models", "routes"],
};

/// Routes match first-to-last within one transport, so a route must sit in the fragment that
/// defines its transport; otherwise renaming a file could reorder them.
async fn load_directory(
    directory: PathBuf,
    expected_uid: u32,
    mode: LoadMode,
) -> Result<(ResolvedConfig, Vec<ConfigError>), ConfigError> {
    let paths = fragments::scan_directory(&directory, expected_uid, "yaml")?;
    let first = paths
        .first()
        .cloned()
        .ok_or_else(|| ConfigError::EmptyConfigDirectory {
            path: directory.clone(),
        })?;
    let mut parsed = Vec::with_capacity(paths.len());
    let mut defined = BTreeMap::<String, PathBuf>::new();
    let mut referenced = Vec::<(PathBuf, String)>::new();
    for path in paths {
        let bytes = read_config_file(path.clone(), expected_uid).await?;
        let mapping = serde_yaml::from_slice::<serde_yaml::Mapping>(&bytes).map_err(|source| {
            ConfigError::DecodeFragment {
                path: path.clone(),
                source,
            }
        })?;
        for transport in names_under(&mapping, "transports", "name") {
            defined.insert(transport, path.clone());
        }
        for transport in names_under(&mapping, "routes", "transport") {
            referenced.push((path.clone(), transport));
        }
        parsed.push((path, mapping));
    }
    let stray = referenced
        .into_iter()
        .filter(|(path, transport)| defined.get(transport).is_some_and(|owner| owner != path))
        .map(|(path, transport)| format!("{transport} in {}", path.display()))
        .collect::<Vec<_>>();
    let mut refusals = Vec::new();
    if !stray.is_empty() {
        let refusal = ConfigError::RouteOutsideTransportFragment { routes: stray };
        if mode == LoadMode::Boot {
            return Err(refusal);
        }
        refusals.push(refusal);
    }
    let (merged, refusal) = fragments::merge_reporting(parsed, &MERGE_RULES);
    if let Some(refusal) = refusal {
        if mode == LoadMode::Boot {
            return Err(refusal.into());
        }
        refusals.push(refusal.into());
    }
    // A later refusal is reported as the fragment refusal it may well be caused by.
    let later =
        |error: ConfigError, mut refusals: Vec<ConfigError>| refusals.pop().unwrap_or(error);
    let config = match serde_yaml::from_value::<DekopondConfig>(serde_yaml::Value::Mapping(merged))
    {
        Ok(config) => config,
        Err(source) => return Err(later(ConfigError::Decode { source }, refusals)),
    };
    match resolve(
        config,
        first,
        &BrokerSocketDiscovery::from_process(None),
        expected_uid,
    ) {
        Ok(resolved) => Ok((resolved, refusals)),
        Err(error) => Err(later(error, refusals)),
    }
}

fn decode(document: &[u8]) -> Result<DekopondConfig, ConfigError> {
    serde_yaml::from_slice::<DekopondConfig>(document)
        .map_err(|source| ConfigError::Decode { source })
}

fn names_under(mapping: &serde_yaml::Mapping, list: &str, field: &str) -> Vec<String> {
    mapping
        .get(list)
        .and_then(serde_yaml::Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get(field)?.as_str().map(str::to_owned))
        .collect()
}

async fn read_config_file(path: PathBuf, expected_uid: u32) -> Result<Vec<u8>, ConfigError> {
    // This file isn't a secret, since credentials live in the environment and transport credential
    // files, so the bar here is only that nobody else can rewrite it.
    let owned = path.clone();
    tokio::task::spawn_blocking(move || {
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

    let mut transports_incomplete = config.transports.is_empty();
    let mut transport_names = BTreeSet::new();
    let mut transport_kinds: BTreeMap<String, ChatTransportKind> = BTreeMap::new();
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
        transport_kinds.insert(name.clone(), transport.chat_kind());
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
                debounce_ms,
                debounce_max_wait_ms,
                liveness,
                graph_endpoint,
            } => {
                check_env_name(&app_secret_env, &mut problems);
                check_env_name(&verify_token_env, &mut problems);
                check_env_name(&access_token_env, &mut problems);
                if debounce_ms > 0 && debounce_max_wait_ms < debounce_ms {
                    problems.push(ConfigProblem::InvalidWhatsappDebounce { name: name.clone() });
                }
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
                    debounce_ms,
                    debounce_max_wait_ms,
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
            problems.push(ConfigProblem::InvalidModelTimeout { name: name.clone() });
        }
        if let ModelConfig::Openrouter {
            model: id,
            generation,
            reasoning,
            routing,
            cache,
            ..
        } = model
        {
            if id.trim().is_empty() {
                problems.push(ConfigProblem::EmptyModelId { name: name.clone() });
            }
            let settings = dekopon_model::openrouter::settings::Settings {
                generation: generation.clone(),
                reasoning: reasoning.clone(),
                routing: routing.clone(),
                cache: cache.clone(),
            };
            problems.extend(settings.problems().into_iter().map(|problem| {
                ConfigProblem::OpenRouterSetting {
                    name: name.clone(),
                    problem,
                }
            }));
        }
        if let ModelConfig::OpenaiCompatible {
            api_key_env: Some(variable),
            ..
        }
        | ModelConfig::Openrouter {
            api_key_env: variable,
            ..
        } = model
        {
            check_env_name(variable, &mut problems);
        }
    }

    let mut routes = Vec::with_capacity(config.routes.len());
    for (index, route) in config.routes.into_iter().enumerate() {
        if !transports_incomplete && !transport_names.contains(&route.transport) {
            problems.push(ConfigProblem::UnknownRouteTransport {
                transport: route.transport.clone(),
            });
        }
        let conversation = route.conversation.selector();
        if let Some(chat_kind) = transport_kinds.get(&route.transport) {
            for problem in conversation.validate(*chat_kind) {
                problems.push(ConfigProblem::InvalidRouteConversation {
                    route: index,
                    problem,
                });
            }
        }
        if route.progress_detail == ProgressDetail::Off
            && let (Some(liveness), Some(chat_kind)) = (
                liveness_settings.get(&route.transport),
                transport_kinds.get(&route.transport),
            )
            && [
                ConversationKind::DirectMessage,
                ConversationKind::GroupDirectMessage,
                ConversationKind::Channel,
                ConversationKind::Thread,
            ]
            .into_iter()
            .filter(|kind| chat_kind.produces(*kind) && conversation.kind.contains(*kind))
            .any(|kind| {
                let (settings, _) = liveness.for_kind(kind);
                settings.cancel_button && !settings.stream
            })
        {
            problems.push(ConfigProblem::UnsupportedLivenessSurface {
                transport: route.transport.clone(),
                surface: "cancelButton",
                reason: "progressDetail: off requires streaming for a message-backed Stop control",
            });
        }
        let direct_message_only = conversation.kind
            == ConversationKindMatch::Kinds(vec![ConversationKind::DirectMessage]);
        if route.subjects.is_some() && !direct_message_only {
            problems.push(ConfigProblem::SubjectsOnNonDmRoute { route: index });
        }
        if route
            .subjects
            .as_ref()
            .is_some_and(|subjects| subjects.is_empty())
        {
            problems.push(ConfigProblem::EmptyRouteSubjects { route: index });
        }
        if direct_message_only
            && matches!(
                route.memory,
                MemoryConfig::Persistent {
                    scope: MemoryScope::SharedConversation,
                    ..
                }
            )
        {
            problems.push(ConfigProblem::SharedMemoryOnDmRoute { route: index });
        }
        if let Some(model) = &route.model
            && !models_incomplete
            && !model_names.contains(model)
        {
            problems.push(ConfigProblem::UnknownRouteModel {
                model: model.clone(),
            });
        }
        if route.limits.max_steps == 0 || route.limits.max_capability_calls == 0 {
            problems.push(ConfigProblem::InvalidRouteLimits {
                agent: route.agent.to_string(),
            });
        }
        if route.limits.max_duration_ms == Some(0) {
            problems.push(ConfigProblem::InvalidRouteDuration {
                agent: route.agent.to_string(),
            });
        }
        if route.limits.script_timeout_ms == Some(0) {
            problems.push(ConfigProblem::InvalidScriptTimeout {
                agent: route.agent.to_string(),
            });
        }
        if let (Some(script_timeout_ms), Some(max_duration_ms)) =
            (route.limits.script_timeout_ms, route.limits.max_duration_ms)
            && script_timeout_ms > max_duration_ms
        {
            problems.push(ConfigProblem::ScriptTimeoutAboveDuration {
                agent: route.agent.to_string(),
                script_timeout_ms,
                max_duration_ms,
            });
        }
        let memory = match route.memory {
            MemoryConfig::OneShot {} => MemoryPolicy::OneShot,
            MemoryConfig::Persistent {
                scope,
                idle_timeout_ms,
                max_turns,
                max_bytes,
                recall,
                forget_after_ms,
            } => {
                if idle_timeout_ms == 0
                    || max_turns == 0
                    || max_bytes == 0
                    || forget_after_ms == Some(0)
                {
                    problems.push(ConfigProblem::InvalidMemoryBounds {
                        agent: route.agent.to_string(),
                    });
                }
                let journaled = config.sessions.journal.is_some();
                let recall = recall.unwrap_or(if journaled {
                    RecallSource::Journal
                } else {
                    RecallSource::None
                });
                match recall {
                    RecallSource::Journal if !journaled => {
                        problems.push(ConfigProblem::JournalRecallWithoutJournal { route: index });
                    }
                    RecallSource::Platform => {
                        if let Some(kind) = transport_kinds.get(&route.transport)
                            && !matches!(
                                kind,
                                ChatTransportKind::Slack | ChatTransportKind::Discord
                            )
                        {
                            problems.push(ConfigProblem::PlatformRecallUnsupported {
                                route: index,
                                kind: *kind,
                            });
                        }
                    }
                    RecallSource::None if forget_after_ms.is_some() => {
                        problems.push(ConfigProblem::ForgetAfterWithoutRecall { route: index });
                    }
                    RecallSource::None | RecallSource::Journal => {}
                }
                MemoryPolicy::Persistent(MemoryWindow {
                    scope,
                    idle_timeout: Duration::from_millis(idle_timeout_ms),
                    limits: HistoryLimits {
                        max_turns,
                        max_bytes,
                    },
                    recall,
                    forget_after: forget_after_ms
                        .map_or(DEFAULT_FORGET_AFTER, Duration::from_millis),
                })
            }
        };
        if route.wakes {
            match &config.sessions.wakes {
                None => problems.push(ConfigProblem::WakesWithoutStore { route: index }),
                Some(wakes)
                    if Duration::from_millis(wakes.min_interval_ms)
                        <= route.limits.script_timeout() =>
                {
                    problems.push(ConfigProblem::WakeIntervalWithinScriptTimeout {
                        route: index,
                        min_interval_ms: wakes.min_interval_ms,
                        script_timeout_ms: route
                            .limits
                            .script_timeout_ms
                            .unwrap_or(DEFAULT_SCRIPT_TIMEOUT_MS),
                    });
                }
                Some(_) => {}
            }
        }
        routes.push(ResolvedRoute {
            transport: route.transport,
            conversation,
            subjects: route.subjects,
            agent: route.agent,
            model: route.model,
            improvement_suggestions: route.improvement_suggestions,
            inspect_agent_config: route.inspect_agent_config,
            limits: route.limits,
            progress_detail: route.progress_detail,
            memory,
            wakes: route.wakes,
        });
    }

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
    let wakes = config.sessions.wakes.as_ref().map(|wakes| {
        if wakes.max_per_subject == 0 || wakes.min_interval_ms == 0 || wakes.max_horizon_ms == 0 {
            problems.push(ConfigProblem::InvalidWakeBounds);
        }
        ResolvedWakes {
            path: resolve_path(wakes.path.clone()),
            bounds: WakeBounds {
                max_per_subject: wakes.max_per_subject,
                min_interval: Duration::from_millis(wakes.min_interval_ms),
                max_horizon: Duration::from_millis(wakes.max_horizon_ms),
            },
        }
    });
    let journal = config.sessions.journal.as_ref().map(|journal| {
        if journal.max_bytes == 0 {
            problems.push(ConfigProblem::InvalidJournalBytes);
        }
        ResolvedJournal {
            dir: resolve_path(journal.path.clone()),
            max_bytes: journal.max_bytes,
        }
    });
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
                journal,
                wakes,
                shutdown_grace,
                telemetry,
            })
        }
        _ => Err(ConfigError::Invalid {
            path: source,
            problems,
        }),
    }
}

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

    if liveness.mode == LivenessMode::Off {
        for surface in [
            (liveness.progress == ProgressSurface::Message).then_some("progress"),
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

    let whatsapp = matches!(transport, TransportConfig::WhatsappCloudApi { .. });
    let slack_agent = experience == Some(SlackExperience::Agent);
    fn unsupported_surfaces(
        problems: &mut Vec<ConfigProblem>,
        transport: &str,
        whatsapp: bool,
        slack_agent: bool,
        stream: bool,
        cancel_button: bool,
    ) {
        if whatsapp {
            for surface in [
                stream.then_some("stream"),
                cancel_button.then_some("cancelButton"),
            ]
            .into_iter()
            .flatten()
            {
                problems.push(ConfigProblem::UnsupportedLivenessSurface {
                    transport: transport.to_owned(),
                    surface,
                    reason:
                        "WhatsApp messages cannot be edited and carry no interactive components",
                });
            }
        }
        if slack_agent && cancel_button {
            problems.push(ConfigProblem::UnsupportedLivenessSurface {
                transport: transport.to_owned(),
                surface: "cancelButton",
                reason: "Slack's Agent experience renders its own Stop control",
            });
        }
    }
    unsupported_surfaces(
        problems,
        &name,
        whatsapp,
        slack_agent,
        liveness.stream,
        liveness.cancel_button,
    );
    let chat_kind = transport.chat_kind();
    for (kind, overlay) in &liveness.conversations {
        if !chat_kind.produces(*kind) {
            problems.push(ConfigProblem::ImpossibleLivenessConversation {
                transport: name.clone(),
                kind: kind.as_str(),
            });
        }
        unsupported_surfaces(
            problems,
            &name,
            whatsapp,
            slack_agent,
            overlay.stream == Some(true),
            overlay.cancel_button == Some(true),
        );
        if let Some(keep_alive) = &overlay.keep_alive
            && (keep_alive.every_seconds == 0 || keep_alive.at_seconds.contains(&0))
        {
            problems.push(ConfigProblem::InvalidKeepAlive {
                transport: name.clone(),
            });
        }
    }

    match experience {
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

    let resolved = ResolvedLiveness {
        settings: liveness.settings(),
        keep_alive: keep_alive_from(&liveness.keep_alive),
        templates,
        conversations: liveness.conversations.clone(),
    };
    if [
        ConversationKind::DirectMessage,
        ConversationKind::GroupDirectMessage,
        ConversationKind::Channel,
        ConversationKind::Thread,
    ]
    .into_iter()
    .filter(|kind| chat_kind.produces(*kind))
    .any(|kind| {
        let (settings, _) = resolved.for_kind(kind);
        settings.cancel_button && !settings.stream && settings.progress == ProgressSurface::Off
    }) {
        problems.push(ConfigProblem::UnsupportedLivenessSurface {
            transport: name,
            surface: "cancelButton",
            reason: "a message-backed Stop control requires progress or streaming",
        });
    }
    resolved
}

fn check_env_name(name: &str, problems: &mut Vec<ConfigProblem>) {
    if let Err(problem) = validate_env_name(name) {
        problems.push(problem);
    }
}

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

/// Validates a variable name only, never a secret, so it is safe to echo in error messages; the
/// grammar is narrower than the OS's, rejecting a stray space that would otherwise fail cryptically
/// at connect time.
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

/// The loopback-only override lets tests point a transport at a mock without letting the same
/// override send a bot token to an arbitrary host, and the host is checked after stripping
/// userinfo.
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
    // Userinfo before the @ and any path after the slash aren't the actual connection host; both
    // are how a remote address can disguise itself as loopback.
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
        path: PathBuf,
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
    #[error("gateway configuration fragment {path} is not strict valid YAML")]
    DecodeFragment {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("configuration directory {path} holds no *.yaml fragment")]
    EmptyConfigDirectory { path: PathBuf },
    #[error(transparent)]
    Fragments(#[from] FragmentError),
    #[error(
        "routes must sit in the fragment that defines their transport; move: {}",
        routes.join("; ")
    )]
    RouteOutsideTransportFragment { routes: Vec<String> },
    #[error("configured path has no parent")]
    MissingParent,
    #[error("{path}: {}", render_problems(.problems))]
    Invalid {
        path: PathBuf,
        problems: Vec<ConfigProblem>,
    },
}

#[derive(Debug, Error)]
pub enum ConfigProblem {
    #[error("model {name:?} requires a nonempty model identifier")]
    EmptyModelId { name: String },
    #[error("model {name:?}: {problem}")]
    OpenRouterSetting {
        name: String,
        problem: dekopon_model::openrouter::settings::SettingsProblem,
    },
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
    #[error(
        "route for agent {agent:?} sets limits.scriptTimeoutMs to 0, which ends every script the instant it starts; omit it for the {DEFAULT_SCRIPT_TIMEOUT_MS}ms default"
    )]
    InvalidScriptTimeout { agent: String },
    #[error(
        "route for agent {agent:?} sets limits.scriptTimeoutMs to {script_timeout_ms} above limits.maxDurationMs {max_duration_ms}; the session bound is reached first, so that script deadline can never take effect"
    )]
    ScriptTimeoutAboveDuration {
        agent: String,
        script_timeout_ms: u64,
        max_duration_ms: u64,
    },
    #[error("WhatsApp transport {name:?} must bind an explicit nonzero port")]
    InvalidWhatsappBind { name: String },
    #[error("WhatsApp transport {name:?} must use canonical positive WABA and phone-number IDs")]
    InvalidWhatsappScope { name: String },
    #[error("WhatsApp transport {name:?} has an invalid callback path")]
    InvalidWhatsappCallback { name: String },
    #[error("WhatsApp transport {name:?} must pin a Graph API version such as v23.0")]
    InvalidWhatsappGraphVersion { name: String },
    #[error(
        "WhatsApp transport {name} requires debounceMaxWaitMs >= debounceMs when collection is enabled"
    )]
    InvalidWhatsappDebounce { name: String },
    #[error("route names unknown transport {transport:?}")]
    UnknownRouteTransport { transport: String },
    #[error("route names unknown model {model:?}")]
    UnknownRouteModel { model: String },
    #[error("route for agent {agent:?} must allow at least one step and one capability call")]
    InvalidRouteLimits { agent: String },
    #[error("session bounds must be greater than zero")]
    InvalidSessionLimits,
    #[error(
        "route for agent {agent:?} declares a persistent memory window with a zero bound; its idle timeout, turn window, and byte window must each be greater than zero"
    )]
    InvalidMemoryBounds { agent: String },
    #[error("routes[{route}]: conversation selector is invalid: {problem}")]
    InvalidRouteConversation {
        route: usize,
        problem: ConversationMatchProblem,
    },
    #[error(
        "routes[{route}]: `subjects:` is accepted only beside `conversation: {{ kind: [directMessage] }}`; a per-person channel route is an access-control list by another name, and authority is the broker's subject mapping and policy"
    )]
    SubjectsOnNonDmRoute { route: usize },
    #[error(
        "routes[{route}]: `subjects:` is empty, which answers nobody; omit it to answer every subject"
    )]
    EmptyRouteSubjects { route: usize },
    #[error(
        "routes[{route}]: `memory.scope: sharedConversation` on a `kind: [directMessage]` route shares nothing; the direct message already is the subject"
    )]
    SharedMemoryOnDmRoute { route: usize },
    #[error(
        "transport {transport:?} overrides liveness for conversation kind {kind}, which it never produces"
    )]
    ImpossibleLivenessConversation {
        transport: String,
        kind: &'static str,
    },
    #[error(
        "sessions.maxConversations must be greater than zero; a zero ceiling evicts every conversation immediately and turns a persistent route into an expensive one-shot one"
    )]
    InvalidMaxConversations,
    #[error(
        "sessions.journal.maxBytes must be greater than zero; omit sessions.journal to keep nothing on disk"
    )]
    InvalidJournalBytes,
    #[error("routes[{route}]: `wakes: true` needs `sessions.wakes`")]
    WakesWithoutStore { route: usize },
    #[error(
        "sessions.wakes.maxPerSubject, minIntervalMs and maxHorizonMs must be greater than zero; omit sessions.wakes to turn wakes off"
    )]
    InvalidWakeBounds,
    #[error(
        "routes[{route}]: sessions.wakes.minIntervalMs ({min_interval_ms}) must exceed the route's script timeout ({script_timeout_ms} ms), or one watch's checks could overlap"
    )]
    WakeIntervalWithinScriptTimeout {
        route: usize,
        min_interval_ms: u64,
        script_timeout_ms: u64,
    },
    #[error("routes[{route}]: `memory.recall: journal` needs `sessions.journal`")]
    JournalRecallWithoutJournal { route: usize },
    #[error(
        "routes[{route}]: `memory.recall: platform` needs a chat service with a history API; {kind} has none, so use `journal`"
    )]
    PlatformRecallUnsupported {
        route: usize,
        kind: ChatTransportKind,
    },
    #[error(
        "routes[{route}]: `memory.forgetAfterMs` bounds recall, and this route recalls nothing; set `recall` or remove it"
    )]
    ForgetAfterWithoutRecall { route: usize },
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

fn refuse_provider_attachments<'de, D: serde::Deserializer<'de>>(_: D) -> Result<(), D::Error> {
    Err(serde::de::Error::custom(
        "providerAttachments was removed; delivery requires the broker-authorized asset.send capability",
    ))
}
fn refuse_chat_asset_inputs<'de, D: serde::Deserializer<'de>>(_: D) -> Result<(), D::Error> {
    Err(serde::de::Error::custom(
        "chatAssetInputs was removed; descriptor-backed references resolve automatically for every proposal; the broker still authorizes each capability and HTTP effect",
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use dekopon_broker_protocol::BrokerSocketDiscovery;

    use super::{
        ConfigError, DEFAULT_SCRIPT_TIMEOUT_MS, Duration, LivenessMode, ModelConfig,
        ProgressDetail, ProgressSurface, SlackLivenessFallback, resolve,
    };
    use crate::progress::{DEFAULT_KEEP_ALIVE_MAX, KeepAlive};

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
             \x20   conversation: { kind: [directMessage] }\n\
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

    #[test]
    fn auto_defaults_preserve_disabled_liveness_and_explicit_overrides() {
        use dekopon_broker_protocol::ConversationKind;
        for (block, mode, progress) in [
            ("", LivenessMode::Off, ProgressSurface::Auto),
            (
                "liveness: { mode: native }",
                LivenessMode::Native,
                ProgressSurface::Auto,
            ),
            (
                "liveness: { mode: native, progress: message }",
                LivenessMode::Native,
                ProgressSurface::Message,
            ),
            (
                "liveness: { mode: native, progress: off }",
                LivenessMode::Native,
                ProgressSurface::Off,
            ),
            (
                "liveness: { mode: native, progress: auto, conversations: { directMessage: { progress: message } } }",
                LivenessMode::Native,
                ProgressSurface::Message,
            ),
            (
                "liveness: { mode: native, progress: message, conversations: { directMessage: { progress: auto } } }",
                LivenessMode::Native,
                ProgressSurface::Auto,
            ),
        ] {
            let config = resolved(&format!(
                "transports:\n  - name: dev\n    kind: local\n    socketPath: dev.sock\n    {block}\nroutes:\n  - transport: dev\n    conversation: {{ kind: [directMessage] }}\n    agent: reviewer\n"
            )).expect("defaults and explicit settings resolve");
            let (settings, _) = config.liveness["dev"].for_kind(ConversationKind::DirectMessage);
            assert_eq!(settings.mode, mode, "{block}");
            assert_eq!(settings.progress, progress, "{block}");
            assert!(!settings.stream);
        }
    }

    #[test]
    fn stop_controls_require_an_effective_message_surface() {
        for (block, detail, valid) in [
            ("progress: auto, cancelButton: true", "plain", true),
            (
                "progress: off, cancelButton: true, conversations: { directMessage: { progress: message }, groupDirectMessage: { progress: message }, channel: { progress: message }, thread: { progress: message } }",
                "plain",
                true,
            ),
            ("progress: off, cancelButton: true", "plain", false),
            ("progress: auto, cancelButton: true", "off", false),
            ("progress: message, cancelButton: true", "off", false),
            (
                "progress: off, cancelButton: true, stream: true",
                "off",
                true,
            ),
            (
                "progress: auto, conversations: { directMessage: { cancelButton: true } }",
                "off",
                false,
            ),
            (
                "progress: auto, cancelButton: true, conversations: { directMessage: { progress: off } }",
                "plain",
                false,
            ),
            (
                "progress: auto, cancelButton: true, conversations: { directMessage: { stream: true } }",
                "off",
                true,
            ),
        ] {
            let result = resolved(&format!(
                "transports:\n  - name: dev\n    kind: local\n    socketPath: dev.sock\n    liveness: {{ mode: native, {block} }}\nroutes:\n  - transport: dev\n    conversation: {{ kind: [directMessage] }}\n    agent: reviewer\n    progressDetail: {detail}\n"
            ));
            assert_eq!(result.is_ok(), valid, "{block} / {detail}: {result:?}");
            if let Err(error) = result {
                assert!(
                    error.to_string().contains("message-backed Stop control"),
                    "{error}"
                );
            }
        }
        let error = resolved(
            "transports:\n  - name: dev\n    kind: local\n    socketPath: dev.sock\n    liveness: { mode: native, progress: off, cancelButton: true }\nroutes:\n  - transport: dev\n    conversation: { kind: [directMessage] }\n    agent: reviewer\n    progressDetail: off\n"
        ).expect_err("both incompatible settings must be reported");
        let rendered = error.to_string();
        assert!(
            rendered.contains("requires progress or streaming"),
            "{rendered}"
        );
        assert!(
            rendered.contains("progressDetail: off requires streaming"),
            "{rendered}"
        );
    }

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
             \x20   conversation: { kind: [directMessage] }\n\
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
             \x20   conversation: { kind: [directMessage] }\n\
             \x20   agent: reviewer\n",
        )
        .expect("a well-formed configuration resolves");

        assert_eq!(
            config.stop_words,
            vec!["stop".to_owned(), "basta".to_owned()]
        );
    }

    const HEAD: &str = "apiVersion: dekopon.dev/dekopond/v1alpha1\n\
         catalogPath: dekopon.yaml\n\
         broker: { socketPath: /run/dekopon/broker.sock, serverUid: 501 }\n";
    const TAIL: &str = "transports:\n\
         \x20 - name: dev\n\
         \x20   kind: local\n\
         \x20   socketPath: dev.sock\n\
         routes:\n\
         \x20 - transport: dev\n\
         \x20   conversation: { kind: [directMessage] }\n\
         \x20   agent: reviewer\n";

    fn router_document(kind: &str, blocks: &str) -> String {
        format!(
            "{HEAD}models:\n  - name: router\n    kind: {kind}\n    model: any/model\n    apiKeyEnv: OPENROUTER_API_KEY\n    timeoutMs: 1000\n{blocks}{TAIL}"
        )
    }

    #[test]
    fn openrouter_defaults_and_every_authored_block_decode_without_new_legacy_keys() {
        let default = super::decode(router_document("openrouter", "").as_bytes()).unwrap();
        assert!(
            matches!(&default.models[0], ModelConfig::Openrouter { classes, modalities, generation: None, reasoning: None, routing: None, cache: None, .. } if classes.is_empty() && modalities.is_empty())
        );
        let authored = super::decode(router_document("openrouter", "    classes: [general]\n    modalities: [image]\n    generation: {maxOutputTokens: 4096, temperature: 2.0, topP: 1.0}\n    reasoning: {effort: max}\n    routing: {allowFallbacks: false, requireParameters: true, only: [alpha]}\n    cache: {style: explicitPrefix, ttl: 1h}\n").as_bytes()).unwrap();
        assert!(authored.models[0].accepts_images());
        assert_eq!(authored.models[0].classes(), &["general"]);
        assert!(
            resolve(
                authored,
                PathBuf::from("/tmp/gateway.yaml"),
                &BrokerSocketDiscovery::new(None, None, Some(PathBuf::from("/run/user/501")), None),
                501
            )
            .is_ok()
        );
        assert!(matches!(
            super::decode(router_document("openRouter", "").as_bytes()),
            Err(ConfigError::Decode { .. })
        ));
        for field in [
            "endpoint: https://example.test",
            "stream: false",
            "authFile: secret",
            "generation: {extra: 1}",
            "reasoning: {effort: medium, extra: 1}",
            "routing: {extra: true}",
            "cache: {style: automatic, extra: 1}",
            "reasoning: {}",
            "cache: {}",
            "reasoning: {effort: extreme}",
            "cache: {style: explicitPrefix, ttl: 2h}",
            "generation: {maxOutputTokens: -1}",
            "generation: {maxOutputTokens: 0}",
        ] {
            assert!(
                matches!(
                    super::decode(
                        router_document("openrouter", &format!("    {field}\n")).as_bytes()
                    ),
                    Err(ConfigError::Decode { .. })
                ),
                "{field}"
            );
        }
        for kind in ["chatgptSubscription", "openaiCompatible"] {
            for block in [
                "generation: {}",
                "reasoning: {effort: medium}",
                "routing: {}",
                "cache: {style: automatic}",
            ] {
                let mut document = router_document(kind, &format!("    {block}\n"));
                if kind == "chatgptSubscription" {
                    document = document.replace("    apiKeyEnv: OPENROUTER_API_KEY\n", "");
                } else {
                    document = document.replace(
                        "    timeoutMs:",
                        "    endpoint: http://127.0.0.1:1234/v1\n    timeoutMs:",
                    );
                }
                assert!(
                    matches!(
                        super::decode(document.as_bytes()),
                        Err(ConfigError::Decode { .. })
                    ),
                    "{kind} {block}"
                );
            }
        }
    }

    #[test]
    fn openrouter_collects_all_numeric_routing_and_cache_problems_together() {
        let document = router_document(
            "openrouter",
            "    generation: {temperature: .nan, topP: 0.0}\n    routing: {only: [alpha, ' ']}\n    cache: {style: automatic, ttl: 5m}\n",
        );
        let config = super::decode(document.as_bytes()).unwrap();
        let error = resolve(
            config,
            PathBuf::from("/tmp/gateway.yaml"),
            &BrokerSocketDiscovery::new(None, None, Some(PathBuf::from("/run/user/501")), None),
            501,
        )
        .unwrap_err();
        let ConfigError::Invalid { problems, .. } = error else {
            panic!("expected collected semantic problems");
        };
        let actual = problems
            .iter()
            .filter_map(|problem| match problem {
                super::ConfigProblem::OpenRouterSetting { problem, .. } => Some(*problem),
                _ => None,
            })
            .collect::<Vec<_>>();
        use dekopon_model::openrouter::settings::SettingsProblem::*;
        assert_eq!(actual, vec![Temperature, TopP, EmptyProvider, AutomaticTtl]);
    }

    #[test]
    fn openrouter_rejects_an_empty_model_empty_only_and_nonfinite_controls() {
        for (blocks, expected) in [
            (
                "    generation: {temperature: .inf}\n",
                dekopon_model::openrouter::settings::SettingsProblem::Temperature,
            ),
            (
                "    generation: {topP: .nan}\n",
                dekopon_model::openrouter::settings::SettingsProblem::TopP,
            ),
            (
                "    routing: {only: []}\n",
                dekopon_model::openrouter::settings::SettingsProblem::EmptyOnly,
            ),
        ] {
            let config = super::decode(
                router_document("openrouter", blocks)
                    .replace("model: any/model", "model: ' '")
                    .as_bytes(),
            )
            .unwrap();
            let error = resolve(
                config,
                PathBuf::from("/tmp/gateway.yaml"),
                &BrokerSocketDiscovery::new(None, None, Some(PathBuf::from("/run/user/501")), None),
                501,
            )
            .unwrap_err();
            let ConfigError::Invalid { problems, .. } = error else {
                panic!("expected semantic problems");
            };
            assert!(
                problems
                    .iter()
                    .any(|problem| matches!(problem, super::ConfigProblem::EmptyModelId { .. }))
            );
            assert!(problems.iter().any(|problem| matches!(problem, super::ConfigProblem::OpenRouterSetting { problem, .. } if *problem == expected)));
        }
    }

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
                ModelConfig::ChatgptSubscription { .. } | ModelConfig::Openrouter { .. } => None,
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

    #[test]
    fn a_route_script_deadline_is_read_and_otherwise_defaults() {
        let config = resolved(
            "transports:\n\
             \x20 - name: dev\n\
             \x20   kind: local\n\
             \x20   socketPath: dev.sock\n\
             routes:\n\
             \x20 - transport: dev\n\
             \x20   conversation: { kind: [directMessage] }\n\
             \x20   agent: reviewer\n\
             \x20 - transport: dev\n\
             \x20   conversation: { kind: [channel, thread] }\n\
             \x20   agent: reviewer\n\
             \x20   limits: { scriptTimeoutMs: 240000, maxDurationMs: 300000 }\n",
        )
        .expect("a well-formed configuration resolves");

        let written = config.routes.first().expect("the route without the field");
        assert_eq!(written.limits.script_timeout_ms, None);
        assert_eq!(
            written.limits.script_timeout(),
            Duration::from_millis(DEFAULT_SCRIPT_TIMEOUT_MS),
            "an omitted deadline resolves to the documented 30 seconds"
        );
        let named = config.routes.get(1).expect("the route that named one");
        assert_eq!(named.limits.script_timeout_ms, Some(240_000));
        assert_eq!(
            named.limits.script_timeout(),
            Duration::from_millis(240_000)
        );
    }

    #[test]
    fn script_deadlines_that_cannot_take_effect_are_reported_together() {
        let error = resolved(
            "transports:\n\
             \x20 - name: dev\n\
             \x20   kind: local\n\
             \x20   socketPath: dev.sock\n\
             routes:\n\
             \x20 - transport: dev\n\
             \x20   conversation: { kind: [directMessage] }\n\
             \x20   agent: reviewer\n\
             \x20   limits: { scriptTimeoutMs: 0 }\n\
             \x20 - transport: dev\n\
             \x20   conversation: { kind: [channel, thread] }\n\
             \x20   agent: other-reviewer\n\
             \x20   limits: { scriptTimeoutMs: 300000, maxDurationMs: 120000 }\n",
        )
        .expect_err("neither route may start");

        let rendered = error.to_string();
        for expected in [
            "sets limits.scriptTimeoutMs to 0",
            "omit it for the 30000ms default",
            "sets limits.scriptTimeoutMs to 300000 above limits.maxDurationMs 120000",
            "can never take effect",
        ] {
            assert!(
                rendered.contains(expected),
                "both problems are reported at once; {expected:?} is missing from:\n{rendered}"
            );
        }
    }
}
