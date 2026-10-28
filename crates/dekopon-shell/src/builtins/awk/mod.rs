mod lex;
mod parse;
#[cfg(test)]
mod tests;

use dekopon_process::{RetainedBytes, ShellThreads, ThreadName};
use regex_bites::Regex;

use crate::{builtins::CommandFailure, limits::Budget};
use parse::{AwkError, Program, Refusal};

#[derive(Debug)]
struct CompiledRegex {
    regex: Regex,
    _charge: RetainedBytes,
}

#[derive(Debug)]
struct CompiledProgram {
    program: Program,
    regexes: Vec<CompiledRegex>,
    _source_charge: RetainedBytes,
}

fn compile(source: &str, budget: &Budget) -> Result<CompiledProgram, AwkError> {
    if source.len() > 8192 {
        return Err(Refusal::ProgramBytes(source.len()).into());
    }
    let source_charge = budget.charge_value_bytes(source.len() as u64)?;
    let stack = budget.charge_value_bytes(ShellThreads::STACK_BYTES as u64)?;
    let [permit] = budget.tree().shell_threads().reserve()?;
    #[expect(
        clippy::disallowed_methods,
        reason = "owner: awk compile joins its parser before returning; bound: one ShellThreads permit and a charged 2 MiB stack"
    )]
    let parsed = std::thread::scope(|scope| {
        let handle = permit
            .spawn(scope, ThreadName::new("sh-awk-parse"), || {
                parse::program(source)
            })
            .map_err(|error| Refusal::ParserStart(error.to_string()))?;
        match handle.join() {
            Ok(result) => result,
            Err(cause) => std::panic::resume_unwind(cause),
        }
    });
    drop(stack);
    let program = parsed?;
    let regexes = program
        .regexes
        .iter()
        .map(|source| compile_regex(source, budget))
        .collect::<Result<_, _>>()?;
    Ok(CompiledProgram {
        program,
        regexes,
        _source_charge: source_charge,
    })
}

fn compile_regex(source: &str, budget: &Budget) -> Result<CompiledRegex, AwkError> {
    use crate::builtins::text;
    text::check_extended_length("awk", source).map_err(AwkError::regex)?;
    let _rewrite_charge = budget.charge_value_bytes((source.len() * 3) as u64)?;
    let rewritten = lex::regex_source(source)?;
    let charge = budget.charge_value_bytes(256 * 1024)?;
    let regex = text::build_extended("awk", source, &rewritten, false).map_err(AwkError::regex)?;
    if regex.captures_len() != 1 {
        return Err(Refusal::InternalCaptures(regex.captures_len()).into());
    }
    Ok(CompiledRegex {
        regex,
        _charge: charge,
    })
}

fn failure(error: AwkError) -> CommandFailure {
    error.into()
}

fn compile_separator(source: &str, budget: &Budget) -> Result<Option<CompiledRegex>, AwkError> {
    let _decode_charge = budget.charge_value_bytes(source.len() as u64)?;
    let decoded = lex::decode_escapes(source)?;
    if decoded.is_empty() {
        return Err(Refusal::EmptySeparator.into());
    }
    if decoded.chars().count() < 2 {
        return Ok(None);
    }
    compile_regex(&decoded, budget)
        .map(Some)
        .map_err(|error| match error {
            AwkError::Refused(Refusal::Regex(message)) => {
                AwkError::Refused(Refusal::SeparatorRegex(message))
            }
            AwkError::Refused(refusal) => AwkError::Refused(refusal),
            AwkError::Syntax {
                line,
                column,
                expected,
            } => AwkError::Syntax {
                line,
                column,
                expected,
            },
            AwkError::Runtime { error, record } => AwkError::Runtime { error, record },
            AwkError::Limit(limit) => AwkError::Limit(limit),
        })
}
