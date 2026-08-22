//! The `jq` builtin, backed by the real jaq interpreter.
//!
//! This wraps `jaq-core`, `jaq-std`, and `jaq-json` as a library rather than hand-rolling a jq
//! subset. A model that knows jq gets jq, not an approximation of it that quietly differs.
//!
//! Embedding a complete, Turing-complete language inside a sandbox needs two things the rest of
//! this crate does not, and [`evaluate`] handles both:
//!
//! - jq's standard library reaches the host. `jaq_std::funs()` exports `env`, which returns
//!   [`std::env::vars`] — a script could dump the host process environment and post it through
//!   `curl`, defeating this crate's "never reads the host process environment" guarantee outright.
//!   The function set is therefore filtered by name rather than taken wholesale.
//! - jaq has no fuel meter and offers no safe point to interrupt from outside, so nothing in a
//!   tree-walking evaluator can stop `jq 'def f: f; f'`. Every other builtin returns to the
//!   evaluator often enough for the budget to bite; this one need not return at all.
//!
//! The filter therefore runs on a worker thread and the outputs come back over a rendezvous
//! channel. The evaluator charges each output against the step and value-byte budgets, and waits
//! for the next one only until the script's deadline. The cost is stated plainly: a filter that is
//! still running when the deadline passes is *abandoned*, not stopped. That is a worse outcome than
//! a fuel meter and a better one than a runner that hangs forever, and it makes the wall-clock
//! bound this crate advertises true for `jq` as well.
//!
//! # The worker is per thread, not per filter
//!
//! `jq` is the hottest builtin in model-written scripts — `curl ... | jq ...` is the shape of most
//! of them — and each call is one step of a script's budget. Spawning and joining an operating-
//! system thread for every one of those was the largest fixed cost in the builtin, so a thread that
//! has run a filter keeps its worker parked on the job channel and hands it the next one.
//!
//! [`retire`] is what keeps that safe. A worker inside a filter nobody can stop must never be
//! offered another, so the same act that charges an abandonment also drops this thread's handle:
//! the next filter gets a freshly spawned worker, and the abandoned one exits by itself if its
//! filter ever returns and finds nobody left to serve. A worker that died with a panicking filter
//! is answered identically, one send failure later.
//!
//! # Values cross as values
//!
//! Both sides of the boundary speak [`serde_json::Value`], and jaq's [`Val`] implements
//! `Deserialize`, so the input is deserialized straight into a `Val` — moving each string's buffer
//! rather than copying it — and each output is converted back structurally. Rendering the input to
//! JSON text, re-parsing it, rendering every output to text, and re-parsing *that* was four full
//! passes over a payload that is routinely multiple kilobytes.
//!
//! One thing is lost with the text and restored deliberately: `serde_json` refuses to parse more
//! than [`MAX_OUTPUT_DEPTH`] nested containers, and that ceiling was the only thing standing
//! between a filter like `reduce range(100000) as $i (.; [.])` and a recursive conversion deep
//! enough to abort the host process. [`convert`] applies it itself.
//!
//! # What an abandoned worker costs, and what bounds it
//!
//! Abandonment is not uniformly expensive. Dropping the receiver disconnects the channel, so a
//! filter that produces *any* output fails its next `send` and returns — which is the cooperative
//! cancellation check, sited at the only place jaq hands control back. A wrapping iterator over
//! `compiled.id.run()` would stop such a filter one output earlier and nothing more.
//!
//! The residual is the filter that never yields at all: `jq 'def f: f; f'`, `jq 'last(repeat(0))'`.
//! jaq offers no interruption point inside it, so its thread spins at 100% of a core until the
//! process exits. In a long-lived host with a one-core limit that is not a leak to discover from a
//! flame graph, so two things bound it here:
//!
//! - every abandonment logs a `tracing::warn!` carrying the elapsed time and this process's running
//!   total, and [`crate::abandoned_filter_workers`] exposes how many are still going, and
//! - once [`MAX_ABANDONED_WORKERS`] of them are outstanding, `jq` refuses to start another filter
//!   rather than adding one more spinning thread to a host that is already saturated.
//!
//! The count is of *live* abandoned workers, not of abandonments: one that notices the closed
//! channel decrements it immediately, so a script that merely exhausted its value budget does not
//! spend the process's allowance.

