//! `sed s/pattern/replacement/[flags]` — substitution only.

use serde_json::Value;

use crate::{
    builtins::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag},
    value::{from_lines, to_lines},
};

/// Substitutes literal text line by line.
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
        for argument in arguments {
            match argument.as_str() {
                // `-e` simply introduces the script, which is the only form supported anyway.
                "-e" | "--expression" => {}
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

        let substitution = Substitution::parse(&script)?;
        let lines = to_lines(&input.unwrap_or(Value::Null))
            .into_iter()
            .map(|line| substitution.apply(&line))
            .collect::<Vec<_>>();
        Ok(CommandResult::value(from_lines(lines)))
    }
}

/// One parsed `s/pattern/replacement/flags` command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Substitution {
    pattern: String,
    replacement: String,
    global: bool,
    ignore_case: bool,
}

impl Substitution {
    /// Parses a substitution script, accepting any single-character delimiter after `s`.
    pub(crate) fn parse(script: &str) -> Result<Self, CommandFailure> {
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

        Ok(Self {
            pattern: fields[0].clone(),
            replacement: fields[1].clone(),
            global,
            ignore_case,
        })
    }

    /// Applies the substitution to one line.
    pub(crate) fn apply(&self, line: &str) -> String {
        if self.ignore_case {
            return self.apply_case_insensitive(line);
        }
        if self.global {
            return line.replace(&self.pattern, &self.replacement);
        }
        line.replacen(&self.pattern, &self.replacement, 1)
    }

    fn apply_case_insensitive(&self, line: &str) -> String {
        let haystack = line.to_lowercase();
        let needle = self.pattern.to_lowercase();
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
                Substitution::parse(script).is_err(),
                "{script:?} must be rejected"
            );
        }
    }

    #[test]
    fn unsupported_flags_are_rejected_by_name() {
        let failure = run_builtin(&Sed, &["-n", "s/a/b/"], Some(json!("a")))
            .expect_err("-n is not supported");
        assert!(format!("{failure:?}").contains("-n"), "{failure:?}");
    }
}
