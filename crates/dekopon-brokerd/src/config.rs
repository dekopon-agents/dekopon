use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use dekopon_broker::{
    AttestorGrant, AuthenticatedContext, BrokerLimits, ChatMemoryConfig, ConstraintSet,
    ContextError, DEFAULT_MAX_AUDIT_LINE_BYTES, DEFAULT_MAX_AUDIT_RECORDS,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerHostOptions, LockedProviderSource};
use dekopon_broker_protocol::{
    DEFAULT_IO_TIMEOUT, DEFAULT_MAX_FRAME_BYTES, FrameLimits, HARD_MAX_FRAME_BYTES, ProtocolError,
};
use dekopon_core::{
    Actor, CapabilityId, ExternalSubject, PROVIDER_COMPONENT_EXTENSION, PrincipalId,
};
use dekopon_storage_host::StorageLimits;
use dekopon_telemetry::{ExporterSettings, TelemetryError, Transport};
use serde::Deserialize;
use thiserror::Error;
use tokio::io::AsyncReadExt as _;

pub use crate::HARD_MAX_PROVIDERS;
use crate::provider_manager;

pub const CONFIG_API_VERSION: &str = "dekopon.dev/brokerd/v1alpha1";
pub const HARD_MAX_CONFIG_BYTES: usize = 1024 * 1024;
/// Ceiling on the owner-only Cedar policy file, matching `dekopon-policy`'s own source bound.
pub const HARD_MAX_POLICY_BYTES: usize = 1024 * 1024;
pub const HARD_MAX_CONNECTIONS: usize = 1_024;
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
    /// Optional owner-only credentials file resolved into the broker's credential store.
    ///
    /// Absent means the broker holds no provider credentials, and any rule naming one fails
    /// construction. The secret values live only in that file — never here.
    #[serde(default)]
    pub credentials_path: Option<PathBuf>,
    /// Optional owner-only public-DRN to private-source map.
    ///
    /// It coexists with legacy implicit credentials. Loading validates descriptors without network
    /// access; one selected source is resolved only after both capability and `secret.use` policy
    /// decisions allow an invocation.
    #[serde(default)]
    pub secret_map_path: Option<PathBuf>,
    /// Legacy directly named component files or directories.
    #[serde(default)]
    pub providers: Vec<PathBuf>,
    /// Managed provider activation lock and content-addressed store.
    ///
    /// Mutually exclusive with `providers`. Registry access is never attempted while this
    /// configuration is loaded; an offline `dekopon-brokerd provider` command materializes it.
    #[serde(default)]
    pub provider_set: Option<ManagedProviderSetConfig>,
    /// Optional broker-owned directory for Wasmtime's persistent compilation cache.
    ///
    /// Absent means Cranelift recompiles every provider at every start, inside whatever startup
    /// budget the deployment allows. Present means compiled code is read back from this directory,
    /// so it must be broker-owned and writable by nobody else — a deployment points it at durable
    /// state such as `/var/lib/dekopon/compile-cache`.
    #[serde(default)]
    pub compile_cache_path: Option<PathBuf>,
    /// Whether configuration naming something no loaded provider offers refuses startup.
    ///
    /// Defaults to `false`, which warns and continues so a deployment can ship policy and
    /// constraint sets that anticipate a provider it has not dropped in yet. Set it for a
    /// deployment whose provider set is fixed, where a mismatch means someone made a mistake.
    ///
    /// Tolerating grants nothing either way: a capability nothing routes is denied
    /// `unconstrained-capability` at invocation regardless of this setting.
    #[serde(default)]
    pub strict: bool,
    pub identities: Vec<PeerIdentity>,
    /// Owner-controlled subject-to-principal mappings consulted for attested proposals.
    #[serde(default)]
    pub identity_mappings: Vec<IdentityMapping>,
    /// Owner-only Cedar policy file evaluated for every authorization decision.
    ///
    /// Absent means an empty policy set, which permits nothing. Required once any constraint set
    /// exists, because a deployment that declares executable capabilities and no policy is a
    /// configuration mistake rather than a deliberate deny-everything.
    #[serde(default)]
    pub policies_path: Option<PathBuf>,
    /// Execution constraints per capability, keyed by capability identifier.
    ///
    /// A capability with no entry is not deployable: the broker refuses it before consulting
    /// policy, and refuses to start if policy could ever permit it.
    #[serde(default)]
    pub constraint_sets: BTreeMap<CapabilityId, ConstraintSet>,
    #[serde(default)]
    pub host_limits: HostLimitsConfig,
    #[serde(default)]
    pub broker_limits: BrokerLimits,
    #[serde(default)]
    pub server_limits: ServerLimitsConfig,
    /// Optional broker-owned provider storage. Presence requires every field.
    #[serde(default)]
    pub storage: Option<StorageConfig>,
    /// Optional all-or-nothing durable chat-memory surface.
    #[serde(default)]
    pub chat_memory: Option<ChatMemoryConfig>,
    /// Optional OTLP export. Absent means the broker exports no telemetry.
    #[serde(default)]
    pub telemetry: Option<TelemetryConfig>,
}

