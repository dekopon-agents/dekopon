use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
};

#[derive(Debug)]
#[must_use]
pub struct PrivateFileLock {
    file: File,
}

#[derive(Debug, thiserror::Error)]
#[error("private file operation failed at {}", path.display())]
pub struct PrivateFileError {
    pub path: PathBuf,
    #[source]
    pub source: io::Error,
}

#[derive(Debug)]
pub enum TemporarySweep {
    Removed { path: PathBuf },
    Failed { path: PathBuf, source: io::Error },
}

impl PrivateFileLock {
    // The lock lives on a sibling because replacing the record by rename changes its inode.
    pub fn acquire(path: &Path) -> Result<Self, PrivateFileError> {
        let (file, path) = Self::open(path)?;
        file.lock()
            .map_err(|source| PrivateFileError { path, source })?;
        Ok(Self { file })
    }

    pub fn try_acquire(path: &Path) -> Result<Option<Self>, PrivateFileError> {
        let (file, path) = Self::open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(source)) => Err(PrivateFileError { path, source }),
        }
    }

    fn open(path: &Path) -> Result<(File, PathBuf), PrivateFileError> {
        let lock = lock_path(path).ok_or_else(|| PrivateFileError {
            path: path.to_path_buf(),
            source: io::Error::from(io::ErrorKind::InvalidInput),
        })?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_private_file_mode(&mut options);
        let file = options.open(&lock).map_err(|source| PrivateFileError {
            path: lock.clone(),
            source,
        })?;
        Ok((file, lock))
    }
}

impl Drop for PrivateFileLock {
    fn drop(&mut self) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "discard unlock's result because Drop cannot report it and closing the file releases the lock regardless"
        )]
        let _ = self.file.unlock();
    }
}

pub fn lock_path(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    let mut lock_name = OsString::from(name);
    lock_name.push(".lock");
    Some(path.with_file_name(lock_name))
}

#[derive(Clone, Copy, Debug)]
pub enum PrivateFilePublication {
    Replace,
    NoClobber,
}

#[derive(Debug)]
pub enum PrivateFileContents<'a> {
    Bytes(&'a [u8]),
    Prepared {
        file: &'a File,
        temporary: &'a Path,
        publication: PrivateFilePublication,
    },
}

pub fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".tmp-{}", std::process::id()));
    PathBuf::from(name)
}

pub fn replace_private_file(
    path: &Path,
    contents: PrivateFileContents<'_>,
) -> Result<(), PrivateFileError> {
    match contents {
        PrivateFileContents::Bytes(bytes) => {
            let temporary = temporary_path(path);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            set_private_file_mode(&mut options);
            let mut file = options
                .open(&temporary)
                .map_err(|source| PrivateFileError {
                    path: temporary.clone(),
                    source,
                })?;
            let result = file
                .write_all(bytes)
                .map_err(|source| PrivateFileError {
                    path: temporary.clone(),
                    source,
                })
                .and_then(|()| {
                    publish_private_file(&file, &temporary, path, PrivateFilePublication::Replace)
                });
            if result.is_err() {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "discard cleanup's result to preserve the original error; this call owns the successfully created temporary"
                )]
                let _ = fs::remove_file(&temporary);
            }
            result
        }
        PrivateFileContents::Prepared {
            file,
            temporary,
            publication,
        } => publish_private_file(file, temporary, path, publication),
    }
}

fn publish_private_file(
    file: &File,
    temporary: &Path,
    destination: &Path,
    publication: PrivateFilePublication,
) -> Result<(), PrivateFileError> {
    let parent = destination.parent().ok_or_else(|| PrivateFileError {
        path: destination.to_path_buf(),
        source: io::Error::from(io::ErrorKind::InvalidInput),
    })?;
    if temporary == destination || temporary.parent() != Some(parent) {
        return Err(PrivateFileError {
            path: temporary.to_path_buf(),
            source: io::Error::from(io::ErrorKind::InvalidInput),
        });
    }
    file.sync_all().map_err(|source| PrivateFileError {
        path: temporary.to_path_buf(),
        source,
    })?;
    match publication {
        PrivateFilePublication::Replace => fs::rename(temporary, destination),
        PrivateFilePublication::NoClobber => {
            fs::hard_link(temporary, destination).and_then(|()| fs::remove_file(temporary))
        }
    }
    .map_err(|source| PrivateFileError {
        path: destination.to_path_buf(),
        source,
    })?;
    // Without the directory sync, a power failure can lose publication despite the file sync.
    sync_directory(parent).map_err(|source| PrivateFileError {
        path: parent.to_path_buf(),
        source,
    })
}

