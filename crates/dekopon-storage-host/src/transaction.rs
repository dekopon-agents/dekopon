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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PostMarkerFault {
    MarkerFileSync,
    MarkerDirectorySync,
    Apply,
    Scan,
    Accounting,
    Evidence,
    Cleanup,
    TransactionSync,
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
    reservation: Option<Reservation>,
    lease: Option<File>,
    finalized: bool,
    #[cfg(test)]
    post_marker_fault: Option<PostMarkerFault>,
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
        for token in grant.namespace.data_directory.entries()? {
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
            reservation: Some(reservation),
            lease: Some(lease),
            finalized: false,
            #[cfg(test)]
            post_marker_fault: None,
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

        let changes = self.candidate_changes(overrides)?;
        let manifest_bytes = encode_manifest(&self.key, &self.namespace, changes)?;
        let staged = manifest_bytes
            .document
            .changes
            .iter()
            .filter(|change| change.new.is_some())
            .count() as u64;
        // Transaction directory + manifest + commit + applied, plus one base poison marker whose
        // headroom must already exist if any post-marker step becomes outcome-unknown.
        let mut desired = 5_u64
            .checked_add(staged)
            .and_then(|entries| entries.checked_mul(ENTRY_CHARGE))
            .and_then(|bytes| bytes.checked_add(manifest_bytes.encoded.len() as u64))
            .ok_or(StorageHostError::Arithmetic)?;
        for change in &manifest_bytes.document.changes {
            if change.new.is_some() {
                desired = desired
                    .checked_add(change.new_size)
                    .ok_or(StorageHostError::Arithmetic)?;
            }
        }
        let staged_entries = staged.checked_add(5).ok_or(StorageHostError::Arithmetic)?;
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

    fn candidate_changes(
        &self,
        overrides: &[(&str, Option<&[u8]>)],
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
                new: data.map(|bytes| file_commitment(&self.key, bytes)),
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
        self.post_marker_fault = Some(fault);
    }

    #[cfg(test)]
    fn fail_post_marker(&mut self, fault: PostMarkerFault) -> Result<(), StorageHostError> {
        if self.post_marker_fault == Some(fault) {
            self.post_marker_fault = None;
            Err(StorageHostError::Io)
        } else {
            Ok(())
        }
    }

    #[cfg(not(test))]
    fn fail_post_marker(&mut self, _fault: PostMarkerFault) -> Result<(), StorageHostError> {
        Ok(())
    }

    /// Makes all provisional files durable. The synchronized `commit` marker is the durable point.
    pub fn commit(mut self) -> Result<StorageEvidence, StorageHostError> {
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
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(self.limits.finalization_budget_ms))
            .ok_or(StorageHostError::Arithmetic)?;
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
                return self.post_marker_failure();
            }
            // Move a partial manifest/stage set atomically out of the recovery directory before
            // deleting it. A failed recursive unlink can then leave only bounded trash, never an
            // unknown transaction state that blocks the next startup.
            let _ = discard_transaction(
                &self.transactions_root,
                &self.trash_root,
                &transaction_token,
            );
            return Err(error);
        }

        let after_marker = (|| -> Result<(StorageEvidence, String, Usage), StorageHostError> {
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
            let (retired_name, retired_usage) = retire_transaction(
                &self.transactions_root,
                &self.trash_root,
                &transaction_token,
                self.limits.startup_max_entries,
                Some(deadline),
            )?;

            check_budget(deadline)?;
            self.fail_post_marker(PostMarkerFault::Accounting)?;
            if let Some(reservation) = self.reservation.as_mut() {
                reservation.commit(final_usage, retired_usage)?;
            }
            // A finalized reservation drops without changing the reconciled ledger. Keeping it in
            // the option until commit succeeds lets post-marker error handling retain conservative
            // headroom even if reconciliation itself ever fails.
            self.reservation.take();
            Ok((evidence, retired_name, retired_usage))
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
            Err(_) => self.post_marker_failure(),
        }
    }

    fn create_transaction_directory(
        &self,
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
                Err(error) => return Err(error),
            }
        }
        Err(StorageHostError::Busy)
    }

    fn cleanup_retired_best_effort(&self, name: &str, usage: Usage, deadline: Instant) {
        if check_budget(deadline).is_err() {
            return;
        }
        if self.trash_root.remove_tree(name).is_err() || check_budget(deadline).is_err() {
            return;
        }
        if self.trash_root.sync().is_ok() {
            self.ledger.release_root_usage(usage);
        }
    }

    fn post_marker_failure(mut self) -> Result<StorageEvidence, StorageHostError> {
        let _ = poison(&self.namespace.base_directory);
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
        Err(StorageHostError::OutcomeUnaudited)
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

fn discard_transaction(
    transactions: &Directory,
    trash: &Directory,
    transaction_token: &str,
) -> Result<(), StorageHostError> {
    let destination = format!("transaction-{transaction_token}");
    if trash.exists(&destination)? {
        return Err(StorageHostError::Corrupt {
            scope: "trash-collision",
        });
    }
    transactions.rename_to(transaction_token, trash, &destination)?;
    transactions.sync()?;
    trash.sync()?;
    if trash.remove_tree(&destination).is_ok() {
        let _ = trash.sync();
    }
    Ok(())
}

pub(crate) fn recover_transactions(
    layout: &Layout,
    key: &StorageKey,
    limits: &crate::StorageLimits,
) -> Result<(), StorageHostError> {
    let entries = layout.transactions().entries()?;
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

fn recover_transaction(
    layout: &Layout,
    transaction_token: &str,
    transaction: &Directory,
    key: &StorageKey,
    limits: &crate::StorageLimits,
) -> Result<(), StorageHostError> {
    let entries = transaction.entries()?;
    if entries.len() as u64 > limits.max_files_per_namespace.saturating_add(3) {
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
        discard_transaction(layout.transactions(), layout.trash(), transaction_token)?;
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
        discard_transaction(layout.transactions(), layout.trash(), transaction_token)?;
        return Ok(());
    }

    let manifest: ManifestDocument =
        serde_json::from_slice(&transaction.read_bounded("manifest", MAX_MANIFEST_BYTES)?)
            .map_err(|_| StorageHostError::Corrupt { scope: "manifest" })?;
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
        discard_transaction(layout.transactions(), layout.trash(), transaction_token)?;
        return Ok(());
    }

    apply_manifest_data(
        layout.namespaces(),
        transaction,
        &manifest,
        key,
        limits,
        None,
    )?;
    discard_transaction(layout.transactions(), layout.trash(), transaction_token)
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
        let current = match data.metadata(&change.token)? {
            Some(metadata) if metadata.kind == EntryKind::File => {
                if metadata.len > limits.max_file_bytes || metadata.nlink != 1 {
                    return Err(StorageHostError::Corrupt {
                        scope: "transaction-target",
                    });
                }
                maybe_check_budget(deadline)?;
                Some(data.read_bounded(&change.token, limits.max_file_bytes)?)
            }
            Some(_) => {
                return Err(StorageHostError::Corrupt {
                    scope: "transaction-target",
                });
            }
            None => None,
        };
        let current_commitment = current.as_deref().map(|bytes| file_commitment(key, bytes));
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
                let staged = transaction.read_bounded(&change.token, change.new_size)?;
                if staged.len() as u64 != change.new_size
                    || file_commitment(key, &staged) != *expected
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

fn file_commitment(key: &StorageKey, bytes: &[u8]) -> String {
    key.commitment(DOMAIN_CONTENT, &[b"file", bytes])
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

pub(crate) fn monotonic_ns() -> Result<u64, StorageHostError> {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    u64::try_from(START.get_or_init(Instant::now).elapsed().as_nanos())
        .map_err(|_| StorageHostError::Arithmetic)
}

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
    use std::{fs, os::unix::fs::PermissionsExt as _, path::Path};

    use dekopon_capability::{StorageAccess, StorageInterface, StorageNamespace};

    use super::PostMarkerFault;
    use crate::{
        ContinuityPolicy, StorageGrantRequest, StorageHost, StorageHostError, StorageLimits,
    };

    #[test]
    fn every_post_marker_failure_is_outcome_unaudited_poisoned_and_recoverable() {
        for (index, fault) in [
            PostMarkerFault::MarkerFileSync,
            PostMarkerFault::MarkerDirectorySync,
            PostMarkerFault::Apply,
            PostMarkerFault::Scan,
            PostMarkerFault::Accounting,
            PostMarkerFault::Evidence,
            PostMarkerFault::TransactionSync,
            PostMarkerFault::Cleanup,
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
            assert!(matches!(
                transaction.commit(),
                Err(StorageHostError::OutcomeUnaudited)
            ));
            assert!(walk(&root).iter().any(|path| path.ends_with("poisoned")));
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
            drop(host);

            // Startup rolls every durable marker forward, including a fault injected before the
            // first live apply step. The namespace remains poisoned for manual reconciliation.
            let reopened = StorageHost::open(&root, &key, StorageLimits::default())
                .expect("recognized recovery succeeds");
            drop(reopened);
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

    fn walk(path: &Path) -> Vec<std::path::PathBuf> {
        let mut paths = vec![path.to_path_buf()];
        if path.is_dir() {
            for entry in fs::read_dir(path).expect("read directory") {
                paths.extend(walk(&entry.expect("entry").path()));
            }
        }
        paths
    }
}
