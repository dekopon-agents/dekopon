use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
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
        let lock = lock_path(path).ok_or_else(|| PrivateFileError {
            path: path.to_path_buf(),
            source: io::Error::from(io::ErrorKind::InvalidInput),
        })?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_private_file_mode(&mut options);
        let file = options
            .open(&lock)
            .and_then(|file| file.lock().map(|()| file))
            .map_err(|source| PrivateFileError { path: lock, source })?;
        Ok(Self { file })
    }
}

impl Drop for PrivateFileLock {
    fn drop(&mut self) {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "a destructor has no caller to report to, and closing the file releases the \
                      lock regardless of what an explicit unlock answers"
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
    let parent = path.parent().ok_or_else(|| PrivateFileError {
        path: path.to_path_buf(),
        source: io::Error::from(io::ErrorKind::InvalidInput),
    })?;
    let temporary = temporary_path(path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_private_file_mode(&mut options);
    let result = (|| {
        let mut file = options
            .open(&temporary)
            .map_err(|source| PrivateFileError {
                path: temporary.clone(),
                source,
            })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| PrivateFileError {
                path: temporary.clone(),
                source,
            })?;
        replace_file(&temporary, path).map_err(|source| PrivateFileError {
            path: path.to_path_buf(),
            source,
        })?;
        // Without the directory sync, a power failure can lose the rename despite the file sync.
        sync_directory(parent).map_err(|source| PrivateFileError {
            path: parent.to_path_buf(),
            source,
        })
    })();
    if result.is_err() {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "rollback of a temporary the write already failed on; the caller is being \
                      given that write error, and a leftover 0600 temporary is not worth \
                      replacing it with a cleanup error"
        )]
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn sweep_stale_temporaries(path: &Path, keep: Option<&Path>) -> Vec<TemporarySweep> {
    let (Some(parent), Some(stem)) = (path.parent(), path.file_stem()) else {
        return Vec::new();
    };
    let mut prefix = OsString::from(stem);
    prefix.push(".tmp-");
    let Some(prefix) = prefix.to_str() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut results = Vec::new();
    for entry in entries.flatten() {
        let stale = entry.path();
        if Some(stale.as_path()) == keep {
            continue;
        }
        let Some(name) = stale.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(prefix) {
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

fn sync_directory(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

fn set_private_file_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;
    options.mode(0o600);
}

fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, sync::mpsc, thread, time::Duration};

    use tempfile::TempDir;

    use super::{PrivateFileLock, TemporarySweep, replace_private_file, sweep_stale_temporaries};

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
    fn a_sweep_reports_a_removed_temporary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("record.json");
        let stale = dir.path().join("record.tmp-123");
        fs::write(&stale, b"old").unwrap();
        let events = sweep_stale_temporaries(&path, None);
        assert!(matches!(events.as_slice(), [TemporarySweep::Removed { path }] if path == &stale));
        assert!(!stale.exists());
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
