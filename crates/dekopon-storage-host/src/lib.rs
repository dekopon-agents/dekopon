//! Broker-owned namespace-bound provider storage.
//!
//! `StorageGrant` is single-use process authority. Its fields are private:
//!
//! ```compile_fail
//! use dekopon_storage_host::StorageGrant;
//!
//! fn fabricate() -> StorageGrant {
//!     StorageGrant { host_id: [0; 32] }
//! }
//! ```
//!
//! It is neither cloneable nor serializable or deserializable:
//!
//! ```compile_fail
//! use dekopon_storage_host::StorageGrant;
//! fn require_clone<T: Clone>() {}
//! fn main() { require_clone::<StorageGrant>(); }
//! ```
//!
//! ```compile_fail
//! use dekopon_storage_host::StorageGrant;
//! use serde::Serialize;
//! fn require_serialize<T: Serialize>() {}
//! fn main() { require_serialize::<StorageGrant>(); }
//! ```
//!
//! ```compile_fail
//! use dekopon_storage_host::StorageGrant;
//! use serde::de::DeserializeOwned;
//! fn require_deserialize<T: DeserializeOwned>() {}
//! fn main() { require_deserialize::<StorageGrant>(); }
//! ```
//!
//! Rust visibility is defense in depth. Host ownership, trusted broker derivation, exact binding,
//! and filesystem isolation are the authority boundary.

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::File,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use dekopon_capability::{StorageAccess, StorageInterface, StorageNamespace};
use dekopon_core::{AgentId, CapabilityId, ExternalSubject, InvocationId, ProviderId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod config;
mod handle;
mod jsonl;
mod key;
mod layout;
mod metrics;
mod namespace;
mod quota;
mod vfs;

pub use config::{StorageConfigError, StorageLimits};
pub use handle::StorageHandle;
pub use jsonl::JsonlChunk;
pub use vfs::{Durability, FileStat, LockLevel, OpenOptions};

use key::{
    DOMAIN_AUDIT_SCOPE, DOMAIN_CONTENT, DOMAIN_DECISION_EVIDENCE, DOMAIN_NAMESPACE_PATH,
    DOMAIN_RECORD_ID, StorageKey, random_bytes,
};
use layout::{Layout, scan_root_usage, scan_usage, usage_with_directory_entry};
use namespace::{Namespace, NamespacePlan, Reset};
use quota::QuotaLedger;

/// Durable chat-memory continuity behavior.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContinuityPolicy {
    /// Reuse one logical namespace across semantic authority changes. Must be explicit.
    Stable,
    /// Mint a non-reusing random generation whenever the effective authority commitment changes.
    #[default]
    AuthorityBound,
}

/// Opaque keyed commitment identifying a storage scope without disclosing it.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StorageScopeCommitment(String);

impl StorageScopeCommitment {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for StorageScopeCommitment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StorageScopeCommitment([REDACTED])")
    }
}

/// Content-free coarse storage evidence for one invocation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StorageEvidence {
    pub operations: u64,
    pub syncs: u64,
    pub quota_denials: u64,
    /// Coarse powers-of-two read bucket; never exact bytes.
    pub read_byte_bucket: u8,
    /// Coarse powers-of-two write bucket; never exact bytes.
    pub write_byte_bucket: u8,
    pub evidence_commitment: String,
    /// Keyed commitment to the exact successful provider output, when one was supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_commitment: Option<String>,
}

/// Trusted, non-authoritative material from which a host may mint one grant.
///
/// Every formatter is redacted because it contains the complete raw chat scope.
pub struct StorageGrantRequest {
    invocation: InvocationId,
    capability: CapabilityId,
    provider: ProviderId,
    interface: StorageInterface,
    access: StorageAccess,
    namespace: StorageNamespace,
    agent: AgentId,
    subject: ExternalSubject,
    transport_kind: String,
    transport: String,
    channel: String,
    conversation: String,
    continuity_policy: ContinuityPolicy,
    authority_surface: Vec<u8>,
}

impl fmt::Debug for StorageGrantRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StorageGrantRequest([REDACTED])")
    }
}

