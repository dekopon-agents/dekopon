//! Invocation overlay, durable commit point, and recognized crash recovery.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Write as _,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use dekopon_capability::{StorageAccess, StorageInterface};
use serde::{Deserialize, Serialize};

use crate::{
    StorageEvidence, StorageGrant, StorageHostError,
    key::{
        DOMAIN_CONTENT, DOMAIN_LOGICAL_PATH, DOMAIN_MANIFEST, DOMAIN_OPERATION_EVIDENCE,
        DOMAIN_OUTPUT_EVIDENCE, DOMAIN_TRANSACTION, StorageKey, random_bytes,
    },
    layout::{Directory, ENTRY_CHARGE, EntryKind, Layout, Usage, scan_usage, scan_usage_checked},
    metrics::byte_bucket,
    namespace::{Namespace, is_token, lock_exclusive},
    quota::{QuotaLedger, Reservation},
    vfs::LockLevel,
};

const MANIFEST_VERSION: &str = "dekopon.dev/storage-transaction/v1alpha1";
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum PostMarkerFault {
    MarkerFileSync,
    MarkerDirectorySync,
    Apply,
    Scan,
    Accounting,
    Evidence,
    Cleanup,
    TransactionSync,
    FinalizedCreate,
    FinalizedFileSync,
    FinalizedDirectorySync,
    PoisonCreate,
    PoisonFileSync,
    PoisonDirectorySync,
}

#[derive(Clone, Debug)]
pub(crate) struct FileEntry {
    pub(crate) token: String,
    /// Commitment to the original bytes, populated only when contents are loaded.
    ///
    /// Retaining the commitment instead of a second complete `Vec` keeps native read memory under
    /// the invocation ceiling rather than silently doubling every loaded file.
    pub(crate) original_commitment: Option<String>,
    /// Current overlay bytes. `None` means absent only when `loaded` is true.
    pub(crate) data: Option<Vec<u8>>,
    pub(crate) disk_exists: bool,
    pub(crate) disk_size: u64,
    pub(crate) loaded: bool,
    pub(crate) dirty: bool,
    pub(crate) identity: u64,
}

impl FileEntry {
    pub(crate) fn exists(&self) -> bool {
        if self.loaded {
            self.data.is_some()
        } else {
            self.disk_exists
        }
    }

