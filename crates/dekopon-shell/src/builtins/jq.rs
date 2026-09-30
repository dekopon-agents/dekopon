use std::{
    io::{self, BufRead as _, Read as _, Write as _},
    process::{Command, Stdio},
    sync::mpsc::{RecvTimeoutError, sync_channel},
    time::Duration,
};

use jaq_core::{
    Compiler, Ctx, Exn, Vars, data,
    load::{Arena, File, Loader},
};
use jaq_json::{Num, Val};
use serde_json::Value;

use super::{CommandFailure, unsupported_flag};
use crate::{
    CapabilityInvoker, ExitCode,
    jq_worker::{JQ_WORKER_MARKER, WorkerChild, executable},
    limits::{Budget, LimitExceeded},
    pipe::{PipeReader, ReadOutcome},
};

/// env reads the host process environment and now reads the host wall clock; neither is reachable
/// any other way in this crate, so both are excluded from the filter set.
const HOST_REACHING_FILTERS: &[&str] = &["env", "now"];

pub(crate) const HELP: &str = "-r -c -n -s";

#[derive(Default)]
struct Options<'a> {
    filter: Option<&'a str>,
    raw: bool,
    null_input: bool,
    slurp: bool,
}

fn options(arguments: &[String]) -> Result<Options<'_>, CommandFailure> {
    let mut opts = Options::default();
    for argument in arguments {
        match argument.as_str() {
            "-r" | "--raw-output" => opts.raw = true,
            "-c" | "--compact-output" => {}
            "-n" | "--null-input" => opts.null_input = true,
            "-s" | "--slurp" => opts.slurp = true,
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(unsupported_flag("jq", flag, HELP));
            }
            filter if opts.filter.is_none() => opts.filter = Some(filter),
            _ => {
                return Err(CommandFailure::usage(
                    "jq: exactly one filter argument is supported",
                ));
            }
        }
    }
    if opts.filter.is_none() {
        return Err(CommandFailure::usage("jq: a filter argument is required"));
    }
    Ok(opts)
}

pub(crate) fn stream(
    arguments: &[String],
    reader: &mut PipeReader,
    budget: &mut Budget,
    invoker: &dyn CapabilityInvoker,
    emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
) -> Result<(), CommandFailure> {
    let opts = options(arguments)?;
    evaluate(opts, reader, budget, invoker, emit)
}

enum ReadOutput {
    Value(Value),
    Done,
    Exhausted,
    Invalid(String),
}

fn read_outputs(
    stdout: impl io::Read,
    maximum: u64,
    raw: bool,
    sender: &std::sync::mpsc::SyncSender<ReadOutput>,
) -> io::Result<()> {
    let mut stdout = io::BufReader::new(stdout);
    let frame_limit = if raw {
        maximum.saturating_mul(6).saturating_add(3)
    } else {
        maximum.saturating_add(1)
    };
    loop {
        let mut line = Vec::new();
        let count = stdout
            .by_ref()
            .take(frame_limit.saturating_add(1))
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        let item = if count as u64 > frame_limit {
            ReadOutput::Exhausted
        } else {
            match serde_json::from_slice(&line) {
                Ok(value) => ReadOutput::Value(value),
                Err(error) => ReadOutput::Invalid(format!("jq: invalid worker output: {error}")),
            }
        };
        let terminal = !matches!(item, ReadOutput::Value(_));
        if sender.send(item).is_err() || terminal {
            return Ok(());
        }
    }
    let _sent = sender.send(ReadOutput::Done);
    Ok(())
}

