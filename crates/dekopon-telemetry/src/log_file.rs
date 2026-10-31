use std::{
    ffi::OsString,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write as _},
    num::{NonZeroU8, NonZeroU64},
    os::unix::{
        ffi::OsStrExt as _,
        fs::{MetadataExt as _, OpenOptionsExt as _},
    },
    path::{Path, PathBuf},
};

use dekopon_core::{
    AncestorOwners, AncestorPolicy, FileHygieneError, FileTier, check_trusted_ancestors,
    check_trusted_metadata,
};
use thiserror::Error;

use super::ConsoleSink;

const ROTATION_FAILED: &str = "telemetry.console.rotation.failed";

#[cfg(feature = "cli")]
#[derive(Debug, clap::Args)]
pub struct LogFileArgs {
    /// Write console JSON to this private file instead of stdout, rotated by size; serving only.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<PathBuf>,
    /// Rotate the log file before a record would take it past this many bytes.
    #[arg(
        long,
        value_name = "BYTES",
        default_value = "16777216",
        requires = "log_file"
    )]
    pub log_file_max_bytes: NonZeroU64,
    /// Rotated files kept beside the active one; the set stays under (COUNT + 1) × max(BYTES, 4 MiB).
    #[arg(long, value_name = "COUNT", default_value = "4", requires = "log_file")]
    pub log_file_keep: NonZeroU8,
}

#[cfg(feature = "cli")]
impl LogFileArgs {
    pub fn open(&self, euid: u32) -> Option<Result<LogFile, LogFileError>> {
        self.log_file
            .as_deref()
            .map(|path| LogFile::open(path, euid, self.log_file_max_bytes, self.log_file_keep))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogFileOperation {
    Open,
    Truncate,
    List,
    Remove,
}

impl fmt::Display for LogFileOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Open => "open",
            Self::Truncate => "truncate",
            Self::List => "list the directory of",
            Self::Remove => "remove",
        })
    }
}

#[derive(Debug, Error)]
pub enum LogFileError {
    #[error("{} does not name a file", path.display())]
    NoFileName { path: PathBuf },
    #[error("could not read the working directory ({kind:?})")]
    WorkingDirectory { kind: io::ErrorKind },
    #[error("could not use the log file")]
    File {
        #[source]
        source: FileHygieneError,
    },
    #[error("could not {operation} {} ({kind:?})", path.display())]
    Io {
        path: PathBuf,
        operation: LogFileOperation,
        kind: io::ErrorKind,
    },
}