    pub(crate) fn size(&self) -> Option<u64> {
        if self.loaded {
            self.data.as_ref().map(|bytes| bytes.len() as u64)
        } else {
            self.disk_exists.then_some(self.disk_size)
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HandleState {
    pub(crate) token: String,
    pub(crate) read: bool,
    pub(crate) write: bool,
    pub(crate) delete_on_close: bool,
    pub(crate) lock: LockLevel,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct OperationEvidence {
    pub(crate) operations: u64,
    pub(crate) syncs: u64,
    pub(crate) quota_denials: u64,
    pub(crate) read_bytes: u64,
    pub(crate) write_bytes: u64,
}

/// One base→generation lease pair and provisional invocation overlay.
pub struct StorageTransaction {
    pub(crate) interface: StorageInterface,
    pub(crate) access: StorageAccess,
    pub(crate) namespace: Namespace,
    pub(crate) limits: crate::StorageLimits,
    pub(crate) key: Arc<StorageKey>,
    pub(crate) ledger: Arc<QuotaLedger>,
    pub(crate) entries: BTreeMap<String, FileEntry>,
    pub(crate) baseline_files: BTreeSet<String>,
    pub(crate) handles: BTreeMap<u64, HandleState>,
    pub(crate) pending_delete: BTreeSet<String>,
    pub(crate) next_handle: u64,
    pub(crate) next_file_identity: u64,
    pub(crate) host_calls: u64,
    pub(crate) read_bytes: u64,
    pub(crate) write_bytes: u64,
    pub(crate) entropy_bytes: u64,
    pub(crate) native_loaded_bytes: u64,
    pub(crate) evidence: OperationEvidence,
    pub(crate) namespaces_root: Directory,
    pub(crate) transactions_root: Directory,
    pub(crate) trash_root: Directory,
    poisoned_bases: Arc<std::sync::Mutex<BTreeSet<String>>>,
    retired_transaction: Option<String>,
    reservation: Option<Reservation>,
    lease: Option<File>,
    finalized: bool,
    #[cfg(test)]
    post_marker_faults: Vec<(PostMarkerFault, StorageHostError)>,
    #[cfg(test)]
    post_marker_delay: Option<Duration>,
}

impl std::fmt::Debug for StorageTransaction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StorageTransaction([REDACTED])")
    }
}

impl StorageTransaction {
    pub(crate) fn begin(
        grant: StorageGrant,
        ledger: Arc<QuotaLedger>,
        namespaces_root: Directory,
        transactions_root: Directory,
        trash_root: Directory,
        poisoned_bases: Arc<std::sync::Mutex<BTreeSet<String>>>,
    ) -> Result<Self, StorageHostError> {
        if grant.namespace.base_directory.exists("poisoned")?
            || grant.namespace.directory.exists("poisoned")?
        {
            return Err(StorageHostError::Corrupt {
                scope: "poisoned-namespace",
            });
        }
        // `Namespace::resolve` already holds the base lease. This is the one defined lock order.
        let lease = grant
            .namespace
            .directory
            .open_private("lease.lock", false)?;
        lock_exclusive(&lease, grant.limits.lock_timeout_ms)?;

        let mut namespace_usage =
            scan_usage(&grant.namespace.directory, grant.limits.startup_max_entries)?;
        namespace_usage.entries = namespace_usage
            .entries
            .checked_add(1)
            .ok_or(StorageHostError::Arithmetic)?;
        namespace_usage.bytes = namespace_usage
            .bytes
            .checked_add(ENTRY_CHARGE)
            .ok_or(StorageHostError::Arithmetic)?;
        let mut baseline_files = BTreeSet::new();
        for token in grant
            .namespace
            .data_directory
            .entries_bounded(grant.limits.max_files_per_namespace)?
        {
            if !is_token(&token) {
                return Err(StorageHostError::Corrupt {
                    scope: "logical-token",
                });
            }
            let metadata = grant.namespace.data_directory.metadata(&token)?.ok_or(
                StorageHostError::Corrupt {
                    scope: "logical-token",
                },
            )?;
            if metadata.kind != EntryKind::File || metadata.nlink != 1 {
                return Err(StorageHostError::Corrupt {
                    scope: "logical-file",
                });
            }
            if metadata.len > grant.limits.max_file_bytes {
                return Err(StorageHostError::QuotaExceeded);
            }
            let _ = grant.namespace.data_directory.open_private(&token, false)?;
            baseline_files.insert(token);
        }
        if baseline_files.len() as u64 > grant.limits.max_files_per_namespace {
            return Err(StorageHostError::QuotaExceeded);
        }
        if namespace_usage.bytes > grant.limits.max_namespace_bytes {
            return Err(StorageHostError::QuotaExceeded);
        }
        let reservation = ledger.begin(
            format!(
                "{}/{}",
                grant.namespace.base_token, grant.namespace.generation_token
            ),
            namespace_usage,
        )?;
        Ok(Self {
            interface: grant.interface,
            access: grant.access,
            namespace: grant.namespace,
            limits: grant.limits,
            key: grant.key,
            ledger,
            entries: BTreeMap::new(),
            baseline_files,
            handles: BTreeMap::new(),
            pending_delete: BTreeSet::new(),
            next_handle: 1,
            next_file_identity: 1,
            host_calls: 0,
            read_bytes: 0,
            write_bytes: 0,
            entropy_bytes: 0,
            native_loaded_bytes: 0,
            evidence: OperationEvidence::default(),
            namespaces_root,
            transactions_root,
            trash_root,
            poisoned_bases,
            retired_transaction: None,
            reservation: Some(reservation),
            lease: Some(lease),
            finalized: false,
            #[cfg(test)]
            post_marker_faults: Vec::new(),
            #[cfg(test)]
            post_marker_delay: None,
        })
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
    pub fn open_handle_count(&self) -> usize {
        self.handles.len()
    }

    pub(crate) fn note_call(&mut self) -> Result<(), StorageHostError> {
        self.host_calls = self
            .host_calls
            .checked_add(1)
            .ok_or(StorageHostError::Arithmetic)?;
        self.evidence.operations = self
            .evidence
            .operations
            .checked_add(1)
            .ok_or(StorageHostError::Arithmetic)?;
        if self.host_calls > self.limits.max_host_calls_per_invocation {
            return Err(StorageHostError::QuotaExceeded);
        }
        Ok(())
    }

    pub(crate) fn charge_read(&mut self, bytes: u64) -> Result<(), StorageHostError> {
        if bytes > self.limits.max_read_bytes_per_call {
            return Err(StorageHostError::QuotaExceeded);
        }
        let total = self
            .read_bytes
            .checked_add(bytes)
            .ok_or(StorageHostError::Arithmetic)?;
        if total > self.limits.max_read_bytes_per_invocation {
            return Err(StorageHostError::QuotaExceeded);
        }
        self.read_bytes = total;
        self.evidence.read_bytes = total;
        Ok(())
    }

    pub(crate) fn charge_write(&mut self, bytes: u64) -> Result<(), StorageHostError> {
        if self.access != StorageAccess::ReadWrite {
            return Err(StorageHostError::PermissionDenied);
        }
        if bytes > self.limits.max_write_bytes_per_call {
            return Err(StorageHostError::QuotaExceeded);
        }
        let total = self
            .write_bytes
            .checked_add(bytes)
            .ok_or(StorageHostError::Arithmetic)?;
        if total > self.limits.max_write_bytes_per_invocation {
            return Err(StorageHostError::QuotaExceeded);
        }
        self.write_bytes = total;
        self.evidence.write_bytes = total;
        Ok(())
    }

    pub(crate) fn charge_entropy(&mut self, bytes: u64) -> Result<(), StorageHostError> {
        if bytes > self.limits.max_entropy_bytes_per_call {
            return Err(StorageHostError::QuotaExceeded);
        }
        let total = self
            .entropy_bytes
            .checked_add(bytes)
            .ok_or(StorageHostError::Arithmetic)?;
        if total > self.limits.max_entropy_bytes_per_invocation {
            return Err(StorageHostError::QuotaExceeded);
        }
        self.entropy_bytes = total;
        Ok(())
    }

    pub(crate) fn note_quota_denial(&mut self) {
        self.evidence.quota_denials = self.evidence.quota_denials.saturating_add(1);
    }

    pub(crate) fn validate_name(name: &str) -> Result<(), StorageHostError> {
        if name.is_empty()
            || name.len() > 128
            || !name.as_bytes()[0].is_ascii_lowercase() && !name.as_bytes()[0].is_ascii_digit()
            || !name.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
            || name == "."
            || name == ".."
        {
            return Err(StorageHostError::InvalidName);
        }
        Ok(())
    }

    pub(crate) fn logical_token(&self, name: &str) -> Result<String, StorageHostError> {
        Self::validate_name(name)?;
        Ok(self.key.token(
            DOMAIN_LOGICAL_PATH,
            &[
                self.namespace.base_token.as_bytes(),
                self.namespace.generation_token.as_bytes(),
                name.as_bytes(),
            ],
        ))
    }

    /// Loads only trusted metadata. Size/stat calls therefore cannot allocate the complete file.
    pub(crate) fn ensure_entry(&mut self, name: &str) -> Result<String, StorageHostError> {
        let token = self.logical_token(name)?;
        if !self.entries.contains_key(&token) {
            let metadata = self.namespace.data_directory.metadata(&token)?;
            let (disk_exists, disk_size) = match metadata {
                Some(metadata)
                    if metadata.kind == EntryKind::File
                        && metadata.nlink == 1
                        && metadata.len <= self.limits.max_file_bytes =>
                {
                    let _ = self.namespace.data_directory.open_private(&token, false)?;
                    (true, metadata.len)
                }
                Some(_) => {
                    return Err(StorageHostError::Corrupt {
                        scope: "logical-file",
                    });
                }
                None => (false, 0),
            };
            let identity = if disk_exists {
                self.allocate_file_identity()?
            } else {
                0
            };
            self.entries.insert(
                token.clone(),
                FileEntry {
                    token: token.clone(),
                    original_commitment: None,
                    data: None,
                    disk_exists,
                    disk_size,
                    loaded: !disk_exists,
                    dirty: false,
                    identity,
                },
            );
        }
        Ok(token)
    }

    pub(crate) fn ensure_loaded(&mut self, name: &str) -> Result<String, StorageHostError> {
        let token = self.ensure_entry(name)?;
        self.load_token(&token)?;
        Ok(token)
    }

    pub(crate) fn load_token(&mut self, token: &str) -> Result<(), StorageHostError> {
        let entry = self.entries.get(token).ok_or(StorageHostError::Corrupt {
            scope: "logical-entry",
        })?;
        if entry.loaded {
            return Ok(());
        }
        let loaded = self
            .native_loaded_bytes
            .checked_add(entry.disk_size)
            .ok_or(StorageHostError::Arithmetic)?;
        if loaded > self.limits.max_read_bytes_per_invocation {
            self.note_quota_denial();
            return Err(StorageHostError::QuotaExceeded);
        }
        let bytes = self
            .namespace
            .data_directory
            .read_bounded(token, self.limits.max_file_bytes)?;
        if bytes.len() as u64 != entry.disk_size {
            return Err(StorageHostError::Corrupt {
                scope: "logical-size-race",
            });
        }
        let commitment = file_commitment(&self.key, &bytes);
        let entry = self.entries.get_mut(token).expect("entry checked above");
        entry.original_commitment = Some(commitment);
        entry.data = Some(bytes);
        entry.loaded = true;
        self.native_loaded_bytes = loaded;
        Ok(())
    }

    pub(crate) fn allocate_file_identity(&mut self) -> Result<u64, StorageHostError> {
        let identity = self.next_file_identity;
        self.next_file_identity = self
            .next_file_identity
            .checked_add(1)
            .ok_or(StorageHostError::Arithmetic)?;
        Ok(identity)
    }

    pub(crate) fn reserve_candidate(
        &mut self,
        overrides: &[(&str, Option<&[u8]>)],
    ) -> Result<(), StorageHostError> {
        let mut files = 0_u64;
        let mut tokens = self.baseline_files.clone();
        tokens.extend(self.entries.keys().cloned());
        tokens.extend(overrides.iter().map(|(token, _)| (*token).to_owned()));
        for token in &tokens {
            let override_data = overrides
                .iter()
                .rev()
                .find(|(candidate, _)| *candidate == token)
                .map(|(_, data)| *data);
            let exists = match override_data {
                Some(data) => data.is_some(),
                None => self
                    .entries
                    .get(token)
                    .map_or(self.baseline_files.contains(token), FileEntry::exists),
            };
            if exists {
                files = files.checked_add(1).ok_or(StorageHostError::Arithmetic)?;
            }
            if let Some(Some(data)) = override_data
                && data.len() as u64 > self.limits.max_file_bytes
            {
                self.note_quota_denial();
                return Err(StorageHostError::QuotaExceeded);
            }
        }
        if files > self.limits.max_files_per_namespace {
            self.note_quota_denial();
            return Err(StorageHostError::QuotaExceeded);
        }

        let candidate = self.candidate_reservation(overrides)?;
        // Transaction directory + manifest + commit + applied/finalized marker, plus a base poison
        // marker. Failure-state headroom is reserved before staging: a base marker can fail while
        // the already-retired transaction remains the durable unknown-by-default evidence.
        let desired = 6_u64
            .checked_add(candidate.staged)
            .and_then(|entries| entries.checked_mul(ENTRY_CHARGE))
            .and_then(|bytes| bytes.checked_add(candidate.manifest_bytes))
            .and_then(|bytes| bytes.checked_add(candidate.staged_bytes))
            .ok_or(StorageHostError::Arithmetic)?;
        let staged_entries = candidate
            .staged
            .checked_add(6)
            .ok_or(StorageHostError::Arithmetic)?;
        let result = self
            .reservation
            .as_mut()
            .ok_or(StorageHostError::Corrupt {
                scope: "reservation",
            })?
            .reserve_to(desired, staged_entries);
        if matches!(result, Err(StorageHostError::QuotaExceeded)) {
            self.note_quota_denial();
        }
        result
    }

    /// Reservation inputs for a candidate transaction, derived without hashing any file contents.
    ///
    /// Every positional write reserves, so committing to complete candidate bytes here cost
    /// `O(N^2)` bytes hashed to append `N` frames to one file. A reservation only reads sizes, and
    /// a commitment is a fixed-width string that JSON never escapes, so a change set built with
    /// [`placeholder_commitment`] encodes to a byte-identical manifest length and yields the
    /// identical reservation. Only counts are returned: a placeholder has no path to durable bytes.
    fn candidate_reservation(
        &self,
        overrides: &[(&str, Option<&[u8]>)],
    ) -> Result<CandidateReservation, StorageHostError> {
        let changes = self.candidate_change_set(overrides, placeholder_commitment)?;
        let manifest = encode_manifest(&self.key, &self.namespace, changes)?;
        CandidateReservation::from_manifest(&manifest)
    }

    /// The durable change set: every `new` is a real HMAC over the real candidate bytes.
    fn candidate_changes(
        &self,
        overrides: &[(&str, Option<&[u8]>)],
    ) -> Result<Vec<Change>, StorageHostError> {
        self.candidate_change_set(overrides, file_commitment)
    }

    fn candidate_change_set(
        &self,
        overrides: &[(&str, Option<&[u8]>)],
        commitment: CommitmentFn,
    ) -> Result<Vec<Change>, StorageHostError> {
        let mut tokens = self
            .entries
            .values()
            .filter(|entry| entry.dirty)
            .map(|entry| entry.token.clone())
            .collect::<BTreeSet<_>>();
        tokens.extend(overrides.iter().map(|(token, _)| (*token).to_owned()));
        let mut changes = Vec::with_capacity(tokens.len());
        for token in tokens {
            let entry = self.entries.get(&token).ok_or(StorageHostError::Corrupt {
                scope: "candidate-entry",
            })?;
            if !entry.loaded {
                return Err(StorageHostError::Corrupt {
                    scope: "candidate-not-loaded",
                });
            }
            let data = overrides
                .iter()
                .rev()
                .find(|(candidate, _)| *candidate == token)
                .map_or(entry.data.as_deref(), |(_, data)| *data);
            changes.push(Change {
                token,
                old: entry.original_commitment.clone(),
                new: data.map(|bytes| commitment(&self.key, bytes)),
                new_size: data.map_or(0, |bytes| bytes.len() as u64),
            });
        }
        Ok(changes)
    }

    /// Aborts all provisional mutation and returns content-free evidence.
    pub fn abort(mut self) -> StorageEvidence {
        self.close_all_handles();
        if let Some(reservation) = self.reservation.take() {
            reservation.abort();
        }
        self.finalized = true;
        self.lease.take();
        self.make_evidence()
    }

    /// Finishes a read-only invocation after proving no resource leaked.
    pub fn finish_read(mut self) -> Result<StorageEvidence, StorageHostError> {
        if !self.handles.is_empty() {
            return Err(StorageHostError::Busy);
        }
        if self.entries.values().any(|entry| entry.dirty) {
            return Err(StorageHostError::PermissionDenied);
        }
        if let Some(reservation) = self.reservation.take() {
            reservation.abort();
        }
        self.finalized = true;
        self.lease.take();
        Ok(self.make_evidence())
    }

    #[cfg(test)]
    pub(crate) fn inject_post_marker_fault(&mut self, fault: PostMarkerFault) {
        self.inject_post_marker_error(fault, StorageHostError::Io);
    }

    /// Injects a post-marker failure of a chosen class, so a test can distinguish the causes an
    /// unaudited outcome now carries rather than only the input/output one.
    #[cfg(test)]
    pub(crate) fn inject_post_marker_error(
        &mut self,
        fault: PostMarkerFault,
        error: StorageHostError,
    ) {
        self.post_marker_faults.push((fault, error));
    }

    #[cfg(test)]
    pub(crate) fn inject_post_marker_delay(&mut self, delay: Duration) {
        self.post_marker_delay = Some(delay);
    }

    #[cfg(test)]
    fn fail_post_marker(&mut self, fault: PostMarkerFault) -> Result<(), StorageHostError> {
        match self
            .post_marker_faults
            .iter()
            .position(|(candidate, _)| *candidate == fault)
        {
            Some(index) => Err(self.post_marker_faults.remove(index).1),
            None => Ok(()),
        }
    }

    #[cfg(not(test))]
    fn fail_post_marker(&mut self, _fault: PostMarkerFault) -> Result<(), StorageHostError> {
        Ok(())
    }

    /// Returns the complete budget shared by job draining and not-yet-started finalization steps.
    #[must_use]
    pub fn finalization_budget(&self) -> Duration {
        Duration::from_millis(self.limits.finalization_budget_ms)
    }

    /// Makes all provisional files durable. The synchronized `commit` marker is the durable point.
    pub fn commit(self) -> Result<StorageEvidence, StorageHostError> {
        let deadline = Instant::now()
            .checked_add(self.finalization_budget())
            .ok_or(StorageHostError::Arithmetic)?;
        self.commit_before(deadline)
    }

    /// Finalizes against a deadline started by the async adapter before draining active jobs.
    #[doc(hidden)]
    pub fn commit_before(mut self, deadline: Instant) -> Result<StorageEvidence, StorageHostError> {
        if !self.handles.is_empty() {
            return Err(StorageHostError::Busy);
        }
        if self.access != StorageAccess::ReadWrite && self.entries.values().any(|entry| entry.dirty)
        {
            return Err(StorageHostError::PermissionDenied);
        }
        let changes = self.candidate_changes(&[])?;
        if changes.is_empty() {
            if let Some(reservation) = self.reservation.take() {
                reservation.abort();
            }
            self.finalized = true;
            self.lease.take();
            return Ok(self.make_evidence());
        }
        // Serialize and reserve the exact final manifest and every transaction entry before the
        // first staging mutation.
        self.reserve_candidate(&[])?;
        let manifest = encode_manifest(&self.key, &self.namespace, changes)?;
        check_budget(deadline)?;
        let (transaction_token, transaction_directory) =
            self.create_transaction_directory(deadline)?;
        let mut marker_started = false;
        let before_marker = (|| -> Result<(), StorageHostError> {
            // The transaction directory entry itself must be durable before its marker can be the
            // recovery point after a power loss.
            check_budget(deadline)?;
            self.transactions_root.sync()?;
            for change in &manifest.document.changes {
                if change.new.is_some() {
                    let data = self
                        .entries
                        .get(&change.token)
                        .and_then(|entry| entry.data.as_ref())
                        .ok_or(StorageHostError::Corrupt {
                            scope: "staging-data",
                        })?;
                    check_budget(deadline)?;
                    let mut file = transaction_directory.create_private(&change.token)?;
                    file.write_all(data)
                        .map_err(|source| transaction_directory.io_error(source))?;
                    check_budget(deadline)?;
                    file.sync_all()
                        .map_err(|source| transaction_directory.io_error(source))?;
                }
            }
            // Publish the manifest by same-directory rename only after its complete bytes are
            // synchronized. A power loss while writing leaves the explicitly recognized
            // `manifest.pending` pre-marker state, never a truncated file named `manifest` that
            // startup could mistake for unexplained corruption.
            check_budget(deadline)?;
            let mut file = transaction_directory.create_private("manifest.pending")?;
            file.write_all(&manifest.encoded)
                .map_err(|source| transaction_directory.io_error(source))?;
            check_budget(deadline)?;
            file.sync_all()
                .map_err(|source| transaction_directory.io_error(source))?;
            drop(file);
            check_budget(deadline)?;
            transaction_directory.rename_to(
                "manifest.pending",
                &transaction_directory,
                "manifest",
            )?;
            check_budget(deadline)?;
            transaction_directory.sync()?;

            // From the instant marker creation starts, a successful directory entry may survive
            // even when open validation, marker sync, a budget check, or directory sync fails.
            // Recovery treats any valid marker as committed, so every such live error is
            // structurally outcome-unaudited rather than an ordinary storage failure.
            check_budget(deadline)?;
            marker_started = true;
            let marker = transaction_directory.create_private("commit")?;
            self.fail_post_marker(PostMarkerFault::MarkerFileSync)?;
            check_budget(deadline)?;
            marker
                .sync_all()
                .map_err(|source| transaction_directory.io_error(source))?;
            self.fail_post_marker(PostMarkerFault::MarkerDirectorySync)?;
            check_budget(deadline)?;
            transaction_directory.sync()
        })();
        if let Err(error) = before_marker {
            if marker_started {
                return self.post_marker_failure(&error);
            }
            // Move a partial manifest/stage set atomically out of the recovery directory before
            // deleting it. A failed recursive unlink can then leave only bounded trash, never an
            // unknown transaction state that blocks the next startup.
            let fully_removed = discard_transaction(
                &self.transactions_root,
                &self.trash_root,
                &transaction_token,
                Some(deadline),
            )
            .unwrap_or(false);
            if !fully_removed && let Some(reservation) = self.reservation.take() {
                // The outcome is known to be uncommitted, but partial staging/trash still occupies
                // quota. Retain its exact pre-reserved peak until restart reconstructs accounting.
                reservation.retain_after_unknown();
            }
            return Err(error);
        }

        #[cfg(test)]
        if let Some(delay) = self.post_marker_delay.take() {
            std::thread::sleep(delay);
        }
        let after_marker = (|| -> Result<(StorageEvidence, String, Usage), StorageHostError> {
            // A marker sync is one already-started native syscall and may drain after the deadline.
            // Recheck before beginning the first apply step rather than treating the pre-sync check
            // as permission for the rest of finalization.
            check_budget(deadline)?;
            self.fail_post_marker(PostMarkerFault::Apply)?;
            apply_manifest_data(
                &self.namespaces_root,
                &transaction_directory,
                &manifest.document,
                &self.key,
                &self.limits,
                Some(deadline),
            )?;
            check_budget(deadline)?;
            self.fail_post_marker(PostMarkerFault::Scan)?;
            let mut final_usage = scan_usage_checked(
                &self.namespace.directory,
                self.limits.startup_max_entries,
                || check_budget(deadline),
            )?;
            final_usage.entries = final_usage
                .entries
                .checked_add(1)
                .ok_or(StorageHostError::Arithmetic)?;
            final_usage.bytes = final_usage
                .bytes
                .checked_add(ENTRY_CHARGE)
                .ok_or(StorageHostError::Arithmetic)?;

            check_budget(deadline)?;
            self.fail_post_marker(PostMarkerFault::Evidence)?;
            let evidence = self.make_evidence();
            check_budget(deadline)?;
            self.fail_post_marker(PostMarkerFault::TransactionSync)?;
            self.transactions_root.sync()?;
            check_budget(deadline)?;
            self.fail_post_marker(PostMarkerFault::Cleanup)?;
            let retired_candidate = format!("transaction-{transaction_token}");
            if self.trash_root.exists(&retired_candidate)? {
                return Err(StorageHostError::Corrupt {
                    scope: "trash-collision",
                });
            }
            // Record the deterministic destination before rename. If a parent sync or follow-up
            // scan fails after the rename itself, absence of `finalized` keeps the retired
            // directory outcome-unknown on the next startup without requiring another write.
            self.retired_transaction = Some(retired_candidate);
            let (retired_name, retired_usage) = retire_transaction(
                &self.transactions_root,
                &self.trash_root,
                &transaction_token,
                self.limits.startup_max_entries,
                Some(deadline),
            )?;

            let finalized_usage = Usage {
                bytes: retired_usage
                    .bytes
                    .checked_add(ENTRY_CHARGE)
                    .ok_or(StorageHostError::Arithmetic)?,
                entries: retired_usage
                    .entries
                    .checked_add(1)
                    .ok_or(StorageHostError::Arithmetic)?,
                files: retired_usage
                    .files
                    .checked_add(1)
                    .ok_or(StorageHostError::Arithmetic)?,
            };
            check_budget(deadline)?;
            self.fail_post_marker(PostMarkerFault::Accounting)?;
            if let Some(reservation) = self.reservation.as_mut() {
                reservation.commit(final_usage, finalized_usage)?;
            }

            // `finalized` is written only after scan, evidence, retirement, and ledger
            // reconciliation finish. On restart every committed retired transaction lacking this
            // synchronized marker is outcome-unknown by default, even if both live poison and
            // diagnostic marker writes failed.
            self.write_finalized_marker(&retired_name, deadline)?;
            // Release staging/failure headroom only after the marker is durable. Any marker error
            // keeps that reservation (including poison headroom) for the rest of this process.
            if let Some(reservation) = self.reservation.take() {
                reservation.finish_commit();
            }
            Ok((evidence, retired_name, finalized_usage))
        })();
        match after_marker {
            Ok((evidence, retired_name, retired_usage)) => {
                // Recursive deletion is bounded GC, not part of the durable outcome. The atomic
                // synchronized move above left a recognized quota-accounted trash entry; failure
                // here keeps that conservative charge until a later pass rather than turning a
                // committed success into an ordinary storage error.
                self.cleanup_retired_best_effort(&retired_name, retired_usage, deadline);
                self.finalized = true;
                self.lease.take();
                Ok(evidence)
            }
            Err(source) => self.post_marker_failure(&source),
        }
    }

    fn create_transaction_directory(
        &mut self,
        deadline: Instant,
    ) -> Result<(String, Directory), StorageHostError> {
        for _ in 0..16 {
            check_budget(deadline)?;
            let nonce = random_bytes(32)?;
            let token = self.key.token(
                DOMAIN_TRANSACTION,
                &[
                    self.namespace.base_token.as_bytes(),
                    self.namespace.generation_token.as_bytes(),
                    &nonce,
                ],
            );
            check_budget(deadline)?;
            match self.transactions_root.create_directory(&token) {
                Ok(directory) => return Ok((token, directory)),
                Err(StorageHostError::AlreadyExists) => {}
                Err(error) => {
                    // `mkdirat` may have succeeded before opening/validating the retained child
                    // failed. Retire that exact token when possible; otherwise keep its complete
                    // reservation charged instead of turning a failed create into free trash.
                    let remains = self.transactions_root.exists(&token).unwrap_or(true);
                    let fully_removed = remains
                        && discard_transaction(
                            &self.transactions_root,
                            &self.trash_root,
                            &token,
                            Some(deadline),
                        )
                        .unwrap_or(false);
                    if remains
                        && !fully_removed
                        && let Some(reservation) = self.reservation.take()
                    {
                        reservation.retain_after_unknown();
                    }
                    return Err(error);
                }
            }
        }
        Err(StorageHostError::Busy)
    }

    fn cleanup_retired_best_effort(&self, name: &str, usage: Usage, deadline: Instant) {
        if check_budget(deadline).is_err() {
            return;
        }
        if self.trash_root.remove_tree(name).is_err() {
            return;
        }
        // The entry is absent in this process now. Release its charge even if a later deadline or
        // parent sync fails; restart reconstructs whichever directory state survived a crash.
        self.ledger.release_root_usage(usage);
        if check_budget(deadline).is_err() {
            return;
        }
        #[allow(
            clippy::let_underscore_must_use,
            reason = "the entry is already unlinked and its charge released, so this parent sync \
                      is crash-durability only: restart reconstructs whichever state survived"
        )]
        let _ = self.trash_root.sync();
    }

    fn write_finalized_marker(
        &mut self,
        retired: &str,
        deadline: Instant,
    ) -> Result<(), StorageHostError> {
        let directory = self.trash_root.open_directory(retired)?;
        check_budget(deadline)?;
        self.fail_post_marker(PostMarkerFault::FinalizedCreate)?;
        let marker = directory.create_private("finalized.pending")?;
        check_budget(deadline)?;
        self.fail_post_marker(PostMarkerFault::FinalizedFileSync)?;
        marker
            .sync_all()
            .map_err(|source| directory.io_error(source))?;
        drop(marker);
        check_budget(deadline)?;
        directory.rename_to("finalized.pending", &directory, "finalized")?;
        check_budget(deadline)?;
        self.fail_post_marker(PostMarkerFault::FinalizedDirectorySync)?;
        directory.sync()
    }

    #[allow(
        clippy::let_underscore_must_use,
        reason = "every discarded call here rolls back a marker whose own publish already failed; \
                  the in-process poison registry, not this file, is what stops the base, and \
                  recovery treats a surviving `poisoned` entry as poisoned either way"
    )]
    fn persist_poison_best_effort(&mut self) {
        let directory = self.namespace.base_directory.clone();
        if directory.exists("poisoned").unwrap_or(true) {
            return;
        }
        if self
            .fail_post_marker(PostMarkerFault::PoisonCreate)
            .is_err()
        {
            return;
        }
        let Ok(marker) = directory.create_private("poisoned") else {
            return;
        };
        if self
            .fail_post_marker(PostMarkerFault::PoisonFileSync)
            .is_err()
            || marker.sync_all().is_err()
        {
            drop(marker);
            let _ = directory.remove_file("poisoned");
            let _ = directory.sync();
            return;
        }
        drop(marker);
        if self
            .fail_post_marker(PostMarkerFault::PoisonDirectorySync)
            .is_err()
            || directory.sync().is_err()
        {
            let _ = directory.remove_file("poisoned");
            let _ = directory.sync();
        }
    }

    #[allow(
        clippy::let_underscore_must_use,
        reason = "both discarded calls remove a `finalized` marker whose publish already failed; \
                  recovery gives `finalized.pending` precedence of unknown over `finalized`, so a \
                  failed removal reaches the same outcome this function returns"
    )]
    fn post_marker_failure(
        mut self,
        cause: &StorageHostError,
    ) -> Result<StorageEvidence, StorageHostError> {
        if let Some(retired) = &self.retired_transaction
            && let Ok(directory) = self.trash_root.open_directory(retired)
        {
            // A failed final-marker publish must remain unknown even if rename completed before a
            // later synchronization failure. Removal is best effort; recovery also gives an
            // explicit `finalized.pending` precedence of unknown over `finalized`.
            let _ = directory.remove_file("finalized");
            let _ = directory.sync();
        }
        self.poisoned_bases
            .lock()
            .expect("storage poison registry")
            .insert(self.namespace.base_token.clone());
        self.persist_poison_best_effort();
        self.close_all_handles();
        // Every started filesystem step has drained before this point. Keep its conservative
        // quota headroom for the rest of this host process: another namespace may already hold a
        // grant minted before the retained transaction became visible, so releasing now could let
        // that grant overspend the root before its next full scan. Restart rebuilds exact usage.
        if let Some(reservation) = self.reservation.take() {
            reservation.retain_after_unknown();
        }
        self.finalized = true;
        self.lease.take();
        // The outcome stays unknown, but which kind of thing made it unknown does not have to be:
        // a full filesystem and an exhausted quota are different operator actions.
        Err(StorageHostError::OutcomeUnaudited {
            cause: cause.class(),
        })
    }

    fn close_all_handles(&mut self) {
        let count = self.handles.len();
        self.handles.clear();
        for _ in 0..count {
            self.ledger.release_handle();
        }
    }

    /// Commits the exact successful provider output under its dedicated namespace-keyed domain.
    #[must_use]
    pub fn output_commitment(&self, bytes: &[u8]) -> String {
        self.key.commitment(
            DOMAIN_OUTPUT_EVIDENCE,
            &[
                self.namespace.base_token.as_bytes(),
                self.namespace.generation_token.as_bytes(),
                b"provider-output",
                bytes,
            ],
        )
    }

    fn make_evidence(&self) -> StorageEvidence {
        let operations = self.evidence.operations.to_be_bytes();
        let syncs = self.evidence.syncs.to_be_bytes();
        let denials = self.evidence.quota_denials.to_be_bytes();
        StorageEvidence {
            operations: self.evidence.operations,
            syncs: self.evidence.syncs,
            quota_denials: self.evidence.quota_denials,
            read_byte_bucket: byte_bucket(self.evidence.read_bytes),
            write_byte_bucket: byte_bucket(self.evidence.write_bytes),
            evidence_commitment: self.key.commitment(
                DOMAIN_OPERATION_EVIDENCE,
                &[
                    self.namespace.scope_commitment.as_bytes(),
                    &operations,
                    &syncs,
                    &denials,
                ],
            ),
            output_commitment: None,
        }
    }
}

