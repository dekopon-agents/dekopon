//! `uniq [-c] [-d] [-u]`.

use serde_json::Value;

use crate::{
    builtins::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag},
    value::{from_lines, to_lines},
};

/// Collapses runs of adjacent identical lines, like real `uniq`.
pub(crate) struct Uniq;

impl Builtin for Uniq {
    fn name(&self) -> &'static str {
        "uniq"
    }

    fn run(
        &self,
        _context: &mut BuiltinContext<'_>,
        arguments: &[String],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let mut count = false;
        let mut duplicates_only = false;
        let mut unique_only = false;

        for argument in arguments {
            match argument.as_str() {
                "-c" | "--count" => count = true,
                "-d" | "--repeated" => duplicates_only = true,
                "-u" | "--unique" => unique_only = true,
                flag if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(unsupported_flag("uniq", flag));
                }
                other => {
                    return Err(CommandFailure::usage(format!(
                        "uniq: unexpected argument {other:?}; input arrives through a pipe"
                    )));
                }
            }
        }
        if duplicates_only && unique_only {
            return Err(CommandFailure::usage(
                "uniq: -d and -u are mutually exclusive",
            ));
        }

        let mut runs: Vec<(String, usize)> = Vec::new();
        for line in to_lines(&input.unwrap_or(Value::Null)) {
            match runs.last_mut() {
                Some((previous, occurrences)) if *previous == line => *occurrences += 1,
                _ => runs.push((line, 1)),
            }
        }

        let lines = runs
            .into_iter()
            .filter(|(_, occurrences)| {
                if duplicates_only {
                    return *occurrences > 1;
                }
                if unique_only {
                    return *occurrences == 1;
                }
                true
            })
            .map(|(line, occurrences)| {
                if count {
                    format!("{occurrences} {line}")
                } else {
                    line
                }
            })
            .collect::<Vec<_>>();

        Ok(CommandResult::value(from_lines(lines)))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::builtins::{CommandResult, test_support::run_builtin};

    use super::Uniq;

    fn uniq(arguments: &[&str], input: Value) -> CommandResult {
        run_builtin(&Uniq, arguments, Some(input)).expect("uniq runs")
    }

    #[test]
    fn collapses_only_adjacent_duplicates() {
        assert_eq!(
            uniq(&[], json!(["a", "a", "b", "a"])).value,
            json!(["a", "b", "a"])
        );
    }

    #[test]
    fn counts_occurrences() {
        assert_eq!(
            uniq(&["-c"], json!(["a", "a", "b"])).value,
            json!(["2 a", "1 b"])
        );
    }

    #[test]
    fn selects_duplicates_or_singletons() {
        assert_eq!(uniq(&["-d"], json!(["a", "a", "b"])).value, json!("a"));
        assert_eq!(uniq(&["-u"], json!(["a", "a", "b"])).value, json!("b"));
    }

    #[test]
    fn conflicting_and_unsupported_flags_are_rejected() {
        assert!(run_builtin(&Uniq, &["-d", "-u"], Some(json!("a"))).is_err());
        let failure = run_builtin(&Uniq, &["-i"], Some(json!("a")))
            .expect_err("case folding is not implemented");
        assert!(format!("{failure:?}").contains("-i"), "{failure:?}");
    }
}
