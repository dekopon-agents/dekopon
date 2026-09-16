//! Owned, bounded scratch files for chat payloads. No path is accepted from a model or provider.

use std::{
    fmt,
    io::{self, Read, Seek, SeekFrom, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use tempfile::{NamedTempFile, TempDir};
use thiserror::Error;

/// Per-attachment ceiling shared by inbound assets and provider results.
pub const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_SPOOL_BYTES: usize = 256 * 1024 * 1024;
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

/// A sanitized scratch-storage failure, never a filename or payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum BlobError {
    /// The attachment exceeds the supported per-file size.
    #[error("attachment exceeds the byte limit")]
    TooLarge,
    /// Live leases already occupy the process-wide scratch allowance.
    #[error("attachment scratch capacity exhausted")]
    Capacity,
    /// An operating-system failure. Only its category is retained.
    #[error("attachment scratch IO failed ({0:?})")]
    Io(io::ErrorKind),
    /// The descriptor no longer contains exactly the bytes published by its owner.
    #[error("attachment scratch length changed")]
    LengthChanged,
}

impl From<io::Error> for BlobError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

struct Reservation {
    bytes: usize,
    used: &'static AtomicUsize,
}
impl Reservation {
    fn new(bytes: usize, used: &'static AtomicUsize) -> Result<Self, BlobError> {
        // Charge empty files too; no number of zero-byte leases bypasses accounting.
        let bytes = bytes.max(1);
        used.fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(bytes)
                .filter(|total| *total <= MAX_SPOOL_BYTES)
        })
        .map_err(|_used| BlobError::Capacity)?;
        Ok(Self { bytes, used })
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

struct Owner {
    file: Mutex<Option<NamedTempFile>>,
    directory: Option<TempDir>,
    len: usize,
    _reservation: Reservation,
    origin: tracing::Span,
}

/// A lightweight shared lease on a private temporary file, not a filesystem capability.
///
/// Cloning copies only the lease. The final drop unlinks the file and directory. Reads use the
/// original descriptor, never a path lookup. Equality means the same lease, not equal file contents.
/// Configure the process temporary directory on disk, not tmpfs; no crash durability is promised.
#[derive(Clone)]
pub struct DiskBlob(Arc<Owner>);

impl fmt::Debug for DiskBlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiskBlob")
            .field("bytes", &self.len())
            .finish()
    }
}
impl PartialEq for DiskBlob {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for DiskBlob {}

impl DiskBlob {
    /// Spools already downloaded/decoded bytes before publishing a lease.
    ///
    /// # Errors
    /// Refuses oversized payloads, exhausted aggregate capacity, or scratch IO failure. Failed
    /// writes release their reservation and temporary files; no partially written lease escapes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, BlobError> {
        Self::write_with(bytes, &LIVE_BYTES, |file, bytes| {
            file.write_all(bytes)?;
            file.flush()
        })
    }