impl Drop for StorageTransaction {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        self.close_all_handles();
        if let Some(reservation) = self.reservation.take() {
            reservation.abort();
        }
        self.lease.take();
        self.finalized = true;
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Change {
    token: String,
    old: Option<String>,
    new: Option<String>,
    new_size: u64,
}

/// The complete set of quantities [`StorageTransaction::reserve_candidate`] reads from a candidate
/// manifest.
///
/// Byte counts and nothing else, deliberately: the reservation path builds its manifest from
/// placeholder commitments, and a `CandidateReservation` has no representation that could be
/// staged, encoded, or written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CandidateReservation {
    /// Files that would be staged, one transaction directory entry each.
    staged: u64,
    /// Encoded manifest length.
    manifest_bytes: u64,
    /// Total staged file bytes.
    staged_bytes: u64,
}

impl CandidateReservation {
    fn from_manifest(manifest: &EncodedManifest) -> Result<Self, StorageHostError> {
        let mut staged = 0_u64;
        let mut staged_bytes = 0_u64;
        for change in &manifest.document.changes {
            if change.new.is_some() {
                staged = staged.checked_add(1).ok_or(StorageHostError::Arithmetic)?;
                staged_bytes = staged_bytes
                    .checked_add(change.new_size)
                    .ok_or(StorageHostError::Arithmetic)?;
            }
        }
        Ok(Self {
            staged,
            manifest_bytes: manifest.encoded.len() as u64,
            staged_bytes,
        })
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ManifestBody {
    api_version: String,
    base: String,
    generation: String,
    changes: Vec<Change>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ManifestDocument {
    api_version: String,
    base: String,
    generation: String,
    changes: Vec<Change>,
    mac: String,
}

struct EncodedManifest {
    document: ManifestDocument,
    encoded: Vec<u8>,
}

#[allow(
    clippy::map_err_ignore,
    reason = "serializing these owned string/integer manifest structs has no failing case; \
              serde_json fails only on a non-string map key or a Serialize implementation error, \
              and neither exists here"
)]
fn encode_manifest(
    key: &StorageKey,
    namespace: &Namespace,
    changes: Vec<Change>,
) -> Result<EncodedManifest, StorageHostError> {
    let body = ManifestBody {
        api_version: MANIFEST_VERSION.to_owned(),
        base: namespace.base_token.clone(),
        generation: namespace.generation_token.clone(),
        changes,
    };
    let body_bytes =
        serde_json::to_vec(&body).map_err(|_| StorageHostError::Corrupt { scope: "manifest" })?;
    let document = ManifestDocument {
        api_version: body.api_version,
        base: body.base,
        generation: body.generation,
        changes: body.changes,
        mac: key.commitment(DOMAIN_MANIFEST, &[body_bytes.as_slice()]),
    };
    let mut encoded = serde_json::to_vec(&document)
        .map_err(|_| StorageHostError::Corrupt { scope: "manifest" })?;
    encoded.push(b'\n');
    if encoded.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(StorageHostError::QuotaExceeded);
    }
    Ok(EncodedManifest { document, encoded })
}

