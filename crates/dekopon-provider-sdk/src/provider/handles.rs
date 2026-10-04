use std::marker::PhantomData;

use serde::de::DeserializeOwned;

use super::{ImportSet, Needs, SdkFailure, sealed};

/// Authorized buffered HTTP and asset-backed request operations.
pub struct Http(PhantomData<()>);

impl Http {
    /// Sends a buffered HTTP request under the broker's invocation grant.
    pub fn send(
        &self,
        request: crate::http::Request,
    ) -> Result<crate::http::Response, crate::http::HttpError> {
        #[cfg(target_arch = "wasm32")]
        {
            crate::http::send(request)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            super::port::send(request)
        }
    }

    /// Sends an asset-backed request and returns a spooled response body.
    pub fn stream(
        &self,
        request: crate::http::StreamedRequest<'_>,
    ) -> Result<crate::http::StreamedResponse, crate::http::HttpError> {
        #[cfg(target_arch = "wasm32")]
        {
            crate::http::stream(request)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            super::port::stream(request)
        }
    }
}

impl sealed::Needs for Http {
    fn grant() -> Result<Self, SdkFailure> {
        Ok(Self(PhantomData))
    }
}
impl Needs for Http {
    const IMPORTS: ImportSet = ImportSet::HTTP.union(ImportSet::ASSETS);
}

/// Authorized broker wall clock.
pub struct Clock(PhantomData<()>);

impl Clock {
    /// Reads the wall clock during an authorized call.
    #[must_use]
    pub fn now_unix_millis(&self) -> u64 {
        #[cfg(target_arch = "wasm32")]
        {
            crate::clock::now_unix_millis()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            super::port::now_unix_millis()
        }
    }
}
impl sealed::Needs for Clock {
    fn grant() -> Result<Self, SdkFailure> {
        Ok(Self(PhantomData))
    }
}
impl Needs for Clock {
    const IMPORTS: ImportSet = ImportSet::CLOCK;
}

/// Authorized invocation-relative monotonic clock.
pub struct Monotonic(PhantomData<()>);

impl Monotonic {
    /// Reads elapsed nanoseconds since this invocation's store was created.
    #[must_use]
    pub fn now_nanos(&self) -> u64 {
        #[cfg(target_arch = "wasm32")]
        {
            crate::clock::now_nanos()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            super::port::now_nanos()
        }
    }
}
impl sealed::Needs for Monotonic {
    fn grant() -> Result<Self, SdkFailure> {
        Ok(Self(PhantomData))
    }
}
impl Needs for Monotonic {
    const IMPORTS: ImportSet = ImportSet::MONOTONIC;
}

/// Authorized broker OS entropy source.
pub struct Random(PhantomData<()>);

impl Random {
    /// Fills a buffer with OS entropy, splitting calls at the broker's per-read ceiling.
    pub fn fill(&self, out: &mut [u8]) {
        #[cfg(target_arch = "wasm32")]
        {
            crate::random::fill_chunks(out, crate::random::read)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            crate::random::fill_chunks(out, super::port::fill_random)
        }
    }
}
impl sealed::Needs for Random {
    fn grant() -> Result<Self, SdkFailure> {
        Ok(Self(PhantomData))
    }
}
impl Needs for Random {
    const IMPORTS: ImportSet = ImportSet::RANDOM;
}

/// Parsed provider settings, loaded once for this authorized call.
pub struct Settings<T> {
    value: T,
}

impl<T> Settings<T> {
    /// Returns the settings parsed at the authorized call boundary.
    pub fn into_inner(self) -> T {
        self.value
    }
}
impl<T: DeserializeOwned> sealed::Needs for Settings<T> {
    fn grant() -> Result<Self, SdkFailure> {
        #[cfg(target_arch = "wasm32")]
        let json = settings_bindings::dekopon::settings::config::get();
        #[cfg(not(target_arch = "wasm32"))]
        let json = super::port::settings();
        parse_settings(json).map(|value| Self { value })
    }
}
impl<T: DeserializeOwned> Needs for Settings<T> {
    const IMPORTS: ImportSet = ImportSet::SETTINGS;
}

