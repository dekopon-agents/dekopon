//! Text-shaped builtins.
//!
//! Each of these accepts either a raw string or a JSON array of lines and returns a line list
//! re-coerced by [`crate::value::from_lines`]. That auto-coercion is what lets
//! `curl ... | grep foo | wc -l` read and behave like real bash while `jq` stays JSON-native.
//!
//! Patterns here are **literal strings with optional `^` and `$` anchors**, not regular
//! expressions. That is a deliberate Phase 1 boundary: a regex engine is a large dependency and a
//! large attack surface, and every unsupported flag is rejected by name so a script can never
//! believe a regex was honored when it was matched literally.

pub(crate) mod cut;
pub(crate) mod grep;
pub(crate) mod sed;
pub(crate) mod sort;
pub(crate) mod uniq;
pub(crate) mod wc;

pub(crate) use cut::Cut;
pub(crate) use grep::Grep;
pub(crate) use sed::Sed;
pub(crate) use sort::Sort;
pub(crate) use uniq::Uniq;
pub(crate) use wc::Wc;

/// A literal pattern with optional `^`/`$` anchors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Pattern {
    needle: String,
    anchored_start: bool,
    anchored_end: bool,
    ignore_case: bool,
}

impl Pattern {
    /// Compiles one pattern.
    pub(crate) fn new(pattern: &str, ignore_case: bool) -> Self {
        let mut needle = pattern;
        let anchored_start = needle.starts_with('^');
        if anchored_start {
            needle = &needle[1..];
        }
        let anchored_end = needle.len() > 1 && needle.ends_with('$');
        if anchored_end {
            needle = &needle[..needle.len() - 1];
        }
        Self {
            needle: if ignore_case {
                needle.to_lowercase()
            } else {
                needle.to_owned()
            },
            anchored_start,
            anchored_end,
            ignore_case,
        }
    }

    /// Reports whether one line matches.
    pub(crate) fn matches(&self, line: &str) -> bool {
        let candidate = if self.ignore_case {
            line.to_lowercase()
        } else {
            line.to_owned()
        };
        match (self.anchored_start, self.anchored_end) {
            (true, true) => candidate == self.needle,
            (true, false) => candidate.starts_with(&self.needle),
            (false, true) => candidate.ends_with(&self.needle),
            (false, false) => candidate.contains(&self.needle),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Pattern;

    #[test]
    fn unanchored_patterns_match_substrings() {
        let pattern = Pattern::new("ell", false);
        assert!(pattern.matches("hello"));
        assert!(!pattern.matches("world"));
    }

    #[test]
    fn anchors_constrain_both_ends() {
        assert!(Pattern::new("^he", false).matches("hello"));
        assert!(!Pattern::new("^he", false).matches("the hen"));
        assert!(Pattern::new("lo$", false).matches("hello"));
        assert!(!Pattern::new("lo$", false).matches("hello there"));
        assert!(Pattern::new("^hello$", false).matches("hello"));
        assert!(!Pattern::new("^hello$", false).matches("hello there"));
    }

    #[test]
    fn case_folding_is_opt_in() {
        assert!(!Pattern::new("HELLO", false).matches("hello"));
        assert!(Pattern::new("HELLO", true).matches("hello"));
    }

    #[test]
    fn a_lone_dollar_stays_literal() {
        // `$` alone is a plausible literal search term, so it is not treated as an empty anchor.
        assert!(Pattern::new("$", false).matches("cost: $5"));
    }
}
