use std::time::Duration;

use dekopon_agent::{
    progress::{ProgressEvent, ProgressSink},
    prompt::ScriptRuntime,
};
use dekopon_process::{CallBudget, ExitCode, Limits, TreeContext};
use dekopon_shell::{
    CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandProposal, ScriptOutcome,
    Streams,
};
use serde_json::json;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::{
    CallOutcome, RecordingInvoker, RecordingProgress, RecordingRuntime, Sequence, SessionEvent,
};

/// A leg that answers from a fixed table, so a test asserts on what the decorator reported rather
/// than on what a broker happened to be doing.
struct FixedInvoker;

impl CapabilityInvoker for FixedInvoker {
    fn granted(&self) -> Vec<String> {
        vec!["gh.issue.list".to_owned()]
    }

    fn command_word_help(&self) -> std::collections::BTreeMap<String, String> {
        [("gh".to_owned(), "gh issue list".to_owned())].into()
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        (capability == "gh.issue.list").then(|| CapabilityDescription {
            capability: capability.to_owned(),
            description: "lists issues".to_owned(),
        })
    }

    fn invoke(
        &self,
        proposal: CommandProposal,
        streams: Streams,
        _tree: &TreeContext,
    ) -> CapabilityCallResult {
        match proposal.capability.as_str() {
            "gh.issue.list" => {
                use std::io::Write as _;
                std::fs::File::from(streams.stdout)
                    .write_all(b"[{\"number\":7}]\n")
                    .unwrap();
                CapabilityCallResult::Succeeded
            }
            "gh.pull-request.merge" => CapabilityCallResult::Denied {
                reason: "unconstrained-capability".to_owned(),
            },
            _ => CapabilityCallResult::NotFound,
        }
    }
}

/// A runtime that reports a fixed outcome and drives the invoker exactly as the real one does.
struct FixedRuntime<'invoker> {
    invoker: &'invoker dyn CapabilityInvoker,
}

impl ScriptRuntime for FixedRuntime<'_> {
    fn capability_calls_used(&self) -> u32 {
        2
    }

    fn run_script(&self, _script: &str, _participant: Option<&str>) -> ScriptOutcome {
        self.invoker.invoke(
            CommandProposal::new("gh.issue.list", json!({"state": "open"}), None),
            sink(),
            &tree(),
        );
        self.invoker.invoke(
            CommandProposal::new("gh.pull-request.merge", json!({"number": 7}), None),
            sink(),
            &tree(),
        );
        ScriptOutcome {
            output: "two calls\n".to_owned(),
            exit_code: ExitCode::SUCCESS,
            truncated: false,
            capability_calls: 2,
            steps: 9,
        }
    }

    fn command_words(&self) -> Vec<String> {
        vec!["gh".to_owned()]
    }
}

fn tree() -> TreeContext {
    TreeContext::new(
        Limits::default(),
        CallBudget::new(4),
        dekopon_test_support::shell_threads(),
    )
}

fn sink() -> Streams {
    Streams {
        stdin: None,
        stdout: tempfile::tempfile().unwrap().into(),
    }
}

fn drain(receiver: &mut UnboundedReceiver<SessionEvent>) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    events
}

#[test]
fn reports_bounded_model_progress_without_an_inference_payload() {
    let (sender, mut receiver) = unbounded_channel();
    RecordingProgress::new(sender).emit(ProgressEvent::ModelTurn { turn: 2, of: 8 });
    assert!(
        matches!(receiver.try_recv(), Ok(SessionEvent::Progress(message)) if message == "model turn 2/8")
    );
}

