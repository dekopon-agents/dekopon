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
    Compiler, Ctx, Exn, Vars, data,
    load::{Arena, File, Loader},
};
use jaq_json::{Num, Val};
use serde_json::Value;

use super::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag};
use crate::limits::Budget;

/// env reads the host process environment and now reads the host wall clock; neither is reachable
/// any other way in this crate, so both are excluded from the filter set.
const HOST_REACHING_FILTERS: &[&str] = &["env", "now"];

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

/// A soft threshold, not a reservation, so admitted filters can overshoot it; on this crate's
/// one-core deployment, four already-spinning cores is most of the machine.
const MAX_ABANDONED_WORKERS: usize = 4;

static ABANDONED_WORKERS: AtomicUsize = AtomicUsize::new(0);

static TOTAL_ABANDONMENTS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn abandoned_workers() -> usize {
    ABANDONED_WORKERS.load(Ordering::SeqCst)
}

struct Worker(AtomicU8);

impl Worker {
    const RUNNING: u8 = 0;
    const FINISHED: u8 = 1;
    const ABANDONED: u8 = 2;

    fn new() -> Self {
        Self(AtomicU8::new(Self::RUNNING))
    }

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

    fn abandon(&self) -> Option<u64> {
        // Charge before publishing `ABANDONED`: `finish` releases the charge as soon as it sees that
        // state, and releasing first would wrap the count below zero. A lost exchange undoes the
        // charge, so the count can briefly read one high, never low.
        ABANDONED_WORKERS.fetch_add(1, Ordering::SeqCst);
        if self
            .0
            .compare_exchange(
                Self::RUNNING,
                Self::ABANDONED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            ABANDONED_WORKERS.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(
            TOTAL_ABANDONMENTS
                .fetch_add(1, Ordering::SeqCst)
                .saturating_add(1),
        )
    }
}

fn admit(outstanding: usize) -> Result<(), CommandFailure> {
    if outstanding < MAX_ABANDONED_WORKERS {
        return Ok(());
    }
    Err(CommandFailure::failed(format!(
        "jq: refusing to start another filter: {outstanding} filter workers abandoned by earlier \
         non-terminating filters are still running in this process"
    )))
}

struct FinishOnDrop(Arc<Worker>);

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        let _released = self.0.finish();
    }
}

enum Produced {
    Output { value: Value, bytes: u64 },
    Failed(String),
    Done,
}

enum Stopped {
    Worker(CommandFailure),
    Evaluator(CommandFailure),
}

struct Job {
    filter: String,
    input: Value,
    worker: Arc<Worker>,
    outputs: SyncSender<Produced>,
}

thread_local! {
    static WORKER: RefCell<Option<SyncSender<Job>>> = const { RefCell::new(None) };

    static SPAWNED: Cell<u64> = const { Cell::new(0) };
}

#[cfg(test)]
fn workers_spawned() -> u64 {
    SPAWNED.get()
}

fn submit(job: Job) -> Result<(), CommandFailure> {
    let job = WORKER.with_borrow(|worker| match worker {
        Some(jobs) => jobs.send(job).err().map(|returned| returned.0),
        None => Some(job),
    });
    let Some(job) = job else {
        return Ok(());
    };

    let (jobs, queue) = sync_channel::<Job>(1);
    #[expect(
        clippy::disallowed_methods,
        reason = "owner: one reused worker per shell thread, never joined because a non-yielding \
                  filter cannot be stopped; bound: MAX_ABANDONED_WORKERS plus the session ceiling"
    )]
    std::thread::Builder::new()
        .name("dekopon-shell-jq".to_owned())
        .spawn(move || serve(&queue))
        .map_err(|error| {
            CommandFailure::failed(format!("jq: could not start the filter evaluator: {error}"))
        })?;
    SPAWNED.set(SPAWNED.get().saturating_add(1));
    let sent = jobs.send(job);
    WORKER.replace(Some(jobs));
    #[allow(
        clippy::map_err_ignore,
        reason = "SendError hands back the job nobody received and says nothing else; the worker \
                  died before serving it, which the message already states"
    )]
    sent.map_err(|_| CommandFailure::failed("jq: the filter evaluator stopped before it started"))
}

fn retire() {
    WORKER.replace(None);
}

fn serve(queue: &Receiver<Job>) {
    while let Ok(Job {
        filter,
        input,
        worker,
        outputs,
    }) = queue.recv()
    {
        let _finish = FinishOnDrop(worker);
        let message = match run_filter(&filter, input, &outputs) {
            Ok(()) => Produced::Done,
            Err(message) => Produced::Failed(message),
        };
        #[allow(
            clippy::let_underscore_must_use,
            reason = "a closed receiver is the normal end of a filter the budget cut short, \
                      and the returned SendError only hands back the message nobody is left \
                      to read; this worker has no caller to report to either way"
        )]
        let _ = outputs.send(message);
    }
}

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

