//! `sed [-E] s/pattern/replacement/[flags]` — substitution only.

use regex_bites::{NoExpand, Regex};
use serde_json::Value;

use crate::{
    builtins::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag},
    value::{from_lines, to_lines},
};

/// Substitutes text line by line — literal by default, a regular expression under `-E`.
///
/// Only the `s` command is implemented. Addresses, `d`, `p`, `-n`, and script files are absent
/// rather than approximated, and any other command is rejected by name.
pub(crate) struct Sed;

impl Builtin for Sed {
    fn name(&self) -> &'static str {
        "sed"
    }

    fn run(
        &self,
        _context: &mut BuiltinContext<'_>,
        arguments: &[String],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let mut script = None;
        let mut extended = false;
        for argument in arguments {
            match argument.as_str() {
                // `-e` simply introduces the script, which is the only form supported anyway.
                "-e" | "--expression" => {}
                "-E" | "--regexp-extended" => extended = true,
                flag if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(unsupported_flag("sed", flag));
                }
                literal => {
                    if script.is_some() {
                        return Err(CommandFailure::usage(
                            "sed: exactly one substitution script is supported",
                        ));
                    }
                    script = Some(literal.to_owned());
                }
            }
        }
        let Some(script) = script else {
            return Err(CommandFailure::usage(
                "sed: a substitution script is required, formatted as s/pattern/replacement/flags",
            ));
        };

        let substitution = Substitution::parse(&script, extended)?;
        let lines = to_lines(&input.unwrap_or(Value::Null))
            .into_iter()
            .map(|line| substitution.apply(&line))
            .collect::<Vec<_>>();
        Ok(CommandResult::value(from_lines(lines)))
    }
}

/// One parsed `s/pattern/replacement/flags` command.
#[derive(Clone, Debug)]
pub(crate) struct Substitution {
    matcher: Matcher,
    replacement: String,
    global: bool,
}

/// What the substitution's left-hand side matches with.
#[derive(Clone, Debug)]
enum Matcher {
    /// Literal text, with `-i` folding case through [`str::to_lowercase`].
    Literal { needle: String, ignore_case: bool },
    /// An `-E` regular expression. Each line is its own haystack, so `^` and `$` anchor the line.
    Extended(Regex),
}

impl Substitution {
    /// Parses a substitution script, accepting any single-character delimiter after `s`.
    pub(crate) fn parse(script: &str, extended: bool) -> Result<Self, CommandFailure> {
        let mut characters = script.chars();
        if characters.next() != Some('s') {
            return Err(CommandFailure::usage(format!(
                "sed: only the `s` command is supported; found {script:?}"
            )));
        }
        let Some(delimiter) = characters.next() else {
            return Err(CommandFailure::usage(
                "sed: the `s` command needs a delimiter, as in s/pattern/replacement/",
            ));
        };
        if delimiter.is_alphanumeric() || delimiter == '\\' {
            return Err(CommandFailure::usage(format!(
                "sed: {delimiter:?} is not a usable substitution delimiter"
            )));
        }

        let body = &script[('s'.len_utf8() + delimiter.len_utf8())..];
        let fields = split_unescaped(body, delimiter);
        if fields.len() != 3 {
            return Err(CommandFailure::usage(format!(
                "sed: {script:?} must be formatted as s{delimiter}pattern{delimiter}replacement{delimiter}flags"
            )));
        }

        let mut global = false;
        let mut ignore_case = false;
        for flag in fields[2].chars() {
            match flag {
                'g' => global = true,
                'i' | 'I' => ignore_case = true,
                other => {
                    return Err(CommandFailure::usage(format!(
                        "sed: substitution flag {other:?} is not supported; only `g` and `i` are"
                    )));
                }
            }
        }
        if fields[0].is_empty() {
            return Err(CommandFailure::usage(
                "sed: an empty substitution pattern matches nothing useful",
            ));
        }

        // Without `-E` this `s` command has no anchors at all, so a leading `^` or trailing `$`
        // would be matched as that character. `sed "s/^ *//"` silently returning its input
        // unchanged is precisely the failure this module claims cannot happen. An *escaped* `\$` is
        // a literal dollar sign rather than an anchor, so it passes through to `literal_pattern`.
        // Under `-E` both are the anchors they look like and the engine reads them.
        let matcher = if extended {
            Matcher::Extended(super::extended_pattern("sed", &fields[0], ignore_case)?)
        } else {
            if fields[0].starts_with('^') || super::ends_with_anchor(&fields[0]) {
                return Err(CommandFailure::usage(format!(
                    "sed: {:?} anchors with `^`/`$`, which this substitution does not support without `-E`; match the literal text, use `-E` for a real regular expression, or use `grep` for anchored selection",
                    fields[0]
                )));
            }
            Matcher::Literal {
                needle: super::literal_pattern("sed", &fields[0])?,
                ignore_case,
            }
        };
        // `&` in a real sed replacement inserts the matched text. Treating it as a literal
        // ampersand would rewrite the line into something the script never asked for.
        if has_unescaped_ampersand(&fields[1]) {
            return Err(CommandFailure::usage(
                "sed: `&` in a replacement means the matched text in real sed and is not supported here; write `\\&` for a literal ampersand",
            ));
        }
        // Same rule one step further. `\1` is a capture-group reference in real sed — a BRE group
        // without `-E`, an ERE group with it — and the replacement here is inserted verbatim, so
        // emitting a literal backslash-one is exactly the silent mismatch this module refuses.
        if let Some(digit) = group_reference(&fields[1]) {
            return Err(CommandFailure::usage(format!(
                "sed: `\\{digit}` in a replacement is a capture-group reference in real sed and is not supported here; write `\\\\{digit}` for a literal backslash"
            )));
        }
        let replacement = fields[1].replace("\\&", "&");

        Ok(Self {
            matcher,
            replacement,
            global,
        })
    }

