#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
use std::collections::BTreeMap;
use std::time::Duration;

#[cfg(unix)]
use std::{
    collections::BTreeSet,
    collections::hash_map::RandomState,
    hash::{BuildHasher as _, Hasher as _},
    sync::atomic::{AtomicU32, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use dekopon_broker_protocol::TraceParent;
#[cfg(unix)]
use dekopon_broker_protocol::{
    Attestation, BrokerClient, ChatMemorySurface, ClientError, ERROR_UNAUTHENTICATED,
    InvocationOutcome, InvocationRequest, SessionSurface, Upcall, UpcallExit, UpcallStreams,
};
#[cfg(unix)]
use dekopon_core::{CapabilityId, InvocationId, PrincipalId, TraceId};
use dekopon_process::{
    CallBudget, CancelSignal, ExitCode, Limits as ShellLimits, ShellThreads, ThreadName,
    TreeContext,
};
#[cfg(unix)]
use dekopon_shell::CapabilityDescription;
use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, Interpreter, JobControl, ScriptOutcome,
};
use serde_json::Value;
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use thiserror::Error;

#[cfg(unix)]
use crate::attachment::{ChatAssetInputs, ChatAssetRefusal, ReplyAttachments};
use crate::{meta::EffectiveCapabilityView, prompt::ScriptRuntime};

pub mod attachment;
pub mod improvement;
pub mod memory;
pub mod memory_tools;
pub mod meta;
pub mod progress;
pub mod prompt;
pub mod skills;
pub mod wake;

pub use crate::progress::{
    BudgetLimit, CancelSource, CancelVia, CommandWord, FailureClass, ProgressEvent, ProgressNote,
    ProgressSink, SessionOutcome, ToolOutcome,
};

pub struct ShellRuntime<I> {
    pub invoker: I,
    pub limits: ShellLimits,
    pub calls: CallBudget,
    pub threads: ShellThreads,
}

impl<I: CapabilityInvoker> ScriptRuntime for ShellRuntime<I> {
    fn run_script(&self, script: &str, participant: Option<&str>) -> ScriptOutcome {
        if participant.is_some() {
            return ScriptOutcome {
                output: "Unknown participant handle: this runtime has no participant table."
                    .to_owned(),
                exit_code: ExitCode::from(2),
                truncated: false,
                capability_calls: 0,
                steps: 0,
            };
        }
        let mut tree = TreeContext::new(self.limits, self.calls.clone(), self.threads.clone())
            .with_cancel(self.invoker.cancel_signals());
        self.invoker.script_started(&mut tree);
        let outcome = Interpreter::new(self.limits, self.threads.clone()).run_with_tree(
            script,
            &self.invoker,
            &tree,
        );
        // Call script_finished before returning the outcome, or the next turn's progress event
        // reports out of order.
        self.invoker.script_finished();
        outcome
    }

    fn capability_calls_used(&self) -> u32 {
        self.calls.used()
    }

    fn command_words(&self) -> Vec<String> {
        self.invoker.command_words()
    }

    fn command_word_help(&self) -> BTreeMap<String, String> {
        self.invoker.command_word_help()
    }
}

pub struct SessionInvoker<D> {
    pub direct: D,
    pub broker: Option<Box<dyn CapabilityInvoker + Send + Sync>>,
}

impl<D: CapabilityInvoker> CapabilityInvoker for SessionInvoker<D> {
    fn script_started(&self, tree: &mut TreeContext) {
        self.direct.script_started(tree);
        if let Some(broker) = &self.broker {
            broker.script_started(tree);
        }
    }

    fn cancel_signals(&self) -> Vec<CancelSignal> {
        let mut signals = self.direct.cancel_signals();
        if let Some(broker) = &self.broker {
            signals.extend(broker.cancel_signals());
        }
        signals
    }

    fn granted(&self) -> Vec<String> {
        let mut granted = self.direct.granted();
        if let Some(broker) = &self.broker {
            granted.extend(broker.granted());
        }
        granted.sort_unstable();
        granted.dedup();
        granted
    }

    fn is_granted(&self, capability: &str) -> bool {
        self.direct.is_granted(capability)
            || self
                .broker
                .as_ref()
                .is_some_and(|broker| broker.is_granted(capability))
    }

    fn has_command_word(&self, word: &str) -> bool {
        self.direct.has_command_word(word)
            || self
                .broker
                .as_ref()
                .is_some_and(|broker| broker.has_command_word(word))
    }

    fn describe(&self, capability: &str) -> Option<dekopon_shell::CapabilityDescription> {
        self.direct.describe(capability).or_else(|| {
            self.broker
                .as_ref()
                .and_then(|broker| broker.describe(capability))
        })
    }

    fn invoke(
        &self,
        mut proposal: dekopon_shell::CommandProposal,
        streams: dekopon_shell::Streams,
        tree: &dekopon_process::TreeContext,
    ) -> CapabilityCallResult {
        let capability = &proposal.capability;
        // A secret-use proposal must reach only the broker leg; the direct leg has no authorizer to
        // check it.
        if proposal.secret_use.is_some() {
            return match &self.broker {
                Some(broker) if broker.is_granted(capability) => {
                    broker.invoke(proposal, streams, tree)
                }
                _ => dekopon_shell::secret_use_unsupported(),
            };
        }
        if self.direct.is_granted(capability) {
            let report = proposal.report.take();
            let result = self.direct.invoke(proposal, streams, tree);
            if let Some(report) = report {
                report.complete(report_outcome(call_outcome(&result)));
            }
            return result;
        }
        match &self.broker {
            Some(broker) => broker.invoke(proposal, streams, tree),
            None => CapabilityCallResult::NotFound,
        }
    }

    fn command_words(&self) -> Vec<String> {
        let mut words = self.direct.command_words();
        if let Some(broker) = &self.broker {
            words.extend(broker.command_words());
        }
        words.sort_unstable();
        words.dedup();
        words
    }

    fn command_word_help(&self) -> BTreeMap<String, String> {
        let mut pages = self.direct.command_word_help();
        if let Some(broker) = &self.broker {
            pages.extend(broker.command_word_help());
        }
        pages
    }

    fn run(
        &self,
        call: dekopon_shell::RunCall,
        streams: dekopon_shell::Streams,
        tree: &TreeContext,
    ) -> dekopon_shell::RunResult {
        if self.direct.has_command_word(&call.word) {
            return self.direct.run(call, streams, tree);
        }
        match &self.broker {
            Some(broker) => broker.run(call, streams, tree),
            None => dekopon_shell::render_command(
                streams,
                "",
                format!("dekopon-shell: {}: command not found\n", call.word),
                127,
            ),
        }
    }

    fn note(&self, text: &str, eta: Option<Duration>) {
        if let Some(broker) = &self.broker {
            broker.note(text, eta);
        }
    }

    fn script_finished(&self) {
        self.direct.script_finished();
        if let Some(broker) = &self.broker {
            broker.script_finished();
        }
    }

    fn job_control(&self) -> Option<&dyn JobControl> {
        self.direct
            .job_control()
            .or_else(|| self.broker.as_ref()?.job_control())
    }
}

#[must_use]
pub fn current_trace_parent() -> Option<TraceParent> {
    let parts = dekopon_telemetry::current_trace_context()?;
    TraceParent::new(parts.trace_id, parts.span_id, parts.flags).ok()
}

#[must_use]
pub fn session_trace_parent() -> TraceParent {
    current_trace_parent().unwrap_or_else(minted_trace_parent)
}

fn minted_trace_parent() -> TraceParent {
    let mut bytes = [0_u8; 24];
    if let Err(error) = getrandom::fill(&mut bytes) {
        tracing::warn!(
            event = "session_trace_entropy_unavailable",
            error = %error,
            "session.trace.entropy.unavailable"
        );
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let seed = RandomState::new();
        for (index, chunk) in bytes.chunks_mut(8).enumerate() {
            let mut hasher = seed.build_hasher();
            hasher.write_usize(index);
            hasher.write_u32(std::process::id());
            hasher.write_u128(nanos);
            chunk.copy_from_slice(&hasher.finish().to_be_bytes());
        }
    }
    let mut trace_id = [0_u8; 16];
    trace_id.copy_from_slice(&bytes[..16]);
    let mut parent_id = [0_u8; 8];
    parent_id.copy_from_slice(&bytes[16..]);
    // Setting the low bit avoids the one all-zero value W3C forbids for these identifiers, which a
    // 1-in-2^128 draw does not justify making every caller handle as a failure.
    trace_id[15] |= 1;
    parent_id[7] |= 1;
    TraceParent::new(trace_id, parent_id, 1)
        .expect("the low bit of each identifier is set, so neither is all zeroes")
}

#[cfg(unix)]
#[derive(Debug, Error)]
pub enum BrokerLegError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("the broker answered with duplicate capability identifiers: {capabilities}")]
    DuplicateCapabilities { capabilities: String },
}

#[cfg(unix)]
pub struct BrokerLeg {
    client: BrokerClient,
    runtime: tokio::runtime::Handle,
    capabilities: BTreeMap<String, CapabilityDescription>,
    effective_capabilities: Vec<EffectiveCapabilityView>,
    command_words: BTreeSet<String>,
    command_word_help: BTreeMap<String, String>,
    identifiers: IdSequence,
    attestation: Option<Attestation>,
    chat_memory: Option<ChatMemorySurface>,
    principal: Option<PrincipalId>,
    space: Option<Vec<dekopon_core::CapabilityId>>,
    cancel: CancelSignal,
    script_tree: parking_lot::Mutex<Option<TreeContext>>,
    attachments: Option<Arc<ReplyAttachments>>,
    asset_inputs: Option<ChatAssetInputs>,
    progress: Option<Arc<dyn ProgressSink>>,
    notes_enabled: bool,
    notes: AtomicU32,
    calls: CallBudget,
    jobs: Option<Arc<dyn JobControl>>,
}

#[cfg(unix)]
impl BrokerLeg {
    pub async fn connect(
        client: BrokerClient,
        attestation: Option<Attestation>,
    ) -> Result<Self, BrokerLegError> {
        let surface = client.session_surface(attestation.clone()).await?;
        Self::build(client, surface, attestation)
    }

    fn build(
        client: BrokerClient,
        surface: SessionSurface,
        attestation: Option<Attestation>,
    ) -> Result<Self, BrokerLegError> {
        let SessionSurface {
            capabilities: available,
            command_words,
            command_word_help,
            chat_memory,
            principal,
            space,
        } = surface;
        let (capabilities, effective_capabilities) = snapshot(available)?;
        Ok(Self {
            client,
            runtime: tokio::runtime::Handle::current(),
            capabilities,
            effective_capabilities,
            command_words: command_words.into_iter().collect(),
            command_word_help,
            identifiers: IdSequence::for_session(),
            attestation,
            chat_memory,
            principal,
            space,
            cancel: CancelSignal::never(),
            script_tree: parking_lot::Mutex::new(None),
            attachments: None,
            asset_inputs: None,
            progress: None,
            notes_enabled: false,
            notes: AtomicU32::new(0),
            calls: CallBudget::new(0),
            jobs: None,
        })
    }

    #[must_use]
    pub fn with_identifiers(mut self, identifiers: IdSequence) -> Self {
        self.identifiers = identifiers;
        self
    }

    #[must_use]
    pub fn with_participant(mut self, participant: &str) -> Self {
        if let Some(scope) = self
            .attestation
            .as_mut()
            .and_then(|claim| claim.scope.as_mut())
        {
            scope.participant = Some(participant.to_owned());
        }
        self
    }

    pub fn with_cancel_signal(mut self, signal: CancelSignal) -> Self {
        self.cancel = signal;
        self
    }

    #[must_use]
    pub fn with_job_control(mut self, jobs: Arc<dyn JobControl>) -> Self {
        self.jobs = Some(jobs);
        self
    }

    #[must_use]
    pub fn with_provider_attachments(mut self, slot: Arc<ReplyAttachments>) -> Self {
        self.attachments = Some(slot);
        self
    }

    #[must_use]
    pub fn with_chat_asset_inputs(mut self, inputs: ChatAssetInputs) -> Self {
        self.asset_inputs = Some(inputs);
        self
    }

    #[must_use]
    pub fn with_progress(mut self, sink: Arc<dyn ProgressSink>, calls: CallBudget) -> Self {
        self.progress = Some(sink);
        self.calls = calls;
        self
    }

    #[must_use]
    pub fn with_progress_notes(mut self) -> Self {
        self.notes_enabled = true;
        self
    }

    fn emit(&self, event: ProgressEvent) {
        if let Some(sink) = &self.progress {
            sink.emit(event);
        }
    }

    #[must_use]
    pub fn effective_capabilities(&self) -> Vec<EffectiveCapabilityView> {
        self.effective_capabilities.clone()
    }

    #[must_use]
    pub fn chat_memory_surface(&self) -> Option<&ChatMemorySurface> {
        self.chat_memory.as_ref()
    }

    #[must_use]
    pub fn space_grants(&self) -> Option<&[dekopon_core::CapabilityId]> {
        self.space.as_deref()
    }

    #[must_use]
    pub fn principal(&self) -> Option<&PrincipalId> {
        self.principal.as_ref()
    }

    #[must_use]
    pub const fn session_trace(&self) -> TraceId {
        self.identifiers.trace()
    }
}

