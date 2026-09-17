//! The size bound on one span attribute value.
//!
//! A run's arguments, piped values, and outputs ride its trace, so the bound applies to each value
//! rather than to the number of spans. The shell and the broker host cut through this one function,
//! so the two processes cannot disagree about where a value ends.

use std::{
    borrow::Cow,
    fmt::{self, Write as _},
};

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
    bounded_text(value, MAX_ATTRIBUTE_BYTES)
}

/// One value rendered under [`MAX_ATTRIBUTE_BYTES`], beside the byte length of the whole.
///
/// Returned by [`bounded_display`].
#[derive(Debug)]
pub struct BoundedDisplay {
    text: String,
    bytes: usize,
}

impl BoundedDisplay {
    /// The rendering, cut exactly where [`bounded_attribute`] cuts and marked the same way.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The byte length of the whole rendering, whether or not it was cut.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Renders `value` the way [`bounded_attribute`] would cut it, without ever holding more than the
/// bound.
///
/// [`bounded_attribute`] needs the value already in a `&str`, so a caller with a `Display` value
/// pays a full copy of it before anything is cut — and a provider proposal carries whatever the
/// model sent, which today includes base64 image bytes. This renders straight into a sink that
/// stops copying at the bound and keeps counting past it, so recording a multi-megabyte proposal
/// costs the bound rather than the proposal.
#[must_use]
pub fn bounded_display(value: &impl fmt::Display) -> BoundedDisplay {
    let mut sink = BoundedSink {
        text: String::new(),
        bytes: 0,
        cut: false,
    };
    // The sink itself never fails, so an error is the value's own `Display` giving up part way
    // through. What was written is then a prefix of the value, which is what the marker announces;
    // `bytes` counts that prefix, because no more of the value exists to count.
    if write!(sink, "{value}").is_err() {
        sink.cut = true;
    }
    let BoundedSink {
        mut text,
        bytes,
        cut,
    } = sink;
    if cut {
        text.push_str(TRUNCATION_MARKER);
    }
    BoundedDisplay { text, bytes }
}

/// Keeps the first [`MAX_ATTRIBUTE_BYTES`] of everything written to it and counts the rest.
///
/// Once one fragment crosses the bound the sink stops appending for good, so the kept prefix is the
/// same one [`bounded_attribute`] would take from the joined text rather than a later fragment's
/// short characters slipping in behind a dropped long one.
struct BoundedSink {
    text: String,
    bytes: usize,
    cut: bool,
}

impl fmt::Write for BoundedSink {
    fn write_str(&mut self, fragment: &str) -> fmt::Result {
        self.bytes = self.bytes.saturating_add(fragment.len());
        if self.cut {
            return Ok(());
        }
        let remaining = MAX_ATTRIBUTE_BYTES - self.text.len();
        if fragment.len() <= remaining {
            self.text.push_str(fragment);
        } else {
            self.text
                .push_str(&fragment[..fragment.floor_char_boundary(remaining)]);
            self.cut = true;
        }
        Ok(())
    }
}

/// Applies the same cut as [`bounded_attribute`] at a caller-chosen bound.
///
/// A provider-reported failure code and message are carried on the wire and in the audit record
/// rather than only on a span, so they take tighter bounds than a span attribute. The truncation
/// rule itself stays in one place so the two cannot disagree about where a value ends.
pub(crate) fn bounded_text(value: &str, maximum: usize) -> Cow<'_, str> {
    if value.len() <= maximum {
        return Cow::Borrowed(value);
    }
    let prefix = &value[..value.floor_char_boundary(maximum)];
    Cow::Owned(format!("{prefix}{TRUNCATION_MARKER}"))
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, fmt, fmt::Write as _};

    use super::{
        BoundedSink, MAX_ATTRIBUTE_BYTES, TRUNCATION_MARKER, bounded_attribute, bounded_display,
    };

    /// Writes `fragment` `times` over without ever holding the whole rendering itself.
    struct Repeated<'a> {
        fragment: &'a str,
        times: usize,
    }

    impl fmt::Display for Repeated<'_> {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            for _ in 0..self.times {
                formatter.write_str(self.fragment)?;
            }
            Ok(())
        }
    }

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

    /// Rendering through the sink and cutting an already-built string must not disagree, whatever
    /// the fragment lengths are, including a fragment whose character straddles the bound.
    #[test]
    fn a_rendered_value_is_cut_where_the_same_text_would_be() {
        for value in [
            String::new(),
            "x".repeat(MAX_ATTRIBUTE_BYTES),
            "x".repeat(MAX_ATTRIBUTE_BYTES + 1),
            format!("{}\u{1f980}", "x".repeat(MAX_ATTRIBUTE_BYTES - 3)),
            format!("{}\u{1f980}x", "x".repeat(MAX_ATTRIBUTE_BYTES - 3)),
        ] {
            let expected = bounded_attribute(&value);
            let rendered = bounded_display(&Repeated {
                fragment: &value,
                times: 1,
            });
            assert_eq!(rendered.bytes(), value.len());
            assert_eq!(rendered.text(), expected);

            for chunk in [1, 7, MAX_ATTRIBUTE_BYTES] {
                let mut sink = BoundedSink {
                    text: String::new(),
                    bytes: 0,
                    cut: false,
                };
                let mut rest = value.as_str();
                while !rest.is_empty() {
                    // A fragment is always whole characters, so a character longer than `chunk`
                    // goes across on its own rather than stalling the loop.
                    let mut split = rest.floor_char_boundary(chunk.min(rest.len()));
                    if split == 0 {
                        split = rest
                            .char_indices()
                            .nth(1)
                            .map_or(rest.len(), |(index, _)| index);
                    }
                    let (fragment, remainder) = rest.split_at(split);
                    sink.write_str(fragment).expect("the sink never fails");
                    rest = remainder;
                }

                assert_eq!(sink.bytes, value.len(), "chunk {chunk}");
                assert_eq!(
                    sink.text,
                    expected
                        .strip_suffix(TRUNCATION_MARKER)
                        .unwrap_or(&expected),
                    "chunk {chunk}"
                );
            }
        }
    }

    /// The point of rendering rather than cutting a built string: a value far past the bound is
    /// counted in full while the buffer holding it never grows past the bound.
    #[test]
    fn a_value_far_past_the_bound_never_buffers_more_than_the_bound() {
        let fragment = "x".repeat(64 * 1024);
        let mut sink = BoundedSink {
            text: String::new(),
            bytes: 0,
            cut: false,
        };
        for _ in 0..256 {
            sink.write_str(&fragment).expect("the sink never fails");
        }

        assert_eq!(sink.bytes, 16 * 1024 * 1024);
        assert!(
            sink.text.capacity() <= MAX_ATTRIBUTE_BYTES,
            "the kept buffer grew to {} for a 16 MiB value",
            sink.text.capacity()
        );

        let rendered = bounded_display(&Repeated {
            fragment: &fragment,
            times: 256,
        });

        assert_eq!(rendered.bytes(), 16 * 1024 * 1024);
        assert_eq!(
            rendered.text(),
            format!("{}{TRUNCATION_MARKER}", "x".repeat(MAX_ATTRIBUTE_BYTES))
        );
    }
}
