//! Direct per-invocation namespace storage and bounded native resources.
use crate::{
    StorageEvidence, StorageGrant, StorageHostError,
    key::{DOMAIN_LOGICAL_PATH, DOMAIN_OPERATION_EVIDENCE, DOMAIN_OUTPUT_EVIDENCE, StorageKey},
    layout::{ENTRY_CHARGE, EntryKind, Usage, scan_usage, usage_with_directory_entry},
    metrics::byte_bucket,
    namespace::{Namespace, is_token, lock_exclusive},
    quota::{QuotaLedger, Reservation},
    vfs::LockLevel,
};
use dekopon_capability::{StorageAccess, StorageInterface};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Write as _,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
/// One logical file's live presence and length for this invocation.
///
/// Presence and length are stat-derived when the file is first named and moved by each mutation
/// this invocation applies; no path reads a file to learn how long it is. `data` is the JSONL
/// append/replace working copy — that path builds the next contents from the previous ones — and
/// is the only thing charged against the invocation load ceiling.
#[derive(Clone, Debug)]
pub(crate) struct FileEntry {
    /// Bounded working bytes, authoritative only while `loaded` is true.
    pub(crate) data: Option<Vec<u8>>,
    pub(crate) loaded: bool,
    exists: bool,
    size: u64,
    pub(crate) identity: u64,
}

impl FileEntry {
    pub(crate) const fn exists(&self) -> bool {
        self.exists
    }

    pub(crate) fn size(&self) -> Option<u64> {
        self.exists.then_some(self.size)
    }

    /// Records the length a completed mutation left, which the working copy no longer mirrors.
    pub(crate) fn present(&mut self, size: u64) {
        self.exists = true;
        self.size = size;
        self.data = None;
        self.loaded = false;
    }

    /// Records the contents a completed JSONL mutation wrote, retained as the working copy.
    pub(crate) fn mirrored(&mut self, bytes: Vec<u8>) {
        self.exists = true;
        self.size = bytes.len() as u64;
        self.data = Some(bytes);
        self.loaded = true;
    }