use std::{
    cell::{Cell, RefCell},
    io,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    },
    time::{Duration, Instant},
};

use jaq_core::{
    Compiler, Ctx, Vars, data,
    load::{Arena, File, Loader},
    unwrap_valr,
};
use jaq_json::{Num, Val};
use serde_json::Value;

use super::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag};
use crate::limits::Budget;

/// jq standard-library filters that reach outside this interpreter's value space.
///
/// `env` reads the host process environment; `now` reads the host wall clock. Neither is
/// reachable through any other path in this crate, and a script that names one gets jaq's ordinary
/// "undefined filter" error rather than a silent answer.
const HOST_REACHING_FILTERS: &[&str] = &["env", "now"];

/// `jq [-r|--raw-output] FILTER`.
pub(crate) struct Jq;

impl Builtin for Jq {
    fn name(&self) -> &'static str {
        "jq"
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
                // `-r` and `-c` are accepted and documented as no-ops rather than rejected: the
                // value model already emits string results verbatim and renders structures
                // compactly, so raw compact output is this shell's only output mode. Nothing is
                // silently different from what these flags request.
                "-r" | "--raw-output" | "-c" | "--compact-output" => {}
                flag if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(unsupported_flag("jq", flag));
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

        evaluate(&filter, input.unwrap_or(Value::Null), context.budget).map(CommandResult::value)
    }
}

/// How many abandoned filter workers this process tolerates before refusing to start another.
///
/// Only workers that never yield can accumulate here, and each one is a core spinning until the
/// process exits. On the one-core deployment this crate is embedded in, four is already most of the
/// machine — past that, the honest answer to a new filter is that there is nothing left to run it
/// with, rather than one more thread nobody can stop.
const MAX_ABANDONED_WORKERS: usize = 4;

/// Abandoned filter workers that have not yet noticed nobody is listening.
static ABANDONED_WORKERS: AtomicUsize = AtomicUsize::new(0);

/// Every abandonment this process has seen, for the warning's running total.
static TOTAL_ABANDONMENTS: AtomicU64 = AtomicU64::new(0);

/// Returns how many abandoned `jq` filter workers are still running in this process.
///
/// See [`crate::abandoned_filter_workers`], which is this counter's public face.
pub(crate) fn abandoned_workers() -> usize {
    ABANDONED_WORKERS.load(Ordering::SeqCst)
}

/// One *filter's* liveness, shared with the evaluator paying for it.
///
/// Scoped to a job rather than to the thread serving it: a worker outlives the filters it runs, and
/// what is charged, released, and counted is a filter nobody could stop.
///
/// Whichever side reaches the end first wins the exchange: the evaluator charges an abandonment
/// only when the filter had not already returned, and the worker releases that charge only when its
/// filter was in fact the one abandoned. Without the exchange, a filter that finishes in the same
/// instant the deadline trips would be counted as spinning forever.
struct Worker(AtomicU8);

impl Worker {
    const RUNNING: u8 = 0;
    const FINISHED: u8 = 1;
    const ABANDONED: u8 = 2;

    fn new() -> Self {
        Self(AtomicU8::new(Self::RUNNING))
    }

    /// Called from the worker thread when its filter is done, however it ended.
    ///
    /// Returns whether this released an abandonment charged against the process.
    fn finish(&self) -> bool {
        if self
            .0
            .compare_exchange(
                Self::RUNNING,
                Self::FINISHED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            return false;
        }
        ABANDONED_WORKERS.fetch_sub(1, Ordering::SeqCst);
        true
    }

