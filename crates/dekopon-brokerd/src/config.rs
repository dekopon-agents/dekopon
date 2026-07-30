use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use dekopon_broker::{
    AuthenticatedContext, BrokerLimits, ContextError, DEFAULT_MAX_AUDIT_LINE_BYTES,
    DEFAULT_MAX_AUDIT_RECORDS, PolicyRule,
};
use dekopon_broker_host::BrokerHostLimits;
use dekopon_broker_protocol::{
    DEFAULT_IO_TIMEOUT, DEFAULT_MAX_FRAME_BYTES, FrameLimits, HARD_MAX_FRAME_BYTES,
};
use dekopon_core::{Actor, PrincipalId};
use serde::Deserialize;
use thiserror::Error;
use tokio::io::AsyncReadExt as _;

pub const CONFIG_API_VERSION: &str = "dekopon.dev/brokerd/v1alpha1";
pub const HARD_MAX_CONFIG_BYTES: usize = 1024 * 1024;
pub const HARD_MAX_CONNECTIONS: usize = 1_024;
pub const HARD_MAX_PROVIDERS: usize = 64;
pub const MINIMUM_RESPONSE_OVERHEAD_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_CONNECTIONS: usize = 64;
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum ConfigApiVersion {
    #[serde(rename = "dekopon.dev/brokerd/v1alpha1")]
    V1Alpha1,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrokerdConfig {
    pub api_version: ConfigApiVersion,
    pub socket_path: PathBuf,
    pub audit_path: PathBuf,
    pub checkpoint_path: PathBuf,
    pub checkpoint_lock_path: PathBuf,
    pub broker_principal: PrincipalId,
    pub policy_revision: String,
    pub providers: Vec<PathBuf>,
    pub identities: Vec<PeerIdentity>,
    pub rules: Vec<PolicyRule>,
    #[serde(default)]
    pub host_limits: HostLimitsConfig,
    #[serde(default)]
    pub broker_limits: BrokerLimits,
    #[serde(default)]
    pub server_limits: ServerLimitsConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PeerIdentity {
    pub uid: u32,
    pub principal: PrincipalId,
    pub actor: Actor,
}

impl PeerIdentity {
    pub fn context(&self) -> Result<AuthenticatedContext, ContextError> {
        AuthenticatedContext::new(self.principal.clone(), self.actor.clone())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HostLimitsConfig {
    pub max_memory_bytes: usize,
    pub max_table_elements: usize,
    pub max_instances: usize,
    pub max_tables: usize,
    pub max_memories: usize,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    pub max_http_requests: u32,
    pub max_http_request_bytes: u64,
    pub max_http_response_bytes: u64,
    pub max_http_headers: usize,
    pub max_http_header_bytes: usize,
    pub fuel: u64,
    pub max_timeout_ms: u64,
}

impl Default for HostLimitsConfig {
    fn default() -> Self {
        let defaults = BrokerHostLimits::default();
        Self {
            max_memory_bytes: defaults.max_memory_bytes,
            max_table_elements: defaults.max_table_elements,
            max_instances: defaults.max_instances,
            max_tables: defaults.max_tables,
            max_memories: defaults.max_memories,
            max_input_bytes: defaults.max_input_bytes,
            max_output_bytes: defaults.max_output_bytes,
            max_http_requests: defaults.max_http_requests,
            max_http_request_bytes: defaults.max_http_request_bytes,
            max_http_response_bytes: defaults.max_http_response_bytes,
            max_http_headers: defaults.max_http_headers,
            max_http_header_bytes: defaults.max_http_header_bytes,
            fuel: defaults.fuel,
            max_timeout_ms: u64::try_from(defaults.max_timeout.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

impl HostLimitsConfig {
    pub fn runtime(&self) -> BrokerHostLimits {
        BrokerHostLimits {
            max_memory_bytes: self.max_memory_bytes,
            max_table_elements: self.max_table_elements,
            max_instances: self.max_instances,
            max_tables: self.max_tables,
            max_memories: self.max_memories,
            max_input_bytes: self.max_input_bytes,
            max_output_bytes: self.max_output_bytes,
            max_http_requests: self.max_http_requests,
            max_http_request_bytes: self.max_http_request_bytes,
            max_http_response_bytes: self.max_http_response_bytes,
            max_http_headers: self.max_http_headers,
            max_http_header_bytes: self.max_http_header_bytes,
            fuel: self.fuel,
            max_timeout: Duration::from_millis(self.max_timeout_ms),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ServerLimitsConfig {
    pub max_frame_bytes: usize,
    pub io_timeout_ms: u64,
    pub max_connections: usize,
    pub audit_max_records: usize,
    pub audit_max_line_bytes: usize,
    pub shutdown_grace_ms: u64,
}

impl Default for ServerLimitsConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            io_timeout_ms: u64::try_from(DEFAULT_IO_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            audit_max_records: DEFAULT_MAX_AUDIT_RECORDS,
            audit_max_line_bytes: DEFAULT_MAX_AUDIT_LINE_BYTES,
            shutdown_grace_ms: u64::try_from(DEFAULT_SHUTDOWN_GRACE.as_millis())
                .unwrap_or(u64::MAX),
        }
    }
}

impl ServerLimitsConfig {
    pub fn frame_limits(&self) -> Result<FrameLimits, ConfigError> {
        FrameLimits {
            max_frame_bytes: self.max_frame_bytes,
            io_timeout: Duration::from_millis(self.io_timeout_ms),
        }
        .validate()
        .map_err(|_| ConfigError::InvalidServerLimits)
    }

    pub fn shutdown_grace(&self) -> Duration {
        Duration::from_millis(self.shutdown_grace_ms)
    }
}

#[derive(Debug)]
pub struct ResolvedConfig {
    pub source: PathBuf,
    pub socket_path: PathBuf,
    pub audit_path: PathBuf,
    pub checkpoint_path: PathBuf,
    pub checkpoint_lock_path: PathBuf,
    pub broker_principal: PrincipalId,
    pub policy_revision: String,
    pub providers: Vec<PathBuf>,
    pub identities: Vec<PeerIdentity>,
    pub rules: Vec<PolicyRule>,
    pub host_limits: BrokerHostLimits,
    pub broker_limits: BrokerLimits,
    pub server_limits: ServerLimitsConfig,
}

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
    let config = serde_yaml::from_slice::<BrokerdConfig>(&bytes)
        .map_err(|source| ConfigError::Decode { source })?;
    resolve(config, path)
}

fn absolute(path: &Path) -> Result<PathBuf, ConfigError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|directory| directory.join(path))
        .map_err(|source| ConfigError::CurrentDirectory { source })
}

fn resolve_future_path(path: PathBuf) -> Result<PathBuf, ConfigError> {
    let parent = path.parent().ok_or(ConfigError::MissingParent)?;
    let name = path.file_name().ok_or(ConfigError::MissingFileName)?;
    let parent = std::fs::canonicalize(parent).map_err(|source| ConfigError::ResolvePath {
        path: parent.to_path_buf(),
        source,
    })?;
    Ok(parent.join(name))
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> Result<PathBuf, ConfigError> {
    let name = path.file_name().ok_or(ConfigError::MissingFileName)?;
    let mut sibling = name.to_os_string();
    sibling.push(suffix);
    Ok(path.with_file_name(sibling))
}

fn resolve(config: BrokerdConfig, source: PathBuf) -> Result<ResolvedConfig, ConfigError> {
    if config.providers.is_empty() {
        return Err(ConfigError::NoProviders);
    }
    if config.providers.len() > HARD_MAX_PROVIDERS {
        return Err(ConfigError::TooManyProviders {
            maximum: HARD_MAX_PROVIDERS,
        });
    }
    if config.identities.is_empty() {
        return Err(ConfigError::NoIdentities);
    }
    let source_parent = source.parent().ok_or(ConfigError::MissingParent)?;
    let base = std::fs::canonicalize(source_parent).map_err(|source| ConfigError::ResolvePath {
        path: source_parent.to_path_buf(),
        source,
    })?;
    let resolve_path = |path: PathBuf| {
        if path.is_absolute() {
            path
        } else {
            base.join(path)
        }
    };
    let source = resolve_future_path(source)?;
    let socket_path = resolve_future_path(resolve_path(config.socket_path))?;
    let audit_path = resolve_future_path(resolve_path(config.audit_path))?;
    let checkpoint_path = resolve_future_path(resolve_path(config.checkpoint_path))?;
    let checkpoint_lock_path = resolve_future_path(resolve_path(config.checkpoint_lock_path))?;
    let checkpoint_temporary_path = sibling_with_suffix(&checkpoint_path, ".tmp")?;
    let mut provider_set = BTreeSet::new();
    let mut providers = Vec::with_capacity(config.providers.len());
    for provider in config.providers {
        let unresolved = resolve_path(provider);
        let provider =
            std::fs::canonicalize(&unresolved).map_err(|source| ConfigError::ResolvePath {
                path: unresolved,
                source,
            })?;
        if !provider_set.insert(provider.clone()) {
            return Err(ConfigError::DuplicateProviderPath { path: provider });
        }
        providers.push(provider);
    }
    let reserved = [
        source.clone(),
        socket_path.clone(),
        audit_path.clone(),
        checkpoint_path.clone(),
        checkpoint_lock_path.clone(),
        checkpoint_temporary_path,
    ];
    if reserved.iter().collect::<BTreeSet<_>>().len() != reserved.len()
        || providers
            .iter()
            .any(|provider| reserved.iter().any(|path| path == provider))
    {
        return Err(ConfigError::ConflictingPaths);
    }

    let mut uids = BTreeSet::new();
    for identity in &config.identities {
        if !uids.insert(identity.uid) {
            return Err(ConfigError::DuplicateUid { uid: identity.uid });
        }
        identity
            .context()
            .map_err(|source| ConfigError::Identity { source })?;
    }
    for rule in &config.rules {
        if !config
            .identities
            .iter()
            .any(|identity| identity.principal == rule.principal && identity.actor == rule.actor)
        {
            return Err(ConfigError::UnmappedRule);
        }
    }
    if config.server_limits.max_connections == 0
        || config.server_limits.max_connections > HARD_MAX_CONNECTIONS
        || config.server_limits.audit_max_records == 0
        || config.server_limits.audit_max_line_bytes == 0
        || config.server_limits.audit_max_line_bytes > HARD_MAX_FRAME_BYTES
        || config.server_limits.shutdown_grace_ms == 0
    {
        return Err(ConfigError::InvalidServerLimits);
    }
    let frame_limits = config.server_limits.frame_limits()?;
    let host_limits = config.host_limits.runtime();
    if host_limits.max_timeout.is_zero() {
        return Err(ConfigError::InvalidHostLimits);
    }
    let maximum_response = host_limits
        .max_output_bytes
        .checked_add(MINIMUM_RESPONSE_OVERHEAD_BYTES)
        .ok_or(ConfigError::InvalidHostLimits)?;
    if frame_limits.max_frame_bytes < maximum_response {
        return Err(ConfigError::SmallResponseFrame {
            minimum: maximum_response,
        });
    }
    let minimum_shutdown = host_limits
        .max_timeout
        .checked_add(frame_limits.io_timeout)
        .and_then(|duration| duration.checked_add(frame_limits.io_timeout))
        .ok_or(ConfigError::InvalidServerLimits)?;
    if config.server_limits.shutdown_grace() < minimum_shutdown {
        return Err(ConfigError::ShortShutdownGrace);
    }

    Ok(ResolvedConfig {
        source,
        socket_path,
        audit_path,
        checkpoint_path,
        checkpoint_lock_path,
        broker_principal: config.broker_principal,
        policy_revision: config.policy_revision,
        providers,
        identities: config.identities,
        rules: config.rules,
        host_limits,
        broker_limits: config.broker_limits,
        server_limits: config.server_limits,
    })
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not determine the current directory")]
    CurrentDirectory {
        #[source]
        source: io::Error,
    },
    #[error("could not read broker configuration at {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("broker configuration path is not a regular non-symlink file: {path}")]
    NotRegular { path: PathBuf },
    #[error(
        "broker configuration must be single-link, owned by the server UID, and not group/world writable: {path}"
    )]
    InsecureFile { path: PathBuf },
    #[error("broker configuration is {length} bytes; maximum is {maximum}")]
    TooLarge { length: u64, maximum: usize },
    #[error("broker configuration is not strict valid YAML/JSON")]
    Decode {
        #[source]
        source: serde_yaml::Error,
    },
    #[error("configured path has no parent")]
    MissingParent,
    #[error("configured socket, audit, or checkpoint path has no file name")]
    MissingFileName,
    #[error("could not resolve configured path: {path}")]
    ResolvePath {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("broker configuration must name at least one provider")]
    NoProviders,
    #[error("broker configuration has too many providers; maximum is {maximum}")]
    TooManyProviders { maximum: usize },
    #[error("broker configuration must map at least one peer identity")]
    NoIdentities,
    #[error(
        "configuration, socket, audit, checkpoint, lock, temporary, and provider paths must not conflict"
    )]
    ConflictingPaths,
    #[error("provider component path is repeated: {path}")]
    DuplicateProviderPath { path: PathBuf },
    #[error("peer UID {uid} is mapped more than once")]
    DuplicateUid { uid: u32 },
    #[error("peer identity is not transport-bindable")]
    Identity {
        #[source]
        source: ContextError,
    },
    #[error("every policy rule must match one configured peer identity exactly")]
    UnmappedRule,
    #[error("server limits must be positive and within hard ceilings")]
    InvalidServerLimits,
    #[error("host timeout must be positive")]
    InvalidHostLimits,
    #[error("response frame maximum must be at least {minimum} bytes for configured host output")]
    SmallResponseFrame { minimum: usize },
    #[error("shutdown grace must cover one host deadline and two complete frame deadlines")]
    ShortShutdownGrace,
}
