//! The one identifier a Dekopon run is correlated by.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use thiserror::Error;

/// Hexadecimal digits in the wire form of a trace identifier.
const TRACE_ID_HEX_DIGITS: usize = 32;

/// A W3C trace identifier: sixteen bytes written as thirty-two lowercase hexadecimal digits.
///
/// This is the only correlation identifier a run has. Everything one message produced — gateway
/// spans, model turns, shell commands, broker decisions, audit records, provider invocations, HTTP
/// egress — carries this value, so an operator reconstructs the run by asking the telemetry store
/// for one identifier rather than joining two namespaces. Dekopon used to mint a second,
/// free-form trace identifier of its own beside the W3C one; there is one now.
///
/// It is not an authorization or routing input, and it is chosen by whoever opened the
/// trace. Two runs that share one are correlated, not related by authority.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schemars", schemars(with = "String"))]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TraceId([u8; 16]);

impl TraceId {
    /// Wraps sixteen identifier bytes.
    ///
    /// # Errors
    ///
    /// Returns [`TraceIdError::Zero`] when every byte is zero, which W3C defines as invalid.
    pub const fn new(bytes: [u8; 16]) -> Result<Self, TraceIdError> {
        if u128::from_be_bytes(bytes) == 0 {
            return Err(TraceIdError::Zero);
        }
        Ok(Self(bytes))
    }

    /// The identifier's bytes, in wire order.
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
        // Uppercase hexadecimal is rejected rather than folded: W3C specifies lowercase, and
        // accepting both would let one trace serialize two ways and read as two traces.
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

/// Failures raised while reading a trace identifier.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TraceIdError {
    /// The value was not exactly thirty-two lowercase hexadecimal digits.
    #[error("trace identifier must be 32 lowercase hexadecimal digits")]
    Malformed,
    /// Every byte was zero.
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
            // Uppercase is the same trace written a second way, so it is not a trace.
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

    /// Every identifier derived from a trace is `<trace>-<counter>`, which has to keep validating
    /// as an invocation identifier: thirty-two hexadecimal digits are lowercase and start with a
    /// legal edge character, so the derived name never depends on which trace was drawn.
    #[test]
    fn a_trace_identifier_is_a_legal_identifier_component() {
        let trace = TraceId::new([0xff; 16]).expect("non-zero identifier");
        let derived = format!("{trace}-{}", u32::MAX);
        assert!(derived.parse::<crate::InvocationId>().is_ok(), "{derived}");
    }
}
