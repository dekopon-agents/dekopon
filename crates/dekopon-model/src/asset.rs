//! Owned, bounded scratch files for chat payloads. No path is accepted from a model or provider.

use std::{
    fmt,
    io::{self, Read, Seek, SeekFrom, Write},
    sync::{Arc, Mutex},
    time::Instant,
};

use tempfile::{NamedTempFile, TempDir};
use thiserror::Error;

/// Per-attachment ceiling shared by inbound assets and provider results.
pub const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;

/// A sanitized scratch-storage failure, never a filename or payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum BlobError {
    /// The attachment exceeds the supported per-file size.
    #[error("attachment exceeds the byte limit")]
    TooLarge,
    /// The owner explicitly disabled gateway attachment retention.
    #[error("asset retention is disabled (assetRetentionBytes is zero); answer in text")]
    Disabled,
    /// Active request pins prevent admission within the gateway retention budget.
    #[error("attachment scratch capacity exhausted")]
    Capacity,
    /// No retained or released entry recognizes this scoped ID.
    #[error("unknown asset in this conversation; choose an ID from the current inventory")]
    Unknown,
    /// The gateway has reclaimed this asset; callers must not redownload it.
    #[error("asset was released; ask the user to resend it or choose another asset")]
    Reclaimed,
    /// The reference is no longer authorized in its original generation.
    #[error("asset is unavailable in this conversation generation")]
    Unauthorized,
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

struct Owner {
    file: Mutex<Option<NamedTempFile>>,
    directory: Option<TempDir>,
    len: usize,
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
    /// Refuses oversized payloads or scratch IO failure. The gateway reserves capacity before calling. Failed
    /// writes release their reservation and temporary files; no partially written lease escapes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, BlobError> {
        Self::write_with(bytes, |file, bytes| {
            file.write_all(bytes)?;
            file.flush()
        })
    }

    // The same ownership/unwind path serves real writes and deterministic IO-failure tests.
    fn write_with(
        bytes: &[u8],
        write: impl FnOnce(&mut NamedTempFile, &[u8]) -> io::Result<()>,
    ) -> Result<Self, BlobError> {
        let origin = tracing::Span::current();
        operation("write", bytes.len(), || {
            if bytes.len() > MAX_ATTACHMENT_BYTES {
                return Err(BlobError::TooLarge);
            }
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

    /// Whether an active consumer holds a pin in addition to the cache owner.
    #[must_use]
    pub fn is_pinned(&self) -> bool {
        Arc::strong_count(&self.0) > 1
    }

    /// Unlinks an unpinned cache entry before the gateway releases its byte accounting.
    ///
    /// # Errors
    /// A live consumer pin refuses reclamation. Unlink failure preserves the descriptor and
    /// accounting owner so another admission cannot pretend unreclaimed disk is free.
    pub fn reclaim(&self) -> Result<(), BlobError> {
        if self.is_pinned() {
            return Err(BlobError::Capacity);
        }
        operation("reclaim", self.len(), || {
            let mut file = self
                .0
                .file
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(owned) = file.as_ref() {
                match std::fs::remove_file(owned.path()) {
                    Ok(()) => (),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error.into()),
                }
            }
            // The descriptor closes before accounting is released; unlink alone is not disk reclamation.
            drop(file.take());
            Ok(())
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
                let file = guard.as_mut().ok_or(BlobError::Reclaimed)?;
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

/// A gateway-owned resolver. Resolution must authorize before pinning or touching retention.
pub trait BlobSource: Send + Sync {
    /// Pins an existing asset for actual consumption, never by redownloading a reclaimed asset.
    ///
    /// # Errors
    /// Returns a sanitized retention or IO failure.
    fn pin(&self) -> Result<DiskBlob, BlobError>;
}

/// Byte-free model reference. Clones do not acquire a disk pin.
#[derive(Clone)]
pub struct BlobReference {
    source: Arc<dyn BlobSource>,
    bytes: usize,
    id: u64,
}
impl BlobReference {
    /// Binds a scoped resolver and gateway metadata, never a path.
    pub fn new(source: Arc<dyn BlobSource>, bytes: usize, id: u64) -> Self {
        Self { source, bytes, id }
    }
    /// Metadata only; never updates recency.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes
    }
    /// Whether the payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes == 0
    }
    /// Resolves and reads a transient pin for model inclusion.
    ///
    /// # Errors
    /// Returns the resolver's retention failure or the descriptor's IO failure.
    pub fn read(&self) -> Result<Vec<u8>, BlobError> {
        self.source.pin()?.read()
    }
    /// Explicit gateway notice substituted for a released historical attachment.
    #[must_use]
    pub fn release_notice(&self) -> String {
        format!(
            "[gateway: Chat Asset #{} was released from disk retention and is no longer available. Ask the user to resend it or choose another asset; do not silently reuse an original image.]",
            self.id
        )
    }
}
impl fmt::Debug for BlobReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlobReference")
            .field("id", &self.id)
            .field("bytes", &self.bytes)
            .finish()
    }
}
impl PartialEq for BlobReference {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.source, &other.source)
    }
}
impl Eq for BlobReference {}
impl BlobSource for DiskBlob {
    fn pin(&self) -> Result<DiskBlob, BlobError> {
        Ok(self.clone())
    }
}
impl From<DiskBlob> for BlobReference {
    fn from(blob: DiskBlob) -> Self {
        Self::new(Arc::new(blob.clone()), blob.len(), 0)
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
    fn oversize_and_partial_write_failure_leave_no_files() {
        assert_eq!(
            DiskBlob::from_bytes(&vec![0; MAX_ATTACHMENT_BYTES + 1]),
            Err(BlobError::TooLarge)
        );
        let mut path = None;
        let error = DiskBlob::write_with(b"pixels", |file, _| {
            path = Some(file.path().parent().unwrap().to_owned());
            file.write_all(b"pi")?;
            Err(io::Error::from(io::ErrorKind::StorageFull))
        })
        .unwrap_err();
        assert_eq!(error, BlobError::Io(io::ErrorKind::StorageFull));
        assert!(!path.unwrap().exists());
    }

    #[test]
    fn reclaim_unlinks_disk_and_failed_unlink_preserves_the_owner() {
        let blob = DiskBlob::from_bytes(b"pixels").unwrap();
        let directory = blob.0.directory.as_ref().unwrap().path().to_owned();
        let path = blob
            .0
            .file
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .path()
            .to_owned();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(blob.reclaim(), Err(BlobError::Io(_))));
        assert_eq!(blob.read().unwrap(), b"pixels");
        std::fs::remove_dir(&path).unwrap();
        blob.reclaim().unwrap();
        assert_eq!(blob.read(), Err(BlobError::Reclaimed));
        drop(blob);
        assert!(!directory.exists());
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
