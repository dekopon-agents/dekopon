//! Offline provider resolution, content storage, and lock verification.
//!
//! Network access exists only behind explicit provider-manager commands. Daemon startup consumes a
//! generated lock and local content-addressed blobs without constructing a registry client.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write as _},
    os::unix::fs::{
        DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub use dekopon_broker_host::HARD_MAX_PROVIDER_COMPONENT_BYTES;
use dekopon_broker_host::{
    BrokerHostError, BrokerHostLimits, BrokerHostOptions, BrokerProviderRegistry,
    LoadedProviderMetadata, LockedProviderSource,
};
use dekopon_core::ProviderId;
use fs2::FileExt as _;
use futures_util::StreamExt as _;
use http_auth::parser::ChallengeParser;
use reqwest::{
    StatusCode, Url,
    header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, WWW_AUTHENTICATE},
};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::{HARD_MAX_PROVIDERS, socket};

/// Expected OCI artifact type for a Dekopon provider.
pub const PROVIDER_ARTIFACT_TYPE: &str = "application/vnd.dekopon.provider.v1+wasm";
/// Expected media type for the single provider component layer.
pub const PROVIDER_LAYER_MEDIA_TYPE: &str = "application/wasm";
/// Maximum desired-set or lockfile bytes.
pub const HARD_MAX_PROVIDER_STATE_BYTES: usize = 1024 * 1024;
/// Maximum raw OCI manifest bytes.
pub const HARD_MAX_PROVIDER_MANIFEST_BYTES: usize = 1024 * 1024;
/// Maximum logical bytes retained across installed and orphan provider blobs (4 GiB).
pub const HARD_MAX_PROVIDER_STORE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Maximum files retained in the component-blob directory, including stale temporaries.
pub const HARD_MAX_PROVIDER_STORE_BLOBS: usize = 1024;

const HARD_MAX_TOKEN_BYTES: usize = 64 * 1024;
const HARD_MAX_REGISTRY_ERROR_BYTES: usize = 64 * 1024;
const HARD_MAX_CONFIG_DESCRIPTOR_BYTES: i64 = 4 * 1024;
const REGISTRY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REGISTRY_OPERATION_TIMEOUT: Duration = Duration::from_secs(120);
const OCI_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_EMPTY_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.empty.v1+json";
const OCI_EMPTY_CONFIG_DIGEST: &str =
    "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
const OCI_EMPTY_CONFIG_DATA: &str = "e30=";
const DOCKER_CONTENT_DIGEST: &str = "docker-content-digest";

/// Operator-authored exact provider references.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProviderSet {
    /// Versioned provider-set schema.
    pub api_version: ProviderSetApiVersion,
    /// Exact tagged or manifest-digest references.
    pub providers: Vec<DesiredProvider>,
}

/// Supported provider-set API versions.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProviderSetApiVersion {
    /// Initial exact-reference format.
    #[serde(rename = "dekopon.dev/provider-set/v1alpha1")]
    V1Alpha1,
}

/// One exact operator-authored OCI source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DesiredProvider {
    /// Fully qualified OCI reference carrying an explicit tag or SHA-256 manifest digest.
    pub source: String,
}

/// Generated immutable provider resolution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProviderLock {
    /// Versioned lock schema.
    pub api_version: ProviderLockApiVersion,
    /// Deterministically source-sorted resolutions.
    pub providers: Vec<LockedProvider>,
}

/// Supported provider-lock API versions.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProviderLockApiVersion {
    /// Initial exact-reference format.
    #[serde(rename = "dekopon.dev/provider-lock/v1alpha1")]
    V1Alpha1,
}

/// One immutable OCI manifest and component resolution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LockedProvider {
    /// Exact desired reference that produced this resolution.
    pub source: String,
    /// Semantic version when the exact tag itself parses as strict SemVer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_version: Option<Version>,
    /// Immutable SHA-256 digest of the OCI manifest.
    pub manifest_digest: String,
    /// Immutable SHA-256 digest of the single component layer.
    pub component_digest: String,
    /// Descriptor and verified component byte length.
    pub component_bytes: u64,
    /// Provider identity returned by bounded, import-disabled `describe`.
    pub provider_id: ProviderId,
}

impl ProviderLock {
    fn sources(
        &self,
        store: &ProviderStore,
    ) -> Result<Vec<LockedProviderSource>, ProviderManagerError> {
        self.providers
            .iter()
            .map(|provider| {
                let path = store.blob_path(&provider.component_digest)?;
                LockedProviderSource::new(
                    path,
                    provider.component_bytes,
                    digest_hex(&provider.component_digest)?.to_owned(),
                    provider.provider_id.clone(),
                )
                .map_err(ProviderManagerError::Host)
            })
            .collect()
    }
}

/// Files used by one provider-manager invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderManagerPaths {
    /// Operator-authored desired provider set.
    pub provider_set: Option<PathBuf>,
    /// Generated activation lock.
    pub lock_file: PathBuf,
    /// Content-addressed provider store.
    pub store: PathBuf,
}

/// Offline provider-manager operation settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderManagerOptions {
    /// Desired, locked, and installed state locations.
    pub paths: ProviderManagerPaths,
    /// Exact loopback registries for which plain HTTP is explicitly permitted.
    pub plaintext_loopback_registries: Vec<String>,
}

/// Result of resolving or materializing one desired set.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSyncReport {
    /// Number of activated providers.
    pub providers: usize,
    /// Number of blobs fetched in this operation.
    pub fetched: usize,
    /// Whether the generated lock bytes changed.
    pub lock_changed: bool,
    /// Reminder that daemon state is startup-fixed.
    pub restart_required: bool,
}

/// Offline status for one locked provider.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStatus {
    /// Desired exact source reference.
    pub source: String,
    /// Immutable OCI manifest digest.
    pub manifest_digest: String,
    /// Immutable component digest.
    pub component_digest: String,
    /// Locked provider identity.
    pub provider_id: ProviderId,
    /// Local content-addressed component path.
    pub path: PathBuf,
    /// Byte-verification state: `verified`, `missing`, or `invalid`.
    pub local_status: String,
    /// Bounded actionable category when local verification did not succeed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_reason: Option<String>,
}

/// Complete offline validation result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderVerifyReport {
    /// Number of components checked and described as one complete set.
    pub providers: usize,
}

/// Provider-manager entry point embedded in `dekopon-brokerd`.
pub struct ProviderManager {
    paths: ProviderManagerPaths,
    registry: RegistryClient,
}

impl ProviderManager {
    /// Creates a manager without touching the network or filesystem.
    ///
    /// # Errors
    ///
    /// Refuses a plaintext registry unless it is an exact literal loopback host (with an optional
    /// port). TLS certificate verification can never be disabled.
    pub fn new(options: ProviderManagerOptions) -> Result<Self, ProviderManagerError> {
        let registry = RegistryClient::new(options.plaintext_loopback_registries)?;
        Ok(Self {
            paths: options.paths,
            registry,
        })
    }

    /// Resolves changed exact references, materializes missing blobs, validates the complete set,
    /// and atomically activates the generated lock.
    ///
    /// An unchanged tag keeps its previous immutable manifest resolution. Changing the authored
    /// reference is the only way this exact-reference format asks for a new resolution.
    pub async fn sync(&self) -> Result<ProviderSyncReport, ProviderManagerError> {
        let uid = socket::current_uid();
        let _activation_operation = lock_activation(&self.paths.lock_file, uid)?;
        let desired = load_provider_set(
            self.paths
                .provider_set
                .as_deref()
                .ok_or(ProviderManagerError::MissingProviderSetPath)?,
            uid,
        )
        .await?;
        let store = ProviderStore::open(&self.paths.store, uid, true)?;
        let _store_operation = store.lock()?;
        let old_lock = load_optional_lock(&self.paths.lock_file, uid).await?;
        let old_by_source = old_lock
            .as_ref()
            .map(|lock| {
                lock.providers
                    .iter()
                    .map(|provider| (provider.source.clone(), provider.clone()))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();

        let mut candidates = Vec::with_capacity(desired.providers.len());
        let mut fetched = 0_usize;
        for provider in desired.providers {
            if let Some(locked) = old_by_source.get(&provider.source) {
                if store.ensure_locked_blob(&self.registry, locked).await? {
                    fetched += 1;
                }
                candidates.push(CandidateProvider::from_locked(locked.clone()));
            } else {
                let resolved = self.registry.resolve(&provider.source).await?;
                if store.install_resolved(&self.registry, &resolved).await? {
                    fetched += 1;
                }
                candidates.push(CandidateProvider::from_resolved(resolved));
            }
        }

        let lock = validate_candidates(candidates, &store).await?;
        let encoded = encode_lock(&lock)?;
        let previous = old_lock.as_ref().map(encode_lock).transpose()?;
        let lock_changed = previous.as_deref() != Some(encoded.as_slice());
        if lock_changed {
            atomic_write(&self.paths.lock_file, &encoded, uid)?;
        }
        Ok(ProviderSyncReport {
            providers: lock.providers.len(),
            fetched,
            lock_changed,
            restart_required: lock_changed,
        })
    }

    /// Materializes an existing lock without resolving any desired tag.
    pub async fn sync_locked(&self) -> Result<ProviderSyncReport, ProviderManagerError> {
        let uid = socket::current_uid();
        let _activation_operation = lock_activation(&self.paths.lock_file, uid)?;
        let desired = load_provider_set(
            self.paths
                .provider_set
                .as_deref()
                .ok_or(ProviderManagerError::MissingProviderSetPath)?,
            uid,
        )
        .await?;
        let lock = load_lock(&self.paths.lock_file, uid).await?;
        require_desired_lock_match(&desired, &lock)?;
        let store = ProviderStore::open(&self.paths.store, uid, true)?;
        let _store_operation = store.lock()?;
        let mut fetched = 0_usize;
        for provider in &lock.providers {
            if store.ensure_locked_blob(&self.registry, provider).await? {
                fetched += 1;
            }
        }
        validate_lock(&lock, &store).await?;
        Ok(ProviderSyncReport {
            providers: lock.providers.len(),
            fetched,
            lock_changed: false,
            restart_required: false,
        })
    }

    /// Reports lock and local-byte state without constructing a network client request.
    pub async fn list(&self) -> Result<Vec<ProviderStatus>, ProviderManagerError> {
        let uid = socket::current_uid();
        let lock = load_lock(&self.paths.lock_file, uid).await?;
        let store = ProviderStore::open(&self.paths.store, uid, false)?;
        let mut statuses = Vec::with_capacity(lock.providers.len());
        for provider in lock.providers {
            let path = store.blob_path(&provider.component_digest)?;
            // `list` remains a status command rather than failing the whole set, but it must not
            // collapse distinct remedies into one boolean. Categories are bounded and contain no
            // filesystem or registry text; `verify` retains the full error chain when requested.
            let (local_status, local_reason) = match verify_blob(&path, &provider, uid).await {
                Ok(()) => ("verified", None),
                Err(ProviderManagerError::ReadFile { source, .. })
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    ("missing", Some("not-installed"))
                }
                Err(
                    ProviderManagerError::FileSecurity(_)
                    | ProviderManagerError::InsecureFile { .. },
                ) => ("invalid", Some("insecure-metadata")),
                Err(ProviderManagerError::BlobSizeMismatch { .. }) => {
                    ("invalid", Some("size-mismatch"))
                }
                Err(ProviderManagerError::BlobDigestMismatch { .. }) => {
                    ("invalid", Some("digest-mismatch"))
                }
                Err(ProviderManagerError::ReadFile { .. }) => ("invalid", Some("unreadable")),
                Err(_) => ("invalid", Some("verification-failed")),
            };
            statuses.push(ProviderStatus {
                source: provider.source,
                manifest_digest: provider.manifest_digest,
                component_digest: provider.component_digest,
                provider_id: provider.provider_id,
                path,
                local_status: local_status.to_owned(),
                local_reason: local_reason.map(str::to_owned),
            });
        }
        Ok(statuses)
    }

    /// Verifies local bytes and validates the complete locked provider set without network access.
    pub async fn verify(&self) -> Result<ProviderVerifyReport, ProviderManagerError> {
        let uid = socket::current_uid();
        let lock = load_lock(&self.paths.lock_file, uid).await?;
        let store = ProviderStore::open(&self.paths.store, uid, false)?;
        validate_lock(&lock, &store).await?;
        Ok(ProviderVerifyReport {
            providers: lock.providers.len(),
        })
    }
}

/// Loads a generated lock into exact broker-host sources without performing network access.
///
/// The daemon still validates each blob path's ownership and link count separately. The host then
/// compares digest, length, and provider identity against the exact buffer it passes to Wasmtime.
pub(crate) async fn load_locked_sources(
    lock_path: &Path,
    store_path: &Path,
    expected_uid: u32,
) -> Result<Vec<LockedProviderSource>, ProviderManagerError> {
    let lock = load_lock(lock_path, expected_uid).await?;
    let store = ProviderStore::open(store_path, expected_uid, false)?;
    let sources = lock.sources(&store)?;
    for source in &sources {
        socket::validate_owned_file(source.path(), expected_uid)
            .map_err(ProviderManagerError::FileSecurity)?;
    }
    Ok(sources)
}

#[derive(Clone)]
struct CandidateProvider {
    source: String,
    resolved_version: Option<Version>,
    manifest_digest: String,
    component_digest: String,
    component_bytes: u64,
    expected_provider_id: Option<ProviderId>,
}

impl CandidateProvider {
    fn from_locked(locked: LockedProvider) -> Self {
        Self {
            source: locked.source,
            resolved_version: locked.resolved_version,
            manifest_digest: locked.manifest_digest,
            component_digest: locked.component_digest,
            component_bytes: locked.component_bytes,
            expected_provider_id: Some(locked.provider_id),
        }
    }

