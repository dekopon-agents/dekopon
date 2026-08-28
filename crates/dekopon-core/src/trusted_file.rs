//! One definition of what makes a local file trusted input.
//!
//! The same predicate was hand-written at every site that reads owner-authored state — open
//! without following a symlink, refuse anything that is not a regular file, require this process's
//! UID, refuse a permission bit outside the owner, require exactly one hard link, and bound the
//! read — and the permission mask silently differed between copies with nothing naming the two
//! tiers. Both tiers are here, named, with the reason they differ in [`FileTier`].
//!
//! This is Unix-only: every caller is a Unix-only process, and `O_NOFOLLOW`, an owning UID, and a
//! permission mask have no portable equivalent worth pretending to.

use std::{
    fmt,
    fs::{self, Metadata},
    io::Read as _,
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

use thiserror::Error;

/// How far outside its owner a trusted file may be reachable.
///
/// The two tiers exist because reading a file and trusting a file are different risks. Anything
/// holding secret material must also be unreadable outside its owner, because disclosure alone is
/// the loss; anything merely *authored* by the owner only has to be unwritable, because the risk is
/// another user editing what this process will obey.
///
/// - [`Private`](Self::Private) — `mode & 0o077` must be zero: the broker credentials file, the
///   secret map, the audit checkpoint, the control socket, and storage namespace keys.
/// - [`NotWorldWritable`](Self::NotWorldWritable) — `mode & 0o022` must be zero: `broker.yaml`,
///   `dekopond.yaml`, the Cedar policy file, and managed provider state. These are readable
///   configuration by design, and several deployments hand them to an operator group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileTier {
    /// Owner-only. Nothing outside the owner may read or write it.
    Private,
    /// Owner-writable. Group and world may read it but must not write it.
    NotWorldWritable,
}

impl FileTier {
    /// Permission bits this tier refuses.
    #[must_use]
    pub const fn forbidden_bits(self) -> u32 {
        match self {
            Self::Private => 0o077,
            Self::NotWorldWritable => 0o022,
        }
    }
}

impl fmt::Display for FileTier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Private => "private",
            Self::NotWorldWritable => "not-world-writable",
        })
    }
}

/// Why a path is not trusted input, and what was observed instead.
///
/// Callers map this into their own error types. Several of them deliberately collapse it — a
/// secret-map source must not tell a caller which check refused it — so every variant carries
/// enough for the collapsing site to log the reason it is dropping.
#[derive(Debug, Error)]
pub enum FileHygieneError {
    /// The path resolved to something that is not a regular file.
    #[error("{} is a {observed}, not a regular file", path.display())]
    NotRegular {
        /// The rejected path.
        path: PathBuf,
        /// What the path actually is.
        observed: &'static str,
    },
    /// A permission bit the tier forbids was set.
    #[error(
        "{} has mode {mode:04o}; a {tier} file must clear {forbidden:03o}",
        path.display()
    )]
    InsecureMode {
        /// The rejected path.
        path: PathBuf,
        /// Tier that was required.
        tier: FileTier,
        /// Observed permission bits.
        mode: u32,
        /// Bits the tier refuses.
        forbidden: u32,
    },
    /// The file belongs to another user.
    #[error("{} is owned by uid {owner}, not uid {expected}", path.display())]
    WrongOwner {
        /// The rejected path.
        path: PathBuf,
        /// Observed owning UID.
        owner: u32,
        /// UID the caller requires.
        expected: u32,
    },
    /// The file has another name elsewhere, so another directory's permissions also govern it.
    #[error("{} has {links} hard links; a trusted file has exactly one", path.display())]
    HardLinked {
        /// The rejected path.
        path: PathBuf,
        /// Observed link count.
        links: u64,
    },
    /// The file is larger than the caller agreed to read.
    #[error("{} is {length} bytes; the maximum is {maximum}", path.display())]
    TooLarge {
        /// The rejected path.
        path: PathBuf,
        /// Observed length.
        length: u64,
        /// Caller's bound.
        maximum: usize,
    },
    /// Opening, inspecting, or reading the file failed.
    #[error("could not read {}", path.display())]
    Io {
        /// The path being read.
        path: PathBuf,
        /// Underlying failure.
        #[source]
        source: std::io::Error,
    },
}

