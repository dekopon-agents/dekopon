mod eval;
#[cfg(test)]
mod eval_tests;
mod format;
#[cfg(test)]
mod interpreter_tests;
mod lex;
mod parse;
#[cfg(test)]
mod tests;

use crate::interp::telemetry::StageObservation;
use crate::{builtins::CommandFailure, limits::Budget};
use dekopon_process::{
    ExitCode,
    pipe::{PipeReader, ReadOutcome},
};
use dekopon_process::{LimitExceeded, RetainedBytes, ShellThreads, ThreadName};
use eval::{Engine, Flow, Text, Value};
use parse::{Program, Refusal};
use regex_bites::Regex;

#[derive(Debug, PartialEq)]
pub(crate) enum AwkError {
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
pub(crate) struct RuntimeError {
    kind: RuntimeKind,
    record: f64,
}

#[derive(Debug, PartialEq)]
enum RuntimeKind {
    DivisionByZero,
    FieldIndex(f64),
    Fields,
    Utf8,
    EmptySeparator,
    Refused(Refusal),
    FormatSyntax {
        line: usize,
        column: usize,
        expected: String,
    },
}

impl RuntimeKind {
    fn message(&self) -> String {
        match self {
            Self::DivisionByZero => "awk: division by zero".into(),
            Self::FieldIndex(index) => format!("awk: field index {index} is out of range"),
            Self::Fields => "awk: more than 65 536 fields".into(),
            Self::Utf8 => "awk: record is not valid UTF-8".into(),
            Self::EmptySeparator => "awk: an empty field separator is not supported".into(),
            Self::Refused(refusal) => refusal.message(),
            Self::FormatSyntax {
                line,
                column,
                expected,
            } => syntax_message(*line, *column, expected),
        }
    }
}

fn syntax_message(line: usize, column: usize, expected: &str) -> String {
    format!("awk: syntax error at line {line}, column {column}: {expected}")
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
            } => Self::usage(syntax_message(line, column, &expected)),
            AwkError::Runtime(RuntimeError { kind, record }) => {
                Self::usage(format!("{} (record {record})", kind.message()))
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

#[derive(Debug)]
pub(crate) struct Options {
    compiled: Compiled,
    assignments: Vec<(Text, Value)>,
    separators: Vec<(Text, ChargedRegex)>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum RunError {
    Awk(AwkError),
    Output(CommandFailure),
}

impl From<AwkError> for RunError {
    fn from(value: AwkError) -> Self {
        Self::Awk(value)
    }
}
impl From<LimitExceeded> for RunError {
    fn from(value: LimitExceeded) -> Self {
        Self::Awk(AwkError::Limit(value))
    }
}
impl From<CommandFailure> for RunError {
    fn from(value: CommandFailure) -> Self {
        use crate::builtins::FatalError;
        match value {
            CommandFailure::Fatal(FatalError::Limit(limit)) => Self::Awk(AwkError::Limit(limit)),
            other @ (CommandFailure::Status { .. }
            | CommandFailure::Fatal(
                FatalError::Unsupported(_) | FatalError::Assertion(_),
            )) => Self::Output(other),
        }
    }
}
impl From<RunError> for CommandFailure {
    fn from(value: RunError) -> Self {
        match value {
            RunError::Awk(error) => error.into(),
            RunError::Output(error) => error,
        }
    }
}

pub(crate) enum StageReader<'a> {
    Ready(PipeReader<StageObservation>, Option<RetainedBytes>),
    Inherited(Option<&'a mut PipeReader<StageObservation>>),
}

impl StageReader<'_> {
    pub(crate) fn open(self) -> (PipeReader<StageObservation>, Option<RetainedBytes>) {
        match self {
            Self::Ready(reader, charge) => (reader, charge),
            Self::Inherited(reader) => (reader.map(std::mem::take).unwrap_or_default(), None),
        }
    }
}

fn identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

pub(crate) fn parse(
    arguments: &[String],
    budget: &Budget,
) -> Result<(Options, Option<String>), AwkError> {
    let mut assignments = Vec::new();
    let mut separators = Vec::new();
    let mut arguments = arguments.iter();
    let source = loop {
        let Some(argument) = arguments.next() else {
            return Err(Refusal::NoProgram.into());
        };
        if argument == "--" {
            break arguments.next().ok_or(Refusal::NoProgram)?;
        }
        if argument.starts_with("-F") || argument.starts_with("-v") {
            let field = argument.starts_with("-F");
            let raw = if argument.len() > 2 {
                &argument[2..]
            } else {
                arguments.next().map(String::as_str).unwrap_or("")
            };
            let (name, value) = if field {
                ("FS", raw)
            } else {
                let Some((name, value)) = raw.split_once('=') else {
                    return Err(Refusal::BadVariable(raw.into()).into());
                };
                if !identifier(name) {
                    return Err(Refusal::BadVariable(raw.into()).into());
                }
                parse::check_name(name)?;
                (name, value)
            };
            let value = Text::build(value.len(), budget, || lex::escapes(value))?;
            if field {
                if value.as_str().is_empty() {
                    return Err(Refusal::EmptySeparator.into());
                }
                if value.as_str().chars().count() > 1 {
                    let regex =
                        ChargedRegex::compile(value.as_str(), budget).map_err(
                            |error| match error {
                                AwkError::Refused(Refusal::Regex(error)) => {
                                    AwkError::Refused(Refusal::BadFieldRegex(error))
                                }
                                other @ (AwkError::Refused(_)
                                | AwkError::Syntax { .. }
                                | AwkError::Runtime(_)
                                | AwkError::Limit(_)) => other,
                            },
                        )?;
                    separators.push((value.clone(), regex));
                }
            }
            assignments.push((
                Text::new(name, budget)?,
                if field {
                    Value::Str(value)
                } else {
                    Value::input(value)
                },
            ));
        } else if argument.starts_with('-') && argument != "-" {
            return Err(Refusal::Option(argument.clone()).into());
        } else {
            break argument;
        }
    };
    let operand = arguments.next().cloned();
    if let Some(value) = &operand
        && let Some((name, _)) = value.split_once('=')
        && identifier(name)
    {
        return Err(Refusal::OperandAssignment(value.clone()).into());
    }
    if arguments.next().is_some() {
        return Err(Refusal::SecondInput.into());
    }
    let compiled = compile(source, budget)?;
    for (name, _) in &assignments {
        if compiled.program.arrays.contains(name.as_str()) {
            return Err(Refusal::ScalarArray(name.as_str().into()).into());
        }
    }
    Ok((
        Options {
            compiled,
            assignments,
            separators,
        },
        operand,
    ))
}

pub(crate) fn run_stream(
    options: Options,
    input: StageReader<'_>,
    budget: &mut Budget,
    mut emit: impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
) -> Result<ExitCode, RunError> {
    let Options {
        compiled,
        assignments,
        separators,
    } = options;
    let mut engine = Engine::new(&compiled, budget)?;
    for (source, regex) in separators {
        engine.seed_regex(source, regex);
    }
    for (name, value) in assignments {
        engine.set_variable(name.as_str(), value)?;
    }
    let begin = engine.phase(parse::Phase::Begin, &mut emit)?;
    let needs_input = compiled
        .program
        .rules
        .iter()
        .any(|rule| rule.phase != parse::Phase::Begin);
    if begin == Flow::Normal && needs_input && !engine.reader_gone {
        let (mut reader, _charge) = input.open();
        read_records(&mut engine, &mut reader, &mut emit)?;
    }
    if !engine.reader_gone {
        engine.phase(parse::Phase::End, &mut emit)?;
    }
    Ok(ExitCode::from_script_exit(i64::from(engine.status)))
}

fn read_records(
    engine: &mut Engine<'_>,
    reader: &mut PipeReader<StageObservation>,
    emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
) -> Result<(), RunError> {
    let mut pending = Vec::new();
    let mut charge = engine.budget.charge_value_bytes(0)?;
    while let ReadOutcome::Bytes(chunk) = reader.read(engine.budget.tree())? {
        engine.budget.charge_step()?;
        let mut rest = chunk.as_slice();
        while !rest.is_empty() {
            let end = rest.iter().position(|c| *c == b'\n');
            let length = end.unwrap_or(rest.len());
            charge.grow(length as u64)?;
            pending.extend_from_slice(&rest[..length]);
            rest = &rest[length + usize::from(end.is_some())..];
            if end.is_some() {
                let next_charge = engine.budget.charge_value_bytes(0)?;
                let flow = record(
                    engine,
                    std::mem::take(&mut pending),
                    std::mem::replace(&mut charge, next_charge),
                    emit,
                )?;
                match flow {
                    Flow::Exit(_) => return Ok(()),
                    Flow::Normal | Flow::Next => {}
                    Flow::Break | Flow::Continue => {
                        unreachable!("parser requires a containing loop")
                    }
                }
            }
        }
    }
    if !pending.is_empty() {
        record(engine, pending, charge, emit)?;
    }
    Ok(())
}

fn record(
    engine: &mut Engine<'_>,
    bytes: Vec<u8>,
    charge: RetainedBytes,
    emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
) -> Result<Flow, RunError> {
    engine.budget.check_deadline()?;
    engine.nr += 1.0;
    let text = String::from_utf8(bytes).map_err(|_invalid_utf8| engine.error(RuntimeKind::Utf8))?;
    engine.set_record(Value::input(Text::owned(text, charge)))?;
    engine.phase(parse::Phase::Record, emit)
}