#[cfg(unix)]
fn bounded_count(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

fn report_outcome(outcome: ToolOutcome) -> dekopon_shell::CommandReportOutcome {
    match outcome {
        ToolOutcome::Succeeded => dekopon_shell::CommandReportOutcome::Succeeded,
        ToolOutcome::Denied => dekopon_shell::CommandReportOutcome::Denied,
        ToolOutcome::Failed => dekopon_shell::CommandReportOutcome::Failed,
        ToolOutcome::Cancelled => dekopon_shell::CommandReportOutcome::Cancelled,
    }
}

fn call_outcome(result: &CapabilityCallResult) -> ToolOutcome {
    match result {
        CapabilityCallResult::Succeeded | CapabilityCallResult::SucceededWithStderr(_) => {
            ToolOutcome::Succeeded
        }
        CapabilityCallResult::Denied { .. } => ToolOutcome::Denied,
        CapabilityCallResult::Exited { .. }
        | CapabilityCallResult::Failed { .. }
        | CapabilityCallResult::NotFound => ToolOutcome::Failed,
    }
}

#[cfg(unix)]
fn snapshot(
    capabilities: Vec<dekopon_broker_protocol::AvailableCapability>,
) -> Result<
    (
        BTreeMap<String, CapabilityDescription>,
        Vec<EffectiveCapabilityView>,
    ),
    BrokerLegError,
> {
    let mut descriptions = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    let mut effective = Vec::with_capacity(capabilities.len());
    for available in capabilities {
        let id = available.capability.id.to_string();
        if descriptions.contains_key(&id) {
            duplicates.insert(id.clone());
        }
        effective.push(EffectiveCapabilityView {
            id: id.clone(),
            provider: available.provider.to_string(),
            description: available.capability.description.clone(),
            effect: available.capability.effect.to_string(),
            risk: available.capability.risk.to_string(),
        });
        descriptions.insert(
            id.clone(),
            CapabilityDescription {
                capability: id,
                description: available.capability.description,
            },
        );
    }
    if !duplicates.is_empty() {
        return Err(BrokerLegError::DuplicateCapabilities {
            capabilities: duplicates.into_iter().collect::<Vec<_>>().join(", "),
        });
    }
    effective.sort_by(|left, right| left.id.cmp(&right.id));
    Ok((descriptions, effective))
}

#[cfg(unix)]
impl CapabilityInvoker for BrokerLeg {
    fn participant_handle(&self) -> Option<&str> {
        self.attestation
            .as_ref()?
            .scope
            .as_ref()?
            .participant
            .as_deref()
    }

    fn cancel_signals(&self) -> Vec<CancelSignal> {
        vec![self.cancel.clone()]
    }

    fn script_started(&self, tree: &mut TreeContext) {
        let _entered = self.runtime.enter();
        let mut timer = Box::pin(tokio::time::sleep(tree.remaining()));
        let _polled = std::future::Future::poll(
            timer.as_mut(),
            &mut std::task::Context::from_waker(std::task::Waker::noop()),
        );
        *tree = tree.clone().with_script_timer(Some(timer));
        *self.script_tree.lock() = Some(tree.clone());
    }

    fn script_finished(&self) {
        self.script_tree.lock().take();
        self.notes.store(0, Ordering::Relaxed);
    }

    fn granted(&self) -> Vec<String> {
        self.capabilities.keys().cloned().collect()
    }

    fn is_granted(&self, capability: &str) -> bool {
        self.capabilities.contains_key(capability)
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        self.capabilities.get(capability).cloned()
    }

    fn command_words(&self) -> Vec<String> {
        self.command_words.iter().cloned().collect()
    }

    fn command_word_help(&self) -> BTreeMap<String, String> {
        self.command_word_help.clone()
    }

    fn has_command_word(&self, word: &str) -> bool {
        self.command_words.contains(word)
    }

    fn run(
        &self,
        call: dekopon_shell::RunCall,
        streams: dekopon_shell::Streams,
        tree: &TreeContext,
    ) -> dekopon_shell::RunResult {
        let reported = self.command_words.contains(&call.word).then(|| {
            let word = CommandWord::new(&call.word);
            self.emit(ProgressEvent::ToolStarted {
                word: word.clone(),
                argument_count: bounded_count(call.argv.len()),
                calls_used: self.calls.used(),
                calls_max: self.calls.maximum(),
            });
            (word, Instant::now())
        });
        let outcome = self.run_call(call, streams, tree);
        let result = match &outcome {
            dekopon_shell::RunResult::Invoked { result, .. }
            | dekopon_shell::RunResult::Returned(result)
            | dekopon_shell::RunResult::Uncertain(result) => result,
        };
        if let Some((word, started)) = reported {
            self.emit(ProgressEvent::ToolFinished {
                word,
                outcome: call_outcome(result),
                duration: started.elapsed(),
            });
        }
        outcome
    }

    fn invoke(
        &self,
        proposal: dekopon_shell::CommandProposal,
        streams: dekopon_shell::Streams,
        tree: &dekopon_process::TreeContext,
    ) -> CapabilityCallResult {
        let dekopon_shell::CommandProposal {
            capability,
            input,
            secret_use,
            report,
        } = proposal;
        let capability = capability.as_str();
        let cancelled = self.cancel.is_cancelled() || tree.is_cancelled();
        let calls_used = self.calls.used();
        // Checks the capability exists before reporting it on the progress surface, since reporting
        // an unvalidated identifier first would let a model put invented text on a person's chat
        // line.
        let reported = report
            .is_none()
            .then(|| {
                self.capabilities.contains_key(capability).then(|| {
                    let word = CommandWord::new(capability);
                    self.emit(ProgressEvent::ToolStarted {
                        word: word.clone(),
                        argument_count: bounded_count(
                            input.as_object().map_or(0, serde_json::Map::len),
                        ),
                        calls_used,
                        calls_max: self.calls.maximum(),
                    });
                    (word, Instant::now())
                })
            })
            .flatten();
        let (result, outcome) = if cancelled {
            (
                CapabilityCallResult::Denied {
                    reason: "session-cancelled".to_owned(),
                },
                ToolOutcome::Cancelled,
            )
        } else {
            let result = self.submit(capability, input, secret_use, streams, tree);
            let outcome = call_outcome(&result);
            (result, outcome)
        };
        if let Some(report) = report {
            report.complete(report_outcome(outcome));
        }
        if let Some((word, started)) = reported {
            self.emit(ProgressEvent::ToolFinished {
                word,
                outcome,
                duration: started.elapsed(),
            });
        }
        result
    }

    fn note(&self, text: &str, eta: Option<Duration>) {
        const MAX_NOTES_PER_SCRIPT: u32 = 8;
        if !self.notes_enabled || self.progress.is_none() {
            return;
        }
        let Some(text) = ProgressNote::new(text) else {
            tracing::debug!(
                event = "agent_progress_note_dropped",
                reason = "empty",
                "agent.progress.note.dropped"
            );
            return;
        };
        if self
            .notes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < MAX_NOTES_PER_SCRIPT).then_some(count + 1)
            })
            .is_err()
        {
            tracing::debug!(
                event = "agent_progress_note_dropped",
                reason = "script-cap",
                "agent.progress.note.dropped"
            );
            return;
        }
        self.emit(ProgressEvent::Note { text, eta });
    }

    fn job_control(&self) -> Option<&dyn JobControl> {
        self.jobs.as_deref()
    }
}

#[cfg(unix)]
impl BrokerLeg {
    async fn prepare_run_assets(
        &self,
        argv: &[dekopon_shell::Arg],
        sends_remaining: u8,
    ) -> Result<attachment::PreparedRun, ChatAssetRefusal> {
        match &self.asset_inputs {
            Some(inputs) => inputs.prepare_run(argv, sends_remaining).await,
            None => {
                let (argv, ids) = attachment::run_arguments(argv)?;
                if !ids.is_empty() {
                    return Err(ChatAssetRefusal::UnknownAsset);
                }
                Ok(attachment::PreparedRun {
                    argv,
                    assets: dekopon_broker_protocol::InvokeAssets {
                        sends_remaining,
                        ..Default::default()
                    },
                    pins: Vec::new(),
                })
            }
        }
    }

    fn run_call(
        &self,
        call: dekopon_shell::RunCall,
        streams: dekopon_shell::Streams,
        tree: &TreeContext,
    ) -> dekopon_shell::RunResult {
        use dekopon_broker_protocol::{RunOutcome, RunRequest};
        use dekopon_shell::{RunResult, render_command};
        if self.cancel.is_cancelled() || tree.is_cancelled() {
            return RunResult::Returned(CapabilityCallResult::Denied {
                reason: "session-cancelled".to_owned(),
            });
        }
        let word = call.word.clone();
        let invocation = self.identifiers.next_invocation();
        let invocation_text = invocation.to_string();
        let remaining = self.attachments.as_ref().map_or(0, |slot| slot.remaining());
        let dispatcher = tracing::dispatcher::get_default(Clone::clone);
        let cancel = &self.cancel;
        let prepared = self.runtime.block_on(async {
            tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                () = tokio::time::sleep(tree.remaining()) => None,
                result = async {
                    let prepared = self.prepare_run_assets(&call.argv, remaining).await?;
                    let output = streams.stdout.try_clone().map_err(|error| {
                        ChatAssetRefusal::Storage(dekopon_model::asset::BlobError::from(error))
                    })?;
                    Ok::<_, ChatAssetRefusal>((prepared, output))
                } => Some(result),
            }
        });
        let (
            attachment::PreparedRun {
                argv,
                mut assets,
                pins: _pins,
            },
            output,
        ) = match prepared {
            None => {
                return RunResult::Returned(CapabilityCallResult::Denied {
                    reason: "script-cancelled".to_owned(),
                });
            }
            Some(Err(refusal)) => {
                tracing::warn!(target: "dekopon_agent::audit", { audit.event = "agent.chat_asset_input.refused", reason = refusal.reason() }, "agent.chat_asset_input.refused");
                return RunResult::Returned(CapabilityCallResult::Denied {
                    reason: format!(
                        "the gateway refused this call before it reached the broker: {}",
                        refusal.note()
                    ),
                });
            }
            Some(Ok(prepared)) => prepared,
        };
        if cancel.is_cancelled() || tree.remaining().is_zero() {
            return RunResult::Returned(CapabilityCallResult::Denied {
                reason: "script-cancelled".to_owned(),
            });
        }
        assets.streams = Some(dekopon_broker_protocol::Streams {
            stdin: streams.stdin,
            stdout: streams.stdout,
        });
        let submitted = upcall_scope(|scope| {
            self.runtime.block_on(async {
                tokio::select! {
                    biased;
                    outcome = self.client.run(
                        self.attestation.clone(),
                        RunRequest { invocation, trace_parent: self.identifiers.trace_parent(), word: call.word, argv },
                        assets,
                        async |upcall| self.answer(scope, upcall, tree, &dispatcher).await,
                    ) => Some(outcome),
                    () = cancel.cancelled() => None,
                    () = tokio::time::sleep(tree.remaining()) => None,
                }
            })
        });
        match submitted {
            None => RunResult::Uncertain(CapabilityCallResult::Denied {
                reason: "script-cancelled".to_owned(),
            }),
            Some(Err(error))
                if !matches!(&error, ClientError::Remote { .. }) && !error.may_have_executed() =>
            {
                RunResult::Returned(CapabilityCallResult::Failed {
                    error: error.to_string(),
                    detail: None,
                })
            }
            Some(Err(error)) => RunResult::Uncertain(CapabilityCallResult::Denied {
                reason: format!(
                    "the broker did not record an outcome for this invocation and it may already have taken effect; do not resubmit it ({error})"
                ),
            }),
            Some(Ok(response)) => {
                let streams = dekopon_shell::Streams {
                    stdin: None,
                    stdout: output,
                };
                match response.outcome {
                    RunOutcome::Rendered {
                        stdout,
                        stderr,
                        status,
                    } => render_command(streams, &stdout, stderr, status),
                    RunOutcome::Refused { error } => render_command(streams, "", error.message, 2),
                    RunOutcome::NotFound => render_command(
                        streams,
                        "",
                        format!("dekopon-shell: {word}: command not found\n"),
                        127,
                    ),
                    RunOutcome::NotGranted { capability } => render_command(
                        streams,
                        "",
                        format!(
                            "dekopon-shell: {word}: requires capability {capability}, which is not granted in this session\n"
                        ),
                        127,
                    ),
                    RunOutcome::Invoked {
                        capability,
                        input,
                        secret_use,
                        result,
                        attached,
                        removed,
                        sent,
                    } => {
                        let result = self.receive_outcome(
                            dekopon_broker_protocol::AssetInvocationOutcome {
                                result,
                                attached,
                                removed,
                                sent,
                                descriptors: response.descriptors,
                            },
                            capability.as_str(),
                            &invocation_text,
                        );
                        RunResult::Invoked {
                            capability: capability.to_string(),
                            input,
                            secret_use,
                            result,
                        }
                    }
                }
            }
        }
    }

    fn receive_outcome(
        &self,
        outcome: dekopon_broker_protocol::AssetInvocationOutcome,
        capability: &str,
        invocation: &str,
    ) -> CapabilityCallResult {
        let result = outcome.result;
        match result.outcome {
            InvocationOutcome::Succeeded => {
                let note = if let Some(slot) = &self.attachments {
                    slot.receive(
                        outcome.attached,
                        outcome.descriptors,
                        outcome.removed,
                        outcome.sent,
                        capability,
                        invocation,
                    )
                } else if !outcome.attached.is_empty()
                    || !outcome.removed.is_empty()
                    || !outcome.sent.is_empty()
                {
                    "[gateway: capability executed but this embedder has no asset store; received asset effects could not be retained or delivered; do not repeat the paid call]".to_owned()
                } else {
                    String::new()
                };
                let mut stderr = result
                    .detail
                    .map(|detail| detail.message)
                    .unwrap_or_default();
                if !note.is_empty() {
                    if !stderr.is_empty() && !stderr.ends_with('\n') {
                        stderr.push('\n');
                    }
                    stderr.push_str(&note);
                    stderr.push('\n');
                }
                if stderr.is_empty() {
                    CapabilityCallResult::Succeeded
                } else {
                    CapabilityCallResult::SucceededWithStderr(stderr)
                }
            }
            InvocationOutcome::Denied => CapabilityCallResult::Denied {
                reason: result
                    .error
                    .unwrap_or_else(|| "authorization refused this invocation".to_owned()),
            },
            InvocationOutcome::Failed => match result.exit_status {
                Some(status) => CapabilityCallResult::Exited {
                    status,
                    stderr: result
                        .detail
                        .map(|detail| detail.message)
                        .unwrap_or_default(),
                },
                None => CapabilityCallResult::Failed {
                    error: result
                        .error
                        .unwrap_or_else(|| "the broker reported a failed invocation".to_owned()),
                    detail: result.detail,
                },
            },
        }
    }

    fn submit(
        &self,
        capability: &str,
        input: Value,
        secret_use: Option<dekopon_core::SecretUseProposal>,
        streams: dekopon_shell::Streams,
        tree: &TreeContext,
    ) -> CapabilityCallResult {
        let Ok(parsed) = capability.parse::<CapabilityId>() else {
            return CapabilityCallResult::NotFound;
        };
        // This is a visibility check only, not an authorization decision; it just avoids spending
        // capability-call budget on probes with guessed identifiers, and any real refusal still
        // comes from the broker.
        if !self.capabilities.contains_key(capability) {
            return CapabilityCallResult::NotFound;
        }
        // Safe only because this runs on a spawn_blocking thread; calling block_on here from an
        // ordinary runtime worker would deadlock the executor.
        let dispatcher = tracing::dispatcher::get_default(Clone::clone);
        // A child script runs on its own scoped thread, never inside this future: its broker calls
        // re-enter `Handle::block_on`, which panics on a thread already inside one. Cancel drops this
        // future and its connection first; the scope then joins a child that sees the same cancel.
        let cancel = &self.cancel;
        let submitted = upcall_scope(|scope| {
            self.runtime.block_on(async {
                tokio::select! {
                    result = async {
                        let mut assets = dekopon_broker_protocol::InvokeAssets {
                            sends_remaining: self.attachments.as_ref().map_or(0, |slot| slot.remaining()),
                            ..Default::default()
                        };
                        assets.streams = Some(dekopon_broker_protocol::Streams {
                            stdin: streams.stdin,
                            stdout: streams.stdout,
                        });
                        let request = InvocationRequest {
                            id: self.identifiers.next_invocation(),
                            capability: parsed,
                            trace_parent: self.identifiers.trace_parent(),
                            secret_use,
                            input,
                        };
                        let invocation = request.id.to_string();
                        let outcome = self.client.invoke(
                            self.attestation.clone(),
                            request,
                            assets,
                            async |upcall| self.answer(scope, upcall, tree, &dispatcher).await,
                        ).await;
                        Ok::<_, ChatAssetRefusal>((outcome, invocation))
                    } => Some(result),
                    () = cancel.cancelled() => None,
                    () = tokio::time::sleep(tree.remaining()) => None,
                }
            })
        });
        match submitted {
            None => CapabilityCallResult::Denied {
                reason: "script-cancelled".to_owned(),
            },
            // This refusal is permanent, the interpreter's non-retryable Denied rather than a
            // retryable Failed, and produces no broker audit record since no proposal was ever
            // submitted.
            Some(Err(refusal)) => CapabilityCallResult::Denied {
                reason: format!(
                    "the gateway refused this call before it reached the broker: {}",
                    refusal.note()
                ),
            },
            Some(Ok((Ok(outcome), invocation))) => {
                self.receive_outcome(outcome, capability, &invocation)
            }
            Some(Ok((Err(ClientError::Remote { code, message }), _)))
                if code == ERROR_UNAUTHENTICATED =>
            {
                CapabilityCallResult::Denied { reason: message }
            }
            // A client-side timeout cannot tell whether the call ran, so it is treated as the one
            // non-retryable Denied status with an explicit refusal to resubmit, rather than a
            // Failed a model would retry.
            Some(Ok((Err(error), _))) if error.may_have_executed() => {
                CapabilityCallResult::Denied {
                    reason: format!(
                        "the broker did not record an outcome for this invocation and it may already \
                     have taken effect; do not resubmit it ({error})"
                    ),
                }
            }
            // Every ClientError renders without the socket path, since this is the one path that
            // could otherwise leak DEKOPON_BROKER_SOCKET back into a script.
            Some(Ok((Err(error), _))) => CapabilityCallResult::Failed {
                error: error.to_string(),
                detail: None,
            },
        }
    }
}