fn collect(
    receiver: &std::sync::mpsc::Receiver<ReadOutput>,
    budget: &mut Budget,
    invoker: &dyn CapabilityInvoker,
    raw: bool,
    emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
) -> Result<bool, CommandFailure> {
    loop {
        let wait = budget
            .remaining()
            .min(Duration::from_secs(1))
            .max(Duration::from_millis(1));
        match receiver.recv_timeout(wait) {
            Ok(ReadOutput::Value(value)) => {
                budget.charge_step_with(invoker)?;
                let bytes = match (raw, value) {
                    (true, Value::String(text)) => text.into_bytes(),
                    (_, value) => serde_json::to_vec(&value).map_err(|error| {
                        CommandFailure::failed(format!("jq: invalid worker output: {error}"))
                    })?,
                };
                let _charge = budget.charge_value_bytes(bytes.len() as u64)?;
                if !emit(&bytes)? || !emit(b"\n")? {
                    return Ok(false);
                }
            }
            Ok(ReadOutput::Exhausted) => {
                return Err(LimitExceeded::ValueBytes {
                    maximum: budget.max_value_bytes(),
                }
                .into());
            }
            Ok(ReadOutput::Invalid(message)) => return Err(CommandFailure::failed(message)),
            Ok(ReadOutput::Done) => {
                if invoker.cancelled() {
                    return Err(LimitExceeded::Cancelled.into());
                }
                budget.check_deadline()?;
                return Ok(true);
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(CommandFailure::failed("jq: worker output reader stopped"));
            }
            Err(RecvTimeoutError::Timeout) => {
                if invoker.cancelled() {
                    return Err(LimitExceeded::Cancelled.into());
                }
                budget.check_deadline()?;
            }
        }
    }
}

fn worker_status(status: std::process::ExitStatus, stderr: &str) -> CommandFailure {
    CommandFailure::Status {
        message: format!("jq: worker exited {status}: {stderr}"),
        status: if status.code() == Some(2) {
            ExitCode::SYNTAX
        } else {
            ExitCode::FAILURE
        },
    }
}

const STDERR_BYTES: u64 = 4096;

fn evaluate(
    opts: Options<'_>,
    input: &mut PipeReader,
    budget: &mut Budget,
    invoker: &dyn CapabilityInvoker,
    emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
) -> Result<(), CommandFailure> {
    let path =
        executable().ok_or_else(|| CommandFailure::failed("jq: no worker executable supplied"))?;
    let child = Command::new(path)
        .env_clear()
        .env(JQ_WORKER_MARKER, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| CommandFailure::failed(format!("jq: could not start worker: {error}")))?;
    let mut child = WorkerChild(child);
    let stdin = child
        .0
        .stdin
        .take()
        .ok_or_else(|| CommandFailure::failed("jq: worker stdin missing"))?;
    let stdout = child
        .0
        .stdout
        .take()
        .ok_or_else(|| CommandFailure::failed("jq: worker stdout missing"))?;
    let stderr = child
        .0
        .stderr
        .take()
        .ok_or_else(|| CommandFailure::failed("jq: worker stderr missing"))?;
    std::thread::scope(|scope| {
        let mut input_budget = budget.fork();
        let writer = scope.spawn(
            move || -> Result<Vec<crate::RetainedBytes>, CommandFailure> {
                let mut stdin = io::BufWriter::new(stdin);
                serde_json::to_writer(&mut stdin, &(opts.filter.unwrap_or_default(), opts.slurp))
                .map_err(|error| CommandFailure::failed(format!("jq: worker request: {error}")))?;
                stdin.write_all(b"\n").map_err(input_error)?;
                if opts.null_input {
                    stdin.write_all(b"null\n").map_err(input_error)?;
                    stdin.flush().map_err(input_error)?;
                    return Ok(Vec::new());
                }
                if opts.slurp {
                    let mut values = Vec::new();
                    let mut charges = Vec::new();
                    let retention = input_budget.fork();
                    let mut source = PipeSource::new(input, &mut input_budget, invoker);
                    loop {
                        let item = serde_json::Deserializer::from_reader(&mut source)
                            .into_iter::<Value>()
                            .next();
                        let value = match item {
                            Some(Ok(value)) => value,
                            Some(Err(error)) => {
                                if let Some(failure) = source.failure {
                                    return Err(failure.into());
                                }
                                return Err(CommandFailure::Status {
                                    message: format!("jq: invalid JSON input: {error}"),
                                    status: ExitCode::SYNTAX,
                                });
                            }
                            None => break,
                        };
                        drop(source.charged.take());
                        charges.push(retention.charge_value_bytes(weigh(&value))?);
                        values.push(value);
                    }
                    if let Some(error) = source.failure {
                        return Err(error.into());
                    }
                    serde_json::to_writer(&mut stdin, &values).map_err(|error| {
                        CommandFailure::failed(format!("jq: worker input: {error}"))
                    })?;
                    stdin.write_all(b"\n").map_err(input_error)?;
                    stdin.flush().map_err(input_error)?;
                    return Ok(charges);
                } else {
                    while let ReadOutcome::Bytes(chunk) = input.read(&input_budget, invoker)? {
                        stdin.write_all(&chunk).map_err(input_error)?;
                        stdin.flush().map_err(input_error)?;
                        input_budget.charge_step_with(invoker)?;
                    }
                }
                stdin.flush().map_err(input_error)?;
                Ok(Vec::new())
            },
        );
        let errors = scope.spawn(move || {
            let mut stderr = stderr;
            let mut excerpt = String::new();
            let _excerpt_result = stderr
                .by_ref()
                .take(STDERR_BYTES)
                .read_to_string(&mut excerpt);
            let _drain_result = io::copy(&mut stderr, &mut io::sink());
            excerpt
        });
        let (sender, receiver) = sync_channel(0);
        let maximum = budget.max_value_bytes();
        let reader = scope.spawn(move || {
            if let Err(error) = read_outputs(stdout, maximum, opts.raw, &sender) {
                let _sent = sender.send(ReadOutput::Invalid(format!(
                    "jq: could not read worker output: {error}"
                )));
            }
        });
        let result = collect(&receiver, budget, invoker, opts.raw, emit);
        drop(receiver);
        if !matches!(result, Ok(true)) {
            drop(child);
            let _reader_result = reader.join();
            let _writer_result = writer.join();
            let _stderr_result = errors.join();
            return result.map(|_| ());
        }
        let status = child.0.wait();
        drop(child);
        let read = reader.join();
        let written = writer.join();
        let stderr = errors.join().unwrap_or_default();
        let status = status.map_err(|error| {
            CommandFailure::failed(format!("jq: could not wait for worker: {error}"))
        })?;
        if !status.success() {
            return Err(worker_status(status, &stderr));
        }
        if read.is_err() {
            return Err(CommandFailure::failed("jq: worker output reader panicked"));
        }
        let _charges = match written {
            Ok(Ok(charges)) => charges,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(CommandFailure::failed("jq: worker input writer panicked")),
        };
        result.map(|_| ())
    })
}