impl From<FileHygieneError> for LogFileError {
    fn from(source: FileHygieneError) -> Self {
        Self::File { source }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Active,
    AwaitingReplacement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Streak {
    Quiet,
    Failing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RotationStep {
    Shift,
    Rename,
    Create,
    Reset,
}

impl RotationStep {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Shift => "shift",
            Self::Rename => "rename",
            Self::Create => "create",
            Self::Reset => "reset",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RotationNotice {
    step: RotationStep,
    kind: io::ErrorKind,
}

impl RotationNotice {
    pub(super) fn line(self) -> Vec<u8> {
        use tracing_subscriber::fmt::time::FormatTime as _;
        let mut timestamp = String::new();
        let formatted = tracing_subscriber::fmt::time::SystemTime.format_time(
            &mut tracing_subscriber::fmt::format::Writer::new(&mut timestamp),
        );
        let mut record = serde_json::json!({
            "level": "WARN",
            "target": module_path!(),
            "message": ROTATION_FAILED,
            "audit.event": ROTATION_FAILED,
            "console.rotation.step": self.step.as_str(),
            "error.kind": format!("{:?}", self.kind),
        });
        if formatted.is_ok() {
            record["timestamp"] = serde_json::Value::String(timestamp);
        }
        let mut line = record.to_string().into_bytes();
        line.push(b'\n');
        line
    }
}

pub struct LogFile {
    path: PathBuf,
    max_bytes: u64,
    keep: u8,
    file: File,
    len: u64,
    phase: Phase,
    streak: Streak,
    notice: Option<RotationNotice>,
}

impl fmt::Debug for LogFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LogFile")
            .field("path", &self.path)
            .field("max_bytes", &self.max_bytes)
            .field("keep", &self.keep)
            .finish_non_exhaustive()
    }
}

impl LogFile {
    pub fn open(
        path: &Path,
        euid: u32,
        max_bytes: NonZeroU64,
        keep: NonZeroU8,
    ) -> Result<Self, LogFileError> {
        let path = std::env::current_dir()
            .map_err(|error| LogFileError::WorkingDirectory { kind: error.kind() })?
            .join(path);
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(LogFileError::NoFileName { path });
        };
        let parent = fs::canonicalize(parent).map_err(|source| FileHygieneError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        check_trusted_ancestors(
            &parent,
            AncestorPolicy {
                canonicalize: false,
                include_self: true,
                owners: AncestorOwners::RootOr(euid),
            },
        )?;
        let path = parent.join(name);
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            check_trusted_metadata(&path, &metadata, euid, FileTier::Private)?;
        }
        let mut options = OpenOptions::new();
        // O_NONBLOCK keeps a FIFO swapped in after the check above from blocking startup.
        options
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options
            .open(&path)
            .map_err(io_error(&path, LogFileOperation::Open))?;
        let metadata = file
            .metadata()
            .map_err(io_error(&path, LogFileOperation::Open))?;
        check_trusted_metadata(&path, &metadata, euid, FileTier::Private)?;
        let mut log = Self {
            max_bytes: max_bytes.get(),
            keep: keep.get(),
            len: metadata.len(),
            file,
            path,
            phase: Phase::Active,
            streak: Streak::Quiet,
            notice: None,
        };
        if log.len > log.ceiling() {
            log.file
                .set_len(0)
                .map_err(io_error(&log.path, LogFileOperation::Truncate))?;
            log.len = 0;
        }
        log.prune(euid, &parent, name.as_bytes())?;
        Ok(log)
    }

    fn prune(&self, euid: u32, parent: &Path, name: &[u8]) -> Result<(), LogFileError> {
        let entries = fs::read_dir(parent).map_err(io_error(parent, LogFileOperation::List))?;
        for entry in entries {
            let entry = entry.map_err(io_error(parent, LogFileOperation::List))?;
            let Some(index) = backup_index(name, entry.file_name().as_bytes()) else {
                continue;
            };
            let backup = entry.path();
            let metadata =
                fs::symlink_metadata(&backup).map_err(|source| FileHygieneError::Io {
                    path: backup.clone(),
                    source,
                })?;
            check_trusted_metadata(&backup, &metadata, euid, FileTier::Private)?;
            if index > u64::from(self.keep) || metadata.size() > self.ceiling() {
                fs::remove_file(&backup).map_err(io_error(&backup, LogFileOperation::Remove))?;
            }
        }
        Ok(())
    }

    fn ceiling(&self) -> u64 {
        self.max_bytes.max(ConsoleSink::MAX_BYTES as u64)
    }

    fn backup(&self, index: u8) -> PathBuf {
        let mut name = OsString::from(self.path.as_os_str());
        name.push(format!(".{index}"));
        PathBuf::from(name)
    }

    pub(super) fn write_line(&mut self, record: &[u8]) -> io::Result<()> {
        if record.len() > ConsoleSink::MAX_BYTES {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let size = record.len() as u64;
        if self.len > 0 && self.len.saturating_add(size) > self.max_bytes {
            match self.phase {
                Phase::Active => self.rotate(),
                Phase::AwaitingReplacement => self.replace(),
            }
        }
        if self.len.saturating_add(size) > self.ceiling() {
            if let Err(error) = self.file.set_len(0) {
                self.fail(RotationStep::Reset, error.kind());
                return Err(error);
            }
            self.len = 0;
        }
        self.file.write_all(record)?;
        self.len += size;
        Ok(())
    }

    pub(super) const fn take_notice(&mut self) -> Option<RotationNotice> {
        self.notice.take()
    }

    pub(super) const fn keep_notice(&mut self, notice: RotationNotice) {
        self.notice = Some(notice);
    }

    fn rotate(&mut self) {
        for index in (1..self.keep).rev() {
            match fs::rename(self.backup(index), self.backup(index + 1)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return self.fail(RotationStep::Shift, error.kind()),
            }
        }
        if let Err(error) = fs::rename(&self.path, self.backup(1)) {
            return self.fail(RotationStep::Rename, error.kind());
        }
        self.phase = Phase::AwaitingReplacement;
        self.replace();
    }

    fn replace(&mut self) {
        let mut options = OpenOptions::new();
        options
            .append(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        match options.open(&self.path) {
            Ok(file) => {
                self.file = file;
                self.len = 0;
                self.phase = Phase::Active;
                self.streak = Streak::Quiet;
            }
            Err(error) => self.fail(RotationStep::Create, error.kind()),
        }
    }

    // The streak is marked before the notice exists, so writing the notice cannot raise another.
    const fn fail(&mut self, step: RotationStep, kind: io::ErrorKind) {
        match self.streak {
            Streak::Quiet => {
                self.streak = Streak::Failing;
                self.notice = Some(RotationNotice { step, kind });
            }
            Streak::Failing => {}
        }
    }
}

fn io_error(path: &Path, operation: LogFileOperation) -> impl FnOnce(io::Error) -> LogFileError {
    let path = path.to_path_buf();
    move |error| LogFileError::Io {
        path,
        operation,
        kind: error.kind(),
    }
}

fn backup_index(name: &[u8], candidate: &[u8]) -> Option<u64> {
    let digits = candidate.strip_prefix(name)?.strip_prefix(b".")?;
    match digits {
        [b'1'..=b'9', rest @ ..] if rest.iter().all(u8::is_ascii_digit) => {
            std::str::from_utf8(digits).ok()?.parse().ok()
        }
        _ => None,
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::{
        fs,
        num::{NonZeroU8, NonZeroU64},
        os::unix::fs::{MetadataExt as _, PermissionsExt as _},
        path::{Path, PathBuf},
        process::Command,
    };

    use dekopon_core::FileHygieneError;

    use super::{LogFile, LogFileError, RotationStep};
    use crate::install::ConsoleSink;

    const RECORD_LIMIT: u64 = ConsoleSink::MAX_BYTES as u64;

    pub(crate) struct Directory {
        pub(crate) root: PathBuf,
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.root));
        }
    }

    pub(crate) fn directory() -> Directory {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "dekopon-log-file-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        Directory { root }
    }

    fn euid() -> u32 {
        let probe = directory();
        fs::metadata(&probe.root).unwrap().uid()
    }

    fn root_owned_parent(name: &str) -> PathBuf {
        Path::new("/tmp").join(format!("dekopon-log-file-{}-{name}", std::process::id()))
    }

    pub(crate) fn open(path: &Path, max_bytes: u64, keep: u8) -> LogFile {
        LogFile::open(
            path,
            euid(),
            NonZeroU64::new(max_bytes).unwrap(),
            NonZeroU8::new(keep).unwrap(),
        )
        .expect("log file opens")
    }

    fn refusal(path: &Path, euid: u32) -> LogFileError {
        LogFile::open(path, euid, NonZeroU64::MIN, NonZeroU8::MIN).expect_err("refused")
    }

    fn set_bytes(directory: &Path) -> u64 {
        fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum()
    }

    fn file_count(directory: &Path) -> usize {
        fs::read_dir(directory).unwrap().count()
    }

    fn record(byte: u8, len: usize) -> Vec<u8> {
        let mut line = vec![byte; len - 1];
        line.push(b'\n');
        line
    }

    fn ceiling(max_bytes: u64, keep: u8) -> u64 {
        (u64::from(keep) + 1) * max_bytes.max(RECORD_LIMIT)
    }

    fn sized(path: &Path, len: u64) {
        let file = fs::File::create(path).unwrap();
        file.set_len(len).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn rotation_at_the_size_bound_keeps_at_most_keep_backups_under_the_ceiling() {
        let directory = directory();
        let path = directory.root.join("gateway.log");
        let max_bytes = RECORD_LIMIT;
        let mut log = open(&path, max_bytes, 2);
        let line = record(b'a', 1024 * 1024);
        for _ in 0..20 {
            log.write_line(&line).unwrap();
            assert!(file_count(&directory.root) <= 3);
            assert!(set_bytes(&directory.root) <= ceiling(max_bytes, 2));
        }
        assert_eq!(file_count(&directory.root), 3);
        assert!(log.take_notice().is_none());
        for name in ["gateway.log", "gateway.log.1", "gateway.log.2"] {
            let metadata = fs::metadata(directory.root.join(name)).unwrap();
            assert_eq!(metadata.mode() & 0o777, 0o600);
        }
        assert_eq!(
            fs::metadata(directory.root.join("gateway.log.1"))
                .unwrap()
                .len(),
            4 * 1024 * 1024
        );
    }

    #[test]
    fn a_restart_with_a_lower_keep_prunes_the_extra_backups() {
        let directory = directory();
        let path = directory.root.join("broker.log");
        sized(&path, 10);
        for index in 1..=4 {
            sized(&directory.root.join(format!("broker.log.{index}")), 10);
        }
        sized(&directory.root.join("broker.log.04"), 10);
        drop(open(&path, 16, 1));
        let mut names: Vec<_> = fs::read_dir(&directory.root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["broker.log", "broker.log.04", "broker.log.1"]);
    }

    #[test]
    fn a_restart_with_a_lower_byte_bound_truncates_and_deletes_oversized_files() {
        let directory = directory();
        let path = directory.root.join("gateway.log");
        let large = 3 * RECORD_LIMIT;
        sized(&path, large);
        sized(&directory.root.join("gateway.log.1"), large);
        sized(&directory.root.join("gateway.log.2"), 10);
        drop(open(&path, 1024, 2));
        assert!(set_bytes(&directory.root) <= ceiling(1024, 2));
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        assert!(!directory.root.join("gateway.log.1").exists());
        assert_eq!(
            fs::metadata(directory.root.join("gateway.log.2"))
                .unwrap()
                .len(),
            10
        );
    }

    #[test]
    fn a_persistent_rename_failure_keeps_the_ceiling_and_recovers() {
        let directory = directory();
        let path = directory.root.join("gateway.log");
        let max_bytes = 1024;
        let mut log = open(&path, max_bytes, 1);
        let squatter = directory.root.join("gateway.log.1");
        fs::create_dir(&squatter).unwrap();
        fs::write(squatter.join("occupied"), b"x").unwrap();
        let line = record(b'r', 1024 * 1024);
        for _ in 0..12 {
            log.write_line(&line).unwrap();
            assert!(fs::metadata(&path).unwrap().len() <= RECORD_LIMIT);
        }
        let notice = log.take_notice().expect("one notice for the streak");
        assert_eq!(notice.step, RotationStep::Rename);
        assert!(log.take_notice().is_none());
        fs::remove_dir_all(&squatter).unwrap();
        log.write_line(&line).unwrap();
        assert_eq!(fs::metadata(&squatter).unwrap().len() % (1024 * 1024), 0);
        assert_eq!(fs::metadata(&path).unwrap().len(), 1024 * 1024);
        assert!(log.take_notice().is_none());
    }

    #[test]
    fn a_persistent_create_failure_after_the_rename_keeps_the_ceiling_and_recovers() {
        let directory = directory();
        let path = directory.root.join("gateway.log");
        let max_bytes = 1024;
        let mut log = open(&path, max_bytes, 2);
        let line = record(b'c', 1024 * 1024);
        log.write_line(&line).unwrap();
        log.rotate_with(|| {
            fs::create_dir(&path).unwrap();
        });
        for _ in 0..12 {
            log.write_line(&line).unwrap();
            assert!(set_bytes_files(&directory.root) <= ceiling(max_bytes, 2));
            assert!(
                fs::metadata(directory.root.join("gateway.log.1"))
                    .unwrap()
                    .len()
                    <= RECORD_LIMIT
            );
        }
        let notice = log.take_notice().expect("one notice for the streak");
        assert_eq!(notice.step, RotationStep::Create);
        assert!(log.take_notice().is_none());
        fs::remove_dir(&path).unwrap();
        log.write_line(&line).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 1024 * 1024);
        log.write_line(&line).unwrap();
        assert!(directory.root.join("gateway.log.2").is_file());
        assert!(log.take_notice().is_none());
    }

    fn set_bytes_files(directory: &Path) -> u64 {
        fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap())
            .filter(fs::Metadata::is_file)
            .map(|metadata| metadata.len())
            .sum()
    }

