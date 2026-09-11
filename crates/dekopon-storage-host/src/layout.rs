//! Opaque physical layout, retained directory descriptors, root locking, and accounting.

use std::{
    fs::{self, File, TryLockError},
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    sync::Arc,
};

use dekopon_core::{AncestorPolicy, FileHygieneError, check_trusted_ancestors};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};

use crate::{StorageHostError, key::random_bytes};

pub(crate) const ENTRY_CHARGE: u64 = 4_096;
const LAYOUT_VERSION: &str = "dekopon.dev/provider-storage-layout/v1alpha1";
const HARD_MAX_DIRECTORY_ENTRIES: u64 = 1_000_000;

/// A directory capability retained for the lifetime of every operation below it.
///
/// Paths are retained only for bounded diagnostics. Tree traversal, opens, creation, rename,
/// unlink, scans, and synchronization are all relative to this descriptor.
#[derive(Clone)]
pub(crate) struct Directory {
    file: Arc<File>,
    diagnostic_path: Arc<PathBuf>,
}

impl std::fmt::Debug for Directory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Directory([RETAINED])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EntryMetadata {
    pub(crate) kind: EntryKind,
    pub(crate) len: u64,
    pub(crate) nlink: u64,
}

#[derive(Debug)]
pub(crate) struct Layout {
    pub(crate) root: Directory,
    namespaces: Directory,
    _writer_lock: File,
}

/// The root's initialization commit point.
///
/// Strict, so a root initialized under the namespace key, whose document still carries its
/// `keyCommitment`, is refused as a corrupt layout rather than opened with every name changed.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct LayoutDocument {
    api_version: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Usage {
    pub(crate) bytes: u64,
    pub(crate) entries: u64,
    pub(crate) files: u64,
}

/// Charges the scanned directory's own entry in its parent, which a scan of it never sees.
pub(crate) fn usage_with_directory_entry(mut usage: Usage) -> Result<Usage, StorageHostError> {
    usage.entries = usage
        .entries
        .checked_add(1)
        .ok_or(StorageHostError::Arithmetic)?;
    usage.bytes = usage
        .bytes
        .checked_add(ENTRY_CHARGE)
        .ok_or(StorageHostError::Arithmetic)?;
    Ok(usage)
}

impl Layout {
    pub(crate) fn minimum_usage(root: &Path) -> Result<Usage, StorageHostError> {
        let document = LayoutDocument {
            api_version: LAYOUT_VERSION.to_owned(),
        };
        #[allow(
            clippy::map_err_ignore,
            reason = "serializing this owned one-string document has no failing case, and \
                      TryFromIntError carries only out-of-range, which Arithmetic already states"
        )]
        let encoded = u64::try_from(
            serde_json::to_vec(&document)
                .map_err(|_| StorageHostError::CorruptLayout {
                    path: root.join("layout"),
                })?
                .len(),
        )
        .map_err(|_| StorageHostError::Arithmetic)?
        .checked_add(1)
        .ok_or(StorageHostError::Arithmetic)?;
        Ok(Usage {
            bytes: 3_u64
                .checked_mul(ENTRY_CHARGE)
                .and_then(|bytes| bytes.checked_add(encoded))
                .ok_or(StorageHostError::Arithmetic)?,
            entries: 3,
            files: 2,
        })
    }

    pub(crate) fn open(root: &Path) -> Result<Self, StorageHostError> {
        validate_ancestors(root)?;
        ensure_root_directory(root)?;
        let root = Directory::open_path(root, true)?;
        let corrupt_layout = |path: PathBuf| StorageHostError::CorruptLayout { path };
        let initial_entries = root.entries_prefix(4)?;
        if initial_entries.len() > 3 {
            return Err(corrupt_layout(root.path().to_path_buf()));
        }
        let has_layout = initial_entries.iter().any(|name| name == "layout");
        let has_writer = initial_entries.iter().any(|name| name == "writer.lock");

        // `layout` is the initialization commit point. Once it exists, every required root entry
        // must already exist and no unknown entry is accepted. Recreating a missing directory here
        // would turn retained-data loss into an apparently healthy empty store before key/layout
        // verification had a chance to fail closed.
        if has_layout && !has_writer {
            return Err(corrupt_layout(root.path().to_path_buf()));
        }
        if !has_layout
            && !(initial_entries.is_empty()
                || initial_entries.as_slice() == ["writer.lock".to_owned()])
        {
            return Err(corrupt_layout(root.path().to_path_buf()));
        }

        let writer = root.open_private("writer.lock", !has_writer)?;
        if writer
            .metadata()
            .map_err(|source| root.io_error(source))?
            .len()
            != 0
        {
            return Err(corrupt_layout(root.diagnostic_child("writer.lock")));
        }
        writer
            .try_lock()
            .map_err(|source| writer_lock_failure(&root, source))?;

        let namespaces = if has_layout {
            let encoded = root.read_bounded("layout", 4_096)?;
            let document: LayoutDocument = serde_json::from_slice(&encoded).map_err(|error| {
                crate::report_decode_failure("layout", &error);
                corrupt_layout(root.diagnostic_child("layout"))
            })?;
            if document.api_version != LAYOUT_VERSION {
                return Err(corrupt_layout(root.diagnostic_child("layout")));
            }
            let expected = ["layout", "namespaces", "writer.lock"];
            let retained = root.entries_prefix(expected.len() as u64 + 1)?;
            if retained.len() != expected.len() || retained.iter().map(String::as_str).ne(expected)
            {
                return Err(corrupt_layout(root.path().to_path_buf()));
            }
            root.open_directory("namespaces")?
        } else {
            let namespaces = root.ensure_directory("namespaces")?;
            let document = LayoutDocument {
                api_version: LAYOUT_VERSION.to_owned(),
            };
            #[allow(
                clippy::map_err_ignore,
                reason = "serializing this owned one-string document has no failing case"
            )]
            let mut encoded = serde_json::to_vec(&document)
                .map_err(|_| corrupt_layout(root.diagnostic_child("layout")))?;
            encoded.push(b'\n');
            let mut file = root.create_private("layout")?;
            file.write_all(&encoded)
                .and_then(|()| file.sync_all())
                .map_err(|source| root.io_error(source))?;
            root.sync()?;
            namespaces
        };

        Ok(Self {
            root,
            namespaces,
            _writer_lock: writer,
        })
    }

    pub(crate) const fn namespaces(&self) -> &Directory {
        &self.namespaces
    }
}

