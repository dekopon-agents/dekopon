use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use schemars::generate::SchemaGenerator;
use schemars::{JsonSchema, Schema, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A string whose UTF-8 representation fits within `MAX` bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Bounded<const MAX: usize>(String);

/// A value exceeds its UTF-8 byte bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TooLong;

impl fmt::Display for TooLong {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("text exceeds the UTF-8 byte limit")
    }
}

impl std::error::Error for TooLong {}

impl<const MAX: usize> Bounded<MAX> {
    /// Refuses text longer than `MAX` UTF-8 bytes.
    pub fn new(text: impl Into<String>) -> Result<Self, TooLong> {
        let text = text.into();
        if text.len() > MAX {
            return Err(TooLong);
        }
        Ok(Self(text))
    }

    /// The bounded text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<const MAX: usize> FromStr for Bounded<MAX> {
    type Err = TooLong;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::new(text)
    }
}

impl<'de, const MAX: usize> Deserialize<'de> for Bounded<MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl<const MAX: usize> Serialize for Bounded<MAX> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<const MAX: usize> JsonSchema for Bounded<MAX> {
    fn schema_name() -> Cow<'static, str> {
        format!("Bounded_{MAX}").into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "maxLength": MAX})
    }
}

/// A string cut at a UTF-8 character boundary when it exceeds `MAX` bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Truncated<const MAX: usize> {
    text: String,
    was_cut: bool,
}

impl<const MAX: usize> Truncated<MAX> {
    /// Cuts text to at most `MAX` UTF-8 bytes without splitting a character.
    #[must_use]
    pub fn new(text: impl AsRef<str>) -> Self {
        let text = text.as_ref();
        let mut end = text.len().min(MAX);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            text: text[..end].to_owned(),
            was_cut: end != text.len(),
        }
    }

    /// The text after any cut.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether construction discarded any bytes.
    #[must_use]
    pub fn was_cut(&self) -> bool {
        self.was_cut
    }
}

impl<const MAX: usize> FromStr for Truncated<MAX> {
    type Err = std::convert::Infallible;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(text))
    }
}

impl<'de, const MAX: usize> Deserialize<'de> for Truncated<MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::new(String::deserialize(deserializer)?))
    }
}

impl<const MAX: usize> Serialize for Truncated<MAX> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

impl<const MAX: usize> JsonSchema for Truncated<MAX> {
    fn schema_name() -> Cow<'static, str> {
        format!("Truncated_{MAX}").into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string"})
    }
}
