//! Secret-carrying wrapper that cannot be rendered in the clear.
//!
//! [`Redacted`] exists because the alternative — remembering, at every log, span, and serializer
//! site, that one particular `String` is a credential — fails the first time someone adds a new
//! site. Wrapping the value moves the guarantee into the type: there is no `Debug`, `Display`, or
//! `Serialize` path that produces the secret, and reading it back requires the deliberately
//! conspicuous [`Redacted::expose`].
//!
//! # Length is deliberately preserved
//!
//! The marker is padded to the character length of the value it replaces, so a redacted field
//! keeps the shape of the record it sits in. That is an explicit operator choice and it does leak
//! one fact: how long the secret was. Token length can narrow down an issuer or credential class,
//! so this is a readability-for-metadata trade rather than a free win.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Renders the redaction marker for a value of `length` characters.
///
/// The marker is exactly `length` characters wide. Below the width of `[REDACTED]` there is no
/// room for the word, so the marker degrades to asterisks rather than truncating into something
/// that reads like a different token.
#[must_use]
pub fn redaction_marker(length: usize) -> String {
    const WORD: &str = "REDACTED";
    const MINIMUM: usize = WORD.len() + 2;

    if length < MINIMUM {
        return "*".repeat(length);
    }
    let padding = length - MINIMUM;
    let left = padding.div_ceil(2);
    let right = padding / 2;
    format!("[{}{WORD}{}]", " ".repeat(left), " ".repeat(right))
}

/// A value that must never reach a log, span, trace, or serialized record in the clear.
///
/// `Debug`, `Display`, and `Serialize` all render [`redaction_marker`] instead of the value. The
/// secret leaves only through [`Redacted::expose`] or [`Redacted::into_inner`].
#[derive(Clone, Default, Eq, Hash, PartialEq)]
pub struct Redacted<T = String>(T);

impl<T> Redacted<T> {
    /// Wraps a secret.
    pub const fn new(secret: T) -> Self {
        Self(secret)
    }

    /// Borrows the secret in the clear.
    ///
    /// Named to be conspicuous at call sites and in review: every use is a place where a
    /// credential leaves its wrapper, and there should be few of them.
    pub const fn expose(&self) -> &T {
        &self.0
    }

    /// Consumes the wrapper and returns the secret in the clear.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: AsRef<str>> Redacted<T> {
    /// Returns the marker this value renders as.
    #[must_use]
    pub fn marker(&self) -> String {
        redaction_marker(self.0.as_ref().chars().count())
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

/// Serializes a secret in the clear, for the few records that must persist it.
///
/// A credential file has to round-trip the real value, but that must be opt-in per field rather
/// than the default — otherwise the first struct someone serializes into a log or span leaks. Use
/// with `#[serde(serialize_with = "dekopon_core::serialize_exposed")]`, and only where the
/// destination is owner-only storage.
///
/// # Errors
///
/// Propagates whatever the underlying serializer returns.
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

    /// The marker must be exactly as wide as what it replaced, including the example the design
    /// was specified with.
    #[test]
    fn marker_matches_the_length_it_replaces() {
        assert_eq!(redaction_marker(13), "[  REDACTED ]");
        assert_eq!(redaction_marker(10), "[REDACTED]");
        assert_eq!(redaction_marker(20), "[     REDACTED     ]");
        for length in 0..64 {
            assert_eq!(
                redaction_marker(length).chars().count(),
                length,
                "marker for {length} is the wrong width"
            );
        }
    }

    /// Below `[REDACTED]` the word cannot fit, and a truncated word would read as a different
    /// token rather than as a redaction.
    #[test]
    fn short_values_degrade_to_asterisks() {
        assert_eq!(redaction_marker(0), "");
        assert_eq!(redaction_marker(9), "*********");
        assert!(!redaction_marker(9).contains("REDACT"));
    }

    /// Every rendering path must be a marker. A single one of these regressing is the whole bug
    /// this type exists to prevent.
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

        // The value itself is intact; only its renderings are replaced.
        assert_eq!(secret.expose(), "sk-live-abcdef0123456789");
        assert_eq!(secret.marker().chars().count(), 24);
    }

    /// Round-tripping must not quietly turn a secret into its own marker.
    #[test]
    fn deserialize_wraps_without_rendering() {
        let secret: Redacted<String> =
            serde_json::from_str("\"sk-live-abcdef0123456789\"").expect("redacted deserializes");
        assert_eq!(secret.expose(), "sk-live-abcdef0123456789");
    }

    /// Multi-byte secrets are measured in characters, so the marker stays the same visual width
    /// rather than the same byte count.
    #[test]
    fn length_is_measured_in_characters() {
        let secret = Redacted::new("señor-señor-señor".to_owned());
        assert_eq!(secret.expose().len(), 20);
        assert_eq!(secret.marker().chars().count(), 17);
    }
}
