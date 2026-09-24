use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use thiserror::Error;

const TRACE_ID_HEX_DIGITS: usize = 32;

/// A trace ID carries no authority: two runs sharing one are correlated, not authorized to each
/// other, and it is chosen by whoever opened the trace.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TraceId([u8; 16]);

impl TraceId {
    pub const fn new(bytes: [u8; 16]) -> Result<Self, TraceIdError> {
        if u128::from_be_bytes(bytes) == 0 {
            return Err(TraceIdError::Zero);
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub const fn to_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[allow(
    clippy::map_err_ignore,
    reason = "the guard below already proved exact width and all-lowercase ASCII hex, so the digit-pair ParseIntError is unreachable"
)]
impl FromStr for TraceId {
    type Err = TraceIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // Uppercase hex is rejected rather than folded because W3C specifies lowercase, and
        // accepting both would let one trace serialize two different ways.
        if value.len() != TRACE_ID_HEX_DIGITS
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(TraceIdError::Malformed);
        }
        let mut bytes = [0_u8; 16];
        for (index, slot) in bytes.iter_mut().enumerate() {
            let start = index * 2;
            *slot = u8::from_str_radix(&value[start..start + 2], 16)
                .map_err(|_| TraceIdError::Malformed)?;
        }
        Self::new(bytes)
    }
}

impl TryFrom<&str> for TraceId {
    type Error = TraceIdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl Serialize for TraceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TraceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TraceIdError {
    #[error("trace identifier must be 32 lowercase hexadecimal digits")]
    Malformed,
    #[error("trace identifier must not be all zeroes")]
    Zero,
}

#[cfg(test)]
mod tests {
    use super::{TraceId, TraceIdError};

    #[test]
    fn a_trace_identifier_round_trips_through_its_wire_form() {
        let bytes = [
            0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e,
            0x47, 0x36,
        ];
        let trace = TraceId::new(bytes).expect("non-zero identifier");
        assert_eq!(trace.to_string(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(
            "4bf92f3577b34da6a3ce929d0e0e4736"
                .parse::<TraceId>()
                .expect("valid wire form"),
            trace
        );
        assert_eq!(trace.to_bytes(), bytes);
        assert_eq!(
            serde_json::to_string(&trace).expect("serializes"),
            "\"4bf92f3577b34da6a3ce929d0e0e4736\""
        );
        assert_eq!(
            serde_json::from_str::<TraceId>("\"4bf92f3577b34da6a3ce929d0e0e4736\"")
                .expect("deserializes"),
            trace
        );
    }

    #[test]
    fn a_trace_identifier_refuses_every_form_that_is_not_one_trace() {
        assert_eq!(TraceId::new([0; 16]), Err(TraceIdError::Zero));
        assert_eq!(
            "00000000000000000000000000000000".parse::<TraceId>(),
            Err(TraceIdError::Zero)
        );
        for value in [
            "",
            "4bf92f3577b34da6a3ce929d0e0e473",
            "4bf92f3577b34da6a3ce929d0e0e47366",
            "4BF92F3577B34DA6A3CE929D0E0E4736",
            "4bf92f3577b34da6a3ce929d0e0e473g",
            "dekopond-session-9f1c4a7b0e35d268",
        ] {
            assert_eq!(
                value.parse::<TraceId>(),
                Err(TraceIdError::Malformed),
                "{value}"
            );
        }
    }

    #[test]
    fn a_trace_identifier_is_a_legal_identifier_component() {
        let trace = TraceId::new([0xff; 16]).expect("non-zero identifier");
        let derived = format!("{trace}-{}", u32::MAX);
        assert!(derived.parse::<crate::InvocationId>().is_ok(), "{derived}");
    }
}
