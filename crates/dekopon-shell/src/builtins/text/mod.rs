//! Text-shaped builtins.
//!
//! Each of these accepts either a raw string or a JSON array of lines and returns a line list
//! re-coerced by [`crate::value::from_lines`]. That auto-coercion is what lets
//! `curl ... | grep foo | wc -l` read and behave like real bash while `jq` stays JSON-native.
//!
//! Patterns here are **literal strings with optional `^` and `$` anchors** unless the command is
//! given `-E`. The default is not about dependency weight — `jq`'s `regex` feature already links
//! [`regex_bites`] into this crate, and `grep -E`/`sed -E` compile against that same engine rather
//! than a second one. It is about having *one* matching semantics: `grep`, `sed`, a `case` arm,
//! `${p#…}`, and the right operand of `[[ == ]]` all read an unflagged pattern the same way, so a
//! script never has to know which of five constructs it is standing in to know what `[0-9]` means.
//! An anchor escapes like anything else: `grep 'price\$'` searches for a literal dollar sign at the
//! end of a token, not for lines ending in `price`.
//!
//! Because basic regexes need no flag in real bash, the most common regex a model writes —
//! `grep "[0-9]"`, `sed "s/^ *//"` — is exactly the one a literal matcher would answer wrongly and
//! silently. So without `-E` the *pattern* is checked as strictly as the flags: an unescaped regex
//! metacharacter is rejected by name, and `\[` escapes it back to a literal. A script therefore
//! cannot believe a regex was honored when it was matched literally.
//!
//! `-E` is that rejection's answer rather than an exception to it. It is explicit, it is the only
//! way regex syntax ever becomes regex syntax, and it fails as loudly: the engine's own compile
//! error is reported by name, and a pattern is bounded by length, compiled size, and nesting
//! before it ever sees input, because an `-E` pattern is model-authored text.
//!
//! `.` is the documented exception to the *rejection*, not to the matching: without `-E` it is left
//! literal rather than rejected, because a dot is far more often part of a hostname, filename, or
//! JSON path than a wildcard, and reading it literally can only ever match *less* than the regex
//! would — never something the script did not ask for. Under `-E` it is the wildcard the script
//! asked for.

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

/// Longest `-E` pattern accepted, in bytes.
///
/// The engine's own parser uses heap proportional to the pattern's length, so the pattern string is
/// the first thing to bound. A kilobyte is far past any regex a shell one-liner writes.
const EXTENDED_PATTERN_MAX_BYTES: usize = 1024;

/// Compiled-program ceiling for an `-E` pattern.
///
/// The engine defaults to ten megabytes, which is sized for a program that compiles a regex from
/// its own source. These patterns arrive from a model, inside a sandbox whose whole point is
/// bounded resource use, so 64 KiB is the budget instead: `[0-9]{1,1000000}` is refused at compile
/// time rather than after it has allocated.
const EXTENDED_PATTERN_SIZE_LIMIT: usize = 64 * 1024;

/// Nesting ceiling for an `-E` pattern. The engine defaults to 50; nothing a script writes on one
/// line needs sixteen levels of groups.
const EXTENDED_PATTERN_NEST_LIMIT: u32 = 16;

/// Compiles an `-E` pattern with this shell's bounds, reporting the engine's own error by name.
///
/// Case folding is the engine's, not [`str::to_lowercase`]'s: `regex-bites` matches codepoint by
/// codepoint and does not implement Unicode case insensitivity, so `-iE` folds ASCII only. That is
/// narrower than the literal path's `-i`, never wider, so it cannot match something the script did
/// not ask for.
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

/// A compiled `grep` pattern: literal text by default, a real regular expression under `-E`.
#[derive(Clone, Debug)]
pub(crate) enum Pattern {
    /// A literal needle with optional `^`/`$` anchors.
    Literal {
        needle: String,
        anchored_start: bool,
        anchored_end: bool,
        ignore_case: bool,
    },
    /// An `-E` regular expression. Each line is its own haystack, so `^` and `$` anchor the line
    /// without the engine's multi-line mode.
    Extended(Regex),
}

impl Pattern {
    /// Compiles one pattern, rejecting regex syntax it cannot honor unless `-E` asked for it.
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

    /// Reports whether one line matches.
    ///
    /// The common literal path borrows. Only `-i` needs a new string, because only case folding
    /// changes the bytes being compared; copying every line for the other three quarters of the
    /// matrix bought nothing and made `grep` allocate once per line of its input.
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
        // quietly matched nothing. `-E` is the only thing that turns any of them into a regex.
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
        // `.` is the wildcard the script asked for here, unlike the literal path.
        assert!(compile_extended("a.c", false).matches("abc"));
        // ASCII case folding is the engine's, and `-i` still has to be asked for.
        assert!(!compile_extended("HELLO", false).matches("hello"));
        assert!(compile_extended("HELLO", true).matches("hello"));
    }

    #[test]
    fn an_uncompilable_extended_pattern_reports_the_engine_error_by_name() {
        let failure =
            Pattern::compile("grep", "a(", false, true).expect_err("an unclosed group is refused");
        let message = format!("{failure:?}");
        // The engine's own words, not a generic "bad pattern": a model has to be able to tell an
        // unclosed group from an unsupported construct to write a different pattern.
        assert!(message.contains("closing ')'"), "{message}");
        assert!(message.contains("did not compile"), "{message}");

        // Backreferences and look-around are the two things this engine does not implement. Both
        // have to be refused by name rather than matched as something else.
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

        // A pattern well inside the length bound can still compile to a program that is not.
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
        // A dot stays literal rather than being rejected; it can only ever match less.
        assert!(compile("example.com", false).matches("host example.com here"));
        assert!(!compile("example.com", false).matches("exampleXcom"));
    }
}