impl StorageGrantRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        invocation: InvocationId,
        capability: CapabilityId,
        provider: ProviderId,
        interface: StorageInterface,
        access: StorageAccess,
        namespace: StorageNamespace,
        agent: AgentId,
        subject: ExternalSubject,
        transport_kind: impl Into<String>,
        transport: impl Into<String>,
        channel: impl Into<String>,
        conversation: impl Into<String>,
        continuity_policy: ContinuityPolicy,
        authority_surface: Vec<u8>,
    ) -> Self {
        Self {
            invocation,
            capability,
            provider,
            interface,
            access,
            namespace,
            agent,
            subject,
            transport_kind: transport_kind.into(),
            transport: transport.into(),
            channel: channel.into(),
            conversation: conversation.into(),
            continuity_policy,
            authority_surface,
        }
    }

    pub(crate) fn scope_values(&self) -> [String; 7] {
        [
            self.provider.to_string(),
            self.agent.to_string(),
            self.subject.canonical(),
            self.transport_kind.clone(),
            self.transport.clone(),
            self.channel.clone(),
            self.conversation.clone(),
        ]
    }
    pub(crate) fn continuity_policy(&self) -> ContinuityPolicy {
        self.continuity_policy
    }
    pub(crate) fn authority_surface(&self) -> &[u8] {
        &self.authority_surface
    }
}

/// Non-mutating, single-use preparation for one invocation-bound storage grant.
///
/// Preparation derives only keyed opaque values. It performs no filesystem operation and reserves
/// no quota, so dropping it after an authorization-audit failure leaves the storage tree exactly
/// unchanged. [`materialize`](Self::materialize) is the explicit mutation boundary.
pub struct StorageGrantPreparation {
    host: StorageHost,
    request: StorageGrantRequest,
    base_token: String,
    scope_commitment: StorageScopeCommitment,
    record_key: [u8; 32],
}

impl fmt::Debug for StorageGrantPreparation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StorageGrantPreparation([REDACTED])")
    }
}

impl StorageGrantPreparation {
    #[must_use]
    pub fn scope_commitment(&self) -> StorageScopeCommitment {
        self.scope_commitment.clone()
    }

    /// Derives one stable namespace-keyed record identifier without touching the namespace tree.
    #[must_use]
    pub fn record_id(&self, delivery: &[u8]) -> String {
        StorageKey::from_bytes(self.record_key).commitment(DOMAIN_RECORD_ID, &[delivery])
    }

    /// Derives keyed low-entropy decision evidence without materializing storage authority.
    #[must_use]
    pub fn evidence_commitment(&self, label: &str, bytes: &[u8]) -> String {
        self.host.inner.key.commitment(
            DOMAIN_DECISION_EVIDENCE,
            &[self.base_token.as_bytes(), label.as_bytes(), bytes],
        )
    }

    /// Derives the provider's content/dedup commitment before filesystem mutation.
    #[must_use]
    pub fn content_commitment(&self, user: &str, assistant: &str) -> String {
        self.host.inner.key.commitment(
            DOMAIN_CONTENT,
            &[
                self.base_token.as_bytes(),
                user.as_bytes(),
                assistant.as_bytes(),
            ],
        )
    }

    /// Crosses the explicit filesystem mutation boundary after durable authorization audit.
    pub fn materialize(self) -> Result<StorageGrant, StorageHostError> {
        self.host.materialize_grant(self.request, self.base_token)
    }
}

/// Single-use invocation-bound storage authority.
pub struct StorageGrant {
    host_id: [u8; 32],
    invocation: InvocationId,
    capability: CapabilityId,
    provider: ProviderId,
    interface: StorageInterface,
    access: StorageAccess,
    namespace_kind: StorageNamespace,
    namespace: Namespace,
    limits: StorageLimits,
    key: Arc<StorageKey>,
}

impl fmt::Debug for StorageGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StorageGrant([REDACTED])")
    }
}

