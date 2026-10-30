use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fmt, fs, io,
    num::NonZeroU32,
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::Command,
};

use thiserror::Error;

use crate::{
    AncestorOwners, AncestorPolicy, FileHygieneError, FileTier, Redacted, check_trusted_ancestors,
    read_trusted_file,
};

pub const JQ_WORKER_MARKER: &str = "DEKOPON_SHELL_JQ_WORKER";

const OPTION: &str = "--env-file";
const MAX_ENV_FILE_BYTES: usize = 65_536;
const RESERVED_PREFIXES: [&str; 3] = ["LD_", "DYLD_", "XDG_"];
// Location variables are refused because their readers echo paths derived from them.
const RESERVED_KEYS: [&str; 8] = [
    JQ_WORKER_MARKER,
    "HOME",
    "TMPDIR",
    "PATH",
    "APPDATA",
    "DEKOPON_CHATGPT_AUTH_FILE",
    "DEKOPON_BROKER_SOCKET",
    "DEKOPON_CONFIG",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum EnvFileProblem {
    #[error("line {line} is not UTF-8")]
    NotUtf8 { line: NonZeroU32 },
    #[error("the file starts with a byte-order mark")]
    ByteOrderMark,
    #[error("line {line} holds a NUL byte")]
    Nul { line: NonZeroU32 },
    #[error("line {line} holds a carriage return that does not end the line")]
    StrayCarriageReturn { line: NonZeroU32 },
    #[error("line {line} does not start with a variable name")]
    InvalidKey { line: NonZeroU32 },
    #[error("line {line} needs '=' directly after the variable name")]
    MissingEquals { line: NonZeroU32 },
    #[error("line {line} has a quote inside an unquoted value")]
    UnquotedSpecial { line: NonZeroU32 },
    #[error("line {line} has an unmatched quote")]
    UnmatchedQuote { line: NonZeroU32 },
    #[error("line {line} has text after its value")]
    TrailingText { line: NonZeroU32 },
    #[error("line {line} sets a variable an environment file may not set")]
    ReservedKey { line: NonZeroU32 },
    #[error("line {second} sets the same variable as line {first}")]
    Duplicate {
        first: NonZeroU32,
        second: NonZeroU32,
    },
}

impl EnvFileProblem {
    #[must_use]
    pub const fn category(self) -> &'static str {
        match self {
            Self::NotUtf8 { .. } => "not-utf8",
            Self::ByteOrderMark => "byte-order-mark",
            Self::Nul { .. } => "nul",
            Self::StrayCarriageReturn { .. } => "stray-carriage-return",
            Self::InvalidKey { .. } => "invalid-key",
            Self::MissingEquals { .. } => "missing-equals",
            Self::UnquotedSpecial { .. } => "unquoted-special",
            Self::UnmatchedQuote { .. } => "unmatched-quote",
            Self::TrailingText { .. } => "trailing-text",
            Self::ReservedKey { .. } => "reserved-key",
            Self::Duplicate { .. } => "duplicate",
        }
    }
}

#[derive(Debug, Error)]
pub enum EnvFileError {
    #[error("{}: {problem}", path.display())]
    Syntax {
        path: PathBuf,
        problem: EnvFileProblem,
    },
    #[error("could not use the environment file")]
    File {
        #[source]
        source: FileHygieneError,
    },
    #[error("{} does not name a file", path.display())]
    NoFileName { path: PathBuf },
    #[error("could not read the working directory ({kind:?})")]
    WorkingDirectory { kind: io::ErrorKind },
    #[error("could not restart the daemon with its environment file ({kind:?})")]
    Exec { kind: io::ErrorKind },
}

impl EnvFileError {
    #[must_use]
    pub const fn category(&self) -> &'static str {
        match self {
            Self::Syntax { problem, .. } => problem.category(),
            Self::File { source } => source.category(),
            Self::NoFileName { .. } => "no-file-name",
            Self::WorkingDirectory { .. } => "working-directory",
            Self::Exec { .. } => "exec",
        }
    }
}

impl From<FileHygieneError> for EnvFileError {
    fn from(source: FileHygieneError) -> Self {
        Self::File { source }
    }
}

pub struct EnvFile {
    pairs: Vec<(String, Redacted<String>)>,
}

impl fmt::Debug for EnvFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvFile")
            .field("variables", &self.pairs.len())
            .finish()
    }
}

impl EnvFile {
    pub fn load(path: &Path, euid: u32) -> Result<Self, EnvFileError> {
        let path = std::env::current_dir()
            .map_err(|error| EnvFileError::WorkingDirectory { kind: error.kind() })?
            .join(path);
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(EnvFileError::NoFileName { path });
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
        let bytes = read_trusted_file(&path, euid, FileTier::Private, MAX_ENV_FILE_BYTES)?;
        let pairs = parse(&bytes).map_err(|problem| EnvFileError::Syntax { path, problem })?;
        Ok(Self { pairs })
    }

