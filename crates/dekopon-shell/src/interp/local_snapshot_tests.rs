use dekopon_process::{CallBudget, Limits, TreeContext};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use serde_json::Value;

use super::{Evaluator, Frame, ShellOptions, reduce_charged, rendered_bytes, telemetry};
use crate::{
    CapabilityCallResult, CapabilityInvoker, CommandProposal,
    builtins::CommandResult,
    limits::{Budget, OutputBuffer},
    pipe,
};

struct NoCalls;

impl CapabilityInvoker for NoCalls {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }

    fn invoke(
        &self,
        _proposal: CommandProposal,
        _streams: crate::Streams,
        _tree: &dekopon_process::TreeContext,
    ) -> CapabilityCallResult {
        crate::secret_use_unsupported()
    }
}

#[test]
fn local_assignment_and_restore_keep_snapshot_globals_shared() {
    let limits = Limits::default();
    let invoker = NoCalls;
    let mut parent = Evaluator {
        nesting: super::nesting::Nesting::root(),
        invoker: &invoker,
        budget: Budget::start_tree(
            limits,
            TreeContext::new(limits, CallBudget::new(1), crate::test_threads()),
        ),
        limits,
        output: OutputBuffer::new(&limits),
        globals: Arc::new(BTreeMap::from([(
            "big".into(),
            Value::String("x".repeat(64 * 1024)),
        )])),
        global_charges: BTreeMap::new(),
        buffer_charges: BTreeMap::new(),
        frames: vec![Frame {
            kind: super::FrameKind::Function,
            locals: BTreeMap::from([("n".into(), Some(Value::from(0)))]),
            positional: Vec::new(),
            local_charges: BTreeMap::new(),
            _positional_charges: Vec::new(),
        }],
        functions: BTreeMap::new(),
        function_names: BTreeSet::new(),
        buffers: Arc::new(BTreeMap::new()),
        captures: Vec::new(),
        active_buffer: None,
        discard_capture_depth: None,
        diagnostics_depth: None,
        expansion_charges: Vec::new(),
        script_stdin: false,
        stdin: Vec::new(),
        stdout: None,
        reader_gone: false,
        stdout_redirected: false,
        shared_charges: Vec::new(),
        options: ShellOptions::default(),
        testing_status: 0,
        stderr_capture: Vec::new(),
        counters: telemetry::ScriptCounters::default(),
        last_status: dekopon_process::ExitCode::SUCCESS,
        last_substitution_status: dekopon_process::ExitCode::SUCCESS,
        jobs: crate::job::ScriptJobs::default(),
    };
    let (writer, _reader) = pipe::pipe(
        None,
        None,
        crate::interp::telemetry::stage_read,
        crate::interp::telemetry::stage_write,
    );
    let mut snapshot = parent.snapshot(writer).expect("snapshot");
    assert!(Arc::ptr_eq(&parent.globals, &snapshot.globals));
    let mut saved = None;
    snapshot
        .assign_prefix("n", CommandResult::value(Value::from(1)), &mut saved)
        .expect("local assign");
    assert!(Arc::ptr_eq(&parent.globals, &snapshot.globals));
    snapshot.restore_all(saved);
    assert!(Arc::ptr_eq(&parent.globals, &snapshot.globals));
    assert!(snapshot.shared_charges.is_empty());
}

#[test]
fn structured_capture_counts_escaped_bytes_before_reduction() {
    let limits = Limits {
        max_value_bytes: 200,
        ..Limits::default()
    };
    let tree = TreeContext::new(limits, CallBudget::new(1), crate::test_threads());
    let budget = Budget::start_tree(limits, tree.clone());
    let value = serde_json::json!({"payload": "\n".repeat(40)});
    assert_eq!(
        rendered_bytes(&value, 200).expect("rendered size"),
        u64::try_from(value.to_string().len()).expect("size fits")
    );
    let mut retained = vec![
        budget.charge_value_bytes(79).expect("first value"),
        budget.charge_value_bytes(79).expect("second value"),
    ];
    let captured = vec![
        CommandResult::value(value.clone()),
        CommandResult::value(value),
    ];
    assert!(matches!(
        reduce_charged(&budget, captured, &mut retained),
        Err(dekopon_process::LimitExceeded::ValueBytes { maximum: 200 })
    ));
    drop(retained);
    assert_eq!(tree.value_bytes(), 0);
}

#[test]
fn capture_reduction_is_bounded_by_the_tree_value_limit() {
    let tree = TreeContext::new(
        Limits {
            max_value_bytes: 1024,
            ..Limits::default()
        },
        CallBudget::new(1),
        crate::test_threads(),
    );
    let budget = Budget::start_tree(
        Limits {
            max_value_bytes: 64,
            ..Limits::default()
        },
        tree.clone(),
    );
    let captured = vec![
        CommandResult::value(Value::String("x".repeat(100))),
        CommandResult::value(Value::String("y".repeat(100))),
    ];
    let mut retained = Vec::new();
    let reduced = reduce_charged(&budget, captured, &mut retained).expect("within the tree limit");
    assert_eq!(reduced.as_str().map(str::len), Some(201));
    assert_eq!(tree.value_bytes(), 217);
    assert_eq!(budget.max_value_bytes(), 1024);
}

#[test]
fn an_unknown_word_with_no_thread_permit_still_runs_its_fallback() {
    let result = crate::Interpreter::new(Limits::default(), crate::test_threads_up_to(0)).run(
        "nosuchword || { echo status=$?; echo fallback; }; echo after",
        &NoCalls,
    );
    assert_eq!(result.exit_code, dekopon_process::ExitCode::SUCCESS);
    assert_eq!(
        result.output,
        "dekopon-shell: nosuchword: command not found\nstatus=127\nfallback\nafter"
    );
    assert_eq!(result.capability_calls, 0);
}