fn retire_transaction(
    transactions: &Directory,
    trash: &Directory,
    transaction_token: &str,
    maximum_entries: u64,
    deadline: Option<Instant>,
) -> Result<(String, Usage), StorageHostError> {
    let destination = format!("transaction-{transaction_token}");
    maybe_check_budget(deadline)?;
    if trash.exists(&destination)? {
        return Err(StorageHostError::Corrupt {
            scope: "trash-collision",
        });
    }
    maybe_check_budget(deadline)?;
    transactions.rename_to(transaction_token, trash, &destination)?;
    maybe_check_budget(deadline)?;
    transactions.sync()?;
    maybe_check_budget(deadline)?;
    trash.sync()?;
    maybe_check_budget(deadline)?;
    let retired = trash.open_directory(&destination)?;
    let mut usage = scan_usage_checked(&retired, maximum_entries, || maybe_check_budget(deadline))?;
    usage.entries = usage
        .entries
        .checked_add(1)
        .ok_or(StorageHostError::Arithmetic)?;
    usage.bytes = usage
        .bytes
        .checked_add(ENTRY_CHARGE)
        .ok_or(StorageHostError::Arithmetic)?;
    Ok((destination, usage))
}

/// Atomically retires a pre-marker transaction and reports whether recursive cleanup was fully
/// synchronized. `false` is safe recognized trash, but its reservation must remain charged live.
fn discard_transaction(
    transactions: &Directory,
    trash: &Directory,
    transaction_token: &str,
    deadline: Option<Instant>,
) -> Result<bool, StorageHostError> {
    let destination = format!("transaction-{transaction_token}");
    maybe_check_budget(deadline)?;
    if trash.exists(&destination)? {
        return Err(StorageHostError::Corrupt {
            scope: "trash-collision",
        });
    }
    maybe_check_budget(deadline)?;
    transactions.rename_to(transaction_token, trash, &destination)?;
    maybe_check_budget(deadline)?;
    transactions.sync()?;
    maybe_check_budget(deadline)?;
    trash.sync()?;
    maybe_check_budget(deadline)?;
    if trash.remove_tree(&destination).is_err() {
        return Ok(false);
    }
    maybe_check_budget(deadline)?;
    trash.sync()?;
    Ok(true)
}

pub(crate) fn recover_transactions(
    layout: &Layout,
    key: &StorageKey,
    limits: &crate::StorageLimits,
) -> Result<(), StorageHostError> {
    let entries = layout
        .transactions()
        .entries_prefix(limits.startup_max_transactions.saturating_add(1))?;
    if entries.len() as u64 > limits.startup_max_transactions {
        return Err(StorageHostError::StartupTransactionLimit);
    }
    for token in entries {
        if !is_token(&token) {
            return Err(StorageHostError::Corrupt {
                scope: "transaction-token",
            });
        }
        let metadata =
            layout
                .transactions()
                .metadata(&token)?
                .ok_or(StorageHostError::Corrupt {
                    scope: "transaction-type",
                })?;
        if metadata.kind != EntryKind::Directory {
            return Err(StorageHostError::Corrupt {
                scope: "transaction-type",
            });
        }
        let transaction = layout.transactions().open_directory(&token)?;
        recover_transaction(layout, &token, &transaction, key, limits)?;
    }
    Ok(())
}

pub(crate) fn recover_retired_transactions(
    layout: &Layout,
    key: &StorageKey,
    limits: &crate::StorageLimits,
) -> Result<(), StorageHostError> {
    let trash_entries = layout
        .trash()
        .entries_prefix(limits.startup_max_entries.saturating_add(1))?;
    if trash_entries.len() as u64 > limits.startup_max_entries {
        return Err(StorageHostError::StartupEntryLimit {
            count: trash_entries.len() as u64,
            maximum: limits.startup_max_entries,
        });
    }
    for name in trash_entries {
        let Some(token) = name.strip_prefix("transaction-") else {
            continue;
        };
        if !is_token(token) {
            return Err(StorageHostError::Corrupt {
                scope: "trash-transaction-token",
            });
        }
        let metadata = layout
            .trash()
            .metadata(&name)?
            .ok_or(StorageHostError::Corrupt {
                scope: "trash-transaction",
            })?;
        if metadata.kind != EntryKind::Directory {
            return Err(StorageHostError::Corrupt {
                scope: "trash-transaction",
            });
        }
        let transaction = layout.trash().open_directory(&name)?;
        let maximum = limits.max_files_per_namespace.saturating_add(5);
        let entries = transaction.entries_prefix(maximum.saturating_add(1))?;
        if entries.len() as u64 > maximum {
            return Err(StorageHostError::Corrupt {
                scope: "transaction-entry-count",
            });
        }
        for entry in &entries {
            let metadata = transaction
                .metadata(entry)?
                .ok_or(StorageHostError::Corrupt {
                    scope: "trash-transaction-entry",
                })?;
            if metadata.kind != EntryKind::File || metadata.nlink != 1 {
                return Err(StorageHostError::Corrupt {
                    scope: "trash-transaction-entry",
                });
            }
            let _ = transaction.open_private(entry, false)?;
        }
        let has_commit = entries.iter().any(|entry| entry == "commit");
        if !has_commit {
            // A failed best-effort pre-marker discard is recognized quota-accounted trash and has
            // no uncertain durable outcome to poison.
            continue;
        }
        let has_manifest = entries.iter().any(|entry| entry == "manifest");
        let has_applied = entries.iter().any(|entry| entry == "applied");
        let outcome_unknown = entries.iter().any(|entry| entry == "outcome-unknown");
        let finalized = entries.iter().any(|entry| entry == "finalized");
        let finalized_pending = entries.iter().any(|entry| entry == "finalized.pending");
        if !has_manifest
            || !has_applied
            || (finalized && finalized_pending)
            || entries.iter().any(|entry| {
                !matches!(
                    entry.as_str(),
                    "manifest"
                        | "commit"
                        | "applied"
                        | "outcome-unknown"
                        | "finalized"
                        | "finalized.pending"
                )
            })
        {
            return Err(StorageHostError::Corrupt {
                scope: "trash-transaction-state",
            });
        }
        validate_empty_marker(&transaction, "commit", true)?;
        validate_empty_marker(&transaction, "applied", true)?;
        validate_empty_marker(&transaction, "outcome-unknown", outcome_unknown)?;
        validate_empty_marker(&transaction, "finalized", finalized)?;
        validate_empty_marker(&transaction, "finalized.pending", finalized_pending)?;
        let manifest: ManifestDocument =
            serde_json::from_slice(&transaction.read_bounded("manifest", MAX_MANIFEST_BYTES)?)
                .map_err(|error| {
                    crate::report_decode_failure("retired-manifest", &error);
                    StorageHostError::Corrupt { scope: "manifest" }
                })?;
        verify_manifest(&manifest, key, limits)?;
        // Unknown wins over every optimistic hint. Most importantly, absence of the one durable
        // final marker is itself unknown; live reconciliation must not depend on successfully
        // creating a second marker on a failing filesystem.
        let unknown = outcome_unknown || finalized_pending || !finalized;
        if !unknown || layout.quarantine().exists(&manifest.base)? {
            continue;
        }
        if !layout.namespaces().exists(&manifest.base)? {
            return Err(StorageHostError::Corrupt {
                scope: "missing-transaction-base",
            });
        }
        let base = layout.namespaces().open_directory(&manifest.base)?;
        poison(&base)?;
    }
    Ok(())
}

