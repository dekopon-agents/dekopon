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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallOutcome {
    Succeeded,
    SucceededWithStderr(String),
    Exited { status: u8, stderr: String },
    Denied(String),
    Failed(String),
    NotFound,
}

impl CallOutcome {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityCall {
    pub sequence: u64,
    pub capability: String,
    pub input: Value,
    pub outcome: CallOutcome,
    pub elapsed: Duration,
}

impl CapabilityCall {
    #[must_use]
    pub fn payloads(&self) -> Vec<(Payload, &Value)> {
        vec![(Payload::Input, &self.input)]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptRun {
    pub sequence: u64,
    pub script: String,
    pub output: String,
    pub exit_code: u8,
    pub truncated: bool,
    pub capability_calls: u32,
    pub steps: u64,
    pub elapsed: Duration,
}

#[derive(Clone, Debug)]
pub enum SessionEvent {
    ScriptStarted { sequence: u64, script: String },
    Capability(Box<CapabilityCall>),
    ScriptFinished(Box<ScriptRun>),
    ModelUsage(Option<ModelUsage>),
    Progress(String),
    ShellFinished(ShellEntry),
    Finished(Box<Result<PromptOutcome, String>>),
}

pub type SessionEvents = UnboundedSender<SessionEvent>;

fn emit(events: &SessionEvents, event: SessionEvent) {
    if events.send(event).is_err() {
        tracing::debug!(
            reason = "console-receiver-closed",
            "dropped a session event"
        );
    }
}

#[derive(Clone, Debug, Default)]
pub struct Sequence(Arc<AtomicU64>);

impl Sequence {
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

pub struct RecordingInvoker<I> {
    inner: I,
    events: SessionEvents,
    sequence: Sequence,
}

impl<I> RecordingInvoker<I> {
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

pub struct RecordingRuntime<R> {
    inner: R,
    events: SessionEvents,
    sequence: Sequence,
}

impl<R> RecordingRuntime<R> {
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

    fn run_script(&self, script: &str, _participant: Option<&str>) -> ScriptOutcome {
        let sequence = self.sequence.next();
        emit(
            &self.events,
            SessionEvent::ScriptStarted {
                sequence,
                script: script.to_owned(),
            },
        );
        let started = Instant::now();
        let outcome = self.inner.run_script(script, _participant);
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

pub struct RecordingProgress {
    events: SessionEvents,
}

impl RecordingProgress {
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

pub struct RecordingUsage {
    events: SessionEvents,
}

impl RecordingUsage {
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
