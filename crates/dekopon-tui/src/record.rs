//! Observation decorators around the two seams a session already passes everything through.
//!
//! Nothing in `dekopon-agent` reports a tool call. The prompt loop returns counters, `History`
//! keeps only the prompt and the answer, and `shell.command` spans deliberately carry an argument
//! *count* rather than argument values. So the console does not read a feed — it wraps the two
//! traits the loop is built on and watches what goes through them:
//!
//! - [`RecordingRuntime`] wraps [`ScriptRuntime`], so it sees each model-authored script and its
//!   outcome, which is one model turn's worth of work.
//! - [`RecordingInvoker`] wraps [`CapabilityInvoker`], so it sees every capability the script
//!   dispatched, with the exact JSON proposal and terminal status. Provider stdout stays on the
//!   shell's descriptor stream; observation never drains or replaces it.
//!
//! Both forward every method unchanged and neither can influence a session: an observer that could
//! deny a call would be a second authorization path, and there is only ever one of those.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use dekopon_agent::{
    progress::{ProgressEvent, ProgressSink},
    prompt::{ModelUsageObserver, PromptOutcome, ScriptRuntime},
};
use dekopon_model_token_governor::ModelUsage;
use dekopon_shell::{
    CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandProposal, JobControl,
    ScriptOutcome, Streams,
};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::{Payload, ShellEntry};

/// What one capability invocation did.
///
/// A flattened [`CapabilityCallResult`], because the console stores and redraws these long after
/// the call returned and the wire enum's borrow-free shape is what survives that.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallOutcome {
    /// The broker authorized it and the provider answered.
    Succeeded,
    /// Successful execution with a provider/gateway diagnostic, not a JSON output payload.
    SucceededWithStderr(String),
    /// Provider exit status and stderr, including downstream-close status 141.
    Exited { status: u8, stderr: String },
    /// Policy refused, with the broker's stable public reason.
    Denied(String),
    /// It ran and failed.
    Failed(String),
    /// No leg of this session claims that capability.
    NotFound,
}

impl CallOutcome {
    /// Stable low-cardinality label.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Succeeded | Self::SucceededWithStderr(_) => "succeeded",
            Self::Exited { .. } => "exited",
            Self::Denied(_) => "denied",
            Self::Failed(_) => "failed",
            Self::NotFound => "not-found",
        }
    }
}

impl From<&CapabilityCallResult> for CallOutcome {
    fn from(result: &CapabilityCallResult) -> Self {
        match result {
            CapabilityCallResult::Succeeded => Self::Succeeded,
            CapabilityCallResult::SucceededWithStderr(stderr) => {
                Self::SucceededWithStderr(stderr.clone())
            }
            CapabilityCallResult::Exited { status, stderr } => Self::Exited {
                status: status.get(),
                stderr: stderr.clone(),
            },
            CapabilityCallResult::Denied { reason } => Self::Denied(reason.clone()),
            CapabilityCallResult::Failed { error, .. } => Self::Failed(error.clone()),
            CapabilityCallResult::NotFound => Self::NotFound,
        }
    }
}

/// One capability invocation, observed at the dispatch seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityCall {
    /// Session-wide ordinal, so a call keeps its identity after the tree is folded and reopened.
    pub sequence: u64,
    /// Canonical capability identifier.
    pub capability: String,
    /// Exactly the input the interpreter dispatched, before any rendering.
    pub input: Value,
    /// What came back.
    pub outcome: CallOutcome,
    /// Wall-clock time inside the seam, which for a broker leg is the whole round trip.
    pub elapsed: Duration,
}

impl CapabilityCall {
    /// The proposal input. Provider output is a stream, retained by the script rather than
    /// fabricated as a per-capability JSON result.
    #[must_use]
    pub fn payloads(&self) -> Vec<(Payload, &Value)> {
        vec![(Payload::Input, &self.input)]
    }
}

/// One model-authored script, observed at the runtime seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptRun {
    /// Session-wide ordinal.
    pub sequence: u64,
    /// The script the model wrote, verbatim.
    pub script: String,
    /// Combined output the model will read back.
    pub output: String,
    /// The interpreter's exit code.
    pub exit_code: u8,
    /// Whether output hit an interpreter ceiling.
    pub truncated: bool,
    /// Capability invocations this script drove.
    pub capability_calls: u32,
    /// Interpreter steps it spent.
    pub steps: u64,
    /// Wall-clock time for the whole script.
    pub elapsed: Duration,
}