    /// Applies the substitution to one line.
    pub(crate) fn apply(&self, line: &str) -> String {
        let (needle, ignore_case) = match &self.matcher {
            // `NoExpand`, not the engine's `$1` interpolation: a replacement is literal text in
            // both modes, so a `$` a script wrote stays the dollar sign it wrote.
            Matcher::Extended(regex) => {
                let replaced = if self.global {
                    regex.replace_all(line, NoExpand(&self.replacement))
                } else {
                    regex.replace(line, NoExpand(&self.replacement))
                };
                return replaced.into_owned();
            }
            Matcher::Literal {
                needle,
                ignore_case,
            } => (needle, *ignore_case),
        };
        if ignore_case {
            return self.apply_case_insensitive(line, needle);
        }
        if self.global {
            return line.replace(needle, &self.replacement);
        }
        line.replacen(needle, &self.replacement, 1)
    }

    fn apply_case_insensitive(&self, line: &str, pattern: &str) -> String {
        let haystack = line.to_lowercase();
        let needle = pattern.to_lowercase();
        let mut output = String::with_capacity(line.len());
        let mut cursor = 0;
        while cursor <= line.len() {
            let Some(offset) = haystack.get(cursor..).and_then(|rest| rest.find(&needle)) else {
                break;
            };
            let start = cursor + offset;
            let end = start + needle.len();
            if !line.is_char_boundary(start) || !line.is_char_boundary(end) {
                break;
            }
            output.push_str(&line[cursor..start]);
            output.push_str(&self.replacement);
            cursor = end;
            if !self.global {
                break;
            }
        }
        output.push_str(line.get(cursor..).unwrap_or_default());
        output
    }
}

/// Returns the digit of the first capture-group reference in a replacement, if there is one.
///
/// `\1` is a group reference; `\\1` is an escaped backslash followed by a digit and is not.
fn group_reference(replacement: &str) -> Option<char> {
    let mut characters = replacement.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            continue;
        }
        match characters.next() {
            Some(next) if next.is_ascii_digit() => return Some(next),
            Some(_) => continue,
            None => break,
        }
    }
    None
}

/// Reports whether a replacement contains a `&` that is not written as `\&`.
fn has_unescaped_ampersand(replacement: &str) -> bool {
    let mut characters = replacement.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            characters.next();
            continue;
        }
        if character == '&' {
            return true;
        }
    }
    false
}