impl Directory {
    fn open_path(path: &Path, private: bool) -> Result<Self, StorageHostError> {
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|source| StorageHostError::RootIo {
            path: path.to_path_buf(),
            source: std::io::Error::from(source),
        })?;
        let directory = Self {
            file: Arc::new(File::from(fd)),
            diagnostic_path: Arc::new(path.to_path_buf()),
        };
        directory.validate_self(private)?;
        Ok(directory)
    }

    pub(crate) fn path(&self) -> &Path {
        self.diagnostic_path.as_path()
    }

    pub(crate) fn diagnostic_child(&self, name: &str) -> PathBuf {
        self.path().join(name)
    }

    pub(crate) fn io_error(&self, source: std::io::Error) -> StorageHostError {
        StorageHostError::RootIo {
            path: self.path().to_path_buf(),
            source,
        }
    }

    /// A corruption found at one entry of this directory, naming its path.
    pub(crate) fn corrupt(&self, name: &str, scope: &'static str) -> StorageHostError {
        StorageHostError::corrupt(scope).at(self.diagnostic_child(name))
    }

    fn validate_self(&self, private: bool) -> Result<(), StorageHostError> {
        let metadata = self
            .file
            .metadata()
            .map_err(|source| self.io_error(source))?;
        let invalid_mode = if private {
            metadata.permissions().mode() & 0o077 != 0
        } else {
            false
        };
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || invalid_mode
        {
            return Err(StorageHostError::UnsafeRoot {
                path: self.path().to_path_buf(),
            });
        }
        Ok(())
    }

    pub(crate) fn ensure_directory(&self, name: &str) -> Result<Self, StorageHostError> {
        validate_component(name)?;
        match rustix::fs::mkdirat(self.file.as_ref(), name, Mode::from_raw_mode(0o700)) {
            Ok(()) => self.sync()?,
            Err(rustix::io::Errno::EXIST) => {}
            Err(source) => return Err(self.io_error(std::io::Error::from(source))),
        }
        self.open_directory(name)
    }

    pub(crate) fn open_directory(&self, name: &str) -> Result<Self, StorageHostError> {
        validate_component(name)?;
        let before = rustix::fs::statat(self.file.as_ref(), name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|source| self.io_error(std::io::Error::from(source)))?;
        if FileType::from_raw_mode(before.st_mode) != FileType::Directory {
            return Err(self.corrupt(name, "directory-type"));
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = rustix::fs::openat(self.file.as_ref(), name, flags, Mode::empty())
            .map_err(|source| self.io_error(std::io::Error::from(source)))?;
        let child = Self {
            file: Arc::new(File::from(fd)),
            diagnostic_path: Arc::new(self.diagnostic_child(name)),
        };
        child.validate_self(true)?;
        let opened = child
            .file
            .metadata()
            .map_err(|source| child.io_error(source))?;
        if !opened.is_dir()
            || opened.dev() != before.st_dev as u64
            || opened.ino() != before.st_ino as u64
        {
            return Err(self.corrupt(name, "directory-identity"));
        }
        Ok(child)
    }

    /// Revalidates that a retained child descriptor is still the entry named by this parent.
    ///
    /// After an advisory-lease wait, refuse a base whose name no longer identifies the
    /// descriptor on which the waiter acquired its lock.
    pub(crate) fn retains_child(&self, name: &str, child: &Self) -> Result<bool, StorageHostError> {
        validate_component(name)?;
        let stat = match rustix::fs::statat(self.file.as_ref(), name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => return Ok(false),
            Err(source) => return Err(self.io_error(std::io::Error::from(source))),
        };
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
            return Ok(false);
        }
        let metadata = child
            .file
            .metadata()
            .map_err(|source| child.io_error(source))?;
        Ok(metadata.dev() == stat.st_dev as u64 && metadata.ino() == stat.st_ino as u64)
    }

    pub(crate) fn metadata(&self, name: &str) -> Result<Option<EntryMetadata>, StorageHostError> {
        validate_component(name)?;
        let stat = match rustix::fs::statat(self.file.as_ref(), name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(source) => return Err(self.io_error(std::io::Error::from(source))),
        };
        let kind = match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => EntryKind::File,
            FileType::Directory => EntryKind::Directory,
            FileType::Symlink => EntryKind::Symlink,
            _ => EntryKind::Other,
        };
        Ok(Some(EntryMetadata {
            kind,
            len: stat.st_size.try_into().unwrap_or(u64::MAX),
            nlink: stat.st_nlink as u64,
        }))
    }

    pub(crate) fn exists(&self, name: &str) -> Result<bool, StorageHostError> {
        Ok(self.metadata(name)?.is_some())
    }

    /// Reads at most one bounded prefix without first materializing the complete directory.
    pub(crate) fn entries_prefix(&self, maximum: u64) -> Result<Vec<String>, StorageHostError> {
        self.read_entries(maximum.min(HARD_MAX_DIRECTORY_ENTRIES), false)
    }

    /// Reads a configured bounded directory and fails on the first excess entry.
    pub(crate) fn entries_bounded(&self, maximum: u64) -> Result<Vec<String>, StorageHostError> {
        self.read_entries(maximum.min(HARD_MAX_DIRECTORY_ENTRIES), true)
    }

    fn read_entries(
        &self,
        maximum: u64,
        fail_on_excess: bool,
    ) -> Result<Vec<String>, StorageHostError> {
        #[cfg(test)]
        note_directory_scan();
        let mut directory = rustix::fs::Dir::read_from(self.file.as_ref())
            .map_err(|source| self.io_error(std::io::Error::from(source)))?;
        let mut entries = Vec::new();
        for entry in &mut directory {
            let entry = entry.map_err(|source| self.io_error(std::io::Error::from(source)))?;
            #[allow(
                clippy::map_err_ignore,
                reason = "Utf8Error reports only the offending byte offset inside a physical name \
                          this crate never exports; the `non-utf8-entry` scope is the complete \
                          diagnosis"
            )]
            let name = entry.file_name().to_str().map_err(|_| {
                StorageHostError::corrupt("non-utf8-entry").at(self.path().to_path_buf())
            })?;
            if name == "." || name == ".." {
                continue;
            }
            validate_component(name)?;
            if entries.len() as u64 >= maximum {
                if fail_on_excess {
                    return Err(StorageHostError::StartupEntryLimit {
                        count: entries.len() as u64 + 1,
                        maximum,
                    });
                }
                break;
            }
            entries.push(name.to_owned());
        }
        entries.sort();
        Ok(entries)
    }

    pub(crate) fn open_private(&self, name: &str, create: bool) -> Result<File, StorageHostError> {
        validate_component(name)?;
        let mut flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        if create {
            flags |= OFlags::CREATE;
        }
        let fd = rustix::fs::openat(self.file.as_ref(), name, flags, Mode::from_raw_mode(0o600))
            .map_err(|source| self.io_error(std::io::Error::from(source)))?;
        let file = File::from(fd);
        self.validate_private_file(name, &file)?;
        Ok(file)
    }

    pub(crate) fn create_private(&self, name: &str) -> Result<File, StorageHostError> {
        validate_component(name)?;
        let fd = rustix::fs::openat(
            self.file.as_ref(),
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|source| self.io_error(std::io::Error::from(source)))?;
        let file = File::from(fd);
        self.validate_private_file(name, &file)?;
        Ok(file)
    }

    pub(crate) fn validate_private_file(
        &self,
        name: &str,
        file: &File,
    ) -> Result<(), StorageHostError> {
        let metadata = file.metadata().map_err(|source| self.io_error(source))?;
        // Private: every retained file is broker-owned state, so group- or world-readability is
        // already the loss. `Corrupt` stays opaque to the guest, so which check refused it is
        // logged here rather than returned.
        if let Err(error) = dekopon_core::check_trusted_metadata(
            &self.diagnostic_child(name),
            &metadata,
            rustix::process::geteuid().as_raw(),
            dekopon_core::FileTier::Private,
        ) {
            tracing::warn!(
                storage.file = %self.diagnostic_child(name).display(),
                storage.check = error.category(),
                "refusing a private storage file that failed its hygiene check"
            );
            return Err(self.corrupt(name, "private-file"));
        }
        let stat = rustix::fs::statat(self.file.as_ref(), name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|source| self.io_error(std::io::Error::from(source)))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
            || metadata.dev() != stat.st_dev as u64
            || metadata.ino() != stat.st_ino as u64
        {
            return Err(self.corrupt(name, "private-file-identity"));
        }
        Ok(())
    }

    pub(crate) fn read_bounded(
        &self,
        name: &str,
        maximum: u64,
    ) -> Result<Vec<u8>, StorageHostError> {
        let file = self.open_private(name, false)?;
        let metadata = file.metadata().map_err(|source| self.io_error(source))?;
        if metadata.len() > maximum {
            return Err(self.corrupt(name, "oversized-file"));
        }
        #[allow(
            clippy::map_err_ignore,
            reason = "TryFromIntError carries only out-of-range, which Arithmetic already states"
        )]
        let capacity = usize::try_from(metadata.len()).map_err(|_| StorageHostError::Arithmetic)?;
        let mut bytes = Vec::with_capacity(capacity);
        file.take(maximum.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|source| self.io_error(source))?;
        if bytes.len() as u64 > maximum {
            return Err(self.corrupt(name, "oversized-file"));
        }
        Ok(bytes)
    }

    pub(crate) fn read_at(
        &self,
        name: &str,
        offset: u64,
        maximum: usize,
    ) -> Result<Vec<u8>, StorageHostError> {
        let mut file = self.open_private(name, false)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|source| self.io_error(source))?;
        let mut bytes = vec![0_u8; maximum];
        let read = file
            .read(&mut bytes)
            .map_err(|source| self.io_error(source))?;
        bytes.truncate(read);
        Ok(bytes)
    }

    pub(crate) fn replace_private(
        &self,
        target: &str,
        bytes: &[u8],
    ) -> Result<(), StorageHostError> {
        validate_component(target)?;
        let temporary = self.unique_temporary_name()?;
        let result = (|| {
            let mut file = self.create_private(&temporary)?;
            file.write_all(bytes)
                .and_then(|()| file.sync_all())
                .map_err(|source| self.io_error(source))?;
            rustix::fs::renameat(
                self.file.as_ref(),
                temporary.as_str(),
                self.file.as_ref(),
                target,
            )
            .map_err(|source| self.io_error(std::io::Error::from(source)))?;
            self.sync()
        })();
        if result.is_err() {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "unique-name rollback cleanup: the create/write/rename failure in `result` \
                          is the reported cause, and a leftover `tmp-` entry is recognized \
                          reservation the next scan still charges"
            )]
            let _ = self.remove_file_if_exists(&temporary);
        }
        result
    }

    fn unique_temporary_name(&self) -> Result<String, StorageHostError> {
        for _ in 0..16 {
            let bytes = random_bytes(16)?;
            let mut name = String::from("tmp-");
            for byte in bytes {
                use std::fmt::Write as _;
                write!(&mut name, "{byte:02x}")
                    .expect("writing a hexadecimal byte to String cannot fail");
            }
            if !self.exists(&name)? {
                return Ok(name);
            }
        }
        Err(StorageHostError::Busy)
    }

    pub(crate) fn rename_to(
        &self,
        name: &str,
        target: &Directory,
        target_name: &str,
    ) -> Result<(), StorageHostError> {
        validate_component(name)?;
        validate_component(target_name)?;
        rustix::fs::renameat(self.file.as_ref(), name, target.file.as_ref(), target_name)
            .map_err(|source| self.io_error(std::io::Error::from(source)))
    }

    pub(crate) fn remove_file(&self, name: &str) -> Result<(), StorageHostError> {
        validate_component(name)?;
        rustix::fs::unlinkat(self.file.as_ref(), name, AtFlags::empty())
            .map_err(|source| self.io_error(std::io::Error::from(source)))
    }

    pub(crate) fn remove_file_if_exists(&self, name: &str) -> Result<(), StorageHostError> {
        match self.remove_file(name) {
            Ok(()) => Ok(()),
            Err(StorageHostError::RootIo { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn sync(&self) -> Result<(), StorageHostError> {
        self.file.sync_all().map_err(|source| self.io_error(source))
    }
}

/// Classifies one `writer.lock` acquisition failure.
///
/// Only a would-block refusal proves another conforming writer holds the root. Every other error
/// is the filesystem itself failing or refusing advisory locks, which must surface as root I/O
/// carrying its cause rather than as a second writer an operator would then go looking for.
fn writer_lock_failure(root: &Directory, source: TryLockError) -> StorageHostError {
    match source {
        TryLockError::WouldBlock => StorageHostError::SecondWriter,
        TryLockError::Error(source) => root.io_error(source),
    }
}

fn ensure_root_directory(root: &Path) -> Result<(), StorageHostError> {
    if fs::symlink_metadata(root).is_ok() {
        return Ok(());
    }
    let parent = root.parent().ok_or_else(|| StorageHostError::UnsafeRoot {
        path: root.to_path_buf(),
    })?;
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| StorageHostError::UnsafeRoot {
            path: root.to_path_buf(),
        })?;
    validate_component(name)?;
    let parent_fd = rustix::fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|source| StorageHostError::RootIo {
        path: parent.to_path_buf(),
        source: std::io::Error::from(source),
    })?;
    match rustix::fs::mkdirat(&parent_fd, name, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => Ok(()),
        Err(source) => Err(StorageHostError::RootIo {
            path: root.to_path_buf(),
            source: std::io::Error::from(source),
        }),
    }
}

fn validate_component(name: &str) -> Result<(), StorageHostError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err(StorageHostError::corrupt("physical-component"));
    }
    Ok(())
}