    /// Records that a completed mutation removed the file, ending its identity.
    pub(crate) fn absent(&mut self) {
        self.exists = false;
        self.size = 0;
        self.identity = 0;
        self.data = None;
        self.loaded = true;
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

/// One logical file's length before a reserved mutation and the length that mutation writes.
#[derive(Debug)]
struct PlannedChange {
    token: String,
    before: Option<u64>,
    after: Option<u64>,
}

/// Every logical file one mutation rewrites, measured while its growth was reserved.
///
/// Reserving already stats each file it is about to change, so the resulting sizes are the
/// mutation's exact accounting delta. Carrying them to the accounting call is what keeps
/// `scan_usage` an open-time function instead of a per-write tree walk.
#[derive(Debug)]
pub(crate) struct PlannedMutation {
    changes: Vec<PlannedChange>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct OperationEvidence {
    pub(crate) operations: u64,
    pub(crate) syncs: u64,
    pub(crate) quota_denials: u64,
    pub(crate) read_bytes: u64,
    pub(crate) write_bytes: u64,
}

/// One invocation holding the namespace lease; mutations apply during each call.
pub struct StorageHandle {
    pub(crate) interface: StorageInterface,
    pub(crate) access: StorageAccess,
    pub(crate) namespace: Namespace,
    pub(crate) limits: crate::StorageLimits,
    pub(crate) key: Arc<StorageKey>,
    pub(crate) ledger: Arc<QuotaLedger>,
    pub(crate) entries: BTreeMap<String, FileEntry>,
    /// What the namespace tree holds, carried forward by every mutation this invocation applies.
    usage: Usage,
    pub(crate) baseline_files: BTreeSet<String>,
    pub(crate) handles: BTreeMap<u64, HandleState>,
    pub(crate) pending_delete: BTreeSet<String>,
    pub(crate) next_handle: u64,
    pub(crate) next_file_identity: u64,
    pub(crate) host_calls: u64,
    pub(crate) read_bytes: u64,
    // Successful JSONL working-copy loads only; writes have their own budget. Never refunded.
    native_loaded_bytes: u64,
    pub(crate) write_bytes: u64,
    pub(crate) entropy_bytes: u64,
    pub(crate) evidence: OperationEvidence,
    reservation: Option<Reservation>,
    lease: Option<File>,
    finalized: bool,
    failed: bool,
}

impl std::fmt::Debug for StorageHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StorageHandle([REDACTED])")
    }
}

impl StorageHandle {
    pub(crate) fn begin(
        grant: StorageGrant,
        ledger: Arc<QuotaLedger>,
    ) -> Result<Self, StorageHostError> {
        // `Namespace::resolve` already holds the base lease. This is the one defined lock order.
        let lease = grant
            .namespace
            .directory
            .open_private("lease.lock", false)?;
        lock_exclusive(&lease, grant.limits.lock_timeout_ms)?;

        // The one tree walk of this invocation. Every mutation below carries its own delta
        // forward instead of asking the tree again.
        let usage = usage_with_directory_entry(scan_usage(
            &grant.namespace.directory,
            grant.limits.startup_max_entries,
        )?)?;
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
        if usage.bytes > grant.limits.max_namespace_bytes {
            return Err(StorageHostError::QuotaExceeded);
        }
        let reservation = ledger.begin(
            format!(
                "{}/{}",
                grant.namespace.base_token, grant.namespace.generation_token
            ),
            usage,
        )?;
        Ok(Self {
            interface: grant.interface,
            access: grant.access,
            namespace: grant.namespace,
            limits: grant.limits,
            key: grant.key,
            ledger,
            entries: BTreeMap::new(),
            usage,
            baseline_files,
            handles: BTreeMap::new(),
            pending_delete: BTreeSet::new(),
            next_handle: 1,
            next_file_identity: 1,
            host_calls: 0,
            read_bytes: 0,
            native_loaded_bytes: 0,
            write_bytes: 0,
            entropy_bytes: 0,
            evidence: OperationEvidence::default(),
            reservation: Some(reservation),
            lease: Some(lease),
            finalized: false,
            failed: false,
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
        if self.failed {
            return Err(StorageHostError::Io);
        }
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

    /// Loads only trusted metadata. Size, stat, and every durable-file mutation therefore cannot
    /// allocate the complete file.
    pub(crate) fn ensure_entry(&mut self, name: &str) -> Result<String, StorageHostError> {
        let token = self.logical_token(name)?;
        if !self.entries.contains_key(&token) {
            let metadata = self.namespace.data_directory.metadata(&token)?;
            let (exists, size) = match metadata {
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
            let identity = if exists {
                self.allocate_file_identity()?
            } else {
                0
            };
            self.entries.insert(
                token.clone(),
                FileEntry {
                    data: None,
                    loaded: !exists,
                    exists,
                    size,
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
            .checked_add(entry.size)
            .ok_or(StorageHostError::Arithmetic)?;
        if loaded > self.limits.max_read_bytes_per_invocation {
            self.note_quota_denial();
            return Err(StorageHostError::QuotaExceeded);
        }
        let bytes = self
            .namespace
            .data_directory
            .read_bounded(token, self.limits.max_file_bytes)?;
        if bytes.len() as u64 != entry.size {
            return Err(StorageHostError::Corrupt {
                scope: "logical-size-race",
            });
        }
        let entry = self.entries.get_mut(token).expect("entry checked above");
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

    // Reserve growth before touching private data, including concurrent root users. Each change
    // names the length its file ends at, never its contents: a positional write knows that length
    // from the file's stat-derived size and never assembles the whole file to measure it.
    pub(crate) fn reserve_candidate(
        &mut self,
        changes: &[(&str, Option<u64>)],
    ) -> Result<PlannedMutation, StorageHostError> {
        let mut files = self.baseline_files.clone();
        let mut growth = 0u64;
        let mut entries = 0u64;
        let mut planned = Vec::with_capacity(changes.len());
        for (token, length) in changes {
            let before = self
                .namespace
                .data_directory
                .metadata(token)?
                .map(|metadata| metadata.len);
            let after = match *length {
                Some(length) => {
                    if length > self.limits.max_file_bytes {
                        return Err(StorageHostError::QuotaExceeded);
                    }
                    files.insert((*token).to_owned());
                    let added = length.saturating_sub(before.unwrap_or(0));
                    growth = growth
                        .checked_add(added)
                        .ok_or(StorageHostError::Arithmetic)?;
                    if before.is_none() {
                        entries += 1;
                        growth = growth
                            .checked_add(ENTRY_CHARGE)
                            .ok_or(StorageHostError::Arithmetic)?;
                    }
                    Some(length)
                }
                None => {
                    files.remove(*token);
                    None
                }
            };
            planned.push(PlannedChange {
                token: (*token).to_owned(),
                before,
                after,
            });
        }
        if files.len() as u64 > self.limits.max_files_per_namespace {
            return Err(StorageHostError::QuotaExceeded);
        }
        let result = self
            .reservation
            .as_mut()
            .ok_or(StorageHostError::Io)?
            .reserve_to(growth, entries);
        if matches!(result, Err(StorageHostError::QuotaExceeded)) {
            self.note_quota_denial();
        }
        result?;
        Ok(PlannedMutation { changes: planned })
    }

    pub(crate) fn write_direct(
        &mut self,
        token: &str,
        bytes: Option<&[u8]>,
        planned: PlannedMutation,
    ) -> Result<(), StorageHostError> {
        let directory = &self.namespace.data_directory;
        let result = match bytes {
            Some(bytes) => (|| {
                let mut file = directory.open_private(token, true)?;
                file.write_all(bytes)
                    .map_err(|source| directory.io_error(source))?;
                file.set_len(bytes.len() as u64)
                    .map_err(|source| directory.io_error(source))
            })(),
            None => directory.remove_file(token),
        };
        self.after_mutation(result, planned)
    }

    pub(crate) fn write_range(
        &mut self,
        token: &str,
        offset: u64,
        bytes: &[u8],
        planned: PlannedMutation,
    ) -> Result<(), StorageHostError> {
        use std::os::unix::fs::FileExt as _;
        let directory = &self.namespace.data_directory;
        let result = (|| {
            let file = directory.open_private(token, false)?;
            file.write_all_at(bytes, offset)
                .map_err(|source| directory.io_error(source))
        })();
        self.after_mutation(result, planned)
    }

    pub(crate) fn truncate_direct(
        &mut self,
        token: &str,
        size: u64,
        planned: PlannedMutation,
    ) -> Result<(), StorageHostError> {
        let directory = &self.namespace.data_directory;
        let result = (|| {
            directory
                .open_private(token, false)?
                .set_len(size)
                .map_err(|source| directory.io_error(source))
        })();
        self.after_mutation(result, planned)
    }

    pub(crate) fn after_mutation(
        &mut self,
        result: Result<(), StorageHostError>,
        planned: PlannedMutation,
    ) -> Result<(), StorageHostError> {
        // Account actual usage even after a partial syscall; no rollback or deferred write.
        let accounting = if result.is_ok() {
            self.account_planned(&planned)
        } else {
            self.account_observed(&planned)
        };
        if accounting.is_err()
            && let Some(reservation) = self.reservation.take()
        {
            reservation.retain_after_unknown();
        }
        let result = result.and(accounting);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Charges the sizes the reservation planned, which every syscall of the mutation produced.
    fn account_planned(&mut self, planned: &PlannedMutation) -> Result<(), StorageHostError> {
        let final_sizes = planned
            .changes
            .iter()
            .map(|change| change.after)
            .collect::<Vec<_>>();
        self.account_final_sizes(planned, &final_sizes)
    }

    /// Charges what a failed mutation actually left behind, at one `statat` per file it touched.
    ///
    /// A short `write_all_at` or a refused `set_len` leaves a length neither the plan nor the
    /// previous state predicts. Re-stating those files is the complete correction; the namespace
    /// tree is never walked here. A stat that fails leaves the length unknown, and the caller
    /// then retains the reservation rather than releasing bytes that may still be occupied.
    fn account_observed(&mut self, planned: &PlannedMutation) -> Result<(), StorageHostError> {
        let mut final_sizes = Vec::with_capacity(planned.changes.len());
        for change in &planned.changes {
            final_sizes.push(
                self.namespace
                    .data_directory
                    .metadata(&change.token)?
                    .map(|metadata| metadata.len),
            );
        }
        self.account_final_sizes(planned, &final_sizes)
    }

    /// Moves the cached usage by each file's own delta and reports the new total to the ledger.
    fn account_final_sizes(
        &mut self,
        planned: &PlannedMutation,
        final_sizes: &[Option<u64>],
    ) -> Result<(), StorageHostError> {
        let mut usage = self.usage;
        for (change, after) in planned.changes.iter().zip(final_sizes) {
            match (change.before, *after) {
                (None, None) => {}
                (None, Some(length)) => {
                    usage.entries = usage
                        .entries
                        .checked_add(1)
                        .ok_or(StorageHostError::Arithmetic)?;
                    usage.files = usage
                        .files
                        .checked_add(1)
                        .ok_or(StorageHostError::Arithmetic)?;
                    usage.bytes = usage
                        .bytes
                        .checked_add(ENTRY_CHARGE)
                        .and_then(|bytes| bytes.checked_add(length))
                        .ok_or(StorageHostError::Arithmetic)?;
                }
                (Some(length), None) => {
                    usage.entries = usage
                        .entries
                        .checked_sub(1)
                        .ok_or(StorageHostError::Arithmetic)?;
                    usage.files = usage
                        .files
                        .checked_sub(1)
                        .ok_or(StorageHostError::Arithmetic)?;
                    usage.bytes = usage
                        .bytes
                        .checked_sub(ENTRY_CHARGE)
                        .and_then(|bytes| bytes.checked_sub(length))
                        .ok_or(StorageHostError::Arithmetic)?;
                }
                (Some(previous), Some(length)) => {
                    usage.bytes = usage
                        .bytes
                        .checked_sub(previous)
                        .and_then(|bytes| bytes.checked_add(length))
                        .ok_or(StorageHostError::Arithmetic)?;
                }
            }
        }
        self.usage = usage;
        for (change, after) in planned.changes.iter().zip(final_sizes) {
            if after.is_some() {
                self.baseline_files.insert(change.token.clone());
            } else {
                self.baseline_files.remove(&change.token);
            }
        }
        self.reservation
            .as_mut()
            .ok_or(StorageHostError::Io)?
            .observe_direct(usage)
    }

    /// Releases resources, retaining every write already applied by this invocation.
    pub fn abort(mut self) -> StorageEvidence {
        self.close_all_handles();
        if let Some(reservation) = self.reservation.take() {
            reservation.abort();
        }
        self.finalized = true;
        self.lease.take();
        self.make_evidence()
    }
    /// Closes an invocation after proving guest handles were released.
    pub fn finish_read(self) -> Result<StorageEvidence, StorageHostError> {
        if self.failed {
            return Err(StorageHostError::Io);
        }
        if !self.handles.is_empty() {
            return Err(StorageHostError::Busy);
        }
        Ok(self.abort())
    }
    #[must_use]
    pub fn finalization_budget(&self) -> Duration {
        Duration::from_millis(self.limits.finalization_budget_ms)
    }
    /// Closes an invocation; writes have already taken effect.
    pub fn commit(self) -> Result<StorageEvidence, StorageHostError> {
        self.finish_read()
    }
    /// Closes against the adapter's resource-drain deadline, without applying any writes.
    pub fn commit_before(self, deadline: Instant) -> Result<StorageEvidence, StorageHostError> {
        if Instant::now() >= deadline {
            return Err(StorageHostError::Timeout);
        }
        self.finish_read()
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

impl Drop for StorageHandle {
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

pub(crate) fn monotonic_ns() -> Result<u64, StorageHostError> {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    u64::try_from(START.get_or_init(Instant::now).elapsed().as_nanos())
        .or(Err(StorageHostError::Arithmetic))
}
pub(crate) fn wall_ms() -> Result<u64, StorageHostError> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .or(Err(StorageHostError::Clock))?
            .as_millis(),
    )
    .or(Err(StorageHostError::Arithmetic))
}

#[cfg(test)]
mod tests {
    use crate::{
        ContinuityPolicy, Durability, OpenOptions, StorageGrantRequest, StorageHandle, StorageHost,
        StorageLimits,
        layout::{scan_usage, usage_with_directory_entry},
    };
    use dekopon_capability::{StorageAccess, StorageInterface, StorageNamespace};
    use std::{fs, os::unix::fs::PermissionsExt as _};
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

    fn vfs_transaction(host: &StorageHost, invocation: &str) -> StorageHandle {
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

    #[test]
    fn writes_are_visible_before_finalization_and_survive_abort() {
        let (_temporary, host) = probe_host(StorageLimits::default());
        let mut invocation = vfs_transaction(&host, "direct-write");
        let handle = invocation
            .vfs_open(
                "direct.db",
                OpenOptions {
                    read: true,
                    write: true,
                    create: true,
                    ..OpenOptions::default()
                },
            )
            .expect("open");
        invocation
            .vfs_write_at(handle, 0, b"visible")
            .expect("write");
        let token = invocation.logical_token("direct.db").expect("token");
        assert_eq!(
            invocation
                .namespace
                .data_directory
                .read_bounded(&token, 64)
                .expect("native read"),
            b"visible"
        );
        invocation.abort();
        let mut next = vfs_transaction(&host, "after-abort");
        let handle = next
            .vfs_open(
                "direct.db",
                OpenOptions {
                    read: true,
                    ..OpenOptions::default()
                },
            )
            .expect("reopen");
        assert_eq!(next.vfs_read_at(handle, 0, 64).expect("read"), b"visible");
        next.vfs_close(handle).expect("close");
        next.finish_read().expect("finish");
    }

    #[test]
    fn a_mutation_reads_no_directory_and_leaves_the_cached_usage_exact() {
        /// Directory reads a mutation may perform, whatever the namespace holds and however many
        /// mutations came before it: none. Accounting carries each change's own delta forward, so
        /// the tree walk `begin` performs is the only one an invocation makes.
        const SCANS_ALLOWED: u64 = 0;
        const FILES: u64 = 8;
        const FRAMES: u64 = 64;
        const FRAME: u64 = 512;

        let (_temporary, host) = probe_host(StorageLimits::default());
        let mut transaction = vfs_transaction(&host, "incremental-accounting");
        let opened = crate::layout::directory_scans();
        assert!(opened > 0, "directory-scan instrumentation is dead");
        let mut handles = Vec::new();
        for index in 0..FILES {
            handles.push(
                transaction
                    .vfs_open(
                        &format!("shard{index}.db"),
                        OpenOptions {
                            read: true,
                            write: true,
                            create: true,
                            ..OpenOptions::default()
                        },
                    )
                    .expect("open"),
            );
        }
        let created = crate::layout::directory_scans();
        assert_eq!(
            created - opened,
            SCANS_ALLOWED,
            "creating {FILES} files read the namespace"
        );

        for handle in handles.drain(1..) {
            transaction.vfs_close(handle).expect("close");
        }
        let frame = vec![0x5a_u8; usize::try_from(FRAME).expect("bounded frame")];
        for index in 0..FRAMES {
            transaction
                .vfs_write_at(handles[0], index * FRAME, &frame)
                .expect("append");
        }
        let appended = crate::layout::directory_scans();
        assert_eq!(
            appended - created,
            SCANS_ALLOWED,
            "{FRAMES} appends read the namespace: accounting is rescanning the tree per write"
        );

        transaction
            .vfs_rename_atomic("shard1.db", "shard1.moved.db", true, Durability::Full)
            .expect("rename");
        transaction
            .vfs_remove("shard2.db", Durability::Full)
            .expect("remove");
        assert_eq!(
            crate::layout::directory_scans() - appended,
            SCANS_ALLOWED,
            "a rename and a removal read the namespace"
        );

        let walked = usage_with_directory_entry(
            scan_usage(
                &transaction.namespace.directory,
                transaction.limits.startup_max_entries,
            )
            .expect("ground truth"),
        )
        .expect("ground truth");
        assert_eq!(
            transaction.usage, walked,
            "the usage carried across {FILES} creations, {FRAMES} appends, a rename, and a \
             removal disagrees with the tree it describes"
        );

        transaction.vfs_close(handles[0]).expect("close");
        transaction.commit().expect("finish");
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
        // roughly 2 GiB here. Reserving direct growth must not hash the file again.
        assert!(
            hashed <= written,
            "{FRAMES} appends totalling {written} bytes hashed {hashed} bytes: \
             the reservation path is committing to complete file contents"
        );

        transaction.vfs_close(handle).expect("close");
        transaction.commit().expect("finish");
    }
}
