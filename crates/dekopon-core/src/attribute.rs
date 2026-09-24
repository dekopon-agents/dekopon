use std::{
    borrow::Cow,
    fmt::{self, Write as _},
};

pub const MAX_ATTRIBUTE_BYTES: usize = 4096;

const TRUNCATION_MARKER: &str = "\u{2026}[truncated]";

#[must_use]
pub fn bounded_attribute(value: &str) -> Cow<'_, str> {
    bounded_text(value, MAX_ATTRIBUTE_BYTES)
}

#[derive(Debug)]
pub struct BoundedDisplay {
    text: String,
    bytes: usize,
}

impl BoundedDisplay {
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Renders straight into a bounded sink instead of formatting to a String first, so recording a
/// multi-megabyte proposal costs the bound, not the proposal.
#[must_use]
pub fn bounded_display(value: &impl fmt::Display) -> BoundedDisplay {
    let mut sink = BoundedSink {
        text: String::new(),
        bytes: 0,
        cut: false,
    };
    // This assumes the sink itself never errors; if it ever could, a real sink failure would be
    // miscounted as a truncated value.
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
