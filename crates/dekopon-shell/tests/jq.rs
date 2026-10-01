use parking_lot::Mutex;
use std::{
    io::{BufRead as _, Write as _},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use dekopon_shell::{CapabilityCallResult, CapabilityInvoker, Interpreter, Limits};
use serde_json::{Value, json};

struct Invoker;
impl CapabilityInvoker for Invoker {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }
    fn invoke(&self, _: dekopon_shell::CommandProposal) -> CapabilityCallResult {
        CapabilityCallResult::NotFound
    }
}

fn worker() {
    let _already_set = dekopon_shell::set_jq_worker_executable(
        env!("CARGO_BIN_EXE_dekopon-shell-jq-worker").into(),
    );
}

fn filter(source: &str, input: Value) -> dekopon_shell::ScriptOutcome {
    worker();
    Interpreter::new(Limits::default())
        .run(&format!("echo '{}' | jq '{}'", input, source), &Invoker)
}

#[test]
fn numbers_and_standard_filters_keep_their_json_values() {
    for (source, input, expected) in [
        (".a", json!({"a": 1}), json!(1)),
        ("map(. * 2)", json!([1, 2, 3]), json!([2, 4, 6])),
        ("3 / 2", json!(null), json!(1.5)),
        ("1e3", json!(null), json!(1000.0)),
        (
            "10000000000000000000 + 1",
            json!(null),
            json!(10_000_000_000_000_000_001_u64),
        ),
        (
            "to_entries | map(.key) | sort",
            json!({"b":2,"a":1}),
            json!(["a", "b"]),
        ),
        (".[0]", json!([1, 2, 3]), json!(1)),
    ] {
        let outcome = filter(source, input);
        assert_eq!(outcome.exit_code.get(), 0, "{source}: {outcome:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&outcome.output).expect("JSON"),
            expected,
            "{source}"
        );
    }
}

#[test]
fn an_empty_filter_result_does_not_supply_a_null_document() {
    let outcome = filter("empty", json!(1));
    assert_eq!(outcome.exit_code.get(), 0, "{outcome:?}");
    assert_eq!(outcome.output, "");
    let piped = Interpreter::new(Limits::default()).run("echo 1 | jq empty | jq type", &Invoker);
    assert_eq!(piped.output, "", "{piped:?}");
}

#[test]
fn one_document_producing_several_results_emits_separate_lines() {
    let outcome = filter(".[]", json!([1, 2, 3]));
    assert_eq!(outcome.exit_code.get(), 0, "{outcome:?}");
    assert_eq!(outcome.output, "1\n2\n3");
}

#[test]
fn several_input_documents_produce_separate_output_lines() {
    worker();
    let outcome =
        Interpreter::new(Limits::default()).run("printf '1\\n2\\n' | jq '. + 1'", &Invoker);
    assert_eq!(outcome.exit_code.get(), 0, "{outcome:?}");
    assert_eq!(outcome.output, "2\n3");
}

#[test]
fn string_results_are_quoted_unless_raw_output_is_requested() {
    worker();
    let strings = Interpreter::new(Limits::default()).run("printf '\"a\"\\n' | jq .", &Invoker);
    assert_eq!(strings.output, "\"a\"");
    let raw = Interpreter::new(Limits::default()).run("printf '\"a\"\\n' | jq -r .", &Invoker);
    assert_eq!(raw.output, "a");
}

#[test]
fn invalid_json_input_has_status_two_even_after_a_valid_result() {
    worker();
    for flag in ["", "-s "] {
        let invalid = Interpreter::new(Limits::default())
            .run(&format!("printf '1\\nnot json' | jq {flag}."), &Invoker);
        assert_eq!(invalid.exit_code.get(), 2, "{flag}: {invalid:?}");
        if flag.is_empty() {
            assert!(invalid.output.starts_with("1\n"), "{invalid:?}");
        }
    }
}

#[test]
fn a_downstream_early_close_kills_a_busy_jq_worker_and_joins_its_stage() {
    worker();
    let outcome = Interpreter::new(Limits {
        timeout: Duration::from_secs(3),
        ..Limits::default()
    })
    .run(
        "set -o pipefail; jq -n 'range(1000000000)' | head -1",
        &Invoker,
    );
    assert_eq!(outcome.exit_code.get(), 0, "{outcome:?}");
    assert_eq!(outcome.output, "0");
    assert_eq!(filter(".", json!(1)).exit_code.get(), 0);
}

#[test]
fn slurp_of_empty_input_emits_an_empty_array() {
    worker();
    let slurp = Interpreter::new(Limits::default()).run("jq -s .", &Invoker);
    assert_eq!(slurp.exit_code.get(), 0, "{slurp:?}");
    assert_eq!(slurp.output, "[]");
}

#[test]
fn null_input_ignores_stdin() {
    worker();
    let null = Interpreter::new(Limits::default()).run("printf 'not json' | jq -n '1'", &Invoker);
    assert_eq!(null.exit_code.get(), 0, "{null:?}");
    assert_eq!(null.output, "1");
}

