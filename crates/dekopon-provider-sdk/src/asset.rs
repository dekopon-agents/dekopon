//! Conversation assets through broker-owned handles, never bytes in model-facing JSON.
//!
//! These imports are available only during an authorized `invoke`, not `run-command`.

use std::{error::Error, fmt};

#[doc(hidden)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "asset-client",
        generate_all,
    });
}

use bindings::dekopon::asset::asset as wit;
pub use wit::{Encoding, Info};

const CHUNK_BYTES: usize = dekopon_core::asset::MAX_ASSET_CHUNK_BYTES;

/// Stable failure classes returned by the broker asset host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetErrorCode {
    /// The reference was not passed with this invocation.
    UnknownReference,
    /// The invocation does not grant this operation.
    Denied,
    /// This turn has no sends remaining.
    SendsExhausted,
    /// A sent asset cannot be removed.
    AlreadySent,
    /// This handle does not support positional reads.
    NotSeekable,
    /// The writer does not contain canonical encoded bytes.
    InvalidEncoding,
    /// An asset or invocation byte bound was exceeded.
    TooLarge,
    /// The broker's in-flight budget or disk is exhausted.
    OverBudget,
    /// This invocation has already attached five assets.
    TooManyAssets,
    /// The broker has no asset directory configured.
    Unconfigured,
    /// Asset I/O failed.
    Io,
    /// The broker encountered an internal failure.
    Internal,
}

impl AssetErrorCode {
    /// Returns the WIT enum name for this class.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownReference => "unknown-reference",
            Self::Denied => "denied",
            Self::SendsExhausted => "sends-exhausted",
            Self::AlreadySent => "already-sent",
            Self::NotSeekable => "not-seekable",
            Self::InvalidEncoding => "invalid-encoding",
            Self::TooLarge => "too-large",
            Self::OverBudget => "over-budget",
            Self::TooManyAssets => "too-many-assets",
            Self::Unconfigured => "unconfigured",
            Self::Io => "io",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for AssetErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A bounded failure returned across the asset component boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetError {
    /// Stable machine-readable class.
    pub code: AssetErrorCode,
    /// Bounded provider-safe detail.
    pub message: String,
}

impl fmt::Display for AssetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for AssetError {}

impl From<wit::Error> for AssetError {
    fn from(error: wit::Error) -> Self {
        let code = match error.code {
            wit::ErrorCode::UnknownReference => AssetErrorCode::UnknownReference,
            wit::ErrorCode::Denied => AssetErrorCode::Denied,
            wit::ErrorCode::SendsExhausted => AssetErrorCode::SendsExhausted,
            wit::ErrorCode::AlreadySent => AssetErrorCode::AlreadySent,
            wit::ErrorCode::NotSeekable => AssetErrorCode::NotSeekable,
            wit::ErrorCode::InvalidEncoding => AssetErrorCode::InvalidEncoding,
            wit::ErrorCode::TooLarge => AssetErrorCode::TooLarge,
            wit::ErrorCode::OverBudget => AssetErrorCode::OverBudget,
            wit::ErrorCode::TooManyAssets => AssetErrorCode::TooManyAssets,
            wit::ErrorCode::Unconfigured => AssetErrorCode::Unconfigured,
            wit::ErrorCode::Io => AssetErrorCode::Io,
            wit::ErrorCode::Internal => AssetErrorCode::Internal,
        };
        Self {
            code,
            message: error.message,
        }
    }
}

/// A single-pass input or spooled HTTP response. Dropping it releases the host resource.
#[derive(Debug)]
pub struct Handle(wit::Handle);

impl Handle {
    #[doc(hidden)]
    pub fn as_inner(&self) -> &wit::Handle {
        &self.0
    }

    #[doc(hidden)]
    pub fn from_inner(handle: wit::Handle) -> Self {
        Self(handle)
    }

    /// Returns metadata, never asset bytes.
    pub fn info(&self) -> Info {
        self.0.info()
    }

