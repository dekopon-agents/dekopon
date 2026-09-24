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
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::File,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, TryLockError},
    time::{Duration, Instant},
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
mod namespace;
mod quota;
mod vfs;

pub use config::{StorageConfigError, StorageLimits};
pub use handle::StorageHandle;
pub use jsonl::JsonlChunk;
pub use vfs::{Durability, FileStat, LockLevel, OpenOptions};

use key::{
    DOMAIN_AUDIT_SCOPE, DOMAIN_CONTENT, DOMAIN_DECISION_EVIDENCE, DOMAIN_NAMESPACE_PATH,
    DOMAIN_RECORD_ID, commitment, random_bytes, token,
};
use layout::{Layout, scan_root_usage, scan_usage, usage_with_directory_entry};
use namespace::{Namespace, NamespacePlan, Reset, deadline_after};
use quota::QuotaLedger;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContinuityPolicy {
    Stable,
    #[default]
    AuthorityBound,
}

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

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StorageEvidence {
    pub operations: u64,
    pub syncs: u64,
    pub quota_denials: u64,
    /// Exact bytes charged against this invocation's read budget: each read counts the length it
    /// asked for, whatever it returned.
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub evidence_commitment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_commitment: Option<String>,
}

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

pub struct StorageGrantPreparation {
    host: StorageHost,
    request: StorageGrantRequest,
    base_token: String,
    scope_commitment: StorageScopeCommitment,
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

    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.base_token
    }

    #[must_use]
    pub fn record_id(&self, delivery: &[u8]) -> String {
        record_id(&self.base_token, delivery)
    }

    #[must_use]
    pub fn evidence_commitment(&self, label: &str, bytes: &[u8]) -> String {
        commitment(
            DOMAIN_DECISION_EVIDENCE,
            &[self.base_token.as_bytes(), label.as_bytes(), bytes],
        )
    }

    #[must_use]
    pub fn content_commitment(&self, user: &str, assistant: &str) -> String {
        content_commitment(&self.base_token, user, assistant)
    }

    pub fn materialize(self) -> Result<StorageGrant, StorageHostError> {
        self.host.materialize_grant(self.request, self.base_token)
    }
}

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
    #[must_use]
    pub fn record_id(&self, delivery: &[u8]) -> String {
        record_id(&self.namespace.base_token, delivery)
    }
    #[must_use]
    pub fn evidence_commitment(&self, label: &str, bytes: &[u8]) -> String {
        commitment(
            DOMAIN_DECISION_EVIDENCE,
            &[
                self.namespace.base_token.as_bytes(),
                label.as_bytes(),
                bytes,
            ],
        )
    }

    #[must_use]
    pub fn content_commitment(&self, user: &str, assistant: &str) -> String {
        content_commitment(&self.namespace.base_token, user, assistant)
    }
}

fn record_id(base_token: &str, delivery: &[u8]) -> String {
    commitment(DOMAIN_RECORD_ID, &[base_token.as_bytes(), delivery])
}

fn content_commitment(base_token: &str, user: &str, assistant: &str) -> String {
    commitment(
        DOMAIN_CONTENT,
        &[base_token.as_bytes(), user.as_bytes(), assistant.as_bytes()],
    )
}