    // The same ownership/unwind path serves real writes and deterministic IO-failure tests.
    fn write_with(
        bytes: &[u8],
        used: &'static AtomicUsize,
        write: impl FnOnce(&mut NamedTempFile, &[u8]) -> io::Result<()>,
    ) -> Result<Self, BlobError> {
        let origin = tracing::Span::current();
        operation("write", bytes.len(), || {
            if bytes.len() > MAX_ATTACHMENT_BYTES {
                return Err(BlobError::TooLarge);
            }
            let reservation = Reservation::new(bytes.len(), used)?;
            // tempfile creates the directory exclusively with 0700 and the file with 0600 on
            // Unix. A random name in this private directory cannot follow a supplied symlink.
            let mut builder = tempfile::Builder::new();
            builder.prefix("dekopon-assets-");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                builder.permissions(std::fs::Permissions::from_mode(0o700));
            }
            let directory = builder.tempdir()?;
            let file = NamedTempFile::new_in(directory.path())?;
            let owner = Owner {
                file: Mutex::new(Some(file)),
                directory: Some(directory),
                len: bytes.len(),
                _reservation: reservation,
                origin,
            };
            {
                let mut guard = owner
                    .file
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let file = guard.as_mut().expect("unpublished owner holds a file");
                write(file, bytes)?;
                if file.as_file().metadata()?.len() != bytes.len() as u64 {
                    return Err(BlobError::LengthChanged);
                }
            }
            Ok(Self(Arc::new(owner)))
        })
    }

    /// Metadata only; does not read the file.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len
    }

    /// Whether this lease represents an empty payload.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Materializes one bounded consumption buffer. Concurrent readers serialize descriptor seeks.
    ///
    /// # Errors
    /// Returns a sanitized IO or length failure; never substitutes empty bytes or retries a call.
    pub fn read(&self) -> Result<Vec<u8>, BlobError> {
        let current = tracing::Span::current();
        let parent = if current.is_none() {
            self.0.origin.clone()
        } else {
            current
        };
        parent.in_scope(|| {
            operation("read", self.len(), || {
                let mut guard = self
                    .0
                    .file
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let file = guard.as_mut().expect("a live owner retains its descriptor");
                if file.as_file().metadata()?.len() != self.len() as u64 {
                    return Err(BlobError::LengthChanged);
                }
                file.seek(SeekFrom::Start(0))?;
                let mut bytes = Vec::with_capacity(self.len());
                file.take(self.len() as u64 + 1).read_to_end(&mut bytes)?;
                if bytes.len() != self.len() {
                    return Err(BlobError::LengthChanged);
                }
                Ok(bytes)
            })
        })
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.origin.in_scope(|| {
            operation("cleanup", self.len, || {
                let file = self
                    .file
                    .get_mut()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                let file_result = file.map_or(Ok(()), NamedTempFile::close);
                let directory_result = self.directory.take().map_or(Ok(()), TempDir::close);
                file_result?;
                directory_result?;
                Ok(())
            })
            .unwrap_or(())
        }); // operation records the sanitized cause once.
    }
}

