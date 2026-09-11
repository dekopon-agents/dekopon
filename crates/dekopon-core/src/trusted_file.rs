//! One definition of what makes a local file trusted input.
//!
//! The same predicate was hand-written at every site that reads owner-authored state — open
//! without following a symlink, refuse anything that is not a regular file, require this process's
//! UID, refuse a permission bit outside the owner, require exactly one hard link, and bound the
//! read — and the permission mask silently differed between copies with nothing naming the two
//! tiers. Both tiers are here, named, with the reason they differ in [`FileTier`].
//!
//! The directories above the file are the other half of the same question, and were hand-written
//! four more times — the storage layout, the namespace key, the provider store, and the broker
//! socket — differing in whether they canonicalized first, in whether they inspected the path
//! itself, and in which error they raised. [`check_trusted_ancestors`] is the one walk; the two
//! decisions that legitimately differ between those callers are its parameters.
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
///   secret map and the file sources it names, the provider store's
///   operation lock, and every private file inside a storage root.
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
    /// A directory on the path to the file may be rewritten by someone other than its owner.
    ///
    /// The file below such a directory carries no more authority than the directory does: whoever
    /// can write it can rename the file away and leave their own in its place, and no check on the
    /// file itself sees that happen. The sticky bit is tolerated, because it is exactly the bit
    /// that makes a shared `/tmp` and the per-user directories under it safe to sit beneath.
    #[error(
        "{} is a {observed} with mode {mode:04o}; an ancestor of a trusted path must be a \
         directory that is not group- or world-writable unless it is sticky",
        path.display()
    )]
    UnsafeAncestor {
        /// The rejected ancestor.
        path: PathBuf,
        /// Observed permission bits, including the sticky bit.
        mode: u32,
        /// What the ancestor actually is.
        observed: &'static str,
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
            Self::UnsafeAncestor { .. } => "unsafe-ancestor",
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
            | Self::UnsafeAncestor { path, .. }
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

/// Whether the ancestor walk resolves the path first or inspects it as the operator wrote it.
///
/// [`AsWritten`](Self::AsWritten) refuses a symlinked ancestor where it stands, which is what a
/// caller needs when the configured spelling is itself the thing being authorized — resolving the
/// alias first would authorize a directory the operator never named.
/// [`Canonical`](Self::Canonical) resolves aliases such as macOS's `/var -> /private/var` and
/// inspects the real directories, which is what a caller needs when the path is a store it
/// manages rather than a spelling it must preserve.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AncestorResolution {
    /// Walk the path exactly as given. Any symlinked ancestor is refused, not followed.
    AsWritten,
    /// Canonicalize the path before walking it. This requires the path to exist.
    Canonical,
}

/// Whether the ancestor walk inspects the path itself or only the directories above it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AncestorScope {
    /// Inspect `path` first, then every directory above it.
    PathAndAbove,
    /// Inspect only the directories above `path`. For a caller that is about to create `path`, or
    /// that inspects it separately under rules stricter than an ancestor's.
    Above,
}