fn input_error(error: io::Error) -> CommandFailure {
    CommandFailure::failed(format!("jq: could not send worker input: {error}"))
}

struct PipeSource<'a> {
    reader: &'a mut PipeReader,
    budget: &'a mut Budget,
    invoker: &'a dyn CapabilityInvoker,
    pending: Vec<u8>,
    offset: usize,
    failure: Option<LimitExceeded>,
    charged: Option<crate::RetainedBytes>,
}

impl<'a> PipeSource<'a> {
    fn new(
        reader: &'a mut PipeReader,
        budget: &'a mut Budget,
        invoker: &'a dyn CapabilityInvoker,
    ) -> Self {
        Self {
            reader,
            budget,
            invoker,
            pending: Vec::new(),
            offset: 0,
            failure: None,
            charged: None,
        }
    }
}

impl io::Read for PipeSource<'_> {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        if target.is_empty() {
            return Ok(0);
        }
        if self.offset == self.pending.len() {
            match self.reader.read(self.budget, self.invoker) {
                Ok(ReadOutcome::Bytes(bytes)) => {
                    if let Err(error) = self.budget.charge_step_with(self.invoker) {
                        self.failure = Some(error);
                        return Err(io::Error::other("jq: input interrupted"));
                    }
                    self.pending = bytes;
                    self.offset = 0;
                }
                Ok(ReadOutcome::End) => return Ok(0),
                Err(error) => {
                    self.failure = Some(error);
                    return Err(io::Error::other("jq: input interrupted"));
                }
            }
        }
        let len = target.len().min(self.pending.len() - self.offset);
        let charge = match self.charged.as_mut() {
            Some(charge) => charge.grow(len as u64),
            None => self.budget.charge_value_bytes(len as u64).map(|charge| {
                self.charged = Some(charge);
            }),
        };
        if let Err(error) = charge {
            self.failure = Some(error);
            return Err(io::Error::other("jq: input exceeds retained-byte budget"));
        }
        target[..len].copy_from_slice(&self.pending[self.offset..self.offset + len]);
        self.offset += len;
        Ok(len)
    }
}

