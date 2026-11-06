use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_HISTORY_KEY_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HistoryKey(String);

#[derive(Debug, Error, Eq, PartialEq)]
pub enum HistoryKeyError {
    #[error("history key must be 1 to 256 bytes")]
    Length,
    #[error("history key may contain only ASCII letters, digits, '.', '_', '-' and '/'")]
    Character,
}

impl HistoryKey {
    pub fn new(value: String) -> Result<Self, HistoryKeyError> {
        if value.is_empty() || value.len() > MAX_HISTORY_KEY_BYTES {
            return Err(HistoryKeyError::Length);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
        {
            return Err(HistoryKeyError::Character);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for HistoryKey {
    type Error = HistoryKeyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<HistoryKey> for String {
    fn from(key: HistoryKey) -> Self {
        key.0
    }
}

#[cfg(test)]
mod tests {
    use super::{HistoryKey, HistoryKeyError};

    #[test]
    fn a_history_key_refuses_whitespace_and_oversized_values() {
        assert!(HistoryKey::new("agent/0a1b".to_owned()).is_ok());
        assert_eq!(
            HistoryKey::new("agent 1".to_owned()),
            Err(HistoryKeyError::Character)
        );
        assert_eq!(
            HistoryKey::new("a".repeat(257)),
            Err(HistoryKeyError::Length)
        );
        assert!(serde_json::from_str::<HistoryKey>("\"\"").is_err());
    }
}
