//! Wasmtime adapters for the two exact storage interfaces.

use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use dekopon_storage_host::{
    Durability, FileStat, LockLevel, OpenOptions, StorageEvidence, StorageHostError,
    StorageTransaction,
};
use wasmtime::component::{Resource, ResourceTable};

use crate::{StoreState, bindings};

use bindings::dekopon::storage::{durable_files as durable, jsonl};

#[doc(hidden)]
#[derive(Debug)]
pub struct FileResource {
    handle: u64,
}

#[derive(Debug)]
struct ActiveJobs {
    count: AtomicUsize,
    mutex: Mutex<()>,
    drained: Condvar,
}

impl ActiveJobs {
    fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            mutex: Mutex::new(()),
            drained: Condvar::new(),
        }
    }
    fn enter(self: &Arc<Self>) -> JobGuard {
        self.count.fetch_add(1, Ordering::AcqRel);
        JobGuard(Arc::clone(self))
    }
    fn wait(&self) {
        let mut guard = self.mutex.lock().expect("storage job drain");
        while self.count.load(Ordering::Acquire) != 0 {
            let (next, _) = self
                .drained
                .wait_timeout(guard, Duration::from_millis(25))
                .expect("storage job drain");
            guard = next;
        }
    }
}

struct JobGuard(Arc<ActiveJobs>);
impl Drop for JobGuard {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.drained.notify_all();
        }
    }
}

#[derive(Debug)]
pub(crate) struct ActiveStorage {
    transaction: Arc<Mutex<Option<StorageTransaction>>>,
    jobs: Arc<ActiveJobs>,
}

/// Per-store storage context. A disabled context is still linked so imports can be diagnosed.
#[derive(Debug)]
pub(crate) enum StorageState {
    Disabled {
        attempted: bool,
    },
    Active {
        active: ActiveStorage,
        violation: Option<&'static str>,
        evidence: Option<StorageEvidence>,
    },
}

impl StorageState {
    pub(crate) const fn disabled() -> Self {
        Self::Disabled { attempted: false }
    }
    pub(crate) fn active(transaction: StorageTransaction) -> Self {
        Self::Active {
            active: ActiveStorage {
                transaction: Arc::new(Mutex::new(Some(transaction))),
                jobs: Arc::new(ActiveJobs::new()),
            },
            violation: None,
            evidence: None,
        }
    }
    pub(crate) const fn attempted(&self) -> bool {
        match self {
            Self::Disabled { attempted } => *attempted,
            Self::Active { .. } => false,
        }
    }
    pub(crate) const fn violation(&self) -> Option<&'static str> {
        match self {
            Self::Active { violation, .. } => *violation,
            Self::Disabled { attempted: true } => Some("disabled"),
            Self::Disabled { attempted: false } => None,
        }
    }

    async fn call<R, F>(&mut self, operation: F) -> Result<R, StorageHostError>
    where
        R: Send + 'static,
        F: FnOnce(&mut StorageTransaction) -> Result<R, StorageHostError> + Send + 'static,
    {
        let active = match self {
            Self::Disabled { attempted } => {
                *attempted = true;
                return Err(StorageHostError::PermissionDenied);
            }
            Self::Active { active, .. } => active,
        };
        let transaction = Arc::clone(&active.transaction);
        let job = active.jobs.enter();
        let result = match tokio::task::spawn_blocking(move || {
            let _job = job;
            let mut slot = transaction.lock().expect("storage transaction");
            let transaction = slot.as_mut().ok_or(StorageHostError::Corrupt {
                scope: "finalized-transaction",
            })?;
            operation(transaction)
        })
        .await
        {
            Ok(result) => result,
            // A blocking worker panic/cancellation is an internal I/O-class failure, but it must
            // still pass through the terminal-state path below. Returning early here would let a
            // guest catch the mapped WIT error and commit earlier writes after the host job failed.
            Err(_) => Err(StorageHostError::Io),
        };
        if let Err(error) = &result
            && terminal(error)
            && let Self::Active { violation, .. } = self
        {
            *violation = Some(public_reason(error));
        }
        result
    }

    pub(crate) async fn finish(
        &mut self,
        commit: bool,
        output: Option<Vec<u8>>,
    ) -> Result<Option<StorageEvidence>, StorageHostError> {
        let Self::Active {
            active,
            violation,
            evidence,
        } = self
        else {
            return Ok(None);
        };
        let active_jobs = Arc::clone(&active.jobs);
        let transaction = Arc::clone(&active.transaction);
        let rejected = violation.is_some();
        let finished = tokio::task::spawn_blocking(move || {
            active_jobs.wait();
            let transaction = transaction
                .lock()
                .expect("storage transaction")
                .take()
                .ok_or(StorageHostError::Corrupt {
                    scope: "finalized-transaction",
                })?;
            let output_commitment = output
                .as_deref()
                .map(|bytes| transaction.output_commitment(bytes));
            let mut result = if !commit || rejected {
                transaction.abort()
            } else if transaction.access() == dekopon_capability::StorageAccess::ReadOnly {
                transaction.finish_read()?
            } else {
                transaction.commit()?
            };
            result.output_commitment = output_commitment;
            Ok::<StorageEvidence, StorageHostError>(result)
        })
        .await
        .map_err(|_| StorageHostError::Io)??;
        *evidence = Some(finished.clone());
        Ok(Some(finished))
    }

    pub(crate) fn take_evidence(&mut self) -> Option<StorageEvidence> {
        match self {
            Self::Active { evidence, .. } => evidence.take(),
            Self::Disabled { .. } => None,
        }
    }
}

