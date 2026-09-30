use std::cmp::Ordering;

use crate::{
    CapabilityInvoker, ExitCode, RetainedBytes,
    builtins::{CommandFailure, unsupported_flag},
    limits::Budget,
    pipe::{PipeReader, ReadOutcome},
};

use super::cut::Selection;

#[derive(Clone, Copy)]
pub(crate) enum ExtraStream {
    Cut,
    Uniq,
    Wc,
    Sort,
}

impl ExtraStream {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Cut => "cut",
            Self::Uniq => "uniq",
            Self::Wc => "wc",
            Self::Sort => "sort",
        }
    }

    pub(crate) fn help(self) -> &'static str {
        match self {
            Self::Cut => "-d -f -c",
            Self::Uniq => "-c -d -u",
            Self::Wc => "-l -w -c",
            Self::Sort => "-r -n -u",
        }
    }

    pub(crate) fn run(
        self,
        arguments: &[String],
        reader: &mut PipeReader,
        budget: &mut Budget,
        invoker: &dyn CapabilityInvoker,
        mut emit: impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<ExitCode, CommandFailure> {
        let operation = Operation::parse(self, arguments)?;
        let mut state = State::new(operation);
        if matches!(self, Self::Wc) {
            while let ReadOutcome::Bytes(chunk) = reader.read(budget, invoker)? {
                budget.charge_step_with(invoker)?;
                state.count_chunk(&chunk)?;
            }
        } else {
            let mut pending = Vec::new();
            let mut charges = Vec::new();
            while let ReadOutcome::Bytes(chunk) = reader.read(budget, invoker)? {
                budget.charge_step_with(invoker)?;
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
                            invoker,
                            &mut emit,
                        )?
                    {
                        return Ok(ExitCode::SUCCESS);
                    }
                }
            }
            if !pending.is_empty() {
                state.line(pending, charges, budget, invoker, &mut emit)?;
            }
        }
        state.finish(budget, invoker, &mut emit)?;
        Ok(ExitCode::SUCCESS)
    }
}