impl StorageGrant {
    #[must_use]
    pub fn invocation(&self) -> &InvocationId {
        &self.invocation
    }
    #[must_use]
    pub fn capability(&self) -> &CapabilityId {
        &self.capability
    }
    #[must_use]
    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }
    #[must_use]
    pub const fn interface(&self) -> StorageInterface {
        self.interface
    }
    #[must_use]
    pub const fn access(&self) -> StorageAccess {
        self.access
    }
    #[must_use]
    pub const fn namespace(&self) -> StorageNamespace {
        self.namespace_kind
    }
    #[must_use]
    pub fn scope_commitment(&self) -> StorageScopeCommitment {
        StorageScopeCommitment(self.namespace.scope_commitment.clone())
    }
    /// Derives one stable namespace-keyed record identifier from a bounded delivery identity.
    #[must_use]
    pub fn record_id(&self, delivery: &[u8]) -> String {
        let record_key = self.key.bytes(
            DOMAIN_RECORD_ID,
            &[self.namespace.base_token.as_bytes(), b"record-key-v1"],
        );
        StorageKey::from_bytes(record_key).commitment(DOMAIN_RECORD_ID, &[delivery])
    }
    /// Derives keyed low-entropy evidence distinct from every path and content domain.
    #[must_use]
    pub fn evidence_commitment(&self, label: &str, bytes: &[u8]) -> String {
        self.key.commitment(
            key::DOMAIN_DECISION_EVIDENCE,
            &[
                self.namespace.base_token.as_bytes(),
                label.as_bytes(),
                bytes,
            ],
        )
    }

    /// Derives a content/dedup commitment distinct from paths, record IDs, audit, and evidence.
    #[must_use]
    pub fn content_commitment(&self, user: &str, assistant: &str) -> String {
        self.key.commitment(
            DOMAIN_CONTENT,
            &[
                self.namespace.base_token.as_bytes(),
                user.as_bytes(),
                assistant.as_bytes(),
            ],
        )
    }
}

#[derive(Debug)]
struct HostInner {
    id: [u8; 32],
    layout: Layout,
    key: Arc<StorageKey>,
    ledger: Arc<QuotaLedger>,
    limits: StorageLimits,
    namespace_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    /// Serializes physical namespace-slot observations with base removal, but never lease waits.
    namespace_observation_lock: Mutex<()>,
}

/// Wasmtime-independent secure native storage engine.
#[derive(Clone)]
pub struct StorageHost {
    inner: Arc<HostInner>,
}

impl fmt::Debug for StorageHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StorageHost([REDACTED])")
    }
}

