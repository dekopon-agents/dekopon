use clap::{Arg, ArgAction, Command};

use crate::builtins::{CommandFailure, parse_flags, text::lines::parse_count};

use super::Pattern;

#[derive(Clone, Copy)]
pub(crate) enum GrepMode {
    Lines,
    Count,
    Quiet,
}

pub(crate) struct GrepConfig {
    invert: bool,
    number: bool,
    pub(crate) mode: GrepMode,
    pub(crate) before: usize,
    pub(crate) after: usize,
    pattern: Pattern,
}

impl GrepConfig {
    pub(crate) fn parse(arguments: &[String]) -> Result<(Self, Option<String>), CommandFailure> {
        let mut grammar = Command::new("grep");
        for (id, short, long) in [
            ("invert", 'v', "invert-match"),
            ("ignore", 'i', "ignore-case"),
            ("count", 'c', "count"),
            ("number", 'n', "line-number"),
            ("extended", 'E', "extended-regexp"),
            ("quiet", 'q', "quiet"),
        ] {
            grammar = grammar.arg(
                Arg::new(id)
                    .short(short)
                    .long(long)
                    .action(ArgAction::SetTrue),
            );
        }
        for (id, short, long) in [
            ("after", 'A', "after-context"),
            ("before", 'B', "before-context"),
            ("context", 'C', "context"),
        ] {
            grammar = grammar.arg(Arg::new(id).short(short).long(long).num_args(1));
        }
        let matches = parse_flags(
            grammar
                .arg(Arg::new("pattern").required(true))
                .arg(Arg::new("input")),
            arguments,
            "grep",
            crate::builtins::Builtin::Grep.help(),
        )?;
        let pattern = matches
            .get_one::<String>("pattern")
            .ok_or_else(|| CommandFailure::usage("grep: a pattern is required"))?;
        let count = |id: &'static str| {
            matches
                .get_one::<String>(id)
                .map(|n| parse_count("grep", n))
                .transpose()
        };
        let context = count("context")?.unwrap_or(0);
        let common = matches.index_of("context");
        let side = |id: &'static str| -> Result<usize, CommandFailure> {
            Ok(if matches.index_of(id) > common {
                count(id)?.unwrap_or(0)
            } else {
                context
            })
        };
        Ok((
            Self {
                mode: match (matches.get_flag("quiet"), matches.get_flag("count")) {
                    (true, _) => GrepMode::Quiet,
                    (false, true) => GrepMode::Count,
                    (false, false) => GrepMode::Lines,
                },
                before: side("before")?,
                after: side("after")?,
                invert: matches.get_flag("invert"),
                number: matches.get_flag("number"),
                pattern: Pattern::compile(
                    "grep",
                    pattern,
                    matches.get_flag("ignore"),
                    matches.get_flag("extended"),
                )?,
            },
            matches.get_one::<String>("input").cloned(),
        ))
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
    use dekopon_process::ExitCode;
    use serde_json::{Value, json};

    use crate::builtins::{Builtin, CommandResult, text::stream::run_test};

    fn grep(arguments: &[&str], input: Value) -> CommandResult {
        run_test(Builtin::Grep, arguments, input).expect("grep runs")
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
        let failure = run_test(Builtin::Grep, &["-o", "a"], json!("a"))
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
            run_test(Builtin::Grep, &["[0-9]"], json!("a1")).expect_err("literal by default");
        let message = format!("{failure:?}");
        assert!(message.contains("literal text"), "{message}");
    }

    #[test]
    fn an_uncompilable_e_pattern_fails_rather_than_matching_nothing() {
        let failure = run_test(Builtin::Grep, &["-E", "a("], json!("a("))
            .expect_err("an unclosed group is not a literal");
        assert!(
            format!("{failure:?}").contains("closing ')'"),
            "{failure:?}"
        );
    }
}
