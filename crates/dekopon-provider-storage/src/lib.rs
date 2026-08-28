//! Feature-gated guest facade for `dekopon:storage@0.1.0`.
//!
//! This crate contains component bindings only. It cannot select a host path, namespace,
//! transaction, authority, or storage backend.

#![forbid(unsafe_code)]

/// The imported storage WIT contract used by the generated guest bindings.
pub const STORAGE_WIT: &str = include_str!("../wit/deps/storage.wit");

#[cfg(feature = "jsonl")]
mod jsonl_bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "jsonl-client",
        generate_all,
    });
}

#[cfg(feature = "durable-files")]
mod durable_bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "durable-files-client",
        generate_all,
    });
}

#[cfg(feature = "jsonl")]
pub mod jsonl {
    //! Curated JSONL operations. Mutations remain provisional for the provider invocation.

    use std::{error::Error, fmt};

    use super::jsonl_bindings::dekopon::storage::jsonl as wit;

    /// One bounded read result.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct Chunk {
        /// Bytes beginning at the requested offset.
        pub bytes: Vec<u8>,
        /// Offset at which the next read should begin.
        pub next_offset: u64,
        /// Whether the complete current file was observed.
        pub eof: bool,
    }

    /// Stable storage failure classes.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum StorageError {
        NotFound,
        AlreadyExists,
        InvalidName,
        InvalidArgument,
        PermissionDenied,
        QuotaExceeded,
        Busy,
        Timeout,
        Unsupported,
        Corrupt,
        Io,
    }

    impl fmt::Display for StorageError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "{}",
                match self {
                    Self::NotFound => "not found",
                    Self::AlreadyExists => "already exists",
                    Self::InvalidName => "invalid logical name",
                    Self::InvalidArgument => "invalid argument",
                    Self::PermissionDenied => "permission denied",
                    Self::QuotaExceeded => "quota exceeded",
                    Self::Busy => "busy",
                    Self::Timeout => "timeout",
                    Self::Unsupported => "unsupported",
                    Self::Corrupt => "corrupt",
                    Self::Io => "storage I/O failed",
                }
            )
        }
    }

    impl Error for StorageError {}

    /// Returns the current logical file size.
    pub fn size(name: &str) -> Result<u64, StorageError> {
        wit::size(name).map_err(map_error)
    }

    /// Reads at most `max_bytes` beginning at `offset`.
    pub fn read_chunk(name: &str, offset: u64, max_bytes: u32) -> Result<Chunk, StorageError> {
        wit::read_chunk(name, offset, max_bytes)
            .map(|chunk| Chunk {
                bytes: chunk.bytes,
                next_offset: chunk.next_offset,
                eof: chunk.eof,
            })
            .map_err(map_error)
    }

    /// Appends one record and exactly one host-supplied LF when `expected_size` still matches.
    pub fn append(name: &str, expected_size: u64, record: &[u8]) -> Result<u64, StorageError> {
        wit::append(name, expected_size, record).map_err(map_error)
    }

    /// Replaces a file with empty or complete LF-terminated JSONL when its size still matches.
    pub fn replace(name: &str, expected_size: u64, contents: &[u8]) -> Result<(), StorageError> {
        wit::replace(name, expected_size, contents).map_err(map_error)
    }

    fn map_error(error: wit::StorageError) -> StorageError {
        match error {
            wit::StorageError::NotFound => StorageError::NotFound,
            wit::StorageError::AlreadyExists => StorageError::AlreadyExists,
            wit::StorageError::InvalidName => StorageError::InvalidName,
            wit::StorageError::InvalidArgument => StorageError::InvalidArgument,
            wit::StorageError::PermissionDenied => StorageError::PermissionDenied,
            wit::StorageError::QuotaExceeded => StorageError::QuotaExceeded,
            wit::StorageError::Busy => StorageError::Busy,
            wit::StorageError::Timeout => StorageError::Timeout,
            wit::StorageError::Unsupported => StorageError::Unsupported,
            wit::StorageError::Corrupt => StorageError::Corrupt,
            wit::StorageError::Io => StorageError::Io,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{StorageError, map_error, wit};

        /// Eleven hand-written arms across two independently declared enums.
        ///
        /// Nothing else checks them: a transposed pair reports "busy" where the host said "quota
        /// exceeded", and the variant is the guest's only signal about why a storage call failed.
        /// A variant added to the WIT enum breaks `map_error`'s match, so this table only has to
        /// pin the pairing.
        #[test]
        fn every_wit_error_maps_to_its_own_variant() {
            let table = [
                (wit::StorageError::NotFound, StorageError::NotFound),
                (
                    wit::StorageError::AlreadyExists,
                    StorageError::AlreadyExists,
                ),
                (wit::StorageError::InvalidName, StorageError::InvalidName),
                (
                    wit::StorageError::InvalidArgument,
                    StorageError::InvalidArgument,
                ),
                (
                    wit::StorageError::PermissionDenied,
                    StorageError::PermissionDenied,
                ),
                (
                    wit::StorageError::QuotaExceeded,
                    StorageError::QuotaExceeded,
                ),
                (wit::StorageError::Busy, StorageError::Busy),
                (wit::StorageError::Timeout, StorageError::Timeout),
                (wit::StorageError::Unsupported, StorageError::Unsupported),
                (wit::StorageError::Corrupt, StorageError::Corrupt),
                (wit::StorageError::Io, StorageError::Io),
            ];
            assert_eq!(table.len(), 11, "every WIT failure class must be covered");
            for (wire, expected) in table {
                assert_eq!(map_error(wire), expected, "{wire:?} must not be remapped");
            }
        }
    }
}