impl StorageHost {
    /// Opens, locks, and accounts one broker-owned root.
    ///
    /// Only the root itself is validated here. Namespaces are checked when a grant opens them, so
    /// a corrupt conversation fails its own invocation rather than the broker's startup.
    pub fn open(
        root: impl AsRef<Path>,
        namespace_key_path: impl AsRef<Path>,
        limits: StorageLimits,
    ) -> Result<Self, StorageHostError> {
        limits.validate()?;
        let root = resolve_storage_root_path(root.as_ref())?;
        let namespace_key_path = resolve_namespace_key_path(namespace_key_path.as_ref())?;
        if namespace_key_path == root || namespace_key_path.starts_with(&root) {
            return Err(StorageHostError::UnsafeKeyFile {
                path: namespace_key_path,
            });
        }
        let key = Arc::new(StorageKey::load(&namespace_key_path)?);
        let minimum = Layout::minimum_usage(&root, &key)?;
        if minimum.bytes > limits.max_root_bytes || minimum.entries > limits.startup_max_entries {
            return Err(StorageHostError::QuotaExceeded);
        }
        let layout = Layout::open(&root, &key)?;
        // The one startup walk. It charges the quota ledger and nothing else: a namespace that
        // will not scan is logged and left uncharged, and its own next grant is where it fails.
        let usage = scan_root_usage(&layout, limits.startup_max_entries)?;
        if usage.bytes > limits.max_root_bytes {
            return Err(StorageHostError::QuotaExceeded);
        }
        let ledger = QuotaLedger::new(limits.clone(), usage);
        #[allow(
            clippy::map_err_ignore,
            reason = "the discarded value is the rejected raw-entropy Vec<u8> itself, whose only \
                      reportable property is the length EntropyLength already names"
        )]
        let id: [u8; 32] = random_bytes(32)?
            .try_into()
            .map_err(|_| StorageHostError::EntropyLength)?;
        Ok(Self {
            inner: Arc::new(HostInner {
                id,
                layout,
                key,
                ledger,
                limits,
                namespace_locks: Mutex::new(BTreeMap::new()),
                namespace_observation_lock: Mutex::new(()),
            }),
        })
    }

    /// Produces deployment-keyed evidence without deriving or creating a namespace.
    ///
    /// Used for denied storage proposals, which must not create storage merely to avoid an
    /// unkeyed low-entropy digest.
    #[must_use]
    pub fn evidence_commitment(&self, label: &str, bytes: &[u8]) -> String {
        self.inner.key.commitment(
            key::DOMAIN_DECISION_EVIDENCE,
            &[b"denied-storage", label.as_bytes(), bytes],
        )
    }

    /// Prepares one grant without reading or mutating the filesystem or reserving quota.
    pub fn prepare_grant(
        &self,
        request: StorageGrantRequest,
    ) -> Result<StorageGrantPreparation, StorageHostError> {
        if request.namespace != StorageNamespace::Chat {
            return Err(StorageHostError::PermissionDenied);
        }
        let values = request.scope_values();
        let fields = values.iter().map(String::as_bytes).collect::<Vec<_>>();
        let base_token = self.inner.key.token(DOMAIN_NAMESPACE_PATH, &fields);
        Ok(StorageGrantPreparation {
            host: self.clone(),
            request,
            scope_commitment: StorageScopeCommitment(
                self.inner.key.commitment(DOMAIN_AUDIT_SCOPE, &fields),
            ),
            record_key: self
                .inner
                .key
                .bytes(DOMAIN_RECORD_ID, &[base_token.as_bytes(), b"record-key-v1"]),
            base_token,
        })
    }

    /// Convenience path for trusted callers that have already durably audited authorization.
    pub fn grant(&self, request: StorageGrantRequest) -> Result<StorageGrant, StorageHostError> {
        self.prepare_grant(request)?.materialize()
    }

    fn materialize_grant(
        &self,
        request: StorageGrantRequest,
        base: String,
    ) -> Result<StorageGrant, StorageHostError> {
        debug_assert_eq!(
            base,
            self.inner.key.token(
                DOMAIN_NAMESPACE_PATH,
                &request
                    .scope_values()
                    .iter()
                    .map(String::as_bytes)
                    .collect::<Vec<_>>()
            )
        );
        let namespace_lock = namespace_lock(&self.inner.namespace_locks, &base);
        let _namespace = namespace_lock
            .lock()
            .expect("storage namespace housekeeping lock");
        let mut namespace_reservation = Some({
            // The observation lock is dropped before any base lease wait, preserving concurrency between
            // distinct namespaces.
            let _observation = self
                .inner
                .namespace_observation_lock
                .lock()
                .expect("storage namespace observation lock");
            let observed_namespaces = self
                .inner
                .layout
                .namespaces()
                .entries_bounded(self.inner.limits.startup_max_entries)?
                .into_iter()
                .collect::<BTreeSet<_>>();
            self.inner
                .ledger
                .reserve_namespace(base.clone(), observed_namespaces)?
        });
        // The ledger is rebuilt once at startup and every host mutation reconciles or retains its
        // reservation. Rescanning here would be unsafe: a scan can start before another namespace
        // commits and publish its stale lower total after that commit releases its reservation.
        let mut plan = NamespacePlan::prepare(
            self.inner.layout.namespaces(),
            &self.inner.key,
            &request,
            self.inner.limits.lock_timeout_ms,
            self.inner.limits.startup_max_entries,
        )?;
        let reset = plan.take_reset();
        if plan.maximum_generation_peak_bytes() > self.inner.limits.max_namespace_bytes {
            return Err(StorageHostError::QuotaExceeded);
        }
        let before_namespace = plan.before_usage();
        let housekeeping_reservation = self
            .inner
            .ledger
            .reserve_root(plan.reserved_bytes(), plan.reserved_entries())?;
        // Reconcile physical existence and keep slot publication under the same observation lock
        // as the first namespace mutation. `prepare` has already completed every lease wait, so
        // this short critical section never serializes an unrelated namespace behind a blocked
        // base lease.
        let _observation = self
            .inner
            .namespace_observation_lock
            .lock()
            .expect("storage namespace observation lock");
        self.inner.ledger.observe_namespaces(
            self.inner
                .layout
                .namespaces()
                .entries_bounded(self.inner.limits.startup_max_entries)?,
        );
        let namespace = match plan.apply(
            self.inner.layout.namespaces(),
            self.inner.limits.lock_timeout_ms,
        ) {
            Ok(namespace) => namespace,
            Err(error) => {
                // `apply` may have completed mkdir/rename before a later open or sync failed. A
                // physically present base continues to own its slot.
                if self.inner.layout.namespaces().exists(&base).unwrap_or(true) {
                    namespace_reservation
                        .take()
                        .expect("namespace reservation")
                        .commit();
                }
                housekeeping_reservation.retain();
                return Err(error);
            }
        };
        let base_directory = match self.inner.layout.namespaces().open_directory(&base) {
            Ok(directory) => directory,
            Err(error) => {
                namespace_reservation
                    .take()
                    .expect("namespace reservation")
                    .commit();
                housekeeping_reservation.retain();
                return Err(error);
            }
        };
        let scanned = match scan_usage(&base_directory, self.inner.limits.startup_max_entries) {
            Ok(usage) => usage,
            Err(error) => {
                namespace_reservation
                    .take()
                    .expect("namespace reservation")
                    .commit();
                housekeeping_reservation.retain();
                return Err(error);
            }
        };
        let after_namespace = match usage_with_directory_entry(scanned) {
            Ok(usage) => usage,
            Err(error) => {
                namespace_reservation
                    .take()
                    .expect("namespace reservation")
                    .commit();
                housekeeping_reservation.retain();
                return Err(error);
            }
        };
        if let Err(error) = housekeeping_reservation.commit(before_namespace, after_namespace) {
            namespace_reservation
                .take()
                .expect("namespace reservation")
                .commit();
            return Err(error);
        }
        namespace_reservation
            .take()
            .expect("namespace reservation")
            .commit();
        if let Some(cause) = reset {
            // The fresh generation is on disk and accounted. This invocation still fails, so the
            // model is told once that what it stored is gone rather than finding it empty.
            return Err(report_namespace_reset(cause, &namespace));
        }
        Ok(StorageGrant {
            host_id: self.inner.id,
            invocation: request.invocation,
            capability: request.capability,
            provider: request.provider,
            interface: request.interface,
            access: request.access,
            namespace_kind: request.namespace,
            namespace,
            limits: self.inner.limits.clone(),
            key: Arc::clone(&self.inner.key),
        })
    }

    /// Consumes and validates one grant, acquiring its namespace lease for the invocation lifetime.
    pub fn begin(&self, grant: StorageGrant) -> Result<StorageHandle, StorageHostError> {
        if grant.host_id != self.inner.id {
            return Err(StorageHostError::GrantHostMismatch);
        }
        StorageHandle::begin(grant, Arc::clone(&self.inner.ledger))
    }

    #[must_use]
    pub fn limits(&self) -> &StorageLimits {
        &self.inner.limits
    }
}

