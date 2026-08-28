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
//!   dispatched, with the exact JSON in and the exact result out.
//!
//! Both forward every method unchanged and neither can influence a session: an observer that could
//! deny a call would be a second authorization path, and there is only ever one of those.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use dekopon_agent::prompt::{ModelUsageObserver, PromptOutcome, ScriptRuntime};
use dekopon_model::model::ModelUsage;
use dekopon_shell::{
    CapabilityCallResult, CapabilityDescription, CapabilityInvoker, ScriptOutcome,
};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

/// What one capability invocation did.
///
/// A flattened [`CapabilityCallResult`], because the console stores and redraws these long after
/// the call returned and the wire enum's borrow-free shape is what survives that.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallOutcome {
    /// The broker authorized it and the provider answered.
    Succeeded(Value),
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
            Self::Succeeded(_) => "succeeded",
            Self::Denied(_) => "denied",
            Self::Failed(_) => "failed",
            Self::NotFound => "not-found",
        }
    }
}

impl From<&CapabilityCallResult> for CallOutcome {
    fn from(result: &CapabilityCallResult) -> Self {
        match result {
            CapabilityCallResult::Succeeded(output) => Self::Succeeded(output.clone()),
            CapabilityCallResult::Denied { reason } => Self::Denied(reason.clone()),
            CapabilityCallResult::Failed { error } => Self::Failed(error.clone()),
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
    fn granted(&self) -> Vec<String> {
        self.inner.granted()
    }

    fn is_granted(&self, capability: &str) -> bool {
        self.inner.is_granted(capability)
    }

    fn grants_namespace(&self, namespace: &str) -> bool {
        self.inner.grants_namespace(namespace)
    }

    fn command_words(&self) -> Vec<String> {
        self.inner.command_words()
    }

    fn has_command_word(&self, word: &str) -> bool {
        self.inner.has_command_word(word)
    }

    fn resolve_command(
        &self,
        word: &str,
        argv: &[String],
    ) -> Option<Result<(String, Value), String>> {
        self.inner.resolve_command(word, argv)
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        self.inner.describe(capability)
    }

    fn invoke(
        &self,
        capability: &str,
        input: Value,
        secret_use: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        let sequence = self.sequence.next();
        let recorded = input.clone();
        let started = Instant::now();
        let result = self.inner.invoke(capability, input, secret_use);
        emit(
            &self.events,
            SessionEvent::Capability(Box::new(CapabilityCall {
                sequence,
                capability: capability.to_owned(),
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
    fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome {
        let sequence = self.sequence.next();
        emit(
            &self.events,
            SessionEvent::ScriptStarted {
                sequence,
                script: script.to_owned(),
            },
        );
        let started = Instant::now();
        let outcome = self.inner.run_script(script, max_capability_calls);
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
mod tests;
