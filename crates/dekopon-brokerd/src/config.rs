use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use dekopon_broker::{
    AttestorGrant, AuthenticatedContext, BrokerLimits, ChatMemoryConfig, ConstraintSet,
    ContextError,
};
use dekopon_broker_host::{
    BrokerHostLimits, BrokerHostOptions, DEFAULT_MAX_TOTAL_MEMORY_BYTES, LockedProviderSource,
    NonPublicHttpsAuthority, PlaintextHostError, PlaintextHosts,
};
use dekopon_broker_protocol::{
    DEFAULT_IO_TIMEOUT, DEFAULT_MAX_FRAME_BYTES, FrameLimits, ProtocolError,
};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, FileHygieneError, FileTier, GroupId,
    PROVIDER_COMPONENT_EXTENSION, PrincipalId, ProviderId, read_trusted_file,
};
use dekopon_storage_host::StorageLimits;
use dekopon_telemetry::{ExporterSettings, TelemetryError, Transport};
use serde::Deserialize;
use thiserror::Error;

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
    pub broker_principal: PrincipalId,
    pub policy_revision: String,
    #[serde(default)]
    pub credentials_path: Option<PathBuf>,
    /// Loading validates descriptors without network access; an actual secret source is resolved
    /// only after both capability and secret.use policy decisions permit the invocation.
    #[serde(default)]
    pub secret_map_path: Option<PathBuf>,
    #[serde(default)]
    pub providers: Vec<PathBuf>,
    #[serde(default)]
    pub provider_set: Option<ManagedProviderSetConfig>,
    #[serde(default)]
    pub compile_on_load: bool,
    /// Tolerating a startup mismatch never grants anything at runtime: a capability nothing routes
    /// is still denied unconstrained-capability at invocation regardless of this setting.
    #[serde(default)]
    pub strict: bool,
    pub identities: Vec<PeerIdentity>,
    #[serde(default)]
    pub principals: BTreeMap<PrincipalId, PrincipalConfig>,
    #[serde(default)]
    pub agents: BTreeMap<AgentId, AgentBindingConfig>,
    #[serde(default)]
    pub policies_path: Option<PathBuf>,
    #[serde(default)]
    pub constraint_sets: BTreeMap<CapabilityId, ConstraintSet>,
    #[serde(default)]
    pub provider_settings: BTreeMap<ProviderId, serde_json::Value>,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub host_limits: HostLimitsConfig,
    #[serde(default)]
    pub broker_limits: BrokerLimits,
    #[serde(default)]
    pub server_limits: ServerLimitsConfig,
    #[serde(default)]
    pub assets: Option<AssetsConfig>,
    #[serde(default)]
    pub storage: Option<StorageConfig>,
    #[serde(default)]
    pub chat_memory: Option<ChatMemoryConfig>,
    #[serde(default)]
    pub telemetry: Option<TelemetryConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct HttpConfig {
    /// Listing a host here doesn't grant access by itself: it must still be named in a constraint
    /// set's allowedHosts with allowPlaintextLoopback set; this only permits plaintext once already
    /// authorized.
    pub plaintext_hosts: Vec<String>,
    #[serde(rename = "extraCABundles")]
    pub extra_ca_bundles: Vec<PathBuf>,
    pub non_public_https: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ManagedProviderSetConfig {
    pub lock_path: PathBuf,
    pub store_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AssetsConfig {
    pub root_path: PathBuf,
    pub max_in_flight_bytes: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StorageConfig {
    pub root_path: PathBuf,
    #[serde(flatten)]
    pub limits: StorageLimits,
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
    #[serde(default)]
    pub attestor: Option<AttestorGrant>,
}

impl PeerIdentity {
    pub fn context(&self) -> Result<AuthenticatedContext, ContextError> {
        AuthenticatedContext::new(self.principal.clone(), self.actor.clone())
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentBindingConfig {
    pub credentials: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct PrincipalConfig {
    pub subjects: Vec<ExternalSubject>,
    pub groups: BTreeSet<GroupId>,
}

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
    /// This memory ceiling is deliberately left out of the authority commitment; including it would
    /// rotate stored authority whenever the concurrency budget changes.
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
            max_total_memory_bytes: Some(DEFAULT_MAX_TOTAL_MEMORY_BYTES),
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
    pub shutdown_grace_ms: u64,
}

impl Default for ServerLimitsConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            io_timeout_ms: u64::try_from(DEFAULT_IO_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
            max_connections: DEFAULT_MAX_CONNECTIONS,
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
    pub broker_principal: PrincipalId,
    pub policy_revision: String,
    pub credentials_path: Option<PathBuf>,
    pub secret_map_path: Option<PathBuf>,
    pub providers: Vec<PathBuf>,
    pub locked_providers: Option<Vec<LockedProviderSource>>,
    pub strict: bool,
    pub identities: Vec<PeerIdentity>,
    pub principals: BTreeMap<PrincipalId, PrincipalConfig>,
    pub agents: BTreeMap<AgentId, AgentBindingConfig>,
    pub policies_path: Option<PathBuf>,
    pub policies: String,
    pub constraint_sets: BTreeMap<CapabilityId, ConstraintSet>,
    pub host_limits: BrokerHostLimits,
    pub host_options: BrokerHostOptions,
    pub plaintext_hosts: PlaintextHosts,
    pub worst_case_guest_memory_bytes: usize,
    pub broker_limits: BrokerLimits,
    pub server_limits: ServerLimitsConfig,
    pub storage: Option<StorageConfig>,
    pub assets: Option<AssetsConfig>,
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
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return load_directory(path, expected_uid).await;
    }
    let bytes = read_owner_only(&path, expected_uid, HARD_MAX_CONFIG_BYTES).await?;
    let config = serde_yaml::from_slice::<BrokerdConfig>(&bytes)
        .map_err(|source| ConfigError::Decode { source })?;
    if !config.constraint_sets.is_empty() && config.policies_path.is_none() {
        return Err(ConfigError::MissingPoliciesPath);
    }
    let mut resolved = resolve(config, path, expected_uid).await?;
    if let Some(policies_path) = resolved.policies_path.clone() {
        let bytes = read_owner_only(&policies_path, expected_uid, HARD_MAX_POLICY_BYTES).await?;
        resolved.policies = String::from_utf8(bytes).map_err(|_| ConfigError::PolicyNotUtf8 {
            path: policies_path,
        })?;
    }
    Ok(resolved)
}

/// Keys whose values are named collections: fragments union them by entry name. Every other key is
/// set by exactly one fragment, so no fragment can override another and file order never matters.
const MERGED_BY_NAME: [&str; 4] = ["principals", "agents", "constraintSets", "providerSettings"];
const CONCATENATED: [&str; 2] = ["identities", "providers"];
const FRAGMENT_EXTENSION: &str = "yaml";
const POLICY_EXTENSION: &str = "cedar";

async fn load_directory(
    directory: PathBuf,
    expected_uid: u32,
) -> Result<ResolvedConfig, ConfigError> {
    let fragments = scan_directory(&directory, expected_uid, FRAGMENT_EXTENSION)?;
    let policy_files = scan_directory(&directory, expected_uid, POLICY_EXTENSION)?;
    let first = fragments
        .first()
        .cloned()
        .ok_or_else(|| ConfigError::EmptyConfigDirectory {
            path: directory.clone(),
        })?;
    let mut parsed = Vec::with_capacity(fragments.len());
    for path in fragments {
        let bytes = read_owner_only(&path, expected_uid, HARD_MAX_CONFIG_BYTES).await?;
        let mapping = serde_yaml::from_slice::<serde_yaml::Mapping>(&bytes).map_err(|source| {
            ConfigError::DecodeFragment {
                path: path.clone(),
                source,
            }
        })?;
        parsed.push((path, mapping));
    }
    let merged = merge_fragments(parsed)?;
    let config = serde_yaml::from_value::<BrokerdConfig>(serde_yaml::Value::Mapping(merged))
        .map_err(|source| ConfigError::Decode { source })?;
    if config.policies_path.is_some() {
        return Err(ConfigError::PoliciesPathInDirectory);
    }
    if !config.constraint_sets.is_empty() && policy_files.is_empty() {
        return Err(ConfigError::MissingPoliciesPath);
    }
    let mut resolved = resolve(config, first, expected_uid).await?;
    let mut policies = String::new();
    for path in policy_files {
        let bytes = read_owner_only(&path, expected_uid, HARD_MAX_POLICY_BYTES).await?;
        let text = String::from_utf8(bytes).map_err(|_| ConfigError::PolicyNotUtf8 { path })?;
        policies.push_str(&text);
        policies.push('\n');
        if policies.len() > HARD_MAX_POLICY_BYTES {
            return Err(ConfigError::TooLarge {
                length: policies.len() as u64,
                maximum: HARD_MAX_POLICY_BYTES,
            });
        }
    }
    resolved.policies = policies;
    Ok(resolved)
}

fn scan_directory(
    directory: &Path,
    expected_uid: u32,
    extension: &str,
) -> Result<Vec<PathBuf>, ConfigError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let metadata =
        std::fs::symlink_metadata(directory).map_err(|source| ConfigError::ResolvePath {
            path: directory.to_path_buf(),
            source,
        })?;
    if metadata.uid() != expected_uid || metadata.permissions().mode() & 0o022 != 0 {
        return Err(ConfigError::InsecureConfigDirectory {
            path: directory.to_path_buf(),
        });
    }
    let entries = std::fs::read_dir(directory).map_err(|source| ConfigError::ResolvePath {
        path: directory.to_path_buf(),
        source,
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ConfigError::ResolvePath {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|candidate| candidate == extension)
        {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn merge_fragments(
    fragments: Vec<(PathBuf, serde_yaml::Mapping)>,
) -> Result<serde_yaml::Mapping, ConfigError> {
    use serde_yaml::Value;

    let mut merged = serde_yaml::Mapping::new();
    let mut owners = BTreeMap::<String, Vec<PathBuf>>::new();
    let mut versions = BTreeSet::new();
    for (path, fragment) in fragments {
        for (key, value) in fragment {
            let name = key.as_str().unwrap_or_default().to_owned();
            if name == "apiVersion" {
                versions.insert(serde_yaml::to_string(&value).unwrap_or_default());
                merged.insert(key, value);
                continue;
            }
            let collection =
                CONCATENATED.contains(&name.as_str()) || MERGED_BY_NAME.contains(&name.as_str());
            match (merged.get_mut(&key), value) {
                (Some(Value::Sequence(into)), Value::Sequence(from)) if collection => {
                    into.extend(from);
                }
                (Some(Value::Mapping(into)), Value::Mapping(from))
                    if MERGED_BY_NAME.contains(&name.as_str()) =>
                {
                    for (entry, entry_value) in from {
                        let owner = format!("{name}.{}", entry.as_str().unwrap_or_default());
                        owners.entry(owner).or_default().push(path.clone());
                        into.insert(entry, entry_value);
                    }
                }
                (None, value) => {
                    if let (true, Value::Mapping(entries)) =
                        (MERGED_BY_NAME.contains(&name.as_str()), &value)
                    {
                        for entry in entries.keys() {
                            let owner = format!("{name}.{}", entry.as_str().unwrap_or_default());
                            owners.entry(owner).or_default().push(path.clone());
                        }
                    }
                    owners.entry(name).or_default().push(path.clone());
                    merged.insert(key, value);
                }
                (Some(_), _) => {
                    owners.entry(name).or_default().push(path.clone());
                }
            }
        }
    }
    let conflicts = owners
        .into_iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|(key, files)| FragmentConflict { key, files })
        .collect::<Vec<_>>();
    if !conflicts.is_empty() {
        return Err(ConfigError::FragmentConflicts { conflicts });
    }
    if versions.len() > 1 {
        return Err(ConfigError::MixedApiVersions);
    }
    Ok(merged)
}

#[derive(Debug, Eq, PartialEq)]
pub struct FragmentConflict {
    pub key: String,
    pub files: Vec<PathBuf>,
}

impl std::fmt::Display for FragmentConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let files = self
            .files
            .iter()
            .map(|file| file.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        write!(formatter, "{} is set in {files}", self.key)
    }
}

fn render_conflicts(conflicts: &[FragmentConflict]) -> String {
    conflicts
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

async fn read_owner_only(
    path: &Path,
    expected_uid: u32,
    maximum: usize,
) -> Result<Vec<u8>, ConfigError> {
    let owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        read_trusted_file(&owned, expected_uid, FileTier::NotWorldWritable, maximum)
    })
    .await
    .map_err(|join| ConfigError::Read {
        path: path.to_path_buf(),
        source: io::Error::other(join),
    })?
    .map_err(|error| trusted_read_error(path, error))
}

fn trusted_read_error(path: &Path, error: FileHygieneError) -> ConfigError {
    match error {
        FileHygieneError::NotRegular { path, .. } => ConfigError::NotRegular { path },
        FileHygieneError::TooLarge {
            length, maximum, ..
        } => ConfigError::TooLarge { length, maximum },
        FileHygieneError::Io { path, source } => ConfigError::Read { path, source },
        insecure => ConfigError::InsecureFile {
            path: path.to_path_buf(),
            source: insecure,
        },
    }
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

/// Entries are scanned in filename order deliberately: the registry builds its capability route
/// table in load order, so an unsorted scan would make identical directories disagree about which
/// provider claims a duplicate capability.
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
    // The final path component is left uncanonicalized so the secret-map loader's O_NOFOLLOW check
    // can actually reject a symlink, rather than being handed a target canonicalization already
    // resolved away.
    let secret_map_path = config
        .secret_map_path
        .map(|path| resolve_future_path(resolve_path(path)))
        .transpose()?;
    let policies_path = canonical(config.policies_path)?;
    let storage = config
        .storage
        .map(|mut storage| {
            storage.root_path =
                dekopon_storage_host::resolve_storage_root_path(&resolve_path(storage.root_path))
                    .map_err(|source| ConfigError::StoragePath { source })?;
            storage
                .limits
                .validate()
                .map_err(|source| ConfigError::InvalidStorage { source })?;
            Ok::<_, ConfigError>(storage)
        })
        .transpose()?;
    let assets = config
        .assets
        .map(|mut assets| {
            assets.root_path =
                dekopon_storage_host::resolve_storage_root_path(&resolve_path(assets.root_path))
                    .map_err(|source| ConfigError::AssetsPath { source })?;
            Ok::<_, ConfigError>(assets)
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
    if providers.len() > HARD_MAX_PROVIDERS {
        return Err(ConfigError::TooManyProviders {
            maximum: HARD_MAX_PROVIDERS,
        });
    }
    if providers.is_empty() {
        return Err(ConfigError::NoProviders);
    }
    let mut reserved = vec![source.clone(), socket_path.clone()];
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
        return Err(ConfigError::StorageStateCollision);
    }
    if let Some(assets) = &assets {
        let overlaps =
            |path: &Path| path.starts_with(&assets.root_path) || assets.root_path.starts_with(path);
        if reserved.iter().any(|path| overlaps(path))
            || providers.iter().any(|path| overlaps(path))
            || storage
                .as_ref()
                .is_some_and(|storage| overlaps(&storage.root_path))
            || managed_provider_paths
                .as_ref()
                .is_some_and(|(_, store)| overlaps(store))
        {
            return Err(ConfigError::AssetsStateCollision);
        }
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
    let mut duplicate_subjects = BTreeSet::new();
    for subject in config
        .principals
        .values()
        .flat_map(|principal| &principal.subjects)
    {
        if !mapped_subjects.insert(subject.canonical()) {
            duplicate_subjects.insert(subject.canonical());
        }
    }
    if !duplicate_subjects.is_empty() {
        return Err(ConfigError::DuplicateSubjects {
            subjects: duplicate_subjects.into_iter().collect(),
        });
    }
    if config.server_limits.max_connections == 0
        || config.server_limits.max_connections > HARD_MAX_CONNECTIONS
        || config.server_limits.shutdown_grace_ms == 0
    {
        return Err(ConfigError::InvalidServerLimits);
    }
    let frame_limits = config.server_limits.frame_limits()?;
    let plaintext_hosts = PlaintextHosts::new(&config.http.plaintext_hosts)
        .map_err(|source| ConfigError::InvalidPlaintextHost { source })?;
    if config.http.extra_ca_bundles.len() > 8 || config.http.non_public_https.len() > 8 {
        return Err(ConfigError::InvalidHttpsConfiguration);
    }
    let mut extra_ca_bundles = Vec::new();
    for path in &config.http.extra_ca_bundles {
        if !path.is_absolute() {
            return Err(ConfigError::InvalidHttpsConfiguration);
        }
        let metadata =
            std::fs::metadata(path).map_err(|_error| ConfigError::InvalidHttpsConfiguration)?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 65_536 {
            return Err(ConfigError::InvalidHttpsConfiguration);
        }
        let pem = std::fs::read(path).map_err(|_error| ConfigError::InvalidHttpsConfiguration)?;
        if reqwest::Certificate::from_pem_bundle(&pem).map_or(true, |certs| certs.is_empty()) {
            return Err(ConfigError::InvalidHttpsConfiguration);
        }
        extra_ca_bundles.push(pem);
    }
    let mut non_public_https = Vec::new();
    for authority in &config.http.non_public_https {
        let entry = NonPublicHttpsAuthority::new(authority)
            .map_err(|_error| ConfigError::InvalidHttpsConfiguration)?;
        if non_public_https
            .iter()
            .any(|existing: &NonPublicHttpsAuthority| existing.authority == entry.authority)
        {
            return Err(ConfigError::InvalidHttpsConfiguration);
        }
        non_public_https.push(entry);
    }
    if config.provider_settings.len() > HARD_MAX_PROVIDERS {
        return Err(ConfigError::InvalidProviderSettings);
    }
    let mut provider_settings = BTreeMap::new();
    for (id, settings) in &config.provider_settings {
        if !settings.is_object() {
            return Err(ConfigError::InvalidProviderSettings);
        }
        let json = serde_json::to_string(settings)
            .map_err(|_error| ConfigError::InvalidProviderSettings)?;
        if json.len() > 4096 {
            return Err(ConfigError::InvalidProviderSettings);
        }
        provider_settings.insert(id.clone(), json);
    }
    let host_limits = config.host_limits.runtime();
    if host_limits.max_timeout.is_zero() {
        return Err(ConfigError::InvalidHostLimits);
    }
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
            })
        })
        .transpose()?;

    Ok(ResolvedConfig {
        source,
        socket_path,
        broker_principal: config.broker_principal,
        policy_revision: config.policy_revision,
        credentials_path,
        secret_map_path,
        providers,
        locked_providers,
        strict: config.strict,
        identities: config.identities,
        principals: config.principals,
        agents: config.agents,
        policies_path,
        policies: String::new(),
        constraint_sets: config.constraint_sets,
        host_limits,
        host_options: BrokerHostOptions {
            cwasm_dir: managed_provider_paths
                .as_ref()
                .filter(|_| !config.compile_on_load)
                .map(|(_, store)| store.join("cwasm")),
            max_total_memory_bytes: config.host_limits.max_total_memory_bytes,
            plaintext_hosts: plaintext_hosts.clone(),
            extra_ca_bundles: Arc::new(extra_ca_bundles),
            non_public_https: Arc::new(non_public_https),
            provider_settings: Arc::new(provider_settings),
        },
        plaintext_hosts,
        worst_case_guest_memory_bytes,
        broker_limits: config.broker_limits,
        server_limits: config.server_limits,
        storage,
        assets,
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
    InsecureFile {
        path: PathBuf,
        #[source]
        source: FileHygieneError,
    },
    #[error("broker configuration is {length} bytes; maximum is {maximum}")]
    TooLarge { length: u64, maximum: usize },
    #[error("broker configuration is not strict valid YAML/JSON")]
    Decode {
        #[source]
        source: serde_yaml::Error,
    },
    #[error("configured path has no parent")]
    MissingParent,
    #[error("configured socket path has no file name")]
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
        #[source]
        source: provider_manager::ProviderManagerError,
    },
    #[error("managed provider store must be disjoint from broker-owned state paths")]
    ProviderStateCollision,
    #[error(
        "provider directory {path} is not owned by this user or is group/world writable; anyone \
         who can write it can add a provider this broker would execute"
    )]
    InsecureProviderDirectory { path: PathBuf },
    #[error("provider directory {path} contains no *.wasm component")]
    EmptyProviderDirectory { path: PathBuf },
    #[error("broker configuration has too many providers; maximum is {maximum}")]
    TooManyProviders { maximum: usize },
    #[error("broker configuration must map at least one peer identity")]
    NoIdentities,
    #[error("configuration, socket, lock, temporary, and provider paths must not conflict")]
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
    #[error(
        "a configuration directory holds its policies as *.cedar files; policiesPath is not allowed there"
    )]
    PoliciesPathInDirectory,
    #[error("configuration directory {path} holds no *.yaml fragment")]
    EmptyConfigDirectory { path: PathBuf },
    #[error(
        "configuration directory {path} must be owned by the broker and not group or world writable"
    )]
    InsecureConfigDirectory { path: PathBuf },
    #[error("configuration fragment {path} is not strict valid YAML")]
    DecodeFragment {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("configuration fragments conflict: {}", render_conflicts(.conflicts))]
    FragmentConflicts { conflicts: Vec<FragmentConflict> },
    #[error("configuration fragments disagree on apiVersion")]
    MixedApiVersions,
    #[error("broker policy file is not valid UTF-8: {path}")]
    PolicyNotUtf8 { path: PathBuf },
    #[error("peer attestor grant is invalid")]
    Attestor {
        #[source]
        source: dekopon_broker::BrokerBuildError,
    },
    #[error("subjects mapped to more than one principal: {subjects:?}")]
    DuplicateSubjects { subjects: Vec<String> },
    #[error("server limits must be positive and within hard ceilings")]
    InvalidServerLimits,
    #[error("invalid broker frame limits")]
    InvalidFrameLimits {
        #[source]
        source: ProtocolError,
    },
    #[error("host timeout must be positive")]
    InvalidHostLimits,
    #[error("http.plaintextHosts is invalid: {source}")]
    InvalidPlaintextHost {
        #[source]
        source: PlaintextHostError,
    },
    #[error(
        "http.extraCABundles requires absolute readable PEM files (64 KiB maximum); http.nonPublicHttps requires unique exact DNS authorities"
    )]
    InvalidHttpsConfiguration,
    #[error(
        "providerSettings must contain at most one bounded JSON object per provider (4 KiB each)"
    )]
    InvalidProviderSettings,
    #[error("could not safely resolve assets.rootPath")]
    AssetsPath {
        #[source]
        source: dekopon_storage_host::StorageHostError,
    },
    #[error("assets.rootPath must not overlap broker files, provider storage or provider paths")]
    AssetsStateCollision,
    #[error("could not safely resolve a configured provider storage path")]
    StoragePath {
        #[source]
        source: dekopon_storage_host::StorageHostError,
    },
    #[error("invalid provider storage limits")]
    InvalidStorage {
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