impl FileHygieneError {
    /// Stable, low-cardinality name for which check refused the file.
    ///
    /// A site that collapses several causes into one opaque error logs this instead of the
    /// rendered message, which carries a path.
    #[must_use]
    pub const fn category(&self) -> &'static str {
        match self {
            Self::NotRegular { .. } => "not-regular",
            Self::InsecureMode { .. } => "insecure-mode",
            Self::WrongOwner { .. } => "wrong-owner",
            Self::HardLinked { .. } => "hard-linked",
            Self::TooLarge { .. } => "too-large",
            Self::Io { .. } => "io",
        }
    }

    /// The path that was refused.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::NotRegular { path, .. }
            | Self::InsecureMode { path, .. }
            | Self::WrongOwner { path, .. }
            | Self::HardLinked { path, .. }
            | Self::TooLarge { path, .. }
            | Self::Io { path, .. } => path,
        }
    }
}

/// Refuses metadata that is not a regular, single-link, `expected_uid`-owned file at `tier`.
///
/// Use this where the file is already open — a descriptor obtained relative to a validated parent,
/// or a path being removed rather than read. [`read_trusted_file`] applies it to a file it opens.
/// `path` names the file for the error only; nothing here touches the filesystem.
///
/// # Errors
///
/// Returns the first failing check as a [`FileHygieneError`]: not a regular file, wrong owner, a
/// permission bit the tier forbids, or more than one hard link.
pub fn check_trusted_metadata(
    path: &Path,
    metadata: &Metadata,
    expected_uid: u32,
    tier: FileTier,
) -> Result<(), FileHygieneError> {
    let file_type = metadata.file_type();
    if !file_type.is_file() {
        return Err(FileHygieneError::NotRegular {
            path: path.to_path_buf(),
            observed: describe(metadata),
        });
    }
    if metadata.uid() != expected_uid {
        return Err(FileHygieneError::WrongOwner {
            path: path.to_path_buf(),
            owner: metadata.uid(),
            expected: expected_uid,
        });
    }
    let mode = metadata.permissions().mode() & 0o7777;
    let forbidden = tier.forbidden_bits();
    if mode & forbidden != 0 {
        return Err(FileHygieneError::InsecureMode {
            path: path.to_path_buf(),
            tier,
            mode,
            forbidden,
        });
    }
    if metadata.nlink() != 1 {
        return Err(FileHygieneError::HardLinked {
            path: path.to_path_buf(),
            links: metadata.nlink(),
        });
    }
    Ok(())
}