    /// Called from the evaluator when it stops waiting.
    ///
    /// Returns this process's running abandonment total when the worker really was still going, and
    /// `None` when it had already returned and nothing outlives the command.
    fn abandon(&self) -> Option<u64> {
        self.0
            .compare_exchange(
                Self::RUNNING,
                Self::ABANDONED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .ok()?;
        ABANDONED_WORKERS.fetch_add(1, Ordering::SeqCst);
        Some(
            TOTAL_ABANDONMENTS
                .fetch_add(1, Ordering::SeqCst)
                .saturating_add(1),
        )
    }
}

/// Refuses a new filter once too many abandoned workers are still burning CPU.
///
/// A recoverable failure rather than a fatal one: the script sees `jq` fail, writes the reason, and
/// carries on with whatever it can still do. Ending the whole script would punish it for a filter
/// an earlier one wrote.
fn admit(outstanding: usize) -> Result<(), CommandFailure> {
    if outstanding < MAX_ABANDONED_WORKERS {
        return Ok(());
    }
    Err(CommandFailure::failed(format!(
        "jq: refusing to start another filter: {outstanding} filter workers abandoned by earlier \
         non-terminating filters are still running in this process"
    )))
}

/// Marks the worker finished when its thread returns, including through a panic.
struct FinishOnDrop(Arc<Worker>);

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        let _released = self.0.finish();
    }
}

/// One message from the filter worker to the evaluator.
enum Produced {
    /// One output value, and the size of the JSON text it stands for.
    Output {
        /// The output, converted into this shell's value model.
        value: Value,
        /// What that value's JSON encoding weighs, for the value-byte budget.
        bytes: u64,
    },
    /// The filter could not be compiled, failed while running, or produced a value JSON has no
    /// form for.
    Failed(String),
    /// The filter's stream ended normally.
    Done,
}

/// Why the evaluator stopped collecting outputs.
enum Stopped {
    /// The worker reported the failure itself and is on its way out.
    Worker(CommandFailure),
    /// The evaluator gave up first, so the filter is still running.
    Evaluator(CommandFailure),
}

/// One filter to run, handed to a worker over its job channel.
struct Job {
    filter: String,
    input: Value,
    /// This filter's liveness, shared with the evaluator paying for it.
    worker: Arc<Worker>,
    /// Where this filter's outputs go. Dropping the far end is what tells it to stop.
    outputs: SyncSender<Produced>,
}

thread_local! {
    /// This thread's parked filter worker, if it has one.
    ///
    /// Per thread rather than per process: scripts run concurrently on separate threads, and one
    /// shared worker would serialize every `jq` in the host behind whichever script got there
    /// first. Nothing here is `Sync`, which is the type system agreeing.
    static WORKER: RefCell<Option<SyncSender<Job>>> = const { RefCell::new(None) };

    /// How many workers this thread has had to spawn, which is what reuse is measured against.
    static SPAWNED: Cell<u64> = const { Cell::new(0) };
}

/// Returns how many filter workers the calling thread has spawned.
#[cfg(test)]
fn workers_spawned() -> u64 {
    SPAWNED.get()
}

/// Hands one job to this thread's worker, spawning one when there is none to hand it to.
///
/// The handle is dropped rather than repaired whenever its worker becomes unusable — abandoned in
/// [`evaluate`], or dead with a panicking filter here — so a send that fails means the worker is
/// gone and a replacement is owed. Capacity one rather than a rendezvous: a worker that has just
/// sent its last output is on its way back to the job channel but need not have arrived, and the
/// caller has no reason to wait for that.
fn submit(job: Job) -> Result<(), CommandFailure> {
    let job = WORKER.with_borrow(|worker| match worker {
        Some(jobs) => jobs.send(job).err().map(|returned| returned.0),
        None => Some(job),
    });
    let Some(job) = job else {
        return Ok(());
    };

    let (jobs, queue) = sync_channel::<Job>(1);
    std::thread::Builder::new()
        .name("dekopon-shell-jq".to_owned())
        .spawn(move || serve(&queue))
        .map_err(|error| {
            CommandFailure::failed(format!("jq: could not start the filter evaluator: {error}"))
        })?;
    SPAWNED.set(SPAWNED.get().saturating_add(1));
    // A fresh worker is parked on an empty channel of capacity one, so this cannot block and
    // cannot fail.
    let sent = jobs.send(job);
    WORKER.replace(Some(jobs));
    #[allow(
        clippy::map_err_ignore,
        reason = "SendError hands back the job nobody received and says nothing else; the worker \
                  died before serving it, which the message already states"
    )]
    sent.map_err(|_| CommandFailure::failed("jq: the filter evaluator stopped before it started"))
}

/// Gives up this thread's worker, so the next filter is served by a new one.
fn retire() {
    WORKER.replace(None);
}

