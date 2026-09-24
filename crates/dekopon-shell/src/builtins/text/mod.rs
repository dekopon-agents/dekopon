//! Without -E, a pattern matches literally and unescaped regex syntax is rejected by name instead
//! of being silently treated as literal text, so a script can never mistake literal matching for a
//! regex that ran.

use std::borrow::Cow;

use regex_bites::{Regex, RegexBuilder};

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

/// An odd number of trailing backslashes escapes the dollar sign as a literal character; an even
/// number leaves it as the end anchor it appears to be.
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

const EXTENDED_PATTERN_MAX_BYTES: usize = 1024;

/// The compiled pattern size limit is set far below the regex engine's own ten-megabyte default
/// because these patterns are untrusted, model-authored input running inside a resource-bounded
/// sandbox.
const EXTENDED_PATTERN_SIZE_LIMIT: usize = 64 * 1024;

const EXTENDED_PATTERN_NEST_LIMIT: u32 = 16;

/// Case-insensitive matching under the extended flag folds ASCII characters only, since the
/// underlying regex engine has no Unicode case folding, making it narrower than the literal path's
/// case-insensitive matching, never wider.
pub(crate) fn extended_pattern(
    command: &str,
    pattern: &str,
    ignore_case: bool,
) -> Result<Regex, CommandFailure> {
    if pattern.len() > EXTENDED_PATTERN_MAX_BYTES {
        return Err(CommandFailure::usage(format!(
            "{command}: -E pattern is {} bytes; the limit is {EXTENDED_PATTERN_MAX_BYTES}",
            pattern.len()
        )));
    }
    RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .size_limit(EXTENDED_PATTERN_SIZE_LIMIT)
        .nest_limit(EXTENDED_PATTERN_NEST_LIMIT)
        .build()
        .map_err(|error| {
            CommandFailure::usage(format!(
                "{command}: -E pattern {pattern:?} did not compile: {error}"
            ))
        })
}

#[derive(Clone, Debug)]
pub(crate) enum Pattern {
    Literal {
        needle: String,
        anchored_start: bool,
        anchored_end: bool,
        ignore_case: bool,
    },
    Extended(Regex),
}

impl Pattern {
    pub(crate) fn compile(
        command: &str,
        pattern: &str,
        ignore_case: bool,
        extended: bool,
    ) -> Result<Self, CommandFailure> {
        if extended {
            return Ok(Self::Extended(extended_pattern(
                command,
                pattern,
                ignore_case,
            )?));
        }
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
        Ok(Self::Literal {
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

    pub(crate) fn matches(&self, line: &str) -> bool {
        let (needle, anchored_start, anchored_end, ignore_case) = match self {
            Self::Extended(regex) => return regex.is_match(line),
            Self::Literal {
                needle,
                anchored_start,
                anchored_end,
                ignore_case,
            } => (needle, *anchored_start, *anchored_end, *ignore_case),
        };
        let candidate = if ignore_case {
            Cow::Owned(line.to_lowercase())
        } else {
            Cow::Borrowed(line)
        };
        let candidate: &str = &candidate;
        match (anchored_start, anchored_end) {
            (true, true) => candidate == needle,
            (true, false) => candidate.starts_with(needle),
            (false, true) => candidate.ends_with(needle),
            (false, false) => candidate.contains(needle),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EXTENDED_PATTERN_MAX_BYTES, Pattern};

    fn compile(pattern: &str, ignore_case: bool) -> Pattern {
        Pattern::compile("grep", pattern, ignore_case, false).expect("a literal pattern")
    }

    fn compile_extended(pattern: &str, ignore_case: bool) -> Pattern {
        Pattern::compile("grep", pattern, ignore_case, true).expect("an extended pattern")
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
        assert!(compile("$", false).matches("cost: $5"));
    }

    #[test]
    fn an_escaped_dollar_is_a_literal_rather_than_an_anchor() {
        let pattern = compile(r"price\$", false);
        assert!(pattern.matches("total price$ here"));
        assert!(!pattern.matches(r"total price\"));

        let anchored = compile(r"price\\$", false);
        assert!(anchored.matches(r"total price\"));
        assert!(!anchored.matches("total price$ here"));
    }

    #[test]
    fn regex_syntax_is_rejected_by_name_rather_than_matched_literally() {
        for pattern in ["[0-9]", "a|b", "^ *", "colou?r", "(a)", "x{2}", "a.*b"] {
            let failure = Pattern::compile("grep", pattern, false, false)
                .expect_err("regex syntax is rejected");
            let message = format!("{failure:?}");
            assert!(message.contains("literal text"), "{pattern}: {message}");
        }
    }

    #[test]
    fn extended_patterns_are_the_regexes_the_literal_path_refuses() {
        assert!(compile_extended("[0-9]", false).matches("port 8080"));
        assert!(!compile_extended("[0-9]", false).matches("no digits"));
        assert!(compile_extended("colou?r", false).matches("color"));
        assert!(compile_extended("^a|b$", false).matches("about"));
        assert!(compile_extended("a.c", false).matches("abc"));
        assert!(!compile_extended("HELLO", false).matches("hello"));
        assert!(compile_extended("HELLO", true).matches("hello"));
    }

    #[test]
    fn an_uncompilable_extended_pattern_reports_the_engine_error_by_name() {
        let failure =
            Pattern::compile("grep", "a(", false, true).expect_err("an unclosed group is refused");
        let message = format!("{failure:?}");
        assert!(message.contains("closing ')'"), "{message}");
        assert!(message.contains("did not compile"), "{message}");

        let backreference = Pattern::compile("grep", r"(a)\1", false, true)
            .expect_err("backreferences are refused");
        assert!(
            format!("{backreference:?}").contains("backreferences"),
            "{backreference:?}"
        );
    }

    #[test]
    fn extended_patterns_are_bounded_before_they_ever_see_input() {
        let long = "a".repeat(EXTENDED_PATTERN_MAX_BYTES + 1);
        let failure = Pattern::compile("grep", &long, false, true).expect_err("too long");
        assert!(
            format!("{failure:?}").contains("the limit is"),
            "{failure:?}"
        );

        let big = "[0-9]{1,1000000}";
        let failure = Pattern::compile("grep", big, false, true).expect_err("too big");
        assert!(format!("{failure:?}").contains("size limit"), "{failure:?}");

        let deep = format!("{}a{}", "(".repeat(64), ")".repeat(64));
        let failure = Pattern::compile("grep", &deep, false, true).expect_err("too deep");
        assert!(format!("{failure:?}").contains("nesting"), "{failure:?}");
    }

    #[test]
    fn escaping_recovers_a_metacharacter_as_ordinary_text() {
        assert!(compile(r"\[warn\]", false).matches("a [warn] line"));
        assert!(compile(r"2 \+ 2", false).matches("2 + 2"));
        assert!(compile("example.com", false).matches("host example.com here"));
        assert!(!compile("example.com", false).matches("exampleXcom"));
    }
}
