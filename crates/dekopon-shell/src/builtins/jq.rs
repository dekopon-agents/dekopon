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
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, sync_channel},
    },
    time::{Duration, Instant},
};

use jaq_core::{
    Compiler, Ctx, Vars, data,
    load::{Arena, File, Loader},
    unwrap_valr,
};
use jaq_json::Val;
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

        let input = input.unwrap_or(Value::Null);
        evaluate(&filter, &input, context.budget).map(CommandResult::value)
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

/// One filter worker's liveness, shared with the evaluator paying for it.
///
/// Whichever side reaches the end first wins the exchange: the evaluator charges an abandonment
/// only when the worker had not already returned, and the worker releases that charge only when it
/// was in fact the one abandoned. Without the exchange, a filter that finishes in the same instant
/// the deadline trips would be counted as spinning forever.
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
    /// One output value, already rendered as JSON text.
    Output(String),
    /// The filter could not be compiled, or failed while running.
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

/// Compiles and runs one jq filter over one value under the script's budget.
///
/// See the module documentation for why this crosses a thread boundary, and why it refuses to cross
/// it at all once this process is carrying too many workers it can no longer stop.
pub(crate) fn evaluate(
    filter: &str,
    input: &Value,
    budget: &mut Budget,
) -> Result<Value, CommandFailure> {
    admit(abandoned_workers())?;

    // A rendezvous channel, so the filter cannot run ahead of the budget that is paying for it:
    // every output waits until the evaluator has charged the previous one.
    let (sender, receiver) = sync_channel::<Produced>(0);
    let worker = Arc::new(Worker::new());
    let owned = Arc::clone(&worker);
    let filter = filter.to_owned();
    let input = input.clone();
    std::thread::Builder::new()
        .name("dekopon-shell-jq".to_owned())
        .spawn(move || {
            let _finish = FinishOnDrop(owned);
            let message = match run_filter(&filter, &input, &sender) {
                Ok(()) => Produced::Done,
                Err(message) => Produced::Failed(message),
            };
            // A closed receiver means the evaluator already gave up on this filter.
            let _ = sender.send(message);
        })
        .map_err(|error| {
            CommandFailure::failed(format!("jq: could not start the filter evaluator: {error}"))
        })?;

    let started = Instant::now();
    match collect(&receiver, budget) {
        Ok(outputs) => Ok(reduce(outputs)),
        Err(Stopped::Worker(failure)) => Err(failure),
        Err(Stopped::Evaluator(failure)) => {
            if let Some(total) = worker.abandon() {
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
            Ok(Produced::Output(produced)) => {
                // Pulling one value is where a filter's work happens, so each pull is a step and
                // re-reads the deadline. Without this a whole `jq` command cost exactly one step.
                budget
                    .charge_step()
                    .map_err(|limit| Stopped::Evaluator(limit.into()))?;
                budget
                    .charge_value_bytes(produced.len() as u64)
                    .map_err(|limit| Stopped::Evaluator(limit.into()))?;
                outputs.push(serde_json::from_str::<Value>(&produced).map_err(|error| {
                    Stopped::Evaluator(CommandFailure::failed(format!(
                        "jq: could not read filter output: {error}"
                    )))
                })?);
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
fn run_filter(
    filter: &str,
    input: &Value,
    sender: &std::sync::mpsc::SyncSender<Produced>,
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

    let text =
        serde_json::to_string(input).map_err(|error| format!("jq: invalid input: {error}"))?;
    let value = jaq_json::read::parse_single(text.as_bytes())
        .map_err(|error| format!("jq: invalid input: {error}"))?;

    let context = Ctx::<data::JustLut<Val>>::new(&compiled.lut, Vars::new([]));
    for result in compiled.id.run((context, value)).map(unwrap_valr) {
        let produced = result.map_err(|error| format!("jq: {error}"))?;
        // A closed receiver means the evaluator abandoned this filter, so there is nothing left
        // to compute for.
        if sender.send(Produced::Output(produced.to_string())).is_err() {
            return Ok(());
        }
    }
    Ok(())
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
        CommandFailure, MAX_ABANDONED_WORKERS, TOTAL_ABANDONMENTS, Worker, abandoned_workers,
        admit, evaluate,
    };

    fn filter(filter: &str, input: &Value) -> Result<Value, CommandFailure> {
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
            filter(".a", &json!({"a": 1})).expect("filter runs"),
            json!(1)
        );
        assert_eq!(
            filter("map(. * 2)", &json!([1, 2, 3])).expect("filter runs"),
            json!([2, 4, 6])
        );
        assert_eq!(
            filter(
                "{name: .id, total: (.items | length)}",
                &json!({"id": "x", "items": [1, 2]})
            )
            .expect("filter runs"),
            json!({"name": "x", "total": 2})
        );
    }

    #[test]
    fn standard_library_functions_are_available() {
        assert_eq!(
            filter("[.[] | select(. > 1)] | sort | reverse", &json!([3, 1, 2]))
                .expect("filter runs"),
            json!([3, 2])
        );
        // Sorted explicitly: `to_entries` preserves object order, and whether a `serde_json::Map`
        // is sorted or insertion-ordered is a workspace-wide feature decision rather than
        // something this filter promises.
        assert_eq!(
            filter("to_entries | map(.key) | sort", &json!({"b": 2, "a": 1})).expect("filter runs"),
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
            let failure = filter(source, &json!({})).expect_err(source);
            let message = message(failure);
            assert!(message.contains("undefined"), "{source}: {message}");
        }
        // The rest of the standard library is untouched by the filtering.
        assert_eq!(
            filter("ltrimstr(\"a\")", &json!("abc")).expect("filter runs"),
            json!("bc")
        );
    }

    #[test]
    fn a_multi_output_filter_becomes_an_array() {
        assert_eq!(
            filter(".[]", &json!([1, 2, 3])).expect("filter runs"),
            json!([1, 2, 3])
        );
    }

    #[test]
    fn an_empty_stream_becomes_null() {
        assert_eq!(
            filter("empty", &json!(1)).expect("filter runs"),
            Value::Null
        );
    }

    #[test]
    fn a_streaming_filter_is_charged_against_the_step_budget() {
        // Each pulled output costs a step, so an unbounded stream is bounded by the same budget
        // every other looping construct answers to instead of running to completion for free.
        let mut budget = Budget::start(Limits {
            max_steps: 16,
            ..Limits::default()
        });
        let failure = evaluate("range(1000000)", &json!(null), &mut budget)
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
        let failure = evaluate("def f: f; f", &json!(1), &mut budget)
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
        let failure = evaluate("range(100000) | tostring", &json!(null), &mut budget)
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
        let error = message(filter(".[", &json!({})).expect_err("unbalanced filter"));
        assert!(error.starts_with("jq: invalid filter"), "{error}");
        let error = message(filter("no_such_function", &json!({})).expect_err("undefined filter"));
        assert!(error.contains("undefined"), "{error}");
    }

    #[test]
    fn runtime_errors_are_reported_not_fatal() {
        let failure = filter(".a", &json!([1, 2])).expect_err("indexing an array by name fails");
        assert!(
            matches!(failure, CommandFailure::Status { .. }),
            "{failure:?}"
        );
        assert!(message(failure).starts_with("jq:"));
    }
}