fn collect(receiver: &Receiver<Produced>, budget: &mut Budget) -> Result<Vec<Value>, Stopped> {
    let mut outputs = Vec::new();
    loop {
        // Never wait for zero: `remaining` reaching zero one tick before `check_deadline` agrees
        // would otherwise spin instead of waiting.
        let wait = budget.remaining().max(Duration::from_millis(1));
        match receiver.recv_timeout(wait) {
            Ok(Produced::Output { value, bytes }) => {
                // Each pulled value is charged as its own step and re-reads the deadline; otherwise
                // a whole jq command would cost exactly one step regardless of output count.
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
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Stopped::Worker(CommandFailure::failed(
                    "jq: the filter evaluator stopped without producing a result",
                )));
            }
        }
    }
}

fn reduce(outputs: Vec<Value>) -> Value {
    match outputs.len() {
        0 => Value::Null,
        1 => outputs.into_iter().next().unwrap_or(Value::Null),
        _ => Value::Array(outputs),
    }
}

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

    let value = serde_json::from_value::<Val>(input)
        .map_err(|error| format!("jq: invalid input: {error}"))?;

    let context = Ctx::<data::JustLut<Val>>::new(&compiled.lut, Vars::new([]));
    for result in compiled.id.run((context, value)) {
        let produced = result.map_err(describe_exception)?;
        let value = convert(&produced, 0)?;
        let bytes = weigh(&value);
        if sender.send(Produced::Output { value, bytes }).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

/// Stands in for jaq_core::unwrap_valr, which calls process::exit on halt; here that would kill the
/// whole gateway, so a halt becomes an ordinary jq failure instead.
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
/// host process.
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
        assert_eq!(filter(".a", json!({"a": 1})).expect("runs"), json!(1));
        assert_eq!(filter(".a", json!({"a": -7})).expect("runs"), json!(-7));
        assert_eq!(filter(".a", json!({"a": 1.5})).expect("runs"), json!(1.5));
        assert_eq!(filter(".a", json!({"a": 1.0})).expect("runs"), json!(1.0));
        assert_eq!(filter("1 + 1", Value::Null).expect("runs"), json!(2));
        assert_eq!(filter("3 / 2", Value::Null).expect("runs"), json!(1.5));
        assert_eq!(filter("1.50", Value::Null).expect("runs"), json!(1.5));
        assert_eq!(filter("1e3", Value::Null).expect("runs"), json!(1000.0));
        assert_eq!(
            filter("10000000000000000000 + 1", Value::Null).expect("runs"),
            json!(10_000_000_000_000_000_001_u64)
        );
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
        let deep = format!("reduce range({}) as $i (.; [.])", MAX_OUTPUT_DEPTH + 10);
        let failure = filter(&deep, Value::Null).expect_err("an over-nested output is refused");
        let message = message(failure);
        assert!(message.contains("nested deeper"), "{message}");
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
        assert_eq!(
            filter("to_entries | map(.key) | sort", json!({"b": 2, "a": 1})).expect("filter runs"),
            json!(["a", "b"])
        );
    }

    #[test]
    fn host_reaching_standard_library_filters_are_not_linked() {
        assert!(std::env::var_os("PATH").is_some(), "PATH must be set here");
        for source in ["env", "env.PATH", "env|keys", "now"] {
            let failure = filter(source, json!({})).expect_err(source);
            let message = message(failure);
            assert!(message.contains("undefined"), "{source}: {message}");
        }
        assert_eq!(
            filter("ltrimstr(\"a\")", json!("abc")).expect("filter runs"),
            json!("bc")
        );
    }

    #[test]
    fn halt_fails_the_command_instead_of_exiting_the_process() {
        for source in ["halt", "halt(3)", "\"x\" | halt_error", "1, halt, 2"] {
            let message = message(filter(source, json!({})).expect_err(source));
            assert!(message.contains("halt("), "{source}: {message}");
        }
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

        assert!(TOTAL_ABANDONMENTS.load(Ordering::SeqCst) > abandonments);
        assert!(abandoned_workers() >= 1);
    }

    #[test]
    fn a_saturated_process_refuses_to_start_another_filter() {
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
        let worker = Worker::new();
        assert!(worker.abandon().is_some());
        assert!(
            worker.finish(),
            "returning releases the abandonment it was charged"
        );
    }

    #[test]
    fn a_worker_that_finished_first_is_not_counted_as_abandoned() {
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
