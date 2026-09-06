use std::{
    fs, io,
    os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    time::Duration,
};

use thiserror::Error;
use tokio::{
    net::{UnixListener, UnixStream},
    time::timeout,
};

const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

#[must_use]
pub fn current_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

pub fn validate_private_parent(path: &Path, expected_uid: u32) -> Result<(), SocketError> {
    let parent = path.parent().ok_or_else(|| SocketError::MissingParent {
        path: path.to_path_buf(),
    })?;
    let parent = fs::canonicalize(parent).map_err(|source| SocketError::Metadata {
        path: parent.to_path_buf(),
        source,
    })?;
    validate_ancestors(&parent)?;
    let metadata = fs::symlink_metadata(&parent).map_err(|source| SocketError::Metadata {
        path: parent.clone(),
        source,
    })?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != expected_uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(SocketError::InsecureParent { path: parent });
    }
    Ok(())
}

// The directory is the operator-owned IPC group setting. No credential path uses this rule.
pub fn validate_socket_parent(path: &Path, expected_uid: u32) -> Result<fs::Metadata, SocketError> {
    let parent = path.parent().ok_or_else(|| SocketError::MissingParent {
        path: path.to_path_buf(),
    })?;
    let metadata = fs::symlink_metadata(parent).map_err(|source| SocketError::Metadata {
        path: parent.to_path_buf(),
        source,
    })?;
    let mode = metadata.permissions().mode();
    if !metadata.file_type().is_dir()
        || metadata.uid() != expected_uid
        || mode & 0o027 != 0
        || !matches!(mode & 0o070, 0 | 0o010 | 0o050)
    {
        return Err(SocketError::InsecureParent {
            path: parent.to_path_buf(),
        });
    }
    let canonical = fs::canonicalize(parent).map_err(|source| SocketError::Metadata {
        path: parent.to_path_buf(),
        source,
    })?;
    validate_ancestors(&canonical)?;
    Ok(metadata)
}

fn socket_is_secure(metadata: &fs::Metadata, expected_uid: u32, parent: &fs::Metadata) -> bool {
    let mode = metadata.permissions().mode() & 0o7777;
    metadata.file_type().is_socket()
        && metadata.uid() == expected_uid
        && metadata.nlink() == 1
        && (mode == 0o600
            || (mode == 0o660
                && parent.permissions().mode() & 0o010 != 0
                && metadata.gid() == parent.gid()))
}

