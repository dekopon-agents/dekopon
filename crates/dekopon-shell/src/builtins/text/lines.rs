use std::collections::VecDeque;

use clap::{Arg, Command};

use crate::{
    CapabilityInvoker, RetainedBytes,
    builtins::{CommandFailure, parse_flags, unsupported_flag},
    limits::Budget,
    pipe::{PipeReader, ReadOutcome},
};

#[derive(Clone, Copy)]
pub(crate) enum LineCommand {
    Head,
    Tail,
}

pub(crate) enum Selection {
    First(usize),
    FirstBytes(usize),
    Last(usize),
    From(usize),
}

pub(crate) fn parse_count(name: &str, digits: &str) -> Result<usize, CommandFailure> {
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CommandFailure::usage(format!(
            "{name}: count must be nonnegative decimal digits"
        )));
    }
    digits
        .parse::<usize>()
        .map_err(|_overflow| CommandFailure::usage(format!("{name}: count exceeds platform size")))
}

impl LineCommand {
    pub(crate) fn lookup(word: &str) -> Option<Self> {
        match word {
            "head" => Some(Self::Head),
            "tail" => Some(Self::Tail),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Head => "head",
            Self::Tail => "tail",
        }
    }

    pub(crate) fn help(self) -> &'static str {
        match self {
            Self::Head => "-n N --lines N -N -c N --bytes N",
            Self::Tail => "-n N --lines N -N -n +N",
        }
    }

    pub(crate) fn selection(
        self,
        arguments: &[String],
    ) -> Result<(Selection, Option<String>), CommandFailure> {
        let name = self.name();
        if let Some(first) = arguments.first()
            && first.starts_with('+')
        {
            return Err(unsupported_flag(name, first, self.help()));
        }
        let mut rewritten = arguments.to_vec();
        if let Some(first) = rewritten.first()
            && first.starts_with('-')
            && !first[1..].is_empty()
            && first[1..].bytes().all(|byte| byte.is_ascii_digit())
        {
            rewritten.splice(..1, ["-n".to_owned(), first[1..].to_owned()]);
        }
        let mut grammar = Command::new(name).arg(
            Arg::new("lines")
                .short('n')
                .long("lines")
                .num_args(1)
                .allow_hyphen_values(true),
        );
        if let Self::Head = self {
            grammar = grammar.arg(Arg::new("bytes").short('c').long("bytes").num_args(1));
        }
        let matches = parse_flags(
            grammar.arg(Arg::new("input")),
            &rewritten,
            name,
            self.help(),
        )?;
        let input = matches.get_one::<String>("input").cloned();
        if let Some(bytes) = if let Self::Head = self {
            matches.get_one::<String>("bytes")
        } else {
            None
        } {
            let count = parse_count(name, bytes)?;
            if matches.contains_id("lines") {
                return Err(CommandFailure::usage("head: -c and -n cannot be combined"));
            }
            return Ok((Selection::FirstBytes(count), input));
        }
        let number = matches
            .get_one::<String>("lines")
            .map(String::as_str)
            .unwrap_or("10");
        let (from, digits) = match (self, number.strip_prefix('+')) {
            (Self::Tail, Some(digits)) => (true, digits),
            _ => (false, number),
        };
        let count = parse_count(name, digits)?;
        Ok((
            match (self, from) {
                (Self::Head, _) => Selection::First(count),
                (Self::Tail, true) => Selection::From(count.max(1)),
                (Self::Tail, false) => Selection::Last(count),
            },
            input,
        ))
    }

    pub(crate) fn run(
        self,
        selection: Selection,
        reader: &mut PipeReader,
        budget: &mut Budget,
        invoker: &dyn CapabilityInvoker,
        mut emit: impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<(), CommandFailure> {
        if matches!(
            selection,
            Selection::First(0) | Selection::FirstBytes(0) | Selection::Last(0)
        ) {
            return Ok(());
        }
        if let Selection::FirstBytes(mut remaining) = selection {
            while let ReadOutcome::Bytes(chunk) = reader.read(budget, invoker)? {
                budget.charge_step_with(invoker)?;
                let n = remaining.min(chunk.len());
                if !emit(&chunk[..n])? || n == remaining {
                    return Ok(());
                }
                remaining -= n;
            }
            return Ok(());
        }
        let mut pending = Vec::new();
        let mut pending_charges = Vec::new();
        let mut kept: VecDeque<(Vec<u8>, Vec<RetainedBytes>)> = VecDeque::new();
        let mut line_number = 0usize;
        while let ReadOutcome::Bytes(chunk) = reader.read(budget, invoker)? {
            budget.charge_step_with(invoker)?;
            let mut rest = chunk.as_slice();
            while !rest.is_empty() {
                let end = rest.iter().position(|byte| *byte == b'\n').map(|at| at + 1);
                let part = &rest[..end.unwrap_or(rest.len())];
                if let Selection::From(start) = selection {
                    if line_number.saturating_add(1) >= start && !emit(part)? {
                        return Ok(());
                    }
                    if end.is_some() {
                        line_number = line_number.saturating_add(1);
                    }
                    rest = &rest[part.len()..];
                    continue;
                }
                if let Selection::Last(count) = selection
                    && pending.is_empty()
                    && kept.len() == count
                {
                    kept.pop_front();
                }
                let charge = budget.charge_value_bytes(part.len() as u64)?;
                pending.extend_from_slice(part);
                pending_charges.push(charge);
                rest = &rest[part.len()..];
                if end.is_some() {
                    line_number = line_number.saturating_add(1);
                    match selection {
                        Selection::First(count) => {
                            if !emit(&pending)? || line_number >= count {
                                return Ok(());
                            }
                        }
                        Selection::Last(count) => {
                            if count > 0 {
                                kept.push_back((
                                    std::mem::take(&mut pending),
                                    std::mem::take(&mut pending_charges),
                                ));
                                while kept.len() > count {
                                    kept.pop_front();
                                }
                            }
                        }
                        Selection::From(_) | Selection::FirstBytes(_) => {}
                    }
                    pending.clear();
                    pending_charges.clear();
                }
            }
        }
        if !pending.is_empty() {
            match selection {
                Selection::First(_) => {
                    emit(&pending)?;
                }
                Selection::Last(count) if count > 0 => {
                    kept.push_back((pending, pending_charges));
                    while kept.len() > count {
                        kept.pop_front();
                    }
                }
                Selection::From(_) | Selection::Last(_) | Selection::FirstBytes(_) => {}
            }
        }
        for (line, _charge) in kept {
            budget.charge_step_with(invoker)?;
            if !emit(&line)? {
                break;
            }
        }
        Ok(())
    }
}
