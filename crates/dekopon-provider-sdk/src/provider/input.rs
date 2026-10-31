use std::fmt;

use serde::de::DeserializeOwned;
use serde_path_to_error::Segment;
use serde_value::{DeserializerError, Value, ValueDeserializer};

use super::{Code, Failure, SdkFailure};

pub(super) struct InvalidInput {
    path: String,
    kind: Kind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    MissingField,
    UnknownField,
    WrongType,
    InvalidValue,
    InvalidLength,
    DuplicateField,
    Unlocated,
}

impl fmt::Display for InvalidInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            Kind::MissingField => "missing field",
            Kind::UnknownField => "unknown field",
            Kind::WrongType => "wrong type",
            Kind::InvalidValue => "invalid value",
            Kind::InvalidLength => "wrong length",
            Kind::DuplicateField => "duplicate field",
            Kind::Unlocated => return SdkFailure::InvalidInput.fmt(formatter),
        };
        let path = if self.path.is_empty() {
            "input"
        } else {
            &self.path
        };
        write!(formatter, "invalid input: {path}: {kind}")
    }
}

impl Failure for InvalidInput {
    fn code(&self) -> Code {
        Code::INVALID_INPUT
    }
}

fn field(path: &mut String, name: &str) {
    if !path.is_empty() {
        path.push('.');
    }
    path.push_str(name);
}

pub(super) fn refusal<I: DeserializeOwned>(input: &str) -> InvalidInput {
    let fallback = InvalidInput {
        path: String::new(),
        kind: Kind::Unlocated,
    };
    let Ok(value) = serde_json::from_str::<Value>(input) else {
        return fallback;
    };
    let Err(error) = serde_path_to_error::deserialize::<_, I>(
        ValueDeserializer::<DeserializerError>::new(value),
    ) else {
        return fallback;
    };
    let mut path = String::new();
    for segment in error.path() {
        match segment {
            Segment::Map { key } => field(&mut path, key),
            Segment::Seq { index } => {
                path.push('[');
                path.push_str(&index.to_string());
                path.push(']');
            }
            Segment::Enum { .. } | Segment::Unknown => {}
        }
    }
    let kind = match error.inner() {
        DeserializerError::MissingField(name) => {
            field(&mut path, name);
            Kind::MissingField
        }
        DeserializerError::UnknownField(name, _) => {
            if !matches!(error.path().iter().next_back(), Some(Segment::Map { key }) if key == name)
            {
                field(&mut path, name);
            }
            Kind::UnknownField
        }
        DeserializerError::InvalidType(..) => Kind::WrongType,
        DeserializerError::InvalidValue(..)
        | DeserializerError::UnknownVariant(..)
        | DeserializerError::Custom(_) => Kind::InvalidValue,
        DeserializerError::InvalidLength(..) => Kind::InvalidLength,
        DeserializerError::DuplicateField(name) => {
            field(&mut path, name);
            Kind::DuplicateField
        }
    };
    InvalidInput { path, kind }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Deserializer};

    #[test]
    fn a_diagnostic_refusal_keeps_the_invalid_input_code() {
        assert_eq!(refusal::<String>("null").code(), Code::INVALID_INPUT);
    }

    #[test]
    fn enum_and_custom_errors_never_echo_input_values() {
        #[derive(Deserialize)]
        enum Choice {
            Known,
        }

        struct Custom;

        impl<'de> Deserialize<'de> for Custom {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                Err(serde::de::Error::custom(text))
            }
        }

        for (failure, kind, message) in [
            (
                refusal::<Choice>(r#""SENTINEL-INPUT-VALUE""#),
                Kind::InvalidValue,
                "invalid input: input: invalid value",
            ),
            (
                refusal::<Choice>(r#"{"SENTINEL-INPUT-VALUE":null}"#),
                Kind::InvalidValue,
                "invalid input: input: invalid value",
            ),
            (
                refusal::<Choice>(r#"{"Known":"SENTINEL-INPUT-VALUE"}"#),
                Kind::WrongType,
                "invalid input: input: wrong type",
            ),
            (
                refusal::<Custom>(r#""SENTINEL-INPUT-VALUE""#),
                Kind::InvalidValue,
                "invalid input: input: invalid value",
            ),
        ] {
            assert_eq!(failure.kind, kind);
            assert_eq!(failure.to_string(), message);
        }
    }
}