/// Refuses a path whose ancestry would let another user substitute what sits under it.
///
/// One walk, four callers. A group- or world-writable ancestor is refused unless it is sticky, and
/// an ancestor that is not a directory is refused outright — a symlink included, because
/// `symlink_metadata` reports the link rather than what it points at.
///
/// `resolution` and `scope` are the two decisions that legitimately differ between callers; every
/// other part of the rule is the same everywhere, which is the point of this function existing.
///
/// # Errors
///
/// Returns [`FileHygieneError::UnsafeAncestor`] naming the first ancestor that fails the rule, and
/// [`FileHygieneError::Io`] when an ancestor cannot be inspected or, under
/// [`AncestorResolution::Canonical`], when `path` cannot be resolved. Those two are the only
/// variants this function produces; a caller that maps them may treat anything else as a refusal.
pub fn check_trusted_ancestors(
    path: &Path,
    resolution: AncestorResolution,
    scope: AncestorScope,
) -> Result<(), FileHygieneError> {
    let canonical;
    let start = match resolution {
        AncestorResolution::AsWritten => path,
        AncestorResolution::Canonical => {
            canonical = fs::canonicalize(path).map_err(|source| FileHygieneError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            canonical.as_path()
        }
    };
    let mut current = match scope {
        AncestorScope::PathAndAbove => Some(start),
        AncestorScope::Above => start.parent(),
    };
    while let Some(ancestor) = current {
        let metadata = fs::symlink_metadata(ancestor).map_err(|source| FileHygieneError::Io {
            path: ancestor.to_path_buf(),
            source,
        })?;
        let mode = metadata.permissions().mode() & 0o7777;
        let writable_outside_owner = mode & 0o022 != 0;
        let sticky = mode & 0o1000 != 0;
        if !metadata.is_dir() || (writable_outside_owner && !sticky) {
            return Err(FileHygieneError::UnsafeAncestor {
                path: ancestor.to_path_buf(),
                mode,
                observed: describe(&metadata),
            });
        }
        current = ancestor.parent();
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
    if file_type.is_file() {
        "regular file"
    } else if file_type.is_dir() {
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

    use super::{
        AncestorResolution, AncestorScope, FileHygieneError, FileTier, check_trusted_ancestors,
        read_trusted_file,
    };

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

    /// A temporary root with no symlink left in it.
    ///
    /// `TMPDIR` is reached through `/var -> /private/var` on macOS, and an `AsWritten` walk
    /// refuses a symlinked ancestor by design, so every fixture below starts from the resolved
    /// spelling. That is the same thing the storage-host callers require of their operators.
    fn resolved_root() -> (TempDir, PathBuf) {
        let root = tempfile::tempdir().expect("temporary directory");
        let resolved = root
            .path()
            .canonicalize()
            .expect("canonical temporary root");
        (root, resolved)
    }

    /// `AsWritten` + `Above`, as the storage root walks it: the path itself is never inspected.
    ///
    /// The root may not exist yet — `Layout::open` creates it — and once it does it is held to
    /// owner-only rules an ancestor is not held to.
    #[test]
    fn an_above_walk_refuses_the_parent_and_ignores_the_path_itself() {
        let (_root, resolved) = resolved_root();
        let parent = resolved.join("parent");
        fs::create_dir(&parent).expect("parent");
        let target = parent.join("root");

        // The target does not exist and is not consulted.
        check_trusted_ancestors(&target, AncestorResolution::AsWritten, AncestorScope::Above)
            .expect("a clean ancestry with no target yet");

        // Neither is a world-writable target of the caller's own.
        fs::create_dir(&target).expect("target");
        fs::set_permissions(&target, Permissions::from_mode(0o777)).expect("target mode");
        check_trusted_ancestors(&target, AncestorResolution::AsWritten, AncestorScope::Above)
            .expect("the path itself is the caller's business");

        fs::set_permissions(&parent, Permissions::from_mode(0o777)).expect("parent mode");
        let error =
            check_trusted_ancestors(&target, AncestorResolution::AsWritten, AncestorScope::Above)
                .expect_err("a world-writable parent may be replaced under the root");
        assert_eq!(error.category(), "unsafe-ancestor");
        assert_eq!(error.path(), parent);
    }

    /// `AsWritten` + `PathAndAbove`, as the namespace key and the broker socket walk it.
    ///
    /// The key's caller passes the directory holding the key, and the socket's callers pass an
    /// already-canonicalized parent; both need that directory inspected, not just what is above
    /// it. A symlinked ancestor is refused where it stands rather than resolved.
    #[test]
    fn a_path_and_above_walk_refuses_the_path_itself_and_any_symlinked_ancestor() {
        let (_root, resolved) = resolved_root();
        let directory = resolved.join("holder");
        fs::create_dir(&directory).expect("holder");
        fs::set_permissions(&directory, Permissions::from_mode(0o700)).expect("holder mode");
        check_trusted_ancestors(
            &directory,
            AncestorResolution::AsWritten,
            AncestorScope::PathAndAbove,
        )
        .expect("a clean ancestry");

        fs::set_permissions(&directory, Permissions::from_mode(0o777)).expect("holder mode");
        let error = check_trusted_ancestors(
            &directory,
            AncestorResolution::AsWritten,
            AncestorScope::PathAndAbove,
        )
        .expect_err("the path itself is inspected here");
        assert_eq!(error.path(), directory);

        fs::set_permissions(&directory, Permissions::from_mode(0o700)).expect("holder mode");
        let alias = resolved.join("alias");
        std::os::unix::fs::symlink(&directory, &alias).expect("ancestor symlink");
        // `key.rs` passes the key file's parent, which is the alias itself: `symlink_metadata`
        // reports the link, a link is not a directory, and the walk stops there rather than
        // authorizing a directory the operator never spelled.
        let error = check_trusted_ancestors(
            &alias,
            AncestorResolution::AsWritten,
            AncestorScope::PathAndAbove,
        )
        .expect_err("an alias is not the directory the operator named");
        assert_eq!(error.category(), "unsafe-ancestor");
        assert_eq!(error.path(), alias);
        assert!(error.to_string().contains("symbolic link"), "{error}");
    }

    /// `Canonical` + `PathAndAbove`, as the provider store walks it.
    ///
    /// The store is a directory the broker manages rather than a spelling it must preserve, so an
    /// alias is resolved and the real directories are the ones inspected. Resolution needs the
    /// path to exist, which is the one thing this combination refuses that the others do not.
    #[test]
    fn a_canonical_walk_resolves_an_alias_and_requires_the_path_to_exist() {
        let (_root, resolved) = resolved_root();
        let directory = resolved.join("store");
        fs::create_dir(&directory).expect("store");
        fs::set_permissions(&directory, Permissions::from_mode(0o700)).expect("store mode");
        let alias = resolved.join("alias");
        std::os::unix::fs::symlink(&directory, &alias).expect("store symlink");

        check_trusted_ancestors(
            &alias,
            AncestorResolution::Canonical,
            AncestorScope::PathAndAbove,
        )
        .expect("the alias resolves onto a clean ancestry");

        fs::set_permissions(&directory, Permissions::from_mode(0o777)).expect("store mode");
        let error = check_trusted_ancestors(
            &alias,
            AncestorResolution::Canonical,
            AncestorScope::PathAndAbove,
        )
        .expect_err("the resolved directory is world-writable");
        assert_eq!(error.category(), "unsafe-ancestor");
        assert_eq!(error.path(), directory);

        let error = check_trusted_ancestors(
            &resolved.join("absent"),
            AncestorResolution::Canonical,
            AncestorScope::PathAndAbove,
        )
        .expect_err("nothing to resolve");
        assert_eq!(error.category(), "io");
    }

    /// The sticky bit is tolerated, in every combination, on purpose.
    ///
    /// `/tmp` is world-writable and sticky, and so is the per-user directory `TMPDIR` names on
    /// many systems. Refusing it would refuse the ordinary case; tolerating it is the reason the
    /// rule says "non-sticky" rather than "not world-writable".
    #[test]
    fn a_sticky_world_writable_ancestor_is_tolerated() {
        let (_root, resolved) = resolved_root();
        let shared = resolved.join("shared");
        fs::create_dir(&shared).expect("shared");
        fs::set_permissions(&shared, Permissions::from_mode(0o1777)).expect("sticky mode");
        let owned = shared.join("owned");
        fs::create_dir(&owned).expect("owned");
        fs::set_permissions(&owned, Permissions::from_mode(0o700)).expect("owned mode");

        check_trusted_ancestors(
            &owned,
            AncestorResolution::AsWritten,
            AncestorScope::PathAndAbove,
        )
        .expect("sticky is what makes a shared directory safe to sit beneath");
        check_trusted_ancestors(&owned, AncestorResolution::AsWritten, AncestorScope::Above)
            .expect("and the same walk from above");
        check_trusted_ancestors(
            &owned,
            AncestorResolution::Canonical,
            AncestorScope::PathAndAbove,
        )
        .expect("and after resolution");
    }

    /// An ancestor that is not a directory is refused, and the message says what it is.
    #[test]
    fn a_non_directory_ancestor_is_refused_and_described() {
        let (_root, resolved) = resolved_root();
        let file = resolved.join("not-a-directory");
        fs::write(&file, b"contents").expect("file");

        let error = check_trusted_ancestors(
            &file,
            AncestorResolution::AsWritten,
            AncestorScope::PathAndAbove,
        )
        .expect_err("a regular file is not a directory");
        assert_eq!(error.category(), "unsafe-ancestor");
        assert_eq!(error.path(), file);
        assert!(error.to_string().contains("regular file"), "{error}");
    }
}