    fn from_resolved(resolved: ResolvedProvider) -> Self {
        Self {
            source: resolved.source,
            resolved_version: resolved.resolved_version,
            manifest_digest: resolved.manifest_digest,
            component_digest: resolved.layer.digest,
            component_bytes: resolved.layer.size,
            expected_provider_id: None,
        }
    }
}

async fn validate_candidates(
    candidates: Vec<CandidateProvider>,
    store: &ProviderStore,
) -> Result<ProviderLock, ProviderManagerError> {
    let paths = candidates
        .iter()
        .map(|candidate| store.blob_path(&candidate.component_digest))
        .collect::<Result<Vec<_>, _>>()?;
    let registry = BrokerProviderRegistry::load(paths, BrokerHostLimits::default())
        .await
        .map_err(ProviderManagerError::Host)?;
    let metadata = registry
        .loaded_provider_metadata()
        .map(|metadata| (metadata.source.clone(), metadata))
        .collect::<BTreeMap<_, _>>();

    let mut providers = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let path = store.blob_path(&candidate.component_digest)?;
        let loaded = metadata
            .get(&path)
            .ok_or_else(|| ProviderManagerError::MissingValidatedProvider { path: path.clone() })?;
        if loaded.artifact_bytes != candidate.component_bytes {
            return Err(ProviderManagerError::BlobSizeMismatch {
                expected: candidate.component_bytes,
                actual: loaded.artifact_bytes,
            });
        }
        let expected_digest = digest_hex(&candidate.component_digest)?;
        if loaded.artifact_sha256 != expected_digest {
            return Err(ProviderManagerError::BlobDigestMismatch {
                expected: candidate.component_digest,
                actual: format!("sha256:{}", loaded.artifact_sha256),
            });
        }
        if let Some(expected) = candidate.expected_provider_id
            && loaded.manifest.id != expected
        {
            return Err(ProviderManagerError::LockedProviderIdentity {
                reference: candidate.source,
                expected,
                actual: loaded.manifest.id.clone(),
            });
        }
        providers.push(LockedProvider {
            source: candidate.source,
            resolved_version: candidate.resolved_version,
            manifest_digest: candidate.manifest_digest,
            component_digest: candidate.component_digest,
            component_bytes: candidate.component_bytes,
            provider_id: loaded.manifest.id.clone(),
        });
    }
    providers.sort_by(|left, right| left.source.cmp(&right.source));
    let lock = ProviderLock {
        api_version: ProviderLockApiVersion::V1Alpha1,
        providers,
    };
    validate_lock_shape(&lock)?;
    Ok(lock)
}

async fn validate_lock(
    lock: &ProviderLock,
    store: &ProviderStore,
) -> Result<Vec<LoadedProviderMetadata>, ProviderManagerError> {
    let uid = socket::current_uid();
    for provider in &lock.providers {
        let path = store.blob_path(&provider.component_digest)?;
        verify_blob(&path, provider, uid).await?;
    }
    let registry = BrokerProviderRegistry::load_locked_with_options(
        lock.sources(store)?,
        BrokerHostLimits::default(),
        None,
        &BrokerHostOptions::default(),
    )
    .await
    .map_err(ProviderManagerError::Host)?;
    Ok(registry.loaded_provider_metadata().collect())
}

fn require_desired_lock_match(
    desired: &ProviderSet,
    lock: &ProviderLock,
) -> Result<(), ProviderManagerError> {
    let desired = desired
        .providers
        .iter()
        .map(|provider| provider.source.as_str())
        .collect::<BTreeSet<_>>();
    let locked = lock
        .providers
        .iter()
        .map(|provider| provider.source.as_str())
        .collect::<BTreeSet<_>>();
    if desired != locked {
        return Err(ProviderManagerError::LockedStateChanged);
    }
    Ok(())
}

async fn load_provider_set(
    path: &Path,
    expected_uid: u32,
) -> Result<ProviderSet, ProviderManagerError> {
    let bytes = read_secure_file(path, expected_uid, HARD_MAX_PROVIDER_STATE_BYTES).await?;
    let mut set = serde_yaml::from_slice::<ProviderSet>(&bytes)
        .map_err(|source| ProviderManagerError::DecodeProviderSet { source })?;
    if set.providers.is_empty() {
        return Err(ProviderManagerError::NoProviders);
    }
    if set.providers.len() > HARD_MAX_PROVIDERS {
        return Err(ProviderManagerError::TooManyProviders {
            maximum: HARD_MAX_PROVIDERS,
        });
    }
    let mut repositories = BTreeSet::new();
    let mut sources = BTreeSet::new();
    let mut conflicts = BTreeSet::new();
    for provider in &mut set.providers {
        let parsed = ParsedSource::parse(&provider.source)?;
        let repository = parsed.repository_key();
        if !repositories.insert(repository.clone()) {
            conflicts.insert(format!("duplicate repository {repository}"));
        }
        provider.source = parsed.canonical;
        if !sources.insert(provider.source.clone()) {
            conflicts.insert(format!("duplicate source {}", provider.source));
        }
    }
    if !conflicts.is_empty() {
        return Err(ProviderManagerError::ProviderStateConflicts {
            problems: conflicts.into_iter().collect(),
        });
    }
    set.providers
        .sort_by(|left, right| left.source.cmp(&right.source));
    Ok(set)
}

async fn load_optional_lock(
    path: &Path,
    expected_uid: u32,
) -> Result<Option<ProviderLock>, ProviderManagerError> {
    match read_secure_file(path, expected_uid, HARD_MAX_PROVIDER_STATE_BYTES).await {
        Ok(bytes) => decode_lock(&bytes).map(Some),
        Err(ProviderManagerError::ReadFile { source, .. })
            if source.kind() == io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

async fn load_lock(path: &Path, expected_uid: u32) -> Result<ProviderLock, ProviderManagerError> {
    let bytes = read_secure_file(path, expected_uid, HARD_MAX_PROVIDER_STATE_BYTES).await?;
    decode_lock(&bytes)
}

fn decode_lock(bytes: &[u8]) -> Result<ProviderLock, ProviderManagerError> {
    let mut lock = serde_yaml::from_slice::<ProviderLock>(bytes)
        .map_err(|source| ProviderManagerError::DecodeLock { source })?;
    lock.providers
        .sort_by(|left, right| left.source.cmp(&right.source));
    validate_lock_shape(&lock)?;
    Ok(lock)
}

fn validate_lock_shape(lock: &ProviderLock) -> Result<(), ProviderManagerError> {
    if lock.providers.is_empty() {
        return Err(ProviderManagerError::NoProviders);
    }
    if lock.providers.len() > HARD_MAX_PROVIDERS {
        return Err(ProviderManagerError::TooManyProviders {
            maximum: HARD_MAX_PROVIDERS,
        });
    }
    let mut sources = BTreeSet::new();
    let mut repositories = BTreeSet::new();
    let mut provider_ids = BTreeSet::new();
    let mut conflicts = BTreeSet::new();
    for provider in &lock.providers {
        let parsed = ParsedSource::parse(&provider.source)?;
        if parsed.canonical != provider.source {
            return Err(ProviderManagerError::NonCanonicalSource {
                reference: provider.source.clone(),
                canonical: parsed.canonical,
            });
        }
        if !sources.insert(provider.source.clone()) {
            conflicts.insert(format!("duplicate source {}", provider.source));
        }
        let repository = parsed.repository_key();
        if !repositories.insert(repository.clone()) {
            conflicts.insert(format!("duplicate repository {repository}"));
        }
        if !provider_ids.insert(provider.provider_id.clone()) {
            conflicts.insert(format!("duplicate provider ID {}", provider.provider_id));
        }
        validate_digest(&provider.manifest_digest)?;
        validate_digest(&provider.component_digest)?;
        if let Some(expected) = parsed.reference.digest()
            && provider.manifest_digest != expected
        {
            return Err(ProviderManagerError::ManifestDigestMismatch {
                expected: expected.to_owned(),
                actual: provider.manifest_digest.clone(),
            });
        }
        let resolved_version = parsed
            .reference
            .tag()
            .and_then(|tag| tag.parse::<Version>().ok());
        if provider.resolved_version != resolved_version {
            return Err(ProviderManagerError::ResolvedVersionMismatch {
                reference: provider.source.clone(),
                expected: resolved_version,
                actual: provider.resolved_version.clone(),
            });
        }
        if provider.component_bytes == 0
            || provider.component_bytes > HARD_MAX_PROVIDER_COMPONENT_BYTES
        {
            return Err(ProviderManagerError::ComponentSize {
                size: provider.component_bytes,
                maximum: HARD_MAX_PROVIDER_COMPONENT_BYTES,
            });
        }
    }
    if !conflicts.is_empty() {
        return Err(ProviderManagerError::ProviderStateConflicts {
            problems: conflicts.into_iter().collect(),
        });
    }
    Ok(())
}

fn encode_lock(lock: &ProviderLock) -> Result<Vec<u8>, ProviderManagerError> {
    let mut lock = lock.clone();
    lock.providers
        .sort_by(|left, right| left.source.cmp(&right.source));
    validate_lock_shape(&lock)?;
    let mut bytes = serde_yaml::to_string(&lock)
        .map_err(|source| ProviderManagerError::EncodeLock { source })?
        .into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    if bytes.len() > HARD_MAX_PROVIDER_STATE_BYTES {
        return Err(ProviderManagerError::StateTooLarge {
            length: bytes.len(),
            maximum: HARD_MAX_PROVIDER_STATE_BYTES,
        });
    }
    Ok(bytes)
}

#[derive(Clone)]
struct OciReference {
    registry: String,
    repository: String,
    tag: Option<String>,
    digest: Option<String>,
}

impl OciReference {
    fn parse(source: &str) -> Result<Self, ProviderManagerError> {
        if source.is_empty() || source.len() > 512 || source.contains("://") {
            return Err(ProviderManagerError::InvalidSource {
                reference: source.to_owned(),
                reason: "expected a bounded OCI reference rather than a URL".to_owned(),
            });
        }
        let (name, tag, digest) = if let Some((name, digest)) = source.rsplit_once('@') {
            if name.contains('@') || digest.is_empty() {
                return Err(ProviderManagerError::InvalidSource {
                    reference: source.to_owned(),
                    reason: "invalid manifest-digest selector".to_owned(),
                });
            }
            validate_digest(digest)?;
            (name, None, Some(digest.to_owned()))
        } else {
            let slash =
                source
                    .rfind('/')
                    .ok_or_else(|| ProviderManagerError::UnqualifiedSource {
                        reference: source.to_owned(),
                    })?;
            let colon = source[slash + 1..]
                .rfind(':')
                .map(|relative| slash + 1 + relative)
                .ok_or_else(|| ProviderManagerError::MissingSelector {
                    reference: source.to_owned(),
                })?;
            let tag = &source[colon + 1..];
            validate_tag(tag, source)?;
            (&source[..colon], Some(tag.to_owned()), None)
        };
        let (registry, repository) =
            name.split_once('/')
                .ok_or_else(|| ProviderManagerError::UnqualifiedSource {
                    reference: source.to_owned(),
                })?;
        validate_registry(registry, source)?;
        validate_repository(repository, source)?;
        Ok(Self {
            registry: registry.to_owned(),
            repository: repository.to_owned(),
            tag,
            digest,
        })
    }

    fn registry(&self) -> &str {
        &self.registry
    }

    fn resolve_registry(&self) -> &str {
        &self.registry
    }

    fn repository(&self) -> &str {
        &self.repository
    }

    fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    fn digest(&self) -> Option<&str> {
        self.digest.as_deref()
    }

    fn canonical(&self) -> String {
        let mut canonical = format!("{}/{}", self.registry, self.repository);
        if let Some(tag) = &self.tag {
            canonical.push(':');
            canonical.push_str(tag);
        }
        if let Some(digest) = &self.digest {
            canonical.push('@');
            canonical.push_str(digest);
        }
        canonical
    }
}

fn validate_registry(registry: &str, source: &str) -> Result<(), ProviderManagerError> {
    if registry.is_empty()
        || registry.bytes().any(|byte| {
            !(byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'-' | b':'))
        })
        || registry.matches(':').count() > 1
    {
        return Err(ProviderManagerError::InvalidSource {
            reference: source.to_owned(),
            reason: "registry authority is not canonical lowercase host[:port]".to_owned(),
        });
    }
    let (host, port) = registry
        .rsplit_once(':')
        .map_or((registry, None), |(host, port)| (host, Some(port)));
    if host.is_empty()
        || host.starts_with('.')
        || host.starts_with('-')
        || host.ends_with('.')
        || host.ends_with('-')
        || host
            .split('.')
            .any(|segment| segment.is_empty() || segment.starts_with('-') || segment.ends_with('-'))
        || port.is_some_and(|port| port.parse::<u16>().is_err())
        || !(host == "localhost" || host.contains('.') || port.is_some())
    {
        return Err(ProviderManagerError::UnqualifiedSource {
            reference: source.to_owned(),
        });
    }
    Ok(())
}

fn validate_repository(repository: &str, source: &str) -> Result<(), ProviderManagerError> {
    if repository.is_empty()
        || repository.len() > 255
        || repository.split('/').any(|segment| {
            segment.is_empty()
                || !segment.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'_' | b'-')
                })
                || !segment
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                || !segment
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
    {
        return Err(ProviderManagerError::InvalidSource {
            reference: source.to_owned(),
            reason: "repository is not a canonical lowercase OCI name".to_owned(),
        });
    }
    Ok(())
}