pub fn validate_owned_file(path: &Path, expected_uid: u32) -> Result<(), SocketError> {
    let parent = path.parent().ok_or_else(|| SocketError::MissingParent {
        path: path.to_path_buf(),
    })?;
    let parent = fs::canonicalize(parent).map_err(|source| SocketError::Metadata {
        path: parent.to_path_buf(),
        source,
    })?;
    validate_ancestors(&parent)?;
    let parent_metadata =
        fs::symlink_metadata(&parent).map_err(|source| SocketError::Metadata {
            path: parent.clone(),
            source,
        })?;
    if !parent_metadata.file_type().is_dir()
        || parent_metadata.uid() != expected_uid
        || parent_metadata.permissions().mode() & 0o022 != 0
    {
        return Err(SocketError::InsecureFileParent { path: parent });
    }
    let metadata = fs::symlink_metadata(path).map_err(|source| SocketError::Metadata {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file()
        || metadata.uid() != expected_uid
        || metadata.permissions().mode() & 0o022 != 0
        || metadata.nlink() != 1
    {
        return Err(SocketError::InsecureFile {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn validate_ancestors(path: &Path) -> Result<(), SocketError> {
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(|source| SocketError::Metadata {
            path: ancestor.to_path_buf(),
            source,
        })?;
        let mode = metadata.permissions().mode();
        if !metadata.file_type().is_dir() || (mode & 0o022 != 0 && mode & 0o1000 == 0) {
            return Err(SocketError::InsecureAncestor {
                path: ancestor.to_path_buf(),
            });
        }
    }
    Ok(())
}

#[allow(
    clippy::let_underscore_must_use,
    reason = "each `remove_file` here only rolls back a socket this call just created, on a path already returning the SocketError that explains the failure; a leftover socket is the lesser problem and must not replace that cause"
)]
pub async fn bind(
    path: &Path,
    expected_uid: u32,
) -> Result<(UnixListener, SocketGuard), SocketError> {
    let parent = validate_socket_parent(path, expected_uid)?;
    remove_stale(path, expected_uid, &parent).await?;
    let listener = UnixListener::bind(path).map_err(|source| SocketError::Bind {
        path: path.to_path_buf(),
        source,
    })?;
    // Group traversal opts into shared IPC. Set its exact group before granting access;
    // an unprivileged broker must itself belong to this group. Private parents stay 0600.
    let mode = if parent.permissions().mode() & 0o010 != 0 {
        0o660
    } else {
        0o600
    };
    if mode == 0o660
        && let Err(source) = std::os::unix::fs::chown(path, None, Some(parent.gid()))
    {
        let _ = fs::remove_file(path);
        return Err(SocketError::Permissions {
            path: path.to_path_buf(),
            source,
        });
    }
    if let Err(source) = fs::set_permissions(path, fs::Permissions::from_mode(mode)) {
        let _ = fs::remove_file(path);
        return Err(SocketError::Permissions {
            path: path.to_path_buf(),
            source,
        });
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) => {
            drop(listener);
            let _ = fs::remove_file(path);
            return Err(SocketError::Metadata {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if !socket_is_secure(&metadata, expected_uid, &parent) {
        let _ = fs::remove_file(path);
        return Err(SocketError::InsecureSocket {
            path: path.to_path_buf(),
        });
    }
    Ok((
        listener,
        SocketGuard {
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
        },
    ))
}

async fn remove_stale(
    path: &Path,
    expected_uid: u32,
    parent: &fs::Metadata,
) -> Result<(), SocketError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(SocketError::Metadata {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if !socket_is_secure(&metadata, expected_uid, parent) {
        return Err(SocketError::InsecureSocket {
            path: path.to_path_buf(),
        });
    }
    match timeout(SOCKET_PROBE_TIMEOUT, UnixStream::connect(path)).await {
        Ok(Ok(_)) => Err(SocketError::AlreadyRunning {
            path: path.to_path_buf(),
        }),
        Ok(Err(error))
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            fs::remove_file(path).map_err(|source| SocketError::RemoveStale {
                path: path.to_path_buf(),
                source,
            })
        }
        Ok(Err(source)) => Err(SocketError::Probe {
            path: path.to_path_buf(),
            source,
        }),
        Err(_) => Err(SocketError::ProbeTimeout {
            path: path.to_path_buf(),
        }),
    }
}

#[derive(Debug)]
pub struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl SocketGuard {
    pub fn cleanup(&mut self) -> Result<(), SocketError> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(SocketError::Metadata {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        if !metadata.file_type().is_socket()
            || metadata.dev() != self.device
            || metadata.ino() != self.inode
        {
            return Err(SocketError::SocketReplaced {
                path: self.path.clone(),
            });
        }
        fs::remove_file(&self.path).map_err(|source| SocketError::Cleanup {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "the last-resort teardown for a guard nobody called `cleanup` on; a Drop cannot report a SocketError and the explicit path already does"
        )]
        let _ = self.cleanup();
    }
}

#[derive(Debug, Error)]
pub enum SocketError {
    #[error("path has no parent: {path}")]
    MissingParent { path: PathBuf },
    #[error("could not inspect path: {path}")]
    Metadata {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "parent directory must be server-owned, non-symlink, private or IPC-group traversable, without group writes or others access: {path}"
    )]
    InsecureParent { path: PathBuf },
    #[error("path ancestor must be a directory without unprotected group/world writes: {path}")]
    InsecureAncestor { path: PathBuf },
    #[error("file parent must be owned by the server UID and not group/world writable: {path}")]
    InsecureFileParent { path: PathBuf },
    #[error(
        "file must be regular, single-link, owned by the server UID, and not group/world writable: {path}"
    )]
    InsecureFile { path: PathBuf },
    #[error(
        "socket must be server-owned and single-link, mode 0600 or 0660 in its parent IPC group: {path}"
    )]
    InsecureSocket { path: PathBuf },
    #[error("a broker is already listening at {path}")]
    AlreadyRunning { path: PathBuf },
    #[error("could not safely probe existing broker socket at {path}")]
    Probe {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("timed out while probing existing broker socket at {path}")]
    ProbeTimeout { path: PathBuf },
    #[error("could not remove stale broker socket at {path}")]
    RemoveStale {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not bind broker socket at {path}")]
    Bind {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not make broker socket private at {path}")]
    Permissions {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("broker socket path was replaced; refusing to remove it: {path}")]
    SocketReplaced { path: PathBuf },
    #[error("could not remove broker socket at {path}")]
    Cleanup {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}
