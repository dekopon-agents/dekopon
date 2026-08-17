//! `cut -d DELIM -f LIST` and `cut -c LIST`.

use serde_json::Value;

use crate::{
    builtins::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag},
    value::{from_lines, to_lines},
};

/// Selects delimited fields or character ranges from each line.
pub(crate) struct Cut;

impl Builtin for Cut {
    fn name(&self) -> &'static str {
        "cut"
    }

    fn run(
        &self,
        _context: &mut BuiltinContext<'_>,
        arguments: &[String],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let mut delimiter = '\t';
        let mut fields = None;
        let mut characters = None;

        let mut index = 0;
        while index < arguments.len() {
            let argument = arguments[index].as_str();
            match argument {
                "-d" | "--delimiter" => {
                    let value = take_value(arguments, &mut index, argument)?;
                    let mut value_characters = value.chars();
                    let Some(single) = value_characters.next() else {
                        return Err(CommandFailure::usage("cut: -d requires a delimiter"));
                    };
                    if value_characters.next().is_some() {
                        return Err(CommandFailure::usage(
                            "cut: -d accepts exactly one delimiter character",
                        ));
                    }
                    delimiter = single;
                }
                "-f" | "--fields" => {
                    let value = take_value(arguments, &mut index, argument)?;
                    fields = Some(Selection::parse("cut", &value)?);
                }
                "-c" | "--characters" => {
                    let value = take_value(arguments, &mut index, argument)?;
                    characters = Some(Selection::parse("cut", &value)?);
                }
                flag if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(unsupported_flag("cut", flag));
                }
                other => {
                    return Err(CommandFailure::usage(format!(
                        "cut: unexpected argument {other:?}; input arrives through a pipe"
                    )));
                }
            }
        }

        let selection = match (fields, characters) {
            (Some(_), Some(_)) => {
                return Err(CommandFailure::usage(
                    "cut: -f and -c are mutually exclusive",
                ));
            }
            (Some(fields), None) => Mode::Fields(fields),
            (None, Some(characters)) => Mode::Characters(characters),
            (None, None) => {
                return Err(CommandFailure::usage("cut: -f or -c is required"));
            }
        };

        let lines = to_lines(&input.unwrap_or(Value::Null))
            .into_iter()
            .map(|line| match &selection {
                Mode::Fields(selection) => {
                    let parts = line.split(delimiter).collect::<Vec<_>>();
                    // Real cut passes lines without the delimiter through unchanged.
                    if parts.len() == 1 {
                        return line;
                    }
                    selection
                        .select(&parts)
                        .into_iter()
                        .collect::<Vec<_>>()
                        .join(&delimiter.to_string())
                }
                Mode::Characters(selection) => {
                    let parts = line.chars().map(String::from).collect::<Vec<_>>();
                    let parts = parts.iter().map(String::as_str).collect::<Vec<_>>();
                    selection.select(&parts).concat()
                }
            })
            .collect::<Vec<_>>();

        Ok(CommandResult::value(from_lines(lines)))
    }
}

enum Mode {
    Fields(Selection),
    Characters(Selection),
}

/// A one-based `N`, `N-M`, `-M`, `N-`, comma-separated selection list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Selection {
    ranges: Vec<(usize, Option<usize>)>,
}

impl Selection {
    /// Parses a selection list.
    pub(crate) fn parse(command: &str, list: &str) -> Result<Self, CommandFailure> {
        let mut ranges = Vec::new();
        for entry in list.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                return Err(CommandFailure::usage(format!(
                    "{command}: empty entry in selection list {list:?}"
                )));
            }
            let parsed = match entry.split_once('-') {
                None => {
                    let position = parse_position(command, entry)?;
                    (position, Some(position))
                }
                Some(("", end)) => (1, Some(parse_position(command, end)?)),
                Some((start, "")) => (parse_position(command, start)?, None),
                Some((start, end)) => (
                    parse_position(command, start)?,
                    Some(parse_position(command, end)?),
                ),
            };
            if let (start, Some(end)) = parsed
                && start > end
            {
                return Err(CommandFailure::usage(format!(
                    "{command}: selection {entry:?} ends before it starts"
                )));
            }
            ranges.push(parsed);
        }
        Ok(Self { ranges })
    }

    /// Returns the selected parts in ascending order, without duplicates.
    pub(crate) fn select<'a>(&self, parts: &[&'a str]) -> Vec<&'a str> {
        let mut selected = Vec::new();
        for (position, part) in parts.iter().enumerate() {
            let one_based = position + 1;
            let included = self
                .ranges
                .iter()
                .any(|(start, end)| one_based >= *start && end.is_none_or(|end| one_based <= end));
            if included {
                selected.push(*part);
            }
        }
        selected
    }
}

fn parse_position(command: &str, text: &str) -> Result<usize, CommandFailure> {
    let position = text.trim().parse::<usize>().map_err(|_| {
        CommandFailure::usage(format!("{command}: {text:?} is not a positive position"))
    })?;
    if position == 0 {
        return Err(CommandFailure::usage(format!(
            "{command}: positions are one-based, so 0 is not valid"
        )));
    }
    Ok(position)
}

fn take_value(
    arguments: &[String],
    index: &mut usize,
    flag: &str,
) -> Result<String, CommandFailure> {
    let Some(value) = arguments.get(*index + 1) else {
        return Err(CommandFailure::usage(format!(
            "cut: {flag} requires a value"
        )));
    };
    *index += 2;
    Ok(value.clone())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::builtins::{CommandResult, test_support::run_builtin};

    use super::{Cut, Selection};

    fn cut(arguments: &[&str], input: Value) -> CommandResult {
        run_builtin(&Cut, arguments, Some(input)).expect("cut runs")
    }

    #[test]
    fn selects_single_fields() {
        assert_eq!(
            cut(&["-d", ":", "-f", "2"], json!("a:b:c")).value,
            json!("b")
        );
    }

    #[test]
    fn selects_ranges_and_lists() {
        assert_eq!(
            cut(&["-d", ":", "-f", "1,3"], json!("a:b:c:d")).value,
            json!("a:c")
        );
        assert_eq!(
            cut(&["-d", ":", "-f", "2-3"], json!("a:b:c:d")).value,
            json!("b:c")
        );
        assert_eq!(
            cut(&["-d", ":", "-f", "3-"], json!("a:b:c:d")).value,
            json!("c:d")
        );
        assert_eq!(
            cut(&["-d", ":", "-f", "-2"], json!("a:b:c:d")).value,
            json!("a:b")
        );
    }

    #[test]
    fn selects_characters() {
        assert_eq!(cut(&["-c", "1-3"], json!("abcdef")).value, json!("abc"));
    }

    #[test]
    fn lines_without_the_delimiter_pass_through() {
        assert_eq!(
            cut(&["-d", ":", "-f", "2"], json!("plain")).value,
            json!("plain")
        );
    }

    #[test]
    fn operates_over_arrays_of_lines() {
        assert_eq!(
            cut(&["-d", ",", "-f", "1"], json!(["a,b", "c,d"])).value,
            json!(["a", "c"])
        );
    }

    #[test]
    fn malformed_selections_are_rejected() {
        for list in ["0", "3-1", "x", "1,,2", ""] {
            assert!(
                Selection::parse("cut", list).is_err(),
                "{list:?} must be rejected"
            );
        }
        assert!(run_builtin(&Cut, &["-f"], Some(json!("a"))).is_err());
        assert!(run_builtin(&Cut, &[], Some(json!("a"))).is_err());
        assert!(run_builtin(&Cut, &["-f", "1", "-c", "1"], Some(json!("a"))).is_err());
    }
}