fn recover_transaction(
    layout: &Layout,
    transaction_token: &str,
    transaction: &Directory,
    key: &StorageKey,
    limits: &crate::StorageLimits,
) -> Result<(), StorageHostError> {
    let maximum = limits.max_files_per_namespace.saturating_add(3);
    let entries = transaction.entries_prefix(maximum.saturating_add(1))?;
    if entries.len() as u64 > maximum {
        return Err(StorageHostError::Corrupt {
            scope: "transaction-entry-count",
        });
    }
    for name in &entries {
        let metadata = transaction
            .metadata(name)?
            .ok_or(StorageHostError::Corrupt {
                scope: "transaction-entry",
            })?;
        if metadata.kind != EntryKind::File || metadata.nlink != 1 {
            return Err(StorageHostError::Corrupt {
                scope: "transaction-entry",
            });
        }
        let _ = transaction.open_private(name, false)?;
        if is_token(name) && metadata.len > limits.max_file_bytes {
            return Err(StorageHostError::Corrupt {
                scope: "transaction-stage-size",
            });
        }
        if !is_token(name)
            && !matches!(
                name.as_str(),
                "manifest.pending" | "manifest" | "commit" | "applied"
            )
        {
            return Err(StorageHostError::Corrupt {
                scope: "transaction-entry-name",
            });
        }
    }
    let has_pending_manifest = entries.iter().any(|name| name == "manifest.pending");
    let has_manifest = entries.iter().any(|name| name == "manifest");
    let has_commit = entries.iter().any(|name| name == "commit");
    let has_applied = entries.iter().any(|name| name == "applied");
    validate_empty_marker(transaction, "commit", has_commit)?;
    validate_empty_marker(transaction, "applied", has_applied)?;

    if has_pending_manifest {
        if has_manifest || has_commit || has_applied {
            return Err(StorageHostError::Corrupt {
                scope: "transaction-state",
            });
        }
        // The only non-token pre-marker file is the complete-or-partial manifest staging name.
        // Its target was never atomically published, so rollback is unambiguous.
        let _fully_removed = discard_transaction(
            layout.transactions(),
            layout.trash(),
            transaction_token,
            None,
        )?;
        return Ok(());
    }
    if !has_manifest {
        if has_commit || has_applied || entries.iter().any(|name| !is_token(name)) {
            return Err(StorageHostError::Corrupt {
                scope: "transaction-state",
            });
        }
        // A token-named directory containing only bounded token-named stages is the other
        // recognized pre-manifest crash state.
        let _fully_removed = discard_transaction(
            layout.transactions(),
            layout.trash(),
            transaction_token,
            None,
        )?;
        return Ok(());
    }

    let manifest: ManifestDocument =
        serde_json::from_slice(&transaction.read_bounded("manifest", MAX_MANIFEST_BYTES)?)
            .map_err(|error| {
                crate::report_decode_failure("manifest", &error);
                StorageHostError::Corrupt { scope: "manifest" }
            })?;
    verify_manifest(&manifest, key, limits)?;
    if has_applied && !has_commit {
        return Err(StorageHostError::Corrupt {
            scope: "transaction-state",
        });
    }
    let expected_stages = manifest
        .changes
        .iter()
        .filter(|change| change.new.is_some())
        .map(|change| change.token.as_str())
        .collect::<BTreeSet<_>>();
    for stage in entries.iter().filter(|name| is_token(name)) {
        if !expected_stages.contains(stage.as_str()) {
            return Err(StorageHostError::Corrupt {
                scope: "unknown-stage",
            });
        }
    }
    if !has_commit {
        if expected_stages
            .iter()
            .any(|stage| !entries.iter().any(|entry| entry == stage))
        {
            return Err(StorageHostError::Corrupt {
                scope: "missing-stage",
            });
        }
        let _fully_removed = discard_transaction(
            layout.transactions(),
            layout.trash(),
            transaction_token,
            None,
        )?;
        return Ok(());
    }

    // Namespace validation runs before recovery so one corrupt base cannot block healthy peers.
    // If that base was quarantined, retain its committed roll-forward state beside it rather than
    // turning isolated corruption into a root-wide startup failure or deleting outcome evidence.
    if layout.quarantine().exists(&manifest.base)? {
        let destination = format!("transaction-{transaction_token}");
        if layout.quarantine().exists(&destination)? {
            return Err(StorageHostError::Corrupt {
                scope: "quarantine-collision",
            });
        }
        layout
            .transactions()
            .rename_to(transaction_token, layout.quarantine(), &destination)?;
        layout.transactions().sync()?;
        layout.quarantine().sync()?;
        return Ok(());
    }

    // Finding a durable marker at startup means the previous process could not return a known,
    // audited outcome. Persist poison even if that process failed before it could write its live
    // marker; recovery without poison would make uncertain data silently usable after restart.
    let recovery = (|| {
        let base = layout.namespaces().open_directory(&manifest.base)?;
        poison(&base)?;
        apply_manifest_data(
            layout.namespaces(),
            transaction,
            &manifest,
            key,
            limits,
            None,
        )
    })();
    if let Err(error) = recovery {
        if isolated_recovery_error(&error) {
            quarantine_recovery(layout, &manifest.base, transaction_token, limits)?;
            return Ok(());
        }
        return Err(error);
    }
    // Keep the recovered committed transaction as reconciliation evidence. It intentionally has
    // no `finalized` marker, so retired-state recovery and every GC pass classify it as unknown.
    let _ = retire_transaction(
        layout.transactions(),
        layout.trash(),
        transaction_token,
        limits.startup_max_entries,
        None,
    )?;
    Ok(())
}

fn isolated_recovery_error(error: &StorageHostError) -> bool {
    matches!(
        error,
        StorageHostError::Corrupt { .. } | StorageHostError::UnsafeRoot { .. }
    ) || matches!(
        error,
        StorageHostError::RootIo { source, .. }
            if matches!(
                source.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotADirectory
            )
    )
}

fn quarantine_recovery(
    layout: &Layout,
    base: &str,
    transaction_token: &str,
    limits: &crate::StorageLimits,
) -> Result<(), StorageHostError> {
    if !layout.namespaces().exists(base)? {
        return Err(StorageHostError::Corrupt {
            scope: "missing-transaction-base",
        });
    }
    let base_count = layout
        .quarantine()
        .entries_prefix(limits.max_quarantined_namespaces.saturating_add(1))?
        .into_iter()
        .filter(|name| !name.starts_with("transaction-"))
        .count() as u64;
    if base_count >= limits.max_quarantined_namespaces {
        return Err(StorageHostError::Corrupt {
            scope: "quarantine-capacity",
        });
    }
    if layout.quarantine().exists(base)? {
        return Err(StorageHostError::Corrupt {
            scope: "quarantine-collision",
        });
    }
    let transaction = format!("transaction-{transaction_token}");
    if layout.quarantine().exists(&transaction)? {
        return Err(StorageHostError::Corrupt {
            scope: "quarantine-collision",
        });
    }
    layout
        .namespaces()
        .rename_to(base, layout.quarantine(), base)?;
    layout.namespaces().sync()?;
    layout.quarantine().sync()?;
    layout
        .transactions()
        .rename_to(transaction_token, layout.quarantine(), &transaction)?;
    layout.transactions().sync()?;
    layout.quarantine().sync()
}

fn validate_empty_marker(
    transaction: &Directory,
    name: &str,
    present: bool,
) -> Result<(), StorageHostError> {
    if !present {
        return Ok(());
    }
    let metadata = transaction
        .metadata(name)?
        .ok_or(StorageHostError::Corrupt {
            scope: "transaction-marker",
        })?;
    if metadata.kind != EntryKind::File || metadata.len != 0 || metadata.nlink != 1 {
        return Err(StorageHostError::Corrupt {
            scope: "transaction-marker",
        });
    }
    let _ = transaction.open_private(name, false)?;
    Ok(())
}

fn verify_manifest(
    manifest: &ManifestDocument,
    key: &StorageKey,
    limits: &crate::StorageLimits,
) -> Result<(), StorageHostError> {
    if manifest.api_version != MANIFEST_VERSION
        || !is_token(&manifest.base)
        || !is_token(&manifest.generation)
        || manifest.changes.len() as u64 > limits.max_files_per_namespace
    {
        return Err(StorageHostError::Corrupt { scope: "manifest" });
    }
    let mut tokens = BTreeSet::new();
    for change in &manifest.changes {
        if !is_token(&change.token)
            || !tokens.insert(change.token.as_str())
            || change
                .old
                .as_deref()
                .is_some_and(|value| !is_commitment(value))
            || change
                .new
                .as_deref()
                .is_some_and(|value| !is_commitment(value))
            || change.new_size > limits.max_file_bytes
            || (change.new.is_none() && change.new_size != 0)
        {
            return Err(StorageHostError::Corrupt { scope: "manifest" });
        }
    }
    let body = ManifestBody {
        api_version: manifest.api_version.clone(),
        base: manifest.base.clone(),
        generation: manifest.generation.clone(),
        changes: manifest.changes.clone(),
    };
    #[allow(
        clippy::map_err_ignore,
        reason = "re-serializing the already-decoded manifest body has no failing case; every \
                  rejection of retained bytes happens in the checks above and the MAC below"
    )]
    let encoded =
        serde_json::to_vec(&body).map_err(|_| StorageHostError::Corrupt { scope: "manifest" })?;
    let expected = key.commitment(DOMAIN_MANIFEST, &[encoded.as_slice()]);
    if manifest.mac != expected {
        return Err(StorageHostError::Corrupt {
            scope: "manifest-mac",
        });
    }
    Ok(())
}