    pub fn merged_with(
        self,
        inherited: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Vec<(OsString, OsString)> {
        let mut merged: Vec<_> = inherited.into_iter().collect();
        let absent: Vec<_> = self
            .pairs
            .into_iter()
            .filter(|(key, _)| merged.iter().all(|(known, _)| known != OsStr::new(key)))
            .map(|(key, value)| (key.into(), value.into_inner().into()))
            .collect();
        merged.extend(absent);
        merged
    }
}

/// Only sound once Clap has accepted `arguments` in serve mode: no positional argument and no
/// option that takes a hyphen value can hold the option's text.
#[must_use]
pub fn strip_option(arguments: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut arguments = arguments.into_iter();
    let mut kept = Vec::new();
    while let Some(argument) = arguments.next() {
        if argument == OPTION {
            arguments.next();
            break;
        }
        if argument
            .to_str()
            .is_some_and(|text| text.starts_with("--env-file="))
        {
            break;
        }
        kept.push(argument);
    }
    kept.extend(arguments);
    kept
}

/// Must run before any thread exists; it returns only when the file is refused or exec fails.
#[must_use]
pub fn reexec(path: &Path, euid: u32) -> EnvFileError {
    let file = match EnvFile::load(path, euid) {
        Ok(file) => file,
        Err(error) => return error,
    };
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => return EnvFileError::Exec { kind: error.kind() },
    };
    let mut arguments = std::env::args_os();
    let mut command = Command::new(executable);
    if let Some(argv0) = arguments.next() {
        command.arg0(argv0);
    }
    let error = command
        .args(strip_option(arguments))
        .env_clear()
        .envs(file.merged_with(std::env::vars_os()))
        .exec();
    EnvFileError::Exec { kind: error.kind() }
}

fn line_number(index: usize) -> NonZeroU32 {
    NonZeroU32::MIN.saturating_add(u32::try_from(index).unwrap_or(u32::MAX))
}

fn parse(bytes: &[u8]) -> Result<Vec<(String, Redacted<String>)>, EnvFileProblem> {
    if bytes.starts_with("\u{feff}".as_bytes()) {
        return Err(EnvFileProblem::ByteOrderMark);
    }
    let text = std::str::from_utf8(bytes).map_err(|error| {
        let before = bytes.get(..error.valid_up_to()).unwrap_or_default();
        EnvFileProblem::NotUtf8 {
            line: line_number(before.iter().filter(|byte| **byte == b'\n').count()),
        }
    })?;
    let mut pairs = Vec::new();
    let mut seen: HashMap<&str, NonZeroU32> = HashMap::new();
    for (index, raw) in text.split('\n').enumerate() {
        let line = line_number(index);
        let content = raw.strip_suffix('\r').unwrap_or(raw);
        if content.contains('\0') {
            return Err(EnvFileProblem::Nul { line });
        }
        if content.contains('\r') {
            return Err(EnvFileProblem::StrayCarriageReturn { line });
        }
        let Some((key, value)) = parse_line(content, line)? else {
            continue;
        };
        if RESERVED_KEYS.contains(&key) || RESERVED_PREFIXES.iter().any(|p| key.starts_with(p)) {
            return Err(EnvFileProblem::ReservedKey { line });
        }
        if let Some(first) = seen.insert(key, line) {
            return Err(EnvFileProblem::Duplicate {
                first,
                second: line,
            });
        }
        pairs.push((key.to_owned(), Redacted::from(value.to_owned())));
    }
    Ok(pairs)
}

fn is_blank(character: char) -> bool {
    character == ' ' || character == '\t'
}

