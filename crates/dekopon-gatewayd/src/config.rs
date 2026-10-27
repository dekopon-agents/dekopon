//! This file is checked as strictly as the broker's own config, since it names agent-reachable
//! models and credential env vars, so a writable or symlinked copy could redirect the daemon.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    env, io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use dekopon_agent::prompt::HistoryLimits;
use dekopon_broker_protocol::{
    BrokerClient, BrokerSocketDiscovery, ChatTransportKind, ClientError, ConversationKind,
    ConversationKindMatch, ConversationMatch, ConversationMatchProblem, DEFAULT_IO_TIMEOUT,
    DEFAULT_MAX_FRAME_BYTES, FrameLimits, ProtocolError, ResolvedBrokerSocket,
};
use dekopon_config::{Projection, ProjectionError};
use dekopon_core::{
    AgentId, ExternalSubject, FileHygieneError, FileTier,
    fragments::{self, FragmentError, MergeRules},
    read_trusted_file,
};
use dekopon_telemetry::{ExporterSettings, TelemetryError, Transport};
use rustls::pki_types::{CertificateDer, pem::PemObject as _};
use serde::Deserialize;
use thiserror::Error;

pub(crate) mod memory;
use memory::{
    CaptureConfig, JournalRecall, KnobInputs, KnobOverrides, PolicyProblem, SelectionConfig,
    SummariesConfig, resolve_knobs, resolve_settings,
};
pub use memory::{MemoryKnobs, MemorySettings, ResolvedSettings};

use crate::{
    progress::{KeepAlive, ProgressDetail, Templates},
    session::{FAILURE_REPLY, STOPPED_REPLY},
};

