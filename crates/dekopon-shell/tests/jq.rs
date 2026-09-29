use std::time::{Duration, Instant};

use dekopon_shell::{CapabilityCallResult, CapabilityInvoker, Interpreter, Limits};
use serde_json::{Value, json};

struct Invoker;
impl CapabilityInvoker for Invoker {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }
    fn invoke(
        &self,
        _: &str,
        _: Value,
        _: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
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
        (".[]", json!([1, 2, 3]), json!([1, 2, 3])),
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
fn an_empty_stream_becomes_null() {
    let outcome = filter("empty", json!(1));
    assert_eq!(outcome.exit_code.get(), 0, "{outcome:?}");
    assert_eq!(outcome.output, "");
    let piped = Interpreter::new(Limits::default()).run("echo 1 | jq empty | jq type", &Invoker);
    assert_eq!(piped.output, "null", "{piped:?}");
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
    .run("jq 'range(1000000)'", &Invoker);
    assert!(
        outcome.output.contains("step budget exhausted"),
        "{outcome:?}"
    );
    let outcome = Interpreter::new(Limits {
        max_value_bytes: 1024,
        ..Limits::default()
    })
    .run("jq 'range(100000) | tostring'", &Invoker);
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
    .run("jq '[range(5000)]'", &Invoker);
    assert!(
        outcome.output.contains("more than 1024 bytes"),
        "{outcome:?}"
    );
}

#[test]
fn raw_compact_flags_and_piped_json_text_keep_working() {
    for flag in ["-r", "-c", "--raw-output", "--compact-output"] {
        let outcome = Interpreter::new(Limits::default())
            .run(&format!("echo '{{\"a\":\"x\"}}' | jq {flag} .a"), &Invoker);
        assert_eq!(outcome.output, "x", "{flag}: {outcome:?}");
    }
    assert_eq!(
        Interpreter::new(Limits::default())
            .run("echo abc | jq 'ltrimstr(\"a\")'", &Invoker)
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
    .run("jq 'def f: f; f'", &Invoker);
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
    fn invoke(
        &self,
        _: &str,
        _: Value,
        _: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        CapabilityCallResult::NotFound
    }
}

#[test]
fn a_cancelled_turn_kills_its_jq_stage() {
    worker();
    let start = Instant::now();
    let outcome = Interpreter::new(Limits::default()).run("jq 'def f: f; f'", &Cancelled(start));
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