/// Splits on an unescaped delimiter, honoring `\<delimiter>`.
fn split_unescaped(body: &str, delimiter: char) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut characters = body.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            match characters.next() {
                Some(escaped) if escaped == delimiter => {
                    if let Some(last) = fields.last_mut() {
                        last.push(escaped);
                    }
                }
                Some('n') => {
                    if let Some(last) = fields.last_mut() {
                        last.push('\n');
                    }
                }
                Some('t') => {
                    if let Some(last) = fields.last_mut() {
                        last.push('\t');
                    }
                }
                Some(other) => {
                    if let Some(last) = fields.last_mut() {
                        last.push('\\');
                        last.push(other);
                    }
                }
                None => {
                    if let Some(last) = fields.last_mut() {
                        last.push('\\');
                    }
                }
            }
            continue;
        }
        if character == delimiter {
            fields.push(String::new());
            continue;
        }
        if let Some(last) = fields.last_mut() {
            last.push(character);
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::builtins::{CommandResult, test_support::run_builtin};

    use super::{Sed, Substitution};

    fn sed(arguments: &[&str], input: Value) -> CommandResult {
        run_builtin(&Sed, arguments, Some(input)).expect("sed runs")
    }

    #[test]
    fn replaces_the_first_occurrence_by_default() {
        assert_eq!(sed(&["s/a/X/"], json!("banana")).value, json!("bXnana"));
    }

    #[test]
    fn the_g_flag_replaces_every_occurrence() {
        assert_eq!(sed(&["s/a/X/g"], json!("banana")).value, json!("bXnXnX"));
    }

    #[test]
    fn the_i_flag_folds_case() {
        assert_eq!(sed(&["s/A/X/gi"], json!("Banana")).value, json!("BXnXnX"));
    }

    #[test]
    fn operates_line_by_line_over_arrays() {
        assert_eq!(
            sed(&["s/o/0/g"], json!(["foo", "bop"])).value,
            json!(["f00", "b0p"])
        );
    }

    #[test]
    fn alternate_delimiters_and_escapes_work() {
        assert_eq!(
            sed(&["s|/usr|/opt|"], json!("/usr/bin")).value,
            json!("/opt/bin")
        );
        assert_eq!(sed(&[r"s/a\/b/x/"], json!("a/b")).value, json!("x"));
    }

    #[test]
    fn malformed_scripts_are_rejected() {
        for script in ["d", "s/a", "s/a/b/z", "s//x/", "sxaxbx"] {
            assert!(
                Substitution::parse(script, false).is_err(),
                "{script:?} must be rejected"
            );
            assert!(
                Substitution::parse(script, true).is_err(),
                "{script:?} must be rejected under -E too"
            );
        }
    }

    #[test]
    fn regex_syntax_and_anchors_are_rejected_rather_than_silently_literal() {
        // `s/^ *//` returning its input unchanged, and `s/x/[&]/` inserting a literal ampersand,
        // are the two ways this substitution could quietly answer a question it was not asked.
        for script in [
            "s/^ *//",
            "s/[0-9]*//g",
            "s/^foo/bar/",
            "s/foo$/bar/",
            "s/a|b/x/",
            "s/x/[&]/",
        ] {
            let failure = Substitution::parse(script, false).expect_err(script);
            let message = format!("{failure:?}");
            assert!(
                message.contains("literal text")
                    || message.contains("anchors")
                    || message.contains("matched text"),
                "{script}: {message}"
            );
        }
        // Escaped forms recover the literal characters.
        assert_eq!(
            sed(&[r"s/\[x\]/y/"], json!("a [x] b")).value,
            json!("a y b")
        );
        assert_eq!(sed(&[r"s/x/a\&b/"], json!("x")).value, json!("a&b"));
    }

    #[test]
    fn an_escaped_dollar_is_substituted_rather_than_rejected_as_an_anchor() {
        // `s/price\$/x/` names a literal dollar sign, not an anchor, and rejecting it sent a script
        // looking for a workaround that does not exist. An even backslash run is still an anchor.
        assert_eq!(
            sed(&[r"s/price\$/cost/"], json!("the price$ line")).value,
            json!("the cost line")
        );
        assert!(Substitution::parse(r"s/price\\$/x/", false).is_err());
    }

    #[test]
    fn unsupported_flags_are_rejected_by_name() {
        let failure = run_builtin(&Sed, &["-n", "s/a/b/"], Some(json!("a")))
            .expect_err("-n is not supported");
        assert!(format!("{failure:?}").contains("-n"), "{failure:?}");
    }

    #[test]
    fn the_e_flag_substitutes_with_the_regex_engine() {
        // `sed "s/^ *//"` is the other idiom a model reaches for first. Unflagged it is a usage
        // error naming the anchor; with `-E` the anchor is the anchor.
        assert_eq!(
            sed(&["-E", "s/^ *//"], json!("   indented")).value,
            json!("indented")
        );
        assert_eq!(
            sed(&["-E", "s/[0-9]+/N/g"], json!("a1b22c333")).value,
            json!("aNbNcN")
        );
        // The `g` and `i` flags still mean what they meant.
        assert_eq!(
            sed(&["-E", "s/a+/X/"], json!("aaa aaa")).value,
            json!("X aaa")
        );
        assert_eq!(
            sed(&["-E", "s/A+/X/gi"], json!("aaa aaa")).value,
            json!("X X")
        );
        assert_eq!(
            sed(&["-E", "s/o$/0/"], json!(["foo", "of"])).value,
            json!(["fo0", "of"])
        );
    }

    #[test]
    fn an_e_replacement_stays_literal_text() {
        // The engine's own `$1` interpolation is off: a `$` a script wrote is a dollar sign, the
        // same as it is on the literal path.
        assert_eq!(sed(&["-E", "s/(a)(b)/$2/"], json!("ab")).value, json!("$2"));
        // ...which is exactly why a real-sed group reference has to be refused rather than emitted
        // verbatim. Both modes refuse it, because both have group syntax in real sed.
        for script in [r"s/(a)/\1/", r"s/a/\1/"] {
            let failure = Substitution::parse(script, true).expect_err(script);
            assert!(
                format!("{failure:?}").contains("capture-group reference"),
                "{script}: {failure:?}"
            );
        }
        // An escaped backslash is a backslash, not a reference, so it parses rather than being
        // refused.
        assert!(Substitution::parse(r"s/a/\\1/", true).is_ok());
    }

    #[test]
    fn without_the_e_flag_a_regex_is_still_refused_by_name() {
        let failure =
            run_builtin(&Sed, &["s/^ *//"], Some(json!("  x"))).expect_err("literal by default");
        assert!(format!("{failure:?}").contains("anchors"), "{failure:?}");
    }

    #[test]
    fn an_uncompilable_e_pattern_fails_rather_than_matching_nothing() {
        let failure = run_builtin(&Sed, &["-E", "s/a(/x/"], Some(json!("a(")))
            .expect_err("an unclosed group is not a literal");
        assert!(
            format!("{failure:?}").contains("closing ')'"),
            "{failure:?}"
        );
    }
}