fn apply_manifest_data(
    namespaces_root: &Directory,
    transaction: &Directory,
    manifest: &ManifestDocument,
    key: &StorageKey,
    limits: &crate::StorageLimits,
    deadline: Option<Instant>,
) -> Result<(), StorageHostError> {
    verify_manifest(manifest, key, limits)?;
    maybe_check_budget(deadline)?;
    let base = namespaces_root.open_directory(&manifest.base)?;
    maybe_check_budget(deadline)?;
    let generation = base.open_directory(&manifest.generation)?;
    maybe_check_budget(deadline)?;
    let data = generation.open_directory("data")?;
    for change in &manifest.changes {
        maybe_check_budget(deadline)?;
        let current_commitment = match data.metadata(&change.token)? {
            Some(metadata) if metadata.kind == EntryKind::File => {
                if metadata.len > limits.max_file_bytes || metadata.nlink != 1 {
                    return Err(StorageHostError::Corrupt {
                        scope: "transaction-target",
                    });
                }
                maybe_check_budget(deadline)?;
                Some(streamed_file_commitment(
                    &data,
                    &change.token,
                    metadata.len,
                    key,
                )?)
            }
            Some(_) => {
                return Err(StorageHostError::Corrupt {
                    scope: "transaction-target",
                });
            }
            None => None,
        };
        if current_commitment == change.new {
            maybe_check_budget(deadline)?;
            if transaction.exists(&change.token)? {
                maybe_check_budget(deadline)?;
                transaction.remove_file(&change.token)?;
            }
            continue;
        }
        if current_commitment != change.old {
            return Err(StorageHostError::Corrupt {
                scope: "transaction-identity",
            });
        }
        match &change.new {
            Some(expected) => {
                maybe_check_budget(deadline)?;
                if !transaction.exists(&change.token)? {
                    return Err(StorageHostError::Corrupt {
                        scope: "missing-stage",
                    });
                }
                maybe_check_budget(deadline)?;
                let staged =
                    transaction
                        .metadata(&change.token)?
                        .ok_or(StorageHostError::Corrupt {
                            scope: "missing-stage",
                        })?;
                if staged.kind != EntryKind::File
                    || staged.nlink != 1
                    || staged.len != change.new_size
                    || streamed_file_commitment(transaction, &change.token, staged.len, key)?
                        != *expected
                {
                    return Err(StorageHostError::Corrupt {
                        scope: "staging-identity",
                    });
                }
                maybe_check_budget(deadline)?;
                transaction.rename_to(&change.token, &data, &change.token)?;
                maybe_check_budget(deadline)?;
                let file = data.open_private(&change.token, false)?;
                data.validate_private_file(&change.token, &file)?;
            }
            None => {
                maybe_check_budget(deadline)?;
                if data.exists(&change.token)? {
                    maybe_check_budget(deadline)?;
                    data.remove_file(&change.token)?;
                }
            }
        }
    }
    maybe_check_budget(deadline)?;
    data.sync()?;
    maybe_check_budget(deadline)?;
    if !transaction.exists("applied")? {
        maybe_check_budget(deadline)?;
        let file = transaction.create_private("applied")?;
        maybe_check_budget(deadline)?;
        file.sync_all()
            .map_err(|source| transaction.io_error(source))?;
        maybe_check_budget(deadline)?;
        transaction.sync()?;
    } else {
        maybe_check_budget(deadline)?;
        validate_empty_marker(transaction, "applied", true)?;
    }
    Ok(())
}

fn poison(namespace: &Directory) -> Result<(), StorageHostError> {
    if !namespace.exists("poisoned")? {
        let file = namespace.create_private("poisoned")?;
        file.sync_all()
            .map_err(|source| namespace.io_error(source))?;
        namespace.sync()?;
    }
    Ok(())
}

/// Width of every commitment this crate produces: `hmac-sha256:` plus 32 hex-encoded bytes.
const COMMITMENT_CHARS: usize = "hmac-sha256:".len() + 64;

/// Fixed-width stand-in for a file commitment, used only while sizing a reservation.
///
/// It is deliberately *not* commitment-shaped: [`is_commitment`] rejects it, so [`verify_manifest`]
/// rejects any manifest carrying it and [`apply_manifest_data`] refuses before touching data. It is
/// exactly as wide as a real commitment and contains only characters JSON never escapes, so a
/// manifest encoded with it is byte-for-byte the same length as the durable one.
const RESERVATION_PLACEHOLDER: &str =
    "reservation-placeholder-not-a-commitment-never-durable-000000000000000000000";
const _: () = assert!(RESERVATION_PLACEHOLDER.len() == COMMITMENT_CHARS);

/// How a candidate change set commits to file contents.
type CommitmentFn = fn(&StorageKey, &[u8]) -> String;

/// The durable commitment. Hashes the complete file and belongs on the commit path.
fn file_commitment(key: &StorageKey, bytes: &[u8]) -> String {
    key.commitment(DOMAIN_CONTENT, &[b"file", bytes])
}

/// The reservation-path substitute for [`file_commitment`]. Hashes nothing.
///
/// Its one caller is [`StorageTransaction::candidate_reservation`], which returns byte counts
/// rather than a manifest.
fn placeholder_commitment(_key: &StorageKey, _bytes: &[u8]) -> String {
    RESERVATION_PLACEHOLDER.to_owned()
}

fn streamed_file_commitment(
    directory: &Directory,
    name: &str,
    length: u64,
    key: &StorageKey,
) -> Result<String, StorageHostError> {
    let file = directory.open_private(name, false)?;
    key.commitment_reader(DOMAIN_CONTENT, &[b"file"], length, file)
        .map_err(|source| directory.io_error(source))
}

fn is_commitment(value: &str) -> bool {
    value.strip_prefix("hmac-sha256:").is_some_and(is_token)
}

fn check_budget(deadline: Instant) -> Result<(), StorageHostError> {
    if Instant::now() >= deadline {
        Err(StorageHostError::Timeout)
    } else {
        Ok(())
    }
}

fn maybe_check_budget(deadline: Option<Instant>) -> Result<(), StorageHostError> {
    deadline.map_or(Ok(()), check_budget)
}

#[allow(
    clippy::map_err_ignore,
    reason = "TryFromIntError carries only out-of-range, which Arithmetic already states, and an \
              exact clock value may not be exported as storage telemetry"
)]
pub(crate) fn monotonic_ns() -> Result<u64, StorageHostError> {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    u64::try_from(START.get_or_init(Instant::now).elapsed().as_nanos())
        .map_err(|_| StorageHostError::Arithmetic)
}

