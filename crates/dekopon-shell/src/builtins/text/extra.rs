use crate::interp::telemetry::StageObservation;
use dekopon_process::pipe::{PipeReader, ReadOutcome};
use dekopon_process::{ExitCode, RetainedBytes};
use std::cmp::Ordering;

use clap::{Arg, ArgAction, Command};

use crate::{
    builtins::{CommandFailure, parse_flags},
    limits::Budget,
};

use super::cut::Selection;

pub(crate) fn run_parsed(
    operation: Operation,
    reader: &mut PipeReader<StageObservation>,
    budget: &mut Budget,

    mut emit: impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
) -> Result<ExitCode, CommandFailure> {
    let count_chunks = match &operation {
        Operation::Wc { .. } => true,
        Operation::Cut { .. } | Operation::Uniq { .. } | Operation::Sort { .. } => false,
    };
    let mut state = State::new(operation);
    if count_chunks {
        while let ReadOutcome::Bytes(chunk) = reader.read(budget.tree())? {
            budget.charge_step()?;
            state.count_chunk(&chunk)?;
        }
    } else {
        let mut pending = Vec::new();
        let mut charges = Vec::new();
        while let ReadOutcome::Bytes(chunk) = reader.read(budget.tree())? {
            budget.charge_step()?;
            let mut rest = chunk.as_slice();
            while !rest.is_empty() {
                let end = rest.iter().position(|byte| *byte == b'\n').map(|at| at + 1);
                let part = &rest[..end.unwrap_or(rest.len())];
                charges.push(budget.charge_value_bytes(part.len() as u64)?);
                pending.extend_from_slice(part);
                rest = &rest[part.len()..];
                if end.is_some()
                    && !state.line(
                        std::mem::take(&mut pending),
                        std::mem::take(&mut charges),
                        budget,
                        &mut emit,
                    )?
                {
                    return Ok(ExitCode::SUCCESS);
                }
            }
        }
        if !pending.is_empty() {
            state.line(pending, charges, budget, &mut emit)?;
        }
    }
    state.finish(budget, &mut emit)?;
    Ok(ExitCode::SUCCESS)
}

pub(crate) enum Operation {
    Cut {
        delimiter: char,
        mode: CutMode,
    },
    Uniq {
        count: bool,
        duplicates: bool,
        unique: bool,
    },
    Wc {
        lines: bool,
        words: bool,
        bytes: bool,
    },
    Sort {
        reverse: bool,
        numeric: bool,
        unique: bool,
    },
}

pub(crate) enum CutMode {
    Fields(Selection),
    Characters(Selection),
}