/// Paths for one generated provider lock and its immutable blob store.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ManagedProviderSetConfig {
    /// Generated lock consumed as trusted startup input.
    pub lock_path: PathBuf,
    /// Store containing `blobs/sha256/<component-digest>.wasm`.
    pub store_path: PathBuf,
}

/// Strict broker-owned provider-storage paths and ceilings.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StorageConfig {
    pub root_path: PathBuf,
    pub namespace_key_path: PathBuf,
    #[serde(flatten)]
    pub limits: StorageLimits,
}

/// Broker-owned OTLP export settings.
///
/// The credential is deliberately absent. Ingest authentication is read by the OpenTelemetry SDK
/// from `OTEL_EXPORTER_OTLP_HEADERS`, so a token never enters this owner-readable configuration
/// file, the process command line, or any span attribute.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TelemetryConfig {
    /// OTLP receiver endpoint.
    pub endpoint: String,
    /// Wire transport: `grpc` or `http`.
    pub transport: Transport,
    /// OpenTelemetry service name attached to broker spans.
    pub service_name: String,
    /// Timeout for each OTLP export and the final shutdown flush.
    pub export_timeout_ms: u64,
    /// Whether spans carry provider payloads and HTTP URLs.
    ///
    /// Enabling this declares the telemetry sink in scope for the data this broker handles. It
    /// never exposes a credential: `Redacted` values render their marker in either mode.
    pub telemetry_payloads: bool,
}

/// Broker telemetry after validation.
#[derive(Clone, Debug)]
pub struct ResolvedTelemetry {
    /// Exporter transport and endpoint.
    pub settings: ExporterSettings,
    /// Whether spans carry provider payloads and HTTP URLs.
    pub telemetry_payloads: bool,
}

