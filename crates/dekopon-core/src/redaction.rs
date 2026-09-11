//! Secret-carrying wrapper that cannot be rendered in the clear.
//!
//! [`Redacted`] exists because the alternative — remembering, at every log, span, and serializer
//! site, that one particular `String` is a credential — fails the first time someone adds a new
//! site. Wrapping the value moves the guarantee into the type: there is no `Debug`, `Display`, or
//! `Serialize` path that produces the secret, and reading it back requires the deliberately
//! conspicuous [`Redacted::expose`].
//!
//! # The marker is constant
//!
//! Every redacted value renders as `[REDACTED]`, whatever it replaced. A marker padded to the
//! value's width leaked one fact for free — how long the secret was, which narrows down an issuer
//! or a credential class — and bought only that a record kept its column alignment.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The marker every redacted value renders as.
pub const REDACTION_MARKER: &str = "[REDACTED]";

/// Renders the redaction marker.
///
/// `_length` is ignored: the marker is [`REDACTION_MARKER`] regardless of what it replaces. The
/// parameter is kept because `dekopon-console` calls this function on a pinned `dekopon-core`, and
/// dropping it would break that build for a signature nobody needs.
#[must_use]
pub fn redaction_marker(_length: usize) -> String {
    REDACTION_MARKER.to_owned()
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
    /// Returns the marker this value renders as, which is [`REDACTION_MARKER`] for every value.
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

    /// The marker carries no trace of what it replaced, so no length reaches a record.
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
        assert_eq!(secret.marker(), "[REDACTED]");
    }

    /// Round-tripping must not quietly turn a secret into its own marker.
    #[test]
    fn deserialize_wraps_without_rendering() {
        let secret: Redacted<String> =
            serde_json::from_str("\"sk-live-abcdef0123456789\"").expect("redacted deserializes");
        assert_eq!(secret.expose(), "sk-live-abcdef0123456789");
    }

    /// A multi-byte secret renders the same marker as any other, so neither its byte count nor
    /// its character count reaches the record.
    #[test]
    fn multi_byte_secrets_render_the_same_marker() {
        let secret = Redacted::new("señor-señor-señor".to_owned());
        assert_eq!(secret.expose().len(), 20);
        assert_eq!(secret.marker(), "[REDACTED]");
    }
}
