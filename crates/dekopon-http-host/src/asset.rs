//! Broker-owned temporary files and a shared in-flight byte budget.

use std::{
    fs::File,
    io::{self, Write as _},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};
use tempfile::TempPath;
use thiserror::Error;

/// File I/O failures safe to classify across the provider boundary.
#[derive(Debug, Error)]
pub enum AssetIoError {
    /// The process-wide in-flight budget or backing filesystem is exhausted.
    #[error("broker asset byte budget is exhausted")]
    OverBudget,
    /// A disk operation failed; no path or file bytes cross the host boundary.
    #[error("broker asset I/O failed ({kind:?})")]
    Io { kind: io::ErrorKind },
    /// A blocking operation could not complete on the runtime.
    #[error("broker asset worker failed")]
    Worker,
}

impl From<io::Error> for AssetIoError {
    fn from(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::StorageFull {
            Self::OverBudget
        } else {
            Self::Io { kind: error.kind() }
        }
    }
}

#[derive(Debug)]
struct Budget {
    maximum: u64,
    used: AtomicU64,
}

#[derive(Debug, Default)]
struct Jobs {
    active: AtomicUsize,
    idle: tokio::sync::Notify,
}

struct Pending(Arc<Jobs>);
impl Drop for Pending {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

// A cancelled waiter's result is dropped before its completion notification: files and budget
// must actually be reclaimed before the invocation's drain can return.
pub(crate) struct Completed<T> {
    pub(crate) value: T,
    _pending: Pending,
}

/// Tracks every filesystem job belonging to one invocation, including its readers.
#[derive(Clone, Debug, Default)]
pub struct AssetJobs(Arc<Jobs>);

impl AssetJobs {
    pub(crate) fn spawn<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> tokio::task::JoinHandle<Completed<T>> {
        self.0.active.fetch_add(1, Ordering::AcqRel);
        let pending = Pending(Arc::clone(&self.0));
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || Completed {
            value: span.in_scope(work),
            _pending: pending,
        })
    }
    /// Runs bounded native work and keeps its cancelled result alive until actually reclaimed.
    pub async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T, AssetIoError> + Send + 'static,
    ) -> Result<T, AssetIoError> {
        self.spawn(work)
            .await
            .map_err(|failure| {
                tracing::error!(error = %failure, "asset worker failed");
                AssetIoError::Worker
            })?
            .value
    }
    /// Waits until every worker and any cancelled result has released its ownership.
    pub async fn drain(&self) {
        loop {
            let idle = self.0.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self.0.active.load(Ordering::Acquire) == 0 {
                return;
            }
            idle.await;
        }
    }
}

/// Shared capacity beneath all active spools and output writers.
#[derive(Clone, Debug)]
pub struct AssetDirectory {
    root: PathBuf,
    budget: Arc<Budget>,
    jobs: AssetJobs,
}

impl AssetDirectory {
    /// Uses a directory already validated and emptied by broker startup.
    pub fn new(root: PathBuf, maximum: u64) -> Self {
        Self {
            root,
            jobs: AssetJobs::default(),
            budget: Arc::new(Budget {
                maximum,
                used: AtomicU64::new(0),
            }),
        }
    }

    /// Shares the byte budget but tracks this invocation's pending filesystem jobs independently.
    pub fn invocation(&self, jobs: AssetJobs) -> Self {
        Self {
            root: self.root.clone(),
            budget: Arc::clone(&self.budget),
            jobs,
        }
    }

    /// Waits for cancelled filesystem jobs to close/unlink their own results, without retrying I/O.
    pub async fn drain(&self) {
        self.jobs.drain().await;
    }

    /// Allocates a private temporary writer without blocking an async runtime worker.
    pub async fn allocate(&self) -> Result<Spool, AssetIoError> {
        let root = self.root.clone();
        let budget = Arc::clone(&self.budget);
        let jobs = self.jobs.clone();
        self.jobs
            .run(move || {
                let (file, path) = tempfile::NamedTempFile::new_in(root)?.into_parts();
                Ok(Spool {
                    sink: Sink::File { fd: file, path },
                    reservation: Reservation {
                        budget,
                        bytes: 0,
                        #[cfg(test)]
                        release_probe: None,
                    },
                    jobs,
                })
            })
            .await
    }
}

#[derive(Debug)]
struct Reservation {
    budget: Arc<Budget>,
    bytes: u64,
    #[cfg(test)]
    release_probe: Option<tests::ReleaseProbe>,
}

impl Reservation {
    fn grow(&mut self, bytes: u64) -> Result<(), AssetIoError> {
        self.budget
            .used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= self.budget.maximum)
            })
            .map_err(|_current| AssetIoError::OverBudget)?;
        self.bytes += bytes;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        #[cfg(test)]
        if let Some(probe) = &self.release_probe {
            probe.verify();
        }
        self.budget.used.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// A private writer. Field order closes the file and unlinks before releasing its accounting.
#[derive(Debug)]
pub struct Spool {
    sink: Sink,
    reservation: Reservation,
    jobs: AssetJobs,
}