/// Runs whatever filters this worker's thread is given, until nobody is left to give it any.
fn serve(queue: &Receiver<Job>) {
    while let Ok(Job {
        filter,
        input,
        worker,
        outputs,
    }) = queue.recv()
    {
        // Scoped to one job: however this filter ends, an abandonment charged against it is
        // released here, and the next job starts with its own liveness.
        let _finish = FinishOnDrop(worker);
        let message = match run_filter(&filter, input, &outputs) {
            Ok(()) => Produced::Done,
            Err(message) => Produced::Failed(message),
        };
        // A closed receiver means the evaluator already gave up on this filter.
        #[allow(
            clippy::let_underscore_must_use,
            reason = "a closed receiver is the normal end of a filter the budget cut short, \
                      and the returned SendError only hands back the message nobody is left \
                      to read; this worker has no caller to report to either way"
        )]
        let _ = outputs.send(message);
    }
}

/// Compiles and runs one jq filter over one value under the script's budget.
///
/// See the module documentation for why this crosses a thread boundary, and why it refuses to cross
/// it at all once this process is carrying too many workers it can no longer stop.
pub(crate) fn evaluate(
    filter: &str,
    input: Value,
    budget: &mut Budget,
) -> Result<Value, CommandFailure> {
    admit(abandoned_workers())?;

    // A rendezvous channel, so the filter cannot run ahead of the budget that is paying for it:
    // every output waits until the evaluator has charged the previous one.
    let (sender, receiver) = sync_channel::<Produced>(0);
    let worker = Arc::new(Worker::new());
    submit(Job {
        filter: filter.to_owned(),
        input,
        worker: Arc::clone(&worker),
        outputs: sender,
    })?;

    let started = Instant::now();
    match collect(&receiver, budget) {
        Ok(outputs) => Ok(reduce(outputs)),
        Err(Stopped::Worker(failure)) => Err(failure),
        Err(Stopped::Evaluator(failure)) => {
            if let Some(total) = worker.abandon() {
                // This worker is inside a filter it may never leave, so it stops being this
                // thread's worker. It exits by itself if the filter does return, and the next
                // `jq` on this thread starts from a new one either way.
                retire();
                tracing::warn!(
                    event = "shell_jq_filter_abandoned",
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    abandoned_total = total,
                    abandoned_live = abandoned_workers(),
                    "a jq filter outlived the budget that was paying for it; its worker stops at \
                     its next output, or runs until this process exits if it produces none"
                );
            }
            Err(failure)
        }
    }
}

/// Pulls outputs off the channel, charging each one, until the filter or the budget ends.
fn collect(receiver: &Receiver<Produced>, budget: &mut Budget) -> Result<Vec<Value>, Stopped> {
    let mut outputs = Vec::new();
    loop {
        // Never wait for zero: `remaining` reaching zero one tick before `check_deadline` agrees
        // would otherwise spin instead of waiting.
        let wait = budget.remaining().max(Duration::from_millis(1));
        match receiver.recv_timeout(wait) {
            Ok(Produced::Output { value, bytes }) => {
                // Pulling one value is where a filter's work happens, so each pull is a step and
                // re-reads the deadline. Without this a whole `jq` command cost exactly one step.
                budget
                    .charge_step()
                    .map_err(|limit| Stopped::Evaluator(limit.into()))?;
                budget
                    .charge_value_bytes(bytes)
                    .map_err(|limit| Stopped::Evaluator(limit.into()))?;
                outputs.push(value);
            }
            Ok(Produced::Failed(message)) => {
                return Err(Stopped::Worker(CommandFailure::failed(message)));
            }
            Ok(Produced::Done) => return Ok(outputs),
            Err(RecvTimeoutError::Timeout) => {
                budget
                    .check_deadline()
                    .map_err(|limit| Stopped::Evaluator(limit.into()))?;
                // The clock has not actually passed the deadline, so keep waiting for the filter.
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Stopped::Worker(CommandFailure::failed(
                    "jq: the filter evaluator stopped without producing a result",
                )));
            }
        }
    }
}

/// Reduces a filter's output stream to one value.
///
/// A jq filter is a stream. One output stays scalar so `| jq .field | grep x` reads naturally;
/// several outputs become a JSON array so nothing is silently discarded.
fn reduce(outputs: Vec<Value>) -> Value {
    match outputs.len() {
        0 => Value::Null,
        1 => outputs.into_iter().next().unwrap_or(Value::Null),
        _ => Value::Array(outputs),
    }
}

