use crate::interp::telemetry::StageObservation;
use dekopon_process::pipe::{PipeReader, ReadOutcome};
use std::collections::VecDeque;

use clap::{Arg, Command};

use crate::{
    builtins::{CommandFailure, parse_flags, unsupported_flag},
    limits::Budget,
};
use dekopon_process::RetainedBytes;

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

pub(crate) fn head(arguments: &[String]) -> Result<(Selection, Option<String>), CommandFailure> {
    let matches = flags(
        crate::builtins::Builtin::Head,
        arguments,
        Some(Arg::new("bytes").short('c').long("bytes").num_args(1)),
    )?;
    let input = matches.get_one::<String>("input").cloned();
    if let Some(bytes) = matches.get_one::<String>("bytes") {
        let count = parse_count("head", bytes)?;
        if matches.contains_id("lines") {
            return Err(CommandFailure::usage("head: -c and -n cannot be combined"));
        }
        return Ok((Selection::FirstBytes(count), input));
    }
    let number = matches
        .get_one::<String>("lines")
        .map(String::as_str)
        .unwrap_or("10");
    Ok((Selection::First(parse_count("head", number)?), input))
}

pub(crate) fn tail(arguments: &[String]) -> Result<(Selection, Option<String>), CommandFailure> {
    let matches = flags(crate::builtins::Builtin::Tail, arguments, None)?;
    let number = matches
        .get_one::<String>("lines")
        .map(String::as_str)
        .unwrap_or("10");
    let selection = match number.strip_prefix('+') {
        Some(digits) => Selection::From(parse_count("tail", digits)?.max(1)),
        None => Selection::Last(parse_count("tail", number)?),
    };
    Ok((selection, matches.get_one::<String>("input").cloned()))
}

fn flags(
    command: crate::builtins::Builtin,
    arguments: &[String],
    extra: Option<Arg>,
) -> Result<clap::ArgMatches, CommandFailure> {
    if let Some(first) = arguments.first()
        && first.starts_with('+')
    {
        return Err(unsupported_flag(command.name(), first, command.help()));
    }
    let mut rewritten = arguments.to_vec();
    if let Some(first) = rewritten.first()
        && first.starts_with('-')
        && !first[1..].is_empty()
        && first[1..].bytes().all(|byte| byte.is_ascii_digit())
    {
        rewritten.splice(..1, ["-n".to_owned(), first[1..].to_owned()]);
    }
    let mut grammar = Command::new(command.name()).arg(
        Arg::new("lines")
            .short('n')
            .long("lines")
            .num_args(1)
            .allow_hyphen_values(true),
    );
    if let Some(extra) = extra {
        grammar = grammar.arg(extra);
    }
    parse_flags(
        grammar.arg(Arg::new("input")),
        &rewritten,
        command.name(),
        command.help(),
    )
}

pub(crate) fn run(
    selection: Selection,
    reader: &mut PipeReader<StageObservation>,
    budget: &mut Budget,

    mut emit: impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
) -> Result<(), CommandFailure> {
    if matches!(
        selection,
        Selection::First(0) | Selection::FirstBytes(0) | Selection::Last(0)
    ) {
        return Ok(());
    }
    if let Selection::FirstBytes(mut remaining) = selection {
        while let ReadOutcome::Bytes(chunk) = reader.read(budget.tree())? {
            budget.charge_step()?;
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
    while let ReadOutcome::Bytes(chunk) = reader.read(budget.tree())? {
        budget.charge_step()?;
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
        budget.charge_step()?;
        if !emit(&line)? {
            break;
        }
    }
    Ok(())
}
