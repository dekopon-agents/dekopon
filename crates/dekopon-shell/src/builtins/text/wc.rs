//! `wc [-l] [-w] [-c]`.

use serde_json::{Value, json};

use crate::{
    builtins::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag},
    value::{to_lines, to_text},
};

/// Counts lines, words, or bytes.
///
/// With exactly one flag the result is a bare number so `| wc -l` composes with arithmetic and
/// `test`. With no flag it is an object carrying all three counts, which stays JSON-native instead
/// of forcing a script to parse columns.
pub(crate) struct Wc;

impl Builtin for Wc {
    fn name(&self) -> &'static str {
        "wc"
    }

    fn run(
        &self,
        _context: &mut BuiltinContext<'_>,
        arguments: &[String],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let mut lines_flag = false;
        let mut words_flag = false;
        let mut bytes_flag = false;

        for argument in arguments {
            match argument.as_str() {
                "-l" | "--lines" => lines_flag = true,
                "-w" | "--words" => words_flag = true,
                "-c" | "--bytes" => bytes_flag = true,
                flag if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(unsupported_flag("wc", flag));
                }
                other => {
                    return Err(CommandFailure::usage(format!(
                        "wc: unexpected argument {other:?}; input arrives through a pipe"
                    )));
                }
            }
        }

        let input = input.unwrap_or(Value::Null);
        let lines = to_lines(&input).len();
        let text = to_text(&input);
        let words = text.split_whitespace().count();
        let bytes = text.len();

        let selected = [
            (lines_flag, lines),
            (words_flag, words),
            (bytes_flag, bytes),
        ];
        let chosen = selected
            .iter()
            .filter(|(enabled, _)| *enabled)
            .map(|(_, count)| *count)
            .collect::<Vec<_>>();

        let value = match chosen.as_slice() {
            [] => json!({"lines": lines, "words": words, "bytes": bytes}),
            [single] => Value::from(*single),
            several => Value::Array(several.iter().copied().map(Value::from).collect()),
        };
        Ok(CommandResult::value(value))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::builtins::{CommandResult, test_support::run_builtin};

    use super::Wc;

    fn wc(arguments: &[&str], input: Value) -> CommandResult {
        run_builtin(&Wc, arguments, Some(input)).expect("wc runs")
    }

    #[test]
    fn a_single_flag_yields_a_bare_number() {
        assert_eq!(wc(&["-l"], json!("a\nb\nc")).value, json!(3));
        assert_eq!(wc(&["-w"], json!("one two three")).value, json!(3));
        assert_eq!(wc(&["-c"], json!("abcd")).value, json!(4));
    }

    #[test]
    fn no_flag_yields_every_count() {
        assert_eq!(
            wc(&[], json!("a bb\nccc")).value,
            json!({"lines": 2, "words": 3, "bytes": 8})
        );
    }

    #[test]
    fn counts_json_arrays_as_line_lists() {
        assert_eq!(wc(&["-l"], json!(["a", "b"])).value, json!(2));
        assert_eq!(wc(&["-l"], Value::Null).value, json!(0));
    }

    #[test]
    fn several_flags_yield_an_ordered_array() {
        assert_eq!(wc(&["-l", "-w"], json!("a b\nc")).value, json!([2, 3]));
    }

    #[test]
    fn unsupported_flags_are_rejected_by_name() {
        let failure = run_builtin(&Wc, &["-m"], Some(json!("a")))
            .expect_err("character counting is not implemented");
        assert!(format!("{failure:?}").contains("-m"), "{failure:?}");
    }
}