fn validate_tag(tag: &str, source: &str) -> Result<(), ProviderManagerError> {
    if tag.is_empty()
        || tag.len() > 128
        || !tag
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        return Err(ProviderManagerError::InvalidSource {
            reference: source.to_owned(),
            reason: "tag is not a canonical OCI tag".to_owned(),
        });
    }
    Ok(())
}

struct ParsedSource {
    reference: OciReference,
    canonical: String,
}

impl ParsedSource {
    fn parse(source: &str) -> Result<Self, ProviderManagerError> {
        let reference = OciReference::parse(source)?;
        let canonical = reference.canonical();
        Ok(Self {
            reference,
            canonical,
        })
    }

    fn repository_key(&self) -> String {
        format!(
            "{}/{}",
            self.reference.registry(),
            self.reference.repository()
        )
    }
}

#[derive(Clone)]
struct ResolvedProvider {
    source: String,
    resolved_version: Option<Version>,
    manifest_digest: String,
    layer: LayerDescriptor,
}

#[derive(Clone)]
struct LayerDescriptor {
    digest: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct OciProviderManifest {
    schema_version: u8,
    media_type: String,
    artifact_type: String,
    config: OciDescriptor,
    layers: Vec<OciDescriptor>,
    #[serde(default)]
    subject: Option<OciDescriptor>,
    #[serde(default, rename = "annotations")]
    _annotations: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct OciDescriptor {
    media_type: String,
    digest: String,
    size: i64,
    #[serde(default)]
    urls: Option<Vec<String>>,
    #[serde(default, rename = "annotations")]
    _annotations: Option<BTreeMap<String, String>>,
    #[serde(default)]
    artifact_type: Option<String>,
    #[serde(default)]
    data: Option<String>,
}

struct RegistryClient {
    client: reqwest::Client,
    plaintext: BTreeSet<String>,
    tokens: Arc<tokio::sync::Mutex<BTreeMap<String, RegistryToken>>>,
}

#[derive(Clone)]
struct RegistryToken(String);

impl RegistryClient {
    fn new(plaintext: Vec<String>) -> Result<Self, ProviderManagerError> {
        let mut allowed = BTreeSet::new();
        for registry in plaintext {
            if !is_literal_loopback_registry(&registry) {
                return Err(ProviderManagerError::PlaintextRegistryNotLoopback { registry });
            }
            allowed.insert(registry);
        }
        let client = reqwest::Client::builder()
            .connect_timeout(REGISTRY_CONNECT_TIMEOUT)
            .no_proxy()
            .http1_only()
            .redirect(registry_redirect_policy(allowed.clone()))
            .user_agent(concat!("dekopon-brokerd/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|source| ProviderManagerError::RegistryClient { source })?;
        Ok(Self {
            client,
            plaintext: allowed,
            tokens: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
        })
    }

    async fn resolve(&self, source: &str) -> Result<ResolvedProvider, ProviderManagerError> {
        let parsed = ParsedSource::parse(source)?;
        let selector = parsed
            .reference
            .digest()
            .or_else(|| parsed.reference.tag())
            .ok_or_else(|| ProviderManagerError::MissingSelector {
                reference: source.to_owned(),
            })?;
        let url = self.registry_url(&parsed.reference, &format!("manifests/{selector}"))?;
        let response = self
            .get_authorized(
                &parsed.reference,
                url,
                Some(OCI_MANIFEST_MEDIA_TYPE),
                "manifest",
            )
            .await?;
        let headers = response.headers().clone();
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        if content_type != Some(OCI_MANIFEST_MEDIA_TYPE) {
            return Err(ProviderManagerError::InvalidManifestContentType {
                actual: content_type.unwrap_or("missing").to_owned(),
            });
        }
        let bytes =
            bounded_response_bytes(response, HARD_MAX_PROVIDER_MANIFEST_BYTES, "manifest").await?;
        let manifest_digest = prefixed_sha256(&bytes);
        if let Some(expected) = parsed.reference.digest()
            && manifest_digest != expected
        {
            return Err(ProviderManagerError::ManifestDigestMismatch {
                expected: expected.to_owned(),
                actual: manifest_digest,
            });
        }
        if let Some(header) = headers.get(DOCKER_CONTENT_DIGEST) {
            let header = header
                .to_str()
                .map_err(|source| ProviderManagerError::InvalidDigestHeader { source })?;
            validate_digest(header)?;
            if header != manifest_digest {
                return Err(ProviderManagerError::ManifestDigestMismatch {
                    expected: header.to_owned(),
                    actual: manifest_digest,
                });
            }
        }
        let manifest = serde_json::from_slice::<OciProviderManifest>(&bytes)
            .map_err(|source| ProviderManagerError::DecodeManifest { source })?;
        let layer = validate_manifest(&manifest)?;
        let resolved_version = parsed
            .reference
            .tag()
            .and_then(|tag| tag.parse::<Version>().ok());
        Ok(ResolvedProvider {
            source: parsed.canonical,
            resolved_version,
            manifest_digest,
            layer,
        })
    }

    async fn download_blob(
        &self,
        source: &str,
        descriptor: &LayerDescriptor,
        output: &mut tokio::fs::File,
    ) -> Result<(), ProviderManagerError> {
        if descriptor.size == 0 || descriptor.size > HARD_MAX_PROVIDER_COMPONENT_BYTES {
            return Err(ProviderManagerError::ComponentSize {
                size: descriptor.size,
                maximum: HARD_MAX_PROVIDER_COMPONENT_BYTES,
            });
        }
        let parsed = ParsedSource::parse(source)?;
        let url = self.registry_url(&parsed.reference, &format!("blobs/{}", descriptor.digest))?;
        let response = self
            .get_authorized(&parsed.reference, url, None, "blob")
            .await?;
        if let Some(length) = response.content_length()
            && length != descriptor.size
        {
            return Err(ProviderManagerError::BlobSizeMismatch {
                expected: descriptor.size,
                actual: length,
            });
        }
        let mut stream = response.bytes_stream();
        let mut digest = Sha256::new();
        let mut length = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|source| ProviderManagerError::RegistryRead {
                operation: "blob",
                source: source.without_url(),
            })?;
            length = length.checked_add(chunk.len() as u64).ok_or(
                ProviderManagerError::ComponentSize {
                    size: u64::MAX,
                    maximum: HARD_MAX_PROVIDER_COMPONENT_BYTES,
                },
            )?;
            if length > descriptor.size || length > HARD_MAX_PROVIDER_COMPONENT_BYTES {
                return Err(ProviderManagerError::BlobSizeMismatch {
                    expected: descriptor.size,
                    actual: length,
                });
            }
            digest.update(&chunk);
            output
                .write_all(&chunk)
                .await
                .map_err(|source| ProviderManagerError::WriteBlob { source })?;
        }
        if length != descriptor.size {
            return Err(ProviderManagerError::BlobSizeMismatch {
                expected: descriptor.size,
                actual: length,
            });
        }
        let actual = format!("sha256:{}", hex_digest(digest.finalize().as_slice()));
        if actual != descriptor.digest {
            return Err(ProviderManagerError::BlobDigestMismatch {
                expected: descriptor.digest.clone(),
                actual,
            });
        }
        Ok(())
    }

    fn registry_url(
        &self,
        reference: &OciReference,
        suffix: &str,
    ) -> Result<Url, ProviderManagerError> {
        let registry = reference.resolve_registry();
        let scheme = if self.plaintext.contains(registry) {
            "http"
        } else {
            "https"
        };
        Url::parse(&format!(
            "{scheme}://{registry}/v2/{}/{suffix}",
            reference.repository()
        ))
        .map_err(|source| ProviderManagerError::RegistryUrl { source })
    }

    async fn get_authorized(
        &self,
        reference: &OciReference,
        url: Url,
        accept: Option<&str>,
        operation: &'static str,
    ) -> Result<reqwest::Response, ProviderManagerError> {
        let key = format!(
            "{}/{}",
            reference.resolve_registry(),
            reference.repository()
        );
        let cached = { self.tokens.lock().await.get(&key).cloned() };
        let mut response = self
            .send_get(url.clone(), accept, cached.as_ref(), operation)
            .await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            let challenge = response
                .headers()
                .get(WWW_AUTHENTICATE)
                .ok_or(ProviderManagerError::MissingRegistryChallenge)?
                .to_str()
                .map_err(|source| ProviderManagerError::InvalidRegistryChallengeHeader { source })?
                .to_owned();
            // Bound and discard the unauthenticated response before retrying. Registry text is
            // untrusted and never enters the surfaced error or ordinary logs.
            bounded_response_bytes(response, HARD_MAX_REGISTRY_ERROR_BYTES, operation).await?;
            let token = self.fetch_token(reference, &challenge).await?;
            self.tokens.lock().await.insert(key, token.clone());
            response = self.send_get(url, accept, Some(&token), operation).await?;
        }
        if !response.status().is_success() {
            let status = response.status();
            bounded_response_bytes(response, HARD_MAX_REGISTRY_ERROR_BYTES, operation).await?;
            return Err(ProviderManagerError::RegistryStatus { operation, status });
        }
        Ok(response)
    }

    async fn send_get(
        &self,
        url: Url,
        accept: Option<&str>,
        token: Option<&RegistryToken>,
        operation: &'static str,
    ) -> Result<reqwest::Response, ProviderManagerError> {
        let mut request = self.client.get(url).timeout(REGISTRY_OPERATION_TIMEOUT);
        if let Some(accept) = accept {
            request = request.header(ACCEPT, accept);
        }
        if let Some(token) = token {
            let value = HeaderValue::from_str(&format!("Bearer {}", token.0))
                .map_err(|source| ProviderManagerError::InvalidAuthorizationHeader { source })?;
            request = request.header(AUTHORIZATION, value);
        }
        request
            .send()
            .await
            .map_err(|source| ProviderManagerError::RegistryRequest {
                operation,
                source: source.without_url(),
            })
    }

    async fn fetch_token(
        &self,
        reference: &OciReference,
        challenge: &str,
    ) -> Result<RegistryToken, ProviderManagerError> {
        let challenge = parse_bearer_challenge(challenge)?;
        let mut realm = Url::parse(&challenge.realm)
            .map_err(|source| ProviderManagerError::RegistryTokenUrl { source })?;
        if realm.scheme() != "https" {
            let authority = url_authority(&realm)?;
            if realm.scheme() != "http" || !self.plaintext.contains(&authority) {
                return Err(ProviderManagerError::InsecureTokenRealm { realm: authority });
            }
        }
        {
            let mut query = realm.query_pairs_mut();
            if let Some(service) = &challenge.service {
                query.append_pair("service", service);
            }
            query.append_pair(
                "scope",
                challenge
                    .scope
                    .as_deref()
                    .unwrap_or(&format!("repository:{}:pull", reference.repository())),
            );
        }
        let response = self
            .client
            .get(realm)
            .timeout(REGISTRY_OPERATION_TIMEOUT)
            .send()
            .await
            .map_err(|source| ProviderManagerError::RegistryRequest {
                operation: "token",
                source: source.without_url(),
            })?;
        if !response.status().is_success() {
            let status = response.status();
            bounded_response_bytes(response, HARD_MAX_REGISTRY_ERROR_BYTES, "token").await?;
            return Err(ProviderManagerError::RegistryStatus {
                operation: "token",
                status,
            });
        }
        let bytes = bounded_response_bytes(response, HARD_MAX_TOKEN_BYTES, "token").await?;
        let document = serde_json::from_slice::<TokenDocument>(&bytes)
            .map_err(|source| ProviderManagerError::DecodeRegistryToken { source })?;
        let token = match (document.token, document.access_token) {
            (Some(token), None) | (None, Some(token)) => token,
            (Some(token), Some(access)) if token == access => token,
            (Some(_), Some(_)) => return Err(ProviderManagerError::AmbiguousRegistryToken),
            (None, None) => return Err(ProviderManagerError::MissingRegistryToken),
        };
        if token.is_empty() || token.len() > HARD_MAX_TOKEN_BYTES {
            return Err(ProviderManagerError::InvalidRegistryToken);
        }
        Ok(RegistryToken(token))
    }
}

#[derive(Deserialize)]
struct TokenDocument {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

#[derive(Debug)]
struct BearerChallenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

fn parse_bearer_challenge(value: &str) -> Result<BearerChallenge, ProviderManagerError> {
    for parsed in ChallengeParser::new(value) {
        let Ok(parsed) = parsed else {
            return Err(ProviderManagerError::InvalidRegistryChallenge);
        };
        if !parsed.scheme.eq_ignore_ascii_case("Bearer") {
            continue;
        }
        let mut realm = None;
        let mut service = None;
        let mut scope = None;
        for (key, value) in parsed.params {
            match key.to_ascii_lowercase().as_str() {
                "realm" => realm = Some(value.to_unescaped()),
                "service" => service = Some(value.to_unescaped()),
                "scope" => scope = Some(value.to_unescaped()),
                _ => {}
            }
        }
        return Ok(BearerChallenge {
            realm: realm.ok_or(ProviderManagerError::MissingTokenRealm)?,
            service,
            scope,
        });
    }
    Err(ProviderManagerError::MissingRegistryChallenge)
}

fn validate_manifest(
    manifest: &OciProviderManifest,
) -> Result<LayerDescriptor, ProviderManagerError> {
    if manifest.schema_version != 2 || manifest.media_type != OCI_MANIFEST_MEDIA_TYPE {
        return Err(ProviderManagerError::InvalidManifestType {
            schema: manifest.schema_version,
            media_type: manifest.media_type.clone(),
        });
    }
    if manifest.artifact_type != PROVIDER_ARTIFACT_TYPE {
        return Err(ProviderManagerError::InvalidArtifactType {
            actual: manifest.artifact_type.clone(),
        });
    }
    if manifest.subject.is_some() {
        return Err(ProviderManagerError::ManifestHasSubject);
    }
    validate_digest(&manifest.config.digest)?;
    if manifest.config.media_type != OCI_EMPTY_CONFIG_MEDIA_TYPE
        || manifest.config.digest != OCI_EMPTY_CONFIG_DIGEST
        || manifest.config.size != 2
        || manifest.config.size > HARD_MAX_CONFIG_DESCRIPTOR_BYTES
        || manifest
            .config
            .data
            .as_deref()
            .is_some_and(|data| data != OCI_EMPTY_CONFIG_DATA)
        || manifest
            .config
            .urls
            .as_ref()
            .is_some_and(|urls| !urls.is_empty())
        || manifest.config.artifact_type.is_some()
    {
        return Err(ProviderManagerError::InvalidConfigDescriptor);
    }
    if manifest.layers.len() != 1 {
        return Err(ProviderManagerError::LayerCount {
            actual: manifest.layers.len(),
        });
    }
    let layer = &manifest.layers[0];
    if layer.media_type != PROVIDER_LAYER_MEDIA_TYPE {
        return Err(ProviderManagerError::InvalidLayerMediaType {
            actual: layer.media_type.clone(),
        });
    }
    if layer.urls.as_ref().is_some_and(|urls| !urls.is_empty())
        || layer.artifact_type.is_some()
        || layer.data.is_some()
    {
        return Err(ProviderManagerError::InvalidLayerDescriptor);
    }
    validate_digest(&layer.digest)?;
    let Ok(size) = u64::try_from(layer.size) else {
        return Err(ProviderManagerError::ComponentSize {
            size: 0,
            maximum: HARD_MAX_PROVIDER_COMPONENT_BYTES,
        });
    };
    if size == 0 || size > HARD_MAX_PROVIDER_COMPONENT_BYTES {
        return Err(ProviderManagerError::ComponentSize {
            size,
            maximum: HARD_MAX_PROVIDER_COMPONENT_BYTES,
        });
    }
    // OCI annotations are accepted by the strict decoder but remain bounded incidental metadata;
    // they are never trusted for filenames, identity, authorization, or I/O.
    Ok(LayerDescriptor {
        digest: layer.digest.clone(),
        size,
    })
}

struct ProviderStore {
    root: PathBuf,
    sha256: PathBuf,
    uid: u32,
}

impl ProviderStore {
    fn open(path: &Path, expected_uid: u32, create: bool) -> Result<Self, ProviderManagerError> {
        let root = absolute(path)?;
        if create {
            ensure_private_directory(&root, expected_uid)?;
            ensure_private_directory(&root.join("blobs"), expected_uid)?;
            ensure_private_directory(&root.join("blobs/sha256"), expected_uid)?;
        }
        let root = fs::canonicalize(&root)
            .map_err(|source| ProviderManagerError::StorePath { path: root, source })?;
        validate_directory(&root, expected_uid, true)?;
        let blobs = root.join("blobs");
        validate_directory(&blobs, expected_uid, true)?;
        let sha256 = blobs.join("sha256");
        validate_directory(&sha256, expected_uid, true)?;
        Ok(Self {
            root,
            sha256,
            uid: expected_uid,
        })
    }