impl TelemetryConfig {
    fn resolve(&self) -> Result<ExporterSettings, ConfigError> {
        ExporterSettings::new(
            &self.endpoint,
            self.transport,
            &self.service_name,
            "dekopon-brokerd",
            env!("CARGO_PKG_VERSION"),
            Duration::from_millis(self.export_timeout_ms),
        )
        .map_err(|source| ConfigError::Telemetry { source })
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PeerIdentity {
    pub uid: u32,
    pub principal: PrincipalId,
    pub actor: Actor,
    /// Optional authority to attest external subjects inside canonical namespaces.
    #[serde(default)]
    pub attestor: Option<AttestorGrant>,
}

impl PeerIdentity {
    pub fn context(&self) -> Result<AuthenticatedContext, ContextError> {
        AuthenticatedContext::new(self.principal.clone(), self.actor.clone())
    }
}

/// One owner-controlled mapping from a canonical external subject to a stable principal.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct IdentityMapping {
    /// Canonical subject, e.g. `slack.t0123abc.u9xyz` or `tel.16034700182`.
    pub subject: ExternalSubject,
    /// The stable principal that subject resolves to.
    pub principal: PrincipalId,
}

/// Per-invocation Wasmtime ceilings and the optional aggregate memory budget.
///
/// Every field defaults independently to the value [`HostLimitsConfig::default`] gives it — the
/// same value an entirely absent `hostLimits` block produces. Setting `maxTotalMemoryBytes` alone
/// is therefore one line rather than fifteen, which is what makes the aggregate budget something a
/// deployment actually sets. The cross-field checks in [`resolve`] still run on the merged result.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
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
    /// Aggregate guest linear memory reservable across concurrently live provider stores.
    ///
    /// `maxMemoryBytes` bounds one invocation. Nothing bounds all of them at once unless this is
    /// set, so the worst case is `serverLimits.maxConnections` times `maxMemoryBytes` — well past
    /// a small container's limit at the defaults. Setting it turns an OOM kill into a refusal.
    ///
    /// Deliberately absent from the authority commitment: it is a concurrency budget, not a
    /// ceiling an authorization could narrow, and changing it must not rotate stored authority.
    #[serde(default)]
    pub max_total_memory_bytes: Option<usize>,
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
            max_total_memory_bytes: None,
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
        .map_err(|source| ConfigError::InvalidFrameLimits { source })
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
    pub credentials_path: Option<PathBuf>,
    pub secret_map_path: Option<PathBuf>,
    pub providers: Vec<PathBuf>,
    /// Expected component identities when providers came from a generated lock.
    pub locked_providers: Option<Vec<LockedProviderSource>>,
    pub strict: bool,
    pub identities: Vec<PeerIdentity>,
    pub identity_mappings: Vec<IdentityMapping>,
    pub policies_path: Option<PathBuf>,
    pub policies: String,
    pub constraint_sets: BTreeMap<CapabilityId, ConstraintSet>,
    pub host_limits: BrokerHostLimits,
    pub host_options: BrokerHostOptions,
    /// Worst-case concurrent guest memory: `maxConnections` times `maxMemoryBytes`.
    pub worst_case_guest_memory_bytes: usize,
    pub broker_limits: BrokerLimits,
    pub server_limits: ServerLimitsConfig,
    pub storage: Option<StorageConfig>,
    pub chat_memory: Option<ChatMemoryConfig>,
    pub telemetry: Option<ResolvedTelemetry>,
}

#[allow(
    clippy::map_err_ignore,
    reason = "the policy file's FromUtf8Error would carry its offending bytes back into a log line; PolicyNotUtf8 names the file and deliberately stops there"
)]
pub async fn load(
    path: impl AsRef<Path>,
    expected_uid: u32,
) -> Result<ResolvedConfig, ConfigError> {
    let path = absolute(path.as_ref())?;
    let bytes = read_owner_only(&path, expected_uid, HARD_MAX_CONFIG_BYTES).await?;
    let config = serde_yaml::from_slice::<BrokerdConfig>(&bytes)
        .map_err(|source| ConfigError::Decode { source })?;
    let mut resolved = resolve(config, path, expected_uid).await?;
    // The policy file gets the configuration's own hygiene: owner-owned, single-link, not
    // group/world writable, no symlink following, byte-capped. It is trusted input in exactly the
    // same sense the configuration is, so it is read under exactly the same rules.
    if let Some(policies_path) = resolved.policies_path.clone() {
        let bytes = read_owner_only(&policies_path, expected_uid, HARD_MAX_POLICY_BYTES).await?;
        resolved.policies = String::from_utf8(bytes).map_err(|_| ConfigError::PolicyNotUtf8 {
            path: policies_path,
        })?;
    }
    Ok(resolved)
}