fn parse_settings<T: DeserializeOwned>(json: Option<String>) -> Result<T, SdkFailure> {
    json.ok_or(SdkFailure::InvalidSettings).and_then(|json| {
        serde_json::from_str(&json).map_err(|_invalid_json| SdkFailure::InvalidSettings)
    })
}

#[cfg(test)]
mod tests {
    use super::{Clock, Http, ImportSet, Jsonl, Needs, Settings, Storage, parse_settings};
    use crate::provider::SdkFailure;

    #[test]
    fn needs_union_deduplicates_interfaces_and_pure_needs_import_nothing() {
        assert_eq!(<() as Needs>::IMPORTS, ImportSet::EMPTY);
        let imports = <(Http, Clock, Storage<Jsonl>, Http) as Needs>::IMPORTS;
        assert!(imports.contains(ImportSet::HTTP));
        assert!(imports.contains(ImportSet::ASSETS));
        assert!(imports.contains(ImportSet::CLOCK));
        assert!(imports.contains(ImportSet::JSONL));
        assert!(!imports.contains(ImportSet::SETTINGS));
        assert_eq!(<(Settings<String>,) as Needs>::IMPORTS, ImportSet::SETTINGS);
    }

    #[test]
    fn invalid_settings_never_expose_json_in_sdk_failure() {
        assert_eq!(
            parse_settings::<u64>(None),
            Err(SdkFailure::InvalidSettings)
        );
        assert_eq!(
            parse_settings::<u64>(Some("secret".into())),
            Err(SdkFailure::InvalidSettings)
        );
        assert_eq!(parse_settings::<u64>(Some("42".into())), Ok(42));
    }
}

#[cfg(target_arch = "wasm32")]
mod settings_bindings {
    wit_bindgen::generate!({ path: "wit", world: "settings-client", generate_all });
}

/// JSONL storage operations.
pub struct Jsonl;
/// Durable file storage operations.
pub struct DurableFiles;

mod storage_kind {
    use super::ImportSet;
    pub trait Kind: Sized {
        const IMPORTS: ImportSet;
    }
    impl Kind for super::Jsonl {
        const IMPORTS: ImportSet = ImportSet::JSONL;
    }
    impl Kind for super::DurableFiles {
        const IMPORTS: ImportSet = ImportSet::DURABLE_FILES;
    }
}

/// Authorized storage interface selected by `K`.
pub struct Storage<K: storage_kind::Kind>(PhantomData<K>);
impl<K: storage_kind::Kind> sealed::Needs for Storage<K> {
    fn grant() -> Result<Self, SdkFailure> {
        #[cfg(target_arch = "wasm32")]
        {
            Ok(Self(PhantomData))
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            Err(SdkFailure::ComponentHarnessRequired)
        }
    }
}
impl<K: storage_kind::Kind + 'static> Needs for Storage<K> {
    const IMPORTS: ImportSet = K::IMPORTS;
}

impl Storage<Jsonl> {
    /// Returns the size of a named JSONL file.
    pub fn size(&self, name: &str) -> Result<u64, crate::storage::jsonl::StorageError> {
        crate::storage::jsonl::size(name)
    }
    /// Reads one bounded chunk from a JSONL file.
    pub fn read_chunk(
        &self,
        name: &str,
        offset: u64,
        max_bytes: u32,
    ) -> Result<crate::storage::jsonl::Chunk, crate::storage::jsonl::StorageError> {
        crate::storage::jsonl::read_chunk(name, offset, max_bytes)
    }
    /// Appends one record with an expected size check.
    pub fn append(
        &self,
        name: &str,
        expected_size: u64,
        record: &[u8],
    ) -> Result<u64, crate::storage::jsonl::StorageError> {
        crate::storage::jsonl::append(name, expected_size, record)
    }
    /// Replaces one JSONL file with an expected size check.
    pub fn replace(
        &self,
        name: &str,
        expected_size: u64,
        contents: &[u8],
    ) -> Result<(), crate::storage::jsonl::StorageError> {
        crate::storage::jsonl::replace(name, expected_size, contents)
    }
}