impl jsonl::Host for StoreState {
    async fn size(&mut self, name: String) -> wasmtime::Result<Result<u64, jsonl::StorageError>> {
        Ok(self
            .storage
            .call(move |tx| tx.jsonl_size(&name))
            .await
            .map_err(map_jsonl_error))
    }

    async fn read_chunk(
        &mut self,
        name: String,
        offset: u64,
        max_bytes: u32,
    ) -> wasmtime::Result<Result<jsonl::Chunk, jsonl::StorageError>> {
        Ok(self
            .storage
            .call(move |tx| tx.jsonl_read_chunk(&name, offset, max_bytes))
            .await
            .map(|chunk| jsonl::Chunk {
                bytes: chunk.bytes,
                next_offset: chunk.next_offset,
                eof: chunk.eof,
            })
            .map_err(map_jsonl_error))
    }

    async fn append(
        &mut self,
        name: String,
        expected_size: u64,
        record: Vec<u8>,
    ) -> wasmtime::Result<Result<u64, jsonl::StorageError>> {
        Ok(self
            .storage
            .call(move |tx| tx.jsonl_append(&name, expected_size, &record))
            .await
            .map_err(map_jsonl_error))
    }

    async fn replace(
        &mut self,
        name: String,
        expected_size: u64,
        contents: Vec<u8>,
    ) -> wasmtime::Result<Result<(), jsonl::StorageError>> {
        Ok(self
            .storage
            .call(move |tx| tx.jsonl_replace(&name, expected_size, &contents))
            .await
            .map_err(map_jsonl_error))
    }
}

impl durable::HostFile for StoreState {
    async fn read_at(
        &mut self,
        file: Resource<FileResource>,
        offset: u64,
        max_bytes: u32,
    ) -> wasmtime::Result<Result<Vec<u8>, durable::StorageError>> {
        let handle = self.table.get(&file)?.handle;
        Ok(self
            .storage
            .call(move |tx| tx.vfs_read_at(handle, offset, max_bytes))
            .await
            .map_err(map_durable_error))
    }

    async fn write_at(
        &mut self,
        file: Resource<FileResource>,
        offset: u64,
        bytes: Vec<u8>,
    ) -> wasmtime::Result<Result<(), durable::StorageError>> {
        let handle = self.table.get(&file)?.handle;
        Ok(self
            .storage
            .call(move |tx| tx.vfs_write_at(handle, offset, &bytes))
            .await
            .map_err(map_durable_error))
    }

    async fn size(
        &mut self,
        file: Resource<FileResource>,
    ) -> wasmtime::Result<Result<u64, durable::StorageError>> {
        let handle = self.table.get(&file)?.handle;
        Ok(self
            .storage
            .call(move |tx| tx.vfs_size(handle))
            .await
            .map_err(map_durable_error))
    }

    async fn truncate(
        &mut self,
        file: Resource<FileResource>,
        size: u64,
    ) -> wasmtime::Result<Result<(), durable::StorageError>> {
        let handle = self.table.get(&file)?.handle;
        Ok(self
            .storage
            .call(move |tx| tx.vfs_truncate(handle, size))
            .await
            .map_err(map_durable_error))
    }

