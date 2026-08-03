//! `sort [-r] [-n] [-u]`.

use serde_json::Value;

use crate::{
    builtins::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag},
    value::{from_lines, to_lines},
};

/// Sorts lines lexicographically, or numerically with `-n`.
pub(crate) struct Sort;

impl Builtin for Sort {
    fn name(&self) -> &'static str {
        "sort"
    }

    fn run(
        &self,
        _context: &mut BuiltinContext<'_>,
        arguments: &[String],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let mut reverse = false;
        let mut numeric = false;
        let mut unique = false;

        for argument in arguments {
            match argument.as_str() {
                "-r" | "--reverse" => reverse = true,
                "-n" | "--numeric-sort" => numeric = true,
                "-u" | "--unique" => unique = true,
                flag if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(unsupported_flag("sort", flag));
                }
                other => {
                    return Err(CommandFailure::usage(format!(
                        "sort: unexpected argument {other:?}; input arrives through a pipe"
                    )));
                }
            }
        }

        let mut lines = to_lines(&input.unwrap_or(Value::Null));
        if numeric {
            // Non-numeric lines sort before numeric ones, matching coreutils closely enough that a
            // mixed list stays stable and predictable. NaN counts as non-numeric on purpose: a
            // comparator that calls NaN equal to every number is not a strict weak ordering, and a
            // single `nan` line — ordinary content in real capability output — left the whole list
            // in input order with no diagnostic.
            lines.sort_by(
                |left, right| match (numeric_key(left), numeric_key(right)) {
                    (Some(left_number), Some(right_number)) => left_number.total_cmp(&right_number),
                    (Some(_), None) => std::cmp::Ordering::Greater,
                    (None, Some(_)) => std::cmp::Ordering::Less,
                    (None, None) => left.cmp(right),
                },
            );
        } else {
            lines.sort();
        }
        if reverse {
            lines.reverse();
        }
        if unique {
            lines.dedup();
        }

        Ok(CommandResult::value(from_lines(lines)))
    }
}

/// Reads one line as a sort key for `-n`, rejecting NaN so the comparator stays total.
fn numeric_key(line: &str) -> Option<f64> {
    line.trim()
        .parse::<f64>()
        .ok()
        .filter(|number| !number.is_nan())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::builtins::{CommandResult, test_support::run_builtin};

    use super::Sort;

    fn sort(arguments: &[&str], input: Value) -> CommandResult {
        run_builtin(&Sort, arguments, Some(input)).expect("sort runs")
    }

    #[test]
    fn sorts_lexicographically_by_default() {
        assert_eq!(
            sort(&[], json!(["pear", "apple", "fig"])).value,
            json!(["apple", "fig", "pear"])
        );
    }

    #[test]
    fn numeric_sorting_orders_by_value_not_text() {
        assert_eq!(
            sort(&["-n"], json!(["10", "9", "100"])).value,
            json!(["9", "10", "100"])
        );
        assert_eq!(
            sort(&[], json!(["10", "9", "100"])).value,
            json!(["10", "100", "9"])
        );
    }

    #[test]
    fn reverse_and_unique_compose() {
        assert_eq!(
            sort(&["-r"], json!(["a", "c", "b"])).value,
            json!(["c", "b", "a"])
        );
        assert_eq!(
            sort(&["-u"], json!(["b", "a", "b"])).value,
            json!(["a", "b"])
        );
        assert_eq!(
            sort(&["-n", "-r", "-u"], json!(["2", "1", "2"])).value,
            json!(["2", "1"])
        );
    }

    #[test]
    fn a_nan_line_cannot_leave_the_list_unsorted() {
        // `nan` parses as a float but compares equal to nothing, so treating it as a number made
        // the comparator inconsistent and `sort -n` silently returned its input untouched.
        assert_eq!(
            sort(&["-n"], json!(["5", "nan", "3", "NaN", "1"])).value,
            json!(["NaN", "nan", "1", "3", "5"])
        );
    }

    #[test]
    fn accepts_newline_separated_text() {
        assert_eq!(sort(&[], json!("b\na")).value, json!(["a", "b"]));
    }

    #[test]
    fn unsupported_flags_are_rejected_by_name() {
        let failure = run_builtin(&Sort, &["-k", "2"], Some(json!("a")))
            .expect_err("key sorting is not implemented");
        assert!(format!("{failure:?}").contains("-k"), "{failure:?}");
    }
}
