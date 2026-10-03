/// Native entropy failure or a malformed host response; Wasm host refusals trap the invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RandomError {
    /// The native test port cannot supply the requested bytes.
    SourceUnavailable,
    /// The guest host response did not contain the requested number of bytes.
    InvalidLength,
}

impl std::fmt::Display for RandomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceUnavailable => f.write_str("entropy source unavailable"),
            Self::InvalidLength => f.write_str("entropy source returned an unexpected length"),
        }
    }
}

impl std::error::Error for RandomError {}

pub(crate) fn fill_chunks(
    out: &mut [u8],
    mut read: impl FnMut(&mut [u8]) -> Result<(), RandomError>,
) -> Result<(), RandomError> {
    for chunk in out.chunks_mut(4096) {
        read(chunk)?;
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
mod bindings {
    wit_bindgen::generate!({ path: "wit", world: "random-client", generate_all });
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn read(out: &mut [u8]) -> Result<(), RandomError> {
    let bytes = bindings::dekopon::random::source::get_random_bytes(out.len() as u32);
    if bytes.len() != out.len() {
        return Err(RandomError::InvalidLength);
    }
    out.copy_from_slice(&bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{RandomError, fill_chunks};

    #[test]
    fn large_buffers_are_filled_in_bounded_chunks() {
        let mut buffer = vec![0; 9000];
        let mut lengths = Vec::new();
        fill_chunks(&mut buffer, |chunk| {
            lengths.push(chunk.len());
            chunk.fill(42);
            Ok(())
        })
        .expect("source works");
        assert_eq!(lengths, [4096, 4096, 808]);
        assert!(buffer.iter().all(|byte| *byte == 42));
    }

    #[test]
    fn source_failure_is_returned_without_more_reads() {
        let mut calls = 0;
        let error = fill_chunks(&mut [0; 8193], |_| {
            calls += 1;
            Err(RandomError::SourceUnavailable)
        });
        assert!(matches!(error, Err(RandomError::SourceUnavailable)));
        assert_eq!(calls, 1);
    }
}