#[derive(Debug)]
enum Sink {
    File { fd: File, path: TempPath },
}

impl Spool {
    /// Stored byte count, charged before each write.
    pub fn len(&self) -> u64 {
        self.reservation.bytes
    }

    /// Whether this writer has accepted no bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends bounded chunks; callers enforce their own invocation and asset ceilings.
    pub async fn write(mut self, bytes: Vec<u8>) -> Result<Self, AssetIoError> {
        self.reservation.grow(bytes.len() as u64)?;
        // The worker owns the file AND reservation, including when its awaiting call is cancelled.
        let jobs = self.jobs.clone();
        jobs.run(move || {
            match &mut self.sink {
                Sink::File { fd, .. } => fd.write_all(&bytes)?,
            }
            Ok(self)
        })
        .await
    }

    /// Opens a separate read-only description, closes the writer and unlinks its path.
    pub async fn finish(self) -> Result<AssetFile, AssetIoError> {
        let jobs = self.jobs.clone();
        jobs.run(move || {
            // A failed reopen must drop the intact sink before releasing its reservation.
            let file = match &self.sink {
                Sink::File { path, .. } => File::open(path)?,
            };
            let Self {
                sink: Sink::File { fd: writer, path },
                reservation,
                jobs,
            } = self;
            drop(writer);
            // Preserve close-before-release ordering on the fallible unlink path as well.
            let output = AssetFile {
                file,
                reservation,
                jobs,
            };
            path.close()?;
            Ok(output)
        })
        .await
    }
}

/// An unlinked, read-only file; its budget stays charged until broker ownership ends.
#[derive(Debug)]
pub struct AssetFile {
    file: File,
    reservation: Reservation,
    jobs: AssetJobs,
}