pub(crate) fn cut(args: &[String]) -> Result<(Operation, Option<String>), CommandFailure> {
    let matches = flags(
        crate::builtins::Builtin::Cut,
        args,
        &[
            ("delimiter", 'd', "delimiter", true),
            ("fields", 'f', "fields", true),
            ("characters", 'c', "characters", true),
        ],
    )?;
    let value = |id| matches.get_one::<String>(id).map(String::as_str);
    let operation = {
        let delimiter = match value("delimiter") {
            None => '\t',
            Some(text) => {
                let mut chars = text.chars();
                let ch = chars
                    .next()
                    .ok_or_else(|| CommandFailure::usage("cut: -d requires a delimiter"))?;
                if chars.next().is_some() {
                    return Err(CommandFailure::usage(
                        "cut: -d accepts exactly one delimiter character",
                    ));
                }
                ch
            }
        };
        let fields = value("fields")
            .map(|v| Selection::parse("cut", v))
            .transpose()?;
        let characters = value("characters")
            .map(|v| Selection::parse("cut", v))
            .transpose()?;
        let mode = match (fields, characters) {
            (Some(_), Some(_)) => {
                return Err(CommandFailure::usage(
                    "cut: -f and -c are mutually exclusive",
                ));
            }
            (Some(fields), None) => CutMode::Fields(fields),
            (None, Some(characters)) => CutMode::Characters(characters),
            (None, None) => {
                return Err(CommandFailure::usage("cut: -f or -c is required"));
            }
        };
        Operation::Cut { delimiter, mode }
    };
    Ok((operation, matches.get_one::<String>("input").cloned()))
}
pub(crate) fn uniq(args: &[String]) -> Result<(Operation, Option<String>), CommandFailure> {
    let matches = flags(
        crate::builtins::Builtin::Uniq,
        args,
        &[
            ("count", 'c', "count", false),
            ("duplicates", 'd', "repeated", false),
            ("unique", 'u', "unique", false),
        ],
    )?;
    let flag = |id| matches.get_flag(id);
    let operation = {
        let (count, duplicates, unique) = (flag("count"), flag("duplicates"), flag("unique"));
        if duplicates && unique {
            return Err(CommandFailure::usage(
                "uniq: -d and -u are mutually exclusive",
            ));
        }
        Operation::Uniq {
            count,
            duplicates,
            unique,
        }
    };
    Ok((operation, matches.get_one::<String>("input").cloned()))
}
pub(crate) fn wc(args: &[String]) -> Result<(Operation, Option<String>), CommandFailure> {
    let matches = flags(
        crate::builtins::Builtin::Wc,
        args,
        &[
            ("lines", 'l', "lines", false),
            ("words", 'w', "words", false),
            ("bytes", 'c', "bytes", false),
        ],
    )?;
    let flag = |id| matches.get_flag(id);
    let operation = Operation::Wc {
        lines: flag("lines"),
        words: flag("words"),
        bytes: flag("bytes"),
    };
    Ok((operation, matches.get_one::<String>("input").cloned()))
}
pub(crate) fn sort(args: &[String]) -> Result<(Operation, Option<String>), CommandFailure> {
    let matches = flags(
        crate::builtins::Builtin::Sort,
        args,
        &[
            ("reverse", 'r', "reverse", false),
            ("numeric", 'n', "numeric-sort", false),
            ("unique", 'u', "unique", false),
        ],
    )?;
    let flag = |id| matches.get_flag(id);
    let operation = Operation::Sort {
        reverse: flag("reverse"),
        numeric: flag("numeric"),
        unique: flag("unique"),
    };
    Ok((operation, matches.get_one::<String>("input").cloned()))
}
fn flags(
    command: crate::builtins::Builtin,
    args: &[String],
    options: &[(&'static str, char, &'static str, bool)],
) -> Result<clap::ArgMatches, CommandFailure> {
    let mut grammar = Command::new(command.name());
    for &(id, short, long, value) in options {
        let mut arg = Arg::new(id).short(short).long(long);
        arg = if value {
            arg.num_args(1).allow_hyphen_values(true)
        } else {
            arg.action(ArgAction::SetTrue)
        };
        grammar = grammar.arg(arg);
    }
    parse_flags(
        grammar.arg(Arg::new("input")),
        args,
        command.name(),
        command.help(),
    )
}

struct State {
    operation: Operation,
    previous: Vec<u8>,
    previous_charges: Vec<RetainedBytes>,
    run: usize,
    sorted: Vec<(Vec<u8>, Vec<RetainedBytes>)>,
    lines: usize,
    words: usize,
    bytes: usize,
    in_word: bool,
    utf8: Vec<u8>,
    ended: bool,
}

impl State {
    fn new(operation: Operation) -> Self {
        Self {
            operation,
            previous: Vec::new(),
            previous_charges: Vec::new(),
            run: 0,
            sorted: Vec::new(),
            lines: 0,
            words: 0,
            bytes: 0,
            in_word: false,
            utf8: Vec::new(),
            ended: true,
        }
    }