pub fn sweep_stale_temporaries(path: &Path, keep: Option<&Path>) -> Vec<TemporarySweep> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Vec::new();
    };
    let mut prefix = OsString::from(name);
    prefix.push(".tmp-");
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut results = Vec::new();
    for entry in entries.flatten() {
        let stale = entry.path();
        if stale == path || Some(stale.as_path()) == keep {
            continue;
        }
        let Some(name) = stale.file_name() else {
            continue;
        };
        if !name.as_bytes().starts_with(prefix.as_bytes()) {
            continue;
        }
        match fs::remove_file(&stale) {
            Ok(()) => results.push(TemporarySweep::Removed { path: stale }),
            Err(source) => results.push(TemporarySweep::Failed {
                path: stale,
                source,
            }),
        }
    }
    results
}

pub fn sync_directory(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

fn set_private_file_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;
    options.mode(0o600);
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, sync::mpsc, thread, time::Duration};

    use tempfile::TempDir;

    use super::{
        PrivateFileContents, PrivateFileLock, TemporarySweep, replace_private_file,
        sweep_stale_temporaries, temporary_path,
    };

    #[test]
    fn two_handles_to_one_lock_file_exclude_each_other() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("record");
        let first = PrivateFileLock::acquire(&path).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let second = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _guard = PrivateFileLock::acquire(&path).unwrap();
            acquired_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            acquired_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(first);
        acquired_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        second.join().unwrap();
    }

    #[test]
    fn a_rename_replaces_the_file_and_leaves_no_temporary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("record.json");
        fs::write(&path, b"old").unwrap();
        replace_private_file(&path, PrivateFileContents::Bytes(b"new")).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_failed_create_does_not_unlink_another_writers_temporary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("record.json");
        let temporary = temporary_path(&path);
        fs::write(&path, b"old").unwrap();
        fs::write(&temporary, b"another writer").unwrap();
        let error = replace_private_file(&path, PrivateFileContents::Bytes(b"new")).unwrap_err();
        assert_eq!(error.source.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(fs::read(&temporary).unwrap(), b"another writer");
    }

    #[test]
    fn a_sweep_matches_the_complete_filename_and_preserves_the_destination() {
        let dir = TempDir::new().unwrap();
        let path = dir
            .path()
            .join(format!("providers.tmp-{}", std::process::id()));
        let unrelated = dir.path().join("providers.other.tmp-123");
        let temporary = temporary_path(&path);
        fs::write(&path, b"old").unwrap();
        fs::write(&unrelated, b"unrelated").unwrap();
        fs::write(&temporary, b"partial").unwrap();
        let events = sweep_stale_temporaries(&path, None);
        assert!(
            matches!(events.as_slice(), [TemporarySweep::Removed { path }] if path == &temporary)
        );
        assert_eq!(fs::read(path).unwrap(), b"old");
        assert_eq!(fs::read(unrelated).unwrap(), b"unrelated");
    }

    #[test]
    fn a_non_utf8_filename_can_sweep_stale_staging_and_retry() {
        use std::os::unix::ffi::OsStringExt as _;

        let dir = TempDir::new().unwrap();
        let path = dir
            .path()
            .join(std::ffi::OsString::from_vec(b"record.\xff".to_vec()));
        let temporary = temporary_path(&path);
        fs::write(&path, b"old").unwrap();
        fs::write(&temporary, b"partial").unwrap();
        assert_eq!(sweep_stale_temporaries(&path, None).len(), 1);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        replace_private_file(&path, PrivateFileContents::Bytes(b"new")).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"new");
    }

    #[test]
    fn a_prepared_destination_cannot_be_its_own_temporary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("record");
        fs::write(&path, b"old").unwrap();
        let file = fs::File::open(&path).unwrap();
        let error = replace_private_file(
            &path,
            PrivateFileContents::Prepared {
                file: &file,
                temporary: &path,
                publication: super::PrivateFilePublication::Replace,
            },
        )
        .unwrap_err();
        assert_eq!(error.source.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(path).unwrap(), b"old");
    }

    #[test]
    fn a_sweep_reports_a_removed_temporary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("record.json");
        let stale = dir.path().join("record.json.tmp-123");
        fs::write(&stale, b"old").unwrap();
        let events = sweep_stale_temporaries(&path, None);
        assert!(matches!(events.as_slice(), [TemporarySweep::Removed { path }] if path == &stale));
        assert!(!stale.exists());
    }

    #[test]
    fn a_sweep_preserves_the_keep_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("record.json");
        let keep = dir.path().join("record.json.tmp-123");
        fs::write(&keep, b"still here").unwrap();
        assert!(sweep_stale_temporaries(&path, Some(&keep)).is_empty());
        assert_eq!(fs::read(keep).unwrap(), b"still here");
    }
}