/// Opens `path` without following a symlink, applies [`check_trusted_metadata`], and reads it.
///
/// The length is checked twice on purpose: once against the metadata, so an oversized file is
/// refused before any of it is read, and once against the bytes actually delivered, because a file
/// can grow between the two.
///
/// This blocks. Every current caller is inside a Tokio runtime and wraps it in one
/// `spawn_blocking`, which is both what `tokio::fs` does internally and one hop instead of the four
/// an open, a stat, and a chunked read would each take.
///
/// # Errors
///
/// Returns [`FileHygieneError::Io`] when the file cannot be opened, inspected, or read,
/// [`FileHygieneError::TooLarge`] when it exceeds `max_bytes`, and otherwise whatever
/// [`check_trusted_metadata`] refused.
pub fn read_trusted_file(
    path: &Path,
    expected_uid: u32,
    tier: FileTier,
    max_bytes: usize,
) -> Result<Vec<u8>, FileHygieneError> {
    let io_error = |source| FileHygieneError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut options = fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path).map_err(io_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    check_trusted_metadata(path, &metadata, expected_uid, tier)?;
    let too_large = |length| FileHygieneError::TooLarge {
        path: path.to_path_buf(),
        length,
        maximum: max_bytes,
    };
    if metadata.len() > max_bytes as u64 {
        return Err(too_large(metadata.len()));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > max_bytes {
        return Err(too_large(bytes.len() as u64));
    }
    Ok(bytes)
}

fn describe(metadata: &Metadata) -> &'static str {
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        "directory"
    } else if file_type.is_symlink() {
        "symbolic link"
    } else {
        use std::os::unix::fs::FileTypeExt as _;
        if file_type.is_socket() {
            "socket"
        } else if file_type.is_fifo() {
            "named pipe"
        } else if file_type.is_block_device() {
            "block device"
        } else if file_type.is_char_device() {
            "character device"
        } else {
            "special file"
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, Permissions},
        io::Write as _,
        os::unix::fs::{MetadataExt as _, PermissionsExt as _},
        path::PathBuf,
    };

    use tempfile::TempDir;

    use super::{FileHygieneError, FileTier, read_trusted_file};

    struct Fixture {
        root: TempDir,
        path: PathBuf,
        uid: u32,
    }

    /// Creates one file and reads its owner back from the filesystem.
    ///
    /// This crate forbids unsafe, so there is no `getuid` call available; the UID of a file this
    /// process just created is the same answer.
    fn fixture(mode: u32, contents: &[u8]) -> Fixture {
        let root = tempfile::tempdir().expect("temporary directory");
        let path = root.path().join("trusted");
        let mut file = fs::File::create(&path).expect("fixture file");
        file.write_all(contents).expect("fixture contents");
        fs::set_permissions(&path, Permissions::from_mode(mode)).expect("fixture mode");
        let uid = fs::metadata(&path).expect("fixture metadata").uid();
        Fixture { root, path, uid }
    }

    /// Every refusal reports which check failed, so a collapsing caller can still log the cause.
    #[test]
    fn each_refusal_names_the_check_that_failed() {
        let readable = fixture(0o644, b"contents");
        assert_eq!(
            read_trusted_file(&readable.path, readable.uid, FileTier::Private, 8)
                .expect_err("0o644 is readable by group and world")
                .category(),
            "insecure-mode"
        );
        assert_eq!(
            read_trusted_file(&readable.path, readable.uid, FileTier::NotWorldWritable, 8)
                .expect("0o644 is not world-writable"),
            b"contents"
        );

        let private = fixture(0o600, b"contents");
        assert_eq!(
            read_trusted_file(
                &private.path,
                private.uid.wrapping_add(1),
                FileTier::Private,
                8
            )
            .expect_err("another uid owns nothing here")
            .category(),
            "wrong-owner"
        );
        assert_eq!(
            read_trusted_file(&private.path, private.uid, FileTier::Private, 7)
                .expect_err("eight bytes exceed a seven-byte bound")
                .category(),
            "too-large"
        );

        let linked = fixture(0o600, b"contents");
        fs::hard_link(&linked.path, linked.root.path().join("alias")).expect("hard link");
        assert_eq!(
            read_trusted_file(&linked.path, linked.uid, FileTier::Private, 8)
                .expect_err("a second name means a second directory governs it")
                .category(),
            "hard-linked"
        );

        let error = read_trusted_file(private.root.path(), private.uid, FileTier::Private, 8)
            .expect_err("a directory is not a regular file");
        // Opening a directory read-only succeeds on Linux and fails with EISDIR elsewhere; either
        // way the caller must not be told it read a trusted file.
        assert!(
            matches!(
                error,
                FileHygieneError::NotRegular { .. } | FileHygieneError::Io { .. }
            ),
            "{error}"
        );

        let missing = private.root.path().join("absent");
        assert_eq!(
            read_trusted_file(&missing, private.uid, FileTier::Private, 8)
                .expect_err("a missing file is an I/O failure")
                .category(),
            "io"
        );
    }

    /// A symlink to a perfectly good file is still refused: `O_NOFOLLOW` is the whole point.
    #[test]
    fn a_symlink_is_refused_without_being_followed() {
        let target = fixture(0o600, b"contents");
        let link = target.root.path().join("link");
        std::os::unix::fs::symlink(&target.path, &link).expect("symlink");

        let error = read_trusted_file(&link, target.uid, FileTier::Private, 8)
            .expect_err("O_NOFOLLOW refuses the open");

        assert_eq!(error.category(), "io");
        assert_eq!(error.path(), link);
    }
}