/// Walks as written, parent and above, because the root may not exist yet and
/// `a_configured_root_ancestor_symlink_is_rejected_before_canonicalization` pins it.
fn validate_ancestors(path: &Path) -> Result<(), StorageHostError> {
    let policy = AncestorPolicy {
        canonicalize: false,
        include_self: false,
    };
    check_trusted_ancestors(path, policy).map_err(|error| match error {
        FileHygieneError::Io { path, source } => StorageHostError::RootIo { path, source },
        refusal => StorageHostError::UnsafeRoot {
            path: refusal.path().to_path_buf(),
        },
    })
}

#[cfg(test)]
thread_local! {
    /// Directory reads performed on this thread.
    ///
    /// Test-only instrumentation. Accounting a mutation from the tree it just changed is a cost
    /// this crate has to hold to — a write reads no directory — so directory reads are counted
    /// rather than assumed. A tree walk bumps this once for the walk and once per directory it
    /// reads, so a scan on a path that must not scan cannot register as one read.
    static DIRECTORY_SCANS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_directory_scan() {
    DIRECTORY_SCANS.with(|cell| cell.set(cell.get().saturating_add(1)));
}

/// Directory reads performed on this thread so far.
#[cfg(test)]
pub(crate) fn directory_scans() -> u64 {
    DIRECTORY_SCANS.with(std::cell::Cell::get)
}

pub(crate) fn scan_usage(
    directory: &Directory,
    maximum_entries: u64,
) -> Result<Usage, StorageHostError> {
    #[cfg(test)]
    note_directory_scan();
    let mut usage = Usage::default();
    scan(directory, maximum_entries, &mut usage)?;
    Ok(usage)
}

fn scan(
    directory: &Directory,
    maximum_entries: u64,
    usage: &mut Usage,
) -> Result<(), StorageHostError> {
    let remaining = maximum_entries.saturating_sub(usage.entries);
    let entries = directory.entries_prefix(remaining.saturating_add(1))?;
    if entries.len() as u64 > remaining {
        return Err(StorageHostError::StartupEntryLimit {
            count: usage.entries.saturating_add(entries.len() as u64),
            maximum: maximum_entries,
        });
    }
    for name in entries {
        scan_entry(directory, &name, maximum_entries, usage)?;
    }
    Ok(())
}

/// Charges one entry and, for a directory, everything under it.
fn scan_entry(
    directory: &Directory,
    name: &str,
    maximum_entries: u64,
    usage: &mut Usage,
) -> Result<(), StorageHostError> {
    let metadata = directory
        .metadata(name)?
        .ok_or_else(|| directory.corrupt(name, "vanished-entry"))?;
    usage.entries = usage
        .entries
        .checked_add(1)
        .ok_or(StorageHostError::Arithmetic)?;
    if usage.entries > maximum_entries {
        return Err(StorageHostError::StartupEntryLimit {
            count: usage.entries,
            maximum: maximum_entries,
        });
    }
    usage.bytes = usage
        .bytes
        .checked_add(ENTRY_CHARGE)
        .ok_or(StorageHostError::Arithmetic)?;
    match metadata.kind {
        EntryKind::File => {
            let _ = directory.open_private(name, false)?;
            usage.files = usage
                .files
                .checked_add(1)
                .ok_or(StorageHostError::Arithmetic)?;
            usage.bytes = usage
                .bytes
                .checked_add(metadata.len)
                .ok_or(StorageHostError::Arithmetic)?;
        }
        EntryKind::Directory => {
            let child = directory.open_directory(name)?;
            scan(&child, maximum_entries, usage)?;
        }
        EntryKind::Symlink => return Err(directory.corrupt(name, "symlink")),
        EntryKind::Other => return Err(directory.corrupt(name, "file-type")),
    }
    Ok(())
}

/// The startup quota walk.
///
/// Root-level entries are the store's own shape and a fault in one refuses startup. Everything
/// under `namespaces/` belongs to one conversation: a base that will not scan is logged as
/// `storage_root_entry_ignored` and left uncharged, and its next grant is where it fails.
pub(crate) fn scan_root_usage(
    layout: &Layout,
    maximum_entries: u64,
) -> Result<Usage, StorageHostError> {
    let mut usage = Usage::default();
    for name in layout.root.entries_bounded(maximum_entries)? {
        if name != "namespaces" {
            scan_entry(&layout.root, &name, maximum_entries, &mut usage)?;
            continue;
        }
        // The `namespaces` entry itself, then each base on its own.
        usage = usage_with_directory_entry(usage)?;
        let namespaces = layout.namespaces();
        for base in namespaces.entries_bounded(maximum_entries)? {
            let mut charged = usage;
            match scan_entry(namespaces, &base, maximum_entries, &mut charged) {
                Ok(()) => usage = charged,
                Err(error) => {
                    let check = match &error {
                        StorageHostError::Corrupt { scope, .. } => *scope,
                        StorageHostError::UnsafeRoot { .. } => "unsafe-directory",
                        StorageHostError::RootIo { .. } => "io",
                        _ => return Err(error),
                    };
                    tracing::warn!(
                        event = "storage_root_entry_ignored",
                        category = "storage",
                        storage.namespace = %base,
                        storage.path = %namespaces.diagnostic_child(&base).display(),
                        storage.check = check,
                        error = %dekopon_core::error_chain(&error),
                        "a namespace did not scan at startup; it is not charged and its next \
                         grant fails on it"
                    );
                }
            }
        }
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use std::io::{Error, ErrorKind};

    use super::{Directory, TryLockError, writer_lock_failure};
    use crate::StorageHostError;

    #[test]
    fn only_a_would_block_writer_lock_failure_is_a_second_writer() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let root = Directory::open_path(temporary.path(), false).expect("root directory");
        assert!(matches!(
            writer_lock_failure(&root, TryLockError::WouldBlock),
            StorageHostError::SecondWriter
        ));
        // A filesystem that fails or refuses advisory locks is not another conforming writer.
        assert!(matches!(
            writer_lock_failure(&root, TryLockError::Error(Error::from(ErrorKind::Unsupported))),
            StorageHostError::RootIo { source, .. } if source.kind() == ErrorKind::Unsupported
        ));
    }
}