    /// Reads at most 64 KiB of decoded bytes from this handle's cursor. Zero means EOF.
    pub fn read(&self, buffer: &mut [u8]) -> Result<usize, AssetError> {
        read_into(buffer, |len| self.0.read(len).map_err(Into::into))
    }

    /// Reads at most 64 KiB of decoded bytes at a decoded offset, without moving the cursor.
    pub fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<usize, AssetError> {
        read_into(buffer, |len| self.0.read_at(offset, len).map_err(Into::into))
    }

    /// Reads the remaining decoded bytes, reserving the stored length when known.
    pub fn read_all(&self) -> Result<Vec<u8>, AssetError> {
        read_all_with(self.info().stored_bytes, |len| {
            self.0.read(len).map_err(Into::into)
        })
    }
}

/// An output under construction. Dropping an unattached writer discards its file.
#[derive(Debug)]
pub struct Writer(wit::Writer);

impl Writer {
    /// Appends bytes in host calls of at most 64 KiB. An empty slice makes no host call.
    pub fn write_all(&self, bytes: &[u8]) -> Result<(), AssetError> {
        write_all_with(bytes, |chunk| self.0.write(chunk).map_err(Into::into))
    }
}

/// Opens a reference passed by the gateway for this invocation.
pub fn open(reference: &str) -> Result<Handle, AssetError> {
    wit::open(reference).map(Handle).map_err(Into::into)
}

/// Allocates an output with the stated content type and stored encoding.
pub fn allocate(content_type: &str, encoding: Encoding) -> Result<Writer, AssetError> {
    wit::allocate(content_type, encoding)
        .map(Writer)
        .map_err(Into::into)
}

/// Joins the conversation's temp files, consuming the writer. This does not send it.
pub fn attach(writer: Writer) -> Result<Handle, AssetError> {
    wit::attach(writer.0).map(Handle).map_err(Into::into)
}

/// Returns the conversation's metadata table, not authority to open unreferenced assets.
pub fn list() -> Vec<Info> {
    wit::list()
}

/// Removes an unsent asset when the invocation grants removal.
pub fn remove(handle: &Handle) -> Result<(), AssetError> {
    wit::remove(&handle.0).map_err(Into::into)
}

/// Marks an asset for delivery on this turn's reply when the invocation grants sending.
pub fn send(handle: &Handle) -> Result<(), AssetError> {
    wit::send(&handle.0).map_err(Into::into)
}

fn read_into(
    buffer: &mut [u8],
    read: impl FnOnce(u32) -> Result<Vec<u8>, AssetError>,
) -> Result<usize, AssetError> {
    if buffer.is_empty() {
        return Ok(0);
    }
    let bytes = read(buffer.len().min(CHUNK_BYTES) as u32)?;
    buffer[..bytes.len()].copy_from_slice(&bytes);
    Ok(bytes.len())
}

fn read_all_with(
    stored_bytes: Option<u64>,
    mut read: impl FnMut(u32) -> Result<Vec<u8>, AssetError>,
) -> Result<Vec<u8>, AssetError> {
    let mut bytes = match stored_bytes {
        Some(len) => Vec::with_capacity(usize::try_from(len).map_err(|_| AssetError {
            code: AssetErrorCode::TooLarge,
            message: "asset length does not fit guest memory".into(),
        })?),
        None => Vec::new(),
    };
    loop {
        let chunk = read(CHUNK_BYTES as u32)?;
        if chunk.is_empty() {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&chunk);
    }
}

fn write_all_with(
    bytes: &[u8],
    mut write: impl FnMut(&[u8]) -> Result<(), AssetError>,
) -> Result<(), AssetError> {
    for chunk in bytes.chunks(CHUNK_BYTES) {
        write(chunk)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_limit_matches_the_shared_contract() {
        assert_eq!(CHUNK_BYTES, dekopon_core::asset::MAX_ASSET_CHUNK_BYTES);
    }

    #[test]
    fn error_codes_render_the_wit_names() {
        let codes = [
            wit::ErrorCode::UnknownReference,
            wit::ErrorCode::Denied,
            wit::ErrorCode::SendsExhausted,
            wit::ErrorCode::AlreadySent,
            wit::ErrorCode::NotSeekable,
            wit::ErrorCode::InvalidEncoding,
            wit::ErrorCode::TooLarge,
            wit::ErrorCode::OverBudget,
            wit::ErrorCode::TooManyAssets,
            wit::ErrorCode::Unconfigured,
            wit::ErrorCode::Io,
            wit::ErrorCode::Internal,
        ];
        let block = crate::ASSET_WIT
            .split_once("enum error-code {")
            .unwrap()
            .1
            .split_once('}')
            .unwrap()
            .0;
        let declared = block
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("///"))
            .filter_map(|line| line.strip_suffix(','))
            .collect::<Vec<_>>();
        let mapped = codes.map(|code| {
            AssetError::from(wit::Error {
                code,
                message: "detail".into(),
            })
        });
        assert_eq!(
            declared,
            mapped
                .iter()
                .map(|error| error.code.as_str())
                .collect::<Vec<_>>()
        );
        for error in mapped {
            assert_eq!(error.code.to_string(), error.code.as_str());
            assert_eq!(error.to_string(), format!("{}: detail", error.code));
        }
    }

    #[test]
    fn reads_are_bounded_and_empty_buffers_make_no_call() {
        let mut buffer = vec![0; CHUNK_BYTES + 1];
        let len = read_into(&mut buffer, |len| {
            assert_eq!(len, CHUNK_BYTES as u32);
            Ok(vec![7; len as usize])
        })
        .unwrap();
        assert_eq!(len, CHUNK_BYTES);
        assert_eq!(buffer[CHUNK_BYTES], 0);
        assert_eq!(read_into(&mut [], |_| panic!("empty read")).unwrap(), 0);
        assert_eq!(
            read_into(&mut buffer[..3], |len| {
                assert_eq!(len, 3);
                Ok(vec![1, 2])
            })
            .unwrap(),
            2
        );
    }

    #[test]
    fn read_all_reserves_known_length_and_grows_unknown_length() {
        for stored_bytes in [Some(3), None] {
            let mut chunks = [vec![1, 2], vec![3], vec![]].into_iter();
            let bytes = read_all_with(stored_bytes, |len| {
                assert_eq!(len, CHUNK_BYTES as u32);
                Ok(chunks.next().unwrap())
            })
            .unwrap();
            assert_eq!(bytes, [1, 2, 3]);
            if stored_bytes.is_some() {
                assert_eq!(bytes.capacity(), 3);
            }
        }
    }

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn unrepresentable_stored_length_is_too_large() {
        let error = read_all_with(Some(u64::MAX), |_| panic!("must not read")).unwrap_err();
        assert_eq!(error.code, AssetErrorCode::TooLarge);
    }

    #[test]
    fn writes_are_chunked_and_empty_slices_make_no_call() {
        let bytes = vec![7; CHUNK_BYTES * 2 + 1];
        let mut lengths = Vec::new();
        write_all_with(&bytes, |chunk| {
            lengths.push(chunk.len());
            assert!(chunk.iter().all(|byte| *byte == 7));
            Ok(())
        })
        .unwrap();
        assert_eq!(lengths, [CHUNK_BYTES, CHUNK_BYTES, 1]);
        write_all_with(&[], |_| panic!("empty write")).unwrap();
    }

    #[test]
    fn host_failures_stop_chunking() {
        let error = AssetError {
            code: AssetErrorCode::OverBudget,
            message: "full".into(),
        };
        let mut calls = 0;
        assert_eq!(
            write_all_with(&vec![0; CHUNK_BYTES + 1], |_| {
                calls += 1;
                Err(error.clone())
            }),
            Err(error.clone())
        );
        assert_eq!(calls, 1);
        assert_eq!(read_all_with(None, |_| Err(error.clone())), Err(error));
    }
}
