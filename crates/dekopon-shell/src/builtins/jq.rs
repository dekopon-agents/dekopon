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
//! still running when the deadline passes is *abandoned*, not stopped — its thread stays alive
//! until the process exits, parked on a send nobody will receive, or spinning if it never yields.
//! That is a worse outcome than a fuel meter and a better one than a runner that hangs forever, and
//! it makes the wall-clock bound this crate advertises true for `jq` as well.

use std::{
    sync::mpsc::{RecvTimeoutError, sync_channel},
    time::Duration,
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

/// One message from the filter worker to the evaluator.
enum Produced {
    /// One output value, already rendered as JSON text.
    Output(String),
    /// The filter could not be compiled, or failed while running.
    Failed(String),
    /// The filter's stream ended normally.
    Done,
}

/// Compiles and runs one jq filter over one value under the script's budget.
///
/// See the module documentation for why this crosses a thread boundary.
pub(crate) fn evaluate(
    filter: &str,
    input: &Value,
    budget: &mut Budget,
) -> Result<Value, CommandFailure> {
    // A rendezvous channel, so the filter cannot run ahead of the budget that is paying for it:
    // every output waits until the evaluator has charged the previous one.
    let (sender, receiver) = sync_channel::<Produced>(0);
    let filter = filter.to_owned();
    let input = input.clone();
    std::thread::Builder::new()
        .name("dekopon-shell-jq".to_owned())
        .spawn(move || {
            let message = match run_filter(&filter, &input, &sender) {
                Ok(()) => Produced::Done,
                Err(message) => Produced::Failed(message),
            };
            // A closed receiver means the evaluator already gave up on this filter.
            #[allow(
                clippy::let_underscore_must_use,
                reason = "a closed receiver is the normal end of a filter the budget cut short, \
                          and the returned SendError only hands back the message nobody is left \
                          to read; this thread has no caller to report to either way"
            )]
            let _ = sender.send(message);
        })
        .map_err(|error| {
            CommandFailure::failed(format!("jq: could not start the filter evaluator: {error}"))
        })?;

    let mut outputs = Vec::new();
    loop {
        // Never wait for zero: `remaining` reaching zero one tick before `check_deadline` agrees
        // would otherwise spin instead of waiting.
        let wait = budget.remaining().max(Duration::from_millis(1));
        match receiver.recv_timeout(wait) {
            Ok(Produced::Output(produced)) => {
                // Pulling one value is where a filter's work happens, so each pull is a step and
                // re-reads the deadline. Without this a whole `jq` command cost exactly one step.
                budget.charge_step()?;
                budget.charge_value_bytes(produced.len() as u64)?;
                outputs.push(serde_json::from_str::<Value>(&produced).map_err(|error| {
                    CommandFailure::failed(format!("jq: could not read filter output: {error}"))
                })?);
            }
            Ok(Produced::Failed(message)) => return Err(CommandFailure::failed(message)),
            Ok(Produced::Done) => break,
            Err(RecvTimeoutError::Timeout) => {
                budget.check_deadline()?;
                // The clock has not actually passed the deadline, so keep waiting for the filter.
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(CommandFailure::failed(
                    "jq: the filter evaluator stopped without producing a result",
                ));
            }
        }
    }

    // A jq filter is a stream. One output stays scalar so `| jq .field | grep x` reads naturally;
    // several outputs become a JSON array so nothing is silently discarded.
    Ok(match outputs.len() {
        0 => Value::Null,
        1 => outputs.into_iter().next().unwrap_or(Value::Null),
        _ => Value::Array(outputs),
    })
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
    use serde_json::{Value, json};

    use crate::limits::{Budget, Limits};

    use super::{CommandFailure, evaluate};

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
    fn a_filter_that_never_yields_is_stopped_by_the_deadline() {
        // `def f: f; f` recurses forever inside jaq without ever producing an output, so nothing
        // cooperative can reach it. The wall-clock bound this crate advertises has to hold anyway.
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
