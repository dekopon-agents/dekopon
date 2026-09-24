use std::{
    fmt,
    fs::{self, Metadata},
    io::Read as _,
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileTier {
    Private,
    NotWorldWritable,
}

impl FileTier {
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

#[derive(Debug, Error)]
pub enum FileHygieneError {
    #[error("{} is a {observed}, not a regular file", path.display())]
    NotRegular {
        path: PathBuf,
        observed: &'static str,
    },
    #[error(
        "{} has mode {mode:04o}; a {tier} file must clear {forbidden:03o}",
        path.display()
    )]
    InsecureMode {
        path: PathBuf,
        tier: FileTier,
        mode: u32,
        forbidden: u32,
    },
    #[error("{} is owned by uid {owner}, not uid {expected}", path.display())]
    WrongOwner {
        path: PathBuf,
        owner: u32,
        expected: u32,
    },
    #[error("{} has {links} hard links; a trusted file has exactly one", path.display())]
    HardLinked { path: PathBuf, links: u64 },
    #[error("{} is {length} bytes; the maximum is {maximum}", path.display())]
    TooLarge {
        path: PathBuf,
        length: u64,
        maximum: usize,
    },
    #[error(
        "{} is a {observed} with mode {mode:04o}; an ancestor must be a directory that is not \
         group- or world-writable unless it is sticky",
        path.display()
    )]
    UnsafeAncestor {
        path: PathBuf,
        mode: u32,
        observed: &'static str,
    },
    #[error("could not read {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl FileHygieneError {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AncestorPolicy {
    pub canonicalize: bool,
    pub include_self: bool,
}

pub fn check_trusted_ancestors(
    path: &Path,
    policy: AncestorPolicy,
) -> Result<(), FileHygieneError> {
    let walked = if policy.canonicalize {
        fs::canonicalize(path).map_err(|source| FileHygieneError::Io {
            path: path.to_path_buf(),
            source,
        })?
    } else {
        path.to_path_buf()
    };
    for ancestor in walked.ancestors().skip(usize::from(!policy.include_self)) {
        let metadata = fs::symlink_metadata(ancestor).map_err(|source| FileHygieneError::Io {
            path: ancestor.to_path_buf(),
            source,
        })?;
        let mode = metadata.permissions().mode() & 0o7777;
        if !metadata.is_dir() || (mode & 0o022 != 0 && mode & 0o1000 == 0) {
            return Err(FileHygieneError::UnsafeAncestor {
                path: ancestor.to_path_buf(),
                mode,
                observed: describe(&metadata),
            });
        }
    }
    Ok(())
}