    fn count_chunk(&mut self, chunk: &[u8]) -> Result<(), CommandFailure> {
        self.bytes = self.bytes.saturating_add(chunk.len());
        self.lines = self
            .lines
            .saturating_add(chunk.iter().filter(|byte| **byte == b'\n').count());
        self.ended = chunk.ends_with(b"\n");
        if matches!(
            self.operation,
            Operation::Wc {
                words: false,
                lines: true,
                ..
            } | Operation::Wc {
                words: false,
                bytes: true,
                ..
            }
        ) {
            return Ok(());
        }
        self.utf8.extend_from_slice(chunk);
        let complete = match std::str::from_utf8(&self.utf8) {
            Ok(_) => self.utf8.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => {
                return Err(CommandFailure::failed(
                    "standard input is not valid UTF-8 text",
                ));
            }
        };
        for ch in std::str::from_utf8(&self.utf8[..complete])
            .map_err(|_invalid_utf8| {
                CommandFailure::failed("standard input is not valid UTF-8 text")
            })?
            .chars()
        {
            if ch.is_whitespace() {
                self.in_word = false;
            } else if !self.in_word {
                self.words = self.words.saturating_add(1);
                self.in_word = true;
            }
        }
        self.utf8.drain(..complete);
        Ok(())
    }

