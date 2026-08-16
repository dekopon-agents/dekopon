//! Dependency-light domain types for Dekopon.
//!
//! Identifiers are validated at construction and during deserialization. This prevents
//! malformed resource references from leaking into the rest of the workspace while
//! keeping transport, command-line, async-runtime, and policy concerns out of this crate.

#![forbid(unsafe_code)]

mod redaction;
mod span_payloads;

use std::{fmt, str::FromStr};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use thiserror::Error;

pub use redaction::{Redacted, redaction_marker, serialize_exposed};
pub use span_payloads::{set_span_payloads, span_payloads};

const MAX_IDENTIFIER_LENGTH: usize = 253;

/// The reason a Dekopon identifier could not be parsed.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum IdentifierError {
    /// The identifier was empty.
    #[error("{kind} identifier must not be empty")]
    Empty {
        /// Human-readable identifier kind.
        kind: &'static str,
    },
    /// The identifier exceeded the supported wire limit.
    #[error("{kind} identifier is {length} bytes; the maximum is {maximum}")]
    TooLong {
        /// Human-readable identifier kind.
        kind: &'static str,
        /// Actual byte length.
        length: usize,
        /// Maximum byte length.
        maximum: usize,
    },
    /// The first character was not an ASCII lowercase letter or digit.
    #[error(
        "{kind} identifier must start with a lowercase ASCII letter or digit, found {character:?}"
    )]
    InvalidStart {
        /// Human-readable identifier kind.
        kind: &'static str,
        /// Invalid character.
        character: char,
    },
    /// The last character was a separator.
    #[error(
        "{kind} identifier must end with a lowercase ASCII letter or digit, found {character:?}"
    )]
    InvalidEnd {
        /// Human-readable identifier kind.
        kind: &'static str,
        /// Invalid character.
        character: char,
    },
    /// A character outside the portable identifier alphabet was present.
    #[error(
        "{kind} identifier contains invalid character {character:?} at byte {index}; use lowercase ASCII letters, digits, '.', '-', or '_'"
    )]
    InvalidCharacter {
        /// Human-readable identifier kind.
        kind: &'static str,
        /// Byte offset in the submitted value.
        index: usize,
        /// Invalid character.
        character: char,
    },
    /// Two separator characters appeared next to one another.
    #[error("{kind} identifier contains adjacent separators at byte {index}")]
    AdjacentSeparators {
        /// Human-readable identifier kind.
        kind: &'static str,
        /// Byte offset of the second separator.
        index: usize,
    },
}

fn is_edge_character(character: char) -> bool {
    character.is_ascii_lowercase() || character.is_ascii_digit()
}

fn is_separator(character: char) -> bool {
    matches!(character, '.' | '-' | '_')
}

fn validate_identifier(value: &str, kind: &'static str) -> Result<(), IdentifierError> {
    if value.is_empty() {
        return Err(IdentifierError::Empty { kind });
    }
    if value.len() > MAX_IDENTIFIER_LENGTH {
        return Err(IdentifierError::TooLong {
            kind,
            length: value.len(),
            maximum: MAX_IDENTIFIER_LENGTH,
        });
    }

    let mut characters = value.char_indices();
    let (_, first) = characters.next().ok_or(IdentifierError::Empty { kind })?;
    if !is_edge_character(first) {
        return Err(IdentifierError::InvalidStart {
            kind,
            character: first,
        });
    }

    let mut previous_was_separator = false;
    for (index, character) in value.char_indices() {
        if !is_edge_character(character) && !is_separator(character) {
            return Err(IdentifierError::InvalidCharacter {
                kind,
                index,
                character,
            });
        }
        if is_separator(character) && previous_was_separator {
            return Err(IdentifierError::AdjacentSeparators { kind, index });
        }
        previous_was_separator = is_separator(character);
    }

    let last = value
        .chars()
        .next_back()
        .ok_or(IdentifierError::Empty { kind })?;
    if !is_edge_character(last) {
        return Err(IdentifierError::InvalidEnd {
            kind,
            character: last,
        });
    }

    Ok(())
}

macro_rules! identifier {
    ($name:ident, $label:literal, $docs:literal) => {
        #[doc = $docs]
        #[derive(Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Returns the validated identifier as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                validate_identifier(value, $label)?;
                Ok(Self(value.to_owned()))
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdentifierError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                validate_identifier(&value, $label)?;
                Ok(Self(value))
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdentifierError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                value.parse()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(D::Error::custom)
            }
        }
    };
}

identifier!(AgentId, "agent", "A validated agent resource identifier.");
identifier!(
    CapabilityId,
    "capability",
    "A validated capability resource identifier."
);
identifier!(
    ProviderId,
    "provider",
    "A validated capability-provider identifier."
);
identifier!(TaskId, "task", "A validated task identifier.");
identifier!(
    InvocationId,
    "invocation",
    "A validated capability invocation identifier."
);
identifier!(TraceId, "trace", "A validated end-to-end trace identifier.");
identifier!(
    PrincipalId,
    "principal",
    "A validated authenticated principal identifier."
);

/// The authenticated actor responsible for an operation.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum Actor {
    /// A human operator.
    Human {
        /// The operator's trusted principal identity.
        principal: PrincipalId,
    },
    /// A Dekopon agent. The envelope carrying this value must authenticate it.
    Agent {
        /// The agent identity.
        agent: AgentId,
    },
    /// A non-human service principal.
    Service {
        /// The service's trusted principal identity.
        principal: PrincipalId,
    },
}

/// Coarse risk classification used as policy input.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "PascalCase")]
pub enum RiskLevel {
    /// No expected external side effect and limited data exposure.
    Low,
    /// Meaningful data access or a reversible/local effect.
    Medium,
    /// An external write, sensitive data access, or difficult rollback.
    High,
    /// A potentially destructive or high-impact operation.
    Critical,
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

/// Operational phase reported for an agent.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "PascalCase")]
pub enum AgentStatus {
    /// Configuration intentionally prevents the agent from running.
    Disabled,
    /// The agent is valid but not yet ready.
    Pending,
    /// The agent is ready for orchestration.
    Ready,
    /// The agent cannot operate because of an error.
    Error,
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentId, IdentifierError, RiskLevel};

    #[test]
    fn accepts_portable_identifiers() {
        for value in ["reviewer", "github.pull-request.read", "agent_2"] {
            let parsed = value.parse::<AgentId>();
            assert!(parsed.is_ok(), "{value} should be valid: {parsed:?}");
        }
    }

    #[test]
    fn rejects_invalid_identifiers_with_context() {
        assert!(matches!(
            "Reviewer".parse::<AgentId>(),
            Err(IdentifierError::InvalidStart { .. })
        ));
        assert!(matches!(
            "github..read".parse::<AgentId>(),
            Err(IdentifierError::AdjacentSeparators { .. })
        ));
        assert!(matches!(
            "reviewer/one".parse::<AgentId>(),
            Err(IdentifierError::InvalidCharacter { index: 8, .. })
        ));
        assert!(matches!(
            "reviewer-".parse::<AgentId>(),
            Err(IdentifierError::InvalidEnd { .. })
        ));
    }

    #[test]
    fn deserialization_cannot_bypass_validation() {
        let error = serde_json::from_str::<AgentId>(r#""not valid""#)
            .expect_err("whitespace must be rejected");
        assert!(error.to_string().contains("invalid character"));
    }

    #[test]
    fn display_is_stable() {
        assert_eq!(RiskLevel::High.to_string(), "High");
    }
}