#[test]
fn slurp_collects_multiple_documents() {
    worker();
    let grouped = Interpreter::new(Limits::default()).run("printf '1\\n2\\n' | jq -s .", &Invoker);
    assert_eq!(grouped.exit_code.get(), 0, "{grouped:?}");
    assert_eq!(grouped.output, "[1,2]");
}

#[test]
fn slurp_refuses_documents_past_the_retained_budget() {
    worker();
    let refused = Interpreter::new(Limits {
        max_value_bytes: 1024,
        ..Limits::default()
    })
    .run(
        &format!("jq -s . <<EOF\n{}\nEOF", "1\n".repeat(1100)),
        &Invoker,
    );
    assert!(
        refused.output.contains("more than 1024 bytes"),
        "{refused:?}"
    );
}

#[test]
fn the_interpreter_forwards_a_jq_result_while_the_producer_is_still_open() {
    struct HeldProducer {
        release: Mutex<mpsc::Receiver<()>>,
        observed: mpsc::SyncSender<String>,
    }

    impl CapabilityInvoker for HeldProducer {
        fn granted(&self) -> Vec<String> {
            vec!["test.hold".to_owned()]
        }
        fn has_command_word(&self, word: &str) -> bool {
            word == "hold"
        }
        fn run_command(
            &self,
            word: &str,
            _: &[String],
            _: Option<&str>,
        ) -> Option<dekopon_shell::CommandRun> {
            (word == "hold").then(|| dekopon_shell::CommandRun::Proposed {
                capability: "test.hold".to_owned(),
                input: Value::Null,
                secret_use: None,
                report: None,
            })
        }
        fn invoke(&self, _: dekopon_shell::CommandProposal) -> CapabilityCallResult {
            self.release
                .lock()
                .recv_timeout(Duration::from_secs(5))
                .expect("release producer after first result");
            CapabilityCallResult::Succeeded(Value::Null)
        }
        fn note(&self, text: &str, _: Option<Duration>) {
            self.observed
                .send(text.to_owned())
                .expect("signal first jq result");
        }
    }

    worker();
    let (release, released) = mpsc::sync_channel(1);
    let (observed, first) = mpsc::sync_channel(1);
    let invoker = HeldProducer {
        release: Mutex::new(released),
        observed,
    };
    std::thread::scope(|scope| {
        let run = scope.spawn(|| Interpreter::new(Limits::default()).run(
            "{ printf '1\\n'; hold; printf '2\\n'; } | jq . | { read first; progress \"$first\"; cat; }",
            &invoker,
        ));
        let arrived = first.recv_timeout(Duration::from_secs(3));
        release.send(()).expect("release producer");
        let outcome = run.join().expect("join interpreter");
        assert_eq!(arrived.expect("jq output before input closes"), "1");
        assert_eq!(outcome.exit_code.get(), 0, "{outcome:?}");
        assert_eq!(outcome.output, "2");
    });
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "owner: this test joins its reader before returning; bound: two output lines from two documents"
)]
fn worker_emits_the_first_document_before_stdin_ends() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dekopon-shell-jq-worker"))
        .env_clear()
        .env("DEKOPON_SHELL_JQ_WORKER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start worker");
    let mut stdin = child.stdin.take().expect("worker stdin");
    let stdout = child.stdout.take().expect("worker stdout");
    let (sender, receiver) = mpsc::sync_channel(2);
    let reader = std::thread::spawn(move || {
        let mut lines = std::io::BufReader::new(stdout).lines();
        sender
            .send(lines.next().expect("first line").expect("read first line"))
            .expect("send first line");
        sender
            .send(
                lines
                    .next()
                    .expect("second line")
                    .expect("read second line"),
            )
            .expect("send second line");
    });
    stdin
        .write_all(b"[\". + 1\",false]\n1\n")
        .expect("send first document");
    stdin.flush().expect("flush first document");
    let first = receiver.recv_timeout(Duration::from_secs(5));
    stdin.write_all(b"2\n").expect("send second document");
    drop(stdin);
    let second = receiver.recv_timeout(Duration::from_secs(5));
    let _kill = child.kill();
    let _wait = child.wait();
    reader.join().expect("join output reader");
    assert_eq!(first.expect("first output before end of input"), "2");
    assert_eq!(second.expect("second output after second input"), "3");
}

#[test]
fn non_json_values_and_deep_outputs_are_refused() {
    for source in [
        "nan",
        "infinite",
        "{(1): 2}",
        "reduce range(140) as $i (.; [.])",
    ] {
        let outcome = filter(source, json!({}));
        assert_eq!(outcome.exit_code.get(), 1, "{source}: {outcome:?}");
        assert!(outcome.output.contains("jq:"), "{source}: {outcome:?}");
    }
}

#[test]
fn host_filters_and_halt_do_not_escape_the_worker() {
    for source in ["env", "now", "halt", "halt(3)", ".[", "no_such_function"] {
        let outcome = filter(source, json!({}));
        assert_eq!(outcome.exit_code.get(), 1, "{source}: {outcome:?}");
    }
    assert_eq!(filter("length", json!([1, 2])).exit_code.get(), 0);
}

