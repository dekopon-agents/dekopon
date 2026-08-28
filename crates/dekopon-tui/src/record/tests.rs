use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use dekopon_agent::prompt::ScriptRuntime;
use dekopon_core::{SecretDrn, SecretUseProposal};
use dekopon_shell::{
    CapabilityCallResult, CapabilityDescription, CapabilityInvoker, ExitCode, ScriptOutcome,
};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::{CallOutcome, RecordingInvoker, RecordingRuntime, Sequence, SessionEvent};

/// A leg that answers from a fixed table, so a test asserts on what the decorator reported rather
/// than on what a broker happened to be doing.
#[derive(Default)]
struct FixedInvoker {
    /// Every secret-use field this leg was handed, in call order.
    secret_uses: Mutex<Vec<Option<dekopon_core::SecretUseProposal>>>,
}

impl CapabilityInvoker for FixedInvoker {
    fn granted(&self) -> Vec<String> {
        vec!["gh.issue.list".to_owned()]
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        (capability == "gh.issue.list").then(|| CapabilityDescription {
            capability: capability.to_owned(),
            description: "lists issues".to_owned(),
            input_schema: json!({"type": "object"}),
        })
    }

    fn invoke(
        &self,
        capability: &str,
        _input: Value,
        secret_use: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        // Records what the decorator forwarded, so the assertion is about the wrapper rather than
        // about a broker that happened to accept the proposal.
        self.secret_uses
            .lock()
            .expect("recorded secret uses")
            .push(secret_use);
        match capability {
            "gh.issue.list" => CapabilityCallResult::Succeeded(json!([{"number": 7}])),
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
    fn run_script(&self, _script: &str, _max_capability_calls: u32) -> ScriptOutcome {
        self.invoker
            .invoke("gh.issue.list", json!({"state": "open"}), None);
        self.invoker
            .invoke("gh.pull-request.merge", json!({"number": 7}), None);
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

fn drain(receiver: &mut UnboundedReceiver<SessionEvent>) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    events
}

#[test]
fn reports_a_script_and_every_call_inside_it_in_order() {
    let (sender, mut receiver) = unbounded_channel();
    let sequence = Sequence::default();
    let invoker = RecordingInvoker::new(FixedInvoker::default(), sender.clone(), sequence.clone());
    let runtime = RecordingRuntime::new(FixedRuntime { invoker: &invoker }, sender, sequence);

    let outcome = runtime.run_script("gh issue list", 8);
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
    assert_eq!(
        first.outcome,
        CallOutcome::Succeeded(json!([{"number": 7}]))
    );

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
    let invoker = RecordingInvoker::new(FixedInvoker::default(), sender, Sequence::default());

    assert_eq!(invoker.granted(), ["gh.issue.list"]);
    assert!(invoker.is_granted("gh.issue.list"));
    assert!(invoker.describe("gh.issue.list").is_some());
    assert!(invoker.describe("gh.nope").is_none());

    assert!(
        drain(&mut receiver).is_empty(),
        "snapshot reads cost no round trip and must not become event volume"
    );
}

#[test]
fn a_closed_console_does_not_stop_a_session() {
    let (sender, receiver) = unbounded_channel();
    let invoker = RecordingInvoker::new(FixedInvoker::default(), sender, Sequence::default());
    drop(receiver);

    // A call the broker has already accepted is not something an observer may abort, so a torn-down
    // console must not change the result the session sees.
    assert_eq!(
        CallOutcome::from(&invoker.invoke("gh.issue.list", json!({}), None)),
        CallOutcome::Succeeded(json!([{"number": 7}]))
    );
}

/// The decorator observes a call; it must not narrow what the call may carry.
///
/// It used to forward eight trait methods and inherit the ninth's deny-by-default, so a `curl
/// --user USER:${drn:...}` in a console session was refused inside the console — the proposal never
/// left the process, and the broker never saw the `secret.use` decision it exists to make.
#[test]
fn a_secret_use_proposal_reaches_the_wrapped_leg() {
    let (sender, mut receiver) = unbounded_channel();
    let inner = Arc::new(FixedInvoker::default());
    let proposal = SecretUseProposal::HttpBearer {
        secret: "drn:com.xrl:secret:prod:api/token"
            .parse::<SecretDrn>()
            .expect("canonical DRN"),
    };
    let invoker = RecordingInvoker::new(Arc::clone(&inner), sender, Sequence::default());

    assert_eq!(
        invoker.invoke("gh.issue.list", json!({}), Some(proposal.clone())),
        CapabilityCallResult::Succeeded(json!([{"number": 7}]))
    );
    assert_eq!(
        inner.secret_uses.lock().expect("recorded")[0],
        Some(proposal),
        "the wrapper dropped the proposal on its way to the leg"
    );
    assert_eq!(
        drain(&mut receiver).len(),
        1,
        "a secret-carrying call is still one reported call"
    );
}

#[test]
fn outcome_labels_are_stable() {
    assert_eq!(CallOutcome::Succeeded(Value::Null).label(), "succeeded");
    assert_eq!(CallOutcome::Denied(String::new()).label(), "denied");
    assert_eq!(CallOutcome::Failed(String::new()).label(), "failed");
    assert_eq!(CallOutcome::NotFound.label(), "not-found");
}

#[test]
fn elapsed_is_measured_around_the_seam() {
    let (sender, mut receiver) = unbounded_channel();
    let invoker = RecordingInvoker::new(FixedInvoker::default(), sender, Sequence::default());
    invoker.invoke("gh.issue.list", json!({}), None);

    let events = drain(&mut receiver);
    let SessionEvent::Capability(call) = &events[0] else {
        panic!("expected a capability event");
    };
    assert!(
        call.elapsed < Duration::from_secs(5),
        "a table lookup is not slow"
    );
}
