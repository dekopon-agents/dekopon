mod lex;
mod parse;
#[cfg(test)]
mod tests;

use crate::{builtins::CommandFailure, limits::Budget};
use dekopon_process::{LimitExceeded, RetainedBytes, ShellThreads, ThreadName};
use parse::{Program, Refusal};
use regex_bites::Regex;

#[derive(Debug, PartialEq)]
enum AwkError {
    Refused(Refusal),
    Syntax {
        line: usize,
        column: usize,
        expected: String,
    },
    Runtime(RuntimeError),
    Limit(LimitExceeded),
}

#[derive(Debug, PartialEq)]
struct RuntimeError {
    refusal: Refusal,
    record: f64,
}

impl From<Refusal> for AwkError {
    fn from(value: Refusal) -> Self {
        Self::Refused(value)
    }
}
impl From<LimitExceeded> for AwkError {
    fn from(value: LimitExceeded) -> Self {
        Self::Limit(value)
    }
}
impl From<AwkError> for CommandFailure {
    fn from(value: AwkError) -> Self {
        match value {
            AwkError::Refused(refusal) => Self::usage(refusal.message()),
            AwkError::Syntax {
                line,
                column,
                expected,
            } => Self::usage(format!(
                "awk: syntax error at line {line}, column {column}: {expected}"
            )),
            AwkError::Runtime(RuntimeError { refusal, record }) => {
                Self::usage(format!("{} (record {record})", refusal.message()))
            }
            AwkError::Limit(limit) => limit.into(),
        }
    }
}

#[derive(Debug)]
struct Compiled {
    program: Program,
    regexes: Vec<ChargedRegex>,
}

#[derive(Debug)]
struct ChargedRegex {
    regex: Regex,
    _charge: RetainedBytes,
}

impl ChargedRegex {
    fn compile(source: &str, budget: &Budget) -> Result<Self, AwkError> {
        crate::builtins::text::check_extended_length("awk", source).map_err(Refusal::Regex)?;
        let charge = budget.charge_value_bytes(256 * 1024)?;
        let rewritten = lex::rewrite_regex(source)?;
        let regex = crate::builtins::text::build_extended("awk", &rewritten, source, false)
            .map_err(Refusal::Regex)?;
        if regex.captures_len() != 1 {
            return Err(Refusal::InternalRegex.into());
        }
        Ok(Self {
            regex,
            _charge: charge,
        })
    }
}

fn compile(source: &str, budget: &Budget) -> Result<Compiled, AwkError> {
    if source.len() > 8192 {
        return Err(Refusal::ProgramBytes(source.len()).into());
    }
    let stack = budget.charge_value_bytes(ShellThreads::STACK_BYTES as u64)?;
    let [permit] = budget.tree().shell_threads().reserve()?;
    #[expect(
        clippy::disallowed_methods,
        reason = "owner: compile joins the parser; bound: one admitted thread and a charged 2 MiB stack"
    )]
    let parsed = std::thread::scope(|scope| {
        let parser = permit
            .spawn(scope, ThreadName::new("awk-parse"), || {
                parse::program(source)
            })
            .map_err(|error| Refusal::ParserStart(error.to_string()))?;
        parser.join().map_err(|panic| {
            let reason = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic payload");
            AwkError::Refused(Refusal::ParserPanic(reason.to_owned()))
        })?
    });
    drop(stack);
    let program = parsed?;
    let regexes = program
        .regex_sources
        .iter()
        .map(|source| ChargedRegex::compile(source, budget))
        .collect::<Result<_, _>>()?;
    Ok(Compiled { program, regexes })
}
