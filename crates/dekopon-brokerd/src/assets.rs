use crate::config::AssetsConfig;
use dekopon_http_host::asset::AssetDirectory;
use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt as _, MetadataExt as _},
    path::PathBuf,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AssetsStartupError {
    #[error("assets.rootPath is not a broker-owned 0700 directory")]
    UnsafeRoot,
    #[error("assets.rootPath traversal was refused")]
    Path {
        #[source]
        source: dekopon_storage_host::StorageHostError,
    },
    #[error("assets.rootPath initialization failed at {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("asset initialization worker failed")]
    Worker {
        #[source]
        source: tokio::task::JoinError,
    },
}

pub async fn initialize(config: &AssetsConfig) -> Result<AssetDirectory, AssetsStartupError> {
    let config = config.clone();
    tokio::task::spawn_blocking(move || {
        let root = dekopon_storage_host::resolve_storage_root_path(&config.root_path)
            .map_err(|source| AssetsStartupError::Path { source })?;
        let io_error = |source| AssetsStartupError::Io {
            path: root.clone(),
            source,
        };
        match fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error(error)),
        }
        let metadata = fs::symlink_metadata(&root).map_err(io_error)?;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o7777 != 0o700
        {
            return Err(AssetsStartupError::UnsafeRoot);
        }
        for entry in fs::read_dir(&root).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let kind = entry.file_type().map_err(io_error)?;
            // Uses file_type, not metadata, so a symlink is unlinked rather than followed into
            // remove_dir_all and deleted recursively.
            if kind.is_dir() {
                fs::remove_dir_all(entry.path()).map_err(io_error)?;
            } else {
                fs::remove_file(entry.path()).map_err(io_error)?;
            }
        }
        Ok(AssetDirectory::new(root, config.max_in_flight_bytes))
    })
    .await
    .map_err(|source| AssetsStartupError::Worker { source })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    #[tokio::test]
    async fn startup_empties_the_root_without_following_entries() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().canonicalize().unwrap().join("assets");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let outside = parent.path().join("keep");
        fs::write(&outside, b"keep").unwrap();
        fs::write(root.join("old"), b"old").unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("nested/file"), b"old").unwrap();
        symlink(&outside, root.join("link")).unwrap();
        initialize(&AssetsConfig {
            root_path: root.clone(),
            max_in_flight_bytes: 1,
        })
        .await
        .unwrap();
        assert_eq!(fs::read_dir(root).unwrap().count(), 0);
        assert_eq!(fs::read(outside).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn startup_refuses_a_public_root_before_removing_entries() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().canonicalize().unwrap().join("assets");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("keep"), b"keep").unwrap();
        assert!(matches!(
            initialize(&AssetsConfig {
                root_path: root.clone(),
                max_in_flight_bytes: 1
            })
            .await,
            Err(AssetsStartupError::UnsafeRoot)
        ));
        assert!(root.join("keep").exists());
    }
}
