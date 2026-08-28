//! Opaque physical layout, retained directory descriptors, root locking, and accounting.

use std::{
    fs::{self, File, TryLockError},
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    sync::Arc,
};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};

use crate::{
    StorageHostError,
    key::{DOMAIN_AUTHORITY, StorageKey, random_bytes},
};

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

pub(crate) struct EntryStream {
    directory: Directory,
    inner: rustix::fs::Dir,
}

impl std::fmt::Debug for EntryStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("EntryStream([RETAINED])")
    }
}

impl EntryStream {
    pub(crate) fn next_name(&mut self) -> Result<Option<String>, StorageHostError> {
        loop {
            let Some(entry) = self.inner.read() else {
                return Ok(None);
            };
            let entry =
                entry.map_err(|source| self.directory.io_error(std::io::Error::from(source)))?;
            #[allow(
                clippy::map_err_ignore,
                reason = "Utf8Error reports only the offending byte offset inside a physical name \
                          this crate never exports; the `non-utf8-entry` scope is the complete \
                          diagnosis"
            )]
            let name = entry
                .file_name()
                .to_str()
                .map_err(|_| StorageHostError::Corrupt {
                    scope: "non-utf8-entry",
                })?;
            if name == "." || name == ".." {
                continue;
            }
            validate_component(name)?;
            return Ok(Some(name.to_owned()));
        }
    }
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
    transactions: Directory,
    quarantine: Directory,
    trash: Directory,
    _writer_lock: File,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct LayoutDocument {
    api_version: String,
    key_commitment: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Usage {
    pub(crate) bytes: u64,
    pub(crate) entries: u64,
    pub(crate) files: u64,
}

impl Layout {
    pub(crate) fn minimum_usage(key: &StorageKey) -> Result<Usage, StorageHostError> {
        let document = LayoutDocument {
            api_version: LAYOUT_VERSION.to_owned(),
            key_commitment: key.commitment(DOMAIN_AUTHORITY, &[b"layout-key-v1"]),
        };
        #[allow(
            clippy::map_err_ignore,
            reason = "serializing this owned two-string document has no failing case, and \
                      TryFromIntError carries only out-of-range, which Arithmetic already states"
        )]
        let encoded = u64::try_from(
            serde_json::to_vec(&document)
                .map_err(|_| StorageHostError::CorruptLayout)?
                .len(),
        )
        .map_err(|_| StorageHostError::Arithmetic)?
        .checked_add(1)
        .ok_or(StorageHostError::Arithmetic)?;
        Ok(Usage {
            bytes: 6_u64
                .checked_mul(ENTRY_CHARGE)
                .and_then(|bytes| bytes.checked_add(encoded))
                .ok_or(StorageHostError::Arithmetic)?,
            entries: 6,
            files: 2,
        })
    }

    pub(crate) fn open(root: &Path, key: &StorageKey) -> Result<Self, StorageHostError> {
        validate_ancestors(root)?;
        ensure_root_directory(root)?;
        let root = Directory::open_path(root, true)?;
        let initial_entries = root.entries_prefix(7)?;
        if initial_entries.len() > 6 {
            return Err(StorageHostError::CorruptLayout);
        }
        let has_layout = initial_entries.iter().any(|name| name == "layout");
        let has_writer = initial_entries.iter().any(|name| name == "writer.lock");

        // `layout` is the initialization commit point. Once it exists, every required root entry
        // must already exist and no unknown entry is accepted. Recreating a missing directory here
        // would turn retained-data loss into an apparently healthy empty store before key/layout
        // verification had a chance to fail closed.
        if has_layout && !has_writer {
            return Err(StorageHostError::CorruptLayout);
        }
        if !has_layout
            && !(initial_entries.is_empty()
                || initial_entries.as_slice() == ["writer.lock".to_owned()])
        {
            return Err(StorageHostError::CorruptLayout);
        }

        let writer = root.open_private("writer.lock", !has_writer)?;
        if writer
            .metadata()
            .map_err(|source| root.io_error(source))?
            .len()
            != 0
        {
            return Err(StorageHostError::CorruptLayout);
        }
        writer
            .try_lock()
            .map_err(|source| writer_lock_failure(&root, source))?;

        let key_commitment = key.commitment(DOMAIN_AUTHORITY, &[b"layout-key-v1"]);
        let (namespaces, transactions, quarantine, trash) = if has_layout {
            let encoded = root.read_bounded("layout", 4_096)?;
            let document: LayoutDocument = serde_json::from_slice(&encoded).map_err(|error| {
                crate::report_decode_failure("layout", &error);
                StorageHostError::CorruptLayout
            })?;
            if document.api_version != LAYOUT_VERSION || document.key_commitment != key_commitment {
                return Err(StorageHostError::KeyMismatch);
            }
            let expected = [
                "layout",
                "namespaces",
                "quarantine",
                "transactions",
                "trash",
                "writer.lock",
            ];
            let retained = root.entries_prefix(expected.len() as u64 + 1)?;
            if retained.len() != expected.len() || retained.iter().map(String::as_str).ne(expected)
            {
                return Err(StorageHostError::CorruptLayout);
            }
            (
                root.open_directory("namespaces")?,
                root.open_directory("transactions")?,
                root.open_directory("quarantine")?,
                root.open_directory("trash")?,
            )
        } else {
            let namespaces = root.ensure_directory("namespaces")?;
            let transactions = root.ensure_directory("transactions")?;
            let quarantine = root.ensure_directory("quarantine")?;
            let trash = root.ensure_directory("trash")?;
            let document = LayoutDocument {
                api_version: LAYOUT_VERSION.to_owned(),
                key_commitment,
            };
            #[allow(
                clippy::map_err_ignore,
                reason = "serializing this owned two-string document has no failing case"
            )]
            let mut encoded =
                serde_json::to_vec(&document).map_err(|_| StorageHostError::CorruptLayout)?;
            encoded.push(b'\n');
            let mut file = root.create_private("layout")?;
            file.write_all(&encoded)
                .and_then(|()| file.sync_all())
                .map_err(|source| root.io_error(source))?;
            root.sync()?;
            (namespaces, transactions, quarantine, trash)
        };

        Ok(Self {
            root,
            namespaces,
            transactions,
            quarantine,
            trash,
            _writer_lock: writer,
        })
    }

    pub(crate) const fn namespaces(&self) -> &Directory {
        &self.namespaces
    }

    pub(crate) const fn transactions(&self) -> &Directory {
        &self.transactions
    }

    pub(crate) const fn quarantine(&self) -> &Directory {
        &self.quarantine
    }

    pub(crate) const fn trash(&self) -> &Directory {
        &self.trash
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

    /// Restores owner-only traversal before moving an owner-owned corrupt directory to quarantine.
    pub(crate) fn make_owned_directory_traversable(
        &self,
        name: &str,
    ) -> Result<(), StorageHostError> {
        validate_component(name)?;
        let stat = rustix::fs::statat(self.file.as_ref(), name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|source| self.io_error(std::io::Error::from(source)))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
            || stat.st_uid != rustix::process::geteuid().as_raw()
        {
            return Err(StorageHostError::Corrupt {
                scope: "quarantine-directory",
            });
        }
        rustix::fs::chmodat(
            self.file.as_ref(),
            name,
            Mode::from_raw_mode(0o700),
            AtFlags::empty(),
        )
        .map_err(|source| self.io_error(std::io::Error::from(source)))
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

    /// Creates one directory without accepting an existing entry under the same token.
    ///
    /// Callers synchronize this directory's parent explicitly so a finalization deadline can be
    /// checked before that separate filesystem step begins.
    pub(crate) fn create_directory(&self, name: &str) -> Result<Self, StorageHostError> {
        validate_component(name)?;
        match rustix::fs::mkdirat(self.file.as_ref(), name, Mode::from_raw_mode(0o700)) {
            Ok(()) => self.open_directory(name),
            Err(rustix::io::Errno::EXIST) => Err(StorageHostError::AlreadyExists),
            Err(source) => Err(self.io_error(std::io::Error::from(source))),
        }
    }

    pub(crate) fn open_directory(&self, name: &str) -> Result<Self, StorageHostError> {
        self.open_directory_impl(name, true)
    }

    /// Opens quarantined corruption without accepting it back into the trusted layout.
    ///
    /// Owner/mode failures are exactly why an entry may have been quarantined, but its complete
    /// apparent size must still be charged. Type, no-follow, and before/after identity checks stay
    /// mandatory; an unreadable directory fails startup rather than disappearing from quota.
    fn open_quarantined_directory(&self, name: &str) -> Result<Self, StorageHostError> {
        self.open_directory_impl(name, false)
    }

    fn open_directory_impl(
        &self,
        name: &str,
        validate_private: bool,
    ) -> Result<Self, StorageHostError> {
        validate_component(name)?;
        let before = rustix::fs::statat(self.file.as_ref(), name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|source| self.io_error(std::io::Error::from(source)))?;
        if FileType::from_raw_mode(before.st_mode) != FileType::Directory {
            return Err(StorageHostError::Corrupt {
                scope: "directory-type",
            });
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = match rustix::fs::openat(self.file.as_ref(), name, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(source)
                if !validate_private
                    && matches!(source, rustix::io::Errno::ACCESS | rustix::io::Errno::PERM)
                    && before.st_uid == rustix::process::geteuid().as_raw() =>
            {
                // Permission corruption is quarantinable only if this process owns the directory.
                // Restore traverse permission inside quarantine so exact quota accounting can walk
                // the preserved contents; no symlink is followed and type/identity are rechecked.
                rustix::fs::chmodat(
                    self.file.as_ref(),
                    name,
                    Mode::from_raw_mode(0o700),
                    AtFlags::empty(),
                )
                .map_err(|source| self.io_error(std::io::Error::from(source)))?;
                rustix::fs::openat(self.file.as_ref(), name, flags, Mode::empty())
                    .map_err(|source| self.io_error(std::io::Error::from(source)))?
            }
            Err(source) => return Err(self.io_error(std::io::Error::from(source))),
        };
        let child = Self {
            file: Arc::new(File::from(fd)),
            diagnostic_path: Arc::new(self.diagnostic_child(name)),
        };
        if validate_private {
            child.validate_self(true)?;
        }
        let opened = child
            .file
            .metadata()
            .map_err(|source| child.io_error(source))?;
        if !opened.is_dir()
            || opened.dev() != before.st_dev as u64
            || opened.ino() != before.st_ino as u64
        {
            return Err(StorageHostError::Corrupt {
                scope: "directory-identity",
            });
        }
        Ok(child)
    }

    /// Revalidates that a retained child descriptor is still the entry named by this parent.
    ///
    /// This check runs after an advisory-lease wait. Without it, GC could rename an already-open
    /// base directory to trash while a waiter later acquired the lock on the now-unlinked inode.
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

    pub(crate) fn entries(&self) -> Result<Vec<String>, StorageHostError> {
        self.read_entries(HARD_MAX_DIRECTORY_ENTRIES, true)
    }

    pub(crate) fn entry_stream(&self) -> Result<EntryStream, StorageHostError> {
        Ok(EntryStream {
            directory: self.clone(),
            inner: rustix::fs::Dir::read_from(self.file.as_ref())
                .map_err(|source| self.io_error(std::io::Error::from(source)))?,
        })
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
            let name = entry
                .file_name()
                .to_str()
                .map_err(|_| StorageHostError::Corrupt {
                    scope: "non-utf8-entry",
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
        if !metadata.is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(StorageHostError::Corrupt {
                scope: "private-file",
            });
        }
        let stat = rustix::fs::statat(self.file.as_ref(), name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|source| self.io_error(std::io::Error::from(source)))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
            || metadata.dev() != stat.st_dev as u64
            || metadata.ino() != stat.st_ino as u64
        {
            return Err(StorageHostError::Corrupt {
                scope: "private-file-identity",
            });
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
            return Err(StorageHostError::Corrupt {
                scope: "oversized-file",
            });
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
            return Err(StorageHostError::Corrupt {
                scope: "oversized-file",
            });
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

    pub(crate) fn remove_tree(&self, name: &str) -> Result<(), StorageHostError> {
        validate_component(name)?;
        let Some(metadata) = self.metadata(name)? else {
            return Ok(());
        };
        match metadata.kind {
            EntryKind::Directory => {
                let child = self.open_directory(name)?;
                for entry in child.entries()? {
                    child.remove_tree(&entry)?;
                }
                drop(child);
                rustix::fs::unlinkat(self.file.as_ref(), name, AtFlags::REMOVEDIR)
                    .map_err(|source| self.io_error(std::io::Error::from(source)))
            }
            EntryKind::File => self.remove_file(name),
            EntryKind::Symlink | EntryKind::Other => Err(StorageHostError::Corrupt {
                scope: "remove-tree-type",
            }),
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
        return Err(StorageHostError::Corrupt {
            scope: "physical-component",
        });
    }
    Ok(())
}

fn validate_ancestors(path: &Path) -> Result<(), StorageHostError> {
    let mut current = path.parent();
    while let Some(ancestor) = current {
        let metadata =
            fs::symlink_metadata(ancestor).map_err(|source| StorageHostError::RootIo {
                path: ancestor.to_path_buf(),
                source,
            })?;
        let mode = metadata.permissions().mode();
        let sticky = mode & 0o1000 != 0;
        if !metadata.is_dir() || (mode & 0o022 != 0 && !sticky) {
            return Err(StorageHostError::UnsafeRoot {
                path: ancestor.to_path_buf(),
            });
        }
        current = ancestor.parent();
    }
    Ok(())
}

pub(crate) fn scan_usage(
    directory: &Directory,
    maximum_entries: u64,
) -> Result<Usage, StorageHostError> {
    scan_usage_checked(directory, maximum_entries, || Ok(()))
}

/// Scans no more than the configured entry/byte budget.
///
/// `None` means the subtree cannot fit this GC pass. Traversal stops as soon as that is known,
/// rather than walking a large trash tree before applying the byte limit.
pub(crate) fn scan_usage_capped(
    directory: &Directory,
    maximum_entries: u64,
    maximum_bytes: u64,
) -> Result<Option<Usage>, StorageHostError> {
    let mut usage = Usage::default();
    if scan_capped(directory, maximum_entries, maximum_bytes, &mut usage)? {
        Ok(Some(usage))
    } else {
        Ok(None)
    }
}

fn scan_capped(
    directory: &Directory,
    maximum_entries: u64,
    maximum_bytes: u64,
    usage: &mut Usage,
) -> Result<bool, StorageHostError> {
    let remaining = maximum_entries.saturating_sub(usage.entries);
    let byte_slots = maximum_bytes
        .saturating_sub(usage.bytes)
        .checked_div(ENTRY_CHARGE)
        .unwrap_or(0);
    let bounded = remaining.min(byte_slots);
    let entries = directory.entries_prefix(bounded.saturating_add(1))?;
    if entries.len() as u64 > bounded {
        return Ok(false);
    }
    for name in entries {
        let metadata = directory
            .metadata(&name)?
            .ok_or(StorageHostError::Corrupt {
                scope: "vanished-entry",
            })?;
        usage.entries = usage
            .entries
            .checked_add(1)
            .ok_or(StorageHostError::Arithmetic)?;
        usage.bytes = usage
            .bytes
            .checked_add(ENTRY_CHARGE)
            .ok_or(StorageHostError::Arithmetic)?;
        if usage.bytes > maximum_bytes {
            return Ok(false);
        }
        match metadata.kind {
            EntryKind::File => {
                let next = usage
                    .bytes
                    .checked_add(metadata.len)
                    .ok_or(StorageHostError::Arithmetic)?;
                if next > maximum_bytes {
                    return Ok(false);
                }
                let file = directory.open_private(&name, false)?;
                directory.validate_private_file(&name, &file)?;
                usage.bytes = next;
                usage.files = usage
                    .files
                    .checked_add(1)
                    .ok_or(StorageHostError::Arithmetic)?;
            }
            EntryKind::Directory => {
                let child = directory.open_directory(&name)?;
                if !scan_capped(&child, maximum_entries, maximum_bytes, usage)? {
                    return Ok(false);
                }
            }
            EntryKind::Symlink => {
                return Err(StorageHostError::Corrupt { scope: "symlink" });
            }
            EntryKind::Other => {
                return Err(StorageHostError::Corrupt { scope: "file-type" });
            }
        }
    }
    Ok(true)
}

/// Scans exact logical usage while checking a caller-owned deadline before each filesystem step.
///
/// One native operation may still block past the deadline. The callback prevents beginning the
/// next entry/open after that operation drains.
pub(crate) fn scan_usage_checked(
    directory: &Directory,
    maximum_entries: u64,
    mut check: impl FnMut() -> Result<(), StorageHostError>,
) -> Result<Usage, StorageHostError> {
    let mut usage = Usage::default();
    scan(directory, maximum_entries, &mut usage, &mut check)?;
    Ok(usage)
}

fn scan(
    directory: &Directory,
    maximum_entries: u64,
    usage: &mut Usage,
    check: &mut impl FnMut() -> Result<(), StorageHostError>,
) -> Result<(), StorageHostError> {
    check()?;
    let remaining = maximum_entries.saturating_sub(usage.entries);
    let entries = directory.entries_prefix(remaining.saturating_add(1))?;
    if entries.len() as u64 > remaining {
        return Err(StorageHostError::StartupEntryLimit {
            count: usage.entries.saturating_add(entries.len() as u64),
            maximum: maximum_entries,
        });
    }
    for name in entries {
        check()?;
        let metadata = directory
            .metadata(&name)?
            .ok_or(StorageHostError::Corrupt {
                scope: "vanished-entry",
            })?;
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
                check()?;
                let file = directory.open_private(&name, false)?;
                directory.validate_private_file(&name, &file)?;
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
                check()?;
                let child = directory.open_directory(&name)?;
                scan(&child, maximum_entries, usage, check)?;
            }
            EntryKind::Symlink => {
                return Err(StorageHostError::Corrupt { scope: "symlink" });
            }
            EntryKind::Other => {
                return Err(StorageHostError::Corrupt { scope: "file-type" });
            }
        }
    }
    Ok(())
}

/// Counts quarantined bytes without trusting their shape or following any link.
///
/// Corrupt entries remain quota-accounted; symlinks contribute only their own entry charge and
/// are never traversed.
pub(crate) fn scan_quarantine_usage(
    directory: &Directory,
    maximum_entries: u64,
) -> Result<Usage, StorageHostError> {
    let mut usage = Usage::default();
    scan_quarantine(directory, maximum_entries, &mut usage)?;
    Ok(usage)
}

fn scan_quarantine(
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
        let metadata = directory
            .metadata(&name)?
            .ok_or(StorageHostError::Corrupt {
                scope: "vanished-quarantine-entry",
            })?;
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
                let child = directory.open_quarantined_directory(&name)?;
                scan_quarantine(&child, maximum_entries, usage)?;
            }
            EntryKind::Symlink | EntryKind::Other => {
                // Never follow the entry, but do charge its own apparent bytes in addition to the
                // universal entry charge. Corruption cannot become free space by changing type.
                usage.bytes = usage
                    .bytes
                    .checked_add(metadata.len)
                    .ok_or(StorageHostError::Arithmetic)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn add_usage(left: Usage, right: Usage) -> Result<Usage, StorageHostError> {
    Ok(Usage {
        bytes: left
            .bytes
            .checked_add(right.bytes)
            .ok_or(StorageHostError::Arithmetic)?,
        entries: left
            .entries
            .checked_add(right.entries)
            .ok_or(StorageHostError::Arithmetic)?,
        files: left
            .files
            .checked_add(right.files)
            .ok_or(StorageHostError::Arithmetic)?,
    })
}

pub(crate) fn scan_root_usage(
    layout: &Layout,
    maximum_entries: u64,
) -> Result<Usage, StorageHostError> {
    let mut usage = Usage::default();
    for name in layout.root.entries_bounded(maximum_entries)? {
        let metadata = layout
            .root
            .metadata(&name)?
            .ok_or(StorageHostError::Corrupt {
                scope: "vanished-root-entry",
            })?;
        usage.entries = usage
            .entries
            .checked_add(1)
            .ok_or(StorageHostError::Arithmetic)?;
        usage.bytes = usage
            .bytes
            .checked_add(ENTRY_CHARGE)
            .ok_or(StorageHostError::Arithmetic)?;
        let child = match metadata.kind {
            EntryKind::File => {
                let _ = layout.root.open_private(&name, false)?;
                Usage {
                    bytes: metadata.len,
                    entries: 0,
                    files: 1,
                }
            }
            EntryKind::Directory => {
                let directory = layout.root.open_directory(&name)?;
                if name == "quarantine" {
                    scan_quarantine_usage(&directory, maximum_entries)?
                } else {
                    scan_usage(&directory, maximum_entries)?
                }
            }
            EntryKind::Symlink => {
                return Err(StorageHostError::Corrupt { scope: "symlink" });
            }
            EntryKind::Other => {
                return Err(StorageHostError::Corrupt { scope: "file-type" });
            }
        };
        usage = add_usage(usage, child)?;
        if usage.entries > maximum_entries {
            return Err(StorageHostError::StartupEntryLimit {
                count: usage.entries,
                maximum: maximum_entries,
            });
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