#[allow(
    clippy::map_err_ignore,
    reason = "SystemTimeError carries only how far the clock sits before the epoch and \
              TryFromIntError only out-of-range; Clock and Arithmetic already state both, and \
              neither value may be exported as storage telemetry"
)]
pub(crate) fn wall_ms() -> Result<u64, StorageHostError> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StorageHostError::Clock)?
            .as_millis(),
    )
    .map_err(|_| StorageHostError::Arithmetic)
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, path::Path, time::Duration};

    use dekopon_capability::{StorageAccess, StorageInterface, StorageNamespace};

    use super::{
        COMMITMENT_CHARS, CandidateReservation, Change, FileEntry, ManifestDocument,
        PostMarkerFault, RESERVATION_PLACEHOLDER, StorageTransaction, encode_manifest,
        file_commitment, is_commitment, verify_manifest,
    };
    use crate::{
        ContinuityPolicy, OpenOptions, StorageFailureClass, StorageGrantRequest, StorageHost,
        StorageHostError, StorageLimits,
    };

    /// An unknown outcome that cannot say what made it unknown is an operator dead end.
    ///
    /// Every post-marker failure answers the guest with the same opaque mapped error, so the cause
    /// has nowhere to live except this discriminant and the message that renders it. A quota and a
    /// filesystem failure are different actions — raise the limit, or free the disk — and before
    /// this they were the same fieldless variant.
    #[test]
    fn an_unaudited_outcome_names_the_class_of_the_failure_that_caused_it() {
        let mut causes = Vec::new();
        for injected in [StorageHostError::QuotaExceeded, StorageHostError::Io] {
            let temporary = tempfile::tempdir().expect("temporary directory");
            let directory = temporary.path().canonicalize().expect("canonical tempdir");
            let root = directory.join("storage");
            let key = directory.join("key.yaml");
            fs::write(
                &key,
                "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
            )
            .expect("write key");
            fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
            let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
            let grant = host
                .grant(StorageGrantRequest::new(
                    "cause-probe".parse().expect("invocation"),
                    "memory.chat.record".parse().expect("capability"),
                    "memory-chat".parse().expect("provider"),
                    StorageInterface::Jsonl,
                    StorageAccess::ReadWrite,
                    StorageNamespace::Chat,
                    "reviewer".parse().expect("agent"),
                    "slack.t0123abc.u9xyz".parse().expect("subject"),
                    "slack",
                    "scientist-slack",
                    "c0123abc",
                    "c0123abc:1712345678.000100",
                    ContinuityPolicy::Stable,
                    b"authority".to_vec(),
                ))
                .expect("grant");
            let mut transaction = host.begin(grant).expect("transaction");
            transaction
                .jsonl_append("turns.jsonl", 0, br#"{"durable":true}"#)
                .expect("append");
            transaction.inject_post_marker_error(PostMarkerFault::Accounting, injected);
            let error = transaction
                .commit()
                .expect_err("a post-marker failure is never a success");
            let StorageHostError::OutcomeUnaudited { cause } = error else {
                panic!("a post-marker failure is an unaudited outcome, got {error:?}");
            };
            // The rendered message is what an operator reads through the broker's error chain.
            assert!(
                error.to_string().contains(cause.label()),
                "{error} does not name its cause"
            );
            causes.push(cause);
        }
        assert_eq!(
            causes,
            vec![StorageFailureClass::Quota, StorageFailureClass::Io],
            "two different causes must not collapse into one discriminant"
        );
    }

    #[test]
    fn every_post_marker_failure_is_outcome_unaudited_poisoned_and_recoverable() {
        for (index, (fault, reconciliation_fault)) in [
            (PostMarkerFault::MarkerFileSync, None),
            (PostMarkerFault::MarkerDirectorySync, None),
            (PostMarkerFault::Apply, None),
            (PostMarkerFault::Scan, None),
            (PostMarkerFault::Accounting, None),
            (PostMarkerFault::Evidence, None),
            (PostMarkerFault::TransactionSync, None),
            (PostMarkerFault::Cleanup, None),
            (PostMarkerFault::FinalizedCreate, None),
            (PostMarkerFault::FinalizedFileSync, None),
            (PostMarkerFault::FinalizedDirectorySync, None),
            (PostMarkerFault::Apply, Some(PostMarkerFault::PoisonCreate)),
            (
                PostMarkerFault::Apply,
                Some(PostMarkerFault::PoisonFileSync),
            ),
            (
                PostMarkerFault::Apply,
                Some(PostMarkerFault::PoisonDirectorySync),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let temporary = tempfile::tempdir().expect("temporary directory");
            let directory = temporary.path().canonicalize().expect("canonical tempdir");
            let root = directory.join("storage");
            let key = directory.join("key.yaml");
            fs::write(
                &key,
                "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
            )
            .expect("write key");
            fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
            let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
            let grant = host
                .grant(StorageGrantRequest::new(
                    format!("fault-{index}").parse().expect("invocation"),
                    "memory.chat.record".parse().expect("capability"),
                    "memory-chat".parse().expect("provider"),
                    StorageInterface::Jsonl,
                    StorageAccess::ReadWrite,
                    StorageNamespace::Chat,
                    "reviewer".parse().expect("agent"),
                    "slack.t0123abc.u9xyz".parse().expect("subject"),
                    "slack",
                    "scientist-slack",
                    "c0123abc",
                    "c0123abc:1712345678.000100",
                    ContinuityPolicy::Stable,
                    b"authority".to_vec(),
                ))
                .expect("grant");
            let mut transaction = host.begin(grant).expect("transaction");
            transaction
                .jsonl_append("turns.jsonl", 0, br#"{"durable":true}"#)
                .expect("append");
            transaction.inject_post_marker_fault(fault);
            if let Some(reconciliation_fault) = reconciliation_fault {
                transaction.inject_post_marker_fault(reconciliation_fault);
            }
            assert!(matches!(
                transaction.commit(),
                Err(StorageHostError::OutcomeUnaudited {
                    cause: StorageFailureClass::Io
                })
            ));
            assert_eq!(
                walk(&root).iter().any(|path| path.ends_with("poisoned")),
                reconciliation_fault.is_none(),
                "injected poison-marker refusal was not honored"
            );
            assert!(matches!(
                host.grant(StorageGrantRequest::new(
                    format!("fault-followup-{index}")
                        .parse()
                        .expect("invocation"),
                    "memory.chat.record".parse().expect("capability"),
                    "memory-chat".parse().expect("provider"),
                    StorageInterface::Jsonl,
                    StorageAccess::ReadWrite,
                    StorageNamespace::Chat,
                    "reviewer".parse().expect("agent"),
                    "slack.t0123abc.u9xyz".parse().expect("subject"),
                    "slack",
                    "scientist-slack",
                    "c0123abc",
                    "c0123abc:1712345678.000100",
                    ContinuityPolicy::AuthorityBound,
                    b"different-authority".to_vec(),
                )),
                Err(StorageHostError::Corrupt { .. })
            ));
            if matches!(
                fault,
                PostMarkerFault::Accounting
                    | PostMarkerFault::FinalizedCreate
                    | PostMarkerFault::FinalizedFileSync
                    | PostMarkerFault::FinalizedDirectorySync
            ) {
                host.gc_once()
                    .expect("GC skips committed state without finalized");
                let paths = walk(&root);
                assert!(
                    paths.iter().any(|path| {
                        path.parent()
                            .is_some_and(|parent| parent.ends_with("trash"))
                            && path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .is_some_and(|name| name.starts_with("transaction-"))
                    }),
                    "GC deleted unknown-by-default reconciliation state"
                );
                assert!(
                    !paths.iter().any(|path| path.ends_with("finalized")),
                    "failed finalization left an optimistic marker"
                );
            }
            if reconciliation_fault.is_none()
                && (index == 0 || fault == PostMarkerFault::Accounting)
            {
                let marker = walk(&root)
                    .into_iter()
                    .find(|path| path.ends_with("poisoned"))
                    .expect("poison marker");
                fs::remove_file(marker).expect("simulate a failed live poison write");
            }
            drop(host);

            // Startup rolls every durable marker forward, including a fault injected before the
            // first live apply step. The namespace remains poisoned for manual reconciliation.
            let reopened = StorageHost::open(&root, &key, StorageLimits::default())
                .expect("recognized recovery succeeds");
            drop(reopened);
            assert!(
                walk(&root).iter().any(|path| path.ends_with("poisoned")),
                "startup must restore poison for every recovered durable marker"
            );
            let contents = walk(&root)
                .into_iter()
                .filter(|path| path.parent().is_some_and(|parent| parent.ends_with("data")))
                .filter_map(|path| fs::read(path).ok())
                .collect::<Vec<_>>();
            assert!(
                contents
                    .iter()
                    .any(|bytes| bytes == b"{\"durable\":true}\n"),
                "fault {fault:?} lost the durable transaction"
            );
        }
    }

    #[test]
    fn missing_committed_stage_quarantines_attributable_base_and_transaction() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let directory = temporary.path().canonicalize().expect("canonical tempdir");
        let root = directory.join("storage");
        let key = directory.join("key.yaml");
        fs::write(
            &key,
            "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
        )
        .expect("write key");
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
        let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");

        let healthy = host
            .grant(StorageGrantRequest::new(
                "healthy-before-quarantine".parse().expect("invocation"),
                "memory.chat.recent".parse().expect("capability"),
                "memory-chat".parse().expect("provider"),
                StorageInterface::Jsonl,
                StorageAccess::ReadOnly,
                StorageNamespace::Chat,
                "reviewer".parse().expect("agent"),
                "slack.t0123abc.uhealthy".parse().expect("subject"),
                "slack",
                "scientist-slack",
                "c0123abc",
                "c0123abc:1712345678.000100",
                ContinuityPolicy::AuthorityBound,
                b"authority".to_vec(),
            ))
            .expect("healthy grant");
        host.begin(healthy)
            .expect("healthy transaction")
            .finish_read()
            .expect("healthy finish");

        let damaged = host
            .grant(StorageGrantRequest::new(
                "damaged-commit".parse().expect("invocation"),
                "memory.chat.record".parse().expect("capability"),
                "memory-chat".parse().expect("provider"),
                StorageInterface::Jsonl,
                StorageAccess::ReadWrite,
                StorageNamespace::Chat,
                "reviewer".parse().expect("agent"),
                "slack.t0123abc.udamaged".parse().expect("subject"),
                "slack",
                "scientist-slack",
                "c0123abc",
                "c0123abc:1712345678.000100",
                ContinuityPolicy::AuthorityBound,
                b"authority".to_vec(),
            ))
            .expect("damaged grant");
        let mut transaction = host.begin(damaged).expect("damaged transaction");
        transaction
            .jsonl_append("turns.jsonl", 0, br#"{"retained":true}"#)
            .expect("append");
        transaction.inject_post_marker_fault(PostMarkerFault::Apply);
        assert!(matches!(
            transaction.commit(),
            Err(StorageHostError::OutcomeUnaudited {
                cause: StorageFailureClass::Io
            })
        ));

        let transaction = fs::read_dir(root.join("transactions"))
            .expect("transactions")
            .next()
            .expect("committed transaction")
            .expect("transaction entry")
            .path();
        let stage = fs::read_dir(&transaction)
            .expect("transaction entries")
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_str().is_some_and(super::is_token))
            .expect("staged file")
            .path();
        fs::remove_file(stage).expect("simulate corrupt committed stage");
        drop(host);

        let reopened = StorageHost::open(&root, &key, StorageLimits::default())
            .expect("healthy namespaces survive attributable recovery corruption");
        assert_eq!(
            fs::read_dir(root.join("quarantine"))
                .expect("quarantine")
                .count(),
            2,
            "the base and its committed transaction are retained together"
        );
        let healthy = reopened
            .grant(StorageGrantRequest::new(
                "healthy-after-quarantine".parse().expect("invocation"),
                "memory.chat.recent".parse().expect("capability"),
                "memory-chat".parse().expect("provider"),
                StorageInterface::Jsonl,
                StorageAccess::ReadOnly,
                StorageNamespace::Chat,
                "reviewer".parse().expect("agent"),
                "slack.t0123abc.uhealthy".parse().expect("subject"),
                "slack",
                "scientist-slack",
                "c0123abc",
                "c0123abc:1712345678.000100",
                ContinuityPolicy::AuthorityBound,
                b"authority".to_vec(),
            ))
            .expect("healthy grant after quarantine");
        reopened
            .begin(healthy)
            .expect("healthy transaction")
            .finish_read()
            .expect("healthy finish");
    }

    #[test]
    fn finalization_budget_stops_the_next_step_after_a_marker_syscall_drains() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let directory = temporary.path().canonicalize().expect("canonical tempdir");
        let root = directory.join("storage");
        let key = directory.join("key.yaml");
        fs::write(
            &key,
            "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
        )
        .expect("write key");
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
        let limits = StorageLimits {
            finalization_budget_ms: 200,
            ..StorageLimits::default()
        };
        let host = StorageHost::open(&root, &key, limits.clone()).expect("host");
        let grant = host
            .grant(StorageGrantRequest::new(
                "budget-delay".parse().expect("invocation"),
                "memory.chat.record".parse().expect("capability"),
                "memory-chat".parse().expect("provider"),
                StorageInterface::Jsonl,
                StorageAccess::ReadWrite,
                StorageNamespace::Chat,
                "reviewer".parse().expect("agent"),
                "slack.t0123abc.u9xyz".parse().expect("subject"),
                "slack",
                "scientist-slack",
                "c0123abc",
                "c0123abc:1712345678.000100",
                ContinuityPolicy::Stable,
                b"authority".to_vec(),
            ))
            .expect("grant");
        let mut transaction = host.begin(grant).expect("transaction");
        transaction
            .jsonl_append("turns.jsonl", 0, br#"{"budget":true}"#)
            .expect("append");
        transaction.inject_post_marker_delay(Duration::from_millis(300));
        // A blown finalization budget is an unaudited outcome with a different cause than a failed
        // write, which is exactly the distinction an operator clearing the namespace needs.
        assert!(matches!(
            transaction.commit(),
            Err(StorageHostError::OutcomeUnaudited {
                cause: StorageFailureClass::Timeout
            })
        ));
        drop(host);

        let reopened = StorageHost::open(&root, &key, limits).expect("recovery");
        drop(reopened);
        assert!(
            walk(&root)
                .into_iter()
                .filter_map(|path| fs::read(path).ok())
                .any(|bytes| bytes == b"{\"budget\":true}\n")
        );
    }

    fn probe_host(limits: StorageLimits) -> (tempfile::TempDir, StorageHost) {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let directory = temporary.path().canonicalize().expect("canonical tempdir");
        let key = directory.join("key.yaml");
        fs::write(
            &key,
            "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
        )
        .expect("write key");
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
        let host =
            StorageHost::open(directory.join("storage"), &key, limits).expect("durable-files host");
        (temporary, host)
    }

    fn vfs_transaction(host: &StorageHost, invocation: &str) -> StorageTransaction {
        let grant = host
            .grant(StorageGrantRequest::new(
                invocation.parse().expect("invocation"),
                "probe.vfs".parse().expect("capability"),
                "storage-probe".parse().expect("provider"),
                StorageInterface::DurableFiles,
                StorageAccess::ReadWrite,
                StorageNamespace::Chat,
                "reviewer".parse().expect("agent"),
                "slack.t0123abc.u9xyz".parse().expect("subject"),
                "slack",
                "scientist-slack",
                "c0123abc",
                "c0123abc:1712345678.000100",
                ContinuityPolicy::Stable,
                b"authority".to_vec(),
            ))
            .expect("grant");
        host.begin(grant).expect("transaction")
    }

    /// The pre-change reservation computation, kept verbatim as the property-test oracle.
    fn real_commitment_reservation(
        transaction: &StorageTransaction,
        overrides: &[(&str, Option<&[u8]>)],
    ) -> Result<CandidateReservation, StorageHostError> {
        let changes = transaction.candidate_changes(overrides)?;
        let manifest = encode_manifest(&transaction.key, &transaction.namespace, changes)?;
        CandidateReservation::from_manifest(&manifest)
    }

    #[track_caller]
    fn assert_reservations_agree(
        transaction: &StorageTransaction,
        overrides: &[(&str, Option<&[u8]>)],
        shape: &str,
    ) {
        let expected = real_commitment_reservation(transaction, overrides);
        let actual = transaction.candidate_reservation(overrides);
        match (&expected, &actual) {
            (Ok(expected), Ok(actual)) => assert_eq!(expected, actual, "{shape}"),
            (Err(expected), Err(actual)) => assert_eq!(
                format!("{expected:?}"),
                format!("{actual:?}"),
                "{shape}: refusals differ"
            ),
            _ => panic!("{shape}: {expected:?} against {actual:?}"),
        }
    }

    fn token_at(index: usize) -> String {
        format!("{index:064x}")
    }

    fn install_entry(
        transaction: &mut StorageTransaction,
        token: &str,
        original: Option<&[u8]>,
        data: Option<Vec<u8>>,
        dirty: bool,
        loaded: bool,
    ) {
        let entry = FileEntry {
            token: token.to_owned(),
            original_commitment: original.map(|bytes| file_commitment(&transaction.key, bytes)),
            data,
            disk_exists: original.is_some(),
            disk_size: original.map_or(0, |bytes| bytes.len() as u64),
            loaded,
            dirty,
            identity: 1,
        };
        transaction.entries.insert(token.to_owned(), entry);
    }

    /// One deterministic xorshift stream. A property test needs many shapes, not real entropy.
    struct Shapes(u64);

    impl Shapes {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    #[test]
    fn every_commitment_is_one_fixed_width_that_the_placeholder_matches_but_never_impersonates() {
        let (_temporary, host) = probe_host(StorageLimits::default());
        let transaction = vfs_transaction(&host, "commitment-width");
        for bytes in [
            b"".as_slice(),
            b"\x00",
            &[0xff; 1],
            &[0x5a; 4096],
            &vec![0x00; 1024 * 1024],
        ] {
            let commitment = file_commitment(&transaction.key, bytes);
            assert_eq!(commitment.chars().count(), COMMITMENT_CHARS);
            assert_eq!(commitment.len(), RESERVATION_PLACEHOLDER.len());
            assert!(is_commitment(&commitment));
            // Encoded length must not depend on the value: neither shape can be JSON-escaped.
            assert_eq!(
                serde_json::to_vec(&commitment)
                    .expect("commitment json")
                    .len(),
                serde_json::to_vec(RESERVATION_PLACEHOLDER)
                    .expect("placeholder json")
                    .len()
            );
        }
        // The placeholder is deliberately not commitment-shaped, so the durable validator rejects
        // it even if one ever reached an encoded manifest.
        assert!(!is_commitment(RESERVATION_PLACEHOLDER));
        transaction.abort();
    }

    #[test]
    fn a_manifest_carrying_the_reservation_placeholder_never_validates() {
        let (_temporary, host) = probe_host(StorageLimits::default());
        let transaction = vfs_transaction(&host, "placeholder-refused");
        let token = token_at(1);
        let real = encode_manifest(
            &transaction.key,
            &transaction.namespace,
            vec![Change {
                token: token.clone(),
                old: None,
                new: Some(file_commitment(&transaction.key, b"durable")),
                new_size: 7,
            }],
        )
        .expect("real manifest");
        let placeholder = encode_manifest(
            &transaction.key,
            &transaction.namespace,
            vec![Change {
                token,
                old: None,
                new: Some(RESERVATION_PLACEHOLDER.to_owned()),
                new_size: 7,
            }],
        )
        .expect("placeholder manifest");
        // Identical size, which is the whole point, and an authentic MAC over its own body.
        assert_eq!(real.encoded.len(), placeholder.encoded.len());
        let limits = StorageLimits::default();
        verify_manifest(&real.document, &transaction.key, &limits).expect("real manifest verifies");
        assert!(matches!(
            verify_manifest(&placeholder.document, &transaction.key, &limits),
            Err(StorageHostError::Corrupt { scope: "manifest" })
        ));
        // The same refusal survives a round trip through the durable encoding.
        let decoded: ManifestDocument =
            serde_json::from_slice(&placeholder.encoded).expect("decode placeholder manifest");
        assert!(matches!(
            verify_manifest(&decoded, &transaction.key, &limits),
            Err(StorageHostError::Corrupt { scope: "manifest" })
        ));
        transaction.abort();
    }

    #[test]
    fn placeholder_reservations_equal_real_commitment_reservations_for_every_candidate_shape() {
        let limits = StorageLimits {
            // Keeps the `max_file_bytes` boundary reachable without hashing 16 MiB per case on the
            // oracle path.
            max_file_bytes: 64 * 1024,
            ..StorageLimits::default()
        };
        let (_temporary, host) = probe_host(limits);
        let mut transaction = vfs_transaction(&host, "reservation-property");
        let maximum = usize::try_from(transaction.limits.max_file_bytes).expect("bounded");
        let lengths = [0_usize, 1, 2, 17, 512, 4096, maximum - 1, maximum];
        let empty = Vec::new();
        let small = vec![b'c'; 17];
        let large = vec![b'c'; maximum];

        // Named shapes first, so a reviewer sees the boundaries this covers without reading the
        // generator below.
        assert_reservations_agree(&transaction, &[], "empty transaction, no overrides");

        install_entry(
            &mut transaction,
            &token_at(1),
            None,
            Some(empty.clone()),
            true,
            true,
        );
        assert_reservations_agree(&transaction, &[], "one dirty zero-length file");
        assert_reservations_agree(
            &transaction,
            &[(&token_at(1), Some(&small))],
            "override shadowing a dirty entry",
        );
        assert_reservations_agree(
            &transaction,
            &[(&token_at(1), Some(&small)), (&token_at(1), Some(&large))],
            "repeated override for one token",
        );
        assert_reservations_agree(
            &transaction,
            &[(&token_at(1), None)],
            "override deleting a dirty entry",
        );

        install_entry(
            &mut transaction,
            &token_at(2),
            Some(&small),
            Some(large.clone()),
            true,
            true,
        );
        assert_reservations_agree(&transaction, &[], "sparse growth to max_file_bytes");
        install_entry(
            &mut transaction,
            &token_at(3),
            Some(&small),
            None,
            true,
            true,
        );
        assert_reservations_agree(&transaction, &[], "dirty deletion of a loaded file");
        for index in 4..12 {
            install_entry(
                &mut transaction,
                &token_at(index),
                Some(&small),
                Some(vec![b'd'; index * 331]),
                true,
                true,
            );
        }
        assert_reservations_agree(&transaction, &[], "eleven dirty files");
        install_entry(
            &mut transaction,
            &token_at(12),
            Some(&small),
            None,
            false,
            false,
        );
        assert_reservations_agree(&transaction, &[], "an unloaded clean entry is not a change");
        assert_reservations_agree(
            &transaction,
            &[(&token_at(12), Some(&small))],
            "override against an unloaded entry",
        );
        assert_reservations_agree(
            &transaction,
            &[(&token_at(900), Some(&small))],
            "override without an entry",
        );

        let mut shapes = Shapes(0x9e37_79b9_7f4a_7c15);
        for case in 0..192 {
            transaction.entries.clear();
            let count = shapes.below(6);
            let mut tokens = Vec::with_capacity(count);
            for index in 0..count {
                let token = token_at(index);
                let original = (shapes.below(3) != 0)
                    .then(|| vec![b'o'; lengths[shapes.below(lengths.len())]]);
                let data = (shapes.below(4) != 0)
                    .then(|| vec![b'c'; lengths[shapes.below(lengths.len())]]);
                install_entry(
                    &mut transaction,
                    &token,
                    original.as_deref(),
                    data,
                    shapes.below(2) == 0,
                    shapes.below(16) != 0,
                );
                tokens.push(token);
            }
            let mut owned: Vec<(String, Option<Vec<u8>>)> = Vec::new();
            for _ in 0..shapes.below(4) {
                let token = if tokens.is_empty() || shapes.below(8) == 0 {
                    token_at(900 + shapes.below(3))
                } else {
                    tokens[shapes.below(tokens.len())].clone()
                };
                let data = (shapes.below(4) != 0)
                    .then(|| vec![b'v'; lengths[shapes.below(lengths.len())]]);
                owned.push((token, data));
            }
            let overrides = owned
                .iter()
                .map(|(token, data)| (token.as_str(), data.as_deref()))
                .collect::<Vec<_>>();
            assert_reservations_agree(&transaction, &overrides, &format!("generated case {case}"));
            assert_reservations_agree(
                &transaction,
                &[],
                &format!("generated case {case} without overrides"),
            );
        }
        transaction.abort();
    }

    #[test]
    fn appending_frames_never_recommits_to_the_whole_file() {
        const FRAME: u64 = 4096;
        const FRAMES: u64 = 1_000;

        let (_temporary, host) = probe_host(StorageLimits::default());
        let mut transaction = vfs_transaction(&host, "append-cost");
        let handle = transaction
            .vfs_open(
                "wal.db",
                OpenOptions {
                    read: true,
                    write: true,
                    create: true,
                    ..OpenOptions::default()
                },
            )
            .expect("open");
        let frame = vec![0x5a_u8; usize::try_from(FRAME).expect("bounded frame")];
        let before = crate::key::hashed_bytes();
        let started = std::time::Instant::now();
        for index in 0..FRAMES {
            transaction
                .vfs_write_at(handle, index * FRAME, &frame)
                .expect("append");
        }
        let hashed = crate::key::hashed_bytes() - before;
        let elapsed = started.elapsed();
        let written = FRAMES * FRAME;
        println!("{FRAMES} appends of {FRAME} B hashed {hashed} bytes in {elapsed:?}");
        // Recommitting to the whole candidate file on every write hashes `written * FRAMES / 2`,
        // roughly 2 GiB here. Reserving a write may cost the manifest, never the file again.
        assert!(
            hashed <= written,
            "{FRAMES} appends totalling {written} bytes hashed {hashed} bytes: \
             the reservation path is committing to complete file contents"
        );

        // The durable path still commits to the real bytes: the file itself, plus the staged copy
        // and the on-disk target that `apply_manifest_data` re-reads.
        transaction.vfs_close(handle).expect("close");
        let before_commit = crate::key::hashed_bytes();
        transaction.commit().expect("commit");
        let commit_hashed = crate::key::hashed_bytes() - before_commit;
        println!("commit hashed {commit_hashed} bytes for a {written}-byte file");
        assert!(
            commit_hashed >= written,
            "commit hashed only {commit_hashed} bytes for a {written}-byte file"
        );
    }

    /// Every path under `root`, absolute, so a test can read or remove what it finds.
    fn walk(root: &Path) -> Vec<std::path::PathBuf> {
        dekopon_test_support::snapshot_tree(root)
            .into_iter()
            .map(|entry| root.join(entry.relative))
            .collect()
    }
}