/// This blocks and must be wrapped in spawn_blocking from async code; the length is checked twice
/// because the file can grow between the metadata check and the read.
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
        AncestorPolicy, FileHygieneError, FileTier, check_trusted_ancestors, read_trusted_file,
    };

    const AS_WRITTEN_ABOVE: AncestorPolicy = AncestorPolicy {
        canonicalize: false,
        include_self: false,
    };
    const AS_WRITTEN_SELF: AncestorPolicy = AncestorPolicy {
        canonicalize: false,
        include_self: true,
    };
    const CANONICAL_SELF: AncestorPolicy = AncestorPolicy {
        canonicalize: true,
        include_self: true,
    };

    struct Fixture {
        root: TempDir,
        path: PathBuf,
        uid: u32,
    }

    fn fixture(mode: u32, contents: &[u8]) -> Fixture {
        let root = tempfile::tempdir().expect("temporary directory");
        let path = root.path().join("trusted");
        let mut file = fs::File::create(&path).expect("fixture file");
        file.write_all(contents).expect("fixture contents");
        fs::set_permissions(&path, Permissions::from_mode(mode)).expect("fixture mode");
        let uid = fs::metadata(&path).expect("fixture metadata").uid();
        Fixture { root, path, uid }
    }

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
        // Opening a directory read-only succeeds on Linux but fails with EISDIR elsewhere, so
        // either way the caller must not be told it read a trusted file.
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

    /// TMPDIR resolves through /var to /private/var on macOS, so fixtures must start from the
    /// already-resolved path since a symlinked ancestor is refused by design.
    fn resolved_root() -> (TempDir, PathBuf) {
        let root = tempfile::tempdir().expect("temporary directory");
        let resolved = root
            .path()
            .canonicalize()
            .expect("canonical temporary root");
        (root, resolved)
    }

    #[test]
    fn an_above_walk_refuses_the_parent_and_ignores_the_path_itself() {
        let (_root, resolved) = resolved_root();
        let parent = resolved.join("parent");
        fs::create_dir(&parent).expect("parent");
        let target = parent.join("root");

        check_trusted_ancestors(&target, AS_WRITTEN_ABOVE)
            .expect("a clean ancestry with no target yet");

        fs::create_dir(&target).expect("target");
        fs::set_permissions(&target, Permissions::from_mode(0o777)).expect("target mode");
        check_trusted_ancestors(&target, AS_WRITTEN_ABOVE)
            .expect("the path itself is the caller's business");

        fs::set_permissions(&parent, Permissions::from_mode(0o777)).expect("parent mode");
        let error = check_trusted_ancestors(&target, AS_WRITTEN_ABOVE)
            .expect_err("a world-writable parent may be replaced under the root");
        assert_eq!(error.category(), "unsafe-ancestor");
        assert_eq!(error.path(), parent);
    }

    #[test]
    fn a_path_and_above_walk_refuses_the_path_itself_and_any_symlinked_ancestor() {
        let (_root, resolved) = resolved_root();
        let directory = resolved.join("holder");
        fs::create_dir(&directory).expect("holder");
        fs::set_permissions(&directory, Permissions::from_mode(0o700)).expect("holder mode");
        check_trusted_ancestors(&directory, AS_WRITTEN_SELF).expect("a clean ancestry");

        fs::set_permissions(&directory, Permissions::from_mode(0o777)).expect("holder mode");
        let error = check_trusted_ancestors(&directory, AS_WRITTEN_SELF)
            .expect_err("the path itself is inspected here");
        assert_eq!(error.path(), directory);

        fs::set_permissions(&directory, Permissions::from_mode(0o700)).expect("holder mode");
        let alias = resolved.join("alias");
        std::os::unix::fs::symlink(&directory, &alias).expect("ancestor symlink");
        let error = check_trusted_ancestors(&alias, AS_WRITTEN_SELF)
            .expect_err("an alias is not the directory the operator named");
        assert_eq!(error.category(), "unsafe-ancestor");
        assert_eq!(error.path(), alias);
        assert!(error.to_string().contains("symbolic link"), "{error}");
    }

    #[test]
    fn a_canonical_walk_resolves_an_alias_and_requires_the_path_to_exist() {
        let (_root, resolved) = resolved_root();
        let directory = resolved.join("store");
        fs::create_dir(&directory).expect("store");
        fs::set_permissions(&directory, Permissions::from_mode(0o700)).expect("store mode");
        let alias = resolved.join("alias");
        std::os::unix::fs::symlink(&directory, &alias).expect("store symlink");

        check_trusted_ancestors(&alias, CANONICAL_SELF)
            .expect("the alias resolves onto a clean ancestry");

        fs::set_permissions(&directory, Permissions::from_mode(0o777)).expect("store mode");
        let error = check_trusted_ancestors(&alias, CANONICAL_SELF)
            .expect_err("the resolved directory is world-writable");
        assert_eq!(error.category(), "unsafe-ancestor");
        assert_eq!(error.path(), directory);

        let error = check_trusted_ancestors(&resolved.join("absent"), CANONICAL_SELF)
            .expect_err("nothing to resolve");
        assert_eq!(error.category(), "io");
    }

    #[test]
    fn a_sticky_world_writable_ancestor_is_tolerated() {
        let (_root, resolved) = resolved_root();
        let shared = resolved.join("shared");
        fs::create_dir(&shared).expect("shared");
        fs::set_permissions(&shared, Permissions::from_mode(0o1777)).expect("sticky mode");
        let owned = shared.join("owned");
        fs::create_dir(&owned).expect("owned");
        fs::set_permissions(&owned, Permissions::from_mode(0o700)).expect("owned mode");

        check_trusted_ancestors(&owned, AS_WRITTEN_SELF)
            .expect("sticky is what makes a shared directory safe to sit beneath");
        check_trusted_ancestors(&owned, AS_WRITTEN_ABOVE).expect("and the same walk from above");
        check_trusted_ancestors(&owned, CANONICAL_SELF).expect("and after resolution");
    }

    #[test]
    fn a_non_directory_ancestor_is_refused_and_described() {
        let (_root, resolved) = resolved_root();
        let file = resolved.join("not-a-directory");
        fs::write(&file, b"contents").expect("file");

        let error = check_trusted_ancestors(&file, AS_WRITTEN_SELF)
            .expect_err("a regular file is not a directory");
        assert_eq!(error.category(), "unsafe-ancestor");
        assert_eq!(error.path(), file);
        assert!(error.to_string().contains("regular file"), "{error}");
    }
}
