use std::{
    fs::{self, Metadata},
    io,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

const DATA_LINK: &str = "..data";

#[derive(Debug)]
pub struct Projection {
    entry: PathBuf,
    root: PathBuf,
    generation: PathBuf,
    target: PathBuf,
    owner: u32,
    expected_uid: u32,
}

#[derive(Debug, Error)]
pub enum ProjectionError {
    #[error("{} is not a trusted read-only projection", path.display())]
    Untrusted { path: PathBuf },
    #[error("{} moved to another projection generation while it was read", path.display())]
    Switched { path: PathBuf },
    #[error("could not read projection {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[must_use]
pub fn effective_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

impl Projection {
    /// Only a claim: [`Projection::resolve`] decides whether it holds.
    #[must_use]
    pub fn is_claimed(entry: &Path) -> bool {
        entry.parent().is_some_and(|root| {
            fs::symlink_metadata(root.join(DATA_LINK))
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
        })
    }

    pub fn resolve(entry: &Path, expected_uid: u32) -> Result<Self, ProjectionError> {
        let root = entry.parent().ok_or_else(|| ProjectionError::Untrusted {
            path: entry.to_path_buf(),
        })?;
        let read_only = rustix::fs::statvfs(root)
            .map_err(|source| ProjectionError::Io {
                path: root.to_path_buf(),
                source: source.into(),
            })?
            .f_flag
            .contains(rustix::fs::StatVfsMountFlags::RDONLY);
        Self::verify(entry, expected_uid, read_only)
    }

    pub fn verify(
        entry: &Path,
        expected_uid: u32,
        read_only: bool,
    ) -> Result<Self, ProjectionError> {
        let untrusted = || ProjectionError::Untrusted {
            path: entry.to_path_buf(),
        };
        let inspect = |path: &Path| {
            fs::symlink_metadata(path).map_err(|source| ProjectionError::Io {
                path: path.to_path_buf(),
                source,
            })
        };
        let link = |path: &Path| {
            fs::read_link(path).map_err(|source| ProjectionError::Io {
                path: path.to_path_buf(),
                source,
            })
        };
        let owned = |metadata: &Metadata| metadata.uid() == 0 || metadata.uid() == expected_uid;
        let root = entry.parent().ok_or_else(untrusted)?;
        let name = entry.file_name().ok_or_else(untrusted)?;
        let data = root.join(DATA_LINK);
        let root_metadata = inspect(root)?;
        if !read_only || !root_metadata.is_dir() || !owned(&root_metadata) {
            return Err(untrusted());
        }
        let entry_link = inspect(entry)?;
        let data_link = inspect(&data)?;
        if !entry_link.file_type().is_symlink()
            || !data_link.file_type().is_symlink()
            || !owned(&entry_link)
            || !owned(&data_link)
            || link(entry)? != Path::new(DATA_LINK).join(name)
        {
            return Err(untrusted());
        }
        let generation = link(&data)?;
        if !matches!(
            generation.components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_)]
        ) {
            return Err(untrusted());
        }
        let directory = root.join(&generation);
        let target = directory.join(name);
        let projection = Self {
            entry: entry.to_path_buf(),
            root: root.to_path_buf(),
            generation,
            target,
            owner: 0,
            expected_uid,
        };
        let target = inspect(&projection.target)?;
        if !projection.admits(&inspect(&directory)?) || !projection.admits(&target) {
            return Err(untrusted());
        }
        Ok(Self {
            owner: target.uid(),
            ..projection
        })
    }

    #[must_use]
    pub fn admits(&self, metadata: &Metadata) -> bool {
        !metadata.file_type().is_symlink()
            && (metadata.uid() == 0 || metadata.uid() == self.expected_uid)
            && metadata.permissions().mode() & 0o022 == 0
    }

    #[must_use]
    pub fn target(&self) -> &Path {
        &self.target
    }

    #[must_use]
    pub fn owner(&self) -> u32 {
        self.owner
    }

    /// Call after the last read from [`Projection::target`]: an unchanged `..data` link means
    /// every read came from one generation.
    pub fn confirm(&self) -> Result<(), ProjectionError> {
        let data = self.root.join(DATA_LINK);
        let current = fs::read_link(&data).map_err(|source| ProjectionError::Io {
            path: data.clone(),
            source,
        })?;
        if current == self.generation {
            Ok(())
        } else {
            Err(ProjectionError::Switched {
                path: self.entry.clone(),
            })
        }
    }
}
