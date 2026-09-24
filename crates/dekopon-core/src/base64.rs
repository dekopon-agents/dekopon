pub use ::base64::{
    DecodeError, Engine, display::Base64Display, engine::general_purpose::STANDARD,
    read::DecoderReader, write::EncoderWriter,
};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum CodecError {
    #[error("asset length exceeds the codec range")]
    TooLarge,
    #[error("asset is not canonical standard base64")]
    InvalidEncoding,
}

pub fn encoded_len(bytes: u64) -> Result<u64, CodecError> {
    let groups = bytes / 3 + u64::from(!bytes.is_multiple_of(3));
    groups.checked_mul(4).ok_or(CodecError::TooLarge)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadOffset {
    pub encoded: u64,
    pub skip: u8,
}

pub fn read_offset(decoded: u64) -> Result<ReadOffset, CodecError> {
    Ok(ReadOffset {
        encoded: (decoded / 3).checked_mul(4).ok_or(CodecError::TooLarge)?,
        skip: (decoded % 3) as u8,
    })
}

pub fn decoded_len(stored: u64, tail: &[u8]) -> Result<u64, CodecError> {
    if stored == 0 {
        return Ok(0);
    }
    if !stored.is_multiple_of(4) || tail.len() != 2 {
        return Err(CodecError::InvalidEncoding);
    }
    let padding = tail.iter().rev().take_while(|byte| **byte == b'=').count() as u64;
    (stored / 4)
        .checked_mul(3)
        .and_then(|len| len.checked_sub(padding))
        .ok_or(CodecError::InvalidEncoding)
}

#[derive(Debug, Default)]
pub struct Validator {
    quantum: [u8; 4],
    filled: usize,
    padded: bool,
    invalid: bool,
    decoded: u64,
}

impl Validator {
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        if self.invalid {
            return Err(CodecError::InvalidEncoding);
        }
        for byte in bytes {
            if self.padded {
                self.invalid = true;
                return Err(CodecError::InvalidEncoding);
            }
            self.quantum[self.filled] = *byte;
            self.filled += 1;
            if self.filled == 4 {
                let mut decoded = [0; 3];
                let len = match STANDARD.decode_slice(self.quantum, &mut decoded) {
                    Ok(len) => len,
                    Err(_) => {
                        self.invalid = true;
                        return Err(CodecError::InvalidEncoding);
                    }
                };
                self.decoded = self
                    .decoded
                    .checked_add(len as u64)
                    .ok_or(CodecError::TooLarge)?;
                self.padded = self.quantum[3] == b'=';
                self.filled = 0;
            }
        }
        Ok(())
    }

    pub fn decoded_len(&self) -> u64 {
        self.decoded
    }

    pub fn finish(&self) -> Result<(), CodecError> {
        if self.invalid || self.filled != 0 {
            Err(CodecError::InvalidEncoding)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_lengths_and_offsets_are_exact_and_checked() {
        for bytes in 0..100 {
            assert_eq!(
                encoded_len(bytes).unwrap(),
                ::base64::encoded_len(bytes as usize, true).unwrap() as u64
            );
            let offset = read_offset(bytes).unwrap();
            assert_eq!(offset.encoded / 4 * 3 + u64::from(offset.skip), bytes);
        }
        let maximum = u64::MAX / 4 * 3;
        assert!(encoded_len(maximum).is_ok());
        assert_eq!(encoded_len(maximum + 1), Err(CodecError::TooLarge));
        assert_eq!(read_offset(u64::MAX), Err(CodecError::TooLarge));
    }

    #[test]
    fn every_write_boundary_preserves_canonical_validation() {
        for input in ["", "Zg==", "Zm8=", "Zm9v", "Zm9vYmFy"] {
            for split in 0..=input.len() {
                let mut validator = Validator::default();
                validator.write(&input.as_bytes()[..split]).unwrap();
                validator.write(&input.as_bytes()[split..]).unwrap();
                validator.finish().unwrap();
                assert_eq!(
                    validator.decoded_len(),
                    STANDARD.decode(input).unwrap().len() as u64
                );
            }
        }
    }

    #[test]
    fn invalid_or_incomplete_quanta_cannot_be_attached() {
        for input in [
            "Z", "Zg", "Zg=", "Zh==", "Zm9=", "Zg==AAAA", "Zm9v\n", "____", "====",
        ] {
            let mut validator = Validator::default();
            let result = validator
                .write(input.as_bytes())
                .and_then(|()| validator.finish());
            assert_eq!(result, Err(CodecError::InvalidEncoding), "{input}");
        }
    }

    #[test]
    fn tail_padding_decides_validated_decoded_length() {
        assert_eq!(decoded_len(0, b""), Ok(0));
        assert_eq!(decoded_len(4, b"=="), Ok(1));
        assert_eq!(decoded_len(4, b"8="), Ok(2));
        assert_eq!(decoded_len(4, b"9v"), Ok(3));
        assert_eq!(decoded_len(5, b"=="), Err(CodecError::InvalidEncoding));
    }
}