fn parse_line(content: &str, line: NonZeroU32) -> Result<Option<(&str, &str)>, EnvFileProblem> {
    let rest = content.trim_start_matches(is_blank);
    if rest.is_empty() || rest.starts_with('#') {
        return Ok(None);
    }
    let rest = match rest.strip_prefix("export") {
        Some(after) if after.starts_with(is_blank) => after.trim_start_matches(is_blank),
        Some(_) | None => rest,
    };
    let key_end = rest
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(rest.len());
    let (key, rest) = rest.split_at(key_end);
    if key.is_empty() || key.starts_with(|character: char| character.is_ascii_digit()) {
        return Err(EnvFileProblem::InvalidKey { line });
    }
    let value = match rest.strip_prefix('=') {
        Some(value) if !value.starts_with(is_blank) => value,
        Some(_) => return Err(EnvFileProblem::MissingEquals { line }),
        None if rest.is_empty() || rest.starts_with(is_blank) => {
            return Err(EnvFileProblem::MissingEquals { line });
        }
        None => return Err(EnvFileProblem::InvalidKey { line }),
    };
    let quoted = ['\'', '"']
        .into_iter()
        .find_map(|quote| Some((quote, value.strip_prefix(quote)?)));
    let (value, after) = match quoted {
        Some((quote, inner)) => inner
            .split_once(quote)
            .ok_or(EnvFileProblem::UnmatchedQuote { line })?,
        None => {
            let end = value.find(is_blank).unwrap_or(value.len());
            let (unquoted, after) = value.split_at(end);
            if unquoted.contains(['\'', '"']) {
                return Err(EnvFileProblem::UnquotedSpecial { line });
            }
            (unquoted, after)
        }
    };
    if !after.chars().all(is_blank) {
        return Err(EnvFileProblem::TrailingText { line });
    }
    Ok(Some((key, value)))
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs::{self, Permissions},
        num::NonZeroU32,
        os::unix::fs::{MetadataExt as _, PermissionsExt as _},
        path::PathBuf,
    };

    use tempfile::TempDir;

    use super::{EnvFile, EnvFileError, EnvFileProblem, JQ_WORKER_MARKER, parse, strip_option};
    use crate::FileHygieneError;

    fn line(number: u32) -> NonZeroU32 {
        NonZeroU32::new(number).unwrap()
    }

    fn pairs(text: &str) -> Vec<(String, String)> {
        parse(text.as_bytes())
            .unwrap()
            .into_iter()
            .map(|(key, value)| (key, value.into_inner()))
            .collect()
    }

    fn private_file(contents: &[u8]) -> (TempDir, PathBuf, u32) {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().canonicalize().unwrap().join("private");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("daemon.env");
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        let uid = fs::metadata(&path).unwrap().uid();
        (root, path, uid)
    }

    #[test]
    fn literal_values_parse_as_written() {
        let parsed = pairs(
            "# comment\n\n  \t\nPLAIN=a#b$c\\d\r\nexport  SPACED='two words # $HOME \\n'\n\t \
             DOUBLE=\"it's\"\nEMPTY=\nEMPTY_SINGLE=''\nEMPTY_DOUBLE=\"\"  \nexport=keyword\nLAST=end",
        );
        let expected = [
            ("PLAIN", "a#b$c\\d"),
            ("SPACED", "two words # $HOME \\n"),
            ("DOUBLE", "it's"),
            ("EMPTY", ""),
            ("EMPTY_SINGLE", ""),
            ("EMPTY_DOUBLE", ""),
            ("export", "keyword"),
            ("LAST", "end"),
        ]
        .map(|(key, value)| (key.to_owned(), value.to_owned()));
        assert_eq!(parsed, expected);
    }

    #[test]
    fn each_grammar_refusal_names_its_line() {
        for (text, problem) in [
            ("OK=1\n1KEY=v", EnvFileProblem::InvalidKey { line: line(2) }),
            (
                "OK=1\nKEY-NAME=v",
                EnvFileProblem::InvalidKey { line: line(2) },
            ),
            ("KEY =v", EnvFileProblem::MissingEquals { line: line(1) }),
            ("KEY= v", EnvFileProblem::MissingEquals { line: line(1) }),
            (
                "export KEY",
                EnvFileProblem::MissingEquals { line: line(1) },
            ),
            (
                "A=1\nB=2\nA=3",
                EnvFileProblem::Duplicate {
                    first: line(1),
                    second: line(3),
                },
            ),
            ("OK=1\nKEY=a\0b", EnvFileProblem::Nul { line: line(2) }),
            (
                "KEY='open",
                EnvFileProblem::UnmatchedQuote { line: line(1) },
            ),
            (
                "KEY=\"open'",
                EnvFileProblem::UnmatchedQuote { line: line(1) },
            ),
            ("KEY=a'b", EnvFileProblem::UnquotedSpecial { line: line(1) }),
            ("KEY='a'b", EnvFileProblem::TrailingText { line: line(1) }),
            (
                "KEY=value # note",
                EnvFileProblem::TrailingText { line: line(1) },
            ),
            (
                "KEY=two words",
                EnvFileProblem::TrailingText { line: line(1) },
            ),
            ("\u{feff}KEY=v", EnvFileProblem::ByteOrderMark),
            (
                "OK=1\nKEY=a\rb\n",
                EnvFileProblem::StrayCarriageReturn { line: line(2) },
            ),
        ] {
            assert_eq!(parse(text.as_bytes()).map(|_| ()), Err(problem), "{text:?}");
        }
        assert_eq!(
            parse(b"OK=1\n\nKEY=\xff").map(|_| ()),
            Err(EnvFileProblem::NotUtf8 { line: line(3) })
        );
    }

    #[test]
    fn reserved_keys_are_refused() {
        for key in [
            JQ_WORKER_MARKER,
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "HOME",
            "XDG_CONFIG_HOME",
        ] {
            assert_eq!(
                parse(format!("OK=1\n{key}=1").as_bytes()).map(|_| ()),
                Err(EnvFileProblem::ReservedKey { line: line(2) }),
                "{key}"
            );
        }
    }

    #[test]
    fn an_env_file_over_the_bound_is_refused() {
        let mut contents = b"KEY=".to_vec();
        contents.resize(65_537, b'a');
        let (_root, path, uid) = private_file(&contents);
        let error = EnvFile::load(&path, uid).unwrap_err();
        assert!(
            matches!(
                error,
                EnvFileError::File {
                    source: FileHygieneError::TooLarge { length: 65_537, .. }
                }
            ),
            "{error}"
        );
        contents.truncate(65_536);
        fs::write(&path, &contents).unwrap();
        EnvFile::load(&path, uid).expect("the bound itself is accepted");
    }

    #[test]
    fn env_file_refuses_unsafe_metadata() {
        let (_root, path, uid) = private_file(b"KEY=v\n");
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            EnvFile::load(&path, uid).unwrap_err(),
            EnvFileError::File {
                source: FileHygieneError::InsecureMode { .. }
            }
        ));
        fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        let shared = tempfile::Builder::new()
            .tempfile_in(PathBuf::from("/tmp").canonicalize().unwrap())
            .unwrap();
        fs::write(shared.path(), b"KEY=v\n").unwrap();
        assert!(matches!(
            EnvFile::load(shared.path(), uid.wrapping_add(1)).unwrap_err(),
            EnvFileError::File {
                source: FileHygieneError::WrongOwner { .. }
            }
        ));
        fs::hard_link(&path, path.with_file_name("alias")).unwrap();
        assert!(matches!(
            EnvFile::load(&path, uid).unwrap_err(),
            EnvFileError::File {
                source: FileHygieneError::HardLinked { .. }
            }
        ));
    }

    #[test]
    fn an_untrusted_ancestor_is_refused() {
        let (_root, path, uid) = private_file(b"KEY=v\n");
        let directory = path.parent().unwrap();
        fs::set_permissions(directory, Permissions::from_mode(0o770)).unwrap();
        assert!(matches!(
            EnvFile::load(&path, uid).unwrap_err(),
            EnvFileError::File {
                source: FileHygieneError::UnsafeAncestor { .. }
            }
        ));
        fs::set_permissions(directory, Permissions::from_mode(0o700)).unwrap();
        let foreign = if uid == 0 {
            std::os::unix::fs::chown(directory, Some(12_345), None).unwrap();
            0
        } else {
            uid.wrapping_add(1)
        };
        let error = EnvFile::load(&path, foreign).unwrap_err();
        assert!(
            matches!(
                error,
                EnvFileError::File {
                    source: FileHygieneError::UntrustedAncestorOwner { .. }
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_symlink_directory_or_fifo_is_refused_without_blocking() {
        let (_root, path, uid) = private_file(b"KEY=v\n");
        let link = path.with_file_name("link.env");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(matches!(
            EnvFile::load(&link, uid).unwrap_err(),
            EnvFileError::File {
                source: FileHygieneError::Io { .. }
            }
        ));
        let directory = path.with_file_name("directory.env");
        fs::create_dir(&directory).unwrap();
        assert!(matches!(
            EnvFile::load(&directory, uid).unwrap_err(),
            EnvFileError::File {
                source: FileHygieneError::NotRegular { .. } | FileHygieneError::Io { .. }
            }
        ));
        let fifo = path.with_file_name("fifo.env");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert!(matches!(
            EnvFile::load(&fifo, uid).unwrap_err(),
            EnvFileError::File {
                source: FileHygieneError::NotRegular { .. }
            }
        ));
    }

    #[test]
    fn inherited_values_win_and_the_option_is_stripped_once() {
        let (_root, path, uid) = private_file(b"FROM_FILE=file\nINHERITED=file\nBLANK=file\n");
        let merged = EnvFile::load(&path, uid).unwrap().merged_with([
            (OsString::from("INHERITED"), OsString::from("inherited")),
            (OsString::from("BLANK"), OsString::new()),
        ]);
        assert_eq!(
            merged,
            [
                ("INHERITED", "inherited"),
                ("BLANK", ""),
                ("FROM_FILE", "file")
            ]
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        );

        let strip = |arguments: &[&str]| -> Vec<OsString> {
            strip_option(arguments.iter().map(OsString::from))
        };
        assert_eq!(
            strip(&["--env-file", "a.env", "--config", "c"]),
            ["--config", "c"]
        );
        assert_eq!(strip(&["--config=c", "--env-file=a.env"]), ["--config=c"]);
        assert_eq!(strip(&["--worker-threads", "2"]), ["--worker-threads", "2"]);
    }
}