#[cfg(feature = "durable-files")]
pub mod durable_files {
    //! Namespace-bound durable-file operations for guest-owned storage engines.

    use std::{error::Error, fmt};

    use super::durable_bindings::dekopon::storage::durable_files as wit;

    /// Validated open intent. At least one of read or write must be selected.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct OpenOptions {
        pub read: bool,
        pub write: bool,
        pub create: bool,
        pub create_new: bool,
        pub delete_on_close: bool,
    }

    impl OpenOptions {
        #[must_use]
        pub const fn new() -> Self {
            Self {
                read: false,
                write: false,
                create: false,
                create_new: false,
                delete_on_close: false,
            }
        }

        #[must_use]
        pub const fn read(mut self, enabled: bool) -> Self {
            self.read = enabled;
            self
        }
        #[must_use]
        pub const fn write(mut self, enabled: bool) -> Self {
            self.write = enabled;
            self
        }
        #[must_use]
        pub const fn create(mut self, enabled: bool) -> Self {
            self.create = enabled;
            self
        }
        #[must_use]
        pub const fn create_new(mut self, enabled: bool) -> Self {
            self.create_new = enabled;
            self
        }
        #[must_use]
        pub const fn delete_on_close(mut self, enabled: bool) -> Self {
            self.delete_on_close = enabled;
            self
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum Durability {
        Data,
        DataAndMetadata,
        Full,
    }

    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub enum LockLevel {
        None,
        Shared,
        Reserved,
        Pending,
        Exclusive,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct FileStat {
        pub size: u64,
        pub identity: u64,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum StorageError {
        NotFound,
        AlreadyExists,
        InvalidName,
        InvalidArgument,
        PermissionDenied,
        QuotaExceeded,
        Busy,
        Timeout,
        Unsupported,
        Corrupt,
        Io,
    }

    impl fmt::Display for StorageError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "{}",
                match self {
                    Self::NotFound => "not found",
                    Self::AlreadyExists => "already exists",
                    Self::InvalidName => "invalid logical name",
                    Self::InvalidArgument => "invalid argument",
                    Self::PermissionDenied => "permission denied",
                    Self::QuotaExceeded => "quota exceeded",
                    Self::Busy => "busy",
                    Self::Timeout => "timeout",
                    Self::Unsupported => "unsupported",
                    Self::Corrupt => "corrupt",
                    Self::Io => "storage I/O failed",
                }
            )
        }
    }

    impl Error for StorageError {}

    /// One guest handle. Dropping it closes the host resource and releases its locks.
    pub struct File(wit::File);

    impl File {
        pub fn read_at(&self, offset: u64, max_bytes: u32) -> Result<Vec<u8>, StorageError> {
            self.0.read_at(offset, max_bytes).map_err(map_error)
        }
        pub fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<(), StorageError> {
            self.0.write_at(offset, bytes).map_err(map_error)
        }
        pub fn size(&self) -> Result<u64, StorageError> {
            self.0.size().map_err(map_error)
        }
        pub fn truncate(&self, size: u64) -> Result<(), StorageError> {
            self.0.truncate(size).map_err(map_error)
        }
        pub fn sync(&self, mode: Durability) -> Result<(), StorageError> {
            self.0.sync(map_durability(mode)).map_err(map_error)
        }
        pub fn lock(&self, level: LockLevel) -> Result<(), StorageError> {
            self.0.lock(map_lock(level)).map_err(map_error)
        }
        pub fn unlock(&self, to: LockLevel) -> Result<(), StorageError> {
            self.0.unlock(map_lock(to)).map_err(map_error)
        }
        pub fn check_reserved_lock(&self) -> Result<bool, StorageError> {
            self.0.check_reserved_lock().map_err(map_error)
        }
    }

    pub fn open(name: &str, options: OpenOptions) -> Result<File, StorageError> {
        let mut flags = wit::OpenFlags::empty();
        flags.set(wit::OpenFlags::READ, options.read);
        flags.set(wit::OpenFlags::WRITE, options.write);
        flags.set(wit::OpenFlags::CREATE, options.create);
        flags.set(wit::OpenFlags::CREATE_NEW, options.create_new);
        flags.set(wit::OpenFlags::DELETE_ON_CLOSE, options.delete_on_close);
        wit::open(name, flags).map(File).map_err(map_error)
    }

    pub fn stat(name: &str) -> Result<Option<FileStat>, StorageError> {
        wit::stat(name)
            .map(|value| {
                value.map(|stat| FileStat {
                    size: stat.size,
                    identity: stat.identity,
                })
            })
            .map_err(map_error)
    }

    pub fn remove(name: &str, mode: Durability) -> Result<(), StorageError> {
        wit::remove(name, map_durability(mode)).map_err(map_error)
    }

    pub fn rename_atomic(
        from: &str,
        to: &str,
        replace: bool,
        mode: Durability,
    ) -> Result<(), StorageError> {
        wit::rename_atomic(from, to, replace, map_durability(mode)).map_err(map_error)
    }

    pub fn random_bytes(length: u32) -> Result<Vec<u8>, StorageError> {
        wit::random_bytes(length).map_err(map_error)
    }
    pub fn monotonic_time_ns() -> Result<u64, StorageError> {
        wit::monotonic_time_ns().map_err(map_error)
    }
    pub fn wall_time_ms() -> Result<u64, StorageError> {
        wit::wall_time_ms().map_err(map_error)
    }

    fn map_durability(value: Durability) -> wit::Durability {
        match value {
            Durability::Data => wit::Durability::Data,
            Durability::DataAndMetadata => wit::Durability::DataAndMetadata,
            Durability::Full => wit::Durability::Full,
        }
    }
    fn map_lock(value: LockLevel) -> wit::LockLevel {
        match value {
            LockLevel::None => wit::LockLevel::None,
            LockLevel::Shared => wit::LockLevel::Shared,
            LockLevel::Reserved => wit::LockLevel::Reserved,
            LockLevel::Pending => wit::LockLevel::Pending,
            LockLevel::Exclusive => wit::LockLevel::Exclusive,
        }
    }
    fn map_error(error: wit::StorageError) -> StorageError {
        match error {
            wit::StorageError::NotFound => StorageError::NotFound,
            wit::StorageError::AlreadyExists => StorageError::AlreadyExists,
            wit::StorageError::InvalidName => StorageError::InvalidName,
            wit::StorageError::InvalidArgument => StorageError::InvalidArgument,
            wit::StorageError::PermissionDenied => StorageError::PermissionDenied,
            wit::StorageError::QuotaExceeded => StorageError::QuotaExceeded,
            wit::StorageError::Busy => StorageError::Busy,
            wit::StorageError::Timeout => StorageError::Timeout,
            wit::StorageError::Unsupported => StorageError::Unsupported,
            wit::StorageError::Corrupt => StorageError::Corrupt,
            wit::StorageError::Io => StorageError::Io,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{StorageError, map_error, wit};

        /// Eleven hand-written arms across two independently declared enums.
        ///
        /// Nothing else checks them: a transposed pair reports "busy" where the host said "quota
        /// exceeded", and the variant is the guest's only signal about why a storage call failed.
        /// A variant added to the WIT enum breaks `map_error`'s match, so this table only has to
        /// pin the pairing.
        #[test]
        fn every_wit_error_maps_to_its_own_variant() {
            let table = [
                (wit::StorageError::NotFound, StorageError::NotFound),
                (
                    wit::StorageError::AlreadyExists,
                    StorageError::AlreadyExists,
                ),
                (wit::StorageError::InvalidName, StorageError::InvalidName),
                (
                    wit::StorageError::InvalidArgument,
                    StorageError::InvalidArgument,
                ),
                (
                    wit::StorageError::PermissionDenied,
                    StorageError::PermissionDenied,
                ),
                (
                    wit::StorageError::QuotaExceeded,
                    StorageError::QuotaExceeded,
                ),
                (wit::StorageError::Busy, StorageError::Busy),
                (wit::StorageError::Timeout, StorageError::Timeout),
                (wit::StorageError::Unsupported, StorageError::Unsupported),
                (wit::StorageError::Corrupt, StorageError::Corrupt),
                (wit::StorageError::Io, StorageError::Io),
            ];
            assert_eq!(table.len(), 11, "every WIT failure class must be covered");
            for (wire, expected) in table {
                assert_eq!(map_error(wire), expected, "{wire:?} must not be remapped");
            }
        }
    }
}