#[test]
fn reports_a_script_and_every_call_inside_it_in_order() {
    let (sender, mut receiver) = unbounded_channel();
    let sequence = Sequence::default();
    let invoker = RecordingInvoker::new(FixedInvoker, sender.clone(), sequence.clone());
    let runtime = RecordingRuntime::new(FixedRuntime { invoker: &invoker }, sender, sequence);

    let outcome = runtime.run_script("gh issue list", None);
    assert_eq!(
        outcome.capability_calls, 2,
        "the outcome must pass through unchanged"
    );

    let events = drain(&mut receiver);
    assert_eq!(events.len(), 4, "one start, two calls, one finish");

    let SessionEvent::ScriptStarted {
        sequence: opened,
        script,
    } = &events[0]
    else {
        panic!("first event must open the script: {:?}", events[0]);
    };
    assert_eq!(script, "gh issue list");

    let SessionEvent::Capability(first) = &events[1] else {
        panic!("second event must be a capability: {:?}", events[1]);
    };
    assert_eq!(first.capability, "gh.issue.list");
    assert_eq!(
        first.input,
        json!({"state": "open"}),
        "the exact dispatched input is kept"
    );
    assert_eq!(first.outcome, CallOutcome::Succeeded);

    let SessionEvent::Capability(second) = &events[2] else {
        panic!("third event must be a capability: {:?}", events[2]);
    };
    assert_eq!(
        second.outcome,
        CallOutcome::Denied("unconstrained-capability".to_owned()),
        "a refusal is an outcome to render, not an error to swallow"
    );

    let SessionEvent::ScriptFinished(run) = &events[3] else {
        panic!("last event must close the script: {:?}", events[3]);
    };
    assert_eq!(
        run.sequence, *opened,
        "the close must name the script it opened"
    );
    assert_eq!(run.exit_code, ExitCode::SUCCESS.get());
    assert_eq!(run.steps, 9);
    assert!(first.sequence < second.sequence && second.sequence < run.sequence + 4);
}

#[test]
fn forwards_every_query_without_reporting_it() {
    let (sender, mut receiver) = unbounded_channel();
    let invoker = RecordingInvoker::new(FixedInvoker, sender, Sequence::default());

    assert_eq!(invoker.granted(), ["gh.issue.list"]);
    assert!(invoker.is_granted("gh.issue.list"));
    assert!(invoker.describe("gh.issue.list").is_some());
    assert!(invoker.describe("gh.nope").is_none());
    assert_eq!(invoker.command_word_help()["gh"], "gh issue list");

    assert!(
        drain(&mut receiver).is_empty(),
        "snapshot reads cost no round trip and must not become event volume"
    );
}

#[test]
fn a_closed_console_does_not_stop_a_session() {
    let (sender, receiver) = unbounded_channel();
    let invoker = RecordingInvoker::new(FixedInvoker, sender, Sequence::default());
    drop(receiver);

    // A call the broker has already accepted is not something an observer may abort, so a torn-down
    // console must not change the result the session sees.
    assert_eq!(
        CallOutcome::from(&invoker.invoke(
            CommandProposal::new("gh.issue.list", json!({}), None),
            sink(),
            &tree(),
        )),
        CallOutcome::Succeeded
    );
}

#[test]
fn outcome_labels_are_stable() {
    assert_eq!(CallOutcome::Succeeded.label(), "succeeded");
    assert_eq!(CallOutcome::Denied(String::new()).label(), "denied");
    assert_eq!(CallOutcome::Failed(String::new()).label(), "failed");
    assert_eq!(CallOutcome::NotFound.label(), "not-found");
}

#[test]
fn elapsed_is_measured_around_the_seam() {
    let (sender, mut receiver) = unbounded_channel();
    let invoker = RecordingInvoker::new(FixedInvoker, sender, Sequence::default());
    invoker.invoke(
        CommandProposal::new("gh.issue.list", json!({}), None),
        sink(),
        &tree(),
    );

    let events = drain(&mut receiver);
    let SessionEvent::Capability(call) = &events[0] else {
        panic!("expected a capability event");
    };
    assert!(
        call.elapsed < Duration::from_secs(5),
        "a table lookup is not slow"
    );
}