impl Storage<DurableFiles> {
    /// Opens a durable file under the broker's storage grant.
    pub fn open(
        &self,
        name: &str,
        options: crate::storage::durable_files::OpenOptions,
    ) -> Result<crate::storage::durable_files::File, crate::storage::durable_files::StorageError>
    {
        crate::storage::durable_files::open(name, options)
    }
    /// Returns metadata for a durable file, if present.
    pub fn stat(
        &self,
        name: &str,
    ) -> Result<
        Option<crate::storage::durable_files::FileStat>,
        crate::storage::durable_files::StorageError,
    > {
        crate::storage::durable_files::stat(name)
    }
    /// Removes a durable file.
    pub fn remove(
        &self,
        name: &str,
        mode: crate::storage::durable_files::Durability,
    ) -> Result<(), crate::storage::durable_files::StorageError> {
        crate::storage::durable_files::remove(name, mode)
    }
    /// Atomically renames a durable file.
    pub fn rename_atomic(
        &self,
        from: &str,
        to: &str,
        replace: bool,
        mode: crate::storage::durable_files::Durability,
    ) -> Result<(), crate::storage::durable_files::StorageError> {
        crate::storage::durable_files::rename_atomic(from, to, replace, mode)
    }
    /// Returns host-generated random bytes.
    pub fn random_bytes(
        &self,
        length: u32,
    ) -> Result<Vec<u8>, crate::storage::durable_files::StorageError> {
        crate::storage::durable_files::random_bytes(length)
    }
    /// Returns the host's monotonic time in nanoseconds.
    pub fn monotonic_time_ns(&self) -> Result<u64, crate::storage::durable_files::StorageError> {
        crate::storage::durable_files::monotonic_time_ns()
    }
    /// Returns the host's wall time in milliseconds.
    pub fn wall_time_ms(&self) -> Result<u64, crate::storage::durable_files::StorageError> {
        crate::storage::durable_files::wall_time_ms()
    }
}

/// Authorized conversation asset operations.
pub struct Assets(PhantomData<()>);
impl Assets {
    /// Opens a reference passed into this invocation.
    pub fn open(&self, reference: &str) -> Result<crate::asset::Handle, crate::asset::AssetError> {
        crate::asset::open(reference)
    }
    /// Allocates an output asset.
    pub fn allocate(
        &self,
        content_type: &str,
        encoding: crate::asset::Encoding,
    ) -> Result<crate::asset::Writer, crate::asset::AssetError> {
        crate::asset::allocate(content_type, encoding)
    }
    /// Attaches an asset writer.
    pub fn attach(
        &self,
        writer: crate::asset::Writer,
    ) -> Result<crate::asset::Handle, crate::asset::AssetError> {
        crate::asset::attach(writer)
    }
    /// Lists the conversation's asset metadata.
    #[must_use]
    pub fn list(&self) -> Vec<crate::asset::Info> {
        crate::asset::list()
    }
    /// Removes an unsent asset.
    pub fn remove(&self, handle: &crate::asset::Handle) -> Result<(), crate::asset::AssetError> {
        crate::asset::remove(handle)
    }
    /// Marks an asset for delivery.
    pub fn send(&self, handle: &crate::asset::Handle) -> Result<(), crate::asset::AssetError> {
        crate::asset::send(handle)
    }
}
impl sealed::Needs for Assets {
    fn grant() -> Result<Self, SdkFailure> {
        #[cfg(target_arch = "wasm32")]
        {
            Ok(Self(PhantomData))
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            Err(SdkFailure::ComponentHarnessRequired)
        }
    }
}
impl Needs for Assets {
    const IMPORTS: ImportSet = ImportSet::ASSETS;
}