fn namespace_lock(
    registry: &Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    base: &str,
) -> Arc<Mutex<()>> {
    let mut locks = registry.lock().expect("storage namespace lock registry");
    locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    Arc::clone(
        locks
            .entry(base.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

/// Resolves a configured storage root without following any original ancestor symlink.
pub fn resolve_storage_root_path(path: &Path) -> Result<PathBuf, StorageHostError> {
    resolve_parent_leaf(path, false)
}

/// Resolves a configured namespace-key path without following any original ancestor symlink.
pub fn resolve_namespace_key_path(path: &Path) -> Result<PathBuf, StorageHostError> {
    resolve_parent_leaf(path, true)
}

fn resolve_parent_leaf(path: &Path, key: bool) -> Result<PathBuf, StorageHostError> {
    let io_error = |path: &Path, source: std::io::Error| {
        if key {
            StorageHostError::KeyIo {
                path: path.to_path_buf(),
                source,
            }
        } else {
            StorageHostError::RootIo {
                path: path.to_path_buf(),
                source,
            }
        }
    };
    let unsafe_path = |path: &Path| {
        if key {
            StorageHostError::UnsafeKeyFile {
                path: path.to_path_buf(),
            }
        } else {
            StorageHostError::UnsafeRoot {
                path: path.to_path_buf(),
            }
        }
    };
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| io_error(path, source))?
            .join(path)
    };
    let mut components = Vec::new();
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(component) => components.push(component.to_os_string()),
            // Do not normalize a parent component away: every component in the configured spelling
            // must be traversed under a retained no-follow directory descriptor.
            Component::ParentDir | Component::Prefix(_) => return Err(unsafe_path(&absolute)),
        }
    }
    if components.is_empty() {
        return Err(unsafe_path(&absolute));
    }

    let root_fd = rustix::fs::open(
        "/",
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|source| io_error(Path::new("/"), std::io::Error::from(source)))?;
    let mut directory = File::from(root_fd);
    let mut traversed = PathBuf::from("/");
    for component in &components[..components.len() - 1] {
        traversed.push(component);
        let fd = rustix::fs::openat(
            &directory,
            component,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|source| io_error(&traversed, std::io::Error::from(source)))?;
        directory = File::from(fd);
    }
    traversed.push(components.last().expect("non-empty path components"));
    Ok(traversed)
}

