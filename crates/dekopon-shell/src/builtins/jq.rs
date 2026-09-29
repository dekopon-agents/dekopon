use std::{
    io::{self, Read as _, Write as _},
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

use super::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag};
use crate::{
    CapabilityInvoker,
    jq_worker::{JQ_WORKER_MARKER, WorkerChild, executable},
    limits::{Budget, LimitExceeded},
};

/// env reads the host process environment and now reads the host wall clock; neither is reachable
/// any other way in this crate, so both are excluded from the filter set.
const HOST_REACHING_FILTERS: &[&str] = &["env", "now"];

const HELP: &str = "-r -c";

pub(crate) struct Jq;

impl Builtin for Jq {
    fn name(&self) -> &'static str {
        "jq"
    }

    fn help(&self) -> &'static str {
        HELP
    }

    fn run(
        &self,
        context: &mut BuiltinContext<'_>,
        arguments: &[String],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let mut filter = None;
        for argument in arguments {
            match argument.as_str() {
                "-r" | "--raw-output" | "-c" | "--compact-output" => {}
                flag if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(unsupported_flag("jq", flag, HELP));
                }
                _ => {
                    if filter.is_some() {
                        return Err(CommandFailure::usage(
                            "jq: exactly one filter argument is supported",
                        ));
                    }
                    filter = Some(argument.clone());
                }
            }
        }
        let Some(filter) = filter else {
            return Err(CommandFailure::usage("jq: a filter argument is required"));
        };

        evaluate(
            &filter,
            parse_string_input(input),
            context.budget,
            context.invoker,
        )
        .map(CommandResult::value)
    }
}

/// `echo "$r"` stringifies a captured value's structure into display text; parsing that text
/// back into an object or array here makes filtering it match filtering the original value. A
/// scalar stays a string: this crate parses without `arbitrary_precision`, so a decimal-seconds
/// timestamp like `1727400000.123450` would come back a rounded `f64`, and a filter built for a
/// string (`test("^[0-9]+$")`) would see a number instead.
fn parse_string_input(input: Option<Value>) -> Value {
    match input {
        Some(Value::String(text)) => match serde_json::from_str(&text) {
            Ok(parsed @ (Value::Object(_) | Value::Array(_))) => parsed,
            _ => Value::String(text),
        },
        Some(other) => other,
        None => Value::Null,
    }
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
    sender: &std::sync::mpsc::SyncSender<ReadOutput>,
) -> io::Result<()> {
    let mut limited = io::BufReader::new(stdout).take(maximum);
    let mut stream = serde_json::Deserializer::from_reader(&mut limited).into_iter::<Value>();
    let mut failure = None;
    for item in &mut stream {
        match item {
            Ok(value) => {
                if sender.send(ReadOutput::Value(value)).is_err() {
                    return Ok(());
                }
            }
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    drop(stream);
    if let Some(error) = failure {
        let output = if limited.limit() == 0 {
            ReadOutput::Exhausted
        } else {
            ReadOutput::Invalid(format!("jq: invalid worker output: {error}"))
        };
        let _sent = sender.send(output);
        return Ok(());
    }
    if limited.limit() == 0 {
        let mut extra = [0];
        if limited.get_mut().read(&mut extra)? != 0 {
            let _sent = sender.send(ReadOutput::Exhausted);
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
) -> Result<Vec<Value>, CommandFailure> {
    let mut outputs = Vec::new();
    loop {
        let wait = budget
            .remaining()
            .min(Duration::from_secs(1))
            .max(Duration::from_millis(1));
        match receiver.recv_timeout(wait) {
            Ok(ReadOutput::Value(value)) => {
                budget.charge_step_with(invoker)?;
                budget.charge_value_bytes(weigh(&value))?;
                outputs.push(value);
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
                return Ok(outputs);
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
    CommandFailure::failed(format!("jq: worker exited {status}: {stderr}"))
}

const STDERR_BYTES: u64 = 4096;

fn evaluate(
    filter: &str,
    input: Value,
    budget: &mut Budget,
    invoker: &dyn CapabilityInvoker,
) -> Result<Value, CommandFailure> {
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
        let writer = scope.spawn(move || -> io::Result<()> {
            let mut stdin = io::BufWriter::new(stdin);
            serde_json::to_writer(&mut stdin, &(filter, input))?;
            stdin.flush()
        });
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
        let maximum = budget
            .max_value_bytes()
            .saturating_sub(budget.value_bytes());
        let reader = scope.spawn(move || {
            if let Err(error) = read_outputs(stdout, maximum, &sender) {
                let _sent = sender.send(ReadOutput::Invalid(format!(
                    "jq: could not read worker output: {error}"
                )));
            }
        });
        let result = collect(&receiver, budget, invoker);
        drop(receiver);
        if result.is_err() {
            drop(child);
            let _reader_result = reader.join();
            let _writer_result = writer.join();
            let _stderr_result = errors.join();
            return result.map(reduce);
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
        if let Ok(Err(error)) = written {
            return Err(CommandFailure::failed(format!(
                "jq: could not send worker input: {error}"
            )));
        }
        result.map(reduce)
    })
}

fn reduce(outputs: Vec<Value>) -> Value {
    match outputs.len() {
        0 => Value::Null,
        1 => outputs.into_iter().next().unwrap_or(Value::Null),
        _ => Value::Array(outputs),
    }
}

pub(crate) fn run_filter(
    filter: &str,
    input: Val,
    output: &mut impl io::Write,
) -> Result<(), String> {
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
        .map_err(|errors| format!("jq: invalid filter: {}", describe_load_errors(&errors)))?;
    let compiled = Compiler::default()
        .with_funs(functions)
        .compile(modules)
        .map_err(|errors| format!("jq: invalid filter: {}", describe_compile_errors(&errors)))?;

    let context = Ctx::<data::JustLut<Val>>::new(&compiled.lut, Vars::new([]));
    for result in compiled.id.run((context, input)) {
        let produced = result.map_err(describe_exception)?;
        let value = convert(&produced, 0)?;
        serde_json::to_writer(&mut *output, &value)
            .map_err(|error| format!("jq: could not write output: {error}"))?;
        output
            .write_all(b"\n")
            .map_err(|error| format!("jq: could not write output: {error}"))?;
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
