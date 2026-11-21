use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
};

use tempfile::TempPath;

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

pub fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension(format!("tmp-{}", std::process::id()))
}

pub fn replace_private_file(path: &Path, bytes: &[u8]) -> Result<(), PrivateFileError> {
    replace_private_file_with(path, |file| file.write_all(bytes))
}

pub fn replace_private_file_with(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<(), PrivateFileError> {
    let parent = parent_directory(path)?;
    let prefix = temporary_prefix(path).ok_or_else(|| PrivateFileError {
        path: path.to_path_buf(),
        source: io::Error::from(io::ErrorKind::InvalidInput),
    })?;
    let mut temporary = tempfile::Builder::new()
        .prefix(&prefix)
        .tempfile_in(parent)
        .map_err(|source| PrivateFileError {
            path: parent.to_path_buf(),
            source,
        })?;
    let file = temporary.as_file_mut();
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .and_then(|()| write(file))
        .and_then(|()| file.sync_all())
        .map_err(|source| PrivateFileError {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    publish_private_file(path, temporary.into_temp_path(), |staged, path| {
        staged.persist(path)
    })
}

pub fn replace_private_file_noclobber(
    path: &Path,
    staged: TempPath,
) -> Result<(), PrivateFileError> {
    publish_private_file(path, staged, |staged, path| staged.persist_noclobber(path))
}

fn publish_private_file(
    path: &Path,
    staged: TempPath,
    persist: impl FnOnce(TempPath, &Path) -> Result<(), tempfile::PathPersistError>,
) -> Result<(), PrivateFileError> {
    let parent = parent_directory(path)?;
    persist(staged, path).map_err(|error| PrivateFileError {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    // Without the directory sync, a power failure can lose the rename despite the file sync.
    sync_directory(parent).map_err(|source| PrivateFileError {
        path: parent.to_path_buf(),
        source,
    })
}

fn parent_directory(path: &Path) -> Result<&Path, PrivateFileError> {
    path.parent()
        .map(|parent| {
            if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            }
        })
        .ok_or_else(|| PrivateFileError {
            path: path.to_path_buf(),
            source: io::Error::from(io::ErrorKind::InvalidInput),
        })
}

fn temporary_prefix(path: &Path) -> Option<OsString> {
    let mut prefix = path.file_name()?.to_os_string();
    prefix.push(".tmp-private-");
    Some(prefix)
}

pub fn sweep_stale_temporaries(path: &Path, keep: Option<&Path>) -> Vec<TemporarySweep> {
    let (Ok(parent), Some(prefix), Some(stem)) = (
        parent_directory(path),
        temporary_prefix(path),
        path.file_stem(),
    ) else {
        return Vec::new();
    };
    let mut legacy_prefix = stem.to_os_string();
    legacy_prefix.push(".tmp-");
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut results = Vec::new();
    let destination = path.file_name();
    for entry in entries.flatten() {
        let stale = entry.path();
        let name = entry.file_name();
        if Some(name.as_os_str()) == destination || Some(stale.as_path()) == keep {
            continue;
        }
        let name = name.as_encoded_bytes();
        let legacy = name
            .strip_prefix(legacy_prefix.as_encoded_bytes())
            .is_some_and(|pid| !pid.is_empty() && pid.iter().all(u8::is_ascii_digit));
        if !legacy && !name.starts_with(prefix.as_encoded_bytes()) {
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
    use std::{
        fs,
        io::{self, Write as _},
        os::unix::fs::PermissionsExt as _,
        sync::mpsc,
        thread,
        time::Duration,
    };

    use tempfile::TempDir;

    use super::{
        PrivateFileLock, TemporarySweep, replace_private_file, replace_private_file_with,
        sweep_stale_temporaries,
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
        replace_private_file(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_failed_writer_leaves_the_destination_whole_and_does_not_block_another_writer() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("record.json");
        fs::write(&path, b"previous record").unwrap();

        let error = replace_private_file_with(&path, |file| {
            file.write_all(b"unfinished")?;
            assert_eq!(fs::read(&path).unwrap(), b"previous record");
            replace_private_file(&path, b"complete replacement").unwrap();
            Err(io::Error::from(io::ErrorKind::Interrupted))
        })
        .unwrap_err();
        assert_eq!(error.source.kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read(&path).unwrap(), b"complete replacement");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_sweep_removes_current_and_legacy_temporaries_for_its_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("record.json");
        let legacy = dir.path().join("record.tmp-123");
        let stale = dir.path().join("record.json.tmp-private-abandoned");
        let other = dir.path().join("record.yaml.tmp-private-active");
        for file in [&legacy, &stale, &other] {
            fs::write(file, b"staged").unwrap();
        }
        let events = sweep_stale_temporaries(&path, None);
        assert_eq!(events.len(), 2);
        for file in [legacy, stale] {
            assert!(
                events.iter().any(
                    |event| matches!(event, TemporarySweep::Removed { path } if path == &file)
                )
            );
            assert!(!file.exists());
        }
        assert!(other.exists());
    }

    #[test]
    fn a_sweep_preserves_the_keep_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("record.json");
        let keep = dir.path().join("record.tmp-123");
        fs::write(&keep, b"still here").unwrap();
        assert!(sweep_stale_temporaries(&path, Some(&keep)).is_empty());
        assert_eq!(fs::read(keep).unwrap(), b"still here");
    }
}
