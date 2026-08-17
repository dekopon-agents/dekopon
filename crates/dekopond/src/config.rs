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
    path::{Path, PathBuf},
    time::Duration,
};

use dekopon_broker_protocol::{
    DEFAULT_IO_TIMEOUT, DEFAULT_MAX_FRAME_BYTES, FrameLimits, ProtocolError,
};
use dekopon_core::AgentId;
use dekopon_telemetry::{ExporterSettings, TelemetryError, Transport};
use serde::Deserialize;
use thiserror::Error;
use tokio::io::AsyncReadExt as _;

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
/// The only non-loopback Slack origin this daemon will talk to.
pub const SLACK_ENDPOINT: &str = "https://slack.com";
/// The only non-loopback Telegram origin this daemon will talk to.
pub const TELEGRAM_ENDPOINT: &str = "https://api.telegram.org";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum ConfigApiVersion {
    #[serde(rename = "dekopon.dev/dekopond/v1alpha1")]
    V1Alpha1,
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
        /// Overridable only to `https://slack.com` or a literal loopback HTTP URL, for tests.
        #[serde(default)]
        endpoint: Option<String>,
    },
    /// Telegram long polling: the poll is the wakeup and advancing the offset is the ack.
    TelegramLongPoll {
        name: String,
        bot_token_env: String,
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
            | Self::TelegramLongPoll { name, .. }
            | Self::Local { name, .. } => name,
        }
    }

    /// Stable low-cardinality label for lifecycle logs.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::SlackSocketMode { .. } => "slackSocketMode",
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
    },
}

impl ModelConfig {
    /// The operator-chosen name routes refer to.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::OpenaiCompatible { name, .. } | Self::ChatgptSubscription { name, .. } => name,
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
    DirectMessage,
    /// One named channel or group, where the bot must additionally be @-mentioned.
    Channel { channel: String },
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
    #[serde(default)]
    pub limits: RouteLimits,
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
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            max_concurrent: DEFAULT_MAX_CONCURRENT_SESSIONS,
            reply_on_busy: true,
        }
    }
}

const fn default_max_concurrent() -> usize {
    DEFAULT_MAX_CONCURRENT_SESSIONS
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
    pub routes: Vec<RouteConfig>,
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
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(&path)
        .await
        .map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
    let metadata = file.metadata().await.map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(ConfigError::NotRegular { path });
    }
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    if metadata.uid() != expected_uid
        || metadata.permissions().mode() & 0o022 != 0
        || metadata.nlink() != 1
    {
        return Err(ConfigError::InsecureFile { path });
    }
    if metadata.len() > HARD_MAX_CONFIG_BYTES as u64 {
        return Err(ConfigError::TooLarge {
            length: metadata.len(),
            maximum: HARD_MAX_CONFIG_BYTES,
        });
    }
    let mut bytes = Vec::new();
    file.take((HARD_MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
    if bytes.len() > HARD_MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            length: bytes.len() as u64,
            maximum: HARD_MAX_CONFIG_BYTES,
        });
    }
    let config = serde_yaml::from_slice::<DekopondConfig>(&bytes)
        .map_err(|source| ConfigError::Decode { source })?;
    resolve(config, path, &SocketDiscovery::from_process(), expected_uid)
}

fn absolute(path: &Path) -> Result<PathBuf, ConfigError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    env::current_dir()
        .map(|directory| directory.join(path))
        .map_err(|source| ConfigError::CurrentDirectory { source })
}

/// Broker socket discovery, mirroring `dekopon-run`'s documented precedence exactly.
///
/// No candidate is probed for existence: the socket is simply absent whenever the broker is not
/// running, so the tightest resolved tier is trusted and the startup probe reports against it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SocketDiscovery {
    environment: Option<PathBuf>,
    xdg_runtime_dir: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl SocketDiscovery {
    fn from_process() -> Self {
        Self {
            environment: env::var_os("DEKOPON_BROKER_SOCKET")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            xdg_runtime_dir: env::var_os("XDG_RUNTIME_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            home: env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
        }
    }

    /// Creates an injectable discovery context for deterministic tests.
    #[must_use]
    pub const fn new(
        environment: Option<PathBuf>,
        xdg_runtime_dir: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> Self {
        Self {
            environment,
            xdg_runtime_dir,
            home,
        }
    }

    fn resolve(&self) -> Result<PathBuf, ConfigError> {
        if let Some(path) = &self.environment {
            return Ok(path.clone());
        }
        if let Some(root) = &self.xdg_runtime_dir {
            return Ok(root.join("dekopon/broker.sock"));
        }
        if let Some(home) = &self.home {
            return Ok(home.join(".local/run/dekopon/broker.sock"));
        }
        Err(ConfigError::BrokerSocketUnresolved)
    }
}

pub(crate) fn resolve(
    config: DekopondConfig,
    source: PathBuf,
    discovery: &SocketDiscovery,
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
                endpoint,
            } => {
                validate_env_name(&app_token_env)?;
                validate_env_name(&bot_token_env)?;
                let endpoint = validate_endpoint(endpoint, SLACK_ENDPOINT)?;
                TransportConfig::SlackSocketMode {
                    name,
                    app_token_env,
                    bot_token_env,
                    endpoint: Some(endpoint),
                }
            }
            TransportConfig::TelegramLongPoll {
                name,
                bot_token_env,
                endpoint,
            } => {
                validate_env_name(&bot_token_env)?;
                let endpoint = validate_endpoint(endpoint, TELEGRAM_ENDPOINT)?;
                TransportConfig::TelegramLongPoll {
                    name,
                    bot_token_env,
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

    for route in &config.routes {
        if !transport_names.contains(&route.transport) {
            return Err(ConfigError::UnknownRouteTransport {
                transport: route.transport.clone(),
            });
        }
        if let Some(model) = &route.model
            && !model_names.contains(model)
        {
            return Err(ConfigError::UnknownRouteModel {
                model: model.clone(),
            });
        }
        if route.limits.max_steps == 0 || route.limits.max_capability_calls == 0 {
            return Err(ConfigError::InvalidRouteLimits {
                agent: route.agent.to_string(),
            });
        }
    }

    if config.sessions.max_concurrent == 0 {
        return Err(ConfigError::InvalidSessionLimits);
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
        None => discovery.resolve()?,
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
        routes: config.routes,
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
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
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
    InsecureFile { path: PathBuf },
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
    #[error("route names unknown transport {transport:?}")]
    UnknownRouteTransport { transport: String },
    #[error("route names unknown model {model:?}")]
    UnknownRouteModel { model: String },
    #[error("route for agent {agent:?} must allow at least one step and one capability call")]
    InvalidRouteLimits { agent: String },
    #[error("session bounds must be greater than zero")]
    InvalidSessionLimits,
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
