use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use dekopon_storage_host::{
    Durability, FileStat, LockLevel, OpenOptions, StorageEvidence, StorageHandle, StorageHostError,
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
pub(crate) struct ActiveStorage {
    /// Every storage effect holds this lock; finish acquiring it drains a cancelled job, which
    /// either still holds the lock and makes finish wait, or never started.
    transaction: Arc<Mutex<Option<StorageHandle>>>,
    finalization_budget: Duration,
}

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
    pub(crate) fn active(transaction: StorageHandle) -> Self {
        let finalization_budget = transaction.finalization_budget();
        Self::Active {
            active: ActiveStorage {
                transaction: Arc::new(Mutex::new(Some(transaction))),
                finalization_budget,
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
        F: FnOnce(&mut StorageHandle) -> Result<R, StorageHostError> + Send + 'static,
    {
        let active = match self {
            Self::Disabled { attempted } => {
                *attempted = true;
                return Err(StorageHostError::PermissionDenied);
            }
            Self::Active { active, .. } => active,
        };
        let transaction = Arc::clone(&active.transaction);
        let result = match tokio::task::spawn_blocking(move || {
            let mut slot = transaction.lock().expect("storage transaction");
            let transaction = slot
                .as_mut()
                .ok_or(StorageHostError::corrupt("finalized-transaction"))?;
            operation(transaction)
        })
        .await
        {
            Ok(result) => result,
            // A worker panic or cancellation must still pass through the terminal-state path below;
            // returning early would let the guest report success despite the failure.
            Err(_) => Err(StorageHostError::Io),
        };
        if let Err(error) = &result
            && terminal(error)
            && let Self::Active { violation, .. } = self
        {
            *violation = Some(error.class().label());
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
        let transaction = Arc::clone(&active.transaction);
        let rejected = violation.is_some();
        let deadline = Instant::now()
            .checked_add(active.finalization_budget)
            .ok_or(StorageHostError::Arithmetic)?;
        #[allow(
            clippy::map_err_ignore,
            reason = "a `JoinError` from `spawn_blocking` distinguishes only a panic from runtime \
                      cancellation, and the panic hook has already printed the panic with its \
                      location; `call` above classes the same failure as `Io` for the same reason"
        )]
        let finished = tokio::task::spawn_blocking(move || {
            let transaction = transaction
                .lock()
                .expect("storage transaction")
                .take()
                .ok_or(StorageHostError::corrupt("finalized-transaction"))?;
            if commit && !rejected && Instant::now() >= deadline {
                transaction.abort();
                return Err(StorageHostError::Timeout);
            }
            let output_commitment = output
                .as_deref()
                .map(|bytes| transaction.output_commitment(bytes));
            if commit && !rejected && Instant::now() >= deadline {
                transaction.abort();
                return Err(StorageHostError::Timeout);
            }
            let mut result = if !commit || rejected {
                transaction.abort()
            } else if transaction.access() == dekopon_capability::StorageAccess::ReadOnly {
                transaction.finish_read()?
            } else {
                transaction.commit_before(deadline)?
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
            #[allow(
                clippy::map_err_ignore,
                reason = "`ResourceTable::push` fails only with `ResourceTableError::Full`, so the \
                          discarded value is single-valued and adds nothing to the guest's `Io`"
            )]
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
            .call(StorageHandle::vfs_monotonic_time_ns)
            .await
            .map_err(map_durable_error))
    }

    async fn wall_time_ms(&mut self) -> wasmtime::Result<Result<u64, durable::StorageError>> {
        Ok(self
            .storage
            .call(StorageHandle::vfs_wall_time_ms)
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
        StorageHostError::Corrupt { .. } | StorageHostError::CorruptLayout { .. } => {
            jsonl::StorageError::Corrupt
        }
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
        StorageHostError::Corrupt { .. } | StorageHostError::CorruptLayout { .. } => {
            durable::StorageError::Corrupt
        }
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
#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use dekopon_capability::{StorageAccess, StorageInterface, StorageNamespace};
    use dekopon_storage_host::{
        ContinuityPolicy, StorageGrantRequest, StorageHost, StorageHostError, StorageLimits,
    };

    use super::StorageState;

    fn request(invocation: &str) -> StorageGrantRequest {
        StorageGrantRequest::new(
            invocation.parse().expect("invocation"),
            "storage-probe.run".parse().expect("capability"),
            "storage-probe".parse().expect("provider"),
            StorageInterface::DurableFiles,
            StorageAccess::ReadWrite,
            StorageNamespace::Chat,
            "provider-test".parse().expect("agent"),
            "slack.t0123abc.u9xyz".parse().expect("subject"),
            "slack",
            "probe-transport",
            "c0123abc",
            "c0123abc:1712345678.000100",
            ContinuityPolicy::Stable,
            b"probe-authority".to_vec(),
        )
    }

    fn jsonl_request(invocation: &str, access: StorageAccess) -> StorageGrantRequest {
        StorageGrantRequest::new(
            invocation.parse().expect("invocation"),
            "memory.chat.record".parse().expect("capability"),
            "memory-chat".parse().expect("provider"),
            StorageInterface::Jsonl,
            access,
            StorageNamespace::Chat,
            "provider-test".parse().expect("agent"),
            "slack.t0123abc.u9xyz".parse().expect("subject"),
            "slack",
            "probe-transport",
            "c0123abc",
            "c0123abc:1712345678.000100",
            ContinuityPolicy::Stable,
            b"probe-authority".to_vec(),
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn finalization_budget_includes_draining_an_already_started_host_job() {
        let directory = tempfile::tempdir().expect("storage directory");
        let directory = directory
            .path()
            .canonicalize()
            .expect("canonical directory");
        let root = directory.join("root");
        let host = StorageHost::open(
            &root,
            StorageLimits {
                finalization_budget_ms: 30,
                ..StorageLimits::default()
            },
        )
        .expect("host");
        let mut transaction = host
            .begin(
                host.grant(jsonl_request("drain-budget", StorageAccess::ReadWrite))
                    .expect("grant"),
            )
            .expect("transaction");
        transaction
            .jsonl_append("turns.jsonl", 0, br#"{"provisional":true}"#)
            .expect("provisional append");
        let mut state = StorageState::active(transaction);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(5),
                state.call(move |_transaction| {
                    started_tx.send(()).expect("test is listening");
                    std::thread::sleep(Duration::from_millis(100));
                    Ok(())
                }),
            )
            .await
            .is_err(),
            "the host-call future should be cancelled while its blocking job drains"
        );
        started_rx
            .recv()
            .expect("the orphaned job holds the transaction");
        let started = Instant::now();
        assert!(matches!(
            state.finish(true, None).await,
            Err(StorageHostError::Timeout)
        ));
        assert!(started.elapsed() >= Duration::from_millis(70));

        let mut reader = host
            .begin(
                host.grant(jsonl_request("drain-budget-read", StorageAccess::ReadOnly))
                    .expect("read grant"),
            )
            .expect("reader");
        let expected = b"{\"provisional\":true}\n";
        assert_eq!(
            reader.jsonl_size("turns.jsonl").expect("persisted size"),
            expected.len() as u64
        );
        let chunk = reader
            .jsonl_read_chunk("turns.jsonl", 0, 1024)
            .expect("persisted record");
        assert_eq!(chunk.bytes, expected);
        assert_eq!(chunk.next_offset, expected.len() as u64);
        assert!(chunk.eof);
        reader.finish_read().expect("finish reader");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_a_host_call_drains_its_blocking_job_before_releasing_the_lease() {
        let directory = tempfile::tempdir().expect("storage directory");
        let directory = directory
            .path()
            .canonicalize()
            .expect("canonical directory");
        let root = directory.join("root");
        let host = StorageHost::open(
            &root,
            StorageLimits {
                lock_timeout_ms: 1_000,
                ..StorageLimits::default()
            },
        )
        .expect("host");
        let transaction = host
            .begin(host.grant(request("cancel-held")).expect("grant"))
            .expect("transaction");
        let (started_send, started_receive) = tokio::sync::oneshot::channel();
        let operation = tokio::spawn(async move {
            let mut state = StorageState::active(transaction);
            state
                .call(move |_transaction| {
                    #[allow(
                        clippy::let_underscore_must_use,
                        reason = "the oneshot only unblocks the test below; a dropped receiver \
                                  means the test already failed on `started_receive`"
                    )]
                    let _ = started_send.send(());
                    std::thread::sleep(Duration::from_millis(150));
                    Ok(())
                })
                .await
        });
        started_receive.await.expect("blocking operation started");
        operation.abort();

        let competing_host = host.clone();
        let competing =
            tokio::task::spawn_blocking(move || competing_host.grant(request("cancel-competing")));
        assert!(
            tokio::time::timeout(Duration::from_millis(40), competing)
                .await
                .is_err(),
            "cancellation released a lease while its native job was still running"
        );
        tokio::time::sleep(Duration::from_millis(180)).await;
        let followup_host = host.clone();
        let followup =
            tokio::task::spawn_blocking(move || followup_host.grant(request("cancel-followup")));
        tokio::time::timeout(Duration::from_secs(2), followup)
            .await
            .expect("drained job retained the lease forever")
            .expect("blocking task")
            .expect("followup grant");
    }
}