#[derive(Debug)]
struct HostInner {
    id: [u8; 32],
    layout: Layout,
    ledger: Arc<QuotaLedger>,
    limits: StorageLimits,
    namespace_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    namespace_observation_lock: Mutex<()>,
}

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
    pub fn open(root: impl AsRef<Path>, limits: StorageLimits) -> Result<Self, StorageHostError> {
        limits.validate()?;
        let root = resolve_storage_root_path(root.as_ref())?;
        let minimum = Layout::minimum_usage(&root)?;
        if minimum.bytes > limits.max_root_bytes || minimum.entries > limits.startup_max_entries {
            return Err(StorageHostError::QuotaExceeded);
        }
        let layout = Layout::open(&root)?;
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
                ledger,
                limits,
                namespace_locks: Mutex::new(BTreeMap::new()),
                namespace_observation_lock: Mutex::new(()),
            }),
        })
    }

    #[must_use]
    pub fn evidence_commitment(&self, label: &str, bytes: &[u8]) -> String {
        commitment(
            DOMAIN_DECISION_EVIDENCE,
            &[b"denied-storage", label.as_bytes(), bytes],
        )
    }

    pub fn prepare_grant(
        &self,
        request: StorageGrantRequest,
    ) -> Result<StorageGrantPreparation, StorageHostError> {
        if request.namespace != StorageNamespace::Chat {
            return Err(StorageHostError::PermissionDenied);
        }
        let values = request.scope_values();
        let fields = values.iter().map(String::as_bytes).collect::<Vec<_>>();
        Ok(StorageGrantPreparation {
            host: self.clone(),
            base_token: token(DOMAIN_NAMESPACE_PATH, &fields),
            scope_commitment: StorageScopeCommitment(commitment(DOMAIN_AUDIT_SCOPE, &fields)),
            request,
        })
    }

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
            token(
                DOMAIN_NAMESPACE_PATH,
                &request
                    .scope_values()
                    .iter()
                    .map(String::as_bytes)
                    .collect::<Vec<_>>()
            )
        );
        // One deadline covers all of a grant's waits; per-wait timeouts would let the k-th
        // contender behind the lease wait k times as long.
        let deadline = deadline_after(self.inner.limits.lock_timeout_ms)?;
        let namespace_lock = namespace_lock(&self.inner.namespace_locks, &base);
        let _namespace = lock_before(&namespace_lock, deadline)?;
        let mut namespace_reservation = Some({
            // The observation lock is dropped before any base lease wait, preserving concurrency
            // between distinct namespaces.
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
        // Rescanning here would race another namespace's commit, publishing a stale lower total
        // after that commit releases its reservation.
        let mut plan = NamespacePlan::prepare(
            self.inner.layout.namespaces(),
            &request,
            deadline,
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
        // This critical section must stay free of lease waits, or it would serialize an unrelated
        // namespace behind a blocked lease.
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
        let namespace = match plan.apply(self.inner.layout.namespaces(), deadline) {
            Ok(namespace) => namespace,
            Err(error) => {
                // A namespace mutation may partially complete before failing, so a physically
                // present base still owns its slot afterward.
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
        })
    }

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

fn lock_before(
    lock: &Mutex<()>,
    deadline: Instant,
) -> Result<MutexGuard<'_, ()>, StorageHostError> {
    loop {
        match lock.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::WouldBlock) => return Err(StorageHostError::Timeout),
            Err(TryLockError::Poisoned(_)) => {
                panic!("storage namespace housekeeping lock poisoned")
            }
        }
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

pub fn resolve_storage_root_path(path: &Path) -> Result<PathBuf, StorageHostError> {
    let io_error = |path: &Path, source: std::io::Error| StorageHostError::RootIo {
        path: path.to_path_buf(),
        source,
    };
    let unsafe_path = |path: &Path| StorageHostError::UnsafeRoot {
        path: path.to_path_buf(),
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
        site: Some(Box::new(CorruptionSite {
            namespace: Some(fresh.base_token.clone()),
            generation: reset.previous_generation,
            path: reset.path,
            reset: Some(fresh.generation_token.clone()),
        })),
    }
}

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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum StorageFailureClass {
    Quota,
    Timeout,
    Corrupt,
    Denied,
    Io,
}

impl StorageFailureClass {
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

/// No variant carries guest content; paths and tokens here are for operator logs only. A guest sees
/// just the WIT error enum, and a model only the broker's fixed public code.
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
    #[error("storage root or ancestor is unsafe: {}", path.display())]
    UnsafeRoot { path: PathBuf },
    #[error("another conforming storage writer holds the root")]
    SecondWriter,
    #[error("storage layout is corrupt: {}", path.display())]
    CorruptLayout { path: PathBuf },
    #[error("storage corruption detected: {scope}{}", SiteSuffix(.site))]
    Corrupt {
        scope: &'static str,
        site: Option<Box<CorruptionSite>>,
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CorruptionSite {
    pub namespace: Option<String>,
    pub generation: Option<String>,
    pub path: Option<PathBuf>,
    pub reset: Option<String>,
}

struct SiteSuffix<'a>(&'a Option<Box<CorruptionSite>>);

impl fmt::Display for SiteSuffix<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(site) = self.0 else {
            return Ok(());
        };
        if let Some(namespace) = &site.namespace {
            write!(formatter, " in namespace {namespace}")?;
        }
        if let Some(generation) = &site.generation {
            write!(formatter, " generation {generation}")?;
        }
        if let Some(path) = &site.path {
            write!(formatter, " at {}", path.display())?;
        }
        if let Some(reset) = &site.reset {
            write!(formatter, "; reset to generation {reset}")?;
        }
        Ok(())
    }
}

impl StorageHostError {
    #[must_use]
    pub const fn corrupt(scope: &'static str) -> Self {
        Self::Corrupt { scope, site: None }
    }

    fn site_mut(&mut self) -> Option<&mut CorruptionSite> {
        match self {
            Self::Corrupt { site, .. } => Some(site.get_or_insert_with(Box::default)),
            _ => None,
        }
    }

    pub(crate) fn at(mut self, entry: PathBuf) -> Self {
        if let Some(site) = self.site_mut() {
            site.path.get_or_insert(entry);
        }
        self
    }

    pub(crate) fn in_namespace(mut self, base: &str, generation_token: Option<&str>) -> Self {
        if let Some(site) = self.site_mut() {
            site.namespace.get_or_insert_with(|| base.to_owned());
            if let Some(token) = generation_token {
                site.generation.get_or_insert_with(|| token.to_owned());
            }
        }
        self
    }

    #[must_use]
    pub fn namespace_reset(&self) -> bool {
        matches!(self, Self::Corrupt { site: Some(site), .. } if site.reset.is_some())
    }

    #[must_use]
    pub const fn class(&self) -> StorageFailureClass {
        match self {
            Self::QuotaExceeded | Self::Arithmetic => StorageFailureClass::Quota,
            Self::Timeout => StorageFailureClass::Timeout,
            Self::Corrupt { .. } | Self::CorruptLayout { .. } => StorageFailureClass::Corrupt,
            Self::PermissionDenied | Self::GrantHostMismatch => StorageFailureClass::Denied,
            _ => StorageFailureClass::Io,
        }
    }
}
