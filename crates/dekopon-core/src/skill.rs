//! The identifier grammar of an agent skill.
//!
//! A skill is a directory carrying a `SKILL.md` whose front matter names it. The name grammar is
//! the one the open Agent Skills format fixes rather than Dekopon's own resource grammar: lowercase
//! ASCII letters, digits, and single hyphens, at most 64 bytes, and equal to the directory's own
//! name. It is narrower than [`crate::AgentId`] on purpose — a skill authored for another client
//! has to load here unchanged, and one authored here has to load there — so the two grammars are
//! two types rather than one type with a mode.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use thiserror::Error;

/// Maximum bytes in a skill name, fixed by the Agent Skills format.
pub const MAX_SKILL_NAME_LENGTH: usize = 64;

/// The reason a skill name could not be parsed.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SkillIdError {
    /// The name was empty.
    #[error("skill name must not be empty")]
    Empty,
    /// The name exceeded the format's limit.
    #[error("skill name is {length} bytes; the maximum is {maximum}")]
    TooLong {
        /// Actual byte length.
        length: usize,
        /// Maximum byte length.
        maximum: usize,
    },
    /// A character outside `[a-z0-9-]` was present.
    #[error(
        "skill name contains invalid character {character:?} at byte {index}; use lowercase ASCII letters, digits, or '-'"
    )]
    InvalidCharacter {
        /// Byte offset in the submitted value.
        index: usize,
        /// Invalid character.
        character: char,
    },
    /// The name started or ended with a hyphen.
    #[error("skill name must start and end with a lowercase ASCII letter or digit")]
    InvalidEdge,
    /// Two hyphens appeared next to one another.
    #[error("skill name contains consecutive hyphens at byte {index}")]
    ConsecutiveHyphens {
        /// Byte offset of the second hyphen.
        index: usize,
    },
}

/// A validated skill name, equal to the name of the directory the skill lives in.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SkillId(String);

impl SkillId {
    /// Returns the validated name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_skill_name(value: &str) -> Result<(), SkillIdError> {
    if value.is_empty() {
        return Err(SkillIdError::Empty);
    }
    if value.len() > MAX_SKILL_NAME_LENGTH {
        return Err(SkillIdError::TooLong {
            length: value.len(),
            maximum: MAX_SKILL_NAME_LENGTH,
        });
    }
    let mut previous_hyphen = false;
    for (index, character) in value.char_indices() {
        let allowed = character.is_ascii_lowercase() || character.is_ascii_digit();
        if !allowed && character != '-' {
            return Err(SkillIdError::InvalidCharacter { index, character });
        }
        if character == '-' && previous_hyphen {
            return Err(SkillIdError::ConsecutiveHyphens { index });
        }
        previous_hyphen = character == '-';
    }
    if value.starts_with('-') || value.ends_with('-') {
        return Err(SkillIdError::InvalidEdge);
    }
    Ok(())
}

impl fmt::Display for SkillId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for SkillId {
    type Err = SkillIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_skill_name(value)?;
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for SkillId {
    type Error = SkillIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_skill_name(&value)?;
        Ok(Self(value))
    }
}

impl From<SkillId> for String {
    fn from(value: SkillId) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for SkillId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_SKILL_NAME_LENGTH, SkillId, SkillIdError};

    #[test]
    fn accepts_the_agent_skills_grammar() {
        for name in ["pdf", "pull-request-review", "a1", "x-2-y", "abc123"] {
            assert!(name.parse::<SkillId>().is_ok(), "{name}");
        }
        let longest = "a".repeat(MAX_SKILL_NAME_LENGTH);
        assert_eq!(
            longest
                .parse::<SkillId>()
                .expect("64 bytes is the limit")
                .as_str(),
            longest
        );
    }

    /// Every refusal names what was wrong, because a skill author reads it off a load failure.
    #[test]
    fn rejects_names_outside_the_grammar_by_reason() {
        assert_eq!("".parse::<SkillId>(), Err(SkillIdError::Empty));
        assert!(matches!(
            "a".repeat(MAX_SKILL_NAME_LENGTH + 1).parse::<SkillId>(),
            Err(SkillIdError::TooLong { length: 65, .. })
        ));
        assert_eq!(
            "Pdf".parse::<SkillId>(),
            Err(SkillIdError::InvalidCharacter {
                index: 0,
                character: 'P'
            })
        );
        assert_eq!(
            "pdf_tools".parse::<SkillId>(),
            Err(SkillIdError::InvalidCharacter {
                index: 3,
                character: '_'
            })
        );
        assert_eq!(
            "a.b".parse::<SkillId>(),
            Err(SkillIdError::InvalidCharacter {
                index: 1,
                character: '.'
            })
        );
        assert_eq!("-pdf".parse::<SkillId>(), Err(SkillIdError::InvalidEdge));
        assert_eq!("pdf-".parse::<SkillId>(), Err(SkillIdError::InvalidEdge));
        assert_eq!(
            "pdf--tools".parse::<SkillId>(),
            Err(SkillIdError::ConsecutiveHyphens { index: 4 })
        );
    }

    #[test]
    fn deserializes_only_valid_names() {
        let valid: SkillId = serde_json::from_str("\"pdf-tools\"").expect("valid name decodes");
        assert_eq!(valid.as_str(), "pdf-tools");
        assert!(serde_json::from_str::<SkillId>("\"PDF\"").is_err());
        assert_eq!(
            serde_json::to_string(&valid).expect("name serializes"),
            "\"pdf-tools\""
        );
    }
}
