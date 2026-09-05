//! Portable session coordinates and configured model intent. None of these values grants authority.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A coordinate or configured model ID was outside the bounded portable grammar.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("control identifier must match [a-z0-9][a-z0-9._-]{{0,63}}")]
pub struct ControlIdentifierError;

macro_rules! identifier {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        #[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
        pub struct $name(String);

        impl $name {
            /// Validates a host coordinate or configured name; never derives it from model text.
            pub fn new(value: impl Into<String>) -> Result<Self, ControlIdentifierError> {
                let value = value.into();
                let valid = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
                if value.is_empty()
                    || value.len() > 64
                    || !value.bytes().next().is_some_and(valid)
                    || !value
                        .bytes()
                        .all(|b| valid(b) || matches!(b, b'.' | b'_' | b'-'))
                {
                    return Err(ControlIdentifierError);
                }
                Ok(Self(value))
            }

            /// Exact validated bytes.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl FromStr for $name {
            type Err = ControlIdentifierError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

identifier!(
    ConfiguredModelId,
    "Owner-configured model alias, never an endpoint or provider model name."
);
identifier!(JobId, "Opaque logical-job coordinate; resume preserves it.");
identifier!(SessionId, "Opaque host-bound session coordinate.");
identifier!(
    RequestId,
    "Opaque authenticated ingress request coordinate."
);
identifier!(
    GenerationId,
    "Opaque host generation fence, not a model-selected identifier."
);
identifier!(
    SurfaceEpoch,
    "Opaque broker-startup epoch; never a permission or a model-visible value."
);

/// Explicit inference effort. Unsupported settings must be refused, never silently dropped.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum Effort {
    #[default]
    ProviderDefault,
    Low,
    Medium,
    High,
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ProviderDefault => "providerDefault",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        })
    }
}

/// Complete configured selection; the broker treats it as intent, not verified runtime state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ModelSelection {
    pub model: ConfiguredModelId,
    pub effort: Effort,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_names_have_the_exact_bounded_grammar_and_effort_is_strict() {
        for valid in ["gpt-5.6-sol", "a.._-", &"a".repeat(64)] {
            assert!(valid.parse::<ConfiguredModelId>().is_ok(), "{valid}");
        }
        for invalid in ["", "Upper", "_model", "https://model", &"a".repeat(65)] {
            assert!(invalid.parse::<ConfiguredModelId>().is_err());
            assert!(serde_json::from_value::<JobId>(serde_json::json!(invalid)).is_err());
        }
        for effort in [
            Effort::ProviderDefault,
            Effort::Low,
            Effort::Medium,
            Effort::High,
        ] {
            assert_eq!(serde_json::to_value(effort).unwrap(), effort.to_string());
        }
        assert!(serde_json::from_str::<Effort>("\"xhigh\"").is_err());
    }
}
