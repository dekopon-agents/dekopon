//! Text-shaped builtins.
//!
//! Each of these accepts either a raw string or a JSON array of lines and returns a line list
//! re-coerced by [`crate::value::from_lines`]. That auto-coercion is what lets
//! `curl ... | grep foo | wc -l` read and behave like real bash while `jq` stays JSON-native.
//!
//! Patterns here are **literal strings with optional `^` and `$` anchors**, not regular
//! expressions. That is a deliberate Phase 1 boundary: a regex engine is a large dependency and a
//! large attack surface. An anchor escapes like anything else: `grep 'price\$'` searches for a
//! literal dollar sign at the end of a token, not for lines ending in `price`.
//!
//! Because basic regexes need no flag, the most common regex a model writes — `grep "[0-9]"`,
//! `sed "s/^ *//"` — is exactly the one a literal matcher would answer wrongly and silently. So the
//! *pattern* is checked as strictly as the flags: an unescaped regex metacharacter is rejected by
//! name, and `\[` escapes it back to a literal. A script therefore cannot believe a regex was
//! honored when it was matched literally.
//!
//! `.` is the documented exception: it is left literal rather than rejected, because a dot is far
//! more often part of a hostname, filename, or JSON path than a wildcard, and reading it literally
//! can only ever match *less* than the regex would — never something the script did not ask for.

use crate::builtins::CommandFailure;

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

/// Regex syntax that changes what a pattern means, and what to say about each piece.
const METACHARACTERS: &[(char, &str)] = &[
    ('[', "a character class"),
    (']', "a character class"),
    ('*', "a repetition"),
    ('+', "a repetition"),
    ('?', "an optional match"),
    ('(', "a group"),
    (')', "a group"),
    ('|', "an alternation"),
    ('{', "a repetition count"),
    ('}', "a repetition count"),
];

/// Reads a pattern as literal text, rejecting unescaped regex syntax by name.
pub(crate) fn literal_pattern(command: &str, pattern: &str) -> Result<String, CommandFailure> {
    let mut literal = String::with_capacity(pattern.len());
    let mut characters = pattern.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            match characters.next() {
                Some(escaped) => literal.push(escaped),
                None => literal.push('\\'),
            }
            continue;
        }
        if let Some((_, meaning)) = METACHARACTERS
            .iter()
            .find(|(candidate, _)| *candidate == character)
        {
            return Err(CommandFailure::usage(format!(
                "{command}: {pattern:?} uses {character:?}, which would mean {meaning} in a regular expression; patterns here are literal text, so write `\\{character}` for the character itself or use `jq` for real matching"
            )));
        }
        literal.push(character);
    }
    Ok(literal)
}

/// Reports whether a trailing `$` is an end anchor rather than an escaped dollar sign.
///
/// Anchors are stripped before [`literal_pattern`] processes escapes, so this has to read the
/// escape itself. `grep 'price\$'` — the standard way to search for a literal dollar at the end of
/// a token — otherwise loses its `$` to the anchor and keeps the now-dangling `\` as literal text,
/// silently matching lines ending in `price\`. An odd run of backslashes escapes the `$`; an even
/// one leaves it as the anchor it looks like, with the backslashes escaping each other.
pub(crate) fn ends_with_anchor(pattern: &str) -> bool {
    if !pattern.ends_with('$') {
        return false;
    }
    let escapes = pattern[..pattern.len() - 1]
        .chars()
        .rev()
        .take_while(|character| *character == '\\')
        .count();
    escapes % 2 == 0
}

/// A literal pattern with optional `^`/`$` anchors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Pattern {
    needle: String,
    anchored_start: bool,
    anchored_end: bool,
    ignore_case: bool,
}

impl Pattern {
    /// Compiles one pattern, rejecting regex syntax it cannot honor.
    pub(crate) fn compile(
        command: &str,
        pattern: &str,
        ignore_case: bool,
    ) -> Result<Self, CommandFailure> {
        let mut needle = pattern;
        let anchored_start = needle.starts_with('^');
        if anchored_start {
            needle = &needle[1..];
        }
        let anchored_end = needle.len() > 1 && ends_with_anchor(needle);
        if anchored_end {
            needle = &needle[..needle.len() - 1];
        }
        let needle = literal_pattern(command, needle)?;
        Ok(Self {
            needle: if ignore_case {
                needle.to_lowercase()
            } else {
                needle
            },
            anchored_start,
            anchored_end,
            ignore_case,
        })
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

    fn compile(pattern: &str, ignore_case: bool) -> Pattern {
        Pattern::compile("grep", pattern, ignore_case).expect("a literal pattern")
    }

    #[test]
    fn unanchored_patterns_match_substrings() {
        let pattern = compile("ell", false);
        assert!(pattern.matches("hello"));
        assert!(!pattern.matches("world"));
    }

    #[test]
    fn anchors_constrain_both_ends() {
        assert!(compile("^he", false).matches("hello"));
        assert!(!compile("^he", false).matches("the hen"));
        assert!(compile("lo$", false).matches("hello"));
        assert!(!compile("lo$", false).matches("hello there"));
        assert!(compile("^hello$", false).matches("hello"));
        assert!(!compile("^hello$", false).matches("hello there"));
    }

    #[test]
    fn case_folding_is_opt_in() {
        assert!(!compile("HELLO", false).matches("hello"));
        assert!(compile("HELLO", true).matches("hello"));
    }

    #[test]
    fn a_lone_dollar_stays_literal() {
        // `$` alone is a plausible literal search term, so it is not treated as an empty anchor.
        assert!(compile("$", false).matches("cost: $5"));
    }

    #[test]
    fn an_escaped_dollar_is_a_literal_rather_than_an_anchor() {
        // `grep 'price\$'` is how a script searches for a literal dollar sign. Reading the `$` as
        // an anchor left the dangling `\` as literal text, so the pattern matched lines ending in
        // `price\` — a silently wrong match, which is the one thing this module promises cannot
        // happen.
        let pattern = compile(r"price\$", false);
        assert!(pattern.matches("total price$ here"));
        assert!(!pattern.matches(r"total price\"));

        // An even run of backslashes escapes itself, so the `$` stays the anchor it looks like.
        let anchored = compile(r"price\\$", false);
        assert!(anchored.matches(r"total price\"));
        assert!(!anchored.matches("total price$ here"));
    }

    #[test]
    fn regex_syntax_is_rejected_by_name_rather_than_matched_literally() {
        // These are the patterns a model writes without thinking, and every one of them would have
        // quietly matched nothing.
        for pattern in ["[0-9]", "a|b", "^ *", "colou?r", "(a)", "x{2}", "a.*b"] {
            let failure =
                Pattern::compile("grep", pattern, false).expect_err("regex syntax is rejected");
            let message = format!("{failure:?}");
            assert!(message.contains("literal text"), "{pattern}: {message}");
        }
    }

    #[test]
    fn escaping_recovers_a_metacharacter_as_ordinary_text() {
        assert!(compile(r"\[warn\]", false).matches("a [warn] line"));
        assert!(compile(r"2 \+ 2", false).matches("2 + 2"));
        // A dot stays literal rather than being rejected; it can only ever match less.
        assert!(compile("example.com", false).matches("host example.com here"));
        assert!(!compile("example.com", false).matches("exampleXcom"));
    }
}
