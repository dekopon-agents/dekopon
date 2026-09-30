use std::collections::VecDeque;

use crate::{
    CapabilityInvoker, RetainedBytes,
    builtins::{CommandFailure, unsupported_flag},
    limits::Budget,
    pipe::{PipeReader, ReadOutcome},
};

#[derive(Clone, Copy)]
pub(crate) enum LineCommand {
    Head,
    Tail,
}

enum Selection {
    First(usize),
    Last(usize),
    From(usize),
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
            Self::Head => "-n N -N",
            Self::Tail => "-n N -N -n +N",
        }
    }

    fn selection(self, arguments: &[String]) -> Result<Selection, CommandFailure> {
        let name = self.name();
        let number = match arguments {
            [] => "10",
            [flag, number] if flag == "-n" => number.as_str(),
            [short] if short.starts_with('-') && short.len() > 1 => {
                if short.as_bytes()[1] == b'+' {
                    return Err(unsupported_flag(name, short, self.help()));
                }
                &short[1..]
            }
            [flag, ..] if flag.starts_with('-') => {
                return Err(unsupported_flag(name, flag, self.help()));
            }
            [other, ..] => {
                return Err(CommandFailure::usage(format!(
                    "{name}: unexpected argument {other:?}; input arrives through a pipe"
                )));
            }
        };
        let (from, digits) = if let Some(digits) = number.strip_prefix('+') {
            (true, digits)
        } else {
            (false, number)
        };
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(unsupported_flag(name, &format!("-{number}"), self.help()));
        }
        let count = digits.parse::<usize>().map_err(|_overflow| {
            CommandFailure::usage(format!("{name}: line count exceeds platform size"))
        })?;
        match (self, from) {
            (Self::Head, true) => Err(unsupported_flag(name, &format!("-n {number}"), self.help())),
            (Self::Head, false) => Ok(Selection::First(count)),
            (Self::Tail, true) => Ok(Selection::From(count.max(1))),
            (Self::Tail, false) => Ok(Selection::Last(count)),
        }
    }

    pub(crate) fn run(
        self,
        arguments: &[String],
        reader: &mut PipeReader,
        budget: &mut Budget,
        invoker: &dyn CapabilityInvoker,
        mut emit: impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<(), CommandFailure> {
        let selection = self.selection(arguments)?;
        if matches!(selection, Selection::First(0) | Selection::Last(0)) {
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
                        Selection::From(_) => {}
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
                Selection::From(_) | Selection::Last(_) => {}
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
