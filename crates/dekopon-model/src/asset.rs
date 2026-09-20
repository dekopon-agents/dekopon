//! Owned, bounded scratch files for chat payloads. No path is accepted from a model or provider.

use std::{
    fmt,
    fs::File,
    io::{self, Write},
    os::{fd::OwnedFd, unix::fs::FileExt},
    sync::{Arc, Mutex},
    time::Instant,
};

use tempfile::{NamedTempFile, TempDir};
use thiserror::Error;

/// Decoded per-attachment ceiling shared by inbound assets and provider results.
pub const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;
/// Largest stored representation: padded base64 of an eight-MiB decoded asset.
pub const MAX_STORED_ATTACHMENT_BYTES: usize = MAX_ATTACHMENT_BYTES.div_ceil(3) * 4;

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

enum BlobFile {
    Path(NamedTempFile),
    Fd(File),
}

impl BlobFile {
    fn as_file(&self) -> &File {
        match self {
            Self::Path(file) => file.as_file(),
            Self::Fd(file) => file,
        }
    }
    fn close(self) -> io::Result<()> {
        match self {
            Self::Path(file) => file.close(),
            Self::Fd(file) => {
                drop(file);
                Ok(())
            }
        }
    }
}

struct Owner {
    file: Mutex<Option<BlobFile>>,
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
                file: Mutex::new(Some(BlobFile::Path(file))),
                directory: Some(directory),
                len: bytes.len(),
                origin,
            };
            {
                let mut guard = owner
                    .file
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(BlobFile::Path(file)) = guard.as_mut() else {
                    return Err(BlobError::Reclaimed);
                };
                write(file, bytes)?;
                if file.as_file().metadata()?.len() != bytes.len() as u64 {
                    return Err(BlobError::LengthChanged);
                }
            }
            Ok(Self(Arc::new(owner)))
        })
    }

    /// Admits a read-only broker descriptor without copying its contents.
    ///
    /// # Errors
    /// Refuses a changed fstat length or a stored payload over the representation ceiling.
    /// The encoding-aware caller must also check the decoded per-asset ceiling before admission.
    pub fn from_descriptor(descriptor: OwnedFd, len: usize) -> Result<Self, BlobError> {
        if len > MAX_STORED_ATTACHMENT_BYTES {
            return Err(BlobError::TooLarge);
        }
        let file = File::from(descriptor);
        if file.metadata()?.len() != len as u64 {
            return Err(BlobError::LengthChanged);
        }
        Ok(Self(Arc::new(Owner {
            file: Mutex::new(Some(BlobFile::Fd(file))),
            directory: None,
            len,
            origin: tracing::Span::current(),
        })))
    }

    /// Obtains a read-only, close-on-exec descriptor for one invocation.
    ///
    /// Path-backed scratch has a writable owner, so it is opened afresh; a broker output is
    /// read-only by construction and can be duplicated because all readers are positional.
    /// # Errors
    /// Refuses reclaimed files, changed lengths or descriptor IO failure.
    pub fn descriptor(&self) -> Result<OwnedFd, BlobError> {
        let guard = self
            .0
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let file = guard.as_ref().ok_or(BlobError::Reclaimed)?;
        let descriptor = match file {
            BlobFile::Path(file) => File::open(file.path())?,
            BlobFile::Fd(file) => file.try_clone()?,
        };
        if descriptor.metadata()?.len() != self.len() as u64 {
            return Err(BlobError::LengthChanged);
        }
        Ok(descriptor.into())
    }

    /// Reads a bounded range without observing or changing a shared file offset.
    /// # Errors
    /// Refuses reclaimed, truncated or otherwise unreadable files.
    pub fn read_exact_at(&self, bytes: &mut [u8], offset: u64) -> Result<(), BlobError> {
        let guard = self
            .0
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let file = guard.as_ref().ok_or(BlobError::Reclaimed)?.as_file();
        if file.metadata()?.len() != self.len() as u64 {
            return Err(BlobError::LengthChanged);
        }
        file.read_exact_at(bytes, offset)?;
        Ok(())
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
            if let Some(BlobFile::Path(owned)) = file.as_ref() {
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

    /// Materializes one bounded consumption buffer using positional reads only.
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
                let mut bytes = vec![0; self.len()];
                self.read_exact_at(&mut bytes, 0)?;
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
    /// Reads the consumer representation, decoding retained encodings when necessary.
    /// # Errors
    /// Returns the same scoped retention and IO failures as pinning.
    fn read(&self) -> Result<Vec<u8>, BlobError> {
        self.pin()?.read()
    }
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
        self.source.read()
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
                let file_result = file.map_or(Ok(()), BlobFile::close);
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
    fn pathless_descriptors_are_reusable_and_positional_across_invocations() {
        let original = DiskBlob::from_bytes(b"0123456789").unwrap();
        let blob = DiskBlob::from_descriptor(original.descriptor().unwrap(), 10).unwrap();
        drop(original);
        for _ in 0..2 {
            let first = File::from(blob.descriptor().unwrap());
            let second = File::from(blob.descriptor().unwrap());
            let mut prefix = [0; 4];
            first.read_exact_at(&mut prefix, 0).unwrap();
            assert_eq!(&prefix, b"0123");
            assert_eq!(blob.read().unwrap(), b"0123456789");
            first.read_exact_at(&mut prefix, 4).unwrap();
            assert_eq!(&prefix, b"4567");
            second.read_exact_at(&mut prefix, 0).unwrap();
            assert_eq!(&prefix, b"0123");
            assert!(first.write_at(b"x", 0).is_err());
        }
        blob.reclaim().unwrap();
        assert_eq!(blob.read(), Err(BlobError::Reclaimed));
        assert!(blob.0.file.lock().unwrap().is_none());
    }

    #[test]
    fn descriptor_admission_checks_fstat_and_the_exact_ceiling() {
        let file = NamedTempFile::new().unwrap();
        file.as_file()
            .set_len(MAX_STORED_ATTACHMENT_BYTES as u64)
            .unwrap();
        let blob = DiskBlob::from_descriptor(
            File::open(file.path()).unwrap().into(),
            MAX_STORED_ATTACHMENT_BYTES,
        )
        .unwrap();
        assert!(
            DiskBlob::from_descriptor(blob.descriptor().unwrap(), MAX_STORED_ATTACHMENT_BYTES)
                .is_ok()
        );
        assert_eq!(
            DiskBlob::from_descriptor(blob.descriptor().unwrap(), MAX_STORED_ATTACHMENT_BYTES + 1),
            Err(BlobError::TooLarge)
        );
        assert_eq!(
            DiskBlob::from_descriptor(blob.descriptor().unwrap(), MAX_ATTACHMENT_BYTES - 1),
            Err(BlobError::LengthChanged)
        );
    }

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
            let BlobFile::Path(file) = file.as_ref().unwrap() else {
                panic!("path fixture")
            };
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
            let BlobFile::Path(owned) = guard.take().unwrap() else {
                panic!("path fixture")
            };
            let (file, path) = owned.into_parts();
            let write_only = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            drop(file);
            *guard = Some(BlobFile::Path(NamedTempFile::from_parts(write_only, path)));
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
            let BlobFile::Path(file) = guard.as_ref().unwrap() else {
                panic!("path fixture")
            };
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
        let path = {
            let guard = blob.0.file.lock().unwrap();
            let BlobFile::Path(file) = guard.as_ref().unwrap() else {
                panic!("path fixture")
            };
            file.path().to_owned()
        };
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