/// Everything the console learns while a session runs, in the order it happened.
///
/// One ordered channel rather than a structure the session mutates: the prompt loop is synchronous
/// and runs on a blocking task, the console draws on an async one, and the ordering of these events
/// is the only thing that reconstructs which capability belonged to which script.
#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// A script is about to run. Its calls follow until [`SessionEvent::ScriptFinished`].
    ScriptStarted {
        /// Session-wide ordinal, matching the eventual `ScriptFinished`.
        sequence: u64,
        /// The script the model wrote.
        script: String,
    },
    /// One capability was dispatched and answered.
    Capability(Box<CapabilityCall>),
    /// The script above finished.
    ScriptFinished(Box<ScriptRun>),
    /// The provider reported token accounting for one model response, or reported none.
    ModelUsage(Option<ModelUsage>),
    /// Bounded metadata about in-flight work; no model text or tool payload.
    Progress(String),
    /// The local bounded shell pane completed one line.
    ShellFinished(ShellEntry),
    /// The session ended. Always the last event.
    Finished(Box<Result<PromptOutcome, String>>),
}

/// Sender half handed to both decorators.
pub type SessionEvents = UnboundedSender<SessionEvent>;

/// Reports one event, naming the cause when the console is no longer listening.
///
/// A closed receiver means the console is tearing down, which is a legitimate state rather than a
/// failure to hide — but it is still the reason an event vanished, so it is said once per lost
/// event at `debug` rather than dropped silently. The session continues either way: a call the
/// broker has already accepted is not something an observer may abort.
fn emit(events: &SessionEvents, event: SessionEvent) {
    if events.send(event).is_err() {
        tracing::debug!(
            reason = "console-receiver-closed",
            "dropped a session event"
        );
    }
}

/// Session-wide ordinal source shared by both decorators.
///
/// One counter rather than two so a script and the calls inside it interleave in a single order an
/// operator can read, which is the whole point of the tree the console draws from these.
#[derive(Clone, Debug, Default)]
pub struct Sequence(Arc<AtomicU64>);

impl Sequence {
    /// Takes the next ordinal.
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

/// A [`CapabilityInvoker`] that reports every invocation and changes none of them.
///
/// Every other method forwards verbatim. `granted`, `describe`, and `resolve_command` answer from
/// the wrapped leg's snapshot and cost no round trip, so observing them would add volume without
/// adding anything an operator reads.
pub struct RecordingInvoker<I> {
    inner: I,
    events: SessionEvents,
    sequence: Sequence,
}

impl<I> RecordingInvoker<I> {
    /// Wraps one invoker.
    pub const fn new(inner: I, events: SessionEvents, sequence: Sequence) -> Self {
        Self {
            inner,
            events,
            sequence,
        }
    }
}

impl<I: CapabilityInvoker> CapabilityInvoker for RecordingInvoker<I> {
    fn cancel_signals(&self) -> Vec<dekopon_process::CancelSignal> {
        self.inner.cancel_signals()
    }

    fn script_started(&self, tree: &mut dekopon_process::TreeContext) {
        self.inner.script_started(tree);
    }

    fn job_control(&self) -> Option<&dyn JobControl> {
        self.inner.job_control()
    }

    fn granted(&self) -> Vec<String> {
        self.inner.granted()
    }

    fn is_granted(&self, capability: &str) -> bool {
        self.inner.is_granted(capability)
    }

    fn command_words(&self) -> Vec<String> {
        self.inner.command_words()
    }

    fn command_word_help(&self) -> BTreeMap<String, String> {
        self.inner.command_word_help()
    }

    fn has_command_word(&self, word: &str) -> bool {
        self.inner.has_command_word(word)
    }

    fn note(&self, text: &str, eta: Option<Duration>) {
        self.inner.note(text, eta);
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        self.inner.describe(capability)
    }

    fn script_finished(&self) {
        self.inner.script_finished();
    }

    fn run(
        &self,
        call: dekopon_shell::RunCall,
        streams: Streams,
        tree: &dekopon_process::TreeContext,
    ) -> dekopon_shell::RunResult {
        let sequence = self.sequence.next();
        let started = Instant::now();
        let outcome = self.inner.run(call, streams, tree);
        match &outcome {
            dekopon_shell::RunResult::Invoked {
                capability,
                input,
                result,
                ..
            } => emit(
                &self.events,
                SessionEvent::Capability(Box::new(CapabilityCall {
                    sequence,
                    capability: capability.clone(),
                    input: input.clone(),
                    outcome: CallOutcome::from(result),
                    elapsed: started.elapsed(),
                })),
            ),
            dekopon_shell::RunResult::Returned(_) | dekopon_shell::RunResult::Uncertain(_) => {}
        }
        outcome
    }