    fn lock(&self) -> Result<ProviderOperationLock, ProviderManagerError> {
        open_operation_lock(&self.root.join(".provider-manager.lock"), self.uid)
    }

    fn blob_path(&self, digest: &str) -> Result<PathBuf, ProviderManagerError> {
        Ok(self.sha256.join(format!("{}.wasm", digest_hex(digest)?)))
    }

    async fn install_resolved(
        &self,
        registry: &RegistryClient,
        resolved: &ResolvedProvider,
    ) -> Result<bool, ProviderManagerError> {
        self.install_blob(registry, &resolved.source, &resolved.layer)
            .await
    }

    async fn ensure_locked_blob(
        &self,
        registry: &RegistryClient,
        locked: &LockedProvider,
    ) -> Result<bool, ProviderManagerError> {
        let path = self.blob_path(&locked.component_digest)?;
        match verify_blob(&path, locked, self.uid).await {
            Ok(()) => return Ok(false),
            Err(ProviderManagerError::ReadFile { source, .. })
                if source.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let descriptor = LayerDescriptor {
            digest: locked.component_digest.clone(),
            size: locked.component_bytes,
        };
        self.install_blob(registry, &locked.source, &descriptor)
            .await
    }

    async fn install_blob(
        &self,
        registry: &RegistryClient,
        source: &str,
        descriptor: &LayerDescriptor,
    ) -> Result<bool, ProviderManagerError> {
        let destination = self.blob_path(&descriptor.digest)?;
        if destination.exists() {
            verify_component_file(&destination, &descriptor.digest, descriptor.size, self.uid)
                .await?;
            return Ok(false);
        }
        self.ensure_capacity(descriptor.size)?;
        let temporary = tempfile::Builder::new()
            .prefix(".provider-")
            .tempfile_in(&self.sha256)
            .map_err(|source| ProviderManagerError::CreateTemporaryBlob { source })?;
        let std_file = temporary
            .reopen()
            .map_err(|source| ProviderManagerError::CreateTemporaryBlob { source })?;
        let mut file = tokio::fs::File::from_std(std_file);
        registry
            .download_blob(source, descriptor, &mut file)
            .await?;
        file.flush()
            .await
            .map_err(|source| ProviderManagerError::WriteBlob { source })?;
        file.sync_all()
            .await
            .map_err(|source| ProviderManagerError::SyncBlob { source })?;
        drop(file);
        temporary.persist_noclobber(&destination).map_err(|error| {
            ProviderManagerError::PublishBlob {
                path: destination.clone(),
                source: error.error,
            }
        })?;
        sync_directory(&self.sha256)?;
        socket::validate_owned_file(&destination, self.uid)
            .map_err(ProviderManagerError::FileSecurity)?;
        Ok(true)
    }

    fn ensure_capacity(&self, requested: u64) -> Result<(), ProviderManagerError> {
        let mut blobs = 0_usize;
        let mut bytes = 0_u64;
        let entries =
            fs::read_dir(&self.sha256).map_err(|source| ProviderManagerError::StoreScan {
                path: self.sha256.clone(),
                source,
            })?;
        for entry in entries {
            let entry = entry.map_err(|source| ProviderManagerError::StoreScan {
                path: self.sha256.clone(),
                source,
            })?;
            let path = entry.path();
            socket::validate_owned_file(&path, self.uid)
                .map_err(ProviderManagerError::FileSecurity)?;
            let metadata = entry
                .metadata()
                .map_err(|source| ProviderManagerError::StoreScan {
                    path: path.clone(),
                    source,
                })?;
            blobs = blobs.saturating_add(1);
            bytes =
                bytes
                    .checked_add(metadata.len())
                    .ok_or(ProviderManagerError::StoreCapacity {
                        blobs,
                        bytes: u64::MAX,
                        requested,
                        maximum_blobs: HARD_MAX_PROVIDER_STORE_BLOBS,
                        maximum_bytes: HARD_MAX_PROVIDER_STORE_BYTES,
                    })?;
        }
        if blobs >= HARD_MAX_PROVIDER_STORE_BLOBS
            || bytes
                .checked_add(requested)
                .is_none_or(|total| total > HARD_MAX_PROVIDER_STORE_BYTES)
        {
            return Err(ProviderManagerError::StoreCapacity {
                blobs,
                bytes,
                requested,
                maximum_blobs: HARD_MAX_PROVIDER_STORE_BLOBS,
                maximum_bytes: HARD_MAX_PROVIDER_STORE_BYTES,
            });
        }
        Ok(())
    }
}

struct ProviderOperationLock {
    _file: fs::File,
}

fn lock_activation(
    lock_file: &Path,
    expected_uid: u32,
) -> Result<ProviderOperationLock, ProviderManagerError> {
    let lock_file = absolute(lock_file)?;
    validate_file_parent(&lock_file, expected_uid)?;
    open_operation_lock(&lock_file.with_extension("manager.lock"), expected_uid)
}

fn open_operation_lock(
    path: &Path,
    expected_uid: u32,
) -> Result<ProviderOperationLock, ProviderManagerError> {
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .map_err(|source| ProviderManagerError::OperationLock {
            path: path.to_path_buf(),
            source,
        })?;
    validate_file_metadata(
        &file
            .metadata()
            .map_err(|source| ProviderManagerError::OperationLock {
                path: path.to_path_buf(),
                source,
            })?,
        path,
        expected_uid,
        true,
    )?;
    file.try_lock_exclusive().map_err(|source| {
        if source.kind() == io::ErrorKind::WouldBlock {
            ProviderManagerError::OperationInProgress {
                path: path.to_path_buf(),
            }
        } else {
            ProviderManagerError::OperationLock {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    Ok(ProviderOperationLock { _file: file })
}

async fn verify_blob(
    path: &Path,
    locked: &LockedProvider,
    expected_uid: u32,
) -> Result<(), ProviderManagerError> {
    verify_component_file(
        path,
        &locked.component_digest,
        locked.component_bytes,
        expected_uid,
    )
    .await
}

async fn verify_component_file(
    path: &Path,
    component_digest: &str,
    component_bytes: u64,
    expected_uid: u32,
) -> Result<(), ProviderManagerError> {
    fs::symlink_metadata(path).map_err(|source| ProviderManagerError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    socket::validate_owned_file(path, expected_uid).map_err(ProviderManagerError::FileSecurity)?;
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let mut file = options
        .open(path)
        .await
        .map_err(|source| ProviderManagerError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
    let metadata = file
        .metadata()
        .await
        .map_err(|source| ProviderManagerError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
    if metadata.len() != component_bytes {
        return Err(ProviderManagerError::BlobSizeMismatch {
            expected: component_bytes,
            actual: metadata.len(),
        });
    }
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut length = 0_u64;
    loop {
        let read =
            file.read(&mut buffer)
                .await
                .map_err(|source| ProviderManagerError::ReadFile {
                    path: path.to_path_buf(),
                    source,
                })?;
        if read == 0 {
            break;
        }
        length += read as u64;
        if length > component_bytes || length > HARD_MAX_PROVIDER_COMPONENT_BYTES {
            return Err(ProviderManagerError::BlobSizeMismatch {
                expected: component_bytes,
                actual: length,
            });
        }
        digest.update(&buffer[..read]);
    }
    let actual = format!("sha256:{}", hex_digest(digest.finalize().as_slice()));
    if actual != component_digest {
        return Err(ProviderManagerError::BlobDigestMismatch {
            expected: component_digest.to_owned(),
            actual,
        });
    }
    Ok(())
}

async fn read_secure_file(
    path: &Path,
    expected_uid: u32,
    maximum: usize,
) -> Result<Vec<u8>, ProviderManagerError> {
    let path = absolute(path)?;
    validate_file_parent(&path, expected_uid)?;
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(&path)
        .await
        .map_err(|source| ProviderManagerError::ReadFile {
            path: path.clone(),
            source,
        })?;
    let metadata = file
        .metadata()
        .await
        .map_err(|source| ProviderManagerError::ReadFile {
            path: path.clone(),
            source,
        })?;
    validate_file_metadata(&metadata, &path, expected_uid, false)?;
    if metadata.len() > maximum as u64 {
        return Err(ProviderManagerError::StateTooLarge {
            length: metadata.len() as usize,
            maximum,
        });
    }
    let mut bytes = Vec::new();
    file.take((maximum + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| ProviderManagerError::ReadFile { path, source })?;
    if bytes.len() > maximum {
        return Err(ProviderManagerError::StateTooLarge {
            length: bytes.len(),
            maximum,
        });
    }
    Ok(bytes)
}

fn validate_file_metadata(
    metadata: &fs::Metadata,
    path: &Path,
    expected_uid: u32,
    private: bool,
) -> Result<(), ProviderManagerError> {
    let forbidden = if private { 0o077 } else { 0o022 };
    if !metadata.file_type().is_file()
        || metadata.uid() != expected_uid
        || metadata.permissions().mode() & forbidden != 0
        || metadata.nlink() != 1
    {
        return Err(ProviderManagerError::InsecureFile {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn validate_file_parent(path: &Path, expected_uid: u32) -> Result<(), ProviderManagerError> {
    let parent = path
        .parent()
        .ok_or_else(|| ProviderManagerError::MissingParent {
            path: path.to_path_buf(),
        })?;
    let parent = fs::canonicalize(parent).map_err(|source| ProviderManagerError::StorePath {
        path: parent.to_path_buf(),
        source,
    })?;
    validate_directory(&parent, expected_uid, false)
}

fn validate_directory(
    path: &Path,
    expected_uid: u32,
    private: bool,
) -> Result<(), ProviderManagerError> {
    validate_ancestors(path)?;
    let metadata =
        fs::symlink_metadata(path).map_err(|source| ProviderManagerError::StorePath {
            path: path.to_path_buf(),
            source,
        })?;
    let forbidden = if private { 0o077 } else { 0o022 };
    if !metadata.file_type().is_dir()
        || metadata.uid() != expected_uid
        || metadata.permissions().mode() & forbidden != 0
    {
        return Err(ProviderManagerError::InsecureDirectory {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn validate_ancestors(path: &Path) -> Result<(), ProviderManagerError> {
    // Intermediate aliases such as macOS's `/var -> /private/var` are resolved before the walk;
    // the final entry itself is still inspected with `symlink_metadata` by the caller.
    let canonical = fs::canonicalize(path).map_err(|source| ProviderManagerError::StorePath {
        path: path.to_path_buf(),
        source,
    })?;
    for ancestor in canonical.ancestors() {
        let metadata =
            fs::symlink_metadata(ancestor).map_err(|source| ProviderManagerError::StorePath {
                path: ancestor.to_path_buf(),
                source,
            })?;
        let mode = metadata.permissions().mode();
        if !metadata.file_type().is_dir() || (mode & 0o022 != 0 && mode & 0o1000 == 0) {
            return Err(ProviderManagerError::InsecureAncestor {
                path: ancestor.to_path_buf(),
            });
        }
    }
    Ok(())
}

fn ensure_private_directory(path: &Path, expected_uid: u32) -> Result<(), ProviderManagerError> {
    match fs::symlink_metadata(path) {
        Ok(_) => validate_directory(path, expected_uid, true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| ProviderManagerError::MissingParent {
                    path: path.to_path_buf(),
                })?;
            validate_directory(parent, expected_uid, false)?;
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(source) => {
                    return Err(ProviderManagerError::CreateStore {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            }
            validate_directory(path, expected_uid, true)?;
            sync_directory(parent)
        }
        Err(source) => Err(ProviderManagerError::StorePath {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn atomic_write(path: &Path, bytes: &[u8], expected_uid: u32) -> Result<(), ProviderManagerError> {
    let path = absolute(path)?;
    validate_file_parent(&path, expected_uid)?;
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        validate_file_metadata(&metadata, &path, expected_uid, false)?;
    }
    let parent = path
        .parent()
        .ok_or_else(|| ProviderManagerError::MissingParent { path: path.clone() })?;
    let mut temporary = NamedTempFile::new_in(parent)
        .map_err(|source| ProviderManagerError::CreateTemporaryState { source })?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|source| ProviderManagerError::WriteState { source })?;
    temporary
        .write_all(bytes)
        .map_err(|source| ProviderManagerError::WriteState { source })?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|source| ProviderManagerError::SyncState { source })?;
    temporary
        .persist(&path)
        .map_err(|error| ProviderManagerError::PublishState {
            path: path.clone(),
            source: error.error,
        })?;
    sync_directory(parent)?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), ProviderManagerError> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| ProviderManagerError::SyncDirectory {
            path: path.to_path_buf(),
            source,
        })
}

async fn bounded_response_bytes(
    response: reqwest::Response,
    maximum: usize,
    operation: &'static str,
) -> Result<Vec<u8>, ProviderManagerError> {
    if let Some(length) = response.headers().get(CONTENT_LENGTH) {
        let length = length
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok());
        if length.is_some_and(|length| length > maximum as u64) {
            return Err(ProviderManagerError::RegistryResponseTooLarge { operation, maximum });
        }
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|source| ProviderManagerError::RegistryRead {
            operation,
            source: source.without_url(),
        })?;
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > maximum)
        {
            return Err(ProviderManagerError::RegistryResponseTooLarge { operation, maximum });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn validate_digest(digest: &str) -> Result<(), ProviderManagerError> {
    let _ = digest_hex(digest)?;
    Ok(())
}

fn digest_hex(digest: &str) -> Result<&str, ProviderManagerError> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(ProviderManagerError::InvalidDigest {
            digest: digest.to_owned(),
        });
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ProviderManagerError::InvalidDigest {
            digest: digest.to_owned(),
        });
    }
    Ok(hex)
}

fn prefixed_sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", hex_digest(Sha256::digest(bytes).as_slice()))
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            use std::fmt::Write as _;
            write!(&mut text, "{byte:02x}").expect("writing to a String cannot fail");
            text
        })
}

fn absolute(path: &Path) -> Result<PathBuf, ProviderManagerError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|directory| directory.join(path))
        .map_err(|source| ProviderManagerError::CurrentDirectory { source })
}

fn registry_redirect_policy(plaintext: BTreeSet<String>) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("OCI registry redirect limit exceeded");
        }
        if redirect_target_allowed(attempt.url(), &plaintext) {
            attempt.follow()
        } else {
            attempt.error("OCI registry redirect attempted an unapproved plaintext target")
        }
    })
}

fn redirect_target_allowed(target: &Url, plaintext: &BTreeSet<String>) -> bool {
    if !target.username().is_empty() || target.password().is_some() {
        return false;
    }
    if target.scheme() == "https" {
        return true;
    }
    let authority = target.host_str().map(|host| match target.port() {
        Some(port) if host.contains(':') => format!("[{host}]:{port}"),
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    });
    target.scheme() == "http" && authority.is_some_and(|value| plaintext.contains(&value))
}

fn is_literal_loopback_registry(registry: &str) -> bool {
    // The strict OCI source grammar in this slice accepts DNS names and IPv4 host[:port].
    if registry.starts_with('[') {
        return false;
    }
    let host = registry.split(':').next().unwrap_or(registry);
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn url_authority(url: &Url) -> Result<String, ProviderManagerError> {
    let host = url
        .host_str()
        .ok_or(ProviderManagerError::InvalidTokenRealm)?;
    Ok(match url.port() {
        Some(port) if host.contains(':') => format!("[{host}]:{port}"),
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

/// Provider resolution, store, lock, or validation failure.
#[derive(Debug, Error)]
pub enum ProviderManagerError {
    /// Current working directory could not be determined for a relative path.
    #[error("could not determine the current directory")]
    CurrentDirectory {
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// A sync command omitted its desired provider-set path.
    #[error("provider sync requires a provider-set path")]
    MissingProviderSetPath,
    /// A state path has no parent directory.
    #[error("path has no parent: {}", path.display())]
    MissingParent {
        /// Offending path.
        path: PathBuf,
    },
    /// Provider state could not be read.
    #[error("could not read provider state at {}", path.display())]
    ReadFile {
        /// State path.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Trusted state file hygiene failed.
    #[error(
        "provider state must be regular, single-link, owned by this UID, and not group/world writable: {}",
        path.display()
    )]
    InsecureFile {
        /// Offending path.
        path: PathBuf,
    },
    /// Trusted state or store directory hygiene failed.
    #[error("provider directory is not protected and owned by this UID: {}", path.display())]
    InsecureDirectory {
        /// Offending path.
        path: PathBuf,
    },
    /// A path ancestor permits unprotected writes.
    #[error("provider path ancestor permits unprotected group/world writes: {}", path.display())]
    InsecureAncestor {
        /// Offending ancestor.
        path: PathBuf,
    },
    /// Store path could not be resolved or inspected.
    #[error("could not inspect provider store path {}", path.display())]
    StorePath {
        /// Store path.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Store directory could not be created.
    #[error("could not create protected provider store directory {}", path.display())]
    CreateStore {
        /// Directory path.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Desired state is not strict valid YAML.
    #[error("provider set is not strict valid YAML")]
    DecodeProviderSet {
        /// YAML failure.
        #[source]
        source: serde_yaml::Error,
    },
    /// Generated lock is not strict valid YAML.
    #[error("provider lock is not strict valid YAML")]
    DecodeLock {
        /// YAML failure.
        #[source]
        source: serde_yaml::Error,
    },
    /// Generated lock could not be encoded.
    #[error("could not encode provider lock")]
    EncodeLock {
        /// YAML failure.
        #[source]
        source: serde_yaml::Error,
    },
    /// A provider state file exceeded its hard byte ceiling.
    #[error("provider state is {length} bytes; maximum is {maximum}")]
    StateTooLarge {
        /// Actual bytes.
        length: usize,
        /// Hard maximum.
        maximum: usize,
    },
    /// At least one provider is required.
    #[error("provider set must contain at least one provider")]
    NoProviders,
    /// Provider count exceeded the common broker ceiling.
    #[error("provider set has too many providers; maximum is {maximum}")]
    TooManyProviders {
        /// Hard maximum.
        maximum: usize,
    },
    /// OCI source omitted an explicit registry.
    #[error("provider source must be fully qualified with an explicit registry: {reference}")]
    UnqualifiedSource {
        /// Authored source.
        reference: String,
    },
    /// OCI source silently relied on `latest`.
    #[error("provider source must name an explicit tag or manifest digest: {reference}")]
    MissingSelector {
        /// Authored source.
        reference: String,
    },
    /// OCI source could not be parsed or violated exact-reference rules.
    #[error("invalid provider source {reference}: {reason}")]
    InvalidSource {
        /// Authored source.
        reference: String,
        /// Parse or semantic reason.
        reason: String,
    },
    /// Generated lock source spelling was not canonical.
    #[error("provider lock source {reference} is not canonical; expected {canonical}")]
    NonCanonicalSource {
        /// Locked source.
        reference: String,
        /// Canonical source.
        canonical: String,
    },
    /// Desired or locked state contained one or more ambiguous duplicate identities.
    #[error(
        "provider state has {} conflict(s): {}",
        problems.len(),
        problems.join("; ")
    )]
    ProviderStateConflicts {
        /// Deterministically sorted bounded conflict descriptions.
        problems: Vec<String>,
    },
    /// Informational SemVer did not match the exact authored tag.
    #[error(
        "provider lock resolvedVersion for {reference} is {actual:?}; exact tag implies {expected:?}"
    )]
    ResolvedVersionMismatch {
        /// Exact source reference.
        reference: String,
        /// Version implied by its tag.
        expected: Option<Version>,
        /// Version recorded in the lock.
        actual: Option<Version>,
    },
    /// Digest was not canonical SHA-256.
    #[error(
        "provider digest must be sha256 followed by sixty-four lowercase hexadecimal characters: {digest}"
    )]
    InvalidDigest {
        /// Invalid digest.
        digest: String,
    },
    /// Component descriptor size was zero or over the hard ceiling.
    #[error("provider component is {size} bytes; maximum is {maximum}")]
    ComponentSize {
        /// Descriptor size.
        size: u64,
        /// Hard maximum.
        maximum: u64,
    },
    /// Plain HTTP was requested for a non-loopback registry.
    #[error("plaintext OCI registry must be a literal loopback authority: {registry}")]
    PlaintextRegistryNotLoopback {
        /// Rejected registry authority.
        registry: String,
    },
    /// HTTP client construction failed.
    #[error("could not initialize bounded OCI registry client")]
    RegistryClient {
        /// HTTP client failure.
        #[source]
        source: reqwest::Error,
    },
    /// Registry URL construction failed.
    #[error("could not construct OCI registry URL")]
    RegistryUrl {
        /// URL parse failure.
        #[source]
        source: url::ParseError,
    },
    /// Registry request failed before a response.
    #[error("OCI registry {operation} request failed")]
    RegistryRequest {
        /// Low-cardinality operation.
        operation: &'static str,
        /// HTTP failure.
        #[source]
        source: reqwest::Error,
    },
    /// Registry response body failed while streaming.
    #[error("OCI registry {operation} response failed while streaming")]
    RegistryRead {
        /// Low-cardinality operation.
        operation: &'static str,
        /// HTTP failure.
        #[source]
        source: reqwest::Error,
    },
    /// Registry returned a non-success status.
    #[error("OCI registry {operation} returned HTTP {status}")]
    RegistryStatus {
        /// Low-cardinality operation.
        operation: &'static str,
        /// HTTP status.
        status: StatusCode,
    },
    /// Registry response exceeded its independent byte bound.
    #[error("OCI registry {operation} response exceeds {maximum} bytes")]
    RegistryResponseTooLarge {
        /// Low-cardinality operation.
        operation: &'static str,
        /// Hard maximum.
        maximum: usize,
    },
    /// Authentication challenge header was absent.
    #[error("OCI registry required authentication without a WWW-Authenticate challenge")]
    MissingRegistryChallenge,
    /// Authentication challenge header was not valid text.
    #[error("OCI registry authentication challenge header is invalid")]
    InvalidRegistryChallengeHeader {
        /// Header failure.
        #[source]
        source: reqwest::header::ToStrError,
    },
    /// Authentication challenge could not be parsed.
    #[error("OCI registry authentication challenge is invalid")]
    InvalidRegistryChallenge,
    /// Bearer challenge omitted its realm.
    #[error("OCI registry Bearer challenge omitted realm")]
    MissingTokenRealm,
    /// Token realm was not a valid URL.
    #[error("OCI registry token realm is not a valid URL")]
    RegistryTokenUrl {
        /// URL parse failure.
        #[source]
        source: url::ParseError,
    },
    /// Token realm lacked a host.
    #[error("OCI registry token realm is invalid")]
    InvalidTokenRealm,
    /// Token realm attempted plaintext outside the explicit loopback registry.
    #[error("OCI registry token realm may not use plaintext HTTP: {realm}")]
    InsecureTokenRealm {
        /// Realm authority.
        realm: String,
    },
    /// Registry token response was not valid JSON.
    #[error("OCI registry token response is invalid")]
    DecodeRegistryToken {
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
    /// Registry token response omitted both token field spellings.
    #[error("OCI registry token response omitted token")]
    MissingRegistryToken,
    /// Registry token response supplied conflicting token spellings.
    #[error("OCI registry token response supplied conflicting token fields")]
    AmbiguousRegistryToken,
    /// Registry token was blank or unreasonably large.
    #[error("OCI registry token is blank or exceeds its hard byte ceiling")]
    InvalidRegistryToken,
    /// Authorization header could not be represented.
    #[error("OCI registry returned a token that cannot form an HTTP authorization header")]
    InvalidAuthorizationHeader {
        /// Header failure.
        #[source]
        source: reqwest::header::InvalidHeaderValue,
    },
    /// Manifest digest header was malformed.
    #[error("OCI registry manifest digest header is invalid")]
    InvalidDigestHeader {
        /// Header failure.
        #[source]
        source: reqwest::header::ToStrError,
    },
    /// Raw manifest bytes did not match the selected or returned digest.
    #[error("OCI manifest digest mismatch: expected {expected}, got {actual}")]
    ManifestDigestMismatch {
        /// Selected or header digest.
        expected: String,
        /// Digest of bounded raw bytes.
        actual: String,
    },
    /// Registry did not label the response as an OCI image manifest.
    #[error("OCI manifest response content type is {actual}; expected {OCI_MANIFEST_MEDIA_TYPE}")]
    InvalidManifestContentType {
        /// Actual or `missing` content type.
        actual: String,
    },
    /// OCI manifest JSON was malformed or contained unknown fields.
    #[error("OCI provider manifest is not strict valid JSON")]
    DecodeManifest {
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
    /// Manifest was not one OCI v1 image manifest.
    #[error("OCI provider manifest has schema {schema} and media type {media_type}")]
    InvalidManifestType {
        /// Schema version.
        schema: u8,
        /// Manifest media type.
        media_type: String,
    },
    /// Manifest artifact type was not Dekopon's provider type.
    #[error("OCI artifact type is {actual}; expected {PROVIDER_ARTIFACT_TYPE}")]
    InvalidArtifactType {
        /// Actual artifact type.
        actual: String,
    },
    /// Provider artifact unexpectedly attached itself to a subject.
    #[error("OCI provider manifest must not carry a subject")]
    ManifestHasSubject,
    /// Config descriptor was not the small inline-free artifact config convention.
    #[error("OCI provider config descriptor is invalid or too large")]
    InvalidConfigDescriptor,
    /// Manifest did not carry exactly one component layer.
    #[error("OCI provider manifest has {actual} layers; expected exactly one")]
    LayerCount {
        /// Actual layer count.
        actual: usize,
    },
    /// Layer media type was not `application/wasm`.
    #[error("OCI provider layer media type is {actual}; expected {PROVIDER_LAYER_MEDIA_TYPE}")]
    InvalidLayerMediaType {
        /// Actual layer media type.
        actual: String,
    },
    /// Layer descriptor attempted an alternate URL, inline data, or nested artifact type.
    #[error("OCI provider layer descriptor contains unsupported indirection")]
    InvalidLayerDescriptor,
    /// Blob response byte count differed from the descriptor.
    #[error("OCI provider blob is {actual} bytes; descriptor expects {expected}")]
    BlobSizeMismatch {
        /// Descriptor bytes.
        expected: u64,
        /// Actual bytes.
        actual: u64,
    },
    /// Blob response digest differed from the descriptor.
    #[error("OCI provider blob digest mismatch: expected {expected}, got {actual}")]
    BlobDigestMismatch {
        /// Descriptor digest.
        expected: String,
        /// Actual digest.
        actual: String,
    },
    /// Store directory could not be scanned under the operation lock.
    #[error("could not inspect provider store directory {}", path.display())]
    StoreScan {
        /// Store directory or entry.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Retained blobs and stale temporaries reached the hard lifetime ceiling.
    #[error(
        "provider store holds {blobs} file(s) and {bytes} bytes; adding {requested} bytes would exceed {maximum_blobs} files or {maximum_bytes} bytes"
    )]
    StoreCapacity {
        /// Current file count.
        blobs: usize,
        /// Current logical bytes.
        bytes: u64,
        /// Proposed component bytes.
        requested: u64,
        /// Hard file-count ceiling.
        maximum_blobs: usize,
        /// Hard logical-byte ceiling.
        maximum_bytes: u64,
    },
    /// Temporary blob could not be created.
    #[error("could not create provider blob temporary file")]
    CreateTemporaryBlob {
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Blob could not be written.
    #[error("could not write provider blob")]
    WriteBlob {
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Blob could not be synchronized.
    #[error("could not synchronize provider blob")]
    SyncBlob {
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Blob could not be atomically published.
    #[error("could not atomically publish provider blob at {}", path.display())]
    PublishBlob {
        /// Destination path.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Lock temporary file could not be created.
    #[error("could not create provider-lock temporary file")]
    CreateTemporaryState {
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Lock temporary file could not be written.
    #[error("could not write provider-lock temporary file")]
    WriteState {
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Lock temporary file could not be synchronized.
    #[error("could not synchronize provider-lock temporary file")]
    SyncState {
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Lock could not be atomically activated.
    #[error("could not atomically publish provider lock at {}", path.display())]
    PublishState {
        /// Lock path.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Parent directory could not be synchronized.
    #[error("could not synchronize provider directory {}", path.display())]
    SyncDirectory {
        /// Directory path.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Manager operation lock could not be opened or acquired.
    #[error("could not lock provider store operation at {}", path.display())]
    OperationLock {
        /// Lock path.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// Another manager currently owns the operation lock.
    #[error("another provider-manager operation is in progress at {}", path.display())]
    OperationInProgress {
        /// Lock path.
        path: PathBuf,
    },
    /// Broker provider file hygiene failed.
    #[error("provider blob failed broker file-security validation")]
    FileSecurity(#[source] socket::SocketError),
    /// Complete provider-host validation failed.
    #[error("provider set failed broker-host validation")]
    Host(#[source] BrokerHostError),
    /// Host metadata unexpectedly omitted one candidate path.
    #[error("validated provider set omitted component {}", path.display())]
    MissingValidatedProvider {
        /// Component path.
        path: PathBuf,
    },
    /// Existing lock identity disagreed with a fresh bounded description.
    #[error(
        "locked source {reference} expects provider {expected}, but component describes {actual}"
    )]
    LockedProviderIdentity {
        /// Exact source.
        reference: String,
        /// Locked identity.
        expected: ProviderId,
        /// Described identity.
        actual: ProviderId,
    },
    /// Desired state no longer matches the immutable lock in `--locked` mode.
    #[error("provider set changed relative to the lock; rerun without --locked to resolve it")]
    LockedStateChanged,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{
        Router,
        body::Body,
        extract::{Path as AxumPath, State},
        http::{HeaderMap as AxumHeaderMap, HeaderValue as AxumHeaderValue, Response},
        routing::get,
    };
    use tokio::sync::oneshot;

    use super::*;

    #[derive(Clone)]
    struct RegistryState {
        manifest: Arc<Vec<u8>>,
        manifest_digest: String,
        component: Arc<Vec<u8>>,
        manifests: Arc<AtomicUsize>,
        blobs: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    struct RedirectState {
        location: String,
    }

    async fn redirect_source(State(state): State<RedirectState>) -> Response<Body> {
        Response::builder()
            .status(StatusCode::FOUND)
            .header(reqwest::header::LOCATION, state.location)
            .body(Body::empty())
            .expect("redirect fixture")
    }

    async fn redirect_target(
        State(saw_authorization): State<Arc<std::sync::atomic::AtomicBool>>,
        headers: AxumHeaderMap,
    ) -> Response<Body> {
        saw_authorization.store(headers.contains_key(AUTHORIZATION), Ordering::Relaxed);
        Response::new(Body::from("redirected"))
    }

    struct TestRegistry {
        authority: String,
        state: RegistryState,
        shutdown: oneshot::Sender<()>,
        task: tokio::task::JoinHandle<Result<(), io::Error>>,
    }

    impl TestRegistry {
        async fn start(component: Vec<u8>) -> Self {
            let component_digest = prefixed_sha256(&component);
            let manifest = serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "mediaType": OCI_MANIFEST_MEDIA_TYPE,
                "artifactType": PROVIDER_ARTIFACT_TYPE,
                "config": {
                    "mediaType": OCI_EMPTY_CONFIG_MEDIA_TYPE,
                    "digest": OCI_EMPTY_CONFIG_DIGEST,
                    "size": 2,
                    "data": OCI_EMPTY_CONFIG_DATA
                },
                "layers": [{
                    "mediaType": PROVIDER_LAYER_MEDIA_TYPE,
                    "digest": component_digest,
                    "size": component.len(),
                    "annotations": {"org.opencontainers.image.title": "provider.wasm"}
                }]
            }))
            .expect("manifest fixture encodes");
            let state = RegistryState {
                manifest_digest: prefixed_sha256(&manifest),
                manifest: Arc::new(manifest),
                component: Arc::new(component),
                manifests: Arc::new(AtomicUsize::new(0)),
                blobs: Arc::new(AtomicUsize::new(0)),
            };
            let app = Router::new()
                .route("/v2/{*path}", get(registry_response))
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind registry fixture");
            let authority = listener.local_addr().expect("registry address").to_string();
            let (shutdown, receiver) = oneshot::channel();
            let task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        drop(receiver.await);
                    })
                    .await
            });
            Self {
                authority,
                state,
                shutdown,
                task,
            }
        }

        async fn stop(self) {
            self.shutdown.send(()).expect("registry still running");
            self.task
                .await
                .expect("registry task joins")
                .expect("registry serves cleanly");
        }
    }

    async fn registry_response(
        AxumPath(path): AxumPath<String>,
        State(state): State<RegistryState>,
    ) -> Response<Body> {
        if path.contains("/manifests/") {
            state.manifests.fetch_add(1, Ordering::Relaxed);
            let mut response = Response::new(Body::from(state.manifest.as_ref().clone()));
            response.headers_mut().insert(
                reqwest::header::CONTENT_TYPE,
                AxumHeaderValue::from_static(OCI_MANIFEST_MEDIA_TYPE),
            );
            response.headers_mut().insert(
                reqwest::header::HeaderName::from_static(DOCKER_CONTENT_DIGEST),
                AxumHeaderValue::from_str(&state.manifest_digest).expect("digest header"),
            );
            return response;
        }
        if path.contains("/blobs/") {
            state.blobs.fetch_add(1, Ordering::Relaxed);
            return Response::new(Body::from(state.component.as_ref().clone()));
        }
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .expect("404 fixture")
    }

    fn echo_component() -> Vec<u8> {
        fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("examples/providers/echo-provider.wasm"),
        )
        .expect("checked echo component")
    }

    fn write_set(path: &Path, sources: &[String]) {
        let providers = sources
            .iter()
            .map(|source| DesiredProvider {
                source: source.clone(),
            })
            .collect();
        let set = ProviderSet {
            api_version: ProviderSetApiVersion::V1Alpha1,
            providers,
        };
        fs::write(path, serde_yaml::to_string(&set).expect("set encodes"))
            .expect("write provider set");
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("secure provider set");
    }

    fn test_manager(
        directory: &Path,
        authority: &str,
        sources: &[String],
    ) -> (ProviderManager, ProviderManagerPaths) {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .expect("private fixture directory");
        let paths = ProviderManagerPaths {
            provider_set: Some(directory.join("providers.yaml")),
            lock_file: directory.join("providers.lock.yaml"),
            store: directory.join("store"),
        };
        write_set(
            paths.provider_set.as_deref().expect("provider set path"),
            sources,
        );
        let manager = ProviderManager::new(ProviderManagerOptions {
            paths: paths.clone(),
            plaintext_loopback_registries: vec![authority.to_owned()],
        })
        .expect("manager fixture");
        (manager, paths)
    }

    #[test]
    fn exact_sources_require_a_registry_and_selector() {
        assert!(ParsedSource::parse("ghcr.io/org/provider:1.2.3").is_ok());
        assert!(
            ParsedSource::parse(
                "ghcr.io/org/provider@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )
            .is_ok()
        );
        assert!(ParsedSource::parse("127.0.0.1:5000/org/provider:test").is_ok());
        assert!(ParsedSource::parse("org/provider:1.2.3").is_err());
        assert!(ParsedSource::parse("ghcr.io/org/provider").is_err());
        assert!(ParsedSource::parse("ghcr.io/Org/provider:1.2.3").is_err());
        assert!(ParsedSource::parse("https://ghcr.io/org/provider:1.2.3").is_err());
    }

    #[test]
    fn bearer_challenge_parsing_preserves_required_fields() {
        let challenge = parse_bearer_challenge(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:org/provider:pull""#,
        )
        .expect("challenge parses");
        assert_eq!(challenge.realm, "https://ghcr.io/token");
        assert_eq!(challenge.service.as_deref(), Some("ghcr.io"));
        assert_eq!(
            challenge.scope.as_deref(),
            Some("repository:org/provider:pull")
        );
    }

    #[test]
    fn malformed_authentication_input_never_reaches_rendered_errors() {
        let sentinel = "registry-secret-sentinel";
        let challenge = format!(r#"Bearer realm="unterminated-{sentinel}"#);
        let error = parse_bearer_challenge(&challenge).expect_err("challenge is malformed");
        assert!(!error.to_string().contains(sentinel));
        assert!(!format!("{error:?}").contains(sentinel));

        let realm = Url::parse(&format!("file:///{sentinel}?token={sentinel}"))
            .expect("syntactically valid hostless URL");
        let error = url_authority(&realm).expect_err("hostless realm fails");
        assert!(!error.to_string().contains(sentinel));
        assert!(!format!("{error:?}").contains(sentinel));
    }

    #[tokio::test]
    async fn request_errors_strip_registry_urls_from_the_complete_source_chain() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let authority = listener.local_addr().expect("reserved address");
        drop(listener);
        let registry = RegistryClient::new(vec![authority.to_string()]).expect("client");
        let sentinel = "request-secret-sentinel";
        let url = Url::parse(&format!("http://{authority}/{sentinel}?token={sentinel}"))
            .expect("request URL");
        let error = registry
            .send_get(url, None, None, "manifest")
            .await
            .expect_err("closed port refuses request");
        let mut rendered = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(current) = source {
            use std::fmt::Write as _;
            write!(&mut rendered, ": {current}").expect("write error chain");
            source = current.source();
        }
        assert!(!rendered.contains(sentinel), "{rendered}");
    }

    #[test]
    fn redirect_policy_rejects_downgrades_except_exact_loopback_opt_ins() {
        let plaintext = BTreeSet::from(["127.0.0.1:5000".to_owned()]);
        assert!(redirect_target_allowed(
            &Url::parse("https://cdn.example/provider.wasm").expect("HTTPS target"),
            &plaintext
        ));
        assert!(!redirect_target_allowed(
            &Url::parse("http://registry.example/provider.wasm").expect("HTTP target"),
            &plaintext
        ));
        assert!(!redirect_target_allowed(
            &Url::parse("http://127.0.0.1:5001/provider.wasm").expect("wrong loopback target"),
            &plaintext
        ));
        assert!(redirect_target_allowed(
            &Url::parse("http://127.0.0.1:5000/provider.wasm").expect("allowed loopback target"),
            &plaintext
        ));
    }

    #[tokio::test]
    async fn cross_authority_redirects_strip_bearer_tokens_and_loops_are_bounded() {
        let saw_authorization = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("target listener");
        let target_authority = target_listener.local_addr().expect("target address");
        let target_saw_authorization = Arc::clone(&saw_authorization);
        let target_task = tokio::spawn(async move {
            axum::serve(
                target_listener,
                Router::new()
                    .route("/target", get(redirect_target))
                    .with_state(target_saw_authorization),
            )
            .await
        });

        let source_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("source listener");
        let source_authority = source_listener.local_addr().expect("source address");
        let source_task = tokio::spawn(async move {
            axum::serve(
                source_listener,
                Router::new()
                    .route("/source", get(redirect_source))
                    .with_state(RedirectState {
                        location: format!("http://{target_authority}/target"),
                    }),
            )
            .await
        });
        let client = RegistryClient::new(vec![
            source_authority.to_string(),
            target_authority.to_string(),
        ])
        .expect("redirect client");
        let response = client
            .send_get(
                Url::parse(&format!("http://{source_authority}/source")).expect("source URL"),
                None,
                Some(&RegistryToken("bearer-secret-sentinel".to_owned())),
                "blob",
            )
            .await
            .expect("approved cross-authority redirect follows");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !saw_authorization.load(Ordering::Relaxed),
            "reqwest must strip Authorization when redirect authority changes"
        );

        source_task.abort();
        target_task.abort();
        assert!(
            source_task
                .await
                .expect_err("source task cancelled")
                .is_cancelled()
        );
        assert!(
            target_task
                .await
                .expect_err("target task cancelled")
                .is_cancelled()
        );

        let loop_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loop listener");
        let loop_authority = loop_listener.local_addr().expect("loop address");
        let loop_task = tokio::spawn(async move {
            axum::serve(
                loop_listener,
                Router::new()
                    .route("/loop", get(redirect_source))
                    .with_state(RedirectState {
                        location: format!("http://{loop_authority}/loop"),
                    }),
            )
            .await
        });
        let client = RegistryClient::new(vec![loop_authority.to_string()]).expect("loop client");
        let error = client
            .send_get(
                Url::parse(&format!("http://{loop_authority}/loop")).expect("loop URL"),
                None,
                None,
                "manifest",
            )
            .await
            .expect_err("redirect loop reaches the hard hop ceiling");
        assert!(matches!(
            error,
            ProviderManagerError::RegistryRequest { .. }
        ));
        loop_task.abort();
        assert!(
            loop_task
                .await
                .expect_err("loop task cancelled")
                .is_cancelled()
        );
    }

    #[test]
    fn lock_encoding_is_sorted_stable_and_timestamp_free() {
        let lock = ProviderLock {
            api_version: ProviderLockApiVersion::V1Alpha1,
            providers: vec![
                LockedProvider {
                    source: "ghcr.io/org/z:1.0.0".to_owned(),
                    resolved_version: Some("1.0.0".parse().expect("version")),
                    manifest_digest: format!("sha256:{}", "1".repeat(64)),
                    component_digest: format!("sha256:{}", "2".repeat(64)),
                    component_bytes: 12,
                    provider_id: "z".parse().expect("provider"),
                },
                LockedProvider {
                    source: "ghcr.io/org/a@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                    resolved_version: None,
                    manifest_digest: format!("sha256:{}", "a".repeat(64)),
                    component_digest: format!("sha256:{}", "b".repeat(64)),
                    component_bytes: 34,
                    provider_id: "a".parse().expect("provider"),
                },
            ],
        };
        let first = encode_lock(&lock).expect("lock encodes");
        let second = encode_lock(&lock).expect("lock encodes identically");
        assert_eq!(first, second);
        let text = String::from_utf8(first).expect("YAML is UTF-8");
        assert!(text.find("providerId: a").unwrap() < text.find("providerId: z").unwrap());
        assert!(!text.to_ascii_lowercase().contains("time"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn lock_rejects_duplicate_sources_and_provider_ids() {
        let source = "ghcr.io/org/provider:1.0.0".to_owned();
        let provider = LockedProvider {
            source: source.clone(),
            resolved_version: Some("1.0.0".parse().expect("version")),
            manifest_digest: format!("sha256:{}", "1".repeat(64)),
            component_digest: format!("sha256:{}", "2".repeat(64)),
            component_bytes: 12,
            provider_id: "same".parse().expect("provider"),
        };
        let lock = ProviderLock {
            api_version: ProviderLockApiVersion::V1Alpha1,
            providers: vec![provider.clone(), provider],
        };
        let error = validate_lock_shape(&lock).expect_err("duplicates conflict");
        let ProviderManagerError::ProviderStateConflicts { problems } = error else {
            panic!("unexpected error: {error}");
        };
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems.iter().any(|problem| problem.contains("source")));
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("repository"))
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("provider ID"))
        );
    }

    #[test]
    fn lock_binds_digest_sources_and_resolved_versions() {
        let base = LockedProvider {
            source: format!("ghcr.io/org/provider@sha256:{}", "a".repeat(64)),
            resolved_version: None,
            manifest_digest: format!("sha256:{}", "b".repeat(64)),
            component_digest: format!("sha256:{}", "c".repeat(64)),
            component_bytes: 12,
            provider_id: "provider".parse().expect("provider ID"),
        };
        let lock = ProviderLock {
            api_version: ProviderLockApiVersion::V1Alpha1,
            providers: vec![base.clone()],
        };
        assert!(matches!(
            validate_lock_shape(&lock),
            Err(ProviderManagerError::ManifestDigestMismatch { .. })
        ));

        let mut tagged = base;
        tagged.source = "ghcr.io/org/provider:1.2.3".to_owned();
        tagged.manifest_digest = format!("sha256:{}", "d".repeat(64));
        tagged.resolved_version = Some("1.2.4".parse().expect("version"));
        let lock = ProviderLock {
            api_version: ProviderLockApiVersion::V1Alpha1,
            providers: vec![tagged],
        };
        assert!(matches!(
            validate_lock_shape(&lock),
            Err(ProviderManagerError::ResolvedVersionMismatch { .. })
        ));
    }

    #[test]
    fn strict_yaml_rejects_unknown_fields() {
        let error = serde_yaml::from_str::<ProviderSet>(
            r#"
apiVersion: dekopon.dev/provider-set/v1alpha1
providers:
  - source: ghcr.io/org/provider:1.0.0
    typo: true
"#,
        )
        .expect_err("unknown desired field fails");
        assert!(error.to_string().contains("unknown field"));

        let duplicate = serde_yaml::from_str::<ProviderSet>(
            r#"
apiVersion: dekopon.dev/provider-set/v1alpha1
providers:
  - source: ghcr.io/org/provider:1.0.0
    source: ghcr.io/org/provider:2.0.0
"#,
        )
        .expect_err("duplicate mapping key fails");
        assert!(duplicate.to_string().contains("duplicate field"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sync_pins_tags_and_locked_sync_fetches_only_missing_blobs() {
        let registry = TestRegistry::start(echo_component()).await;
        let directory = tempfile::tempdir().expect("manager fixture");
        let source = format!("{}/test/provider:1.0.0", registry.authority);
        let (manager, _paths) = test_manager(directory.path(), &registry.authority, &[source]);

        let first = manager
            .sync()
            .await
            .expect("first sync resolves and fetches");
        assert_eq!(first.providers, 1);
        assert_eq!(first.fetched, 1);
        assert!(first.lock_changed);
        assert_eq!(registry.state.manifests.load(Ordering::Relaxed), 1);
        assert_eq!(registry.state.blobs.load(Ordering::Relaxed), 1);

        let second = manager
            .sync()
            .await
            .expect("unchanged exact tag stays pinned");
        assert_eq!(second.fetched, 0);
        assert!(!second.lock_changed);
        assert_eq!(
            registry.state.manifests.load(Ordering::Relaxed),
            1,
            "an unchanged exact tag is not resolved again"
        );
        assert_eq!(registry.state.blobs.load(Ordering::Relaxed), 1);

        let status = manager.list().await.expect("offline list");
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].local_status, "verified");
        assert!(status[0].local_reason.is_none());
        fs::remove_file(&status[0].path).expect("remove installed blob");
        let locked = manager
            .sync_locked()
            .await
            .expect("locked sync materializes by component digest");
        assert_eq!(locked.fetched, 1);
        assert!(!locked.lock_changed);
        assert_eq!(
            registry.state.manifests.load(Ordering::Relaxed),
            1,
            "locked sync must not fetch a tag manifest"
        );
        assert_eq!(registry.state.blobs.load(Ordering::Relaxed), 2);
        manager
            .verify()
            .await
            .expect("offline complete verification");
        assert_eq!(registry.state.manifests.load(Ordering::Relaxed), 1);
        assert_eq!(registry.state.blobs.load(Ordering::Relaxed), 2);

        registry.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn manifest_digest_sources_lock_the_selected_manifest_identity() {
        let registry = TestRegistry::start(echo_component()).await;
        let directory = tempfile::tempdir().expect("manager fixture");
        let source = format!(
            "{}/test/provider@{}",
            registry.authority, registry.state.manifest_digest
        );
        let (manager, paths) = test_manager(directory.path(), &registry.authority, &[source]);
        manager.sync().await.expect("digest source syncs");
        let lock = load_lock(&paths.lock_file, socket::current_uid())
            .await
            .expect("load generated lock");
        assert_eq!(
            lock.providers[0].manifest_digest,
            registry.state.manifest_digest
        );
        assert!(lock.providers[0].resolved_version.is_none());
        registry.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn offline_verification_rejects_missing_insecure_linked_and_symlinked_blobs() {
        use std::os::unix::fs::symlink;

        let registry = TestRegistry::start(echo_component()).await;
        let directory = tempfile::tempdir().expect("manager fixture");
        let source = format!("{}/test/provider:1.0.0", registry.authority);
        let (manager, _paths) = test_manager(directory.path(), &registry.authority, &[source]);
        manager.sync().await.expect("provider activates");
        let blob = manager.list().await.expect("list")[0].path.clone();
        let bytes = fs::read(&blob).expect("read blob fixture");

        fs::set_permissions(&blob, fs::Permissions::from_mode(0o666)).expect("loosen blob");
        let status = manager.list().await.expect("list insecure blob");
        assert_eq!(status[0].local_status, "invalid");
        assert_eq!(status[0].local_reason.as_deref(), Some("insecure-metadata"));
        assert!(matches!(
            manager.verify().await,
            Err(ProviderManagerError::FileSecurity(_))
        ));
        fs::set_permissions(&blob, fs::Permissions::from_mode(0o600)).expect("restore blob");

        let hard_link = blob.with_extension("hard-link");
        fs::hard_link(&blob, &hard_link).expect("hard-link blob");
        assert!(matches!(
            manager.verify().await,
            Err(ProviderManagerError::FileSecurity(_))
        ));
        fs::remove_file(&hard_link).expect("remove hard link");

        let target = blob.with_extension("target");
        fs::write(&target, &bytes).expect("write symlink target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("secure target");
        fs::remove_file(&blob).expect("remove blob for symlink");
        symlink(&target, &blob).expect("symlink blob");
        assert!(matches!(
            manager.verify().await,
            Err(ProviderManagerError::FileSecurity(_))
        ));
        fs::remove_file(&blob).expect("remove symlink");
        fs::write(&blob, &bytes).expect("restore blob");
        fs::set_permissions(&blob, fs::Permissions::from_mode(0o600)).expect("secure blob");
        fs::remove_file(&target).expect("remove target");

        fs::remove_file(&blob).expect("remove blob");
        let status = manager.list().await.expect("list missing blob");
        assert_eq!(status[0].local_status, "missing");
        assert_eq!(status[0].local_reason.as_deref(), Some("not-installed"));
        assert!(matches!(
            manager.verify().await,
            Err(ProviderManagerError::ReadFile { source, .. })
                if source.kind() == io::ErrorKind::NotFound
        ));

        registry.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_complete_set_validation_leaves_the_active_lock_unchanged() {
        let registry = TestRegistry::start(echo_component()).await;
        let directory = tempfile::tempdir().expect("manager fixture");
        let first_source = format!("{}/one/provider:1.0.0", registry.authority);
        let second_source = format!("{}/two/provider:1.0.0", registry.authority);
        let (manager, paths) = test_manager(
            directory.path(),
            &registry.authority,
            std::slice::from_ref(&first_source),
        );
        manager.sync().await.expect("initial provider activates");
        let active = fs::read(&paths.lock_file).expect("read active lock");

        write_set(
            paths.provider_set.as_deref().expect("provider set path"),
            &[first_source, second_source],
        );
        let error = manager
            .sync()
            .await
            .expect_err("two components describing echo conflict as one complete set");
        assert!(
            matches!(
                error,
                ProviderManagerError::Host(BrokerHostError::ConflictingProviders { .. })
            ),
            "{error:?}"
        );
        assert_eq!(
            fs::read(&paths.lock_file).expect("read retained lock"),
            active,
            "validation must finish before the activation lock is replaced"
        );

        registry.stop().await;
    }

    #[test]
    fn store_lifetime_capacity_bounds_orphan_growth() {
        let directory = tempfile::tempdir().expect("store fixture");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private fixture");
        let store =
            ProviderStore::open(&directory.path().join("store"), socket::current_uid(), true)
                .expect("store opens");
        for path in [&store.root, &store.root.join("blobs"), &store.sha256] {
            assert_eq!(
                fs::metadata(path)
                    .expect("store metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let orphan = store.sha256.join("stale.wasm");
        let file = fs::File::create(&orphan).expect("create sparse orphan");
        file.set_len(HARD_MAX_PROVIDER_STORE_BYTES)
            .expect("size sparse orphan");
        fs::set_permissions(&orphan, fs::Permissions::from_mode(0o600)).expect("secure orphan");
        assert!(matches!(
            store.ensure_capacity(1),
            Err(ProviderManagerError::StoreCapacity { .. })
        ));
    }

    #[test]
    fn manager_operation_lock_serializes_competing_writers() {
        let directory = tempfile::tempdir().expect("lock fixture");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private fixture");
        let lock_file = directory.path().join("providers.lock.yaml");
        let first = lock_activation(&lock_file, socket::current_uid()).expect("first owner");
        assert!(matches!(
            lock_activation(&lock_file, socket::current_uid()),
            Err(ProviderManagerError::OperationInProgress { .. })
        ));
        drop(first);
        lock_activation(&lock_file, socket::current_uid()).expect("lock releases on close");
    }

    #[tokio::test]
    async fn locked_sync_acquires_activation_ownership_before_reading_state() {
        let directory = tempfile::tempdir().expect("activation fixture");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private fixture");
        let paths = ProviderManagerPaths {
            provider_set: Some(directory.path().join("missing-providers.yaml")),
            lock_file: directory.path().join("providers.lock.yaml"),
            store: directory.path().join("store"),
        };
        let manager = ProviderManager::new(ProviderManagerOptions {
            paths: paths.clone(),
            plaintext_loopback_registries: Vec::new(),
        })
        .expect("manager");
        let owner = lock_activation(&paths.lock_file, socket::current_uid()).expect("lock owner");
        assert!(matches!(
            manager.sync_locked().await,
            Err(ProviderManagerError::OperationInProgress { .. })
        ));
        drop(owner);
    }

    #[test]
    fn plaintext_is_limited_to_literal_loopback() {
        assert!(is_literal_loopback_registry("localhost:5000"));
        assert!(is_literal_loopback_registry("127.0.0.1:5000"));
        assert!(!is_literal_loopback_registry("registry.example.com"));
        assert!(!is_literal_loopback_registry("localhost.example.com:5000"));
    }
}