fn operation<T>(
    operation: &'static str,
    bytes: usize,
    run: impl FnOnce() -> Result<T, BlobError>,
) -> Result<T, BlobError> {
    let span = tracing::info_span!(
        "asset.spool",
        operation,
        bytes,
        duration_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
        reason = tracing::field::Empty
    );
    span.in_scope(|| {
        let start = Instant::now();
        let result = run();
        span.record("duration_ms", start.elapsed().as_millis() as u64);
        span.record("outcome", if result.is_ok() { "ok" } else { "refused" });
        if let Err(error) = &result {
            span.record("reason", tracing::field::display(error));
            tracing::warn!(error = %error, "attachment scratch operation refused");
        }
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_and_concurrent_readers_keep_one_file_until_last_drop() {
        let blob = DiskBlob::from_bytes(b"private pixels").unwrap();
        let directory = blob.0.directory.as_ref().unwrap().path().to_owned();
        let clone = blob.clone();
        assert_eq!(blob, clone);
        assert_eq!(
            std::mem::size_of::<DiskBlob>(),
            std::mem::size_of::<usize>()
        );
        let thread = std::thread::spawn(move || clone.read().unwrap());
        assert_eq!(blob.read().unwrap(), b"private pixels");
        assert_eq!(thread.join().unwrap(), b"private pixels");
        assert!(directory.exists());
        assert!(!format!("{blob:?}").contains("private pixels"));
        drop(blob);
        assert!(!directory.exists());
    }

    #[test]
    fn truncated_files_fail_and_unlinked_files_still_read_the_owned_descriptor() {
        let blob = DiskBlob::from_bytes(b"pixels").unwrap();
        {
            let file = blob.0.file.lock().unwrap();
            let file = file.as_ref().unwrap();
            std::fs::remove_file(file.path()).unwrap();
        }
        assert_eq!(blob.read().unwrap(), b"pixels");
        blob.0
            .file
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .as_file()
            .set_len(1)
            .unwrap();
        assert_eq!(blob.read(), Err(BlobError::LengthChanged));
    }

    #[test]
    fn an_unreadable_descriptor_is_a_sanitized_io_failure() {
        let blob = DiskBlob::from_bytes(b"pixels").unwrap();
        {
            let mut guard = blob.0.file.lock().unwrap();
            let (file, path) = guard.take().unwrap().into_parts();
            let write_only = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            drop(file);
            *guard = Some(NamedTempFile::from_parts(write_only, path));
        }
        let error = blob.read().unwrap_err();
        assert!(matches!(error, BlobError::Io(_)));
        assert!(!error.to_string().contains("dekopon-assets-"));
    }

    #[cfg(unix)]
    #[test]
    fn replacing_the_name_with_a_symlink_cannot_redirect_a_descriptor_read() {
        let blob = DiskBlob::from_bytes(b"original pixels").unwrap();
        let mut other = NamedTempFile::new().unwrap();
        other.write_all(b"not the image").unwrap();
        {
            let guard = blob.0.file.lock().unwrap();
            let file = guard.as_ref().unwrap();
            std::fs::remove_file(file.path()).unwrap();
            std::os::unix::fs::symlink(other.path(), file.path()).unwrap();
        }
        assert_eq!(blob.read().unwrap(), b"original pixels");
        drop(blob);
        assert_eq!(std::fs::read(other.path()).unwrap(), b"not the image");
    }

    #[test]
    fn bounds_refuse_without_eviction_or_accounting_leaks() {
        assert_eq!(
            Reservation::new(MAX_SPOOL_BYTES + 1, &LIVE_BYTES).err(),
            Some(BlobError::Capacity)
        );
        assert_eq!(
            DiskBlob::from_bytes(&vec![0; MAX_ATTACHMENT_BYTES + 1]),
            Err(BlobError::TooLarge)
        );
        let blob = DiskBlob::from_bytes(b"still live").unwrap();
        assert_eq!(blob.read().unwrap(), b"still live");
        assert_eq!(
            BlobError::from(io::Error::from(io::ErrorKind::StorageFull)),
            BlobError::Io(io::ErrorKind::StorageFull)
        );
    }

    #[test]
    fn partial_write_failure_and_unwind_release_capacity_and_files() {
        static USED: AtomicUsize = AtomicUsize::new(0);
        let mut path = None;
        let error = DiskBlob::write_with(b"pixels", &USED, |file, _| {
            path = Some(file.path().parent().unwrap().to_owned());
            file.write_all(b"pi")?;
            Err(io::Error::from(io::ErrorKind::StorageFull))
        })
        .unwrap_err();
        assert_eq!(error, BlobError::Io(io::ErrorKind::StorageFull));
        assert_eq!(USED.load(Ordering::Acquire), 0);
        assert!(!path.unwrap().exists());
        let result = std::panic::catch_unwind(|| {
            DiskBlob::write_with(b"pixels", &USED, |_file, _| {
                panic!("cancel synchronous writer")
            })
        });
        assert!(result.is_err());
        assert_eq!(USED.load(Ordering::Acquire), 0);
    }

    #[test]
    fn aggregate_capacity_is_shared_and_returns_only_after_last_owner() {
        static USED: AtomicUsize = AtomicUsize::new(0);
        let reservation = Reservation::new(MAX_SPOOL_BYTES - 6, &USED).unwrap();
        let blob =
            DiskBlob::write_with(b"pixels", &USED, |file, bytes| file.write_all(bytes)).unwrap();
        let clone = blob.clone();
        assert_eq!(Reservation::new(1, &USED).err(), Some(BlobError::Capacity));
        drop(blob);
        assert_eq!(USED.load(Ordering::Acquire), MAX_SPOOL_BYTES);
        assert_eq!(clone.read().unwrap(), b"pixels");
        drop(clone);
        assert_eq!(USED.load(Ordering::Acquire), MAX_SPOOL_BYTES - 6);
        drop(reservation);
        assert_eq!(USED.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn scratch_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let blob = DiskBlob::from_bytes(b"pixels").unwrap();
        assert_eq!(
            blob.0
                .directory
                .as_ref()
                .unwrap()
                .path()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            blob.0
                .file
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_file()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