/// Logs one namespace reset and returns the failure that tells the caller it happened.
///
/// Emitted inside the caller's span, so the record carries the trace of the invocation that found
/// the corruption. The previous generation stays on disk under its token, uncollected, exactly as
/// an authority rotation leaves one.
fn report_namespace_reset(reset: Reset, fresh: &Namespace) -> StorageHostError {
    tracing::error!(
        event = "storage_namespace_reset",
        category = "storage",
        storage.namespace = %fresh.base_token,
        storage.generation = %fresh.generation_token,
        storage.previous_generation = reset.previous_generation.as_deref(),
        storage.check = reset.check,
        storage.path = reset
            .path
            .as_deref()
            .map(|path| tracing::field::display(path.display())),
        "storage namespace was corrupt; it now opens a fresh generation and this invocation failed"
    );
    StorageHostError::Corrupt {
        scope: reset.check,
        namespace: Some(fresh.base_token.clone()),
        generation: reset.previous_generation,
        path: reset.path,
        reset: Some(fresh.generation_token.clone()),
    }
}

/// Reports why one retained document did not decode without echoing the rejected bytes.
///
/// The corruption error names the check and the file; the discarded `serde_json` failure is the
/// only description of what is actually wrong inside it. Class, line, and column are its complete
/// content-free part: they separate a truncated write from an unknown or wrongly typed field
/// without exporting any document content.
pub(crate) fn report_decode_failure(document: &'static str, error: &serde_json::Error) {
    tracing::warn!(
        event = "storage_document_decode_failed",
        category = "storage",
        storage.document = document,
        decode.class = ?error.classify(),
        decode.line = error.line(),
        decode.column = error.column(),
        "retained storage document did not decode"
    );
}

/// The coarse class a storage failure is reported under.
///
/// Content-free classification for guest-visible refusal reasons and operator diagnostics.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum StorageFailureClass {
    /// A quota or an accounting overflow refused the write.
    Quota,
    /// The operation ran out of its budget.
    Timeout,
    /// Retained data, layout, or the namespace key disagreed with itself.
    Corrupt,
    /// The grant or the filesystem refused the access.
    Denied,
    /// Everything else, filesystem input/output included.
    Io,
}

impl StorageFailureClass {
    /// The stable label. It is the vocabulary a guest-visible violation reason is drawn from, so
    /// these strings are a contract rather than log text.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Quota => "quota",
            Self::Timeout => "timeout",
            Self::Corrupt => "corrupt",
            Self::Denied => "denied",
            Self::Io => "io",
        }
    }
}

impl std::fmt::Display for StorageFailureClass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Stable native storage failure classes.
///
/// No variant carries guest content. Paths and opaque tokens name on-disk entries for the operator
/// reading a log line; a guest sees only the WIT error enum and a model only the broker's fixed
/// public code, so nothing here renders to either.
#[derive(Debug, Error)]
pub enum StorageHostError {
    #[error(transparent)]
    Configuration(#[from] StorageConfigError),
    #[error("storage root input/output failed at {}", path.display())]
    RootIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("storage namespace-key input/output failed")]
    KeyIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("storage namespace-key file is unsafe")]
    UnsafeKeyFile { path: PathBuf },
    #[error("storage namespace-key document is invalid")]
    InvalidKeyFile,
    #[error("storage root or ancestor is unsafe: {}", path.display())]
    UnsafeRoot { path: PathBuf },
    #[error("another conforming storage writer holds the root")]
    SecondWriter,
    #[error("storage layout is corrupt: {}", path.display())]
    CorruptLayout { path: PathBuf },
    #[error("storage key does not match retained data")]
    KeyMismatch,
    /// Retained namespace state failed one check.
    ///
    /// Everything but `scope` is filled in where the detecting code knows it.
    #[error(
        "storage corruption detected: {scope}{}",
        CorruptionSite::new(.namespace, .generation, .path, .reset)
    )]
    Corrupt {
        /// Compile-time literal naming the check that failed, never retained content.
        scope: &'static str,
        /// Base token of the namespace directory the check was about.
        namespace: Option<String>,
        /// Generation token the check was about.
        generation: Option<String>,
        /// The entry that failed the check.
        path: Option<PathBuf>,
        /// The fresh generation the namespace was rotated to before this failure was returned.
        reset: Option<String>,
    },
    #[error("storage quota exceeded")]
    QuotaExceeded,
    #[error("storage resource is busy")]
    Busy,
    #[error("storage operation timed out")]
    Timeout,
    #[error("storage permission denied")]
    PermissionDenied,
    #[error("storage logical name is invalid")]
    InvalidName,
    #[error("storage argument is invalid")]
    InvalidArgument,
    #[error("storage object was not found")]
    NotFound,
    #[error("storage object already exists")]
    AlreadyExists,
    #[error("storage operation is unsupported")]
    Unsupported,
    #[error("storage input/output failed")]
    Io,
    #[error("storage arithmetic overflowed")]
    Arithmetic,
    #[error("storage entropy failed")]
    Entropy {
        #[source]
        source: std::io::Error,
    },
    #[error("storage entropy returned an invalid length")]
    EntropyLength,
    #[error("storage clock failed")]
    Clock,
    #[error("storage startup scan saw {count} entries, above {maximum}")]
    StartupEntryLimit { count: u64, maximum: u64 },
    #[error("storage grant belongs to another host instance")]
    GrantHostMismatch,
}

