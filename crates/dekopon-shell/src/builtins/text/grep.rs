//! `grep [-v] [-i] [-c] [-n] [-E] PATTERN`.

use serde_json::Value;

use crate::{
    ExitCode,
    builtins::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag},
    value::{from_lines, to_lines},
};

use super::Pattern;

/// Selects lines matching a literal, optionally anchored pattern — or, under `-E`, a regular
/// expression compiled by the engine `jq` already links.
pub(crate) struct Grep;

impl Builtin for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn run(
        &self,
        _context: &mut BuiltinContext<'_>,
        arguments: &[String],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let mut invert = false;
        let mut ignore_case = false;
        let mut count_only = false;
        let mut number = false;
        let mut extended = false;
        let mut pattern = None;

        for argument in arguments {
            match argument.as_str() {
                "-v" | "--invert-match" => invert = true,
                "-i" | "--ignore-case" => ignore_case = true,
                "-c" | "--count" => count_only = true,
                "-n" | "--line-number" => number = true,
                "-E" | "--extended-regexp" => extended = true,
                flag if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(unsupported_flag("grep", flag));
                }
                literal => {
                    if pattern.is_some() {
                        return Err(CommandFailure::usage(
                            "grep: exactly one pattern argument is supported",
                        ));
                    }
                    pattern = Some(literal.to_owned());
                }
            }
        }

        let Some(pattern) = pattern else {
            return Err(CommandFailure::usage(
                "grep: a pattern argument is required",
            ));
        };
        let pattern = Pattern::compile("grep", &pattern, ignore_case, extended)?;

        let mut matched = Vec::new();
        for (index, line) in to_lines(&input.unwrap_or(Value::Null))
            .into_iter()
            .enumerate()
        {
            if pattern.matches(&line) == invert {
                continue;
            }
            matched.push(if number {
                format!("{}:{line}", index + 1)
            } else {
                line
            });
        }

        // Real grep exits 1 when nothing matched; `grep x && ...` depends on that.
        let status = if matched.is_empty() {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        };
        let value = if count_only {
            Value::from(matched.len())
        } else {
            from_lines(matched)
        };
        Ok(CommandResult {
            value,
            status,
            suppress_newline: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{
        ExitCode,
        builtins::{CommandResult, test_support::run_builtin},
    };

    use super::Grep;

    fn grep(arguments: &[&str], input: Value) -> CommandResult {
        run_builtin(&Grep, arguments, Some(input)).expect("grep runs")
    }

    #[test]
    fn selects_matching_lines_from_a_string() {
        let result = grep(&["ell"], json!("hello\nworld\nshell"));
        assert_eq!(result.value, json!(["hello", "shell"]));
        assert_eq!(result.status, ExitCode::SUCCESS);
    }

    #[test]
    fn accepts_a_json_array_of_lines() {
        let result = grep(&["b"], json!(["alpha", "bravo"]));
        assert_eq!(result.value, json!("bravo"));
    }

    #[test]
    fn no_match_exits_one_and_emits_nothing_like_real_grep() {
        let result = grep(&["zzz"], json!("hello"));
        assert_eq!(result.status, ExitCode::FAILURE);
        // Null, not an empty string: real grep prints nothing when nothing matched, and an empty
        // string would emit a phantom blank line into the script's output.
        assert_eq!(result.value, Value::Null);
    }

    #[test]
    fn supports_invert_ignore_case_count_and_number() {
        assert_eq!(grep(&["-v", "o"], json!("foo\nbar")).value, json!("bar"));
        assert_eq!(grep(&["-i", "FOO"], json!("foo")).value, json!("foo"));
        assert_eq!(grep(&["-c", "o"], json!("foo\nbar\nboo")).value, json!(2));
        assert_eq!(
            grep(&["-n", "o"], json!("foo\nbar\nboo")).value,
            json!(["1:foo", "3:boo"])
        );
    }

    #[test]
    fn unsupported_flags_are_rejected_by_name() {
        let failure = run_builtin(&Grep, &["-o", "a"], Some(json!("a")))
            .expect_err("only-matching is not implemented");
        assert!(format!("{failure:?}").contains("-o"), "{failure:?}");
    }

    #[test]
    fn the_e_flag_matches_with_the_regex_engine() {
        // `grep "[0-9]"` is the single most common thing a model writes, and without `-E` it is a
        // usage error naming the character class. With it, the engine answers.
        assert_eq!(
            grep(&["-E", "[0-9]"], json!("port 8080\nno digits")).value,
            json!("port 8080")
        );
        assert_eq!(
            grep(&["-E", "^ba(r|z)$"], json!(["bar", "baz", "barn"])).value,
            json!(["bar", "baz"])
        );
        // The other flags keep working against a regex.
        assert_eq!(
            grep(&["-c", "-E", r"\d"], json!("a1\nb2\ncc")).value,
            json!(2)
        );
        assert_eq!(
            grep(&["-v", "-E", "[0-9]"], json!("a1\ncc")).value,
            json!("cc")
        );
        assert_eq!(
            grep(&["-i", "-E", "^A+$"], json!("aaa")).value,
            json!("aaa")
        );
    }

    #[test]
    fn without_the_e_flag_a_regex_is_still_refused_by_name() {
        let failure =
            run_builtin(&Grep, &["[0-9]"], Some(json!("a1"))).expect_err("literal by default");
        let message = format!("{failure:?}");
        assert!(message.contains("literal text"), "{message}");
    }

    #[test]
    fn an_uncompilable_e_pattern_fails_rather_than_matching_nothing() {
        let failure = run_builtin(&Grep, &["-E", "a("], Some(json!("a(")))
            .expect_err("an unclosed group is not a literal");
        assert!(
            format!("{failure:?}").contains("closing ')'"),
            "{failure:?}"
        );
    }
}
