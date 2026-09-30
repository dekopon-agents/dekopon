use crate::builtins::{CommandFailure, unsupported_flag};

use super::Pattern;

const HELP: &str = "-v -i -c -n -E";

pub(crate) struct GrepConfig {
    invert: bool,
    count_only: bool,
    number: bool,
    pattern: Pattern,
}

impl GrepConfig {
    pub(crate) fn parse(arguments: &[String]) -> Result<Self, CommandFailure> {
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
                    return Err(unsupported_flag("grep", flag, HELP));
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
        Ok(Self {
            invert,
            count_only,
            number,
            pattern: Pattern::compile("grep", &pattern, ignore_case, extended)?,
        })
    }

    pub(crate) fn count_only(&self) -> bool {
        self.count_only
    }

    pub(crate) fn selects(
        &self,
        line: &str,
        budget: &crate::limits::Budget,
    ) -> Result<bool, CommandFailure> {
        Ok(self.pattern.matches_charged(line, budget)? != self.invert)
    }

    pub(crate) fn number(&self) -> bool {
        self.number
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{
        ExitCode,
        builtins::{
            CommandResult,
            text::stream::{TextStream, run_test},
        },
    };

    fn grep(arguments: &[&str], input: Value) -> CommandResult {
        run_test(TextStream::Grep, arguments, input).expect("grep runs")
    }

    #[test]
    fn selects_matching_lines_from_a_string() {
        let result = grep(&["ell"], json!("hello\nworld\nshell"));
        assert_eq!(result.value, json!("hello\nshell"));
        assert_eq!(result.status, ExitCode::SUCCESS);
    }

    #[test]
    fn accepts_separate_text_lines() {
        let result = grep(&["b"], json!("alpha\nbravo"));
        assert_eq!(result.value, json!("bravo"));
    }

    #[test]
    fn no_match_exits_one_and_emits_nothing_like_real_grep() {
        let result = grep(&["zzz"], json!("hello"));
        assert_eq!(result.status, ExitCode::FAILURE);
        assert_eq!(result.value, Value::Null);
    }

    #[test]
    fn supports_invert_ignore_case_count_and_number() {
        assert_eq!(grep(&["-v", "o"], json!("foo\nbar")).value, json!("bar"));
        assert_eq!(grep(&["-i", "FOO"], json!("foo")).value, json!("foo"));
        assert_eq!(grep(&["-c", "o"], json!("foo\nbar\nboo")).value, json!(2));
        assert_eq!(
            grep(&["-n", "o"], json!("foo\nbar\nboo")).value,
            json!("1:foo\n3:boo")
        );
    }

    #[test]
    fn unsupported_flags_are_rejected_by_name() {
        let failure = run_test(TextStream::Grep, &["-o", "a"], json!("a"))
            .expect_err("only-matching is not implemented");
        assert!(format!("{failure:?}").contains("-o"), "{failure:?}");
    }

    #[test]
    fn the_e_flag_matches_with_the_regex_engine() {
        assert_eq!(
            grep(&["-E", "[0-9]"], json!("port 8080\nno digits")).value,
            json!("port 8080")
        );
        assert_eq!(
            grep(&["-E", "^ba(r|z)$"], json!("bar\nbaz\nbarn")).value,
            json!("bar\nbaz")
        );
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
            run_test(TextStream::Grep, &["[0-9]"], json!("a1")).expect_err("literal by default");
        let message = format!("{failure:?}");
        assert!(message.contains("literal text"), "{message}");
    }

    #[test]
    fn an_uncompilable_e_pattern_fails_rather_than_matching_nothing() {
        let failure = run_test(TextStream::Grep, &["-E", "a("], json!("a("))
            .expect_err("an unclosed group is not a literal");
        assert!(
            format!("{failure:?}").contains("closing ')'"),
            "{failure:?}"
        );
    }
}