/// The optional half of a [`StorageHostError::Corrupt`] message.
struct CorruptionSite<'a> {
    namespace: &'a Option<String>,
    generation: &'a Option<String>,
    path: &'a Option<PathBuf>,
    reset: &'a Option<String>,
}

impl<'a> CorruptionSite<'a> {
    const fn new(
        namespace: &'a Option<String>,
        generation: &'a Option<String>,
        path: &'a Option<PathBuf>,
        reset: &'a Option<String>,
    ) -> Self {
        Self {
            namespace,
            generation,
            path,
            reset,
        }
    }
}

impl fmt::Display for CorruptionSite<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(namespace) = self.namespace {
            write!(formatter, " in namespace {namespace}")?;
        }
        if let Some(generation) = self.generation {
            write!(formatter, " generation {generation}")?;
        }
        if let Some(path) = self.path {
            write!(formatter, " at {}", path.display())?;
        }
        if let Some(reset) = self.reset {
            write!(formatter, "; reset to generation {reset}")?;
        }
        Ok(())
    }
}

impl StorageHostError {
    /// A corruption naming only the check that failed.
    #[must_use]
    pub const fn corrupt(scope: &'static str) -> Self {
        Self::Corrupt {
            scope,
            namespace: None,
            generation: None,
            path: None,
            reset: None,
        }
    }

    /// Names the entry a corruption was found at, unless the detecting site already did.
    pub(crate) fn at(mut self, entry: PathBuf) -> Self {
        if let Self::Corrupt { path, .. } = &mut self {
            path.get_or_insert(entry);
        }
        self
    }

    /// Names the namespace, and the generation when known, that a corruption belongs to.
    pub(crate) fn in_namespace(mut self, base: &str, generation_token: Option<&str>) -> Self {
        if let Self::Corrupt {
            namespace,
            generation,
            ..
        } = &mut self
        {
            namespace.get_or_insert_with(|| base.to_owned());
            if let Some(token) = generation_token {
                generation.get_or_insert_with(|| token.to_owned());
            }
        }
        self
    }

    /// Whether this failure rotated its namespace to a fresh, empty generation before returning.
    ///
    /// When it did, the storage an immediate retry opens is already usable.
    #[must_use]
    pub const fn namespace_reset(&self) -> bool {
        matches!(self, Self::Corrupt { reset: Some(_), .. })
    }

    /// The coarse, content-free class this failure is reported under.
    #[must_use]
    pub const fn class(&self) -> StorageFailureClass {
        match self {
            Self::QuotaExceeded | Self::Arithmetic => StorageFailureClass::Quota,
            Self::Timeout => StorageFailureClass::Timeout,
            Self::Corrupt { .. } | Self::CorruptLayout { .. } | Self::KeyMismatch => {
                StorageFailureClass::Corrupt
            }
            Self::PermissionDenied | Self::GrantHostMismatch => StorageFailureClass::Denied,
            // An unaudited outcome is unknown rather than any one class, so it reports the
            // catch-all here and names what actually broke in its own `cause` instead.
            _ => StorageFailureClass::Io,
        }
    }
}