/// Reads one owner-only, single-link, byte-capped regular file without following symlinks.
async fn read_owner_only(
    path: &Path,
    expected_uid: u32,
    maximum: usize,
) -> Result<Vec<u8>, ConfigError> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .await
        .map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    let metadata = file.metadata().await.map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(ConfigError::NotRegular {
            path: path.to_path_buf(),
        });
    }
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    if metadata.uid() != expected_uid
        || metadata.permissions().mode() & 0o022 != 0
        || metadata.nlink() != 1
    {
        return Err(ConfigError::InsecureFile {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > maximum as u64 {
        return Err(ConfigError::TooLarge {
            length: metadata.len(),
            maximum,
        });
    }
    let mut bytes = Vec::new();
    file.take((maximum + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > maximum {
        return Err(ConfigError::TooLarge {
            length: bytes.len() as u64,
            maximum,
        });
    }
    Ok(bytes)
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

/// Expands one configured provider entry into the component files it names.
///
/// A regular file is itself. A directory is every `*.wasm` directly inside it — not recursively, so
/// a nested directory is a place to park something, not a place it loads from — **in filename
/// order**. That sort is load-bearing rather than tidiness: the registry builds its capability
/// route table in load order, so readdir order would make two runs over an identical directory
/// disagree about which provider claimed a duplicate capability.
///
/// A directory is held to the same standard as every other trusted input this file reads: owned by
/// the expected UID and not group- or world-writable. A directory anyone can write to is a
/// directory anyone can add a provider to, and a provider is code this broker compiles and runs.
/// Each file the scan yields is checked again on its own by `socket::validate_owned_file` before
/// anything is loaded.
fn expand_provider_entry(path: &Path, expected_uid: u32) -> Result<Vec<PathBuf>, ConfigError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    // `symlink_metadata` rather than `metadata`: the path is already canonical, so a symlink here
    // would be one planted between canonicalization and now.
    let metadata = std::fs::symlink_metadata(path).map_err(|source| ConfigError::ResolvePath {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_dir() {
        return Ok(vec![path.to_path_buf()]);
    }
    if metadata.uid() != expected_uid || metadata.permissions().mode() & 0o022 != 0 {
        return Err(ConfigError::InsecureProviderDirectory {
            path: path.to_path_buf(),
        });
    }

    let entries = std::fs::read_dir(path).map_err(|source| ConfigError::ResolvePath {
        path: path.to_path_buf(),
        source,
    })?;
    let mut providers = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ConfigError::ResolvePath {
            path: path.to_path_buf(),
            source,
        })?;
        let candidate = entry.path();
        if candidate
            .extension()
            .is_some_and(|extension| extension == PROVIDER_COMPONENT_EXTENSION)
            && entry
                .file_type()
                .map_err(|source| ConfigError::ResolvePath {
                    path: candidate.clone(),
                    source,
                })?
                .is_file()
        {
            providers.push(candidate);
        }
    }
    if providers.is_empty() {
        return Err(ConfigError::EmptyProviderDirectory {
            path: path.to_path_buf(),
        });
    }
    providers.sort();
    Ok(providers)
}

async fn resolve(
    config: BrokerdConfig,
    source: PathBuf,
    expected_uid: u32,
) -> Result<ResolvedConfig, ConfigError> {
    if config.provider_set.is_some() && !config.providers.is_empty() {
        return Err(ConfigError::MixedProviderSources);
    }
    if config.provider_set.is_none() && config.providers.is_empty() {
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
    let canonical = |path: Option<PathBuf>| {
        path.map(|path| {
            let unresolved = resolve_path(path);
            std::fs::canonicalize(&unresolved).map_err(|source| ConfigError::ResolvePath {
                path: unresolved,
                source,
            })
        })
        .transpose()
    };
    let credentials_path = canonical(config.credentials_path)?;
    // Preserve the configured final component so the secret-map loader's O_NOFOLLOW check can
    // actually reject a symlink rather than receiving the target canonicalization erased it into.
    let secret_map_path = config
        .secret_map_path
        .map(|path| resolve_future_path(resolve_path(path)))
        .transpose()?;
    let policies_path = canonical(config.policies_path)?;
    // Wasmtime creates the cache directory itself and requires an absolute path, so this resolves
    // the parent rather than requiring the directory to already exist.
    let compile_cache_path = config
        .compile_cache_path
        .map(|path| resolve_future_path(resolve_path(path)))
        .transpose()?;
    let storage = config
        .storage
        .map(|mut storage| {
            // Storage paths preserve their configured spelling until every original ancestor has
            // been walked through retained `openat(...NOFOLLOW...)` descriptors. Canonicalizing a
            // parent here would erase precisely the symlink the storage boundary must reject.
            storage.root_path =
                dekopon_storage_host::resolve_storage_root_path(&resolve_path(storage.root_path))
                    .map_err(|source| ConfigError::StoragePath { source })?;
            storage.namespace_key_path = dekopon_storage_host::resolve_namespace_key_path(
                &resolve_path(storage.namespace_key_path),
            )
            .map_err(|source| ConfigError::StoragePath { source })?;
            if storage.namespace_key_path.starts_with(&storage.root_path)
                || storage.namespace_key_path == storage.root_path
            {
                return Err(ConfigError::StorageStateCollision);
            }
            storage
                .limits
                .validate()
                .map_err(|source| ConfigError::InvalidStorage { source })?;
            Ok::<_, ConfigError>(storage)
        })
        .transpose()?;
    let chat_memory = config.chat_memory;
    if chat_memory.is_some() && storage.is_none() {
        return Err(ConfigError::ChatMemoryWithoutStorage);
    }
    if let (Some(memory), Some(storage)) = (&chat_memory, &storage) {
        #[allow(
            clippy::map_err_ignore,
            reason = "every rejection here is the unit variant BrokerBuildError::InvalidChatMemory, which says nothing ConfigError::InvalidChatMemory does not"
        )]
        memory
            .validate(&storage.limits)
            .map_err(|_| ConfigError::InvalidChatMemory)?;
    }
    let managed_provider_paths = config
        .provider_set
        .map(|managed| {
            let unresolved_lock = resolve_path(managed.lock_path);
            let lock_path = std::fs::canonicalize(&unresolved_lock).map_err(|source| {
                ConfigError::ResolvePath {
                    path: unresolved_lock,
                    source,
                }
            })?;
            let unresolved_store = resolve_path(managed.store_path);
            let store_path = std::fs::canonicalize(&unresolved_store).map_err(|source| {
                ConfigError::ResolvePath {
                    path: unresolved_store,
                    source,
                }
            })?;
            Ok::<_, ConfigError>((lock_path, store_path))
        })
        .transpose()?;
    let locked_providers = match &managed_provider_paths {
        Some((lock_path, store_path)) => Some(
            provider_manager::load_locked_sources(lock_path, store_path, expected_uid)
                .await
                .map_err(|source| ConfigError::ProviderLock { source })?,
        ),
        None => None,
    };
    let mut provider_set = BTreeSet::new();
    let mut providers = locked_providers
        .as_ref()
        .map(|providers| {
            providers
                .iter()
                .map(|provider| provider.path().to_path_buf())
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| Vec::with_capacity(config.providers.len()));
    for entry in config.providers {
        let unresolved = resolve_path(entry);
        let entry =
            std::fs::canonicalize(&unresolved).map_err(|source| ConfigError::ResolvePath {
                path: unresolved,
                source,
            })?;
        for provider in expand_provider_entry(&entry, expected_uid)? {
            if !provider_set.insert(provider.clone()) {
                return Err(ConfigError::DuplicateProviderPath { path: provider });
            }
            providers.push(provider);
        }
    }
    // The pre-expansion bound above limits what this file may say; this one limits what it
    // actually resolves to, which is what the component host will be asked to compile.
    if providers.len() > HARD_MAX_PROVIDERS {
        return Err(ConfigError::TooManyProviders {
            maximum: HARD_MAX_PROVIDERS,
        });
    }
    if providers.is_empty() {
        return Err(ConfigError::NoProviders);
    }
    let mut reserved = vec![
        source.clone(),
        socket_path.clone(),
        audit_path.clone(),
        checkpoint_path.clone(),
        checkpoint_lock_path.clone(),
        checkpoint_temporary_path,
    ];
    if let Some(credentials_path) = &credentials_path {
        reserved.push(credentials_path.clone());
    }
    if let Some(secret_map_path) = &secret_map_path {
        reserved.push(secret_map_path.clone());
    }
    if let Some(policies_path) = &policies_path {
        reserved.push(policies_path.clone());
    }
    if let Some((lock_path, store_path)) = &managed_provider_paths {
        reserved.push(lock_path.clone());
        if reserved.iter().any(|path| {
            path == store_path || path.starts_with(store_path) || store_path.starts_with(path)
        }) {
            return Err(ConfigError::ProviderStateCollision);
        }
    }
    if let Some(storage) = &storage {
        reserved.push(storage.namespace_key_path.clone());
        if audit_path.starts_with(&storage.root_path)
            || checkpoint_path.starts_with(&storage.root_path)
            || storage.root_path == audit_path.parent().unwrap_or(Path::new("/"))
        {
            return Err(ConfigError::StorageStateCollision);
        }
    }
    if let Some(storage) = &storage
        && (reserved
            .iter()
            .any(|path| path == &storage.root_path || path.starts_with(&storage.root_path))
            || providers
                .iter()
                .any(|path| path == &storage.root_path || path.starts_with(&storage.root_path))
            || managed_provider_paths
                .as_ref()
                .is_some_and(|(_, store_path)| {
                    store_path == &storage.root_path
                        || store_path.starts_with(&storage.root_path)
                        || storage.root_path.starts_with(store_path)
                }))
    {
        // Initialization owns every entry under the root and rejects unknown ones. Refuse this at
        // config resolution rather than letting a future socket or audit file poison the storage
        // layout after the first successful start.
        return Err(ConfigError::StorageStateCollision);
    }
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
        if let Some(attestor) = &identity.attestor {
            attestor
                .validate()
                .map_err(|source| ConfigError::Attestor { source })?;
        }
    }
    let mut mapped_subjects = BTreeSet::new();
    for mapping in &config.identity_mappings {
        if !mapped_subjects.insert(mapping.subject.canonical()) {
            return Err(ConfigError::DuplicateSubject {
                subject: mapping.subject.canonical(),
            });
        }
    }
    // A deployment that declares executable capabilities and no policy file would start and refuse
    // everything, which is a configuration mistake dressed as deny-by-default. Every other check
    // the old reachability validation performed now happens in policy-world construction: an
    // undeclared principal, provider, or capability refuses `PolicyEngine::new` outright.
    if !config.constraint_sets.is_empty() && policies_path.is_none() {
        return Err(ConfigError::MissingPoliciesPath);
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
    // Per-store limits bound one invocation; the connection ceiling decides how many of those can
    // exist at once. Naming the product here is what makes an operator budget it against the
    // container limit instead of discovering it as an OOM kill.
    let worst_case_guest_memory_bytes = config
        .server_limits
        .max_connections
        .checked_mul(host_limits.max_memory_bytes)
        .ok_or(ConfigError::InvalidHostLimits)?;
    if config
        .host_limits
        .max_total_memory_bytes
        .is_some_and(|maximum| maximum < host_limits.max_memory_bytes)
    {
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
    if let Some(memory) = &chat_memory {
        #[allow(
            clippy::map_err_ignore,
            reason = "every rejection here is the unit variant BrokerBuildError::InvalidChatMemory, which says nothing ConfigError::InvalidChatMemory does not"
        )]
        memory
            .validate_host_limits(&host_limits)
            .map_err(|_| ConfigError::InvalidChatMemory)?;
        #[allow(
            clippy::map_err_ignore,
            reason = "TryFromIntError carries only out-of-range, and a maxResultBytes wider than this platform's usize is exactly the bound InvalidChatMemory names"
        )]
        let result =
            usize::try_from(memory.max_result_bytes).map_err(|_| ConfigError::InvalidChatMemory)?;
        if result
            .checked_add(MINIMUM_RESPONSE_OVERHEAD_BYTES)
            .is_none_or(|bytes| bytes > frame_limits.max_frame_bytes)
        {
            return Err(ConfigError::InvalidChatMemory);
        }
    }
    let mut minimum_shutdown = host_limits
        .max_timeout
        .checked_add(frame_limits.io_timeout)
        .and_then(|duration| duration.checked_add(frame_limits.io_timeout))
        .ok_or(ConfigError::InvalidServerLimits)?;
    if let Some(storage) = &storage {
        minimum_shutdown = minimum_shutdown
            .checked_add(Duration::from_millis(storage.limits.lock_timeout_ms))
            .and_then(|duration| {
                duration.checked_add(Duration::from_millis(storage.limits.finalization_budget_ms))
            })
            .ok_or(ConfigError::InvalidServerLimits)?;
    }
    if config.server_limits.shutdown_grace() < minimum_shutdown {
        return Err(ConfigError::ShortShutdownGrace);
    }

    let telemetry = config
        .telemetry
        .as_ref()
        .map(|telemetry| {
            Ok::<_, ConfigError>(ResolvedTelemetry {
                settings: telemetry.resolve()?,
                telemetry_payloads: telemetry.telemetry_payloads,
            })
        })
        .transpose()?;

    Ok(ResolvedConfig {
        source,
        socket_path,
        audit_path,
        checkpoint_path,
        checkpoint_lock_path,
        broker_principal: config.broker_principal,
        policy_revision: config.policy_revision,
        credentials_path,
        secret_map_path,
        providers,
        locked_providers,
        strict: config.strict,
        identities: config.identities,
        identity_mappings: config.identity_mappings,
        policies_path,
        policies: String::new(),
        constraint_sets: config.constraint_sets,
        host_limits,
        host_options: BrokerHostOptions {
            compile_cache_dir: compile_cache_path,
            max_total_memory_bytes: config.host_limits.max_total_memory_bytes,
        },
        worst_case_guest_memory_bytes,
        broker_limits: config.broker_limits,
        server_limits: config.server_limits,
        storage,
        chat_memory,
        telemetry,
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
    #[error("broker configuration must use either providers or providerSet, not both")]
    MixedProviderSources,
    #[error("managed provider lock or store is invalid")]
    ProviderLock {
        /// Strict lock, store, or blob hygiene failure.
        #[source]
        source: provider_manager::ProviderManagerError,
    },
    #[error("managed provider store must be disjoint from broker-owned state paths")]
    ProviderStateCollision,
    /// A provider directory was group- or world-writable, or owned by another user.
    #[error(
        "provider directory {path} is not owned by this user or is group/world writable; anyone \
         who can write it can add a provider this broker would execute"
    )]
    InsecureProviderDirectory {
        /// The offending directory.
        path: PathBuf,
    },
    /// A configured provider directory held no `*.wasm` component.
    #[error("provider directory {path} contains no *.wasm component")]
    EmptyProviderDirectory {
        /// The empty directory.
        path: PathBuf,
    },
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
    #[error(
        "constraintSets requires a policiesPath; a broker with capabilities and no policy \
             would refuse every request"
    )]
    MissingPoliciesPath,
    #[error("broker policy file is not valid UTF-8: {path}")]
    PolicyNotUtf8 { path: PathBuf },
    #[error("peer attestor grant is invalid")]
    Attestor {
        #[source]
        source: dekopon_broker::BrokerBuildError,
    },
    #[error("identity mapping duplicates subject {subject:?}")]
    DuplicateSubject { subject: String },
    #[error("server limits must be positive and within hard ceilings")]
    InvalidServerLimits,
    #[error("invalid broker frame limits")]
    InvalidFrameLimits {
        /// Which frame bound was rejected: a zero or over-ceiling maximum, or a zero I/O timeout.
        #[source]
        source: ProtocolError,
    },
    #[error("host timeout must be positive")]
    InvalidHostLimits,
    #[error("could not safely resolve a configured provider storage path")]
    StoragePath {
        /// The offending path and the reason it was refused.
        #[source]
        source: dekopon_storage_host::StorageHostError,
    },
    #[error("invalid provider storage limits")]
    InvalidStorage {
        /// Which storage field, value, or relationship was rejected.
        #[source]
        source: dekopon_storage_host::StorageConfigError,
    },
    #[error("chatMemory requires storage")]
    ChatMemoryWithoutStorage,
    #[error("chat-memory bounds do not compose with frame, Wasm, host, and storage limits")]
    InvalidChatMemory,
    #[error("provider storage root/key and broker-owned files must be disjoint")]
    StorageStateCollision,
    #[error("response frame maximum must be at least {minimum} bytes for configured host output")]
    SmallResponseFrame { minimum: usize },
    #[error("shutdown grace must cover host, storage lock/finalization, and two frame deadlines")]
    ShortShutdownGrace,
    #[error("invalid broker telemetry configuration")]
    Telemetry {
        #[source]
        source: TelemetryError,
    },
}