    async fn sync(
        &mut self,
        file: Resource<FileResource>,
        mode: durable::Durability,
    ) -> wasmtime::Result<Result<(), durable::StorageError>> {
        let handle = self.table.get(&file)?.handle;
        Ok(self
            .storage
            .call(move |tx| tx.vfs_sync(handle, map_durability(mode)))
            .await
            .map_err(map_durable_error))
    }

    async fn lock(
        &mut self,
        file: Resource<FileResource>,
        level: durable::LockLevel,
    ) -> wasmtime::Result<Result<(), durable::StorageError>> {
        let handle = self.table.get(&file)?.handle;
        Ok(self
            .storage
            .call(move |tx| tx.vfs_lock(handle, map_lock(level)))
            .await
            .map_err(map_durable_error))
    }

    async fn unlock(
        &mut self,
        file: Resource<FileResource>,
        to: durable::LockLevel,
    ) -> wasmtime::Result<Result<(), durable::StorageError>> {
        let handle = self.table.get(&file)?.handle;
        Ok(self
            .storage
            .call(move |tx| tx.vfs_unlock(handle, map_lock(to)))
            .await
            .map_err(map_durable_error))
    }

    async fn check_reserved_lock(
        &mut self,
        file: Resource<FileResource>,
    ) -> wasmtime::Result<Result<bool, durable::StorageError>> {
        let handle = self.table.get(&file)?.handle;
        Ok(self
            .storage
            .call(move |tx| tx.vfs_check_reserved_lock(handle))
            .await
            .map_err(map_durable_error))
    }

    async fn drop(&mut self, file: Resource<FileResource>) -> wasmtime::Result<()> {
        let resource = self.table.delete(file)?;
        self.storage
            .call(move |tx| tx.vfs_close(resource.handle))
            .await
            .map_err(wasmtime::Error::new)
    }
}

impl durable::Host for StoreState {
    async fn open(
        &mut self,
        name: String,
        flags: durable::OpenFlags,
    ) -> wasmtime::Result<Result<Resource<FileResource>, durable::StorageError>> {
        let options = OpenOptions {
            read: flags.contains(durable::OpenFlags::READ),
            write: flags.contains(durable::OpenFlags::WRITE),
            create: flags.contains(durable::OpenFlags::CREATE),
            create_new: flags.contains(durable::OpenFlags::CREATE_NEW),
            delete_on_close: flags.contains(durable::OpenFlags::DELETE_ON_CLOSE),
        };
        let opened = self
            .storage
            .call(move |tx| tx.vfs_open(&name, options))
            .await;
        Ok(match opened {
            Ok(handle) => self
                .table
                .push(FileResource { handle })
                .map_err(wasmtime::Error::new)
                .map_err(|_| durable::StorageError::Io),
            Err(error) => Err(map_durable_error(error)),
        })
    }

    async fn stat(
        &mut self,
        name: String,
    ) -> wasmtime::Result<Result<Option<durable::FileStat>, durable::StorageError>> {
        Ok(self
            .storage
            .call(move |tx| tx.vfs_stat(&name))
            .await
            .map(|value| value.map(map_stat))
            .map_err(map_durable_error))
    }

    async fn remove(
        &mut self,
        name: String,
        mode: durable::Durability,
    ) -> wasmtime::Result<Result<(), durable::StorageError>> {
        Ok(self
            .storage
            .call(move |tx| tx.vfs_remove(&name, map_durability(mode)))
            .await
            .map_err(map_durable_error))
    }

    async fn rename_atomic(
        &mut self,
        from: String,
        to: String,
        replace: bool,
        mode: durable::Durability,
    ) -> wasmtime::Result<Result<(), durable::StorageError>> {
        Ok(self
            .storage
            .call(move |tx| tx.vfs_rename_atomic(&from, &to, replace, map_durability(mode)))
            .await
            .map_err(map_durable_error))
    }

    async fn random_bytes(
        &mut self,
        length: u32,
    ) -> wasmtime::Result<Result<Vec<u8>, durable::StorageError>> {
        Ok(self
            .storage
            .call(move |tx| tx.vfs_random_bytes(length))
            .await
            .map_err(map_durable_error))
    }