pub const CONFIG_API_VERSION: &str = "dekopon.dev/gatewayd/v1alpha1";
pub const HARD_MAX_CONFIG_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_CONCURRENT_SESSIONS: usize = 4;
pub const DEFAULT_MAX_JOBS: usize = 2;
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
    #[serde(rename = "dekopon.dev/gatewayd/v1alpha1")]
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
pub(crate) enum Steering {
    #[default]
    Abort,
    Boundary,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum IndicatorSurface {
    #[default]
    Auto,
    Off,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum WorkingReaction {
    #[default]
    Off,
    On,
    Auto,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SeenReaction {
    #[default]
    Off,
    On,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReactionConfig {
    #[serde(default)]
    pub working: Option<WorkingReaction>,
    #[serde(default)]
    pub seen: Option<SeenReaction>,
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
    pub mode: Option<LivenessMode>,
    pub status: Option<IndicatorSurface>,
    pub typing: Option<IndicatorSurface>,
    pub reaction: Option<ReactionConfig>,
    pub progress: Option<ProgressSurface>,
    pub stream: Option<bool>,
    pub status_text: Option<bool>,
    pub cancel_button: Option<bool>,
    pub keep_alive: Option<KeepAliveConfig>,
    pub templates: Option<TemplateOverrides>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub conversations: BTreeMap<ConversationKind, LivenessOverride>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LivenessOverride {
    pub mode: Option<LivenessMode>,
    pub status: Option<IndicatorSurface>,
    pub typing: Option<IndicatorSurface>,
    pub reaction: Option<ReactionConfig>,
    pub progress: Option<ProgressSurface>,
    pub stream: Option<bool>,
    pub status_text: Option<bool>,
    pub cancel_button: Option<bool>,
    pub keep_alive: Option<KeepAliveConfig>,
    pub templates: Option<TemplateOverrides>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TemplateOverrides {
    pub working: Option<String>,
    pub tool: Option<String>,
    pub keep_alive: Option<String>,
    pub note: Option<String>,
    pub note_eta: Option<String>,
    pub stopped: Option<String>,
    pub failed: Option<String>,
}

impl TemplateOverrides {
    fn overlay(&mut self, other: &Self) {
        if other.working.is_some() {
            self.working.clone_from(&other.working);
        }
        if other.tool.is_some() {
            self.tool.clone_from(&other.tool);
        }
        if other.keep_alive.is_some() {
            self.keep_alive.clone_from(&other.keep_alive);
        }
        if other.note.is_some() {
            self.note.clone_from(&other.note);
        }
        if other.note_eta.is_some() {
            self.note_eta.clone_from(&other.note_eta);
        }
        if other.stopped.is_some() {
            self.stopped.clone_from(&other.stopped);
        }
        if other.failed.is_some() {
            self.failed.clone_from(&other.failed);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LivenessSettings {
    pub mode: LivenessMode,
    pub status: IndicatorSurface,
    pub typing: IndicatorSurface,
    pub working: WorkingReaction,
    pub seen: SeenReaction,
    pub progress: ProgressSurface,
    pub stream: bool,
    pub status_text: bool,
    pub cancel_button: bool,
}

impl LivenessSettings {
    fn overlay(&mut self, value: &LivenessOverride) {
        self.mode = value.mode.unwrap_or(self.mode);
        self.status = value.status.unwrap_or(self.status);
        self.typing = value.typing.unwrap_or(self.typing);
        if let Some(reaction) = value.reaction {
            self.working = reaction.working.unwrap_or(self.working);
            self.seen = reaction.seen.unwrap_or(self.seen);
        }
        self.progress = value.progress.unwrap_or(self.progress);
        self.stream = value.stream.unwrap_or(self.stream);
        self.status_text = value.status_text.unwrap_or(self.status_text);
        self.cancel_button = value.cancel_button.unwrap_or(self.cancel_button);
    }
}

impl LivenessConfig {
    #[must_use]
    pub fn settings(&self) -> LivenessSettings {
        let mut settings = LivenessSettings::default();
        settings.overlay(&self.override_fields());
        settings
    }

    fn override_fields(&self) -> LivenessOverride {
        LivenessOverride {
            mode: self.mode,
            status: self.status,
            typing: self.typing,
            reaction: self.reaction,
            progress: self.progress,
            stream: self.stream,
            status_text: self.status_text,
            cancel_button: self.cancel_button,
            keep_alive: self.keep_alive.clone(),
            templates: self.templates.clone(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ResolvedLiveness {
    pub settings: LivenessSettings,
    pub keep_alive: KeepAlive,
    pub templates: Templates,
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
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
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GatewaydConfig {
    pub api_version: ConfigApiVersion,
    pub catalog_path: PathBuf,
    #[serde(default)]
    pub broker: BrokerConfig,
    pub transports: Vec<TransportConfig>,
    pub models: Vec<ModelConfig>,
    #[serde(default, rename = "extraCABundles")]
    pub extra_ca_bundles: Vec<PathBuf>,
    pub routes: Vec<RouteConfig>,
    #[serde(default)]
    pub stop_words: Option<Vec<String>>,
    #[serde(default)]
    pub sessions: SessionsConfig,
    #[serde(default)]
    pub shutdown_grace_ms: Option<u64>,
    #[serde(default)]
    pub telemetry: Option<TelemetryConfig>,
    #[serde(default)]
    pub metering: Option<crate::metering::MeteringConfig>,
    #[serde(default)]
    pub proxy: Option<crate::proxy::ProxyConfig>,
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
    Fauxlack {
        name: String,
        server: String,
        bot: String,
        #[serde(default, rename = "indicators", deserialize_with = "null_as_default")]
        liveness: LivenessConfig,
    },
    SlackSocketMode {
        name: String,
        app_token_env: String,
        bot_token_env: String,
        #[serde(default)]
        experience: SlackExperience,
        #[serde(default, rename = "indicators", deserialize_with = "null_as_default")]
        liveness: LivenessConfig,
        #[serde(default)]
        endpoint: Option<String>,
    },
    DiscordGateway {
        name: String,
        bot_token_env: String,
        #[serde(default)]
        message_content: bool,
        #[serde(default, rename = "indicators", deserialize_with = "null_as_default")]
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
        #[serde(default, rename = "indicators", deserialize_with = "null_as_default")]
        liveness: LivenessConfig,
        #[serde(default)]
        graph_endpoint: Option<String>,
    },
    GithubWebhook {
        name: String,
        bind: SocketAddr,
        secret_env: String,
    },
    TelegramLongPoll {
        name: String,
        bot_token_env: String,
        #[serde(default, rename = "indicators", deserialize_with = "null_as_default")]
        liveness: LivenessConfig,
        #[serde(default)]
        endpoint: Option<String>,
    },
    Local {
        name: String,
        socket_path: PathBuf,
        #[serde(default, rename = "indicators", deserialize_with = "null_as_default")]
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
    if Instant::now()
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
            Self::Fauxlack { name, .. }
            | Self::SlackSocketMode { name, .. }
            | Self::DiscordGateway { name, .. }
            | Self::WhatsappCloudApi { name, .. }
            | Self::GithubWebhook { name, .. }
            | Self::TelegramLongPoll { name, .. }
            | Self::Local { name, .. } => name,
        }
    }

    #[must_use]
    pub const fn chat_kind(&self) -> ChatTransportKind {
        match self {
            Self::Fauxlack { .. } => ChatTransportKind::Fauxlack,
            Self::SlackSocketMode { .. } => ChatTransportKind::Slack,
            Self::DiscordGateway { .. } => ChatTransportKind::Discord,
            Self::WhatsappCloudApi { .. } => ChatTransportKind::Whatsapp,
            Self::GithubWebhook { .. } => ChatTransportKind::Github,
            Self::TelegramLongPoll { .. } => ChatTransportKind::Telegram,
            Self::Local { .. } => ChatTransportKind::Local,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Fauxlack { .. } => "fauxlack",
            Self::SlackSocketMode { .. } => "slackSocketMode",
            Self::DiscordGateway { .. } => "discordGateway",
            Self::WhatsappCloudApi { .. } => "whatsappCloudApi",
            Self::GithubWebhook { .. } => "githubWebhook",
            Self::TelegramLongPoll { .. } => "telegramLongPoll",
            Self::Local { .. } => "local",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(
    tag = "kind",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ModelConfig {
    OpenaiCompatible {
        #[serde(skip)]
        ca_bundles: Arc<[dekopon_core::Redacted<String>]>,
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
        #[serde(skip)]
        ca_bundles: Arc<[dekopon_core::Redacted<String>]>,
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
        #[serde(skip)]
        ca_bundles: Arc<[dekopon_core::Redacted<String>]>,
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
    Anthropic {
        #[serde(skip)]
        ca_bundles: Arc<[dekopon_core::Redacted<String>]>,
        name: String,
        model: String,
        api_key_env: String,
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
    pub(crate) fn ca_bundles(&self) -> &[dekopon_core::Redacted<String>] {
        match self {
            Self::OpenaiCompatible { ca_bundles, .. }
            | Self::Openrouter { ca_bundles, .. }
            | Self::ChatgptSubscription { ca_bundles, .. }
            | Self::Anthropic { ca_bundles, .. } => ca_bundles,
        }
    }

    fn set_ca_bundles(&mut self, bundles: &Arc<[dekopon_core::Redacted<String>]>) {
        match self {
            Self::OpenaiCompatible { ca_bundles, .. }
            | Self::Openrouter { ca_bundles, .. }
            | Self::ChatgptSubscription { ca_bundles, .. }
            | Self::Anthropic { ca_bundles, .. } => *ca_bundles = Arc::clone(bundles),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::OpenaiCompatible { name, .. }
            | Self::ChatgptSubscription { name, .. }
            | Self::Openrouter { name, .. }
            | Self::Anthropic { name, .. } => name,
        }
    }

    pub(crate) fn vendor_model(&self) -> &str {
        match self {
            Self::OpenaiCompatible { model, .. }
            | Self::ChatgptSubscription { model, .. }
            | Self::Openrouter { model, .. }
            | Self::Anthropic { model, .. } => model,
        }
    }

    #[must_use]
    pub fn accepts_images(&self) -> bool {
        match self {
            Self::OpenaiCompatible { modalities, .. }
            | Self::ChatgptSubscription { modalities, .. }
            | Self::Openrouter { modalities, .. } => modalities.contains(&Modality::Image),
            Self::Anthropic { .. } => false,
        }
    }

    #[must_use]
    pub fn classes(&self) -> &[String] {
        match self {
            Self::OpenaiCompatible { classes, .. }
            | Self::ChatgptSubscription { classes, .. }
            | Self::Openrouter { classes, .. } => classes,
            Self::Anthropic { .. } => &[],
        }
    }

    pub(crate) const fn timeout_ms(&self) -> Option<u64> {
        match self {
            Self::OpenaiCompatible { timeout_ms, .. }
            | Self::ChatgptSubscription { timeout_ms, .. }
            | Self::Openrouter { timeout_ms, .. } => Some(*timeout_ms),
            Self::Anthropic { .. } => None,
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
    #[serde(default)]
    pub job_timeout_ms: Option<u64>,
}

impl Default for RouteLimits {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
            max_capability_calls: DEFAULT_MAX_CAPABILITY_CALLS,
            max_duration_ms: None,
            script_timeout_ms: None,
            job_timeout_ms: None,
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

#[derive(Clone, Debug, Deserialize)]
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
        #[serde(default)]
        max_bytes: Option<usize>,
        #[serde(default)]
        recall: Option<RecallSource>,
        #[serde(default)]
        forget_after_ms: Option<u64>,
        #[serde(default)]
        hot_turns: Option<std::num::NonZeroUsize>,
        #[serde(default)]
        summaries: Option<SummariesConfig>,
        #[serde(default)]
        capture: Option<CaptureConfig>,
        #[serde(default)]
        selection: Option<SelectionConfig>,
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
            max_bytes: None,
            recall: None,
            forget_after_ms: None,
            hot_turns: None,
            summaries: None,
            capture: None,
            selection: None,
        }
    }
}

const fn default_idle_timeout_ms() -> u64 {
    DEFAULT_CONVERSATION_IDLE_TIMEOUT.as_secs() * 1_000
}

const fn default_conversation_max_turns() -> usize {
    DEFAULT_CONVERSATION_MAX_TURNS
}

// One week matches how long WhatsApp keeps an inbound media id, so recalled photos stay fetchable.
pub const DEFAULT_FORGET_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent authored route switches are optional and retain their wire booleans"
)]
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RouteConfig {
    pub transport: String,
    pub conversation: ConversationMatchConfig,
    #[serde(default, deserialize_with = "github_events_present")]
    pub github_events: Option<Vec<GithubEvent>>,
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
    /// Turning inspect_agent_config off only removes the structured config dump; the instructions
    /// stay in the system prompt regardless, so this is never a real secrecy gate against a
    /// determined user.
    #[serde(default)]
    pub inspect_agent_config: bool,
    #[serde(default)]
    pub limits: RouteLimits,
    #[serde(default)]
    pub(crate) progress_detail: ProgressDetail,
    #[serde(default)]
    pub(crate) steering: Steering,
    #[serde(default)]
    pub(crate) progress_notes: bool,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default, deserialize_with = "null_as_default")]
    pub indicators: LivenessOverride,
    #[serde(default)]
    pub wakes: bool,
    #[serde(default, deserialize_with = "null_as_default")]
    pub follow: bool,
}

pub use dekopon_core::github::GithubEventName as GithubEvent;

fn github_events_present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<GithubEvent>>, D::Error> {
    Vec::deserialize(deserializer).map(Some)
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

#[expect(
    clippy::struct_excessive_bools,
    reason = "resolved route preserves independent authored switches"
)]
#[derive(Clone, Debug)]
pub struct ResolvedRoute {
    pub transport: String,
    pub github_events: Option<Vec<GithubEvent>>,
    pub conversation: ConversationMatch,
    pub subjects: Option<Vec<ExternalSubject>>,
    pub agent: AgentId,
    pub model: Option<String>,
    pub inspect_agent_config: bool,
    pub limits: RouteLimits,
    pub(crate) progress_detail: ProgressDetail,
    pub(crate) steering: Steering,
    pub(crate) progress_notes: bool,
    pub memory: MemoryPolicy,
    pub memory_knobs: Option<MemoryKnobs>,
    pub(crate) indicators: BTreeMap<ConversationKind, Arc<ResolvedLiveness>>,
    pub wakes: bool,
    pub follow: bool,
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
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AssetsConfig {
    #[serde(default)]
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct ResolvedJournal {
    pub dir: PathBuf,
    pub retention: Duration,
    pub seal_retention: Duration,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionsConfig {
    #[serde(default, deserialize_with = "null_as_default")]
    pub indicators: LivenessOverride,
    #[serde(default = "default_asset_retention_bytes")]
    pub asset_retention_bytes: usize,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_reply_on_busy")]
    pub reply_on_busy: bool,
    #[serde(default = "default_max_conversations")]
    pub max_conversations: usize,
    #[serde(default = "default_max_jobs")]
    pub max_jobs: usize,
    #[serde(default)]
    pub journal: Option<JournalConfig>,
    #[serde(default)]
    pub assets: Option<AssetsConfig>,
    #[serde(default)]
    pub memory: MemorySettings,
    #[serde(default)]
    pub wakes: Option<WakesConfig>,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            indicators: LivenessOverride::default(),
            asset_retention_bytes: DEFAULT_ASSET_RETENTION_BYTES,
            max_concurrent: DEFAULT_MAX_CONCURRENT_SESSIONS,
            reply_on_busy: true,
            max_conversations: DEFAULT_MAX_CONVERSATIONS,
            max_jobs: DEFAULT_MAX_JOBS,
            journal: None,
            assets: None,
            memory: MemorySettings::default(),
            wakes: None,
        }
    }
}

pub(crate) const DEFAULT_ASSET_RETENTION_BYTES: usize = 1024 * 1024 * 1024;
const fn default_asset_retention_bytes() -> usize {
    DEFAULT_ASSET_RETENTION_BYTES
}

const fn default_max_concurrent() -> usize {
    DEFAULT_MAX_CONCURRENT_SESSIONS
}

const fn default_max_conversations() -> usize {
    DEFAULT_MAX_CONVERSATIONS
}

const fn default_max_jobs() -> usize {
    DEFAULT_MAX_JOBS
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

impl ResolvedBroker {
    /// A leg's client waits out a restarting broker for one I/O timeout before failing.
    pub fn leg_client(&self) -> Result<BrokerClient, ClientError> {
        Ok(
            BrokerClient::new(&self.socket_path, self.server_uid, self.frame)?
                .with_connect_window(self.frame.io_timeout),
        )
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct InProcess {
    #[cfg(feature = "fauxlack")]
    pub(crate) fauxlack: Option<crate::fauxlack::Fauxlack>,
}

impl InProcess {
    fn validate(&self, transports: &[TransportConfig], problems: &mut Vec<ConfigProblem>) {
        let mut attached = BTreeSet::new();
        for transport in transports {
            if let TransportConfig::Fauxlack {
                name, server, bot, ..
            } = transport
            {
                if !attached.insert((server, bot)) {
                    problems.push(ConfigProblem::FauxlackDuplicate {
                        name: name.clone(),
                        server: server.clone(),
                        bot: bot.clone(),
                    });
                }
                #[cfg(feature = "fauxlack")]
                if let Some(service) = &self.fauxlack {
                    service.validate(name, server, bot, problems);
                    continue;
                }
                problems.push(ConfigProblem::FauxlackService { name: name.clone() });
            }
        }
    }
}

#[derive(Debug)]
pub struct ResolvedConfig {
    pub(crate) service: InProcess,
    pub source: PathBuf,
    pub catalog_path: PathBuf,
    pub broker: ResolvedBroker,
    pub transports: Vec<TransportConfig>,
    pub models: Vec<ModelConfig>,
    pub(crate) model_files: BTreeMap<String, PathBuf>,
    pub(crate) ca_sources: BTreeMap<PathBuf, RawCandidate>,
    pub(crate) extra_ca_bundles: Vec<PathBuf>,
    pub routes: Vec<ResolvedRoute>,
    pub(crate) stop_words: Vec<String>,
    pub sessions: SessionsConfig,
    pub journal: Option<ResolvedJournal>,
    pub assets: Option<PathBuf>,
    pub memory: ResolvedSettings,
    pub wakes: Option<ResolvedWakes>,
    pub shutdown_grace: Duration,
    pub telemetry: Option<ResolvedTelemetry>,
    pub metering: crate::metering::ResolvedMetering,
    pub proxy: Option<crate::proxy::ResolvedProxy>,
}

impl ResolvedConfig {
    pub(crate) fn load_model_ca_bundles(&mut self) -> Result<(), ConfigError> {
        let mut problems = Vec::new();
        let (bundles, sources) =
            model_ca_bundles(&self.extra_ca_bundles, LoadMode::Boot, &mut problems);
        if !problems.is_empty() {
            return Err(ConfigError::Invalid {
                path: self.source.clone(),
                problems,
            });
        }
        for model in &mut self.models {
            model.set_ca_bundles(&bundles);
        }
        self.ca_sources = sources;
        Ok(())
    }

    pub(crate) fn emit_effective_indicators(&self) {
        for (index, route) in self.routes.iter().enumerate() {
            for (kind, resolved) in &route.indicators {
                tracing::info!(event = "gateway_indicators_effective", route = index, transport = %route.transport.chars().take(64).collect::<String>(),
                    kind = kind.as_str(), mode = ?resolved.settings.mode, status = ?resolved.settings.status,
                    typing = ?resolved.settings.typing, working = ?resolved.settings.working,
                    seen = ?resolved.settings.seen, progress = ?resolved.settings.progress,
                    stream = resolved.settings.stream, status_text = resolved.settings.status_text,
                    cancel_button = resolved.settings.cancel_button, "gateway.indicators.effective");
            }
        }
    }
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
    let (resolved, mut refusals) = load_reporting(path.as_ref(), expected_uid, mode).await;
    match (resolved, mode, refusals.is_empty()) {
        (Some(resolved), LoadMode::Check, _) | (Some(resolved), LoadMode::Boot, true) => {
            Ok((resolved, refusals))
        }
        _ => Err(refusals.remove(0)),
    }
}

pub(crate) async fn load_reporting(
    path: &Path,
    expected_uid: u32,
    mode: LoadMode,
) -> (Option<ResolvedConfig>, Vec<ConfigError>) {
    load_reporting_with_service(path, expected_uid, mode, &InProcess::default()).await
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "decode constructs only Decode; other ConfigError variants cannot arise here"
)]
pub(crate) async fn load_reporting_with_service(
    path: &Path,
    expected_uid: u32,
    mode: LoadMode,
    service: &InProcess,
) -> (Option<ResolvedConfig>, Vec<ConfigError>) {
    let path = match absolute(path) {
        Ok(path) => path,
        Err(error) => return (None, vec![error]),
    };
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return load_directory(path, expected_uid, mode, service).await;
    }
    let resolved = async {
        let config =
            decode(&read_config_file(path.clone(), expected_uid).await?).map_err(|error| {
                match error {
                    ConfigError::Decode { source, .. } => ConfigError::Decode {
                        path: Some(path.clone()),
                        source,
                    },
                    other => other,
                }
            })?;
        resolve_with_service(
            config,
            path,
            &BrokerSocketDiscovery::from_process(None),
            expected_uid,
            mode,
            service,
        )
    }
    .await;
    match resolved {
        Ok(resolved) => (Some(resolved), Vec::new()),
        Err(error) => (None, vec![error]),
    }
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
    service: &InProcess,
) -> (Option<ResolvedConfig>, Vec<ConfigError>) {
    let paths = match scan_gateway_directory(&directory, expected_uid) {
        Ok(paths) => paths,
        Err(error) => return (None, vec![error.into()]),
    };
    let Some(first) = paths.first().cloned() else {
        return (
            None,
            vec![ConfigError::EmptyConfigDirectory { path: directory }],
        );
    };
    let mut parsed = Vec::with_capacity(paths.len());
    let mut defined = BTreeMap::<String, PathBuf>::new();
    let mut referenced = Vec::<(PathBuf, String)>::new();
    let mut model_files = BTreeMap::<String, PathBuf>::new();
    for path in paths {
        let bytes = match read_config_file(path.clone(), expected_uid).await {
            Ok(bytes) => bytes,
            Err(error) => return (None, vec![error]),
        };
        let mapping = match serde_yaml::from_slice::<serde_yaml::Mapping>(&bytes) {
            Ok(mapping) => mapping,
            Err(source) => return (None, vec![ConfigError::DecodeFragment { path, source }]),
        };
        for transport in names_under(&mapping, "transports", "name") {
            defined.insert(transport, path.clone());
        }
        for transport in names_under(&mapping, "routes", "transport") {
            referenced.push((path.clone(), transport));
        }
        for model in names_under(&mapping, "models", "name") {
            model_files.insert(model, path.clone());
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
        refusals.push(refusal);
    }
    let (merged, refusal) = fragments::merge_reporting(parsed, &MERGE_RULES);
    if let Some(refusal) = refusal {
        refusals.push(refusal.into());
    }
    let config = match serde_yaml::from_value::<GatewaydConfig>(serde_yaml::Value::Mapping(merged))
    {
        Ok(config) => config,
        Err(source) => {
            refusals.push(ConfigError::Decode {
                path: Some(directory),
                source,
            });
            return (None, refusals);
        }
    };
    match resolve_with_service(
        config,
        first,
        &BrokerSocketDiscovery::from_process(None),
        expected_uid,
        mode,
        service,
    ) {
        Ok(mut resolved) => {
            resolved.model_files.extend(model_files);
            (Some(resolved), refusals)
        }
        Err(error) => {
            refusals.push(error);
            (None, refusals)
        }
    }
}

fn decode(document: &[u8]) -> Result<GatewaydConfig, ConfigError> {
    serde_yaml::from_slice::<GatewaydConfig>(document)
        .map_err(|source| ConfigError::Decode { path: None, source })
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

fn scan_gateway_directory(
    directory: &Path,
    expected_uid: u32,
) -> Result<Vec<PathBuf>, FragmentError> {
    let metadata = std::fs::symlink_metadata(directory).map_err(|source| FragmentError::Read {
        path: directory.to_path_buf(),
        source,
    })?;
    use std::os::unix::fs::MetadataExt as _;
    let projected = std::fs::symlink_metadata(directory.join("..data"))
        .is_ok_and(|metadata| metadata.file_type().is_symlink());
    if projected {
        let read_only = rustix::fs::statvfs(directory)
            .is_ok_and(|stat| stat.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY));
        if !metadata.is_dir() || !read_only || metadata.uid() != 0 && metadata.uid() != expected_uid
        {
            return Err(FragmentError::InsecureDirectory {
                path: directory.to_path_buf(),
            });
        }
        let mut paths = std::fs::read_dir(directory)
            .map_err(|source| FragmentError::Read {
                path: directory.to_path_buf(),
                source,
            })?
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(|source| FragmentError::Read {
                        path: directory.to_path_buf(),
                        source,
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        paths.retain(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
        });
        paths.sort();
        Ok(paths)
    } else {
        fragments::scan_directory(directory, expected_uid, "yaml")
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SourceSnapshot(pub(crate) BTreeMap<PathBuf, Vec<u8>>);

/// In memory only, never logged or published: it covers source bytes and exists to tell one raw
/// candidate from another.
#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(crate) struct RawCandidate([u8; 32]);

impl std::fmt::Debug for RawCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RawCandidate")
    }
}

impl RawCandidate {
    pub(crate) fn bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub(crate) fn of(
        config: Result<&SourceSnapshot, &ConfigError>,
        catalog: Option<Result<&SourceSnapshot, &ConfigError>>,
    ) -> Self {
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        for source in [Some(config), catalog] {
            let parts = match source {
                Some(Ok(snapshot)) => snapshot
                    .0
                    .iter()
                    .flat_map(|(path, bytes)| [path.as_os_str().as_encoded_bytes(), bytes])
                    .map(Cow::Borrowed)
                    .collect(),
                Some(Err(error)) => vec![Cow::Owned(dekopon_core::error_chain(error).into_bytes())],
                None => Vec::new(),
            };
            hasher.update(parts.len().to_le_bytes());
            for part in parts {
                hasher.update(part.len().to_le_bytes());
                hasher.update(part);
            }
        }
        Self(hasher.finalize().into())
    }

    pub(crate) fn refusing(self, refusal: &str) -> Self {
        use sha2::{Digest as _, Sha256};
        Self(
            Sha256::new()
                .chain_update(self.0)
                .chain_update(refusal)
                .finalize()
                .into(),
        )
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct GatewaySources {
    pub(crate) config: SourceSnapshot,
    pub(crate) catalog: SourceSnapshot,
    pub(crate) ca_bundles: BTreeMap<PathBuf, RawCandidate>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct GatewayChanges {
    pub(crate) swap: Vec<&'static str>,
    pub(crate) restart_required: Vec<&'static str>,
}

impl GatewaySources {
    pub(crate) async fn still_current(
        &self,
        config_path: &Path,
        catalog_path: &Path,
        uid: u32,
    ) -> Result<bool, ConfigError> {
        for (path, digest) in &self.ca_bundles {
            use tokio::io::AsyncReadExt as _;
            let pem = async {
                let mut pem = Vec::new();
                tokio::fs::File::open(path)
                    .await?
                    .take(65_537)
                    .read_to_end(&mut pem)
                    .await?;
                Ok::<_, io::Error>(pem)
            }
            .await
            .map_err(|error| ConfigError::Invalid {
                path: config_path.to_path_buf(),
                problems: vec![ConfigProblem::ModelCaBundleRead { kind: error.kind() }],
            })?;
            if &ca_digest(&pem) != digest {
                return Ok(false);
            }
        }
        if self.config != source_snapshot(config_path, None, uid).await? {
            return Ok(false);
        }
        let catalog = source_snapshot(catalog_path, None, uid).await?;
        if !catalog
            .0
            .iter()
            .all(|(path, bytes)| self.catalog.0.get(path) == Some(bytes))
        {
            return Ok(false);
        }
        for (path, bytes) in &self.catalog.0 {
            if read_source_entry(path.clone(), uid).await? != *bytes {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) fn changes(&self, next: &Self) -> Result<GatewayChanges, ConfigError> {
        let old = merged_mapping(&self.config)?;
        let new = merged_mapping(&next.config)?;
        let classified = [
            ("models", "models"),
            ("extraCABundles", "extraCABundles"),
            ("routes", "routes"),
            ("stopWords", "stopWords"),
            ("catalogPath", "catalog"),
            ("broker", "broker"),
            ("transports", "transports"),
            ("sessions", "sessions"),
            ("shutdownGraceMs", "shutdownGraceMs"),
            ("telemetry", "telemetry"),
            ("metering", "metering"),
            ("proxy", "proxy"),
            ("apiVersion", "apiVersion"),
        ];
        if old.keys().chain(new.keys()).any(|key| {
            !key.as_str()
                .is_some_and(|key| classified.iter().any(|(known, _)| known == &key))
        }) {
            return Err(ConfigError::UnclassifiedSource);
        }
        let mut changes = GatewayChanges::default();
        for (key, category) in classified {
            let before = effective_value(&old, key);
            let after = effective_value(&new, key);
            if before == after
                || ["broker", "sessions", "metering"].contains(&key)
                    && before.is_none_or(|value| {
                        value
                            .as_mapping()
                            .is_some_and(serde_yaml::Mapping::is_empty)
                    })
                    && after.is_none_or(|value| {
                        value
                            .as_mapping()
                            .is_some_and(serde_yaml::Mapping::is_empty)
                    })
            {
                continue;
            }
            match category {
                "models" | "routes" | "stopWords" | "catalog" | "extraCABundles" => {
                    changes.swap.push(category)
                }
                "transports" | "sessions" => {
                    let (resident_before, indicators_before) = split_indicators(before, category);
                    let (resident_after, indicators_after) = split_indicators(after, category);
                    if resident_before != resident_after {
                        changes.restart_required.push(category);
                    }
                    if indicators_before != indicators_after {
                        changes.swap.push("indicators");
                    }
                }
                "broker" | "shutdownGraceMs" | "telemetry" | "metering" | "proxy" => {
                    changes.restart_required.push(category);
                }
                "apiVersion" => return Err(ConfigError::UnclassifiedSource),
                _ => return Err(ConfigError::UnclassifiedSource),
            }
        }
        if self.ca_bundles != next.ca_bundles && !changes.swap.contains(&"extraCABundles") {
            changes.swap.push("extraCABundles");
        }
        if self.catalog != next.catalog && !changes.swap.contains(&"catalog") {
            changes.swap.push("catalog");
        }
        Ok(changes)
    }
}

fn effective_value<'a>(
    mapping: &'a serde_yaml::Mapping,
    key: &str,
) -> Option<&'a serde_yaml::Value> {
    mapping.get(key).filter(|value| !value.is_null())
}

fn split_indicators(
    value: Option<&serde_yaml::Value>,
    category: &str,
) -> (Option<serde_yaml::Value>, Option<serde_yaml::Value>) {
    let Some(value) = value else {
        return if category == "sessions" {
            (
                Some(serde_yaml::Value::Mapping(serde_yaml::Mapping::new())),
                None,
            )
        } else {
            (None, None)
        };
    };
    if category == "sessions" {
        let Some(mapping) = value.as_mapping() else {
            return (Some(value.clone()), None);
        };
        let mut resident = mapping.clone();
        let indicators = resident
            .remove("indicators")
            .filter(|value| !value.is_null());
        if resident
            .get("maxConcurrent")
            .and_then(serde_yaml::Value::as_u64)
            == u64::try_from(SessionsConfig::default().max_concurrent).ok()
        {
            resident.remove("maxConcurrent");
        }
        return (Some(serde_yaml::Value::Mapping(resident)), indicators);
    }
    let Some(items) = value.as_sequence() else {
        return (Some(value.clone()), None);
    };
    let mut resident = items.clone();
    let mut indicators = Vec::new();
    for item in &mut resident {
        if let Some(mapping) = item.as_mapping_mut() {
            indicators.push(
                mapping
                    .remove("indicators")
                    .unwrap_or(serde_yaml::Value::Null),
            );
        }
    }
    (
        Some(serde_yaml::Value::Sequence(resident)),
        Some(serde_yaml::Value::Sequence(indicators)),
    )
}

pub(crate) fn merged_mapping(
    snapshot: &SourceSnapshot,
) -> Result<serde_yaml::Mapping, ConfigError> {
    let parsed = snapshot
        .0
        .iter()
        .map(|(path, bytes)| {
            serde_yaml::from_slice::<serde_yaml::Mapping>(bytes)
                .map(|mapping| (path.clone(), mapping))
                .map_err(|source| ConfigError::DecodeFragment {
                    path: path.clone(),
                    source,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let (merged, refusal) = fragments::merge_reporting(parsed, &MERGE_RULES);
    match refusal {
        Some(refusal) => Err(refusal.into()),
        None => Ok(merged),
    }
}

pub(crate) async fn source_snapshot(
    source: &Path,
    catalog: Option<&dekopon_config::LocalCatalog>,
    expected_uid: u32,
) -> Result<SourceSnapshot, ConfigError> {
    let mut paths = if source.is_dir() {
        scan_gateway_directory(source, expected_uid)?
    } else {
        vec![source.to_path_buf()]
    };
    let loaded = catalog.map(loaded_catalog_bytes).transpose()?;
    if let Some(loaded) = &loaded {
        paths.extend(loaded.keys().cloned());
    }
    paths.sort();
    paths.dedup();
    let mut entries = BTreeMap::new();
    for path in paths {
        entries.insert(path.clone(), read_source_entry(path, expected_uid).await?);
    }
    // A catalog serves what it loaded, so its snapshot must be those bytes, not a later read.
    if let Some(loaded) = loaded
        && !entries
            .iter()
            .map(|(path, bytes)| (path, bytes.as_slice()))
            .eq(loaded.iter().map(|(path, bytes)| (path, bytes.as_ref())))
    {
        return Err(ConfigError::ChangedDuringLoad);
    }
    Ok(SourceSnapshot(entries))
}

fn loaded_catalog_bytes(
    catalog: &dekopon_config::LocalCatalog,
) -> Result<BTreeMap<PathBuf, Cow<'_, [u8]>>, ConfigError> {
    let mut loaded = BTreeMap::new();
    for (path, text) in catalog.files() {
        record_loaded(&mut loaded, path.clone(), Cow::Borrowed(text.as_bytes()))?;
    }
    let root = catalog.source();
    let base = if root.is_dir() {
        root
    } else {
        root.parent().ok_or(ConfigError::MissingParent)?
    };
    for agent in catalog.agents() {
        if let Some(file) = &agent.spec.instructions_file {
            let text = agent
                .spec
                .instructions
                .as_deref()
                .ok_or(ConfigError::ChangedDuringLoad)?;
            record_loaded(&mut loaded, base.join(file), Cow::Borrowed(text.as_bytes()))?;
        }
        let id = agent
            .metadata
            .name
            .parse()
            .map_err(|_invalid| ConfigError::InvalidCatalogAgent)?;
        for skill in catalog.agent_skills(&id) {
            let root = skill.source();
            record_loaded(
                &mut loaded,
                root.join(dekopon_core::SKILL_FILE_NAME),
                Cow::Borrowed(skill.file().as_bytes()),
            )?;
            for resource in skill.resources() {
                record_loaded(
                    &mut loaded,
                    root.join(&resource.path),
                    Cow::Borrowed(resource.text.as_bytes()),
                )?;
            }
            for (directory, names) in skill.directories() {
                record_loaded(
                    &mut loaded,
                    directory.clone(),
                    Cow::Owned(listing_bytes(directory, names)?),
                )?;
            }
        }
    }
    Ok(loaded)
}

fn record_loaded<'a>(
    loaded: &mut BTreeMap<PathBuf, Cow<'a, [u8]>>,
    path: PathBuf,
    bytes: Cow<'a, [u8]>,
) -> Result<(), ConfigError> {
    match loaded.entry(path) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(bytes);
            Ok(())
        }
        std::collections::btree_map::Entry::Occupied(entry) if *entry.get() == bytes => Ok(()),
        std::collections::btree_map::Entry::Occupied(_) => Err(ConfigError::ChangedDuringLoad),
    }
}

async fn read_source_entry(path: PathBuf, expected_uid: u32) -> Result<Vec<u8>, ConfigError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let metadata = std::fs::symlink_metadata(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    if metadata.file_type().is_symlink()
        && std::fs::metadata(&path).is_ok_and(|target| target.is_dir())
    {
        let projection = Projection::resolve(&path, expected_uid).map_err(projection_refusal)?;
        return read_projected_listing(&path, &projection);
    }
    if !metadata.is_dir() {
        return read_config_file(path, expected_uid).await;
    }
    let read_only = rustix::fs::statvfs(&path)
        .map_err(|source| ConfigError::Read {
            path: path.clone(),
            source: source.into(),
        })?
        .f_flag
        .contains(rustix::fs::StatVfsMountFlags::RDONLY);
    if metadata.uid() != expected_uid && (metadata.uid() != 0 || !read_only)
        || metadata.permissions().mode() & 0o022 != 0 && !read_only
    {
        return Err(ConfigError::InsecureProjection { path });
    }
    listing_bytes(&path, &sorted_names(&path)?)
}

fn sorted_names(directory: &Path) -> Result<Vec<std::ffi::OsString>, ConfigError> {
    let read_error = |source| ConfigError::Read {
        path: directory.to_path_buf(),
        source,
    };
    let mut names = std::fs::read_dir(directory)
        .map_err(read_error)?
        .map(|entry| entry.map(|entry| entry.file_name()).map_err(read_error))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    Ok(names)
}

fn listing_bytes(path: &Path, sorted: &[std::ffi::OsString]) -> Result<Vec<u8>, ConfigError> {
    use std::os::unix::ffi::OsStrExt as _;
    if sorted.len() > 1024 {
        return Err(ConfigError::InsecureProjection {
            path: path.to_path_buf(),
        });
    }
    let mut bytes = Vec::new();
    for name in sorted {
        let name = name.as_bytes();
        if bytes.len().saturating_add(name.len()).saturating_add(4) > HARD_MAX_CONFIG_BYTES {
            return Err(ConfigError::InsecureProjection {
                path: path.to_path_buf(),
            });
        }
        bytes.extend_from_slice(&(name.len() as u32).to_be_bytes());
        bytes.extend_from_slice(name);
    }
    Ok(bytes)
}

fn read_gateway_projection(path: &Path, expected_uid: u32) -> Result<Vec<u8>, ConfigError> {
    let projection = Projection::resolve(path, expected_uid).map_err(projection_refusal)?;
    read_projected_file(path, &projection, || {})
}

pub(crate) fn read_projected_file(
    path: &Path,
    projection: &Projection,
    after_read: impl FnOnce(),
) -> Result<Vec<u8>, ConfigError> {
    let bytes = read_trusted_file(
        projection.target(),
        projection.owner(),
        FileTier::NotWorldWritable,
        HARD_MAX_CONFIG_BYTES,
    )
    .map_err(|source| ConfigError::InsecureFile {
        path: path.to_path_buf(),
        source,
    })?;
    after_read();
    projection.confirm().map_err(projection_refusal)?;
    Ok(bytes)
}

pub(crate) fn read_projected_listing(
    path: &Path,
    projection: &Projection,
) -> Result<Vec<u8>, ConfigError> {
    let bytes = listing_bytes(path, &sorted_names(projection.target())?)?;
    projection.confirm().map_err(projection_refusal)?;
    Ok(bytes)
}

pub(crate) fn projection_refusal(error: ProjectionError) -> ConfigError {
    match error {
        ProjectionError::Untrusted { path } => ConfigError::InsecureProjection { path },
        ProjectionError::Switched { .. } => ConfigError::ChangedDuringLoad,
        ProjectionError::Io { path, source } => ConfigError::Read { path, source },
    }
}

pub(crate) fn read_gateway_regular_file_at(
    path: &Path,
    expected_uid: u32,
    observed: u32,
    read_only: bool,
    read: impl FnOnce(&Path, u32, FileTier, usize) -> Result<Vec<u8>, FileHygieneError>,
) -> Result<Vec<u8>, FileHygieneError> {
    let owner = if observed == 0 && read_only {
        0
    } else {
        expected_uid
    };
    if owner != observed {
        return Err(FileHygieneError::WrongOwner {
            path: path.to_path_buf(),
            owner: observed,
            expected: expected_uid,
        });
    }
    read(
        path,
        owner,
        FileTier::NotWorldWritable,
        HARD_MAX_CONFIG_BYTES,
    )
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
pub(crate) async fn read_config_file(
    path: PathBuf,
    expected_uid: u32,
) -> Result<Vec<u8>, ConfigError> {
    // This file isn't a secret, since credentials live in the environment and transport credential
    // files, so the bar here is only that nobody else can rewrite it.
    let owned = path.clone();
    let projection =
        std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink());
    if projection {
        return tokio::task::spawn_blocking(move || read_gateway_projection(&owned, expected_uid))
            .await
            .map_err(|join| ConfigError::Read {
                path,
                source: io::Error::other(join),
            })?;
    }
    tokio::task::spawn_blocking(move || {
        use std::os::unix::fs::MetadataExt as _;
        let observed = std::fs::symlink_metadata(&owned)
            .map_err(|source| FileHygieneError::Io {
                path: owned.clone(),
                source,
            })?
            .uid();
        let read_only = if observed == 0 && expected_uid != 0 {
            let parent = owned.parent().ok_or_else(|| FileHygieneError::Io {
                path: owned.clone(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "source has no parent"),
            })?;
            rustix::fs::statvfs(parent)
                .map_err(|source| FileHygieneError::Io {
                    path: owned.clone(),
                    source: source.into(),
                })?
                .f_flag
                .contains(rustix::fs::StatVfsMountFlags::RDONLY)
        } else {
            false
        };
        read_gateway_regular_file_at(&owned, expected_uid, observed, read_only, read_trusted_file)
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

fn ca_digest(pem: &[u8]) -> RawCandidate {
    use sha2::{Digest as _, Sha256};
    RawCandidate(Sha256::digest(pem).into())
}

fn model_ca_bundles(
    paths: &[PathBuf],
    mode: LoadMode,
    problems: &mut Vec<ConfigProblem>,
) -> (
    Arc<[dekopon_core::Redacted<String>]>,
    BTreeMap<PathBuf, RawCandidate>,
) {
    if paths.len() > 8 {
        problems.push(ConfigProblem::ModelCaBundleCount);
        return (Arc::default(), BTreeMap::new());
    }
    let mut bundles = Vec::new();
    let mut sources = BTreeMap::new();
    for path in paths {
        if !path.is_absolute() {
            problems.push(ConfigProblem::ModelCaBundlePath);
            continue;
        }
        match mode {
            LoadMode::Check => continue,
            LoadMode::Boot => {}
        }
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                problems.push(ConfigProblem::ModelCaBundleRead { kind: error.kind() });
                continue;
            }
        };
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 65_536 {
            problems.push(ConfigProblem::ModelCaBundleSize);
            continue;
        }
        let pem = match (|| {
            use std::io::Read as _;
            let mut pem = Vec::new();
            std::fs::File::open(path)?
                .take(65_537)
                .read_to_end(&mut pem)?;
            Ok::<_, io::Error>(pem)
        })() {
            Ok(pem) => pem,
            Err(error) => {
                problems.push(ConfigProblem::ModelCaBundleRead { kind: error.kind() });
                continue;
            }
        };
        if pem.is_empty() || pem.len() > 65_536 {
            problems.push(ConfigProblem::ModelCaBundleSize);
            continue;
        }
        let mut roots = rustls::RootCertStore::empty();
        let certificates = CertificateDer::pem_slice_iter(&pem).collect::<Result<Vec<_>, _>>();
        if certificates.map_or(true, |certs| {
            certs.is_empty() || certs.into_iter().any(|cert| roots.add(cert).is_err())
        }) {
            problems.push(ConfigProblem::ModelCaBundlePem);
            continue;
        }
        let digest = ca_digest(&pem);
        match String::from_utf8(pem) {
            Ok(pem) => {
                sources.insert(path.clone(), digest);
                bundles.push(dekopon_core::Redacted::new(pem));
            }
            Err(_) => problems.push(ConfigProblem::ModelCaBundlePem),
        }
    }
    (bundles.into(), sources)
}

#[cfg(test)]
pub(crate) fn resolve(
    config: GatewaydConfig,
    source: PathBuf,
    discovery: &BrokerSocketDiscovery,
    current_uid: u32,
    mode: LoadMode,
) -> Result<ResolvedConfig, ConfigError> {
    resolve_with_service(
        config,
        source,
        discovery,
        current_uid,
        mode,
        &InProcess::default(),
    )
}

#[expect(
    clippy::too_many_lines,
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
fn resolve_with_service(
    mut config: GatewaydConfig,
    source: PathBuf,
    discovery: &BrokerSocketDiscovery,
    current_uid: u32,
    mode: LoadMode,
    service: &InProcess,
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
    let (bundles, ca_sources) = model_ca_bundles(&config.extra_ca_bundles, mode, &mut problems);
    for model in &mut config.models {
        model.set_ca_bundles(&bundles);
    }

    if config.transports.is_empty() {
        problems.push(ConfigProblem::NoTransports);
    }
    if config.models.is_empty() {
        problems.push(ConfigProblem::NoModels);
    }
    if config.routes.is_empty() {
        problems.push(ConfigProblem::NoRoutes);
    }

    service.validate(&config.transports, &mut problems);
    let mut transports_incomplete = config.transports.is_empty();
    let mut transport_names = BTreeSet::new();
    let mut transport_kinds: BTreeMap<String, ChatTransportKind> = BTreeMap::new();
    let mut transports = Vec::with_capacity(config.transports.len());
    let mut webhook_mounts = BTreeSet::new();
    let mut whatsapp_binds: BTreeMap<SocketAddr, Vec<String>> = BTreeMap::new();
    let mut github_binds = Vec::new();
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
        let mount = match &transport {
            TransportConfig::WhatsappCloudApi {
                bind,
                callback_path,
                ..
            } => {
                whatsapp_binds.entry(*bind).or_default().push(name.clone());
                Some((*bind, callback_path.as_str()))
            }
            TransportConfig::GithubWebhook { bind, .. } => {
                github_binds.push((name.clone(), *bind));
                Some((*bind, "/webhooks/github"))
            }
            TransportConfig::Fauxlack { .. }
            | TransportConfig::SlackSocketMode { .. }
            | TransportConfig::DiscordGateway { .. }
            | TransportConfig::TelegramLongPoll { .. }
            | TransportConfig::Local { .. } => None,
        };
        if let Some((bind, path)) = mount
            && !webhook_mounts.insert((bind, path.to_owned()))
        {
            problems.push(ConfigProblem::ConflictingWebhookMount { name: name.clone() });
        }
        let indicators = match &transport {
            TransportConfig::Fauxlack { liveness, .. }
            | TransportConfig::SlackSocketMode { liveness, .. }
            | TransportConfig::DiscordGateway { liveness, .. }
            | TransportConfig::TelegramLongPoll { liveness, .. }
            | TransportConfig::WhatsappCloudApi { liveness, .. }
            | TransportConfig::Local { liveness, .. } => Some(liveness),
            TransportConfig::GithubWebhook { .. } => None,
        };
        for kind in indicators
            .into_iter()
            .flat_map(|liveness| liveness.conversations.keys())
            .filter(|kind| !transport.chat_kind().produces(**kind))
        {
            problems.push(ConfigProblem::ImpossibleLivenessConversation {
                transport: name.clone(),
                kind: kind.as_str(),
            });
        }
        transports.push(match transport {
            TransportConfig::Fauxlack {
                name,
                server,
                bot,
                liveness,
            } => TransportConfig::Fauxlack {
                name,
                server,
                bot,
                liveness,
            },
            TransportConfig::GithubWebhook {
                name,
                bind,
                secret_env,
            } => {
                check_env_name(&secret_env, &mut problems);
                if bind.port() == 0 {
                    problems.push(ConfigProblem::InvalidGithubBind { name: name.clone() });
                }
                TransportConfig::GithubWebhook {
                    name,
                    bind,
                    secret_env,
                }
            }
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
                message_content,
                liveness,
                endpoint,
            } => {
                check_env_name(&bot_token_env, &mut problems);
                let endpoint = checked_endpoint(endpoint, DISCORD_ENDPOINT, &mut problems);
                TransportConfig::DiscordGateway {
                    name,
                    bot_token_env,
                    message_content,
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
    for (bind, transports) in &whatsapp_binds {
        if transports.len() > 1 {
            problems.push(ConfigProblem::SharedWhatsappBind {
                bind: *bind,
                transports: transports.clone(),
            });
        }
    }
    if !whatsapp_binds.is_empty() {
        for (name, _) in github_binds
            .into_iter()
            .filter(|(_, bind)| !whatsapp_binds.contains_key(bind))
        {
            problems.push(ConfigProblem::GithubBindNotShared { name });
        }
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
        if model.timeout_ms() == Some(0) {
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
        }
        | ModelConfig::Anthropic {
            api_key_env: variable,
            ..
        } = model
        {
            check_env_name(variable, &mut problems);
        }
    }

    let proxy_only = config
        .models
        .iter()
        .filter(|model| matches!(model, ModelConfig::Anthropic { .. }))
        .map(|model| model.name().to_owned())
        .collect::<BTreeSet<_>>();
    let mut metering_problems = Vec::new();
    let metering = crate::metering::resolve(
        config.metering,
        &config
            .routes
            .iter()
            .map(|route| route.agent.to_string())
            .collect(),
        &model_names,
        config.telemetry.is_some(),
        &mut metering_problems,
    );
    problems.extend(metering_problems.into_iter().map(ConfigProblem::Metering));
    let mut proxy_problems = Vec::new();
    let proxy = crate::proxy::resolve(
        config.proxy,
        &config
            .routes
            .iter()
            .map(|route| route.agent.to_string())
            .collect(),
        &config.models,
        resolve_path,
        &mut proxy_problems,
    );
    problems.extend(proxy_problems.into_iter().map(ConfigProblem::Proxy));
    if let Some(crate::metering::RestoreConfig::Openobserve { auth_env, .. }) = &metering.restore {
        check_env_name(auth_env, &mut problems);
    }

    let agent_ids = config
        .routes
        .iter()
        .map(|route| route.agent.clone())
        .collect();
    let memory_settings = resolve_settings(&config.sessions.memory, &agent_ids);
    if let Err(problem) = &memory_settings {
        problems.push(ConfigProblem::MemoryPolicy {
            route: None,
            problem: problem.clone(),
        });
    }
    let mut routes = Vec::with_capacity(config.routes.len());
    for (index, route) in config.routes.into_iter().enumerate() {
        if !transports_incomplete && !transport_names.contains(&route.transport) {
            problems.push(ConfigProblem::UnknownRouteTransport { route: index });
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
        let github_route =
            transport_kinds.get(&route.transport) == Some(&ChatTransportKind::Github);
        match (&route.github_events, github_route) {
            (Some(events), true)
                if !events.is_empty()
                    && events
                        .iter()
                        .enumerate()
                        .all(|(index, event)| !events[..index].contains(event)) => {}
            (None, false) => {}
            _ => problems.push(ConfigProblem::InvalidGithubEvents { route: index }),
        }
        if github_route && !matches!(route.memory, MemoryConfig::OneShot { .. }) {
            problems.push(ConfigProblem::GithubRequiresOneShot { route: index });
        }
        if route.progress_notes && route.progress_detail == ProgressDetail::Off {
            problems.push(ConfigProblem::ProgressNotesWithoutDetail { route: index });
        }
        let mut indicators = BTreeMap::new();
        if let Some(transport) = transports.iter().find(|t| t.name() == route.transport) {
            for kind in [
                ConversationKind::DirectMessage,
                ConversationKind::GroupDirectMessage,
                ConversationKind::Channel,
                ConversationKind::Thread,
            ]
            .into_iter()
            .filter(|kind| {
                transport.chat_kind().produces(*kind) && conversation.kind.contains(*kind)
            }) {
                let resolved = resolve_liveness(
                    &config.sessions.indicators,
                    transport,
                    &route.indicators,
                    kind,
                    &mut problems,
                    index,
                );
                if resolved.settings.mode == LivenessMode::Native
                    && route.progress_detail == ProgressDetail::Off
                    && resolved.settings.cancel_button
                    && !resolved.settings.stream
                {
                    problems.push(ConfigProblem::UnsupportedRouteIndicator {
                        route: index, surface: "cancelButton",
                        reason: "progressDetail: off requires streaming for a message-backed Stop control",
                    });
                }
                indicators.insert(kind, Arc::new(resolved));
            }
        }
        if route.follow {
            if config.sessions.journal.is_none() {
                problems.push(ConfigProblem::FollowWithoutJournal { route: index });
            }
            let supported = transports.iter().any(|transport| {
                matches!(transport, TransportConfig::DiscordGateway { name, message_content: true, .. } if name == &route.transport)
            });
            if !supported
                || !matches!(&conversation.kind, ConversationKindMatch::Kinds(kinds) if !kinds.is_empty() && kinds.iter().all(|kind| matches!(kind, ConversationKind::Channel | ConversationKind::Thread)))
            {
                problems.push(ConfigProblem::UnsupportedFollowRoute { route: index });
            }
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
                &route.memory,
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
            problems.push(ConfigProblem::UnknownRouteModel { route: index });
        }
        if let Some(model) = &route.model
            && proxy_only.contains(model)
        {
            problems.push(ConfigProblem::ProxyOnlyRouteModel {
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
        match route.limits.job_timeout_ms {
            Some(0) => problems.push(ConfigProblem::InvalidJobTimeout {
                agent: route.agent.to_string(),
            }),
            // `job.deadline_ms` is recorded as an OpenTelemetry int64.
            Some(milliseconds)
                if i64::try_from(milliseconds).is_err()
                    || Instant::now()
                        .checked_add(Duration::from_millis(milliseconds))
                        .is_none() =>
            {
                problems.push(ConfigProblem::UnrepresentableJobTimeout {
                    agent: route.agent.to_string(),
                    job_timeout_ms: milliseconds,
                });
            }
            Some(_) | None => {}
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
        let agent_memory = config.sessions.memory.agents.get(&route.agent);
        let mut memory_knobs = None;
        let memory = match route.memory {
            MemoryConfig::OneShot {} => MemoryPolicy::OneShot,
            MemoryConfig::Persistent {
                scope,
                idle_timeout_ms,
                max_turns,
                max_bytes,
                recall,
                forget_after_ms,
                hot_turns,
                summaries,
                capture,
                selection,
            } => {
                let journaled = config.sessions.journal.is_some();
                let recall = recall.unwrap_or(if journaled {
                    RecallSource::Journal
                } else {
                    RecallSource::None
                });
                let max_bytes = max_bytes.unwrap_or(match recall {
                    RecallSource::Journal => memory::JOURNAL_RECALL_MAX_BYTES,
                    RecallSource::None | RecallSource::Platform => DEFAULT_CONVERSATION_MAX_BYTES,
                });
                if journaled && max_turns > 0 {
                    match resolve_knobs(&KnobInputs {
                        route: &KnobOverrides {
                            hot_turns,
                            summaries,
                            capture,
                            selection,
                        },
                        agent: agent_memory,
                        max_turns,
                        journal_recall: match recall {
                            RecallSource::Journal => JournalRecall::Replays,
                            RecallSource::None | RecallSource::Platform => JournalRecall::Never,
                        },
                        models: &model_names,
                        proxy_only: &proxy_only,
                    }) {
                        Ok(knobs) => memory_knobs = Some(knobs),
                        Err(problem) => problems.push(ConfigProblem::MemoryPolicy {
                            route: Some(index),
                            problem,
                        }),
                    }
                }
                if idle_timeout_ms == 0
                    || max_turns == 0
                    || max_bytes == 0
                    || forget_after_ms == Some(0)
                {
                    problems.push(ConfigProblem::InvalidMemoryBounds {
                        agent: route.agent.to_string(),
                    });
                }
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
            github_events: route.github_events,
            conversation,
            subjects: route.subjects,
            agent: route.agent,
            model: route.model,
            inspect_agent_config: route.inspect_agent_config,
            limits: route.limits,
            progress_detail: route.progress_detail,
            steering: route.steering,
            progress_notes: route.progress_notes,
            memory,
            memory_knobs,
            indicators,
            wakes: route.wakes,
            follow: route.follow,
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
    if config.sessions.max_jobs == 0 {
        problems.push(ConfigProblem::InvalidMaxJobs);
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
    let journal = config
        .sessions
        .journal
        .as_ref()
        .map(|journal| ResolvedJournal {
            dir: resolve_path(journal.path.clone()),
            seal_retention: routes
                .iter()
                .filter_map(|route| route.memory.window().map(|window| window.forget_after))
                .max()
                .unwrap_or(DEFAULT_FORGET_AFTER),
            retention: routes
                .iter()
                .filter_map(|route| match route.memory {
                    MemoryPolicy::Persistent(window) if window.recall == RecallSource::Journal => {
                        Some(window.forget_after)
                    }
                    _ => None,
                })
                .max()
                .unwrap_or(DEFAULT_FORGET_AFTER)
                .max(Duration::from_millis(
                    config.sessions.memory.retention.max_age_ms.get(),
                )),
        });
    let assets = resolve_assets(&config.sessions, &resolve_path, &mut problems);
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
            "dekopon-gatewayd",
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
                service: service.clone(),
                ca_sources,
                extra_ca_bundles: config.extra_ca_bundles,
                model_files: config
                    .models
                    .iter()
                    .map(|model| (model.name().to_owned(), source.clone()))
                    .collect(),
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
                stop_words,
                sessions: config.sessions,
                journal,
                assets,
                memory: memory_settings.expect("memory settings validated before resolution"),
                wakes,
                shutdown_grace,
                telemetry,
                metering,
                proxy,
            })
        }
        _ => Err(ConfigError::Invalid {
            path: source,
            problems,
        }),
    }
}

fn resolve_liveness(
    sessions: &LivenessOverride,
    transport: &TransportConfig,
    route: &LivenessOverride,
    kind: ConversationKind,
    problems: &mut Vec<ConfigProblem>,
    index: usize,
) -> ResolvedLiveness {
    let config = match transport {
        TransportConfig::Fauxlack { liveness, .. }
        | TransportConfig::SlackSocketMode { liveness, .. }
        | TransportConfig::DiscordGateway { liveness, .. }
        | TransportConfig::TelegramLongPoll { liveness, .. }
        | TransportConfig::WhatsappCloudApi { liveness, .. }
        | TransportConfig::Local { liveness, .. } => liveness,
        TransportConfig::GithubWebhook { .. } => return ResolvedLiveness::default(),
    };
    let mut settings = LivenessSettings::default();
    let mut keep_alive = KeepAliveConfig::default();
    let mut templates = TemplateOverrides::default();
    let transport_fields = config.override_fields();
    let layers = [
        Some(sessions),
        Some(&transport_fields),
        config.conversations.get(&kind),
        Some(route),
    ];
    for layer in layers.into_iter().flatten() {
        settings.overlay(layer);
        if let Some(value) = &layer.keep_alive {
            keep_alive = value.clone();
        }
        if let Some(value) = &layer.templates {
            templates.overlay(value);
        }
    }
    if (keep_alive.every_seconds == 0 || keep_alive.at_seconds.contains(&0))
        && !problems.iter().any(|problem| matches!(problem, ConfigProblem::InvalidKeepAlive { route, .. } if *route == index))
    {
        problems.push(ConfigProblem::InvalidKeepAlive {
            transport: transport.name().to_owned(), route: index,
        });
    }
    let (templates, template_problems) =
        Templates::resolve(&templates, STOPPED_REPLY, FAILURE_REPLY);
    for problem in template_problems {
        if !problems.iter().any(|existing| matches!(existing, ConfigProblem::InvalidLivenessTemplate { route, field, placeholder, .. } if *route == index && *field == problem.field.key() && *placeholder == problem.placeholder)) {
            problems.push(ConfigProblem::InvalidLivenessTemplate {
                transport: transport.name().to_owned(), route: index,
                field: problem.field.key(),
                placeholder: problem.placeholder,
            });
        }
    }
    let mut unsupported = |surface, reason| {
        problems.push(ConfigProblem::UnsupportedRouteIndicator {
            route: index,
            surface,
            reason,
        })
    };
    if matches!(transport, TransportConfig::WhatsappCloudApi { .. })
        && settings.working != WorkingReaction::Off
    {
        unsupported(
            "reaction.working",
            "WhatsApp cannot clear a working reaction",
        );
    }
    if settings.mode == LivenessMode::Native {
        if settings.status_text
            && !matches!(
                transport,
                TransportConfig::SlackSocketMode {
                    experience: SlackExperience::Agent,
                    ..
                }
            )
        {
            unsupported(
                "statusText",
                "only Slack's Agent experience has a thread status line",
            );
        }
        if matches!(transport, TransportConfig::WhatsappCloudApi { .. }) {
            if settings.stream {
                unsupported("stream", "WhatsApp messages cannot be edited");
            }
            if settings.cancel_button {
                unsupported("cancelButton", "WhatsApp has no interactive components");
            }
        }
        if matches!(transport, TransportConfig::SlackSocketMode { .. }) && settings.stream {
            unsupported("stream", "Slack does not stream answers");
        }
        if matches!(
            transport,
            TransportConfig::SlackSocketMode {
                experience: SlackExperience::Agent,
                ..
            }
        ) && settings.cancel_button
        {
            unsupported("cancelButton", "Slack Agent renders its own Stop control");
        }
        if settings.cancel_button && !settings.stream && settings.progress == ProgressSurface::Off {
            unsupported(
                "cancelButton",
                "a message-backed Stop control requires progress or streaming",
            );
        }
        if settings.status_text && settings.progress == ProgressSurface::Message {
            unsupported("statusText", "statusText replaces the progress message");
        }
    }
    ResolvedLiveness {
        settings,
        keep_alive: keep_alive_from(&keep_alive),
        templates,
    }
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

/// A rejected name may be a pasted secret, so the refusal never echoes it; the
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
        Err(ConfigProblem::InvalidEnvironmentName)
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
    #[error("gateway projection is not a trusted read-only source: {path}")]
    InsecureProjection { path: PathBuf },
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
    #[error("gateway configuration {} is not strict valid YAML/JSON", path.as_ref().map_or_else(|| "document".to_owned(), |path| path.display().to_string()))]
    Decode {
        path: Option<PathBuf>,
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
    #[error("loaded catalog has an invalid agent identity")]
    InvalidCatalogAgent,
    #[error("gateway source changed during candidate validation")]
    ChangedDuringLoad,
    #[error("gateway source change has no classified live disposition")]
    UnclassifiedSource,
    #[error("gateway changes require restart: {}", categories.join(", "))]
    RestartRequired { categories: Vec<&'static str> },
    #[error("{path}: {}", render_problems(.problems))]
    Invalid {
        path: PathBuf,
        problems: Vec<ConfigProblem>,
    },
}

fn resolve_assets(
    sessions: &SessionsConfig,
    resolve_path: &impl Fn(PathBuf) -> PathBuf,
    problems: &mut Vec<ConfigProblem>,
) -> Option<PathBuf> {
    let assets = match (&sessions.journal, &sessions.assets) {
        (Some(_), None) => {
            problems.push(ConfigProblem::AssetsRequired);
            return None;
        }
        (None, Some(_)) => {
            problems.push(ConfigProblem::AssetsWithoutJournal);
            return None;
        }
        (None, None) => return None,
        (Some(_), Some(assets)) => assets,
    };
    let Some(path) = assets
        .path
        .as_ref()
        .filter(|path| !path.as_os_str().is_empty())
    else {
        problems.push(ConfigProblem::AssetsPathRequired);
        return None;
    };
    let dir = match resolve_directory(&resolve_path(path.clone())) {
        Ok(dir) => dir,
        Err(error) => {
            problems.push(ConfigProblem::AssetsPathUnresolved { kind: error.kind() });
            return None;
        }
    };
    if let Some(journal) = &sessions.journal {
        match resolve_directory(&resolve_path(journal.path.clone())) {
            Ok(journal) => {
                if dir == journal {
                    problems.push(ConfigProblem::AssetsPathEqualsJournal);
                } else if dir.starts_with(&journal) {
                    problems.push(ConfigProblem::AssetsPathInsideJournal);
                } else if journal.starts_with(&dir) {
                    problems.push(ConfigProblem::AssetsPathContainsJournal);
                }
            }
            Err(error) => problems.push(ConfigProblem::AssetsPathUnresolved { kind: error.kind() }),
        }
    }
    Some(dir)
}

fn resolve_directory(path: &Path) -> io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(path) if path.is_dir() => Ok(path),
        Ok(_) => Err(io::Error::from(io::ErrorKind::NotADirectory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
            let parent = resolve_directory(parent)?;
            let name = path
                .file_name()
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
            Ok(parent.join(name))
        }
        Err(error) => Err(error),
    }
}

#[derive(Debug, Error)]
pub enum ConfigProblem {
    #[error("transport {name}: kind fauxlack needs an in-process fauxlack service")]
    FauxlackService { name: String },
    #[error("transport {name}: unknown fauxlack server {server}")]
    FauxlackServer { name: String, server: String },
    #[error("transport {name}: unknown fauxlack bot {bot}")]
    FauxlackBot { name: String, bot: String },
    #[error("transport {name}: fauxlack user {bot} needs bot: true")]
    FauxlackNotBot { name: String, bot: String },
    #[error("transport {name}: duplicate fauxlack attachment {server}/{bot}")]
    FauxlackDuplicate {
        name: String,
        server: String,
        bot: String,
    },
    #[error("extraCABundles permits at most eight files")]
    ModelCaBundleCount,
    #[error("extraCABundles requires absolute paths")]
    ModelCaBundlePath,
    #[error("could not read extraCABundles file ({kind:?})")]
    ModelCaBundleRead { kind: io::ErrorKind },
    #[error("extraCABundles requires regular files of 1 to 65,536 bytes")]
    ModelCaBundleSize,
    #[error("extraCABundles requires nonempty PEM certificate bundles")]
    ModelCaBundlePem,
    #[error("sessions.assets.path is required with sessions.journal")]
    AssetsRequired,
    #[error("sessions.assets requires sessions.journal")]
    AssetsWithoutJournal,
    #[error("sessions.assets.path must be a nonempty path")]
    AssetsPathRequired,
    #[error("sessions.assets.path could not resolve ({kind:?})")]
    AssetsPathUnresolved { kind: io::ErrorKind },
    #[error("sessions.assets.path equals the journal directory")]
    AssetsPathEqualsJournal,
    #[error("sessions.assets.path is inside the journal directory")]
    AssetsPathInsideJournal,
    #[error("sessions.assets.path contains the journal directory")]
    AssetsPathContainsJournal,
    #[error("memory policy for route {route:?}: {problem}")]
    MemoryPolicy {
        route: Option<usize>,
        problem: PolicyProblem,
    },
    #[error(transparent)]
    Metering(crate::metering::MeteringProblem),
    #[error(transparent)]
    Proxy(crate::proxy::ProxyProblem),
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
    #[error("routes[{route}] cannot use indicators.{surface}: {reason}")]
    UnsupportedRouteIndicator {
        route: usize,
        surface: &'static str,
        reason: &'static str,
    },
    #[error(
        "routes[{route}] transport {transport:?} has an indicators.keepAlive bound of zero; everySeconds must be greater than zero and no offset may be zero"
    )]
    InvalidKeepAlive { transport: String, route: usize },
    #[error(
        "routes[{route}] transport {transport:?} indicators template {field} uses {{{placeholder}}}, which it cannot render"
    )]
    InvalidLivenessTemplate {
        route: usize,
        transport: String,
        field: &'static str,
        placeholder: String,
    },
    #[error("routes[{route}]: follow needs sessions.journal")]
    FollowWithoutJournal { route: usize },
    #[error(
        "routes[{route}]: follow requires a Discord channel/thread route with messageContent enabled"
    )]
    UnsupportedFollowRoute { route: usize },
    #[error(
        "stopWords must not be empty and no word may be blank; omit the key to keep the default list"
    )]
    InvalidStopWords,
    #[error(
        "routes[{route}]: `progressNotes: true` needs a progress line, and `progressDetail: off` has none"
    )]
    ProgressNotesWithoutDetail { route: usize },
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
    #[error("GitHub transport {name:?} must bind an explicit nonzero port")]
    InvalidGithubBind { name: String },
    #[error("GitHub transport {name:?} must bind the same address as a WhatsApp transport")]
    GithubBindNotShared { name: String },
    #[error("webhook transport {name:?} conflicts with another listener bind or callback path")]
    ConflictingWebhookMount { name: String },
    #[error(
        "WhatsApp transports {transports:?} each own a listener on {bind}; give each WhatsApp transport its own bind"
    )]
    SharedWhatsappBind {
        bind: SocketAddr,
        transports: Vec<String>,
    },
    #[error("routes[{route}]: githubEvents must be nonempty, unique and GitHub-only")]
    InvalidGithubEvents { route: usize },
    #[error("routes[{route}]: GitHub requires memory.mode: oneShot")]
    GithubRequiresOneShot { route: usize },
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
    #[error("routes[{route}] names an unknown transport")]
    UnknownRouteTransport { route: usize },
    #[error("routes[{route}] names an unknown model")]
    UnknownRouteModel { route: usize },
    #[error(
        "route names model {model:?}, whose kind anthropic is served only through the guest model proxy"
    )]
    ProxyOnlyRouteModel { model: String },
    #[error("route for agent {agent:?} must allow at least one step and one capability call")]
    InvalidRouteLimits { agent: String },
    #[error("session bounds must be greater than zero")]
    InvalidSessionLimits,
    #[error(
        "sessions.maxJobs is 0, which refuses every job; omit it for the {DEFAULT_MAX_JOBS} default"
    )]
    InvalidMaxJobs,
    #[error(
        "route for agent {agent:?} sets limits.jobTimeoutMs to 0, which ends every job the instant it starts; omit it to turn jobs off"
    )]
    InvalidJobTimeout { agent: String },
    #[error(
        "route for agent {agent:?} sets limits.jobTimeoutMs to {job_timeout_ms}, a deadline this host or its job records cannot represent"
    )]
    UnrepresentableJobTimeout { agent: String, job_timeout_ms: u64 },
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
        "transport {transport:?} overrides indicators for conversation kind {kind}, which it never produces"
    )]
    ImpossibleLivenessConversation {
        transport: String,
        kind: &'static str,
    },
    #[error(
        "sessions.maxConversations must be greater than zero; a zero ceiling evicts every conversation immediately and turns a persistent route into an expensive one-shot one"
    )]
    InvalidMaxConversations,
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
        "an environment variable name is not valid; transports and models name variables, never secrets"
    )]
    InvalidEnvironmentName,
    #[error("an endpoint must be {production} or a literal loopback http:// URL")]
    UnsupportedEndpoint { production: String },
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
        ConfigError, DEFAULT_SCRIPT_TIMEOUT_MS, Duration, LivenessMode, LoadMode, ModelConfig,
        ProgressDetail, ProgressSurface, SeenReaction, resolve,
    };
    use crate::progress::{DEFAULT_KEEP_ALIVE_MAX, KeepAlive};

    const PREAMBLE: &str = "apiVersion: dekopon.dev/gatewayd/v1alpha1\n\
         catalogPath: dekopon.yaml\n\
         broker: { socketPath: /run/dekopon/broker.sock, serverUid: 501 }\n\
         models:\n\
         \x20 - name: m\n\
         \x20   kind: openaiCompatible\n\
         \x20   endpoint: http://127.0.0.1:11434/v1\n\
         \x20   model: q\n\
         \x20   timeoutMs: 1000\n\
         \x20   classes: [reasoning]\n";

    #[test]
    fn model_ca_bundle_defaults_and_authored_fields_are_strict() {
        let absent = resolved(TAIL).unwrap();
        let empty = resolved(&format!("{TAIL}extraCABundles: []\n")).unwrap();
        assert!(absent.extra_ca_bundles.is_empty());
        assert!(absent.ca_sources.is_empty());
        assert_eq!(absent.models, empty.models);
        for key in ["extraCaBundles", "unknown"] {
            assert!(matches!(
                super::decode(format!("{PREAMBLE}{TAIL}{key}: []\n").as_bytes()),
                Err(ConfigError::Decode { .. })
            ));
        }
        let authored_internal = PREAMBLE.replace(
            "kind: openaiCompatible",
            "kind: openaiCompatible\n    caBundles: []",
        );
        assert!(matches!(
            super::decode(format!("{authored_internal}{TAIL}").as_bytes()),
            Err(ConfigError::Decode { .. })
        ));
    }

    #[test]
    fn model_ca_bundles_refuse_invalid_files_with_typed_problems() {
        use super::ConfigProblem;
        let root = tempfile::tempdir().unwrap();
        let empty = root.path().join("empty");
        let large = root.path().join("large");
        let invalid = root.path().join("invalid");
        let malformed = root.path().join("malformed");
        let mixed = root.path().join("mixed");
        std::fs::write(&empty, []).unwrap();
        std::fs::write(&large, vec![0; 65_537]).unwrap();
        std::fs::write(&invalid, "SENTINEL-NOT-A-CERTIFICATE").unwrap();
        let malformed_pem = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        std::fs::write(&malformed, malformed_pem).unwrap();
        let valid = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .unwrap()
            .cert
            .pem();
        std::fs::write(&mixed, format!("{valid}{malformed_pem}")).unwrap();
        for (paths, expected) in [
            (
                vec![PathBuf::from("relative.pem")],
                ConfigProblem::ModelCaBundlePath,
            ),
            (
                vec![root.path().join("missing")],
                ConfigProblem::ModelCaBundleRead {
                    kind: std::io::ErrorKind::NotFound,
                },
            ),
            (vec![empty], ConfigProblem::ModelCaBundleSize),
            (vec![large], ConfigProblem::ModelCaBundleSize),
            (vec![invalid], ConfigProblem::ModelCaBundlePem),
            (vec![malformed], ConfigProblem::ModelCaBundlePem),
            (vec![mixed], ConfigProblem::ModelCaBundlePem),
            (
                vec![root.path().join("missing"); 9],
                ConfigProblem::ModelCaBundleCount,
            ),
        ] {
            let list = serde_json::to_string(&paths).unwrap();
            let error = resolved(&format!(
                "{TAIL}extraCABundles: {list}\nsessions: {{maxConcurrent: 0}}\n"
            ))
            .unwrap_err();
            let ConfigError::Invalid { problems, .. } = error else {
                panic!("typed config error expected");
            };
            assert!(problems.len() >= 2);
            assert!(problems.iter().any(
                |problem| std::mem::discriminant(problem) == std::mem::discriminant(&expected)
            ));
            assert!(!format!("{problems:?}").contains("SENTINEL"));
        }
    }

    #[tokio::test]
    async fn model_ca_limits_match_broker_ca_limits() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = crate::tests::temporary();
        let ca = root.path().join("roots.pem");
        let provider = root.path().join("provider.wasm");
        std::fs::write(&provider, b"component fixture").unwrap();
        let broker = root.path().join("broker.yaml");
        let pem = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .unwrap()
            .cert
            .pem();
        for (count, size) in [(8, 65_536), (9, 65_536), (1, 65_537)] {
            let mut padded = pem.clone();
            padded.extend(std::iter::repeat_n('\n', size - pem.len()));
            std::fs::write(&ca, padded).unwrap();
            let paths = vec![&ca; count];
            let config = serde_json::json!({
                "apiVersion": "dekopon.dev/brokerd/v1alpha1",
                "socketPath": root.path().join("broker.sock"),
                "providers": [provider],
                "identities": [{"uid": crate::current_uid(), "principal": "caller",
                    "actor": {"type": "agent", "agent": "model-ca-test"}}],
                "http": {"extraCABundles": paths},
            });
            std::fs::write(&broker, serde_json::to_vec(&config).unwrap()).unwrap();
            std::fs::set_permissions(&broker, std::fs::Permissions::from_mode(0o600)).unwrap();
            let broker_accepts =
                match dekopon_brokerd::telemetry_settings(&broker, crate::current_uid()).await {
                    Ok(_) => true,
                    Err(dekopon_brokerd::BrokerdError::Config(
                        dekopon_brokerd::ConfigError::InvalidHttpsConfiguration,
                    )) => false,
                    Err(error) => panic!("broker fixture failed: {error:?}"),
                };
            let gateway = resolved(&format!(
                "{TAIL}extraCABundles: {}\n",
                serde_json::to_string(&paths).unwrap()
            ));
            assert_eq!(gateway.is_ok(), broker_accepts);
        }
    }

    #[tokio::test]
    async fn model_ca_check_is_path_only_for_files_and_fragments() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("gateway.yaml");
        let directory = root.path().join("fragments");
        std::fs::create_dir(&directory).unwrap();
        let absent = root.path().join("missing.pem");
        for path in [config, directory] {
            let file = if path.is_dir() {
                path.join("gateway.yaml")
            } else {
                path.clone()
            };
            for (bundle, accepted) in [
                (absent.as_path(), true),
                (std::path::Path::new("relative.pem"), false),
            ] {
                std::fs::write(
                    &file,
                    format!("{PREAMBLE}{TAIL}extraCABundles: [{}]\n", bundle.display()),
                )
                .unwrap();
                let (resolved, errors) =
                    super::load_reporting(&path, crate::current_uid(), LoadMode::Check).await;
                if accepted {
                    assert!(errors.is_empty(), "{errors:?}");
                    assert!(resolved.unwrap().models[0].ca_bundles().is_empty());
                } else {
                    assert!(resolved.is_none());
                    assert!(errors.iter().any(|error| matches!(error,
                        ConfigError::Invalid { problems, .. } if problems.iter().any(
                            |problem| matches!(problem, super::ConfigProblem::ModelCaBundlePath)
                        )
                    )));
                }
            }
        }
    }

    #[test]
    fn durable_assets_require_a_journal_and_a_separate_resolvable_path() {
        use super::ConfigProblem;
        for (sessions, expected) in [
            ("journal: {path: journal}", ConfigProblem::AssetsRequired),
            (
                "journal: {path: journal}, assets: null",
                ConfigProblem::AssetsRequired,
            ),
            (
                "assets: {path: assets}",
                ConfigProblem::AssetsWithoutJournal,
            ),
            (
                "journal: {path: journal}, assets: {}",
                ConfigProblem::AssetsPathRequired,
            ),
            (
                "journal: {path: journal}, assets: {path: ''}",
                ConfigProblem::AssetsPathRequired,
            ),
            (
                "journal: {path: journal}, assets: {path: journal}",
                ConfigProblem::AssetsPathEqualsJournal,
            ),
            (
                "journal: {path: journal}, assets: {path: journal/assets}",
                ConfigProblem::AssetsPathInsideJournal,
            ),
            (
                "journal: {path: journal/records}, assets: {path: journal}",
                ConfigProblem::AssetsPathContainsJournal,
            ),
            (
                "journal: {path: journal}, assets: {path: /dev/null/assets}",
                ConfigProblem::AssetsPathUnresolved {
                    kind: std::io::ErrorKind::NotADirectory,
                },
            ),
        ] {
            let error = resolved(&format!(
                "{TAIL}\nsessions: {{{sessions}, maxConcurrent: 0}}\n"
            ))
            .unwrap_err();
            let ConfigError::Invalid { problems, .. } = error else {
                panic!("typed validation expected");
            };
            assert!(
                problems
                    .iter()
                    .any(|problem| std::mem::discriminant(problem)
                        == std::mem::discriminant(&expected)),
                "{sessions}: {problems:?}"
            );
        }
        let config = resolved(&format!(
            "{TAIL}\nsessions: {{journal: {{path: journal}}, assets: {{path: assets}}}}\n"
        ))
        .unwrap();
        assert_eq!(
            config.assets.unwrap(),
            std::fs::canonicalize("/tmp").unwrap().join("assets")
        );
        assert_eq!(config.sessions.asset_retention_bytes, 1_073_741_824);
    }

    #[test]
    fn only_the_gatewayd_api_version_decodes() {
        let valid = format!("{PREAMBLE}{TAIL}");
        assert!(super::decode(valid.as_bytes()).is_ok());
        let old = valid.replace(
            super::CONFIG_API_VERSION,
            concat!("dekopon.dev/deko", "pond/v1alpha1"),
        );
        assert!(matches!(
            super::decode(old.as_bytes()),
            Err(ConfigError::Decode { .. })
        ));
    }

    fn resolved(document: &str) -> Result<super::ResolvedConfig, ConfigError> {
        let config =
            super::decode(format!("{PREAMBLE}{document}").as_bytes()).expect("the fixture decodes");
        resolve(
            config,
            PathBuf::from("/tmp/gatewayd.yaml"),
            &BrokerSocketDiscovery::new(None, None, Some(PathBuf::from("/run/user/501")), None),
            501,
            LoadMode::Boot,
        )
    }

    #[test]
    fn indicator_leaves_resolve_sessions_transport_conversation_then_route() {
        use dekopon_broker_protocol::ConversationKind;
        let config = resolved(
            "sessions:
  indicators: { mode: off, status: off, reaction: { working: off, seen: on } }
transports:
  - name: dev
    kind: local
    socketPath: dev.sock
    indicators:
      mode: native
      reaction: { seen: off }
      conversations: { directMessage: { status: auto, reaction: { seen: on } } }
routes:
  - transport: dev
    conversation: { kind: [directMessage] }
    agent: reviewer
    indicators: { reaction: { seen: off, working: on } }
",
        )
        .unwrap();
        let settings = config.routes[0].indicators[&ConversationKind::DirectMessage].settings;
        assert_eq!(settings.mode, LivenessMode::Native);
        assert_eq!(settings.status, super::IndicatorSurface::Auto);
        assert_eq!(settings.seen, SeenReaction::Off);
        assert_eq!(settings.working, super::WorkingReaction::On);
    }

    #[test]
    fn null_inherits_and_later_on_overrides_earlier_off_but_final_off_suppresses() {
        use dekopon_broker_protocol::ConversationKind;
        let document = "sessions:
  indicators: { mode: off, reaction: { seen: on } }
transports:
  - name: dev
    kind: local
    socketPath: dev.sock
    indicators: { mode: null, reaction: { seen: null } }
routes:
  - transport: dev
    conversation: { kind: [directMessage] }
    agent: reviewer
    indicators: { mode: native, reaction: { seen: null } }
";
        let config = resolved(document).unwrap();
        let settings = config.routes[0].indicators[&ConversationKind::DirectMessage].settings;
        assert_eq!(settings.mode, LivenessMode::Native);
        assert_eq!(settings.seen, SeenReaction::On);
        let inherited = resolved(
            &document
                .replace(
                    "indicators: { mode: null, reaction: { seen: null } }",
                    "indicators: null",
                )
                .replace(
                    "mode: native, reaction: { seen: null }",
                    "mode: native, reaction: null",
                ),
        )
        .unwrap();
        assert_eq!(
            inherited.routes[0].indicators[&ConversationKind::DirectMessage]
                .settings
                .seen,
            SeenReaction::On
        );
        let off =
            resolved(&document.replace("mode: native, reaction", "mode: off, reaction")).unwrap();
        assert_eq!(
            off.routes[0].indicators[&ConversationKind::DirectMessage]
                .settings
                .mode,
            LivenessMode::Off
        );
        assert_eq!(
            off.routes[0].indicators[&ConversationKind::DirectMessage]
                .settings
                .seen,
            SeenReaction::On
        );
    }

    #[test]
    fn route_keep_alive_replaces_cadence_without_resetting_progress() {
        use dekopon_broker_protocol::ConversationKind;
        let config = resolved("transports:
  - name: dev
    kind: local
    socketPath: dev.sock
    indicators: { mode: native, progress: message, keepAlive: { atSeconds: [15], everySeconds: 60, max: 10 } }
routes:
  - transport: dev
    conversation: { kind: [directMessage] }
    agent: reviewer
    indicators: { keepAlive: { atSeconds: [30], everySeconds: 120, max: 5 } }
").unwrap();
        let resolved = &config.routes[0].indicators[&ConversationKind::DirectMessage];
        assert_eq!(resolved.settings.progress, ProgressSurface::Message);
        assert_eq!(resolved.keep_alive.at, vec![Duration::from_secs(30)]);
        assert_eq!(resolved.keep_alive.every, Duration::from_secs(120));
    }

    #[test]
    fn old_keys_and_unknown_indicator_values_refuse_at_the_parse_boundary() {
        let valid = format!("{PREAMBLE}{TAIL}");
        assert!(super::decode(valid.as_bytes()).is_ok());
        for invalid in [
            valid.replace(
                "socketPath: dev.sock",
                "socketPath: dev.sock\n    liveness: {mode: native}",
            ),
            valid.replace(
                "socketPath: dev.sock",
                "socketPath: dev.sock\n    indicators: {classicFallback: reaction}",
            ),
            valid.replace(
                "socketPath: dev.sock",
                "socketPath: dev.sock\n    indicators: {reaction: {seen: maybe}}",
            ),
        ] {
            assert!(matches!(
                super::decode(invalid.as_bytes()),
                Err(ConfigError::Decode { .. })
            ));
        }
    }

    #[test]
    fn effective_indicator_event_has_only_settings_and_no_message_or_identity() {
        let (capture, _guard) = crate::tests::capture_spans();
        let config = resolved(
            "transports:
  - name: dev
    kind: local
    socketPath: dev.sock
routes:
  - transport: dev
    conversation: { kind: [directMessage] }
    agent: reviewer
",
        )
        .unwrap();
        assert_eq!(config.routes.len(), 1);
        config.emit_effective_indicators();
        let events = capture.events_where(
            "dekopon_gatewayd",
            &[("event", "gateway_indicators_effective")],
        );
        assert_eq!(events.len(), 1);
        let record = &events[0];
        assert_eq!(record.field("route"), Some("0"));
        assert_eq!(record.field("seen"), Some("Off"));
        assert_eq!(record.field("working"), Some("Off"));
        assert_eq!(
            record.field("message"),
            Some("gateway.indicators.effective")
        );
        for forbidden in [
            "agent",
            "subject",
            "principal",
            "conversation",
            "conversation_id",
            "payload",
            "token",
            "coordinate",
        ] {
            assert!(record.field(forbidden).is_none());
        }
    }

    #[test]
    fn status_text_defaults_off_and_resolves_slack_agent_conversation_overrides() {
        use dekopon_broker_protocol::ConversationKind;
        for (block, direct, channel) in [
            ("{}", false, false),
            (
                "{ mode: native, progress: off, statusText: true }",
                true,
                true,
            ),
            (
                "{ mode: native, statusText: true, conversations: { channel: { statusText: false } } }",
                true,
                false,
            ),
            (
                "{ mode: native, progress: off, conversations: { channel: { statusText: true } } }",
                false,
                true,
            ),
        ] {
            let config = resolved(&format!("transports:\n  - name: slack\n    kind: slackSocketMode\n    appTokenEnv: A\n    botTokenEnv: B\n    experience: agent\n    indicators: {block}\nroutes:\n  - transport: slack\n    conversation: {{ kind: any }}\n    agent: reviewer\n")).unwrap();
            assert_eq!(
                config.routes[0].indicators[&ConversationKind::DirectMessage]
                    .settings
                    .status_text,
                direct
            );
            assert_eq!(
                config.routes[0].indicators[&ConversationKind::Channel]
                    .settings
                    .status_text,
                channel
            );
        }
    }

    #[test]
    fn fully_resolved_invalid_surfaces_aggregate_across_routes() {
        let error = resolved(
            "transports:
  - name: dev
    kind: local
    socketPath: dev.sock
  - name: slack
    kind: slackSocketMode
    appTokenEnv: A
    botTokenEnv: B
    experience: agent
routes:
  - transport: dev
    conversation: { kind: [directMessage] }
    agent: reviewer
    indicators: { mode: native, statusText: true }
  - transport: slack
    conversation: { kind: [directMessage] }
    agent: reviewer
    indicators: { mode: native, statusText: true, progress: message }
",
        )
        .unwrap_err();
        let ConfigError::Invalid { problems, .. } = error else {
            panic!("semantic errors");
        };
        assert!(problems.iter().any(|p| matches!(
            p,
            super::ConfigProblem::UnsupportedRouteIndicator {
                route: 0,
                surface: "statusText",
                ..
            }
        )));
        assert!(problems.iter().any(|p| matches!(
            p,
            super::ConfigProblem::UnsupportedRouteIndicator {
                route: 1,
                surface: "statusText",
                ..
            }
        )));
    }

    #[test]
    fn invalid_keep_alive_and_template_are_reported_together() {
        let error = resolved("transports:\n  - name: dev\n    kind: local\n    socketPath: dev.sock\nroutes:\n  - transport: dev\n    conversation: { kind: [directMessage] }\n    agent: reviewer\n    indicators: { mode: native, keepAlive: { everySeconds: 0 }, templates: { working: '{word}' } }\n").unwrap_err();
        let ConfigError::Invalid { problems, .. } = error else {
            panic!("expected semantic refusals")
        };
        assert!(problems.iter().any(|problem| matches!(
            problem,
            super::ConfigProblem::InvalidKeepAlive { route: 0, .. }
        )));
        assert!(problems.iter().any(|problem| matches!(
            problem,
            super::ConfigProblem::InvalidLivenessTemplate { route: 0, .. }
        )));
    }

    #[test]
    fn repeated_indicator_errors_name_the_route_once_per_field() {
        let error = resolved("transports:\n  - name: slack\n    kind: slackSocketMode\n    appTokenEnv: A\n    botTokenEnv: B\n    experience: agent\nroutes:\n  - transport: slack\n    conversation: { kind: [channel, thread] }\n    agent: reviewer\n    indicators: { keepAlive: { everySeconds: 0 }, templates: { working: '{word}' } }\n").unwrap_err();
        let ConfigError::Invalid { problems, .. } = error else {
            panic!("semantic errors")
        };
        assert_eq!(
            problems
                .iter()
                .filter(|problem| matches!(
                    problem,
                    super::ConfigProblem::InvalidKeepAlive { route: 0, .. }
                ))
                .count(),
            1
        );
        assert_eq!(
            problems
                .iter()
                .filter(|problem| matches!(
                    problem,
                    super::ConfigProblem::InvalidLivenessTemplate { route: 0, .. }
                ))
                .count(),
            1
        );
    }

    #[test]
    fn follow_requires_a_durable_discord_channel_with_message_content() {
        let route = "routes:\n  - transport: discord\n    conversation: { kind: [channel] }\n    agent: reviewer\n    follow: true\n";
        let discord = "transports:\n  - name: discord\n    kind: discordGateway\n    botTokenEnv: BOT\n    messageContent: true\n";
        let error = resolved(&format!("{discord}{route}")).unwrap_err();
        assert!(
            matches!(error, ConfigError::Invalid { problems, .. } if problems.iter().any(|p| matches!(p, super::ConfigProblem::FollowWithoutJournal { route: 0 })))
        );
        let with_journal = "sessions:\n  journal: { path: journal }\n  assets: { path: assets }\n";
        let config = resolved(&format!("{with_journal}{discord}{route}")).unwrap();
        assert!(config.routes[0].follow);
        let disabled = route.replace("follow: true", "follow: null");
        assert!(
            !resolved(&format!("{with_journal}{discord}{disabled}"))
                .unwrap()
                .routes[0]
                .follow
        );
        let dm = route.replace("channel", "directMessage");
        let error = resolved(&format!("{with_journal}{discord}{dm}")).unwrap_err();
        assert!(
            matches!(error, ConfigError::Invalid { problems, .. } if problems.iter().any(|p| matches!(p, super::ConfigProblem::UnsupportedFollowRoute { route: 0 })))
        );
    }

    #[test]
    fn status_text_rejects_each_message_surface_after_resolving_overrides() {
        for block in [
            "progress: message".to_owned(),
            "conversations: { channel: { progress: message } }".to_owned(),
        ] {
            let error = resolved(&format!("transports:\n  - name: slack\n    kind: slackSocketMode\n    appTokenEnv: A\n    botTokenEnv: B\n    experience: agent\n    indicators: {{ mode: native, statusText: true, {block} }}\nroutes:\n  - transport: slack\n    conversation: {{ kind: any }}\n    agent: reviewer\n")).unwrap_err();
            let ConfigError::Invalid { problems, .. } = error else {
                panic!("expected a semantic refusal")
            };
            assert!(problems.iter().any(|problem| matches!(
                problem,
                super::ConfigProblem::UnsupportedRouteIndicator {
                    surface: "statusText",
                    ..
                }
            )));
        }
    }

    #[test]
    fn slack_cannot_use_indicators_stream_however_it_is_set() {
        for block in [
            "stream: true".to_owned(),
            "conversations: { channel: { stream: true } }".to_owned(),
        ] {
            let error = resolved(&format!(
                "transports:\n  - name: slack\n    kind: slackSocketMode\n    appTokenEnv: A\n    botTokenEnv: B\n    indicators: {{ mode: native, {block} }}\nroutes:\n  - transport: slack\n    conversation: {{ kind: any }}\n    agent: reviewer\n"
            ))
            .unwrap_err();
            let ConfigError::Invalid { problems, .. } = error else {
                panic!("expected a semantic refusal")
            };
            assert!(problems.iter().any(|problem| matches!(
                problem,
                super::ConfigProblem::UnsupportedRouteIndicator {
                    route: 0,
                    surface: "stream",
                    ..
                }
            )));
        }
    }

    #[test]
    fn whatsapp_working_refuses_both_on_and_auto_even_when_typing_exists() {
        for working in ["on", "auto"] {
            let error = resolved(&format!("transports:\n  - name: wa\n    kind: whatsappCloudApi\n    appSecretEnv: C\n    verifyTokenEnv: D\n    accessTokenEnv: E\n    bind: 0.0.0.0:9080\n    callbackPath: /hook\n    wabaId: '1'\n    phoneNumberId: '2'\n    graphApiVersion: v23.0\nroutes:\n  - transport: wa\n    conversation: {{ kind: [directMessage] }}\n    agent: reviewer\n    indicators: {{ mode: native, reaction: {{ working: {working} }} }}\n")).unwrap_err();
            assert!(
                matches!(error, ConfigError::Invalid { problems, .. } if problems.iter().any(|p| matches!(p, super::ConfigProblem::UnsupportedRouteIndicator { route: 0, surface: "reaction.working", .. })))
            );
        }
    }

    #[test]
    fn auto_defaults_preserve_disabled_indicators_and_explicit_overrides() {
        use dekopon_broker_protocol::ConversationKind;
        for (block, mode, progress) in [
            ("", LivenessMode::Off, ProgressSurface::Auto),
            (
                "indicators: { mode: native }",
                LivenessMode::Native,
                ProgressSurface::Auto,
            ),
            (
                "indicators: { mode: native, progress: message }",
                LivenessMode::Native,
                ProgressSurface::Message,
            ),
            (
                "indicators: { mode: native, progress: off }",
                LivenessMode::Native,
                ProgressSurface::Off,
            ),
            (
                "indicators: { mode: native, progress: auto, conversations: { directMessage: { progress: message } } }",
                LivenessMode::Native,
                ProgressSurface::Message,
            ),
            (
                "indicators: { mode: native, progress: message, conversations: { directMessage: { progress: auto } } }",
                LivenessMode::Native,
                ProgressSurface::Auto,
            ),
        ] {
            let config = resolved(&format!(
                "transports:\n  - name: dev\n    kind: local\n    socketPath: dev.sock\n    {block}\nroutes:\n  - transport: dev\n    conversation: {{ kind: [directMessage] }}\n    agent: reviewer\n"
            )).expect("defaults and explicit settings resolve");
            let settings = config.routes[0].indicators[&ConversationKind::DirectMessage].settings;
            assert_eq!(settings.mode, mode, "{block}");
            assert_eq!(settings.progress, progress, "{block}");
            assert!(!settings.stream);
        }
    }

    #[test]
    fn mode_off_suppresses_a_detail_off_stop_button_after_route_override() {
        use dekopon_broker_protocol::ConversationKind;
        let config = resolved(
            "transports:
  - name: dev
    kind: local
    socketPath: dev.sock
    indicators: { mode: native, progress: off, cancelButton: true }
routes:
  - transport: dev
    conversation: { kind: [directMessage] }
    agent: reviewer
    progressDetail: off
    indicators: { mode: off }
",
        )
        .expect("an off route has no active Stop control");
        let settings = config.routes[0].indicators[&ConversationKind::DirectMessage].settings;
        assert_eq!(settings.mode, LivenessMode::Off);
        assert!(
            settings.cancel_button,
            "off suppresses rather than erases leaves"
        );
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
                "transports:\n  - name: dev\n    kind: local\n    socketPath: dev.sock\n    indicators: {{ mode: native, {block} }}\nroutes:\n  - transport: dev\n    conversation: {{ kind: [directMessage] }}\n    agent: reviewer\n    progressDetail: {detail}\n"
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
            "transports:\n  - name: dev\n    kind: local\n    socketPath: dev.sock\n    indicators: { mode: native, progress: off, cancelButton: true }\nroutes:\n  - transport: dev\n    conversation: { kind: [directMessage] }\n    agent: reviewer\n    progressDetail: off\n"
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
    fn an_indicators_block_resolves_to_the_documented_defaults() {
        let config = resolved(
            "transports:\n\
             \x20 - name: dev\n\
             \x20   kind: local\n\
             \x20   socketPath: dev.sock\n\
             \x20   indicators: { mode: native, progress: message }\n\
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
        let liveness =
            &config.routes[0].indicators[&dekopon_broker_protocol::ConversationKind::DirectMessage];
        assert_eq!(liveness.settings.mode, LivenessMode::Native);
        assert_eq!(liveness.settings.progress, ProgressSurface::Message);
        assert_eq!(liveness.settings.seen, SeenReaction::Off);
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

    const HEAD: &str = "apiVersion: dekopon.dev/gatewayd/v1alpha1\n\
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
                501,
                LoadMode::Boot
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
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
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
            LoadMode::Boot,
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
                LoadMode::Boot,
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
                ModelConfig::ChatgptSubscription { .. }
                | ModelConfig::Openrouter { .. }
                | ModelConfig::Anthropic { .. } => None,
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
    fn agent_config_inspection_is_offered_only_where_a_route_writes_true() {
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
             \x20   inspectAgentConfig: true\n",
        )
        .expect("a well-formed configuration resolves");

        assert!(!config.routes[0].inspect_agent_config);
        assert!(config.routes[1].inspect_agent_config);
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
