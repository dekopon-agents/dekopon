use std::{error::Error, fmt};

pub(crate) mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "asset-client",
        generate_all,
    });
}

use bindings::dekopon::asset::asset as wit;
pub use wit::{Encoding, Info};

const CHUNK_BYTES: usize = dekopon_core::asset::MAX_ASSET_CHUNK_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetErrorCode {
    UnknownReference,
    Denied,
    SendsExhausted,
    AlreadySent,
    NotSeekable,
    InvalidEncoding,
    TooLarge,
    OverBudget,
    TooManyAssets,
    Unconfigured,
    Io,
    Internal,
}

impl AssetErrorCode {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetError {
    pub code: AssetErrorCode,
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

#[derive(Debug)]
pub struct Handle(HandleBacking);

#[derive(Debug)]
enum HandleBacking {
    Wit(wit::Handle),
    #[cfg(all(feature = "native-test", not(target_arch = "wasm32")))]
    Native {
        bytes: Vec<u8>,
        cursor: std::cell::Cell<usize>,
    },
}

impl Handle {
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn as_inner(&self) -> &wit::Handle {
        match &self.0 {
            HandleBacking::Wit(handle) => handle,
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn from_inner(handle: wit::Handle) -> Self {
        Self(HandleBacking::Wit(handle))
    }

    #[cfg(all(feature = "native-test", not(target_arch = "wasm32")))]
    pub fn from_test_bytes(bytes: Vec<u8>) -> Result<Self, AssetError> {
        if bytes.len() > CHUNK_BYTES {
            return Err(AssetError {
                code: AssetErrorCode::TooLarge,
                message: "native test response exceeds 64 KiB".to_owned(),
            });
        }
        Ok(Self(HandleBacking::Native {
            bytes,
            cursor: std::cell::Cell::new(0),
        }))
    }

    fn wit(&self) -> Result<&wit::Handle, AssetError> {
        match &self.0 {
            HandleBacking::Wit(handle) => Ok(handle),
            #[cfg(all(feature = "native-test", not(target_arch = "wasm32")))]
            HandleBacking::Native { .. } => Err(AssetError {
                code: AssetErrorCode::Denied,
                message: "native response is read-only".to_owned(),
            }),
        }
    }

    fn read_bytes(&self, offset: Option<u64>, len: u32) -> Result<Vec<u8>, AssetError> {
        match &self.0 {
            HandleBacking::Wit(handle) => match offset {
                Some(offset) => handle.read_at(offset, len).map_err(Into::into),
                None => handle.read(len).map_err(Into::into),
            },
            #[cfg(all(feature = "native-test", not(target_arch = "wasm32")))]
            HandleBacking::Native { bytes, cursor } => {
                let start = offset
                    .map_or(cursor.get(), |offset| {
                        usize::try_from(offset).unwrap_or(usize::MAX)
                    })
                    .min(bytes.len());
                let end = start.saturating_add(len as usize).min(bytes.len());
                if offset.is_none() {
                    cursor.set(end);
                }
                Ok(bytes[start..end].to_vec())
            }
        }
    }

    pub fn info(&self) -> Info {
        match &self.0 {
            HandleBacking::Wit(handle) => handle.info(),
            #[cfg(all(feature = "native-test", not(target_arch = "wasm32")))]
            HandleBacking::Native { bytes, .. } => Info {
                id: None,
                content_type: "text/plain".to_owned(),
                encoding: Encoding::Identity,
                stored_bytes: Some(bytes.len() as u64),
                seekable: true,
                origin: "logs".to_owned(),
                sent: false,
            },
        }
    }

    pub fn read(&self, buffer: &mut [u8]) -> Result<usize, AssetError> {
        read_into(buffer, |len| self.read_bytes(None, len))
    }

    pub fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<usize, AssetError> {
        read_into(buffer, |len| self.read_bytes(Some(offset), len))
    }

    pub fn read_all(&self) -> Result<Vec<u8>, AssetError> {
        read_all_with(self.info().stored_bytes, |len| self.read_bytes(None, len))
    }
}

#[derive(Debug)]
pub struct Writer(wit::Writer);

impl Writer {
    pub fn write_all(&self, bytes: &[u8]) -> Result<(), AssetError> {
        write_all_with(bytes, |chunk| self.0.write(chunk).map_err(Into::into))
    }
}

pub(crate) fn open(reference: &str) -> Result<Handle, AssetError> {
    wit::open(reference)
        .map(|handle| Handle(HandleBacking::Wit(handle)))
        .map_err(Into::into)
}

pub(crate) fn allocate(content_type: &str, encoding: Encoding) -> Result<Writer, AssetError> {
    wit::allocate(content_type, encoding)
        .map(Writer)
        .map_err(Into::into)
}

pub(crate) fn attach(writer: Writer) -> Result<Handle, AssetError> {
    wit::attach(writer.0)
        .map(|handle| Handle(HandleBacking::Wit(handle)))
        .map_err(Into::into)
}

pub(crate) fn list() -> Vec<Info> {
    wit::list()
}

pub(crate) fn remove(handle: &Handle) -> Result<(), AssetError> {
    wit::remove(handle.wit()?).map_err(Into::into)
}

pub(crate) fn send(handle: &Handle) -> Result<(), AssetError> {
    wit::send(handle.wit()?).map_err(Into::into)
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
        Some(len) => Vec::with_capacity(usize::try_from(len).map_err(|_overflow| AssetError {
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
