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
//! filesystem isolation, and invocation-transactional commit are the authority boundary.

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use dekopon_capability::{StorageAccess, StorageInterface, StorageNamespace};
use dekopon_core::{AgentId, CapabilityId, ExternalSubject, InvocationId, ProviderId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod config;
mod gc;
mod jsonl;
mod key;
mod layout;
mod metrics;
mod namespace;
mod quota;
mod transaction;
mod vfs;

pub use config::{StorageConfigError, StorageLimits};
pub use gc::GcReport;
pub use jsonl::JsonlChunk;
pub use transaction::StorageTransaction;
pub use vfs::{Durability, FileStat, LockLevel, OpenOptions};

use key::{DOMAIN_CONTENT, DOMAIN_NAMESPACE_PATH, DOMAIN_RECORD_ID, StorageKey, random_bytes};
use layout::{ENTRY_CHARGE, Layout, scan_root_usage, scan_usage};
use namespace::{Namespace, NamespacePlan};
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
        let key = StorageKey::from_bytes(self.namespace.record_key);
        key.commitment(DOMAIN_RECORD_ID, &[delivery])
    }
    /// Derives keyed low-entropy evidence distinct from every path and content domain.
    #[must_use]
    pub fn evidence_commitment(&self, label: &str, bytes: &[u8]) -> String {
        self.key.commitment(
            key::DOMAIN_DECISION_EVIDENCE,
            &[
                self.namespace.base_token.as_bytes(),
                self.namespace.generation_token.as_bytes(),
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
                self.namespace.generation_token.as_bytes(),
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
    gc_lock: Mutex<()>,
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
    /// Opens, locks, validates, recovers, and accounts one broker-owned root.
    pub fn open(
        root: impl AsRef<Path>,
        namespace_key_path: impl AsRef<Path>,
        limits: StorageLimits,
    ) -> Result<Self, StorageHostError> {
        limits.validate()?;
        let root = canonical_parent_leaf(root.as_ref(), false)?;
        let namespace_key_path = canonical_parent_leaf(namespace_key_path.as_ref(), true)?;
        if namespace_key_path == root || namespace_key_path.starts_with(&root) {
            return Err(StorageHostError::UnsafeKeyFile {
                path: namespace_key_path,
            });
        }
        let key = Arc::new(StorageKey::load(&namespace_key_path)?);
        let layout = Layout::open(&root, &key)?;
        quarantine_isolated_namespaces(&layout, &key, &limits)?;
        transaction::recover_transactions(&layout, &key, &limits)?;
        let quarantined = layout.quarantine().entries()?.len() as u64;
        if quarantined > limits.max_quarantined_namespaces {
            return Err(StorageHostError::Corrupt {
                scope: "quarantine-capacity",
            });
        }
        let usage = scan_root_usage(&layout, limits.startup_max_entries)?;
        if usage.bytes > limits.max_root_bytes {
            return Err(StorageHostError::QuotaExceeded);
        }
        let ledger = QuotaLedger::new(limits.clone(), usage);
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
                gc_lock: Mutex::new(()),
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

    /// Mints one non-cloneable grant after the broker has authorized a validated chat operation.
    pub fn grant(&self, request: StorageGrantRequest) -> Result<StorageGrant, StorageHostError> {
        if request.namespace != StorageNamespace::Chat {
            return Err(StorageHostError::PermissionDenied);
        }
        let values = request.scope_values();
        let fields = values.iter().map(String::as_bytes).collect::<Vec<_>>();
        let base = self.inner.key.token(DOMAIN_NAMESPACE_PATH, &fields);
        let namespace_lock = namespace_lock(&self.inner.namespace_locks, &base);
        let _namespace = namespace_lock
            .lock()
            .expect("storage namespace housekeeping lock");
        if self.inner.layout.quarantine().exists(&base)? {
            return Err(StorageHostError::Corrupt {
                scope: "quarantined-namespace",
            });
        }
        let namespace_reservation = {
            // A GC base removal takes the same short lock around rename + slot release. The lock
            // is deliberately dropped before any base lease wait, preserving concurrency between
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
                .entries()?
                .into_iter()
                .chain(self.inner.layout.quarantine().entries()?)
                .collect::<BTreeSet<_>>();
            self.inner
                .ledger
                .reserve_namespace(base.clone(), observed_namespaces)?
        };
        let root_before =
            scan_root_usage(&self.inner.layout, self.inner.limits.startup_max_entries)?;
        self.inner.ledger.refresh_root(root_before);
        let plan = NamespacePlan::prepare(
            self.inner.layout.namespaces(),
            &self.inner.key,
            &request,
            self.inner.limits.lock_timeout_ms,
            self.inner.limits.startup_max_entries,
        )?;
        if plan.maximum_generation_peak_bytes() > self.inner.limits.max_namespace_bytes {
            return Err(StorageHostError::QuotaExceeded);
        }
        let before_namespace = plan.before_usage();
        let housekeeping_reservation = self
            .inner
            .ledger
            .reserve_root(plan.reserved_bytes(), plan.reserved_entries())?;
        let namespace = plan.apply(
            self.inner.layout.namespaces(),
            &self.inner.key,
            self.inner.limits.lock_timeout_ms,
        )?;
        let base_directory = self.inner.layout.namespaces().open_directory(&base)?;
        let mut after_namespace =
            scan_usage(&base_directory, self.inner.limits.startup_max_entries)?;
        after_namespace.entries = after_namespace.entries.saturating_add(1);
        after_namespace.bytes = after_namespace.bytes.saturating_add(ENTRY_CHARGE);
        housekeeping_reservation.commit(before_namespace, after_namespace)?;
        namespace_reservation.commit();
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

    /// Consumes and validates one grant, acquiring its namespace lease for the transaction lifetime.
    pub fn begin(&self, grant: StorageGrant) -> Result<StorageTransaction, StorageHostError> {
        if grant.host_id != self.inner.id {
            return Err(StorageHostError::GrantHostMismatch);
        }
        StorageTransaction::begin(
            grant,
            Arc::clone(&self.inner.ledger),
            self.inner.layout.namespaces().clone(),
            self.inner.layout.transactions().clone(),
            self.inner.layout.trash().clone(),
        )
    }

    /// Runs one bounded lifecycle pass. Active namespace leases are skipped.
    pub fn gc_once(&self) -> Result<GcReport, StorageHostError> {
        let _gc = self.inner.gc_lock.lock().expect("storage GC lock");
        gc::run(
            &self.inner.layout,
            &self.inner.key,
            &self.inner.limits,
            &self.inner.namespace_locks,
            &self.inner.namespace_observation_lock,
            &self.inner.ledger,
        )
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

fn canonical_parent_leaf(path: &Path, key: bool) -> Result<PathBuf, StorageHostError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| StorageHostError::RootIo {
                path: path.to_path_buf(),
                source,
            })?
            .join(path)
    };
    let parent = absolute
        .parent()
        .ok_or_else(|| StorageHostError::UnsafeRoot {
            path: absolute.clone(),
        })?;
    let name = absolute
        .file_name()
        .ok_or_else(|| StorageHostError::UnsafeRoot {
            path: absolute.clone(),
        })?;
    let parent = std::fs::canonicalize(parent).map_err(|source| {
        if key {
            StorageHostError::KeyIo {
                path: parent.to_path_buf(),
                source,
            }
        } else {
            StorageHostError::RootIo {
                path: parent.to_path_buf(),
                source,
            }
        }
    })?;
    Ok(parent.join(name))
}

fn quarantine_isolated_namespaces(
    layout: &Layout,
    key: &StorageKey,
    limits: &StorageLimits,
) -> Result<(), StorageHostError> {
    let mut quarantined = layout.quarantine().entries()?.len() as u64;
    for base in layout.namespaces().entries()? {
        let validation = (|| {
            let metadata =
                layout
                    .namespaces()
                    .metadata(&base)?
                    .ok_or(StorageHostError::Corrupt {
                        scope: "namespace-entry",
                    })?;
            if metadata.kind != layout::EntryKind::Directory {
                return Err(StorageHostError::Corrupt {
                    scope: "namespace-entry",
                });
            }
            let directory = layout.namespaces().open_directory(&base)?;
            scan_usage(&directory, limits.startup_max_entries)?;
            namespace::validate_namespace_base(&directory, key, &base)
        })();
        match validation {
            Ok(()) => {}
            Err(StorageHostError::Corrupt { .. }) | Err(StorageHostError::UnsafeRoot { .. }) => {
                if quarantined >= limits.max_quarantined_namespaces {
                    return Err(StorageHostError::Corrupt {
                        scope: "quarantine-capacity",
                    });
                }
                if layout.quarantine().exists(&base)? {
                    return Err(StorageHostError::Corrupt {
                        scope: "quarantine-collision",
                    });
                }
                layout
                    .namespaces()
                    .rename_to(&base, layout.quarantine(), &base)?;
                layout.namespaces().sync()?;
                layout.quarantine().sync()?;
                quarantined = quarantined.saturating_add(1);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Stable native storage failure classes. No variant contains guest names, paths, or content.
#[derive(Debug, Error)]
pub enum StorageHostError {
    #[error(transparent)]
    Configuration(#[from] StorageConfigError),
    #[error("storage root input/output failed")]
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
    #[error("storage root or ancestor is unsafe")]
    UnsafeRoot { path: PathBuf },
    #[error("another conforming storage writer holds the root")]
    SecondWriter,
    #[error("storage layout is corrupt")]
    CorruptLayout,
    #[error("storage key does not match retained data")]
    KeyMismatch,
    #[error("storage corruption detected")]
    Corrupt { scope: &'static str },
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
    #[error("storage startup scan saw too many transactions")]
    StartupTransactionLimit,
    #[error("storage grant belongs to another host instance")]
    GrantHostMismatch,
    #[error("storage committed durably but finalization or audit evidence failed")]
    OutcomeUnaudited,
}