    fn invoke(
        &self,
        proposal: CommandProposal,
        streams: Streams,
        tree: &dekopon_process::TreeContext,
    ) -> CapabilityCallResult {
        let sequence = self.sequence.next();
        let capability = proposal.capability.clone();
        let recorded = proposal.input.clone();
        let started = Instant::now();
        // Pass the owned proposal (including secret intent and report) and descriptors unchanged.
        let result = self.inner.invoke(proposal, streams, tree);
        emit(
            &self.events,
            SessionEvent::Capability(Box::new(CapabilityCall {
                sequence,
                capability,
                input: recorded,
                outcome: CallOutcome::from(&result),
                elapsed: started.elapsed(),
            })),
        );
        result
    }
}

/// A [`ScriptRuntime`] that reports every script and changes none of them.
pub struct RecordingRuntime<R> {
    inner: R,
    events: SessionEvents,
    sequence: Sequence,
}

impl<R> RecordingRuntime<R> {
    /// Wraps one runtime.
    pub const fn new(inner: R, events: SessionEvents, sequence: Sequence) -> Self {
        Self {
            inner,
            events,
            sequence,
        }
    }
}

impl<R: ScriptRuntime> ScriptRuntime for RecordingRuntime<R> {
    fn capability_calls_used(&self) -> u32 {
        self.inner.capability_calls_used()
    }

    fn run_script(&self, script: &str) -> ScriptOutcome {
        let sequence = self.sequence.next();
        emit(
            &self.events,
            SessionEvent::ScriptStarted {
                sequence,
                script: script.to_owned(),
            },
        );
        let started = Instant::now();
        let outcome = self.inner.run_script(script);
        emit(
            &self.events,
            SessionEvent::ScriptFinished(Box::new(ScriptRun {
                sequence,
                script: script.to_owned(),
                output: outcome.output.clone(),
                exit_code: outcome.exit_code.get(),
                truncated: outcome.truncated,
                capability_calls: outcome.capability_calls,
                steps: outcome.steps,
                elapsed: started.elapsed(),
            })),
        );
        outcome
    }

    fn command_words(&self) -> Vec<String> {
        self.inner.command_words()
    }

    fn command_word_help(&self) -> BTreeMap<String, String> {
        self.inner.command_word_help()
    }
}

/// Reports model/tool milestones without a second stream of model-authored text or arguments.
pub struct RecordingProgress {
    events: SessionEvents,
}

impl RecordingProgress {
    /// Creates a progress adapter for the terminal event loop.
    pub const fn new(events: SessionEvents) -> Self {
        Self { events }
    }
}

impl ProgressSink for RecordingProgress {
    fn emit(&self, event: ProgressEvent) {
        let message = match event {
            ProgressEvent::ModelTurn { turn, of } => format!("model turn {turn}/{of}"),
            ProgressEvent::ToolStarted {
                word,
                calls_used,
                calls_max,
                ..
            } => format!("{} started · calls {calls_used}/{calls_max}", word.as_str()),
            ProgressEvent::ToolFinished { word, outcome, .. } => {
                format!("{}: {outcome:?}", word.as_str())
            }
            ProgressEvent::Note { text, .. } => text.as_str().to_owned(),
            ProgressEvent::Cancelled { .. } => "stopped; sent effects are not rolled back".into(),
            ProgressEvent::Failed { class } => format!("turn failed: {class:?}"),
            ProgressEvent::Finished {
                outcome,
                turns,
                tool_calls,
                ..
            } => format!("{outcome:?} · {turns} model turns · {tool_calls} tools"),
            ProgressEvent::Started { .. }
            | ProgressEvent::Steered { .. }
            | ProgressEvent::Answered { .. }
            | ProgressEvent::TextDelta { .. }
            | ProgressEvent::Attachment { .. }
            | ProgressEvent::KeepAlive { .. } => return,
        };
        emit(&self.events, SessionEvent::Progress(message));
    }
}

/// Forwards provider-reported token accounting into the same ordered channel.
pub struct RecordingUsage {
    events: SessionEvents,
}

impl RecordingUsage {
    /// Wraps one channel.
    pub const fn new(events: SessionEvents) -> Self {
        Self { events }
    }
}

impl ModelUsageObserver for RecordingUsage {
    fn observe(&self, usage: Option<ModelUsage>) {
        emit(&self.events, SessionEvent::ModelUsage(usage));
    }
}

#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod tests;