/// Compiles one filter and streams its outputs, on the worker thread.
fn run_filter(filter: &str, input: Value, sender: &SyncSender<Produced>) -> Result<(), String> {
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

    // Consuming rather than borrowing: `serde_json`'s owning deserializer moves each string's
    // buffer into the `Val` that replaces it, so a multi-kilobyte payload changes hands without
    // being copied, and the original is gone before the filter starts.
    let value = serde_json::from_value::<Val>(input)
        .map_err(|error| format!("jq: invalid input: {error}"))?;

    let context = Ctx::<data::JustLut<Val>>::new(&compiled.lut, Vars::new([]));
    for result in compiled.id.run((context, value)).map(unwrap_valr) {
        let produced = result.map_err(|error| format!("jq: {error}"))?;
        let value = convert(&produced, 0)?;
        let bytes = weigh(&value);
        // A closed receiver means the evaluator abandoned this filter, so there is nothing left
        // to compute for.
        if sender.send(Produced::Output { value, bytes }).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

/// How deeply a filter's output may nest before `jq` refuses it.
///
/// `serde_json` applies exactly this ceiling when parsing, so it is what bounded this conversion
/// while every output crossed the boundary as JSON text. It has to be kept: without it a filter
/// like `reduce range(100000) as $i (.; [.])` recurses once per level through [`convert`] on the
/// native stack, and aborting the host process is not one of the outcomes this crate may produce.
const MAX_OUTPUT_DEPTH: usize = 128;

/// Converts one filter output into this shell's value model.
///
/// jaq's value type is a JSON *superset* — byte strings, non-string object keys, `NaN` — and its
/// own writer documents that printing such a value deliberately yields text that is not valid
/// JSON. Reading that text back is what used to refuse them, so this refuses them too rather than
/// inventing a JSON meaning none of them has.
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
        // Lossy for the same reason jaq's own formatter is: a text string holds bytes that are
        // only *interpreted* as UTF-8, and JSON has no way to carry the ones that are not.
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

/// Converts one jaq number, which knows more shapes than a JSON number does.
fn convert_number(number: &Num) -> Result<Value, String> {
    match number {
        Num::Int(int) => i64::try_from(*int)
            .map(|int| Value::Number(int.into()))
            .or_else(|_| convert_written_number(number)),
        Num::Float(float) => serde_json::Number::from_f64(*float)
            .map(Value::Number)
            .ok_or_else(|| not_json(&number.to_string())),
        // A big integer and a decimal literal both keep the text jaq preserved for them, and
        // reading that text is precisely what happened when every output crossed as JSON. `1e3`,
        // an integer past `i64`, and `1.50` therefore land on the values they always did.
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

/// Counts what one value's JSON encoding weighs without building it.
///
/// The value-byte budget charges a `jq` output for the JSON it stands for, which is what the
/// rendered text used to supply for free. Serializing into a counter keeps that meaning and keeps
/// the allocation this whole path exists to avoid.
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
    // Infallible: `Meter` never fails, and a `Value` is always serializable.
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

/// One file's compilation errors, as `jaq-core` reports them.
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
    use std::sync::atomic::Ordering;

    use serde_json::{Value, json};

    use crate::limits::{Budget, Limits};

    use super::{
        CommandFailure, MAX_ABANDONED_WORKERS, MAX_OUTPUT_DEPTH, TOTAL_ABANDONMENTS, Worker,
        abandoned_workers, admit, evaluate, workers_spawned,
    };

    fn filter(filter: &str, input: Value) -> Result<Value, CommandFailure> {
        evaluate(filter, input, &mut Budget::start(Limits::default()))
    }

    fn message(failure: CommandFailure) -> String {
        match failure {
            CommandFailure::Status { message, .. } => message,
            CommandFailure::Fatal(fatal) => format!("{fatal:?}"),
        }
    }

    #[test]
    fn evaluates_real_jq_filters() {
        assert_eq!(
            filter(".a", json!({"a": 1})).expect("filter runs"),
            json!(1)
        );
        assert_eq!(
            filter("map(. * 2)", json!([1, 2, 3])).expect("filter runs"),
            json!([2, 4, 6])
        );
        assert_eq!(
            filter(
                "{name: .id, total: (.items | length)}",
                json!({"id": "x", "items": [1, 2]})
            )
            .expect("filter runs"),
            json!({"name": "x", "total": 2})
        );
    }

    #[test]
    fn a_thread_reuses_its_filter_worker() {
        // The point of the whole worker arrangement: `curl ... | jq ...` in a loop is the shape of
        // most model-written scripts, and each iteration used to spawn and join an operating-system
        // thread. libtest gives every test its own thread, so this count is this test's alone.
        for _ in 0..8 {
            assert_eq!(
                filter(".a", json!({"a": 1})).expect("filter runs"),
                json!(1)
            );
        }
        assert_eq!(workers_spawned(), 1);
    }

    #[test]
    fn an_abandoned_worker_is_replaced_instead_of_being_handed_the_next_filter() {
        // A worker the evaluator gave up on is inside a filter nothing can interrupt, so offering
        // it another would queue behind a thread that may never come back. This one does come back
        // — it notices the closed channel at its next output — but the decision cannot wait to find
        // that out, so the handle goes either way and the next filter gets a fresh worker.
        assert_eq!(filter(".", json!(1)).expect("filter runs"), json!(1));
        assert_eq!(workers_spawned(), 1);

        let mut budget = Budget::start(Limits {
            max_steps: 4,
            ..Limits::default()
        });
        evaluate("range(1000000)", json!(null), &mut budget)
            .expect_err("a long stream exhausts the budget");

        assert_eq!(filter(".", json!(2)).expect("filter runs"), json!(2));
        assert_eq!(workers_spawned(), 2);
    }

    #[test]
    fn numbers_keep_the_values_the_json_boundary_used_to_give_them() {
        // Values now cross as values rather than as JSON text, and jaq knows number shapes JSON
        // does not: machine and big integers, floats, and decimal literals kept as written.
        assert_eq!(filter(".a", json!({"a": 1})).expect("runs"), json!(1));
        assert_eq!(filter(".a", json!({"a": -7})).expect("runs"), json!(-7));
        assert_eq!(filter(".a", json!({"a": 1.5})).expect("runs"), json!(1.5));
        assert_eq!(filter(".a", json!({"a": 1.0})).expect("runs"), json!(1.0));
        assert_eq!(filter("1 + 1", Value::Null).expect("runs"), json!(2));
        assert_eq!(filter("3 / 2", Value::Null).expect("runs"), json!(1.5));
        // A decimal literal is kept as text by jaq and read back as the number it spells.
        assert_eq!(filter("1.50", Value::Null).expect("runs"), json!(1.5));
        assert_eq!(filter("1e3", Value::Null).expect("runs"), json!(1000.0));
        // Past `i64` jaq switches to a big integer; its written form is what JSON always saw.
        assert_eq!(
            filter("10000000000000000000 + 1", Value::Null).expect("runs"),
            json!(10_000_000_000_000_000_001_u64)
        );
        // The round trip through JSON text used to cost a float this last bit: `serde_json` parses
        // the shortest round-tripping form of 2^70 one ULP low. Handing the float over directly is
        // exact instead.
        assert_eq!(
            filter("pow(2; 70)", Value::Null).expect("runs"),
            json!(2f64.powi(70))
        );
        assert_eq!(filter("null", Value::Null).expect("runs"), Value::Null);
        assert_eq!(filter("true", Value::Null).expect("runs"), json!(true));
        assert_eq!(
            filter(".s", json!({"s": "text"})).expect("runs"),
            json!("text")
        );
        assert_eq!(
            filter(".", json!({"a": {"b": [1, {"c": null}]}})).expect("runs"),
            json!({"a": {"b": [1, {"c": null}]}})
        );
    }

    #[test]
    fn a_value_json_has_no_form_for_is_refused_rather_than_invented() {
        // jaq's value type is a JSON superset and its own writer says so: printing one of these
        // deliberately produces text that is not JSON. Reading that text back is what used to
        // refuse them, so converting structurally has to refuse them too — giving `NaN` the `null`
        // jq would is a different answer, not a faster one.
        for (source, expected) in [
            ("nan", "NaN"),
            ("infinite", "Infinity"),
            (r#""a" | tobytes"#, "a byte string"),
            ("{(1): 2}", "an object with a non-string key"),
        ] {
            let failure = filter(source, Value::Null).expect_err(source);
            let message = message(failure);
            assert!(message.starts_with("jq: a filter produced"), "{message}");
            assert!(message.contains(expected), "{source}: {message}");
        }
    }

    #[test]
    fn output_nesting_is_bounded_the_way_parsing_it_used_to_be() {
        // `serde_json` refuses more than 128 nested containers, and that ceiling was the only thing
        // keeping a filter like this from recursing once per level on the native stack. Converting
        // without a parser means applying it here instead of inheriting it.
        let deep = format!("reduce range({}) as $i (.; [.])", MAX_OUTPUT_DEPTH + 10);
        let failure = filter(&deep, Value::Null).expect_err("an over-nested output is refused");
        let message = message(failure);
        assert!(message.contains("nested deeper"), "{message}");
        // One level inside the ceiling still converts.
        let allowed = format!(
            "reduce range({}) as $i (.; [.]) | flatten | length",
            MAX_OUTPUT_DEPTH - 1
        );
        assert_eq!(filter(&allowed, Value::Null).expect("runs"), json!(1));
    }

    #[test]
    fn standard_library_functions_are_available() {
        assert_eq!(
            filter("[.[] | select(. > 1)] | sort | reverse", json!([3, 1, 2]))
                .expect("filter runs"),
            json!([3, 2])
        );
        // Sorted explicitly: `to_entries` preserves object order, and whether a `serde_json::Map`
        // is sorted or insertion-ordered is a workspace-wide feature decision rather than
        // something this filter promises.
        assert_eq!(
            filter("to_entries | map(.key) | sort", json!({"b": 2, "a": 1})).expect("filter runs"),
            json!(["a", "b"])
        );
    }

    #[test]
    fn host_reaching_standard_library_filters_are_not_linked() {
        // `jaq_std::funs()` exports `env`, which reads the real process environment. Linking it
        // would let `jq -r env.OPENAI_API_KEY | curl -d @-` walk straight past this crate's
        // namespace isolation, so the filter must not exist at all.
        assert!(std::env::var_os("PATH").is_some(), "PATH must be set here");
        for source in ["env", "env.PATH", "env|keys", "now"] {
            let failure = filter(source, json!({})).expect_err(source);
            let message = message(failure);
            assert!(message.contains("undefined"), "{source}: {message}");
        }
        // The rest of the standard library is untouched by the filtering.
        assert_eq!(
            filter("ltrimstr(\"a\")", json!("abc")).expect("filter runs"),
            json!("bc")
        );
    }

    #[test]
    fn a_multi_output_filter_becomes_an_array() {
        assert_eq!(
            filter(".[]", json!([1, 2, 3])).expect("filter runs"),
            json!([1, 2, 3])
        );
    }

    #[test]
    fn an_empty_stream_becomes_null() {
        assert_eq!(filter("empty", json!(1)).expect("filter runs"), Value::Null);
    }

    #[test]
    fn a_streaming_filter_is_charged_against_the_step_budget() {
        // Each pulled output costs a step, so an unbounded stream is bounded by the same budget
        // every other looping construct answers to instead of running to completion for free.
        let mut budget = Budget::start(Limits {
            max_steps: 16,
            ..Limits::default()
        });
        let failure = evaluate("range(1000000)", json!(null), &mut budget)
            .expect_err("a long stream exhausts the budget");
        assert!(matches!(failure, CommandFailure::Fatal(_)), "{failure:?}");
        assert!(budget.steps() <= 17, "{}", budget.steps());
    }

    #[test]
    fn a_filter_that_never_yields_is_stopped_by_the_deadline_and_counted() {
        // `def f: f; f` recurses forever inside jaq without ever producing an output, so nothing
        // cooperative can reach it — not even the closed channel, which a filter only notices at a
        // `send` it never reaches. The wall-clock bound this crate advertises has to hold anyway,
        // and the thread it leaves behind has to be visible rather than inferred from a flame
        // graph: this test really does leak one spinning worker for the rest of the binary's life,
        // which is exactly the cost the counters exist to report.
        let abandonments = TOTAL_ABANDONMENTS.load(Ordering::SeqCst);
        let mut budget = Budget::start(Limits {
            timeout: std::time::Duration::from_millis(50),
            ..Limits::default()
        });
        let started = std::time::Instant::now();
        let failure = evaluate("def f: f; f", json!(1), &mut budget)
            .expect_err("a non-terminating filter trips the deadline");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(matches!(failure, CommandFailure::Fatal(_)), "{failure:?}");
        assert!(message(failure).contains("Deadline"));

        // Strictly greater rather than exactly one more: an abandonment is process-wide, and the
        // other tests in this binary produce their own.
        assert!(TOTAL_ABANDONMENTS.load(Ordering::SeqCst) > abandonments);
        // This one can never come back, so it stays counted against the process.
        assert!(abandoned_workers() >= 1);
    }

    #[test]
    fn a_saturated_process_refuses_to_start_another_filter() {
        // Every abandoned worker that cannot come back is a core spinning until the process exits,
        // so past the ceiling the honest answer is that there is nothing left to run a filter with.
        // Driven through `admit` rather than by leaking four real workers: this test suite would
        // then refuse every later `jq` in it, which is the failure being guarded against.
        assert!(admit(MAX_ABANDONED_WORKERS - 1).is_ok());
        let failure = admit(MAX_ABANDONED_WORKERS).expect_err("a saturated process refuses");
        assert!(
            matches!(failure, CommandFailure::Status { .. }),
            "the script continues; only this filter is refused: {failure:?}"
        );
        let message = message(failure);
        assert!(
            message.contains("refusing to start another filter"),
            "{message}"
        );
    }

    #[test]
    fn an_abandoned_worker_stops_counting_once_it_finally_returns() {
        // The common abandonment is benign: a filter that produced output fails its next `send` and
        // returns within microseconds. Charging that permanently against the process would let a
        // script that merely exhausted its value budget disable `jq` for every later session.
        let worker = Worker::new();
        assert!(worker.abandon().is_some());
        assert!(
            worker.finish(),
            "returning releases the abandonment it was charged"
        );
    }

    #[test]
    fn a_worker_that_finished_first_is_not_counted_as_abandoned() {
        // The evaluator gives up and the worker returns in the same instant often enough to matter,
        // and a filter that reported its own error is already on its way out. Whichever side wins
        // the exchange, the count must end where it started.
        let worker = Worker::new();
        assert!(!worker.finish());
        assert!(worker.abandon().is_none());
    }

    #[test]
    fn a_filter_cannot_outgrow_the_value_byte_ceiling() {
        let mut budget = Budget::start(Limits {
            max_value_bytes: 1_024,
            ..Limits::default()
        });
        let failure = evaluate("range(100000) | tostring", json!(null), &mut budget)
            .expect_err("an oversized stream trips the value ceiling");
        assert!(matches!(failure, CommandFailure::Fatal(_)), "{failure:?}");
    }

    #[test]
    fn raw_and_compact_flags_are_accepted_because_they_match_the_only_output_mode() {
        use crate::builtins::test_support::run_builtin;

        for flags in [
            vec!["-r", ".a"],
            vec!["-c", ".a"],
            vec!["--raw-output", ".a"],
            vec!["--compact-output", ".a"],
        ] {
            let result = run_builtin(&super::Jq, &flags, Some(json!({"a": "x"})))
                .expect("documented output flags are accepted");
            assert_eq!(result.value, json!("x"), "{flags:?}");
        }
        assert!(run_builtin(&super::Jq, &["--slurp", "."], Some(json!(1))).is_err());
    }

    #[test]
    fn invalid_filters_report_an_error_instead_of_panicking() {
        let error = message(filter(".[", json!({})).expect_err("unbalanced filter"));
        assert!(error.starts_with("jq: invalid filter"), "{error}");
        let error = message(filter("no_such_function", json!({})).expect_err("undefined filter"));
        assert!(error.contains("undefined"), "{error}");
    }

    #[test]
    fn runtime_errors_are_reported_not_fatal() {
        let failure = filter(".a", json!([1, 2])).expect_err("indexing an array by name fails");
        assert!(
            matches!(failure, CommandFailure::Status { .. }),
            "{failure:?}"
        );
        assert!(message(failure).starts_with("jq:"));
    }
}
