//! The marker is always the same regardless of secret length, since a width-padded marker would
//! leak the secret's length and narrow down its type.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const REDACTION_MARKER: &str = "[REDACTED]";

/// The length parameter is unused but kept because dekopon-console, pinned to this crate, still
/// calls this signature; removing it would break that build.
#[must_use]
pub fn redaction_marker(_length: usize) -> String {
    REDACTION_MARKER.to_owned()
}

#[derive(Clone, Default, Eq, Hash, PartialEq)]
pub struct Redacted<T = String>(T);

impl<T> Redacted<T> {
    pub const fn new(secret: T) -> Self {
        Self(secret)
    }

    /// Named to be conspicuous in code and in review, since every call site is a place a credential
    /// leaves its wrapper.
    pub const fn expose(&self) -> &T {
        &self.0
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: AsRef<str>> Redacted<T> {
    #[must_use]
    pub fn marker(&self) -> String {
        REDACTION_MARKER.to_owned()
    }
}

impl<T: AsRef<str>> fmt::Debug for Redacted<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.marker())
    }
}

impl<T: AsRef<str>> fmt::Display for Redacted<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.marker())
    }
}

impl<T: AsRef<str>> Serialize for Redacted<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.marker())
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Redacted<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self)
    }
}

/// Opt-in per field, and only where the destination is owner-only storage; used elsewhere it
/// defeats the whole redaction guarantee.
pub fn serialize_exposed<S, T>(secret: &Redacted<T>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Serialize,
{
    secret.0.serialize(serializer)
}

impl<T> From<T> for Redacted<T> {
    fn from(secret: T) -> Self {
        Self(secret)
    }
}

#[cfg(test)]
mod tests {
    use super::{Redacted, redaction_marker};

    #[test]
    fn the_marker_is_constant() {
        for length in [0, 1, 9, 10, 13, 20, 64, 4096] {
            assert_eq!(
                redaction_marker(length),
                "[REDACTED]",
                "marker for {length} is not the constant"
            );
        }
    }

    #[test]
    fn no_rendering_path_reveals_the_secret() {
        let secret = Redacted::new("sk-live-abcdef0123456789".to_owned());

        assert!(!format!("{secret}").contains("sk-live"));
        assert!(!format!("{secret:?}").contains("sk-live"));
        assert!(
            !serde_json::to_string(&secret)
                .expect("redacted serializes")
                .contains("sk-live")
        );
        assert!(
            !serde_json::to_string(&vec![&secret, &secret])
                .expect("nested redacted serializes")
                .contains("sk-live")
        );

        assert_eq!(secret.expose(), "sk-live-abcdef0123456789");
        assert_eq!(secret.marker(), "[REDACTED]");
    }

    #[test]
    fn deserialize_wraps_without_rendering() {
        let secret: Redacted<String> =
            serde_json::from_str("\"sk-live-abcdef0123456789\"").expect("redacted deserializes");
        assert_eq!(secret.expose(), "sk-live-abcdef0123456789");
    }

    #[test]
    fn multi_byte_secrets_render_the_same_marker() {
        let secret = Redacted::new("señor-señor-señor".to_owned());
        assert_eq!(secret.expose().len(), 20);
        assert_eq!(secret.marker(), "[REDACTED]");
    }
}
