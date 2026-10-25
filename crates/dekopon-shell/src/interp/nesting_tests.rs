use super::{Evaluator, ShellOptions, telemetry};
use crate::{
    CapabilityCallResult, CapabilityInvoker, CommandProposal,
    limits::{Budget, OutputBuffer},
};
use dekopon_process::{CallBudget, Limits, TreeContext};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
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

fn evaluator(invoker: &dyn CapabilityInvoker, limits: Limits) -> Evaluator<'_> {
    Evaluator {
        nesting: super::nesting::Nesting::root(),
        invoker,
        budget: Budget::start_tree(limits, TreeContext::new(limits, CallBudget::new(1))),
        limits,
        output: OutputBuffer::new(&limits),
        globals: Arc::new(BTreeMap::new()),
        global_charges: BTreeMap::new(),
        buffer_charges: BTreeMap::new(),
        frames: Vec::new(),
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
    }
}

fn chain(n: usize, open: &str, close: &str) -> String {
    let mut script = "f0() { :; }; ".to_owned();
    for i in 1..n {
        script.push_str(&format!("f{i}() {{ {open}f{}{close}; }}; ", i - 1));
    }
    script.push_str(&format!("f{}", n - 1));
    script
}

fn assert_recursion_refusal(error: super::FatalError) {
    match error {
        super::FatalError::Limit(dekopon_process::LimitExceeded::RecursionDepth {
            maximum: 32,
        }) => {}
        other @ (super::FatalError::Limit(_)
        | super::FatalError::Unsupported(_)
        | super::FatalError::Assertion(_)) => {
            panic!("expected typed recursion refusal, got {other:?}")
        }
    }
}

#[test]
fn runtime_paths_preserve_their_measured_ceilings() {
    for (open, close, ceiling) in [("", "", 32), ("echo $(", ")", 16), ("{ ", "; }", 16)] {
        let program = crate::parser::parse(&chain(ceiling, open, close)).expect("chain parses");
        evaluator(&NoCalls, Limits::default())
            .execute_program(&program)
            .expect("at ceiling");
        let program = crate::parser::parse(&chain(ceiling + 1, open, close)).expect("chain parses");
        let mut evaluator = evaluator(&NoCalls, Limits::default());
        let error = evaluator
            .execute_program(&program)
            .expect_err("one past ceiling");
        assert_recursion_refusal(error);
        let error = super::FatalError::Limit(dekopon_process::LimitExceeded::RecursionDepth {
            maximum: 32,
        });
        assert_eq!(
            evaluator.report_fatal(&error),
            dekopon_process::ExitCode::SYNTAX
        );
        assert_eq!(
            evaluator.output.render(),
            "dekopon-shell: function calls, `$( )` substitutions, `${ }` expansions and compound commands (`if`, `for`, `while`, `case`, `{ }`, `[[ ]]`) nested deeper than 32 levels while running; replace recursion with a loop, or move inner results into variables first"
        );
    }
}

#[test]
fn runtime_paths_refuse_two_thousand_levels_on_a_two_mib_stack() {
    for (open, close) in [("", ""), ("echo $(", ")"), ("{ ", "; }")] {
        let program =
            crate::parser::parse(&chain(2_000, open, close)).expect("flat definitions parse");
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024)
                .spawn_scoped(scope, || {
                    let error = evaluator(&NoCalls, Limits::default())
                        .execute_program(&program)
                        .expect_err("deep execution refused");
                    assert_recursion_refusal(error);
                })
                .expect("spawn evaluator thread")
                .join()
                .expect("join evaluator thread");
        });
    }
}

#[test]
fn nested_errors_refund_depth_and_pipeline_stages_inherit_it() {
    let mut evaluator = evaluator(
        &NoCalls,
        Limits {
            max_recursion_depth: 2,
            ..Limits::default()
        },
    );
    let refused = evaluator
        .nested(|this| this.nested(|this| this.nested(|_| Ok::<_, super::FatalError>(()))));
    assert!(matches!(
        refused,
        Err(super::FatalError::Limit(
            dekopon_process::LimitExceeded::RecursionDepth { maximum: 2 }
        ))
    ));
    evaluator
        .nested(|this| this.nested(|_| Ok::<_, super::FatalError>(())))
        .expect("both units refunded after error");
    evaluator
        .nested(|this| {
            let (writer, _reader) = dekopon_process::pipe::pipe(
                None,
                None,
                crate::interp::telemetry::stage_read,
                crate::interp::telemetry::stage_write,
            );
            let mut child = this.snapshot(writer)?;
            child.nested(|child| {
                assert!(matches!(
                    child.nested(|_| Ok::<_, super::FatalError>(())),
                    Err(super::FatalError::Limit(
                        dekopon_process::LimitExceeded::RecursionDepth { maximum: 2 }
                    ))
                ));
                Ok::<_, super::FatalError>(())
            })
        })
        .expect("stage forks the parent's current depth");
}

#[test]
fn refused_substitution_keeps_its_enclosing_capture() {
    let program = crate::parser::parse(":").expect("empty command");
    let mut evaluator = evaluator(
        &NoCalls,
        Limits {
            max_recursion_depth: 0,
            ..Limits::default()
        },
    );
    evaluator
        .captures
        .push(vec![crate::builtins::CommandResult::value(
            serde_json::json!("outer"),
        )]);
    let error = evaluator
        .run_substitution(&program)
        .expect_err("admission refused");
    match error {
        super::CommandFailure::Fatal(super::FatalError::Limit(
            dekopon_process::LimitExceeded::RecursionDepth { maximum: 0 },
        )) => {}
        super::CommandFailure::Fatal(_) | super::CommandFailure::Status { .. } => {
            panic!("unexpected error: {error:?}")
        }
    }
    assert_eq!(evaluator.captures.len(), 1);
    assert_eq!(evaluator.captures[0].len(), 1);
    assert_eq!(evaluator.captures[0][0].value, serde_json::json!("outer"));
}