    impl LogFile {
        fn rotate_with(&mut self, between: impl FnOnce()) {
            fs::rename(&self.path, self.backup(1)).unwrap();
            self.phase = super::Phase::AwaitingReplacement;
            between();
            self.replace();
        }
    }

    #[test]
    fn an_oversized_record_is_refused_without_writing() {
        let directory = directory();
        let path = directory.root.join("gateway.log");
        let mut log = open(&path, 16, 1);
        let error = log
            .write_line(&record(b'o', ConsoleSink::MAX_BYTES + 1))
            .expect_err("refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    }

    #[test]
    fn startup_refuses_each_unsafe_path() {
        let directory = directory();
        let root = &directory.root;
        let target = root.join("target");
        fs::write(&target, b"").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let symlink = root.join("symlink.log");
        std::os::unix::fs::symlink(&target, &symlink).unwrap();
        assert!(matches!(
            refusal(&symlink, euid()),
            LogFileError::File {
                source: FileHygieneError::NotRegular {
                    observed: "symbolic link",
                    ..
                }
            }
        ));

        let other_owner = root_owned_parent("other.log");
        let refused = refusal(&other_owner, euid() + 1);
        drop(fs::remove_file(&other_owner));
        assert!(matches!(
            refused,
            LogFileError::File {
                source: FileHygieneError::WrongOwner { .. }
            }
        ));

        let readable = root.join("readable.log");
        fs::write(&readable, b"").unwrap();
        fs::set_permissions(&readable, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            refusal(&readable, euid()),
            LogFileError::File {
                source: FileHygieneError::InsecureMode { .. }
            }
        ));

        let linked = root.join("linked.log");
        fs::hard_link(&target, &linked).unwrap();
        assert!(matches!(
            refusal(&linked, euid()),
            LogFileError::File {
                source: FileHygieneError::HardLinked { .. }
            }
        ));

        let shared = root.join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o770)).unwrap();
        assert!(matches!(
            refusal(&shared.join("gateway.log"), euid()),
            LogFileError::File {
                source: FileHygieneError::UnsafeAncestor { .. }
            }
        ));
        assert!(!shared.join("gateway.log").exists());

        let fifo = root_owned_parent("fifo.log");
        assert!(
            Command::new("mkfifo")
                .arg("-m")
                .arg("0666")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let refused = [euid(), euid() + 1].map(|owner| refusal(&fifo, owner));
        fs::remove_file(&fifo).unwrap();
        for refused in refused {
            assert!(matches!(
                refused,
                LogFileError::File {
                    source: FileHygieneError::NotRegular {
                        observed: "named pipe",
                        ..
                    }
                }
            ));
        }

        let backups = root.join("backups");
        fs::create_dir(&backups).unwrap();
        fs::set_permissions(&backups, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&target, backups.join("gateway.log.1")).unwrap();
        assert!(matches!(
            refusal(&backups.join("gateway.log"), euid()),
            LogFileError::File {
                source: FileHygieneError::NotRegular {
                    observed: "symbolic link",
                    ..
                }
            }
        ));
    }
}
