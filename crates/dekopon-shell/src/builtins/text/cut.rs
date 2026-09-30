#[cfg(test)]
use serde_json::Value;

use crate::builtins::CommandFailure;
#[cfg(test)]
use crate::{
    builtins::{Builtin, BuiltinContext, CommandResult, unsupported_flag},
    value::to_lines,
};

#[cfg(test)]
const HELP: &str = "-d -f -c";

#[cfg(test)]
pub(crate) struct Cut;

#[cfg(test)]
impl Builtin for Cut {
    fn name(&self) -> &'static str {
        "cut"
    }

    fn help(&self) -> &'static str {
        HELP
    }

    fn reads_stdin(&self) -> bool {
        true
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
                    return Err(unsupported_flag("cut", flag, HELP));
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

        Ok(CommandResult::lines(lines))
    }
}

#[cfg(test)]
enum Mode {
    Fields(Selection),
    Characters(Selection),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Selection {
    ranges: Vec<(usize, Option<usize>)>,
}

impl Selection {
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
        ranges.sort_unstable_by_key(|(start, _)| *start);
        let mut merged: Vec<(usize, Option<usize>)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            if let Some((_, previous_end)) = merged.last_mut()
                && previous_end.is_none_or(|last| start <= last.saturating_add(1))
            {
                *previous_end = match (*previous_end, end) {
                    (None, _) | (_, None) => None,
                    (Some(left), Some(right)) => Some(left.max(right)),
                };
            } else {
                merged.push((start, end));
            }
        }
        Ok(Self { ranges: merged })
    }

    pub(crate) fn cursor(&self) -> SelectionCursor<'_> {
        SelectionCursor {
            ranges: &self.ranges,
            next: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn select<'a>(&self, parts: &[&'a str]) -> Vec<&'a str> {
        let mut cursor = self.cursor();
        parts
            .iter()
            .enumerate()
            .filter(|(position, _)| cursor.includes(position + 1))
            .map(|(_, part)| *part)
            .collect()
    }
}

pub(crate) struct SelectionCursor<'a> {
    ranges: &'a [(usize, Option<usize>)],
    next: usize,
}

impl SelectionCursor<'_> {
    pub(crate) fn includes(&mut self, one_based: usize) -> bool {
        while let Some((_, Some(end))) = self.ranges.get(self.next)
            && one_based > *end
        {
            self.next += 1;
        }
        self.ranges.get(self.next).is_some_and(|(start, end)| {
            one_based >= *start && end.is_none_or(|end| one_based <= end)
        })
    }
}

#[allow(
    clippy::map_err_ignore,
    reason = "ParseIntError separates only empty, non-digit, and overflow for a field spec the \
              message quotes back in full; the zero check below is what distinguishes the one \
              rejection an operator is likely to hit"
)]
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

#[cfg(test)]
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
            json!("a\nc")
        );
    }

    #[test]
    fn unordered_overlapping_ranges_merge_and_a_cursor_advances_once() {
        let selection = Selection::parse("cut", "9-,3-5,1-2,4-8").expect("valid ranges");
        assert_eq!(selection.ranges, vec![(1, None)]);
        let mut cursor = selection.cursor();
        for position in 1..1000 {
            assert!(cursor.includes(position));
        }
        let selection = Selection::parse("cut", "8,2,5-6").expect("valid gaps");
        let mut cursor = selection.cursor();
        let selected = (1..=9)
            .filter(|position| cursor.includes(*position))
            .collect::<Vec<_>>();
        assert_eq!(selected, vec![2, 5, 6, 8]);
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