    async fn monotonic_time_ns(&mut self) -> wasmtime::Result<Result<u64, durable::StorageError>> {
        Ok(self
            .storage
            .call(StorageTransaction::vfs_monotonic_time_ns)
            .await
            .map_err(map_durable_error))
    }

    async fn wall_time_ms(&mut self) -> wasmtime::Result<Result<u64, durable::StorageError>> {
        Ok(self
            .storage
            .call(StorageTransaction::vfs_wall_time_ms)
            .await
            .map_err(map_durable_error))
    }
}

pub(crate) fn new_table() -> ResourceTable {
    ResourceTable::new()
}

fn map_stat(stat: FileStat) -> durable::FileStat {
    durable::FileStat {
        size: stat.size,
        identity: stat.identity,
    }
}
fn map_durability(value: durable::Durability) -> Durability {
    match value {
        durable::Durability::Data => Durability::Data,
        durable::Durability::DataAndMetadata => Durability::DataAndMetadata,
        durable::Durability::Full => Durability::Full,
    }
}
fn map_lock(value: durable::LockLevel) -> LockLevel {
    match value {
        durable::LockLevel::None => LockLevel::None,
        durable::LockLevel::Shared => LockLevel::Shared,
        durable::LockLevel::Reserved => LockLevel::Reserved,
        durable::LockLevel::Pending => LockLevel::Pending,
        durable::LockLevel::Exclusive => LockLevel::Exclusive,
    }
}

fn map_jsonl_error(error: StorageHostError) -> jsonl::StorageError {
    match error {
        StorageHostError::NotFound => jsonl::StorageError::NotFound,
        StorageHostError::AlreadyExists => jsonl::StorageError::AlreadyExists,
        StorageHostError::InvalidName => jsonl::StorageError::InvalidName,
        StorageHostError::InvalidArgument => jsonl::StorageError::InvalidArgument,
        StorageHostError::PermissionDenied | StorageHostError::GrantHostMismatch => {
            jsonl::StorageError::PermissionDenied
        }
        StorageHostError::QuotaExceeded | StorageHostError::Arithmetic => {
            jsonl::StorageError::QuotaExceeded
        }
        StorageHostError::Busy => jsonl::StorageError::Busy,
        StorageHostError::Timeout => jsonl::StorageError::Timeout,
        StorageHostError::Unsupported => jsonl::StorageError::Unsupported,
        StorageHostError::Corrupt { .. }
        | StorageHostError::CorruptLayout
        | StorageHostError::KeyMismatch => jsonl::StorageError::Corrupt,
        _ => jsonl::StorageError::Io,
    }
}
fn map_durable_error(error: StorageHostError) -> durable::StorageError {
    match error {
        StorageHostError::NotFound => durable::StorageError::NotFound,
        StorageHostError::AlreadyExists => durable::StorageError::AlreadyExists,
        StorageHostError::InvalidName => durable::StorageError::InvalidName,
        StorageHostError::InvalidArgument => durable::StorageError::InvalidArgument,
        StorageHostError::PermissionDenied | StorageHostError::GrantHostMismatch => {
            durable::StorageError::PermissionDenied
        }
        StorageHostError::QuotaExceeded | StorageHostError::Arithmetic => {
            durable::StorageError::QuotaExceeded
        }
        StorageHostError::Busy => durable::StorageError::Busy,
        StorageHostError::Timeout => durable::StorageError::Timeout,
        StorageHostError::Unsupported => durable::StorageError::Unsupported,
        StorageHostError::Corrupt { .. }
        | StorageHostError::CorruptLayout
        | StorageHostError::KeyMismatch => durable::StorageError::Corrupt,
        _ => durable::StorageError::Io,
    }
}

fn terminal(error: &StorageHostError) -> bool {
    !matches!(
        error,
        StorageHostError::NotFound
            | StorageHostError::AlreadyExists
            | StorageHostError::InvalidName
            | StorageHostError::InvalidArgument
            | StorageHostError::Busy
    )
}
fn public_reason(error: &StorageHostError) -> &'static str {
    match error {
        StorageHostError::QuotaExceeded | StorageHostError::Arithmetic => "quota",
        StorageHostError::Timeout => "timeout",
        StorageHostError::Corrupt { .. }
        | StorageHostError::CorruptLayout
        | StorageHostError::KeyMismatch => "corrupt",
        StorageHostError::PermissionDenied | StorageHostError::GrantHostMismatch => "denied",
        _ => "io",
    }
}