    fn line(
        &mut self,
        bytes: Vec<u8>,
        charges: Vec<RetainedBytes>,
        budget: &mut Budget,

        emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<bool, CommandFailure> {
        let line = std::str::from_utf8(bytes.strip_suffix(b"\n").unwrap_or(&bytes)).map_err(
            |_invalid_utf8| CommandFailure::failed("standard input is not valid UTF-8 text"),
        )?;
        match &self.operation {
            Operation::Cut { delimiter, mode } => {
                if !emit_cut(line, *delimiter, mode, budget, emit)? {
                    return Ok(false);
                }
                if bytes.ends_with(b"\n") {
                    super::sed::emit_chunks(b"\n", budget, emit)
                } else {
                    Ok(true)
                }
            }
            Operation::Uniq { .. } => {
                if self.run > 0
                    && self.previous.strip_suffix(b"\n").unwrap_or(&self.previous)
                        != bytes.strip_suffix(b"\n").unwrap_or(&bytes)
                    && !self.flush_run(budget, emit)?
                {
                    return Ok(false);
                }
                if self.run == 0 {
                    self.previous = bytes;
                    self.previous_charges = charges;
                }
                self.run = self.run.saturating_add(1);
                Ok(true)
            }
            Operation::Sort { .. } => {
                self.ended = bytes.ends_with(b"\n");
                self.sorted.push((bytes, charges));
                Ok(true)
            }
            Operation::Wc { .. } => unreachable!(),
        }
    }

    fn flush_run(
        &mut self,
        budget: &mut Budget,

        emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<bool, CommandFailure> {
        let Operation::Uniq {
            count,
            duplicates,
            unique,
        } = self.operation
        else {
            return Ok(true);
        };
        if (!duplicates || self.run > 1) && (!unique || self.run == 1) {
            if count && !super::sed::emit_chunks(format!("{} ", self.run).as_bytes(), budget, emit)?
            {
                return Ok(false);
            }
            if !super::sed::emit_chunks(&self.previous, budget, emit)? {
                return Ok(false);
            }
        }
        drop(std::mem::take(&mut self.previous));
        self.previous_charges.clear();
        self.run = 0;
        Ok(true)
    }

    fn finish(
        &mut self,
        budget: &mut Budget,

        emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<(), CommandFailure> {
        match self.operation {
            Operation::Uniq { .. } => {
                self.flush_run(budget, emit)?;
            }
            Operation::Sort {
                reverse,
                numeric,
                unique,
            } => {
                self.sorted.sort_by(|(left, _), (right, _)| {
                    let left = left.strip_suffix(b"\n").unwrap_or(left);
                    let right = right.strip_suffix(b"\n").unwrap_or(right);
                    let compare = if numeric {
                        match (number(left), number(right)) {
                            (Some(a), Some(b)) => a.total_cmp(&b),
                            (Some(_), None) => Ordering::Greater,
                            (None, Some(_)) => Ordering::Less,
                            (None, None) => left.cmp(right),
                        }
                    } else {
                        left.cmp(right)
                    };
                    if reverse { compare.reverse() } else { compare }
                });
                let mut last: Option<&[u8]> = None;
                for (line, _charge) in &self.sorted {
                    budget.charge_step()?;
                    let line = line.strip_suffix(b"\n").unwrap_or(line);
                    if unique && last == Some(line) {
                        continue;
                    }
                    if last.is_some() && !super::sed::emit_chunks(b"\n", budget, emit)? {
                        return Ok(());
                    }
                    if !super::sed::emit_chunks(line, budget, emit)? {
                        return Ok(());
                    }
                    last = Some(line);
                }
                if last.is_some() && self.ended {
                    super::sed::emit_chunks(b"\n", budget, emit)?;
                }
            }
            Operation::Wc {
                lines,
                words,
                bytes,
            } => {
                if !self.utf8.is_empty() {
                    return Err(CommandFailure::failed(
                        "standard input is not valid UTF-8 text",
                    ));
                }
                let line_count = self.lines + usize::from(self.bytes > 0 && !self.ended);
                let selected = [
                    (lines, line_count),
                    (words, self.words),
                    (bytes, self.bytes),
                ];
                let chosen: Vec<_> = selected
                    .iter()
                    .filter(|(enabled, _)| *enabled)
                    .map(|(_, count)| *count)
                    .collect();
                let result = match chosen.as_slice() {
                    [] => {
                        serde_json::json!({"lines": line_count, "words": self.words, "bytes": self.bytes})
                    }
                    [one] => serde_json::json!(one),
                    _ => serde_json::json!(chosen),
                };
                super::sed::emit_chunks(format!("{result}\n").as_bytes(), budget, emit)?;
            }
            Operation::Cut { .. } => {}
        }
        Ok(())
    }
}

fn emit_cut(
    line: &str,
    delimiter: char,
    mode: &CutMode,
    budget: &mut Budget,

    emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
) -> Result<bool, CommandFailure> {
    match mode {
        CutMode::Fields(selection) => {
            if !line.contains(delimiter) {
                return super::sed::emit_chunks(line.as_bytes(), budget, emit);
            }
            let mut first = true;
            let mut encoded = [0; 4];
            let mut cursor = selection.cursor();
            for (index, field) in line.split(delimiter).enumerate() {
                if !cursor.includes(index + 1) {
                    continue;
                }
                if !first
                    && !super::sed::emit_chunks(
                        delimiter.encode_utf8(&mut encoded).as_bytes(),
                        budget,
                        emit,
                    )?
                {
                    return Ok(false);
                }
                if !super::sed::emit_chunks(field.as_bytes(), budget, emit)? {
                    return Ok(false);
                }
                first = false;
            }
        }
        CutMode::Characters(selection) => {
            let mut cursor = selection.cursor();
            for (index, (start, character)) in line.char_indices().enumerate() {
                if cursor.includes(index + 1)
                    && !super::sed::emit_chunks(
                        &line.as_bytes()[start..start + character.len_utf8()],
                        budget,
                        emit,
                    )?
                {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

fn number(line: &[u8]) -> Option<f64> {
    std::str::from_utf8(line.strip_suffix(b"\n").unwrap_or(line))
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| !value.is_nan())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dekopon_process::Limits;

    #[test]
    fn completed_uniq_run_drops_its_allocation_before_refunding() {
        let mut budget = Budget::start(Limits {
            max_value_bytes: 3,
            ..Limits::default()
        });
        let mut state = State::new(Operation::Uniq {
            count: false,
            duplicates: false,
            unique: false,
        });
        let charge = budget.charge_value_bytes(3).expect("one line fits");
        state.previous = b"aa\n".to_vec();
        state.previous_charges.push(charge);
        state.run = 1;
        assert!(
            state
                .flush_run(&mut budget, &mut |_| Ok(true))
                .expect("run emits")
        );
        assert_eq!(state.previous.capacity(), 0);
        assert!(budget.charge_value_bytes(3).is_ok());
    }
}