#[cfg(unix)]
impl BrokerLeg {
    async fn answer<'scope>(
        &'scope self,
        scope: &'scope std::thread::Scope<'scope, '_>,
        upcall: Upcall,
        tree: &'scope TreeContext,
        dispatcher: &tracing::Dispatch,
    ) -> UpcallExit {
        let Upcall {
            parent,
            script,
            trace_parent,
            streams: UpcallStreams { stdout, stdin },
        } = upcall;
        let (script_stdin, feed) = match stdin.map(child_stdin).transpose() {
            Ok(Some((reader, feed))) => (Some(reader), Some(feed)),
            Ok(None) => (None, None),
            Err(error) => {
                tracing::warn!(
                    event = "gateway_child_stdin_unavailable",
                    error = %error,
                    "gateway.child.stdin.unavailable"
                );
                return UpcallExit {
                    status: ExitCode::FAILURE.get(),
                    stderr: "dekopon-shell: the child script's stdin could not be opened"
                        .to_owned(),
                };
            }
        };
        let span = tracing::info_span!("gateway.upcall", parent = %parent);
        if let Some(trace_parent) = trace_parent
            && let Err(error) = dekopon_telemetry::parent_remote(
                &span,
                dekopon_telemetry::TraceContextParts {
                    trace_id: trace_parent.trace_id(),
                    span_id: trace_parent.parent_id(),
                    flags: trace_parent.flags(),
                },
            )
        {
            tracing::debug!(
                event = "gateway_upcall_trace_parent_ignored",
                error = %error,
                "gateway.upcall.trace.parent.ignored"
            );
        }
        let (finished, outcome) = tokio::sync::oneshot::channel();
        let dispatcher = dispatcher.clone();
        let tree = tree.clone().with_parent_cancel(self.cancel.clone());
        let [permit] = match tree.shell_threads().reserve() {
            Ok(permit) => permit,
            Err(limit) => return deliver_child(stdout, ScriptOutcome::refused(limit)).await,
        };
        let spawned = permit.spawn(scope, UPCALL_THREAD, move || {
            let outcome =
                tracing::dispatcher::with_default(&dispatcher, || {
                    span.in_scope(|| {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            Interpreter::new(*tree.limits(), tree.shell_threads().clone())
                                .run_child(&script, script_stdin, self, &tree)
                        }))
                    })
                });
            #[allow(
                clippy::let_underscore_must_use,
                reason = "a closed receiver means the invocation was cancelled and nobody waits"
            )]
            let _ = finished.send(outcome.ok());
        });
        if let Err(error) = spawned {
            tracing::warn!(
                event = "gateway_child_thread_unstarted",
                parent = %parent,
                error = %error,
                "gateway.child.thread.unstarted"
            );
            return UpcallExit {
                status: ExitCode::FAILURE.get(),
                stderr: "dekopon-shell: the child script's thread could not start".to_owned(),
            };
        }
        let pumped = async {
            if let Some((source, sink)) = feed {
                pump_child_stdin(source, sink).await;
            }
            std::future::pending::<std::convert::Infallible>().await
        };
        let outcome = tokio::select! {
            outcome = outcome => outcome.ok().flatten(),
            never = pumped => match never {},
        };
        let Some(outcome) = outcome else {
            tracing::warn!(
                event = "gateway_child_script_panicked",
                parent = %parent,
                "gateway.child.script.panicked"
            );
            return UpcallExit {
                status: CHILD_PANICKED,
                stderr: "dekopon-shell: the child script panicked".to_owned(),
            };
        };
        deliver_child(stdout, outcome).await
    }
}

#[cfg(unix)]
const UPCALL_THREAD: ThreadName = ThreadName::new("sh-upcall");

#[cfg(unix)]
#[expect(
    clippy::disallowed_methods,
    reason = "owner: the broker call, whose scope joins every child-script upcall before returning; bound: each upcall's ShellThreads permit"
)]
fn upcall_scope<'env, T>(
    run: impl for<'scope> FnOnce(&'scope std::thread::Scope<'scope, 'env>) -> T,
) -> T {
    std::thread::scope(run)
}

#[cfg(unix)]
async fn deliver_child(stdout: OwnedFd, outcome: ScriptOutcome) -> UpcallExit {
    let mut text = outcome.output;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if let Err(error) = write_child_stdout(stdout, text.as_bytes()).await {
        tracing::debug!(
            event = "gateway_child_stdout_unread",
            error = %error,
            "gateway.child.stdout.unread"
        );
    }
    UpcallExit {
        status: outcome.exit_code.get(),
        stderr: String::new(),
    }
}

#[cfg(unix)]
const CHILD_PANICKED: u8 = 70;

#[cfg(unix)]
fn child_stdin(
    source: OwnedFd,
) -> std::io::Result<(
    dekopon_shell::ChildStdin,
    (OwnedFd, std::os::unix::net::UnixStream),
)> {
    let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
    Ok((dekopon_shell::ChildStdin::adopt(reader)?, (source, writer)))
}

#[cfg(unix)]
fn nonblocking(socket: std::os::unix::net::UnixStream) -> std::io::Result<tokio::net::UnixStream> {
    socket.set_nonblocking(true)?;
    tokio::net::UnixStream::from_std(socket)
}

#[cfg(unix)]
async fn pump_child_stdin(source: OwnedFd, sink: std::os::unix::net::UnixStream) {
    let pumped = async {
        let mut source = nonblocking(std::os::unix::net::UnixStream::from(source))?;
        let mut sink = nonblocking(sink)?;
        tokio::io::copy(&mut source, &mut sink).await
    };
    if let Err(error) = pumped.await {
        tracing::debug!(
            event = "gateway_child_stdin_ended",
            error = %error,
            "gateway.child.stdin.ended"
        );
    }
}

#[cfg(unix)]
async fn write_child_stdout(stdout: OwnedFd, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let mut stdout = nonblocking(std::os::unix::net::UnixStream::from(stdout))?;
    stdout.write_all(bytes).await
}

#[cfg(unix)]
#[derive(Clone)]
pub struct IdSequence {
    parent: TraceParent,
    next: Arc<AtomicU32>,
}

#[cfg(unix)]
impl IdSequence {
    #[must_use]
    pub fn for_session() -> Self {
        Self {
            parent: session_trace_parent(),
            next: Arc::new(AtomicU32::new(1)),
        }
    }

    #[must_use]
    pub const fn trace(&self) -> TraceId {
        self.parent.trace()
    }

    #[must_use]
    pub fn trace_parent(&self) -> TraceParent {
        current_trace_parent()
            .filter(|live| live.trace() == self.parent.trace())
            .unwrap_or(self.parent)
    }

    #[must_use]
    pub fn next_invocation(&self) -> InvocationId {
        let counter = self.next.fetch_add(1, Ordering::Relaxed);
        format!("{}-{counter}", self.parent.trace())
            .parse()
            .expect("a trace and a counter are a valid invocation identifier")
    }
}

#[cfg(unix)]
impl Default for IdSequence {
    fn default() -> Self {
        Self::for_session()
    }
}

