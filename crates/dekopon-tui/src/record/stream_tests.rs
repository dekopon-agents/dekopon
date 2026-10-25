//! Regression coverage for the 0.31 descriptor/proposal seam, without external artifacts.
use std::{
    io::{Read as _, Write as _},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use dekopon_agent::{ShellRuntime, prompt::ScriptRuntime as _};
use dekopon_process::{CallBudget, Limits};
use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, CommandProposal, CommandReport, CommandReportOutcome,
    CommandRun, Streams,
};
use serde_json::json;

use super::{CallOutcome, RecordingInvoker, RecordingRuntime, Sequence, SessionEvent};

#[derive(Default)]
struct Probe {
    starts: AtomicUsize,
    finishes: AtomicUsize,
    effects: AtomicUsize,
    reports: Arc<AtomicUsize>,
    cancelled: AtomicBool,
}

impl CapabilityInvoker for Probe {
    fn granted(&self) -> Vec<String> {
        vec!["probe.produce".into(), "probe.consume".into()]
    }
    fn command_words(&self) -> Vec<String> {
        vec!["probe".into()]
    }
    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    fn script_started(&self, timeout: Duration) {
        assert_eq!(timeout, Limits::default().timeout);
        self.starts.fetch_add(1, Ordering::SeqCst);
    }
    fn script_finished(&self) {
        self.finishes.fetch_add(1, Ordering::SeqCst);
    }
    fn run_command(&self, word: &str, argv: &[String], stdin_piped: bool) -> Option<CommandRun> {
        if word != "probe" {
            return None;
        }
        let command = argv.first()?.as_str();
        assert_eq!(
            stdin_piped,
            command == "consume",
            "pipe presence is forwarded, not buffered input"
        );
        let reports = self.reports.clone();
        Some(CommandRun::Proposed {
            capability: format!("probe.{command}"),
            input: json!({"command": command}),
            secret_use: None,
            report: Some(CommandReport::new(move |outcome| {
                if matches!(outcome, CommandReportOutcome::Succeeded) {
                    reports.fetch_add(1, Ordering::SeqCst);
                }
            })),
        })
    }
    fn invoke(
        &self,
        mut proposal: CommandProposal,
        streams: Streams,
        _tree: &dekopon_process::TreeContext,
    ) -> CapabilityCallResult {
        self.effects.fetch_add(1, Ordering::SeqCst);
        assert!(proposal.secret_use.is_none());
        let command = proposal.input["command"].as_str().unwrap();
        let output = match command {
            "produce" => {
                assert!(streams.stdin.is_none());
                "hello\n".to_owned()
            }
            "consume" => {
                let mut input = String::new();
                std::fs::File::from(streams.stdin.unwrap())
                    .read_to_string(&mut input)
                    .unwrap();
                input.to_uppercase()
            }
            _ => panic!("unexpected proposal"),
        };
        std::fs::File::from(streams.stdout)
            .write_all(output.as_bytes())
            .unwrap();
        proposal
            .report
            .take()
            .unwrap()
            .complete(CommandReportOutcome::Succeeded);
        CapabilityCallResult::SucceededWithStderr(format!("{command} diagnostic\n"))
    }
}

#[test]
fn recording_preserves_descriptor_pipeline_reports_lifecycle_and_shared_budget() {
    let probe = Arc::new(Probe::default());
    let calls = CallBudget::new(2);
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let sequence = Sequence::default();
    let runtime = RecordingRuntime::new(
        ShellRuntime {
            invoker: RecordingInvoker::new(probe.clone(), events.clone(), sequence.clone()),
            limits: Limits::default(),
            calls: calls.clone(),
        },
        events,
        sequence,
    );
    let outcome = runtime.run_script("probe produce | probe consume");
    assert_eq!(outcome.exit_code.get(), 0, "{}", outcome.output);
    assert!(
        outcome.output.contains("HELLO\n"),
        "stdout was lost: {}",
        outcome.output
    );
    assert!(outcome.output.contains("consume diagnostic"));
    assert_eq!(runtime.capability_calls_used(), 2);
    assert_eq!(
        probe.reports.load(Ordering::SeqCst),
        2,
        "owned reports survived observation"
    );
    let second = runtime.run_script("probe produce");
    assert_ne!(
        second.exit_code.get(),
        0,
        "budget cannot reset between model scripts"
    );
    assert_eq!(probe.effects.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.capability_calls_used(), 2);
    assert_eq!(probe.starts.load(Ordering::SeqCst), 2);
    assert_eq!(probe.finishes.load(Ordering::SeqCst), 2);
    let recorded: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok())
        .filter_map(|event| {
            if let SessionEvent::Capability(call) = event {
                Some(call)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(recorded.len(), 2);
    assert!(
        recorded
            .iter()
            .all(|call| matches!(call.outcome, CallOutcome::SucceededWithStderr(_)))
    );
    assert!(
        recorded.iter().all(|call| call.payloads().len() == 1),
        "streams must not become fake JSON output"
    );
}

#[test]
fn recording_forwards_cancellation_before_a_provider_effect() {
    let probe = Arc::new(Probe::default());
    probe.cancelled.store(true, Ordering::SeqCst);
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let invoker = RecordingInvoker::new(probe.clone(), events, Sequence::default());
    assert!(invoker.cancelled());
    let outcome = ShellRuntime {
        invoker,
        limits: Limits::default(),
        calls: CallBudget::new(2),
    }
    .run_script("probe produce");
    assert_eq!(outcome.exit_code.get(), 130);
    assert_eq!(probe.effects.load(Ordering::SeqCst), 0);
}

#[test]
fn recording_preserves_provider_exit_status_and_stderr() {
    let result = CapabilityCallResult::Exited {
        status: std::num::NonZeroU8::new(141).unwrap(),
        stderr: "downstream closed\n".into(),
    };
    assert_eq!(
        CallOutcome::from(&result),
        CallOutcome::Exited {
            status: 141,
            stderr: "downstream closed\n".into()
        }
    );
    assert_eq!(CallOutcome::from(&result).label(), "exited");
}
