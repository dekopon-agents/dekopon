use base64::{Engine as _, engine::general_purpose::STANDARD};

use super::{CommandFailure, unsupported_flag};
use crate::{
    CapabilityInvoker,
    limits::Budget,
    pipe::{PipeReader, ReadOutcome},
};

pub(crate) const HELP: &str = "-d";

pub(crate) fn stream(
    arguments: &[String],
    reader: &mut PipeReader,
    budget: &mut Budget,
    invoker: &dyn CapabilityInvoker,
    mut emit: impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
) -> Result<(), CommandFailure> {
    let mut decode = false;
    let mut literal = None;
    for argument in arguments {
        match argument.as_str() {
            "-d" | "-D" | "--decode" => decode = true,
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(unsupported_flag("base64", flag, HELP));
            }
            other if literal.is_none() => literal = Some(other.as_bytes()),
            _ => {
                return Err(CommandFailure::usage(
                    "base64: at most one literal argument is supported",
                ));
            }
        }
    }
    let mut pending = Vec::with_capacity(4);
    let mut finished = false;
    let mut process = |chunk: &[u8], budget: &mut Budget| -> Result<bool, CommandFailure> {
        budget.charge_step_with(invoker)?;
        for &byte in chunk {
            if decode && byte.is_ascii_whitespace() {
                continue;
            }
            if finished {
                return Err(CommandFailure::failed(
                    "base64: invalid input after padding",
                ));
            }
            pending.push(byte);
            let width = if decode { 4 } else { 3 };
            if pending.len() == width {
                let output = if decode {
                    STANDARD.decode(&pending).map_err(|error| {
                        CommandFailure::failed(format!("base64: invalid input: {error}"))
                    })?
                } else {
                    STANDARD.encode(&pending).into_bytes()
                };
                if decode && pending.contains(&b'=') {
                    finished = true;
                }
                pending.clear();
                if !emit(&output)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    };
    if let Some(literal) = literal {
        if !process(literal, budget)? {
            return Ok(());
        }
    } else {
        while let ReadOutcome::Bytes(chunk) = reader.read(budget, invoker)? {
            if !process(&chunk, budget)? {
                return Ok(());
            }
        }
    }
    if !pending.is_empty() {
        if decode {
            return Err(CommandFailure::failed(
                "base64: invalid input: incomplete group",
            ));
        }
        budget.charge_step_with(invoker)?;
        emit(STANDARD.encode(&pending).as_bytes())?;
    }
    Ok(())
}