// Always emit duration as whole milliseconds; a mixed type here makes the telemetry backend reject
// later records.
pub(crate) fn milliseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use dekopon_shell::{
        CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandProposal,
    };
    use serde_json::json;
    use std::{io::Read, os::fd::OwnedFd, os::unix::net::UnixStream};

    use super::{SessionInvoker, current_trace_parent};

    fn streams() -> (dekopon_shell::Streams, UnixStream) {
        let (writer, reader) = UnixStream::pair().expect("stdout socketpair");
        reader
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("stdout read timeout");
        (
            dekopon_shell::Streams {
                stdin: None,
                stdout: OwnedFd::from(writer),
            },
            reader,
        )
    }

    fn output(mut reader: UnixStream) -> String {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .expect("every stdout writer closes by the time the call returns");
        String::from_utf8(bytes).expect("stdout text")
    }

    #[test]
    fn trace_parent_is_absent_without_an_active_exporting_span() {
        assert!(current_trace_parent().is_none());
    }

    struct FakeLeg {
        capability: &'static str,
        marker: &'static str,
        invoked: parking_lot::Mutex<Vec<String>>,
        secret_uses: parking_lot::Mutex<Vec<Option<dekopon_core::SecretUseProposal>>>,
    }

    impl FakeLeg {
        fn new(capability: &'static str, marker: &'static str) -> Self {
            Self {
                capability,
                marker,
                invoked: parking_lot::Mutex::new(Vec::new()),
                secret_uses: parking_lot::Mutex::new(Vec::new()),
            }
        }
    }

    impl CapabilityInvoker for FakeLeg {
        fn granted(&self) -> Vec<String> {
            vec![self.capability.to_owned()]
        }

        fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
            (capability == self.capability).then(|| CapabilityDescription {
                capability: capability.to_owned(),
                description: self.marker.to_owned(),
            })
        }

        fn invoke(
            &self,
            proposal: dekopon_shell::CommandProposal,
            streams: dekopon_shell::Streams,
            _tree: &dekopon_process::TreeContext,
        ) -> CapabilityCallResult {
            let capability = proposal.capability;
            let secret_use = proposal.secret_use;
            if capability != self.capability {
                return CapabilityCallResult::NotFound;
            }
            self.invoked.lock().push(capability.to_owned());
            self.secret_uses.lock().push(secret_use);
            streams.reply(&json!({ "leg": self.marker }))
        }
    }

    #[test]
    fn direct_capabilities_are_preferred_over_the_broker() {
        let shared = Box::new(FakeLeg::new("shared.capability", "broker"));
        let invoker = SessionInvoker {
            direct: FakeLeg::new("shared.capability", "direct"),
            broker: Some(shared),
        };

        let (streams, reader) = streams();
        assert_eq!(
            invoker.invoke(
                CommandProposal::new("shared.capability", json!({}), None),
                streams,
                &dekopon_process::TreeContext::new(
                    dekopon_process::Limits::default(),
                    dekopon_process::CallBudget::new(64),
                    dekopon_test_support::shell_threads()
                )
            ),
            CapabilityCallResult::Succeeded
        );
        assert_eq!(output(reader), "{\"leg\":\"direct\"}\n");
    }

    #[test]
    fn capabilities_absent_from_direct_mode_fall_through_to_the_broker() {
        let invoker = SessionInvoker {
            direct: FakeLeg::new("cli-probe.upper", "direct"),
            broker: Some(Box::new(FakeLeg::new("http-probe.fetch", "broker"))),
        };

        let (streams, reader) = streams();
        assert_eq!(
            invoker.invoke(
                CommandProposal::new("http-probe.fetch", json!({}), None),
                streams,
                &dekopon_process::TreeContext::new(
                    dekopon_process::Limits::default(),
                    dekopon_process::CallBudget::new(64),
                    dekopon_test_support::shell_threads()
                )
            ),
            CapabilityCallResult::Succeeded
        );
        assert_eq!(output(reader), "{\"leg\":\"broker\"}\n");
        assert!(invoker.is_granted("http-probe.fetch"));
        assert_eq!(
            invoker.granted(),
            vec!["cli-probe.upper".to_owned(), "http-probe.fetch".to_owned()]
        );
        assert_eq!(
            invoker
                .describe("http-probe.fetch")
                .map(|it| it.description),
            Some("broker".to_owned())
        );
    }

    #[test]
    fn a_session_without_a_broker_is_exactly_as_capable_as_direct_mode() {
        let invoker = SessionInvoker {
            direct: FakeLeg::new("cli-probe.upper", "direct"),
            broker: None,
        };

        assert_eq!(invoker.granted(), vec!["cli-probe.upper".to_owned()]);
        assert!(!invoker.is_granted("http-probe.fetch"));
        assert_eq!(
            invoker.invoke(
                CommandProposal::new("http-probe.fetch", json!({}), None),
                streams().0,
                &dekopon_process::TreeContext::new(
                    dekopon_process::Limits::default(),
                    dekopon_process::CallBudget::new(64),
                    dekopon_test_support::shell_threads()
                )
            ),
            CapabilityCallResult::NotFound
        );
    }

    #[test]
    fn a_secret_use_proposal_reaches_only_a_broker_backed_capability() {
        let proposal = dekopon_core::SecretUseProposal::HttpBearer {
            secret: "drn:com.xrl:secret:prod:api/token"
                .parse::<dekopon_core::SecretDrn>()
                .expect("canonical DRN"),
        };
        let broker = Box::new(FakeLeg::new("http-probe.fetch", "broker"));
        let invoker = SessionInvoker {
            direct: FakeLeg::new("cli-probe.upper", "direct"),
            broker: Some(broker),
        };

        assert_eq!(
            invoker.invoke(
                CommandProposal::new("http-probe.fetch", json!({}), Some(proposal.clone())),
                streams().0,
                &dekopon_process::TreeContext::new(
                    dekopon_process::Limits::default(),
                    dekopon_process::CallBudget::new(64),
                    dekopon_test_support::shell_threads()
                )
            ),
            CapabilityCallResult::Succeeded
        );

        assert_eq!(
            invoker.invoke(
                CommandProposal::new("cli-probe.upper", json!({}), Some(proposal)),
                streams().0,
                &dekopon_process::TreeContext::new(
                    dekopon_process::Limits::default(),
                    dekopon_process::CallBudget::new(64),
                    dekopon_test_support::shell_threads()
                )
            ),
            dekopon_shell::secret_use_unsupported()
        );
        assert!(
            invoker.direct.secret_uses.lock().is_empty(),
            "the direct leg was handed a proposal it cannot authorize"
        );
    }

    #[derive(Default)]
    struct JobsLeg {
        calls: parking_lot::Mutex<Vec<String>>,
    }

    impl dekopon_shell::JobControl for JobsLeg {
        fn start(
            &self,
            seed: dekopon_shell::JobSeed,
        ) -> Result<dekopon_process::JobId, dekopon_shell::JobRefusal> {
            self.calls.lock().push(format!("start {}", seed.text()));
            Ok(dekopon_process::JobId::new(7))
        }

        fn list(&self) -> Vec<dekopon_process::JobSummary> {
            self.calls.lock().push("list".to_owned());
            Vec::new()
        }

        fn wait(
            &self,
            ids: &[dekopon_process::JobId],
            _keep_waiting: &dyn Fn() -> bool,
        ) -> Vec<Result<dekopon_process::JobWait, dekopon_shell::JobRefusal>> {
            ids.iter()
                .map(|id| {
                    self.calls.lock().push(format!("wait {id}"));
                    Ok(dekopon_process::JobWait::Interrupted)
                })
                .collect()
        }

        fn kill(&self, id: dekopon_process::JobId) -> Result<(), dekopon_shell::JobRefusal> {
            self.calls.lock().push(format!("kill {id}"));
            Ok(())
        }
    }

    impl CapabilityInvoker for JobsLeg {
        fn granted(&self) -> Vec<String> {
            Vec::new()
        }

        fn job_control(&self) -> Option<&dyn dekopon_shell::JobControl> {
            Some(self)
        }

        fn invoke(
            &self,
            _: CommandProposal,
            _streams: dekopon_shell::Streams,
            _tree: &dekopon_process::TreeContext,
        ) -> CapabilityCallResult {
            CapabilityCallResult::NotFound
        }
    }

    #[test]
    fn a_session_invoker_reaches_the_job_control_of_its_broker_leg() {
        let leg = std::sync::Arc::new(JobsLeg::default());
        let invoker = SessionInvoker {
            direct: FakeLeg::new("cli-probe.upper", "direct"),
            broker: Some(Box::new(std::sync::Arc::clone(&leg))),
        };
        let outcome = dekopon_shell::Interpreter::new(
            dekopon_process::Limits::default(),
            dekopon_test_support::shell_threads(),
        )
        .run("echo a &", &invoker);
        assert_eq!(outcome.output, "[7]");
        let jobs = invoker.job_control().expect("the broker leg's job control");
        let id = dekopon_process::JobId::new(7);
        assert!(jobs.list().is_empty());
        assert_eq!(
            jobs.wait(&[id], &|| false),
            [Ok(dekopon_process::JobWait::Interrupted)]
        );
        assert_eq!(jobs.kill(id), Ok(()));
        assert_eq!(
            *leg.calls.lock(),
            ["start echo a", "list", "wait 7", "kill 7"]
        );
        let without = SessionInvoker {
            direct: FakeLeg::new("cli-probe.upper", "direct"),
            broker: None,
        };
        assert!(without.job_control().is_none());
    }

    struct CommandLeg {
        word: &'static str,
        capability: &'static str,
    }

    impl CapabilityInvoker for CommandLeg {
        fn granted(&self) -> Vec<String> {
            Vec::new()
        }

        fn is_granted(&self, capability: &str) -> bool {
            capability == self.capability
        }

        fn command_words(&self) -> Vec<String> {
            vec![self.word.to_owned()]
        }

        fn has_command_word(&self, word: &str) -> bool {
            word == self.word
        }

        fn invoke(
            &self,
            _: dekopon_shell::CommandProposal,
            _streams: dekopon_shell::Streams,
            _tree: &dekopon_process::TreeContext,
        ) -> CapabilityCallResult {
            CapabilityCallResult::NotFound
        }
    }

    #[test]
    fn command_words_and_grants_survive_both_legs_rather_than_falling_back_to_the_defaults() {
        let invoker = SessionInvoker {
            direct: CommandLeg {
                word: "probe",
                capability: "cli-probe.upper",
            },
            broker: Some(Box::new(CommandLeg {
                word: "gh",
                capability: "gh.pr-view",
            })),
        };

        assert_eq!(
            invoker.command_words(),
            vec!["gh".to_owned(), "probe".to_owned()],
            "a word a provider contributed became `command not found`"
        );
        assert!(invoker.has_command_word("probe"));
        assert!(invoker.has_command_word("gh"));
        assert!(!invoker.has_command_word("git"));

        assert!(invoker.granted().is_empty());
        assert!(
            invoker.is_granted("cli-probe.upper"),
            "the direct leg holds it"
        );
        assert!(invoker.is_granted("gh.pr-view"), "the broker leg holds it");
        assert!(!invoker.is_granted("gh.pr-merge"));
    }

    #[cfg(unix)]
    mod broker_leg {
        use super::{output, streams};
        use parking_lot::Mutex;
        use std::{
            collections::{BTreeMap, BTreeSet},
            os::unix::fs::PermissionsExt as _,
            path::Path,
            sync::{Arc, atomic::AtomicU32},
            time::Duration,
        };

        use dekopon_broker_protocol::{
            BrokerClient, BrokerRequest, DescriptorStream, ERROR_UNAUTHENTICATED, FrameLimits,
            InvocationOutcome, InvocationResult, RequestEnvelope, ResponseEnvelope,
        };
        use dekopon_capability::DecisionReference;
        use dekopon_core::{
            AgentId, ExternalSubject, ProviderFailureDetail, SecretDrn, SecretUseProposal,
        };
        use dekopon_process::CancelSignal;
        use dekopon_process::ExitCode;
        use dekopon_process::Limits;
        use dekopon_shell::{
            CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandProposal,
        };
        use serde_json::json;
        use tokio::{
            net::UnixListener,
            sync::{mpsc, oneshot},
        };

        use crate::{
            Attestation, BrokerLeg, IdSequence, ProgressEvent, ProgressSink, ShellRuntime,
            current_trace_parent, meta::EffectiveCapabilityView, prompt::ScriptRuntime,
        };

        const CAPABILITY: &str = "http-probe.fetch";
        const SUBJECT: &str = "slack.t0123abc.u9xyz";

        fn server_uid() -> u32 {
            rustix::process::geteuid().as_raw()
        }

        fn private_broker_directory() -> tempfile::TempDir {
            let directory = tempfile::tempdir().expect("temporary broker directory");
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .expect("private broker directory");
            directory
        }

        fn result(outcome: InvocationOutcome, error: Option<&str>) -> InvocationResult {
            InvocationResult {
                invocation: "invoke-stub".parse().expect("valid invocation fixture"),
                decision: DecisionReference {
                    decision_id: "decision-stub".to_owned(),
                    authorized_by: "broker-stub".parse().expect("valid principal fixture"),
                    policy_digest: "a".repeat(64),
                },
                outcome,
                exit_status: None,
                error: error.map(str::to_owned),
                detail: None,
                evidence: Vec::new(),
            }
        }

        async fn stub_leg(directory: &Path, responses: Vec<ResponseEnvelope>) -> BrokerLeg {
            let (leg, _observed) = stub_leg_observing(directory, responses, None).await;
            leg
        }

        async fn stub_leg_observing(
            directory: &Path,
            responses: Vec<ResponseEnvelope>,
            attestation: Option<Attestation>,
        ) -> (BrokerLeg, mpsc::UnboundedReceiver<RequestEnvelope>) {
            let socket = directory.join("broker.sock");
            let listener = UnixListener::bind(&socket).expect("bind stub broker");
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
                .expect("secure stub socket");
            let (observed, receiver) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                let mut responses = responses
                    .into_iter()
                    .map(|response| (response, Vec::new()))
                    .collect();
                while !std::collections::VecDeque::is_empty(&responses) {
                    let (stream, _) = listener.accept().await.expect("stub broker accepts");
                    let mut stream = DescriptorStream::new(stream);
                    let (request, _) = stream
                        .read_frame::<RequestEnvelope>(FrameLimits::default())
                        .await
                        .expect("stub broker reads one request");
                    let (response, _) =
                        dekopon_test_support::next_broker_reply(&request, &mut responses).unwrap();
                    drop(observed.send(request));
                    stream
                        .write_frame(&response, &[], FrameLimits::default())
                        .await
                        .expect("stub broker writes one response");
                }
            });

            (leg_with(&socket, attestation), receiver)
        }

        async fn stub_leg_parked(
            directory: &Path,
        ) -> (
            BrokerLeg,
            mpsc::UnboundedReceiver<RequestEnvelope>,
            oneshot::Sender<()>,
        ) {
            let socket = directory.join("broker.sock");
            let listener = UnixListener::bind(&socket).expect("bind stub broker");
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
                .expect("secure stub socket");
            let (observed, receiver) = mpsc::unbounded_channel();
            let (release, released) = oneshot::channel::<()>();
            tokio::spawn(async move {
                let (stream, _) = listener.accept().await.expect("stub broker accepts");
                let mut stream = DescriptorStream::new(stream);
                let (request, _) = stream
                    .read_frame::<RequestEnvelope>(FrameLimits::default())
                    .await
                    .expect("stub broker reads one request");
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "the test may have finished observing before the stub reports"
                )]
                let _ = observed.send(request);
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "a dropped sender and a sent signal both mean the test is done"
                )]
                let _ = released.await;
                drop(stream);
            });
            (leg_for(&socket), receiver, release)
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_run_error_is_uncertain_and_never_refunds_its_call() {
            let directory = private_broker_directory();
            let (mut leg, mut observed) = stub_leg_observing(
                directory.path(),
                vec![ResponseEnvelope::error(
                    dekopon_broker_protocol::ERROR_BROKER_UNAVAILABLE,
                    "unavailable",
                )],
                None,
            )
            .await;
            leg.command_words.insert("probe".to_owned());
            let calls = dekopon_process::CallBudget::new(1);
            let tree = dekopon_process::TreeContext::new(
                Limits::default(),
                calls.clone(),
                dekopon_test_support::shell_threads(),
            );
            let outcome = tokio::task::spawn_blocking(move || {
                dekopon_shell::Interpreter::new(
                    Limits::default(),
                    dekopon_test_support::shell_threads(),
                )
                .run_with_tree("probe upper", &leg, &tree)
            })
            .await
            .unwrap();
            assert_eq!(outcome.exit_code, ExitCode::DENIED);
            assert!(outcome.output.contains("do not resubmit it"));
            assert_eq!(calls.used(), 1);
            assert!(matches!(
                observed.recv().await.unwrap().request,
                BrokerRequest::Run { .. }
            ));
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn cancellation_during_asset_resolution_refunds_without_a_frame() {
            use crate::attachment::{ChatAssetInputs, ChatAssetSource, ResolveFuture};
            struct PendingAsset(Mutex<Option<oneshot::Sender<()>>>);
            impl ChatAssetSource for PendingAsset {
                fn resolve_input(&self, _: u64) -> ResolveFuture<'_> {
                    let entered = self.0.lock().take().unwrap();
                    Box::pin(async move {
                        entered.send(()).unwrap();
                        std::future::pending().await
                    })
                }
            }
            let directory = private_broker_directory();
            let socket = directory.path().join("broker.sock");
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let (entered, resolving) = oneshot::channel();
            let (handle, signal) = CancelSignal::pair();
            let mut leg = leg_for(&socket)
                .with_cancel_signal(signal)
                .with_chat_asset_inputs(ChatAssetInputs::new(Arc::new(PendingAsset(Mutex::new(
                    Some(entered),
                )))));
            leg.command_words.insert("probe".to_owned());
            let calls = dekopon_process::CallBudget::new(1);
            let tree = dekopon_process::TreeContext::new(
                Limits::default(),
                calls.clone(),
                dekopon_test_support::shell_threads(),
            );
            let run = tokio::task::spawn_blocking(move || {
                dekopon_shell::Interpreter::new(
                    Limits::default(),
                    dekopon_test_support::shell_threads(),
                )
                .run_with_tree("probe chat-asset:1", &leg, &tree)
            });
            resolving.await.unwrap();
            assert_eq!(calls.used(), 1);
            handle.cancel();
            let outcome = run.await.unwrap();
            assert_eq!(outcome.exit_code, ExitCode::DENIED);
            assert_eq!(calls.used(), 0);
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn cancellation_after_a_run_frame_keeps_its_call_charged() {
            let directory = private_broker_directory();
            let (mut leg, mut observed, release) = stub_leg_parked(directory.path()).await;
            leg.command_words.insert("probe".to_owned());
            let (handle, signal) = CancelSignal::pair();
            let leg = leg.with_cancel_signal(signal);
            let calls = dekopon_process::CallBudget::new(1);
            let tree = dekopon_process::TreeContext::new(
                Limits::default(),
                calls.clone(),
                dekopon_test_support::shell_threads(),
            );
            let run = tokio::task::spawn_blocking(move || {
                dekopon_shell::Interpreter::new(
                    Limits::default(),
                    dekopon_test_support::shell_threads(),
                )
                .run_with_tree("probe upper", &leg, &tree)
            });
            assert!(matches!(
                observed.recv().await.unwrap().request,
                BrokerRequest::Run { .. }
            ));
            handle.cancel();
            let outcome = run.await.unwrap();
            assert_eq!(outcome.exit_code, ExitCode::DENIED);
            assert_eq!(calls.used(), 1);
            drop(release);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn pre_frame_asset_refusals_emit_one_reason_and_submit_no_frame() {
            use crate::attachment::{
                ChatAssetInputs, ChatAssetRefusal, ChatAssetSource, ResolveFuture,
            };
            use tracing_subscriber::prelude::*;
            struct UnknownAsset;
            impl ChatAssetSource for UnknownAsset {
                fn resolve_input(&self, _: u64) -> ResolveFuture<'_> {
                    Box::pin(async { Err(ChatAssetRefusal::UnknownAsset) })
                }
            }
            for (script, refusal) in [
                (
                    "probe --image='data:image/png;base64,AAAA'",
                    ChatAssetRefusal::DataUrl,
                ),
                ("probe chat-asset:9", ChatAssetRefusal::UnknownAsset),
                (
                    "probe chat-asset:1 chat-asset:2 chat-asset:3 chat-asset:4 chat-asset:5 chat-asset:6",
                    ChatAssetRefusal::PerInvocationLimit,
                ),
            ] {
                let directory = private_broker_directory();
                let socket = directory.path().join("broker.sock");
                let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
                std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
                listener.set_nonblocking(true).unwrap();
                let mut leg = leg_for(&socket)
                    .with_chat_asset_inputs(ChatAssetInputs::new(Arc::new(UnknownAsset)));
                leg.command_words.insert("probe".to_owned());
                let capture = dekopon_test_support::CaptureLayer::new();
                let subscriber = tracing_subscriber::registry().with(capture.clone());
                let calls = dekopon_process::CallBudget::new(1);
                let tree = dekopon_process::TreeContext::new(
                    Limits::default(),
                    calls.clone(),
                    dekopon_test_support::shell_threads(),
                );
                let outcome = tokio::task::spawn_blocking(move || {
                    tracing::subscriber::with_default(subscriber, || {
                        dekopon_shell::Interpreter::new(
                            Limits::default(),
                            dekopon_test_support::shell_threads(),
                        )
                        .run_with_tree(script, &leg, &tree)
                    })
                })
                .await
                .unwrap();
                assert_eq!(outcome.exit_code, ExitCode::DENIED);
                assert_eq!(calls.used(), 0);
                let events = capture.events_where(
                    "dekopon_agent::audit",
                    &[("audit.event", "agent.chat_asset_input.refused")],
                );
                assert_eq!(events.len(), 1);
                match &events[0] {
                    dekopon_test_support::Record::Event { target, level, .. } => {
                        assert_eq!(target, "dekopon_agent::audit");
                        assert_eq!(*level, "WARN");
                    }
                    dekopon_test_support::Record::Span { .. } => panic!("expected a refusal event"),
                }
                assert_eq!(events[0].field("reason"), Some(refusal.reason()));
                assert!(
                    matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
                );
            }
        }

        fn leg_for(socket: &Path) -> BrokerLeg {
            leg_with(socket, None)
        }

        fn attestation() -> Attestation {
            Attestation {
                subject: SUBJECT
                    .parse::<ExternalSubject>()
                    .expect("canonical subject fixture"),
                agent: "chat-agent"
                    .parse::<AgentId>()
                    .expect("valid agent fixture"),
                scope: None,
                invocation: None,
            }
        }

        fn leg_with(socket: &Path, attestation: Option<Attestation>) -> BrokerLeg {
            let mut capabilities = BTreeMap::new();
            capabilities.insert(
                CAPABILITY.to_owned(),
                CapabilityDescription {
                    capability: CAPABILITY.to_owned(),
                    description: "Fetches one broker-authorized URI".to_owned(),
                },
            );
            BrokerLeg {
                client: BrokerClient::new(socket, server_uid(), FrameLimits::default())
                    .expect("stub broker client"),
                runtime: tokio::runtime::Handle::current(),
                capabilities,
                effective_capabilities: vec![EffectiveCapabilityView {
                    id: CAPABILITY.to_owned(),
                    provider: "http-probe".to_owned(),
                    description: "Fetches one broker-authorized URI".to_owned(),
                    effect: "read-only".to_owned(),
                    risk: "Low".to_owned(),
                }],
                command_words: BTreeSet::new(),
                command_word_help: BTreeMap::new(),
                identifiers: IdSequence::for_session(),
                attestation,
                chat_memory: None,
                principal: None,
                space: None,
                cancel: CancelSignal::never(),
                script_tree: parking_lot::Mutex::new(None),
                attachments: None,
                asset_inputs: None,
                progress: None,
                notes_enabled: false,
                notes: AtomicU32::new(0),
                calls: dekopon_process::CallBudget::new(0),
                jobs: None,
            }
        }

        async fn invoke_with_stdout(
            leg: BrokerLeg,
            capability: &'static str,
        ) -> (CapabilityCallResult, String) {
            tokio::task::spawn_blocking(move || {
                let (streams, reader) = streams();
                let result = leg.invoke(
                    CommandProposal::new(capability, json!({"uri": "http://x/"}), None),
                    streams,
                    &dekopon_process::TreeContext::new(
                        dekopon_process::Limits::default(),
                        dekopon_process::CallBudget::new(64),
                        dekopon_test_support::shell_threads(),
                    ),
                );
                (result, output(reader))
            })
            .await
            .expect("blocking dispatch completes")
        }

        async fn invoke(leg: BrokerLeg, capability: &'static str) -> CapabilityCallResult {
            tokio::task::spawn_blocking(move || {
                leg.invoke(
                    CommandProposal::new(capability, json!({"uri": "http://x/"}), None),
                    streams().0,
                    &dekopon_process::TreeContext::new(
                        dekopon_process::Limits::default(),
                        dekopon_process::CallBudget::new(64),
                        dekopon_test_support::shell_threads(),
                    ),
                )
            })
            .await
            .expect("blocking dispatch completes")
        }

        mod nesting {
            use std::{
                io::Write as _,
                os::fd::{AsFd as _, OwnedFd},
                os::unix::fs::PermissionsExt as _,
                path::{Path, PathBuf},
                time::Duration,
            };

            use dekopon_broker_protocol::{
                BrokerClient, BrokerRequest, BrokerResponse, ChatScopeClaim, ChatTransportKind,
                Conversation, ConversationKind, DescriptorStream, FrameLimits, InvocationOutcome,
                ProtocolVersion, RequestEnvelope, ResponseEnvelope, TraceParent, Trigger,
                UpcallStdin,
            };
            use dekopon_process::CancelSignal;
            use dekopon_process::ExitCode;
            use dekopon_process::{CallBudget, Limits, TreeContext};
            use dekopon_shell::{CapabilityCallResult, CapabilityInvoker as _, CommandProposal};
            use opentelemetry::trace::TracerProvider as _;
            use serde_json::json;
            use tokio::{
                io::AsyncReadExt as _,
                net::{UnixListener, UnixStream},
                sync::oneshot,
            };
            use tracing_subscriber::layer::SubscriberExt as _;

            use super::{
                CAPABILITY, SUBJECT, leg_with, private_broker_directory, result, server_uid,
            };
            use crate::{Attestation, BrokerLeg};

            struct Call {
                stream: DescriptorStream,
                run: bool,
                uri: String,
                attestation: Option<Attestation>,
                trace_parent: TraceParent,
                descriptors: Vec<OwnedFd>,
            }

            impl Call {
                async fn upcall(
                    &mut self,
                    script: &str,
                    trace_parent: Option<TraceParent>,
                ) -> UnixStream {
                    let (read, write) =
                        std::os::unix::net::UnixStream::pair().expect("child stdout");
                    let upcall = ResponseEnvelope {
                        api_version: ProtocolVersion::V1Alpha2,
                        response: BrokerResponse::Upcall {
                            parent: "invoke-parent".parse().expect("invocation"),
                            script: script.to_owned(),
                            trace_parent,
                            stdin: UpcallStdin::None,
                        },
                    };
                    self.stream
                        .write_frame(&upcall, &[write.as_fd()], FrameLimits::default())
                        .await
                        .expect("fake broker writes the upcall");
                    drop(write);
                    read.set_nonblocking(true)
                        .expect("nonblocking child stdout");
                    UnixStream::from_std(read).expect("child stdout")
                }

                async fn upcall_with_stdin(
                    &mut self,
                    script: &str,
                    kind: UpcallStdin,
                    source: &std::os::unix::net::UnixStream,
                ) -> UnixStream {
                    let (read, write) =
                        std::os::unix::net::UnixStream::pair().expect("child stdout");
                    self.stream
                        .write_frame(
                            &ResponseEnvelope {
                                api_version: ProtocolVersion::V1Alpha2,
                                response: BrokerResponse::Upcall {
                                    parent: "invoke-parent".parse().expect("invocation"),
                                    script: script.to_owned(),
                                    trace_parent: None,
                                    stdin: kind,
                                },
                            },
                            &[write.as_fd(), source.as_fd()],
                            FrameLimits::default(),
                        )
                        .await
                        .expect("fake broker writes stdin upcall");
                    drop(write);
                    read.set_nonblocking(true)
                        .expect("nonblocking child stdout");
                    UnixStream::from_std(read).expect("child stdout")
                }

                async fn child_exit(&mut self, mut stdout: UnixStream) -> (String, u8) {
                    let mut text = String::new();
                    stdout
                        .read_to_string(&mut text)
                        .await
                        .expect("child stdout text");
                    let (request, _) = self
                        .stream
                        .read_frame::<RequestEnvelope>(FrameLimits::default())
                        .await
                        .expect("the gateway answers the upcall");
                    let BrokerRequest::UpcallResult { status, .. } = request.request else {
                        panic!("expected an upcall result");
                    };
                    (text, status)
                }

                async fn answer(mut self, outcome: InvocationOutcome, stdout: &str) {
                    let mut out = std::os::unix::net::UnixStream::from(
                        self.descriptors
                            .pop()
                            .expect("the invoke carries its stdout"),
                    );
                    out.write_all(stdout.as_bytes()).expect("provider stdout");
                    drop(out);
                    self.stream
                        .write_frame(
                            &if self.run {
                                ResponseEnvelope {
                                    api_version: ProtocolVersion::V1Alpha2,
                                    response: BrokerResponse::Ran {
                                        outcome: dekopon_broker_protocol::RunOutcome::Invoked {
                                            capability: CAPABILITY.parse().unwrap(),
                                            input: json!({"uri":self.uri}),
                                            secret_use: None,
                                            result: result(outcome, None),
                                            attached: Vec::new(),
                                            removed: Vec::new(),
                                            sent: Vec::new(),
                                        },
                                    },
                                }
                            } else {
                                ResponseEnvelope::invocation(
                                    result(outcome, None),
                                    Vec::new(),
                                    Vec::new(),
                                    Vec::new(),
                                )
                            },
                            &[],
                            FrameLimits::default(),
                        )
                        .await
                        .expect("fake broker answers");
                }

                async fn hung_up(mut self) -> bool {
                    self.stream
                        .read_frame::<RequestEnvelope>(FrameLimits::default())
                        .await
                        .is_err()
                }
            }

            fn listen(directory: &Path) -> (UnixListener, PathBuf) {
                let socket = directory.join("broker.sock");
                let listener = UnixListener::bind(&socket).expect("bind fake broker");
                std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
                    .expect("secure fake socket");
                (listener, socket)
            }

            async fn accept(listener: &UnixListener) -> Call {
                let (stream, _) = listener.accept().await.expect("fake broker accepts");
                let mut stream = DescriptorStream::new(stream);
                let (request, descriptors) = stream
                    .read_frame::<RequestEnvelope>(FrameLimits::default())
                    .await
                    .expect("fake broker reads a request");
                let run = matches!(&request.request, BrokerRequest::Run { .. });
                let (attestation, invocation) = match request.request {
                    BrokerRequest::Run {
                        attestation,
                        invocation,
                        trace_parent,
                        argv,
                        ..
                    } => {
                        let uri = match argv.last() {
                            Some(dekopon_broker_protocol::Arg::Text(text)) => text.as_str(),
                            _ => "",
                        };
                        (
                            attestation,
                            dekopon_broker_protocol::InvocationRequest {
                                id: invocation,
                                trace_parent,
                                capability: CAPABILITY.parse().unwrap(),
                                input: json!({"uri":uri}),
                                secret_use: None,
                            },
                        )
                    }
                    BrokerRequest::Invoke {
                        attestation,
                        invocation,
                        ..
                    } => (attestation, invocation),
                    other @ (BrokerRequest::Capabilities { .. }
                    | BrokerRequest::RecordDeliveredTurn { .. }
                    | BrokerRequest::UpcallResult { .. }
                    | BrokerRequest::PrepareGeneration { .. }
                    | BrokerRequest::CommitGeneration { .. }
                    | BrokerRequest::AbortGeneration { .. }
                    | BrokerRequest::ObserveGeneration) => {
                        panic!("unexpected request {other:?}")
                    }
                };
                Call {
                    run,
                    stream,
                    uri: invocation.input["uri"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    attestation,
                    trace_parent: invocation.trace_parent,
                    descriptors,
                }
            }

            fn probe_leg(socket: &Path, attestation: Option<Attestation>) -> BrokerLeg {
                let mut leg = leg_with(socket, attestation);
                leg.command_words.insert("probe".to_owned());
                leg
            }

            fn tree(calls: &CallBudget) -> TreeContext {
                TreeContext::new(
                    Limits::default(),
                    calls.clone(),
                    dekopon_test_support::shell_threads(),
                )
            }

            fn call(leg: &BrokerLeg, tree: &TreeContext) -> CapabilityCallResult {
                let (stdout, _reader) = std::os::unix::net::UnixStream::pair().expect("stdout");
                leg.invoke(
                    CommandProposal::new(CAPABILITY, json!({"uri": "http://a/"}), None),
                    dekopon_shell::Streams {
                        stdin: None,
                        stdout: OwnedFd::from(stdout),
                    },
                    tree,
                )
            }

            async fn invoke_in(leg: BrokerLeg) -> CapabilityCallResult {
                tokio::task::spawn_blocking(move || call(&leg, &tree(&CallBudget::new(64))))
                    .await
                    .expect("blocking dispatch completes")
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn cancel_depth_two_descendants() {
                let directory = private_broker_directory();
                let (listener, socket) = listen(directory.path());
                let (handle, signal) = CancelSignal::pair();
                let leg = probe_leg(&socket, None).with_cancel_signal(signal);
                let (reached, depth_two) = oneshot::channel();
                let broker = tokio::spawn(async move {
                    let mut parent = accept(&listener).await;
                    let _parent_stdout = parent.upcall("probe --uri http://b/", None).await;
                    let mut child = accept(&listener).await;
                    let _child_stdout = child.upcall("probe --uri http://c/", None).await;
                    let grandchild = accept(&listener).await;
                    reached
                        .send(grandchild.uri.clone())
                        .expect("the test waits");
                    [
                        grandchild.hung_up().await,
                        child.hung_up().await,
                        parent.hung_up().await,
                    ]
                });
                let call = tokio::spawn(invoke_in(leg));
                assert_eq!(depth_two.await.expect("depth two reached"), "http://c/");
                handle.cancel();
                assert!(matches!(
                    call.await.expect("the root call returns"),
                    CapabilityCallResult::Denied { reason } if reason == "script-cancelled"
                ));
                let ended = tokio::time::timeout(Duration::from_secs(10), broker)
                    .await
                    .expect("every connection closes")
                    .expect("fake broker");
                assert_eq!(ended, [true; 3]);
            }

            #[tokio::test(flavor = "current_thread", start_paused = true)]
            async fn child_280s_gateway_io_270s_paused() {
                let directory = private_broker_directory();
                let (listener, socket) = listen(directory.path());
                let mut leg = probe_leg(&socket, None);
                leg.client = BrokerClient::new(
                    &socket,
                    server_uid(),
                    FrameLimits {
                        io_timeout: Duration::from_secs(270),
                        ..FrameLimits::default()
                    },
                )
                .expect("client");
                let (parked, child_parked) = oneshot::channel();
                let broker = tokio::spawn(async move {
                    let mut parent = accept(&listener).await;
                    let stdout = parent.upcall("probe --uri http://slow/", None).await;
                    let child = accept(&listener).await;
                    let started = tokio::time::Instant::now();
                    parked.send(()).expect("the test advances the clock");
                    tokio::time::sleep(Duration::from_secs(280)).await;
                    child.answer(InvocationOutcome::Succeeded, "slow\n").await;
                    let exit = parent.child_exit(stdout).await;
                    parent.answer(InvocationOutcome::Succeeded, "").await;
                    (exit, started.elapsed())
                });
                let root = tokio::task::spawn_blocking(move || {
                    let limits = Limits {
                        timeout: Duration::from_secs(300),
                        ..Limits::default()
                    };
                    call(
                        &leg,
                        &TreeContext::new(
                            limits,
                            CallBudget::new(64),
                            dekopon_test_support::shell_threads(),
                        ),
                    )
                });
                child_parked.await.expect("the child call is parked");
                for _ in 0..280 {
                    tokio::time::advance(Duration::from_secs(1)).await;
                    tokio::task::yield_now().await;
                }
                assert_eq!(
                    root.await.expect("the root call returns"),
                    CapabilityCallResult::Succeeded
                );
                let ((stdout, status), parked) = broker.await.expect("fake broker");
                assert_eq!((stdout.as_str(), status), ("slow\n", 0));
                assert!(parked >= Duration::from_secs(280));
            }

            #[tokio::test(flavor = "current_thread", start_paused = true)]
            async fn job_and_probe_child_invocations_end_at_tree_deadline() {
                for trigger in [Trigger::Job, Trigger::Probe] {
                    let directory = private_broker_directory();
                    let (listener, socket) = listen(directory.path());
                    let scope = ChatScopeClaim {
                        participant: None,
                        github: None,
                        transport: "scientist-slack".parse().expect("transport"),
                        kind: ChatTransportKind::Slack,
                        conversation: Conversation {
                            kind: ConversationKind::Channel,
                            container: Some("t0123abc".to_owned()),
                            id: "c0123abc".to_owned(),
                            thread: None,
                        },
                        delivery: None,
                        trigger,
                    };
                    let attestation = Attestation::for_chat(
                        SUBJECT.parse().expect("subject"),
                        "reviewer".parse().expect("agent"),
                        scope,
                    );
                    let leg = probe_leg(&socket, Some(attestation));
                    let (parked, child_parked) = oneshot::channel();
                    let broker = tokio::spawn(async move {
                        let mut parent = accept(&listener).await;
                        let stdout = parent.upcall("probe --uri http://child/", None).await;
                        let mut child = accept(&listener).await;
                        // The guest is waiting for status, not reading stdout. A real host
                        // owns the descriptor; discard this fixture's copy to permit EOF.
                        child.descriptors.clear();
                        parked.send(()).expect("the test advances the clock");
                        let ended = child.hung_up().await;
                        drop(stdout);
                        ended && parent.hung_up().await
                    });
                    let root = tokio::task::spawn_blocking(move || {
                        let limits = Limits {
                            timeout: Duration::from_secs(30),
                            ..Limits::default()
                        };
                        call(
                            &leg,
                            &TreeContext::new(
                                limits,
                                CallBudget::new(64),
                                dekopon_test_support::shell_threads(),
                            ),
                        )
                    });
                    child_parked.await.expect("child invocation parked");
                    let start = tokio::time::Instant::now();
                    tokio::time::advance(Duration::from_secs(30)).await;
                    assert!(matches!(
                        tokio::time::timeout(Duration::from_secs(1), root)
                            .await
                            .expect("deadline ends outstanding invocation")
                            .expect("blocking call completes"),
                        CapabilityCallResult::Denied { reason } if reason == "script-cancelled"
                    ));
                    assert!(start.elapsed() <= Duration::from_secs(31));
                    assert!(broker.await.expect("both connections close"));
                }
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn nested_job_and_probe_denied_write() {
                for trigger in [Trigger::Probe, Trigger::Job] {
                    let directory = private_broker_directory();
                    let (listener, socket) = listen(directory.path());
                    let scope = ChatScopeClaim {
                        participant: None,
                        github: None,
                        transport: "scientist-slack".parse().expect("transport"),
                        kind: ChatTransportKind::Slack,
                        conversation: Conversation {
                            kind: ConversationKind::Channel,
                            container: Some("t0123abc".to_owned()),
                            id: "c0123abc".to_owned(),
                            thread: None,
                        },
                        delivery: None,
                        trigger,
                    };
                    let attestation = Attestation::for_chat(
                        SUBJECT.parse().expect("subject"),
                        "chat-agent".parse().expect("agent"),
                        scope,
                    );
                    let leg = probe_leg(&socket, Some(attestation));
                    let broker = tokio::spawn(async move {
                        let mut parent = accept(&listener).await;
                        let stdout = parent.upcall("probe --uri http://write/", None).await;
                        let child = accept(&listener).await;
                        let seen = child
                            .attestation
                            .as_ref()
                            .and_then(|claim| claim.scope.as_ref())
                            .map(|scope| scope.trigger);
                        child.answer(InvocationOutcome::Denied, "").await;
                        let (_, status) = parent.child_exit(stdout).await;
                        parent.answer(InvocationOutcome::Succeeded, "").await;
                        (seen, status)
                    });
                    assert_eq!(invoke_in(leg).await, CapabilityCallResult::Succeeded);
                    assert_eq!(
                        broker.await.expect("fake broker"),
                        (Some(trigger), ExitCode::DENIED.get())
                    );
                }
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn inherited_and_reader_stdin_reach_the_child_provider_stage() {
                for kind in [UpcallStdin::Inherit, UpcallStdin::Reader] {
                    let directory = private_broker_directory();
                    let (listener, socket) = listen(directory.path());
                    let leg = probe_leg(&socket, None);
                    let broker = tokio::spawn(async move {
                        let mut parent = accept(&listener).await;
                        let (source, mut writer) =
                            std::os::unix::net::UnixStream::pair().expect("input");
                        writer.write_all(b"child input").expect("input bytes");
                        drop(writer);
                        let stdout = parent
                            .upcall_with_stdin("probe --uri http://stdin/", kind, &source)
                            .await;
                        let mut child = accept(&listener).await;
                        drop(source);
                        assert_eq!(child.uri, "http://stdin/");
                        assert_eq!(child.descriptors.len(), 2, "child stage has piped stdin");
                        let stdin = child.descriptors.remove(0);
                        let mut stdin = std::os::unix::net::UnixStream::from(stdin);
                        use std::io::Read as _;
                        let mut bytes = String::new();
                        stdin
                            .read_to_string(&mut bytes)
                            .expect("child provider stdin");
                        drop(stdin);
                        child.answer(InvocationOutcome::Succeeded, &bytes).await;
                        let exit = parent.child_exit(stdout).await;
                        parent.answer(InvocationOutcome::Succeeded, "").await;
                        exit
                    });
                    assert_eq!(invoke_in(leg).await, CapabilityCallResult::Succeeded);
                    assert_eq!(
                        broker.await.expect("fake broker"),
                        ("child input\n".to_owned(), 0)
                    );
                }
            }

            struct PanickingJobs;

            impl dekopon_shell::JobControl for PanickingJobs {
                fn start(
                    &self,
                    _seed: dekopon_shell::JobSeed,
                ) -> Result<dekopon_process::JobId, dekopon_shell::JobRefusal> {
                    panic!("child job controller panicked")
                }

                fn list(&self) -> Vec<dekopon_process::JobSummary> {
                    Vec::new()
                }

                fn wait(
                    &self,
                    _ids: &[dekopon_process::JobId],
                    _keep_waiting: &dyn Fn() -> bool,
                ) -> Vec<Result<dekopon_process::JobWait, dekopon_shell::JobRefusal>>
                {
                    Vec::new()
                }

                fn kill(
                    &self,
                    _id: dekopon_process::JobId,
                ) -> Result<(), dekopon_shell::JobRefusal> {
                    Ok(())
                }
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn child_panic_yields_status_70_and_terminal_parent_result() {
                let directory = private_broker_directory();
                let (listener, socket) = listen(directory.path());
                let leg =
                    probe_leg(&socket, None).with_job_control(std::sync::Arc::new(PanickingJobs));
                let broker = tokio::spawn(async move {
                    let mut parent = accept(&listener).await;
                    let stdout = parent.upcall("echo hi &", None).await;
                    let (output, status) = parent.child_exit(stdout).await;
                    parent.answer(InvocationOutcome::Succeeded, "").await;
                    (output, status)
                });
                assert_eq!(invoke_in(leg).await, CapabilityCallResult::Succeeded);
                let (output, status) = broker.await.expect("fake broker");
                assert_eq!(status, 70);
                assert!(output.is_empty(), "panicked child does not claim stdout");
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn idle_child_stdin_and_panicking_job_deliver_status_70() {
                let directory = private_broker_directory();
                let (listener, socket) = listen(directory.path());
                let leg =
                    probe_leg(&socket, None).with_job_control(std::sync::Arc::new(PanickingJobs));
                let broker = tokio::spawn(async move {
                    let mut parent = accept(&listener).await;
                    let (source, _idle_writer) =
                        std::os::unix::net::UnixStream::pair().expect("idle child stdin");
                    let stdout = parent
                        .upcall_with_stdin("echo hi &", UpcallStdin::Reader, &source)
                        .await;
                    let (output, status) = parent.child_exit(stdout).await;
                    assert_eq!(status, 70);
                    parent.answer(InvocationOutcome::Succeeded, "").await;
                    output
                });
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(4), invoke_in(leg))
                        .await
                        .expect("parent terminates after child panic"),
                    CapabilityCallResult::Succeeded
                );
                assert!(broker.await.expect("fake broker").is_empty());
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn dropped_child_stdout_still_receives_a_terminal_status() {
                let directory = private_broker_directory();
                let (listener, socket) = listen(directory.path());
                let leg = probe_leg(&socket, None);
                let broker = tokio::spawn(async move {
                    let mut parent = accept(&listener).await;
                    let stdout = parent.upcall("printf child", None).await;
                    drop(stdout);
                    let (result, descriptors) = parent
                        .stream
                        .read_frame::<RequestEnvelope>(FrameLimits::default())
                        .await
                        .expect("child still completes after its reader drops");
                    assert!(descriptors.is_empty());
                    let BrokerRequest::UpcallResult { status, .. } = result.request else {
                        panic!("expected child status");
                    };
                    parent.answer(InvocationOutcome::Succeeded, "").await;
                    status
                });
                assert_eq!(invoke_in(leg).await, CapabilityCallResult::Succeeded);
                assert_eq!(broker.await.expect("fake broker"), 0);
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn child_trace_and_shared_call_budget() {
                let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
                let dispatch =
                    tracing::Dispatch::new(tracing_subscriber::registry().with(
                        tracing_opentelemetry::layer().with_tracer(provider.tracer("nesting")),
                    ));
                let directory = private_broker_directory();
                let (listener, socket) = listen(directory.path());
                let leg = probe_leg(&socket, None);
                let session = leg.identifiers.parent;
                let upcall_parent =
                    TraceParent::new(session.trace_id(), [7; 8], 1).expect("trace parent");
                let broker = tokio::spawn(async move {
                    let mut parent = accept(&listener).await;
                    let stdout = parent
                        .upcall("probe --uri http://b/", Some(upcall_parent))
                        .await;
                    let child = accept(&listener).await;
                    let child_parent = child.trace_parent;
                    child.answer(InvocationOutcome::Succeeded, "").await;
                    let (_, status) = parent.child_exit(stdout).await;
                    parent.answer(InvocationOutcome::Succeeded, "").await;
                    (child_parent, status)
                });
                let calls = CallBudget::new(1);
                let shared = tree(&calls);
                let result = tokio::task::spawn_blocking(move || {
                    tracing::dispatcher::with_default(&dispatch, || call(&leg, &shared))
                })
                .await
                .expect("blocking dispatch completes");
                assert_eq!(result, CapabilityCallResult::Succeeded);
                let (child_parent, status) = broker.await.expect("fake broker");
                assert_eq!(child_parent.trace(), session.trace());
                assert_ne!(child_parent.parent_id(), session.parent_id());
                assert_eq!((calls.used(), status), (1, 0));
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_denied_invocation_stays_denied_all_the_way_to_the_exit_code() {
            let directory = private_broker_directory();
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::invocation(
                    result(InvocationOutcome::Denied, Some("policy-denied")),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )],
            )
            .await;

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Denied {
                    reason: "policy-denied".to_owned()
                }
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn an_unmapped_peer_is_a_denial_rather_than_an_infrastructure_failure() {
            let directory = private_broker_directory();
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::error(
                    ERROR_UNAUTHENTICATED,
                    "peer is not mapped by broker policy",
                )],
            )
            .await;

            assert!(matches!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Denied { .. }
            ));
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn an_embedder_without_a_store_reports_received_descriptors_instead_of_silently_discarding_metadata()
         {
            use dekopon_broker_protocol::{AssetEncoding, NewAsset};
            use std::{io::Write, os::fd::AsFd};
            let directory = private_broker_directory();
            let socket = directory.path().join("broker.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
            let blob = dekopon_model::asset::DiskBlob::from_bytes(b"PRIVATE_PAYLOAD").unwrap();
            let descriptor = blob.descriptor().unwrap();
            drop(blob);
            let peer = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = DescriptorStream::new(stream);
                let (_request, descriptors) = stream
                    .read_frame::<RequestEnvelope>(FrameLimits::default())
                    .await
                    .unwrap();
                assert_eq!(
                    descriptors.len(),
                    1,
                    "stdout descriptor accompanies invocation"
                );
                let mut provider_stdout =
                    std::os::unix::net::UnixStream::from(descriptors.into_iter().next().unwrap());
                provider_stdout.write_all(b"provider line\n").unwrap();
                let reply = ResponseEnvelope::invocation(
                    result(InvocationOutcome::Succeeded, None),
                    vec![NewAsset {
                        descriptor: 0,
                        content_type: "text/plain".to_owned(),
                        encoding: AssetEncoding::Identity,
                        bytes: 15,
                        sha256: "0".repeat(64),
                    }],
                    vec![],
                    vec![],
                );
                stream
                    .write_frame(&reply, &[descriptor.as_fd()], FrameLimits::default())
                    .await
                    .unwrap();
            });
            let (status, stdout) = invoke_with_stdout(leg_with(&socket, None), CAPABILITY).await;
            peer.await.unwrap();
            assert_eq!(
                status,
                CapabilityCallResult::SucceededWithStderr("[gateway: capability executed but this embedder has no asset store; received asset effects could not be retained or delivered; do not repeat the paid call]\n".to_owned()),
                "effect already completed"
            );
            assert_eq!(stdout, "provider line\n");
            assert!(!stdout.contains("PRIVATE_PAYLOAD"));
            assert!(
                !stdout.contains("\"effect\":\"complete\""),
                "returned value leaked into stdout"
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_failed_invocation_carries_the_broker_reason_without_becoming_a_denial() {
            let directory = private_broker_directory();
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::invocation(
                    result(InvocationOutcome::Failed, Some("provider trapped")),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )],
            )
            .await;

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Failed {
                    error: "provider trapped".to_owned(),
                    detail: None
                }
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_guest_exit_uses_the_typed_status_and_stderr_not_a_parsed_detail_code() {
            let directory = private_broker_directory();
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::invocation(
                    InvocationResult {
                        exit_status: std::num::NonZeroU8::new(7),
                        detail: Some(ProviderFailureDetail::new(
                            "provider-exit",
                            "guest failed\n",
                        )),
                        ..result(InvocationOutcome::Failed, Some("provider-failure"))
                    },
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )],
            )
            .await;
            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Exited {
                    status: std::num::NonZeroU8::new(7).unwrap(),
                    stderr: "guest failed\n".to_owned(),
                }
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_typed_provider_failure_reaches_the_script_with_its_code_and_message() {
            let directory = private_broker_directory();
            let detail = ProviderFailureDetail::new(
                "upstream-rejected",
                "the image route refused the request with HTTP 400 (moderation_blocked)",
            );
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::invocation(
                    InvocationResult {
                        detail: Some(detail.clone()),
                        ..result(InvocationOutcome::Failed, Some("provider-failure"))
                    },
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )],
            )
            .await;

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Failed {
                    error: "provider-failure".to_owned(),
                    detail: Some(detail)
                }
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_successful_invocation_has_no_returned_value() {
            let directory = private_broker_directory();
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::invocation(
                    result(InvocationOutcome::Succeeded, None),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )],
            )
            .await;

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Succeeded
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn successful_guest_stderr_reaches_the_gateway_note() {
            let directory = private_broker_directory();
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::invocation(
                    InvocationResult {
                        detail: Some(ProviderFailureDetail::new(
                            "provider-stderr",
                            "guest note\n",
                        )),
                        ..result(InvocationOutcome::Succeeded, None)
                    },
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )],
            )
            .await;
            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::SucceededWithStderr("guest note\n".to_owned())
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn an_attested_leg_proposes_on_behalf_of_its_subject() {
            let directory = private_broker_directory();
            let (leg, mut observed) = stub_leg_observing(
                directory.path(),
                vec![ResponseEnvelope::invocation(
                    result(InvocationOutcome::Succeeded, None),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )],
                Some(attestation()),
            )
            .await;

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Succeeded
            );

            let request = observed.recv().await.expect("stub broker saw one request");
            let BrokerRequest::Invoke {
                attestation: Some(attestation),
                invocation,
                ..
            } = request.request
            else {
                panic!("an attested leg must send an attested invoke frame: {request:?}");
            };
            assert_eq!(attestation.subject.canonical(), SUBJECT);
            assert_eq!(attestation.agent.as_str(), "chat-agent");
            assert_eq!(attestation.invocation, Some(invocation.id));
            assert_eq!(invocation.capability.as_str(), CAPABILITY);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_refused_attestation_is_a_denial_rather_than_an_infrastructure_failure() {
            let directory = private_broker_directory();
            let (leg, _observed) = stub_leg_observing(
                directory.path(),
                vec![ResponseEnvelope::error(
                    ERROR_UNAUTHENTICATED,
                    "attestation refused: no attestor authority for this subject",
                )],
                Some(attestation()),
            )
            .await;

            assert!(matches!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Denied { .. }
            ));
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_direct_leg_still_proposes_without_any_identity_claim() {
            let directory = private_broker_directory();
            let (leg, mut observed) = stub_leg_observing(
                directory.path(),
                vec![ResponseEnvelope::invocation(
                    result(InvocationOutcome::Succeeded, None),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )],
                None,
            )
            .await;

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Succeeded
            );

            let request = observed.recv().await.expect("stub broker saw one request");
            assert!(
                matches!(request.request, BrokerRequest::Invoke { .. }),
                "{request:?}"
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn capabilities_outside_the_session_never_reach_the_broker() {
            let directory = private_broker_directory();
            let leg = leg_for(&directory.path().join("absent.sock"));

            assert_eq!(
                invoke(leg, "totally.unknown").await,
                CapabilityCallResult::NotFound
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_cancelled_session_is_refused_before_a_proposal_is_built() {
            let directory = private_broker_directory();
            let leg = leg_for(&directory.path().join("absent.sock"));
            let (handle, signal) = CancelSignal::pair();
            let leg = leg.with_cancel_signal(signal);
            handle.cancel();

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Denied {
                    reason: "session-cancelled".to_owned(),
                }
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn the_cancellation_check_never_narrows_what_a_proposal_may_carry() {
            let directory = private_broker_directory();
            let (leg, mut observed) = stub_leg_observing(
                directory.path(),
                vec![ResponseEnvelope::invocation(
                    result(InvocationOutcome::Succeeded, None),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )],
                None,
            )
            .await;
            let leg = leg.with_cancel_signal(CancelSignal::pair().1);
            let proposal = SecretUseProposal::HttpBearer {
                secret: "drn:com.xrl:secret:prod:api/token"
                    .parse::<SecretDrn>()
                    .expect("canonical DRN fixture"),
            };
            let submitted = proposal.clone();

            assert_eq!(
                tokio::task::spawn_blocking(move || {
                    leg.invoke(
                        CommandProposal::new(
                            CAPABILITY,
                            json!({"uri": "http://x/"}),
                            Some(submitted),
                        ),
                        streams().0,
                        &dekopon_process::TreeContext::new(
                            dekopon_process::Limits::default(),
                            dekopon_process::CallBudget::new(64),
                            dekopon_test_support::shell_threads(),
                        ),
                    )
                })
                .await
                .expect("blocking dispatch completes"),
                CapabilityCallResult::Succeeded
            );

            let request = observed.recv().await.expect("stub broker saw one request");
            let BrokerRequest::Invoke { invocation, .. } = request.request else {
                panic!("a capability call sends an invoke frame: {request:?}");
            };
            assert_eq!(invocation.secret_use, Some(proposal));
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn transport_failures_never_disclose_where_the_broker_lives() {
            let directory = private_broker_directory();
            let socket = directory.path().join("dekopon-secret-broker.sock");
            let leg = leg_for(&socket);

            let CapabilityCallResult::Failed { error, .. } = invoke(leg, CAPABILITY).await else {
                panic!("a missing broker socket is an infrastructure failure");
            };
            assert!(!error.contains("dekopon-secret-broker"), "{error}");
            assert!(!error.contains(&socket.display().to_string()), "{error}");
        }

        #[tokio::test]
        async fn invocation_identifiers_are_unique_and_extend_the_session_trace() {
            let identifiers = IdSequence::for_session();
            let first = identifiers.next_invocation();
            let second = identifiers.next_invocation();

            assert_ne!(first, second);
            let trace = identifiers.trace().to_string();
            assert!(first.as_str().starts_with(&trace), "{first} vs {trace}");
            assert!(second.as_str().starts_with(&trace), "{second} vs {trace}");

            let other = IdSequence::for_session();
            assert_ne!(identifiers.trace(), other.trace());
        }

        #[tokio::test]
        async fn a_session_that_exports_nothing_still_carries_one_trace() {
            let span = tracing::info_span!("gateway.session");
            let _entered = span.enter();
            assert!(
                current_trace_parent().is_none(),
                "no exporter is installed in this test process"
            );

            let identifiers = IdSequence::for_session();
            let minted = identifiers.trace_parent();
            assert_eq!(minted.trace(), identifiers.trace());
            assert_eq!(
                minted.to_string(),
                format!(
                    "00-{}-{:016x}-01",
                    minted.trace(),
                    u64::from_be_bytes(minted.parent_id())
                ),
                "the minted context is a well-formed traceparent"
            );
            assert_eq!(minted.flags(), 1, "a minted parent instructs the receiver");
            assert!(
                identifiers
                    .next_invocation()
                    .as_str()
                    .starts_with(&identifiers.trace().to_string())
            );
        }

        fn available(id: &str) -> dekopon_broker_protocol::AvailableCapability {
            serde_json::from_value(json!({
                "provider": "http-probe",
                "capability": {
                    "id": id,
                    "description": "Fetches one broker-authorized URI",
                    "effect": "read-only",
                    "risk": "Low",
                    "inputSchema": {"type": "object"}
                }
            }))
            .expect("capability fixture decodes")
        }

        #[test]
        fn a_duplicated_capability_identifier_is_a_malformed_broker_answer() {
            let error = crate::snapshot(vec![
                available("http-probe.fetch"),
                available("cli-probe.upper"),
                available("http-probe.fetch"),
                available("cli-probe.upper"),
            ])
            .expect_err("a duplicate identifier is refused");

            assert!(
                matches!(
                    &error,
                    crate::BrokerLegError::DuplicateCapabilities { capabilities }
                        if capabilities == "cli-probe.upper, http-probe.fetch"
                ),
                "{error}"
            );
        }

        #[test]
        fn a_distinct_capability_set_indexes_both_views() {
            let (descriptions, effective) = crate::snapshot(vec![
                available("http-probe.fetch"),
                available("cli-probe.upper"),
            ])
            .expect("a distinct set is accepted");

            assert_eq!(descriptions.len(), 2);
            assert_eq!(
                effective
                    .iter()
                    .map(|view| view.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["cli-probe.upper", "http-probe.fetch"]
            );
        }

        #[derive(Default)]
        struct RecordingSink {
            events: Mutex<Vec<ProgressEvent>>,
        }

        impl ProgressSink for RecordingSink {
            fn emit(&self, event: ProgressEvent) {
                self.events.lock().push(event);
            }
        }

        fn recording_sink() -> (Arc<RecordingSink>, Arc<dyn ProgressSink>) {
            let recorder = Arc::new(RecordingSink::default());
            let installed = Arc::clone(&recorder) as Arc<dyn ProgressSink>;
            (recorder, installed)
        }

        impl RecordingSink {
            fn labels(&self) -> Vec<String> {
                self.events.lock().iter().map(label).collect()
            }
        }

        #[expect(
            clippy::wildcard_enum_match_arm,
            reason = "reshaped by the unit that next rewrites this"
        )]
        fn label(event: &ProgressEvent) -> String {
            match event {
                ProgressEvent::ToolStarted {
                    word,
                    argument_count,
                    calls_used,
                    calls_max,
                } => format!(
                    "started {} arguments={argument_count} calls={calls_used}/{calls_max}",
                    word.as_str()
                ),
                ProgressEvent::ToolFinished { word, outcome, .. } => {
                    format!("finished {} {outcome:?}", word.as_str())
                }
                ProgressEvent::Attachment {
                    index,
                    media_type,
                    bytes,
                } => format!("attachment {index} {media_type} {bytes}"),
                ProgressEvent::Note { text, eta } => format!("note {:?} {eta:?}", text.as_str()),
                other => format!("unexpected {other:?}"),
            }
        }

        #[tokio::test]
        async fn notes_without_the_broker_opt_in_emit_nothing() {
            let directory = private_broker_directory();
            let (sink, progress) = recording_sink();
            let leg = leg_for(&directory.path().join("absent.sock"))
                .with_progress(progress, dekopon_process::CallBudget::new(4));
            leg.note("not enabled", None);
            assert!(sink.labels().is_empty());
        }

        #[tokio::test]
        async fn the_ninth_note_is_dropped_and_the_next_script_starts_fresh() {
            let directory = private_broker_directory();
            let (sink, progress) = recording_sink();
            let leg = leg_for(&directory.path().join("absent.sock"))
                .with_progress(progress, dekopon_process::CallBudget::new(4))
                .with_progress_notes();
            let calls = leg.calls.clone();
            let runtime = ShellRuntime {
                invoker: crate::SessionInvoker {
                    direct: super::FakeLeg::new("cli-probe.upper", "direct"),
                    broker: Some(Box::new(leg)),
                },
                limits: Limits::default(),
                calls,
                threads: dekopon_test_support::shell_threads(),
            };
            let script = (1..=9)
                .map(|index| format!("progress note-{index}"))
                .collect::<Vec<_>>()
                .join("; ");
            tokio::task::spawn_blocking(move || {
                assert_eq!(
                    runtime.run_script(&script, None).exit_code,
                    ExitCode::SUCCESS
                );
                assert_eq!(
                    runtime.run_script("progress fresh", None).exit_code,
                    ExitCode::SUCCESS
                );
            })
            .await
            .expect("script thread completes");
            let mut expected = (1..=8)
                .map(|index| format!("note \"note-{index}\" None"))
                .collect::<Vec<_>>();
            expected.push("note \"fresh\" None".to_owned());
            assert_eq!(sink.labels(), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_capability_call_is_reported_started_and_finished_with_the_brokers_answer() {
            let directory = private_broker_directory();
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::invocation(
                    result(
                        InvocationOutcome::Denied,
                        Some("authorization refused this invocation"),
                    ),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )],
            )
            .await;
            let (sink, progress) = recording_sink();
            let leg = leg.with_progress(progress, dekopon_process::CallBudget::new(4));

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Denied {
                    reason: "authorization refused this invocation".to_owned(),
                }
            );

            assert_eq!(
                sink.labels(),
                vec![
                    format!("started {CAPABILITY} arguments=1 calls=0/4"),
                    format!("finished {CAPABILITY} Denied"),
                ]
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_call_proposed_after_a_stop_reads_as_cancelled_rather_than_refused() {
            let directory = private_broker_directory();
            let leg = leg_for(&directory.path().join("absent.sock"));
            let (handle, signal) = CancelSignal::pair();
            let (sink, progress) = recording_sink();
            let leg = leg
                .with_cancel_signal(signal)
                .with_progress(progress, dekopon_process::CallBudget::new(4));
            handle.cancel();

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Denied {
                    reason: "session-cancelled".to_owned(),
                }
            );
            assert_eq!(
                sink.labels(),
                vec![
                    format!("started {CAPABILITY} arguments=1 calls=0/4"),
                    format!("finished {CAPABILITY} Cancelled"),
                ]
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_broker_stop_interrupts_a_running_shell_loop() {
            let directory = private_broker_directory();
            let (handle, signal) = CancelSignal::pair();
            let leg = leg_for(&directory.path().join("absent.sock")).with_cancel_signal(signal);
            let limits = Limits {
                max_steps: u64::MAX,
                timeout: Duration::from_secs(300),
                ..Limits::default()
            };
            let runtime = ShellRuntime {
                invoker: leg,
                limits,
                calls: dekopon_process::CallBudget::new(4),
                threads: dekopon_test_support::shell_threads(),
            };
            let running = tokio::task::spawn_blocking(move || {
                runtime.run_script("while true; do :; done", None)
            });
            tokio::time::sleep(Duration::from_millis(200)).await;
            handle.cancel();
            let outcome = tokio::time::timeout(Duration::from_secs(2), running)
                .await
                .expect("script stopped promptly")
                .expect("script worker");
            assert_eq!(outcome.exit_code, ExitCode::CANCELLED);
            assert!(outcome.steps < u64::MAX);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_broker_stop_interrupts_a_sleeping_shell() {
            let directory = private_broker_directory();
            let (handle, signal) = CancelSignal::pair();
            let leg = leg_for(&directory.path().join("absent.sock")).with_cancel_signal(signal);
            let limits = Limits {
                timeout: Duration::from_secs(310),
                ..Limits::default()
            };
            let runtime = ShellRuntime {
                invoker: leg,
                limits,
                calls: dekopon_process::CallBudget::new(4),
                threads: dekopon_test_support::shell_threads(),
            };
            let running =
                tokio::task::spawn_blocking(move || runtime.run_script("sleep 300", None));
            tokio::time::sleep(Duration::from_millis(200)).await;
            handle.cancel();
            let outcome = tokio::time::timeout(Duration::from_millis(1500), running)
                .await
                .expect("sleep stopped promptly")
                .expect("script worker");
            assert_eq!(outcome.exit_code, ExitCode::CANCELLED);
        }

        async fn reporting_probe_leg(
            directory: &Path,
            responses: Vec<ResponseEnvelope>,
        ) -> (BrokerLeg, Arc<RecordingSink>) {
            let (mut leg, _observed) = stub_leg_observing(directory, responses, None).await;
            leg.command_words.insert("probe".to_owned());
            let (sink, progress) = recording_sink();
            (
                leg.with_progress(progress, dekopon_process::CallBudget::new(4)),
                sink,
            )
        }

        fn proposal_of(capability: &str) -> ResponseEnvelope {
            ResponseEnvelope::command_run(dekopon_broker_protocol::CommandRunOutcome::Proposed {
                capability: capability.parse().expect("valid capability fixture"),
                input: json!({"number": 7}),
                secret_use: None,
            })
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn progress_reads_the_same_call_count_across_scripts() {
            let directory = private_broker_directory();
            let denied = || {
                ResponseEnvelope::invocation(
                    result(InvocationOutcome::Denied, Some("policy-denied")),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )
            };
            let (leg, sink) = reporting_probe_leg(
                directory.path(),
                vec![
                    proposal_of(CAPABILITY),
                    denied(),
                    proposal_of(CAPABILITY),
                    denied(),
                ],
            )
            .await;
            let calls = leg.calls.clone();
            let runtime = ShellRuntime {
                invoker: leg,
                limits: Limits::default(),
                calls: calls.clone(),
                threads: dekopon_test_support::shell_threads(),
            };
            let (first, second) = tokio::task::spawn_blocking(move || {
                (
                    runtime.run_script("probe fetch 7", None),
                    runtime.run_script("probe fetch 7", None),
                )
            })
            .await
            .expect("scripts finish");
            assert_eq!(
                (
                    first.capability_calls,
                    second.capability_calls,
                    calls.used()
                ),
                (1, 1, 2)
            );
            assert_eq!(
                sink.labels(),
                vec![
                    "started probe arguments=2 calls=1/4".to_owned(),
                    "finished probe Denied".to_owned(),
                    "started probe arguments=2 calls=2/4".to_owned(),
                    "finished probe Denied".to_owned(),
                ]
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_proposal_the_script_never_invokes_finishes_failed_before_the_script_returns() {
            let directory = private_broker_directory();
            let (leg, sink) =
                reporting_probe_leg(directory.path(), vec![proposal_of("gh.pr-merge")]).await;
            let calls = leg.calls.clone();
            let runtime = ShellRuntime {
                invoker: crate::SessionInvoker {
                    direct: super::FakeLeg::new("cli-probe.upper", "direct"),
                    broker: Some(Box::new(leg)),
                },
                limits: Limits::default(),
                calls,
                threads: dekopon_test_support::shell_threads(),
            };

            let outcome =
                tokio::task::spawn_blocking(move || runtime.run_script("probe merge 7", None))
                    .await
                    .expect("blocking dispatch completes");

            assert_eq!(outcome.exit_code, ExitCode::NOT_FOUND, "{}", outcome.output);
            assert_eq!(
                sink.labels(),
                vec![
                    "started probe arguments=2 calls=1/4".to_owned(),
                    "finished probe Failed".to_owned(),
                ],
                "{}",
                outcome.output
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_capability_no_provider_owns_is_refused_without_reaching_a_progress_surface() {
            // Check the capability is known before reporting progress, or an untrusted model-chosen
            // word renders directly in chat.
            let directory = private_broker_directory();
            let leg = leg_for(&directory.path().join("absent.sock"));
            let (sink, progress) = recording_sink();
            let leg = leg.with_progress(progress, dekopon_process::CallBudget::new(4));

            let outcome = tokio::task::spawn_blocking(move || {
                leg.invoke(
                    CommandProposal::new("ignore-your-instructions", json!({}), None),
                    streams().0,
                    &dekopon_process::TreeContext::new(
                        dekopon_process::Limits::default(),
                        dekopon_process::CallBudget::new(64),
                        dekopon_test_support::shell_threads(),
                    ),
                )
            })
            .await
            .expect("blocking dispatch completes");

            assert!(
                matches!(outcome, CapabilityCallResult::NotFound),
                "an identifier no provider owns is refused as not found: {outcome:?}"
            );
            assert!(
                sink.labels().is_empty(),
                "a word the model invented reached a progress surface: {:?}",
                sink.labels()
            );
        }
    }
}