impl AssetFile {
    /// Read-only descriptor used with positional reads exclusively.
    pub fn file(&self) -> &File {
        &self.file
    }
    /// Stored length measured while writing.
    pub fn len(&self) -> u64 {
        self.reservation.bytes
    }
    /// Whether the file stores no bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Read ownership is indivisible: an output descriptor can never outlive its reservation.
#[derive(Clone, Debug)]
pub struct AssetReader {
    owner: Arc<ReaderFile>,
    jobs: AssetJobs,
}
#[derive(Debug)]
enum ReaderFile {
    Input(File),
    Output(Arc<AssetFile>),
}
impl AssetReader {
    /// Admits an already-checked input descriptor into the invocation's job lifetime.
    pub fn input(file: File, jobs: AssetJobs) -> Self {
        Self {
            owner: Arc::new(ReaderFile::Input(file)),
            jobs,
        }
    }
    /// Retains the same read-only file and reservation, without duplicating its descriptor.
    pub fn output(file: Arc<AssetFile>) -> Self {
        Self {
            jobs: file.jobs.clone(),
            owner: Arc::new(ReaderFile::Output(file)),
        }
    }
    /// An attached file, if this reader owns an invocation-produced output.
    pub fn output_file(&self) -> Option<Arc<AssetFile>> {
        match self.owner.as_ref() {
            ReaderFile::Input(_) => None,
            ReaderFile::Output(file) => Some(Arc::clone(file)),
        }
    }
    pub(crate) fn file(&self) -> &File {
        match self.owner.as_ref() {
            ReaderFile::Input(file) => file,
            ReaderFile::Output(file) => file.file(),
        }
    }
    pub(crate) fn jobs(&self) -> &AssetJobs {
        &self.jobs
    }
    /// Positional work retains file ownership through completion, including cancellation.
    pub async fn read<T: Send + 'static>(
        &self,
        work: impl FnOnce(&File) -> Result<T, AssetIoError> + Send + 'static,
    ) -> Result<T, AssetIoError> {
        let reader = self.clone();
        self.jobs.run(move || work(reader.file())).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt as _;

    // Observe the exact release boundary, not merely the eventual post-error state. Inode
    // identity avoids mistaking unrelated concurrent reuse of the FD number for a live writer;
    // the caller must keep that inode allocated so its number cannot be recycled either.
    #[derive(Debug)]
    pub(super) struct ReleaseProbe {
        descriptor: PathBuf,
        identity: (u64, u64),
        path: PathBuf,
        observed: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ReleaseProbe {
        pub(super) fn verify(&self) {
            use std::os::unix::fs::MetadataExt as _;
            assert_ne!(
                std::fs::metadata(&self.descriptor)
                    .ok()
                    .map(|metadata| (metadata.dev(), metadata.ino())),
                Some(self.identity),
                "writer must close before reservation release"
            );
            assert_eq!(
                std::fs::symlink_metadata(&self.path).unwrap_err().kind(),
                io::ErrorKind::NotFound,
                "temporary path must be removed before reservation release"
            );
            self.observed.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn failed_readonly_reopen_closes_and_unlinks_before_releasing_capacity() {
        use std::os::{
            fd::AsRawFd as _,
            unix::fs::{MetadataExt as _, symlink},
        };
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), 4);
        let mut writer = directory
            .allocate()
            .await
            .unwrap()
            .write(b"four".to_vec())
            .await
            .unwrap();
        let Sink::File { fd, path } = &writer.sink;
        let metadata = fd.metadata().unwrap();
        let descriptor = PathBuf::from(format!("/dev/fd/{}", fd.as_raw_fd()));
        let visible = std::fs::metadata(&descriptor).unwrap();
        // macOS exposes a virtual device through /dev/fd; compare within that namespace.
        assert_eq!(visible.ino(), metadata.ino());
        let identity = (visible.dev(), visible.ino());
        // Once the writer closes, a concurrent test can take both its FD number and, on ext4, its
        // freed inode number, so the probe would see the same identity through a different file.
        // A second description keeps the inode allocated: only the writer's FD can then match.
        let _inode_pin = fd.try_clone().unwrap();
        let path = path.to_path_buf();
        // Leave the writer's real file open but unlinked, and a removable dangling path.
        // Reopen now deterministically fails without permissions or process-wide exhaustion.
        std::fs::remove_file(&path).unwrap();
        symlink(root.path().join("missing-reopen-target"), &path).unwrap();
        let mut bytes = [0; 4];
        fd.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(&bytes, b"four");
        assert_eq!(directory.budget.used.load(Ordering::Relaxed), 4);
        let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        writer.reservation.release_probe = Some(ReleaseProbe {
            descriptor,
            identity,
            path,
            observed: Arc::clone(&observed),
        });
        assert!(matches!(
            writer.finish().await,
            Err(AssetIoError::Io {
                kind: io::ErrorKind::NotFound
            })
        ));
        directory.drain().await;
        assert!(observed.load(Ordering::Acquire));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        assert_eq!(directory.budget.used.load(Ordering::Relaxed), 0);
        directory
            .allocate()
            .await
            .unwrap()
            .write(vec![0; 4])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_exact_budget_is_accepted_and_one_byte_more_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), 4);
        let writer = directory.allocate().await.unwrap();
        let writer = writer.write(b"four".to_vec()).await.unwrap();
        assert!(matches!(
            writer.write(b"!".to_vec()).await,
            Err(AssetIoError::OverBudget)
        ));
        assert_eq!(directory.budget.used.load(Ordering::Relaxed), 0);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn finished_outputs_are_read_only_unlinked_and_charged_until_closed() {
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), 4);
        let writer = directory.allocate().await.unwrap();
        let writer = writer.write(b"four".to_vec()).await.unwrap();
        let output = writer.finish().await.unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        assert_eq!(directory.budget.used.load(Ordering::Relaxed), 4);
        assert!(output.file().write_at(b"!", 0).is_err());
        let mut bytes = [0; 4];
        output.file().read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(&bytes, b"four");
        drop(output);
        assert_eq!(directory.budget.used.load(Ordering::Relaxed), 0);
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_writers_stay_charged_until_the_worker_closes_and_unlinks_them() {
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), 4);
        let writer = directory
            .allocate()
            .await
            .unwrap()
            .write(b"four".to_vec())
            .await
            .unwrap();
        let jobs = directory.jobs.clone();
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            jobs.run(move || {
                started.send(()).unwrap();
                wait.recv().unwrap();
                Ok(writer)
            })
            .await
        });
        running.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(directory.budget.used.load(Ordering::Relaxed), 4);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), directory.drain())
            .await
            .unwrap();
        assert_eq!(directory.budget.used.load(Ordering::Relaxed), 0);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn disk_full_is_the_same_typed_fail_fast_budget_error() {
        assert!(matches!(
            AssetIoError::from(io::Error::from(io::ErrorKind::StorageFull)),
            AssetIoError::OverBudget
        ));
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_readers_keep_capacity_and_terminal_drain_waits_for_actual_close() {
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), 4);
        let output = directory
            .allocate()
            .await
            .unwrap()
            .write(b"four".to_vec())
            .await
            .unwrap()
            .finish()
            .await
            .unwrap();
        let reader = AssetReader::output(Arc::new(output));
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            reader
                .read(move |file| {
                    started.send(()).unwrap();
                    wait.recv().unwrap();
                    let mut bytes = [0; 4];
                    file.read_exact_at(&mut bytes, 0)?;
                    assert_eq!(&bytes, b"four");
                    Ok(())
                })
                .await
        });
        running.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(directory.budget.used.load(Ordering::Acquire), 4);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        assert!(matches!(
            directory.allocate().await.unwrap().write(vec![0]).await,
            Err(AssetIoError::OverBudget)
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), directory.drain())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), directory.drain())
            .await
            .unwrap();
        assert_eq!(directory.budget.used.load(Ordering::Acquire), 0);
        directory
            .allocate()
            .await
            .unwrap()
            .write(vec![0; 4])
            .await
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn kernel_enospc_fails_as_over_budget_and_releases_the_writer() {
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), 4);
        let mut writer = directory.allocate().await.unwrap();
        let Sink::File { fd, .. } = &mut writer.sink;
        *fd = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        assert!(matches!(
            writer.write(b"four".to_vec()).await,
            Err(AssetIoError::OverBudget)
        ));
        assert_eq!(directory.budget.used.load(Ordering::Relaxed), 0);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