#[test]
fn output_steps_and_bytes_are_charged() {
    worker();
    let outcome = Interpreter::new(Limits {
        max_steps: 16,
        ..Limits::default()
    })
    .run("jq -n 'range(1000000)'", &Invoker);
    assert!(
        outcome.output.contains("step budget exhausted"),
        "{outcome:?}"
    );
    let outcome = Interpreter::new(Limits {
        max_value_bytes: 1024,
        ..Limits::default()
    })
    .run("jq -n '[range(5000)]'", &Invoker);
    assert!(
        outcome.output.contains("more than 1024 bytes"),
        "{outcome:?}"
    );
}

#[test]
fn a_custom_value_byte_ceiling_refuses_a_single_jq_output_at_its_configured_maximum() {
    worker();
    let outcome = Interpreter::new(Limits {
        max_value_bytes: 1024,
        ..Limits::default()
    })
    .run("jq -n '[range(5000)]'", &Invoker);
    assert!(
        outcome.output.contains("more than 1024 bytes"),
        "{outcome:?}"
    );
}

#[test]
fn worker_framing_does_not_count_against_the_emitted_result_ceiling() {
    worker();
    let limits = Limits {
        max_value_bytes: 128,
        ..Limits::default()
    };
    let number = Interpreter::new(limits).run("jq -n 1234", &Invoker);
    assert_eq!(number.exit_code.get(), 0, "{number:?}");
    assert_eq!(number.output, "1234");
    let raw = Interpreter::new(limits).run("jq -n -r '\"abcd\"'", &Invoker);
    assert_eq!(raw.exit_code.get(), 0, "{raw:?}");
    assert_eq!(raw.output, "abcd");
    let escaped =
        Interpreter::new(limits).run("jq -n -r '\"\\u0001\\u0002\\u0003\\u0004\"'", &Invoker);
    assert_eq!(escaped.exit_code.get(), 0, "{escaped:?}");
    assert_eq!(escaped.output, "\u{1}\u{2}\u{3}\u{4}");
    let too_large =
        Interpreter::new(limits).run(&format!("jq -n -r '\"{}\"'", "x".repeat(129)), &Invoker);
    assert!(
        too_large.output.contains("more than 128 bytes"),
        "{too_large:?}"
    );
}

#[test]
fn raw_compact_flags_and_piped_json_text_keep_working() {
    for flag in ["-r", "-c", "--raw-output", "--compact-output"] {
        let outcome = Interpreter::new(Limits::default())
            .run(&format!("echo '{{\"a\":\"x\"}}' | jq {flag} .a"), &Invoker);
        assert_eq!(
            outcome.output,
            if flag.contains('r') { "x" } else { "\"x\"" },
            "{flag}: {outcome:?}"
        );
    }
    assert_eq!(
        Interpreter::new(Limits::default())
            .run("echo '\"abc\"' | jq -r 'ltrimstr(\"a\")'", &Invoker)
            .output,
        "bc"
    );
}

#[test]
fn a_filter_that_never_yields_is_killed_at_the_deadline() {
    worker();
    let started = Instant::now();
    let outcome = Interpreter::new(Limits {
        timeout: Duration::from_millis(250),
        ..Limits::default()
    })
    .run("jq -n 'def f: f; f'", &Invoker);
    assert_eq!(outcome.exit_code.get(), 124, "{outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(filter(".", json!(1)).exit_code.get(), 0);
}

struct Cancelled(Instant);
impl CapabilityInvoker for Cancelled {
    fn cancelled(&self) -> bool {
        self.0.elapsed() >= Duration::from_millis(200)
    }
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }
    fn invoke(&self, _: dekopon_shell::CommandProposal) -> CapabilityCallResult {
        CapabilityCallResult::NotFound
    }
}

#[test]
fn a_cancelled_turn_kills_its_jq_stage() {
    worker();
    let start = Instant::now();
    let outcome = Interpreter::new(Limits::default()).run("jq -n 'def f: f; f'", &Cancelled(start));
    assert_eq!(outcome.exit_code.get(), 130, "{outcome:?}");
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[cfg(target_os = "linux")]
#[test]
fn an_unbounded_allocation_fails_the_stage_and_the_process_lives() {
    let outcome = filter("[range(0;1000000000)]", json!({}));
    assert_eq!(outcome.exit_code.get(), 1, "{outcome:?}");
    assert!(outcome.output.contains("jq: worker exited"), "{outcome:?}");
    assert_eq!(filter(".", json!(1)).exit_code.get(), 0);
}

#[test]
fn a_max_value_bytes_sized_input_passes() {
    worker();
    let input = vec!["x".repeat(1024); 30_000];
    let outcome = Interpreter::new(Limits::default())
        .run(&format!("echo '{}' | jq length", json!(input)), &Invoker);
    assert_eq!(outcome.exit_code.get(), 0, "{outcome:?}");
    assert_eq!(outcome.output, "30000");
}