pub(crate) enum WorkerFailure {
    InvalidInput(String),
    Failed(String),
}

impl std::fmt::Display for WorkerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(message) | Self::Failed(message) => f.write_str(message),
        }
    }
}

pub(crate) fn run_filter(
    filter: &str,
    inputs: impl Iterator<Item = Result<Val, serde_json::Error>>,
    output: &mut impl io::Write,
) -> Result<(), WorkerFailure> {
    let definitions = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let functions = jaq_core::funs()
        .chain(jaq_std::funs().filter(|(name, ..)| !HOST_REACHING_FILTERS.contains(name)))
        .chain(jaq_json::funs());

    let loader = Loader::new(definitions);
    let arena = Arena::default();
    let modules = loader
        .load(
            &arena,
            File {
                code: filter,
                path: (),
            },
        )
        .map_err(|errors| {
            WorkerFailure::Failed(format!(
                "jq: invalid filter: {}",
                describe_load_errors(&errors)
            ))
        })?;
    let compiled = Compiler::default()
        .with_funs(functions)
        .compile(modules)
        .map_err(|errors| {
            WorkerFailure::Failed(format!(
                "jq: invalid filter: {}",
                describe_compile_errors(&errors)
            ))
        })?;

    for input in inputs {
        let input = input.map_err(|error| {
            WorkerFailure::InvalidInput(format!("jq: invalid JSON input: {error}"))
        })?;
        let context = Ctx::<data::JustLut<Val>>::new(&compiled.lut, Vars::new([]));
        for result in compiled.id.run((context, input)) {
            let produced = result
                .map_err(describe_exception)
                .map_err(WorkerFailure::Failed)?;
            let value = convert(&produced, 0).map_err(WorkerFailure::Failed)?;
            serde_json::to_writer(&mut *output, &value).map_err(|error| {
                WorkerFailure::Failed(format!("jq: could not write output: {error}"))
            })?;
            output.write_all(b"\n").map_err(|error| {
                WorkerFailure::Failed(format!("jq: could not write output: {error}"))
            })?;
            output.flush().map_err(|error| {
                WorkerFailure::Failed(format!("jq: could not flush output: {error}"))
            })?;
        }
    }
    Ok(())
}

/// jaq_core::unwrap_valr calls process::exit on halt; the worker must report the halt as a failure.
fn describe_exception(exception: Exn<'_, Val>) -> String {
    match exception.get_err() {
        Ok(error) => format!("jq: {error}"),
        Err(exception) => match exception.get_halt() {
            Ok(code) => format!("jq: halt({code}) is not supported"),
            Err(_) => "jq: internal control flow escaped the filter".to_owned(),
        },
    }
}

/// Matches serde_json's own nesting ceiling; without it a filter like
/// `reduce range(100000) as $i (.;[.])` recurses once per level in convert and can abort the
/// worker before reporting a jq failure.
const MAX_OUTPUT_DEPTH: usize = 128;

/// jaq's value type is a JSON superset (byte strings, non-string keys, NaN) that its own writer
/// says cannot round-trip through JSON text, so this refuses them rather than inventing a meaning.
fn convert(value: &Val, depth: usize) -> Result<Value, String> {
    if depth > MAX_OUTPUT_DEPTH {
        return Err(format!(
            "jq: a filter produced a value nested deeper than {MAX_OUTPUT_DEPTH} levels"
        ));
    }
    match value {
        Val::Null => Ok(Value::Null),
        Val::Bool(flag) => Ok(Value::Bool(*flag)),
        Val::Num(number) => convert_number(number),
        Val::TStr(text) => Ok(Value::String(String::from_utf8_lossy(text).into_owned())),
        Val::BStr(_) => Err(not_json("a byte string")),
        Val::Arr(items) => items
            .iter()
            .map(|item| convert(item, depth + 1))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Val::Obj(fields) => {
            let mut object = serde_json::Map::new();
            for (key, field) in fields.iter() {
                let Val::TStr(key) = key else {
                    return Err(not_json("an object with a non-string key"));
                };
                object.insert(
                    String::from_utf8_lossy(key).into_owned(),
                    convert(field, depth + 1)?,
                );
            }
            Ok(Value::Object(object))
        }
    }
}