enum Operation {
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

enum CutMode {
    Fields(Selection),
    Characters(Selection),
}

impl Operation {
    fn parse(command: ExtraStream, args: &[String]) -> Result<Self, CommandFailure> {
        let mut delimiter = '\t';
        let (mut fields, mut characters) = (None, None);
        let (mut count, mut duplicates, mut unique) = (false, false, false);
        let (mut lines, mut words, mut bytes) = (false, false, false);
        let (mut reverse, mut numeric) = (false, false);
        let mut i = 0;
        while i < args.len() {
            let flag = args[i].as_str();
            match (command, flag) {
                (
                    ExtraStream::Cut,
                    "-d" | "--delimiter" | "-f" | "--fields" | "-c" | "--characters",
                ) => {
                    let value = args.get(i + 1).ok_or_else(|| {
                        CommandFailure::usage(format!("cut: {flag} requires a value"))
                    })?;
                    i += 1;
                    match flag {
                        "-d" | "--delimiter" => {
                            let mut chars = value.chars();
                            delimiter = chars.next().ok_or_else(|| {
                                CommandFailure::usage("cut: -d requires a delimiter")
                            })?;
                            if chars.next().is_some() {
                                return Err(CommandFailure::usage(
                                    "cut: -d accepts exactly one delimiter character",
                                ));
                            }
                        }
                        "-f" | "--fields" => fields = Some(Selection::parse("cut", value)?),
                        "-c" | "--characters" => characters = Some(Selection::parse("cut", value)?),
                        _ => unreachable!(),
                    }
                }
                (ExtraStream::Uniq, "-c" | "--count") => count = true,
                (ExtraStream::Uniq, "-d" | "--repeated") => duplicates = true,
                (ExtraStream::Uniq, "-u" | "--unique") => unique = true,
                (ExtraStream::Wc, "-l" | "--lines") => lines = true,
                (ExtraStream::Wc, "-w" | "--words") => words = true,
                (ExtraStream::Wc, "-c" | "--bytes") => bytes = true,
                (ExtraStream::Sort, "-r" | "--reverse") => reverse = true,
                (ExtraStream::Sort, "-n" | "--numeric-sort") => numeric = true,
                (ExtraStream::Sort, "-u" | "--unique") => unique = true,
                (_, flag) if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(unsupported_flag(command.name(), flag, command.help()));
                }
                (_, other) => {
                    return Err(CommandFailure::usage(format!(
                        "{}: unexpected argument {other:?}; input arrives through a pipe",
                        command.name()
                    )));
                }
            }
            i += 1;
        }
        Ok(match command {
            ExtraStream::Cut => {
                let mode = match (fields, characters) {
                    (Some(_), Some(_)) => {
                        return Err(CommandFailure::usage(
                            "cut: -f and -c are mutually exclusive",
                        ));
                    }
                    (Some(fields), None) => CutMode::Fields(fields),
                    (None, Some(characters)) => CutMode::Characters(characters),
                    (None, None) => return Err(CommandFailure::usage("cut: -f or -c is required")),
                };
                Self::Cut { delimiter, mode }
            }
            ExtraStream::Uniq => {
                if duplicates && unique {
                    return Err(CommandFailure::usage(
                        "uniq: -d and -u are mutually exclusive",
                    ));
                }
                Self::Uniq {
                    count,
                    duplicates,
                    unique,
                }
            }
            ExtraStream::Wc => Self::Wc {
                lines,
                words,
                bytes,
            },
            ExtraStream::Sort => Self::Sort {
                reverse,
                numeric,
                unique,
            },
        })
    }
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
        invoker: &dyn CapabilityInvoker,
        emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<bool, CommandFailure> {
        let line = std::str::from_utf8(bytes.strip_suffix(b"\n").unwrap_or(&bytes)).map_err(
            |_invalid_utf8| CommandFailure::failed("standard input is not valid UTF-8 text"),
        )?;
        match &self.operation {
            Operation::Cut { delimiter, mode } => {
                if !emit_cut(line, *delimiter, mode, budget, invoker, emit)? {
                    return Ok(false);
                }
                if bytes.ends_with(b"\n") {
                    super::sed::emit_chunks(b"\n", budget, invoker, emit)
                } else {
                    Ok(true)
                }
            }
            Operation::Uniq { .. } => {
                if self.run > 0
                    && self.previous.strip_suffix(b"\n").unwrap_or(&self.previous)
                        != bytes.strip_suffix(b"\n").unwrap_or(&bytes)
                    && !self.flush_run(budget, invoker, emit)?
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
        invoker: &dyn CapabilityInvoker,
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
            if count
                && !super::sed::emit_chunks(
                    format!("{} ", self.run).as_bytes(),
                    budget,
                    invoker,
                    emit,
                )?
            {
                return Ok(false);
            }
            if !super::sed::emit_chunks(&self.previous, budget, invoker, emit)? {
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
        invoker: &dyn CapabilityInvoker,
        emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<(), CommandFailure> {
        match self.operation {
            Operation::Uniq { .. } => {
                self.flush_run(budget, invoker, emit)?;
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
                    budget.charge_step_with(invoker)?;
                    let line = line.strip_suffix(b"\n").unwrap_or(line);
                    if unique && last == Some(line) {
                        continue;
                    }
                    if last.is_some() && !super::sed::emit_chunks(b"\n", budget, invoker, emit)? {
                        return Ok(());
                    }
                    if !super::sed::emit_chunks(line, budget, invoker, emit)? {
                        return Ok(());
                    }
                    last = Some(line);
                }
                if last.is_some() && self.ended {
                    super::sed::emit_chunks(b"\n", budget, invoker, emit)?;
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
                super::sed::emit_chunks(format!("{result}\n").as_bytes(), budget, invoker, emit)?;
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
    invoker: &dyn CapabilityInvoker,
    emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
) -> Result<bool, CommandFailure> {
    match mode {
        CutMode::Fields(selection) => {
            if !line.contains(delimiter) {
                return super::sed::emit_chunks(line.as_bytes(), budget, invoker, emit);
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
                        invoker,
                        emit,
                    )?
                {
                    return Ok(false);
                }
                if !super::sed::emit_chunks(field.as_bytes(), budget, invoker, emit)? {
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
                        invoker,
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
    use crate::{builtins::test_support::NoCapabilities, limits::Limits};

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
                .flush_run(&mut budget, &NoCapabilities, &mut |_| Ok(true))
                .expect("run emits")
        );
        assert_eq!(state.previous.capacity(), 0);
        assert!(budget.charge_value_bytes(3).is_ok());
    }
}
