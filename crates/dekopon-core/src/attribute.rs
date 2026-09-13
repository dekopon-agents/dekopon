//! The size bound on one span attribute value.
//!
//! A run's arguments, piped values, and outputs ride its trace, so the bound applies to each value
//! rather than to the number of spans. The shell and the broker host cut through this one function,
//! so the two processes cannot disagree about where a value ends.

use std::borrow::Cow;

/// Maximum bytes of one span attribute value [`bounded_attribute`] passes through uncut.
pub const MAX_ATTRIBUTE_BYTES: usize = 4096;

/// Appended to a value [`bounded_attribute`] cut.
const TRUNCATION_MARKER: &str = "\u{2026}[truncated]";

/// Returns `value` unchanged when it fits [`MAX_ATTRIBUTE_BYTES`], otherwise the longest prefix
/// ending on a character boundary within the bound followed by `…[truncated]`.
///
/// The cut never splits a character, so the result is always valid text, and the marker is added
/// past the bound rather than inside it. A caller records the full byte length beside the bounded
/// value, so a reader can tell a cut value from one that happened to end there.
#[must_use]
pub fn bounded_attribute(value: &str) -> Cow<'_, str> {
    if value.len() <= MAX_ATTRIBUTE_BYTES {
        return Cow::Borrowed(value);
    }
    let prefix = &value[..value.floor_char_boundary(MAX_ATTRIBUTE_BYTES)];
    Cow::Owned(format!("{prefix}{TRUNCATION_MARKER}"))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{MAX_ATTRIBUTE_BYTES, bounded_attribute};

    #[test]
    fn a_value_exactly_at_the_cap_is_returned_unchanged() {
        let value = "x".repeat(MAX_ATTRIBUTE_BYTES);

        let bounded = bounded_attribute(&value);

        assert!(
            matches!(bounded, Cow::Borrowed(_)),
            "a value that fits is not copied"
        );
        assert_eq!(bounded, value);
    }

    #[test]
    fn one_byte_over_the_cap_is_cut_to_the_cap_and_marked() {
        let value = "x".repeat(MAX_ATTRIBUTE_BYTES + 1);

        let bounded = bounded_attribute(&value);

        assert_eq!(
            bounded,
            format!("{}…[truncated]", "x".repeat(MAX_ATTRIBUTE_BYTES))
        );
    }

    /// A four-byte character starting three bytes before the cap ends one byte past it: cutting
    /// at the cap would split it, so the whole character goes.
    #[test]
    fn a_multibyte_character_straddling_the_cap_is_dropped_whole() {
        let value = format!("{}\u{1f980}", "x".repeat(MAX_ATTRIBUTE_BYTES - 3));
        assert_eq!(value.len(), MAX_ATTRIBUTE_BYTES + 1);
        assert!(!value.is_char_boundary(MAX_ATTRIBUTE_BYTES));

        let bounded = bounded_attribute(&value);

        assert_eq!(
            bounded,
            format!("{}…[truncated]", "x".repeat(MAX_ATTRIBUTE_BYTES - 3))
        );
    }
}