fn convert_number(number: &Num) -> Result<Value, String> {
    match number {
        Num::Int(int) => i64::try_from(*int)
            .map(|int| Value::Number(int.into()))
            .or_else(|_| convert_written_number(number)),
        Num::Float(float) => serde_json::Number::from_f64(*float)
            .map(Value::Number)
            .ok_or_else(|| not_json(&number.to_string())),
        Num::BigInt(_) | Num::Dec(_) => convert_written_number(number),
    }
}

fn convert_written_number(number: &Num) -> Result<Value, String> {
    let written = number.to_string();
    serde_json::from_str::<Value>(&written)
        .ok()
        .filter(Value::is_number)
        .ok_or_else(|| not_json(&written))
}

fn not_json(what: &str) -> String {
    format!("jq: a filter produced {what}, which has no JSON form")
}

fn weigh(value: &Value) -> u64 {
    struct Meter(u64);

    impl io::Write for Meter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len() as u64);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut meter = Meter(0);
    #[allow(
        clippy::let_underscore_must_use,
        reason = "Meter::write and flush both return Ok, and serde_json only fails here on a \
                  writer error, so there is no failure to propagate and no counter to correct"
    )]
    let _ = serde_json::to_writer(&mut meter, value);
    meter.0
}

fn describe_load_errors<P>(errors: &[(File<&str, P>, jaq_core::load::Error<&str>)]) -> String {
    errors
        .iter()
        .map(|(_, error)| match error {
            jaq_core::load::Error::Io(entries) => entries
                .iter()
                .map(|(name, message)| format!("{name}: {message}"))
                .collect::<Vec<_>>()
                .join("; "),
            // `Expect::as_str` panics for non-standard delimiters, so lex errors are described
            // structurally instead. An untrusted filter must never be able to abort the process.
            jaq_core::load::Error::Lex(entries) => entries
                .iter()
                .map(|(expected, found)| format!("expected {expected:?} near {found:?}"))
                .collect::<Vec<_>>()
                .join("; "),
            jaq_core::load::Error::Parse(entries) => entries
                .iter()
                .map(|(expected, found)| format!("expected {} near {found:?}", expected.as_str()))
                .collect::<Vec<_>>()
                .join("; "),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

type CompileErrors<'a, P> = (File<&'a str, P>, Vec<jaq_core::compile::Error<&'a str>>);

fn describe_compile_errors<P>(errors: &[CompileErrors<'_, P>]) -> String {
    errors
        .iter()
        .flat_map(|(_, entries)| entries.iter())
        .map(|(symbol, undefined)| format!("undefined {} {symbol:?}", undefined.as_str()))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slurp_refuses_an_oversized_document_before_finishing_its_parse() {
        let mut input = PipeReader::from_bytes(format!("\"{}\"", "x".repeat(100_000)).into_bytes());
        let mut budget = Budget::start(crate::Limits {
            max_value_bytes: 64,
            ..crate::Limits::default()
        });
        let invoker = crate::builtins::test_support::NoCapabilities;
        let mut source = PipeSource::new(&mut input, &mut budget, &invoker);
        let parsed = serde_json::from_reader::<_, Value>(&mut source);
        assert!(parsed.is_err());
        assert!(matches!(
            source.failure,
            Some(LimitExceeded::ValueBytes { maximum: 64 })
        ));
        assert!(
            source.offset <= 64,
            "parser consumed {} bytes",
            source.offset
        );
        drop(source);
        assert_eq!(budget.value_bytes(), 0);
    }

    #[test]
    fn emitted_output_releases_its_charge() {
        let (sender, receiver) = sync_channel(2);
        sender
            .send(ReadOutput::Value(Value::from("retained")))
            .unwrap();
        sender.send(ReadOutput::Done).unwrap();
        let mut budget = Budget::start(crate::Limits::default());
        let mut output = Vec::new();
        collect(
            &receiver,
            &mut budget,
            &crate::builtins::test_support::NoCapabilities,
            false,
            &mut |bytes| {
                output.extend_from_slice(bytes);
                Ok(true)
            },
        )
        .expect("collect output");
        assert_eq!(output, b"\"retained\"\n");
        assert_eq!(budget.value_bytes(), 0);
    }
}
