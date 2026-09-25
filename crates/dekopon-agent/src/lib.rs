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
use std::time::Duration;

#[cfg(unix)]
use std::{
    collections::hash_map::RandomState,
    collections::{BTreeMap, BTreeSet},
    hash::{BuildHasher as _, Hasher as _},
    sync::atomic::{AtomicU32, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use dekopon_broker_protocol::TraceParent;
#[cfg(unix)]
use dekopon_broker_protocol::{
    Attestation, BrokerClient, ChatMemorySurface, ClientError, CommandRunOutcome,
    ERROR_UNAUTHENTICATED, InvocationOutcome, InvocationRequest,
};
#[cfg(unix)]
use dekopon_core::{CapabilityId, InvocationId, TraceId};
use dekopon_process::ProcessOutcome;
#[cfg(unix)]
use dekopon_process::{CancelSignal, ProcessMetadata, ProcessRun, process_fn};
#[cfg(unix)]
use dekopon_shell::CapabilityDescription;
use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, CommandRun, Interpreter, Limits as ShellLimits,
    ScriptOutcome,
};
use serde_json::Value;
#[cfg(unix)]
use std::sync::{Arc, Mutex, PoisonError};
#[cfg(unix)]
use thiserror::Error;

#[cfg(unix)]
use crate::attachment::{ChatAssetInputs, ChatAssetRefusal, ReplyAttachments};
use crate::{meta::EffectiveCapabilityView, prompt::ScriptRuntime};

pub mod attachment;
pub mod improvement;
pub mod meta;
pub mod progress;
pub mod prompt;
pub mod skills;
pub mod wake;

pub use crate::progress::{
    BudgetLimit, CancelSource, CancelVia, CommandWord, FailureClass, ProgressEvent, ProgressSink,
    SessionOutcome, ToolOutcome,
};

pub struct ShellRuntime<I> {
    pub invoker: I,
    pub limits: ShellLimits,
}

impl<I: CapabilityInvoker> ScriptRuntime for ShellRuntime<I> {
    fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome {
        // Each script gets a fresh interpreter but not a fresh budget; capability allowance is
        // spent across the whole session, and exhausting it trips the interpreter's own existing
        // ceiling.
        let limits = ShellLimits {
            max_capability_calls: self.limits.max_capability_calls.min(max_capability_calls),
            ..self.limits
        };
        let outcome = Interpreter::new(limits).run(script, &self.invoker);
        // Call script_finished before returning the outcome, or the next turn's progress event
        // reports out of order.
        self.invoker.script_finished();
        outcome
    }

    fn command_words(&self) -> Vec<String> {
        self.invoker.command_words()
    }
}

/// Direct leg runs first because it is unauthorized by construction (its linker is import-free, so
/// it cannot reach anything); the broker leg is only for what direct mode provably cannot do: I/O.
pub struct SessionInvoker<D> {
    pub direct: D,
    pub broker: Option<Box<dyn CapabilityInvoker + Send>>,
}

impl<D: CapabilityInvoker> CapabilityInvoker for SessionInvoker<D> {
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
        capability: &str,
        input: Value,
        secret_use: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        // A secret-use proposal must reach only the broker leg; the direct leg has no authorizer to
        // check it.
        if secret_use.is_some() {
            return match &self.broker {
                Some(broker) if broker.is_granted(capability) => {
                    broker.invoke(capability, input, secret_use)
                }
                _ => dekopon_shell::secret_use_unsupported(),
            };
        }
        if self.direct.is_granted(capability) {
            return self.direct.invoke(capability, input, None);
        }
        match &self.broker {
            Some(broker) => broker.invoke(capability, input, None),
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

    fn run_command(&self, word: &str, argv: &[String], stdin: Option<&str>) -> Option<CommandRun> {
        self.direct
            .run_command(word, argv, stdin)
            .or_else(|| self.broker.as_ref()?.run_command(word, argv, stdin))
    }

    fn script_finished(&self) {
        self.direct.script_finished();
        if let Some(broker) = &self.broker {
            broker.script_finished();
        }
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
        tracing::warn!(event = "session_trace_entropy_unavailable", error = %error);
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

#[must_use]
pub fn command_run_from_outcome(outcome: CommandRunOutcome) -> CommandRun {
    match outcome {
        CommandRunOutcome::Proposed {
            capability,
            input,
            secret_use,
        } => CommandRun::Proposed {
            capability: capability.to_string(),
            input,
            secret_use,
        },
        CommandRunOutcome::Rendered {
            stdout,
            stderr,
            status,
        } => CommandRun::Rendered {
            stdout,
            stderr,
            status,
        },
        CommandRunOutcome::Failed { error } => CommandRun::Failed {
            message: error.message,
        },
    }
}

#[cfg(unix)]
#[derive(Debug, Error)]
pub enum BrokerLegError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("the broker answered with duplicate capability identifiers: {capabilities}")]
    DuplicateCapabilities { capabilities: String },
}

/// A client of brokerd's authorization path, never a participant: it only submits proposals and
/// reports back the broker's decision; an attested leg's claimed subject is still not authority.
#[cfg(unix)]
pub struct BrokerLeg {
    client: BrokerClient,
    runtime: tokio::runtime::Handle,
    capabilities: BTreeMap<String, CapabilityDescription>,
    effective_capabilities: Vec<EffectiveCapabilityView>,
    command_words: BTreeSet<String>,
    identifiers: IdSequence,
    attestation: Option<Attestation>,
    chat_memory: Option<ChatMemorySurface>,
    cancel: CancelSignal,
    attachments: Option<Arc<ReplyAttachments>>,
    asset_inputs: Option<ChatAssetInputs>,
    progress: Option<Arc<dyn ProgressSink>>,
    calls_max: u32,
    calls_used: AtomicU32,
    pending_report: Mutex<Option<(CommandWord, Instant)>>,
}

#[cfg(unix)]
impl BrokerLeg {
    pub async fn connect(
        client: BrokerClient,
        attestation: Option<Attestation>,
    ) -> Result<Self, BrokerLegError> {
        let (capabilities, command_words, chat_memory) =
            client.session_surface(attestation.clone()).await?;
        Self::build(
            client,
            capabilities,
            command_words,
            attestation,
            chat_memory,
        )
    }

    fn build(
        client: BrokerClient,
        available: Vec<dekopon_broker_protocol::AvailableCapability>,
        command_words: Vec<String>,
        attestation: Option<Attestation>,
        chat_memory: Option<ChatMemorySurface>,
    ) -> Result<Self, BrokerLegError> {
        let (capabilities, effective_capabilities) = snapshot(available)?;
        Ok(Self {
            client,
            runtime: tokio::runtime::Handle::current(),
            capabilities,
            effective_capabilities,
            command_words: command_words.into_iter().collect(),
            identifiers: IdSequence::for_session(),
            attestation,
            chat_memory,
            cancel: CancelSignal::never(),
            attachments: None,
            asset_inputs: None,
            progress: None,
            calls_max: 0,
            calls_used: AtomicU32::new(0),
            pending_report: Mutex::new(None),
        })
    }

    #[must_use]
    pub fn with_cancel_signal(mut self, signal: CancelSignal) -> Self {
        self.cancel = signal;
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
    pub fn with_progress(mut self, sink: Arc<dyn ProgressSink>, max_capability_calls: u32) -> Self {
        self.progress = Some(sink);
        self.calls_max = max_capability_calls;
        self
    }

    fn emit(&self, event: ProgressEvent) {
        if let Some(sink) = &self.progress {
            sink.emit(event);
        }
    }

    fn take_pending_report(&self) -> Option<(CommandWord, Instant)> {
        // Nothing inside this lock may panic, since even a poisoned mutex is recovered and trusted
        // here.
        self.pending_report
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn settle_pending_report(&self) {
        if let Some((word, started)) = self.take_pending_report() {
            self.emit(ProgressEvent::ToolFinished {
                word,
                outcome: ToolOutcome::Failed,
                duration: started.elapsed(),
            });
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
    pub const fn session_trace(&self) -> TraceId {
        self.identifiers.trace()
    }

    fn prepare_assets(
        &self,
        input: &Value,
    ) -> Result<
        (
            dekopon_broker_protocol::InvokeAssets,
            Vec<dekopon_model::asset::DiskBlob>,
        ),
        ChatAssetRefusal,
    > {
        let remaining = self.attachments.as_ref().map_or(0, |slot| slot.remaining());
        let prepared = match &self.asset_inputs {
            Some(inputs) => inputs.prepare(input, remaining),
            None => attachment::references(input).and_then(|ids| {
                if ids.is_empty() {
                    Ok((
                        dekopon_broker_protocol::InvokeAssets {
                            rows: Vec::new(),
                            descriptors: Vec::new(),
                            sends_remaining: remaining,
                        },
                        Vec::new(),
                    ))
                } else {
                    Err(ChatAssetRefusal::UnknownAsset)
                }
            }),
        };
        prepared.inspect_err(|refusal| {
            tracing::warn!(target: "dekopon_agent::audit", { audit.event = "agent.chat_asset_input.refused", reason = refusal.reason() }, "chat asset input refused");
        })
    }
}

#[cfg(unix)]
fn bounded_count(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

#[cfg(unix)]
fn call_outcome(result: &CapabilityCallResult) -> ToolOutcome {
    match result {
        CapabilityCallResult::Succeeded(_) => ToolOutcome::Succeeded,
        CapabilityCallResult::Denied { .. } => ToolOutcome::Denied,
        CapabilityCallResult::Failed { .. } | CapabilityCallResult::NotFound => ToolOutcome::Failed,
    }
}

/// A duplicate capability identifier is refused, naming every repeat at once, rather than silently
/// kept as a last-wins entry, matching the broker's own refusal of duplicate routes at startup.
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

    fn has_command_word(&self, word: &str) -> bool {
        self.command_words.contains(word)
    }

    fn run_command(&self, word: &str, argv: &[String], stdin: Option<&str>) -> Option<CommandRun> {
        self.settle_pending_report();
        if !self.command_words.contains(word) {
            return None;
        }
        let reported = CommandWord::new(word);
        let started = Instant::now();
        self.emit(ProgressEvent::ToolStarted {
            word: reported.clone(),
            argument_count: bounded_count(argv.len()),
            calls_used: self.calls_used.load(Ordering::Relaxed),
            calls_max: self.calls_max,
        });
        // The round-trip task must be joined before returning, or the leg could answer while an
        // aborted call is still in flight.
        let client = self.client.clone();
        let attestation = self.attestation.clone();
        let (owned_word, argv, stdin) = (word.to_owned(), argv.to_vec(), stdin.map(str::to_owned));
        let trace_parent = self.identifiers.trace_parent();
        let operation = process_fn(
            ProcessMetadata::cancellable("broker-command", self.cancel.clone()),
            move || async move {
                client
                    .run_command(attestation, owned_word, argv, stdin, trace_parent)
                    .await
                    .map(command_run_from_outcome)
            },
        );
        let outcome = self.runtime.block_on(ProcessRun::execute(operation));
        let run = match outcome {
            ProcessOutcome::Completed(Ok(run)) => run,
            ProcessOutcome::Completed(Err(error)) => CommandRun::Errored {
                message: dekopon_core::error_chain(&error),
            },
            ProcessOutcome::TaskFailed(error) if error.is_cancelled() => CommandRun::Denied {
                reason: "session-cancelled".to_owned(),
            },
            ProcessOutcome::TaskFailed(error) => CommandRun::Errored {
                message: error.to_string(),
            },
        };
        let outcome = match &run {
            CommandRun::Proposed { .. } => {
                *self
                    .pending_report
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some((reported, started));
                return Some(run);
            }
            CommandRun::Rendered { status: 0, .. } => ToolOutcome::Succeeded,
            CommandRun::Rendered { .. }
            | CommandRun::Failed { .. }
            | CommandRun::Errored { .. } => ToolOutcome::Failed,
            CommandRun::Denied { .. } => ToolOutcome::Cancelled,
        };
        self.emit(ProgressEvent::ToolFinished {
            word: reported,
            outcome,
            duration: started.elapsed(),
        });
        Some(run)
    }

    fn invoke(
        &self,
        capability: &str,
        input: Value,
        secret_use: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        let held = self.take_pending_report();
        let cancelled = self.cancel.is_cancelled();
        let calls_used = if cancelled {
            self.calls_used.load(Ordering::Relaxed)
        } else {
            self.calls_used
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1)
        };
        // Checks the capability exists before reporting it on the progress surface, since reporting
        // an unvalidated identifier first would let a model put invented text on a person's chat
        // line.
        let reported = held.or_else(|| {
            self.capabilities.contains_key(capability).then(|| {
                let word = CommandWord::new(capability);
                self.emit(ProgressEvent::ToolStarted {
                    word: word.clone(),
                    argument_count: bounded_count(
                        input.as_object().map_or(0, serde_json::Map::len),
                    ),
                    calls_used,
                    calls_max: self.calls_max,
                });
                (word, Instant::now())
            })
        });
        let (result, outcome) = if cancelled {
            (
                CapabilityCallResult::Denied {
                    reason: "session-cancelled".to_owned(),
                },
                ToolOutcome::Cancelled,
            )
        } else {
            let result = self.submit(capability, input, secret_use);
            let outcome = call_outcome(&result);
            (result, outcome)
        };
        if let Some((word, started)) = reported {
            self.emit(ProgressEvent::ToolFinished {
                word,
                outcome,
                duration: started.elapsed(),
            });
        }
        result
    }

    fn script_finished(&self) {
        self.settle_pending_report();
    }
}

#[cfg(unix)]
impl BrokerLeg {
    fn submit(
        &self,
        capability: &str,
        input: Value,
        secret_use: Option<dekopon_core::SecretUseProposal>,
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
        let (assets, asset_pins) = match self.prepare_assets(&input) {
            Ok(input) => input,
            // This refusal is permanent, the interpreter's non-retryable Denied rather than a
            // retryable Failed, and produces no broker audit record since no proposal was ever
            // submitted.
            Err(refusal) => {
                return CapabilityCallResult::Denied {
                    reason: format!(
                        "the gateway refused this call before it reached the broker: {}",
                        refusal.note()
                    ),
                };
            }
        };
        let request = InvocationRequest {
            id: self.identifiers.next_invocation(),
            capability: parsed,
            trace_parent: self.identifiers.trace_parent(),
            secret_use,
            input,
        };

        // Safe only because this runs on a spawn_blocking thread; calling block_on here from an
        // ordinary runtime worker would deadlock the executor.
        let invocation = request.id.to_string();
        let submitted = self.runtime.block_on(async {
            self.client
                .invoke(self.attestation.clone(), request, assets)
                .await
        });
        drop(asset_pins);
        match submitted {
            Ok(outcome) => {
                let result = outcome.result;
                match result.outcome {
                    InvocationOutcome::Succeeded => {
                        let mut output = result.output.unwrap_or(Value::Null);
                        if output
                            .get("attachments")
                            .and_then(Value::as_array)
                            .is_some_and(|attachments| {
                                attachments
                                    .iter()
                                    .any(|attachment| attachment.get("base64").is_some())
                            })
                        {
                            return CapabilityCallResult::Denied {
                                reason: format!(
                                    "{capability} returned retired result attachments; migrate this provider to dekopon:asset; the capability already executed"
                                ),
                            };
                        }
                        if let Some(slot) = &self.attachments {
                            let note = slot.receive(
                                outcome.attached,
                                outcome.descriptors,
                                outcome.removed,
                                outcome.sent,
                                capability,
                                &invocation,
                            );
                            if !note.is_empty() {
                                output = serde_json::json!({"result": output, "assetNote": note});
                            }
                        } else if !outcome.attached.is_empty()
                            || !outcome.removed.is_empty()
                            || !outcome.sent.is_empty()
                        {
                            output = serde_json::json!({"result": output, "assetNote": "[gateway: capability executed but this embedder has no asset store; received asset effects could not be retained or delivered; do not repeat the paid call]"});
                        }
                        CapabilityCallResult::Succeeded(output)
                    }
                    InvocationOutcome::Denied => CapabilityCallResult::Denied {
                        reason: result
                            .error
                            .unwrap_or_else(|| "authorization refused this invocation".to_owned()),
                    },
                    InvocationOutcome::Failed => CapabilityCallResult::Failed {
                        error: result.error.unwrap_or_else(|| {
                            "the broker reported a failed invocation".to_owned()
                        }),
                        detail: result.detail,
                    },
                }
            }
            Err(ClientError::Remote { code, message }) if code == ERROR_UNAUTHENTICATED => {
                CapabilityCallResult::Denied { reason: message }
            }
            // A client-side timeout cannot tell whether the call ran, so it is treated as the one
            // non-retryable Denied status with an explicit refusal to resubmit, rather than a
            // Failed a model would retry.
            Err(error) if error.may_have_executed() => CapabilityCallResult::Denied {
                reason: format!(
                    "the broker did not record an outcome for this invocation and it may already \
                     have taken effect; do not resubmit it ({error})"
                ),
            },
            // Every ClientError renders without the socket path, since this is the one path that
            // could otherwise leak DEKOPON_BROKER_SOCKET back into a script.
            Err(error) => CapabilityCallResult::Failed {
                error: error.to_string(),
                detail: None,
            },
        }
    }
}

#[cfg(unix)]
pub struct IdSequence {
    parent: TraceParent,
    next: AtomicU32,
}

#[cfg(unix)]
impl IdSequence {
    #[must_use]
    pub fn for_session() -> Self {
        Self {
            parent: session_trace_parent(),
            next: AtomicU32::new(1),
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
    use dekopon_shell::{CapabilityCallResult, CapabilityDescription, CapabilityInvoker};
    use serde_json::{Value, json};

    use super::{SessionInvoker, current_trace_parent};

    #[test]
    fn trace_parent_is_absent_without_an_active_exporting_span() {
        assert!(current_trace_parent().is_none());
    }

    struct FakeLeg {
        capability: &'static str,
        marker: &'static str,
        invoked: std::sync::Mutex<Vec<String>>,
        secret_uses: std::sync::Mutex<Vec<Option<dekopon_core::SecretUseProposal>>>,
    }

    impl FakeLeg {
        fn new(capability: &'static str, marker: &'static str) -> Self {
            Self {
                capability,
                marker,
                invoked: std::sync::Mutex::new(Vec::new()),
                secret_uses: std::sync::Mutex::new(Vec::new()),
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
            capability: &str,
            _input: Value,
            secret_use: Option<dekopon_core::SecretUseProposal>,
        ) -> CapabilityCallResult {
            if capability != self.capability {
                return CapabilityCallResult::NotFound;
            }
            self.invoked
                .lock()
                .expect("invocation lock")
                .push(capability.to_owned());
            self.secret_uses
                .lock()
                .expect("invocation lock")
                .push(secret_use);
            CapabilityCallResult::Succeeded(json!({ "leg": self.marker }))
        }
    }

    #[test]
    fn direct_capabilities_are_preferred_over_the_broker() {
        let shared = Box::new(FakeLeg::new("shared.capability", "broker"));
        let invoker = SessionInvoker {
            direct: FakeLeg::new("shared.capability", "direct"),
            broker: Some(shared),
        };

        assert_eq!(
            invoker.invoke("shared.capability", json!({}), None),
            CapabilityCallResult::Succeeded(json!({"leg": "direct"}))
        );
    }

    #[test]
    fn capabilities_absent_from_direct_mode_fall_through_to_the_broker() {
        let invoker = SessionInvoker {
            direct: FakeLeg::new("cli-probe.upper", "direct"),
            broker: Some(Box::new(FakeLeg::new("http-probe.fetch", "broker"))),
        };

        assert_eq!(
            invoker.invoke("http-probe.fetch", json!({}), None),
            CapabilityCallResult::Succeeded(json!({"leg": "broker"}))
        );
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
            invoker.invoke("http-probe.fetch", json!({}), None),
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
            invoker.invoke("http-probe.fetch", json!({}), Some(proposal.clone())),
            CapabilityCallResult::Succeeded(json!({"leg": "broker"}))
        );

        assert_eq!(
            invoker.invoke("cli-probe.upper", json!({}), Some(proposal)),
            dekopon_shell::secret_use_unsupported()
        );
        assert!(
            invoker
                .direct
                .secret_uses
                .lock()
                .expect("invocation lock")
                .is_empty(),
            "the direct leg was handed a proposal it cannot authorize"
        );
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
            _capability: &str,
            _input: Value,
            _secret_use: Option<dekopon_core::SecretUseProposal>,
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
        use std::{
            collections::{BTreeMap, BTreeSet},
            os::unix::fs::PermissionsExt as _,
            path::Path,
            sync::{Arc, Mutex, atomic::AtomicU32},
        };

        use dekopon_broker_protocol::{
            BrokerClient, BrokerRequest, CommandRunOutcome, ERROR_UNAUTHENTICATED, FrameLimits,
            InvocationOutcome, InvocationResult, RequestEnvelope, ResponseEnvelope, read_frame,
            write_frame,
        };
        use dekopon_capability::DecisionReference;
        use dekopon_core::{
            AgentId, ExternalSubject, ProviderFailureDetail, SecretDrn, SecretUseProposal,
        };
        use dekopon_process::CancelSignal;
        use dekopon_shell::{
            CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandRun, ExitCode,
            Limits,
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
                    policy_revision: "policy-stub".to_owned(),
                },
                outcome,
                output: matches!(outcome, InvocationOutcome::Succeeded)
                    .then(|| json!({"status": 200})),
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
                for response in responses {
                    let (mut stream, _) = listener.accept().await.expect("stub broker accepts");
                    let request =
                        read_frame::<_, RequestEnvelope>(&mut stream, FrameLimits::default())
                            .await
                            .expect("stub broker reads one request");
                    #[allow(
                        clippy::let_underscore_must_use,
                        reason = "`stub_leg` drops the observation receiver immediately, so a \
                                  closed channel is the ordinary case for every unobserved test"
                    )]
                    let _ = observed.send(request);
                    write_frame(&mut stream, &response, FrameLimits::default())
                        .await
                        .expect("stub broker writes one response");
                }
            });

            (leg_with(&socket, attestation), receiver)
        }

        fn leg_for(socket: &Path) -> BrokerLeg {
            leg_with(socket, None)
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
                let (mut stream, _) = listener.accept().await.expect("stub broker accepts");
                let request = read_frame::<_, RequestEnvelope>(&mut stream, FrameLimits::default())
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

        async fn run_word(
            leg: BrokerLeg,
            argv: &'static [&'static str],
            stdin: Option<&'static str>,
        ) -> Option<CommandRun> {
            tokio::task::spawn_blocking(move || {
                let argv = argv
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect::<Vec<_>>();
                leg.run_command("probe", &argv, stdin)
            })
            .await
            .expect("blocking dispatch completes")
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_rendered_run_reaches_the_script_with_its_status() {
            let directory = private_broker_directory();
            let rendered = CommandRunOutcome::Rendered {
                stdout: "Usage: probe <COMMAND>\n".to_owned(),
                stderr: String::new(),
                status: 0,
            };
            let (mut leg, mut observed) = stub_leg_observing(
                directory.path(),
                vec![ResponseEnvelope::command_run(rendered)],
                None,
            )
            .await;
            leg.command_words.insert("probe".to_owned());
            let trace_parent = leg.identifiers.trace_parent();

            assert_eq!(
                run_word(leg, &["--help"], None).await,
                Some(CommandRun::Rendered {
                    stdout: "Usage: probe <COMMAND>\n".to_owned(),
                    stderr: String::new(),
                    status: 0,
                })
            );
            let request = observed.recv().await.expect("stub broker saw the run");
            assert_eq!(
                request.request,
                BrokerRequest::RunCommand {
                    attestation: None,
                    word: "probe".to_owned(),
                    argv: vec!["--help".to_owned()],
                    stdin: None,
                    trace_parent,
                }
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn the_piped_value_travels_in_the_run_frame() {
            let directory = private_broker_directory();
            let proposed = CommandRunOutcome::Proposed {
                capability: "cli-probe.upper".parse().expect("valid capability fixture"),
                input: json!({"text": "hello"}),
                secret_use: None,
            };
            let (mut leg, mut observed) = stub_leg_observing(
                directory.path(),
                vec![ResponseEnvelope::command_run(proposed)],
                None,
            )
            .await;
            leg.command_words.insert("probe".to_owned());
            let trace_parent = leg.identifiers.trace_parent();

            assert_eq!(
                run_word(leg, &["upper", "-"], Some("hello")).await,
                Some(CommandRun::Proposed {
                    capability: "cli-probe.upper".to_owned(),
                    input: json!({"text": "hello"}),
                    secret_use: None,
                })
            );
            let request = observed.recv().await.expect("stub broker saw the run");
            assert!(
                matches!(
                    &request.request,
                    BrokerRequest::RunCommand {
                        stdin: Some(piped),
                        trace_parent: sent,
                        ..
                    } if piped == "hello" && *sent == trace_parent
                ),
                "{request:?}"
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_proposed_secret_use_survives_the_broker_leg() {
            let directory = private_broker_directory();
            let secret_use = SecretUseProposal::HttpBasic {
                secret: "drn:com.xrl:secret:prod:api/token"
                    .parse::<SecretDrn>()
                    .expect("canonical DRN fixture"),
                username: "deploy".to_owned(),
            };
            let proposed = CommandRunOutcome::Proposed {
                capability: CAPABILITY.parse().expect("valid capability fixture"),
                input: json!({"uri": "https://example.test/"}),
                secret_use: Some(secret_use.clone()),
            };
            let (mut leg, _observed) = stub_leg_observing(
                directory.path(),
                vec![ResponseEnvelope::command_run(proposed)],
                None,
            )
            .await;
            leg.command_words.insert("probe".to_owned());

            assert_eq!(
                run_word(leg, &["fetch", "https://example.test/"], None).await,
                Some(CommandRun::Proposed {
                    capability: CAPABILITY.to_owned(),
                    input: json!({"uri": "https://example.test/"}),
                    secret_use: Some(secret_use),
                })
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_cancel_signal_abandons_an_in_flight_run() {
            let directory = private_broker_directory();
            let (mut leg, mut observed, release) = stub_leg_parked(directory.path()).await;
            leg.command_words.insert("probe".to_owned());
            let (handle, signal) = CancelSignal::pair();
            let leg = leg.with_cancel_signal(signal);

            let run = tokio::task::spawn_blocking(move || {
                leg.run_command("probe", &["--help".to_owned()], None)
            });
            observed.recv().await.expect("the run reached the broker");
            handle.cancel();

            assert_eq!(
                run.await.expect("blocking dispatch completes"),
                Some(CommandRun::Denied {
                    reason: "session-cancelled".to_owned(),
                })
            );
            drop(release);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_transport_failure_names_its_cause_and_never_the_socket() {
            let directory = private_broker_directory();
            let socket = directory.path().join("dekopon-secret-broker.sock");
            let mut leg = leg_for(&socket);
            leg.command_words.insert("probe".to_owned());

            let Some(CommandRun::Errored { message }) = run_word(leg, &["--help"], None).await
            else {
                panic!("a missing broker socket is an infrastructure failure, not a decline");
            };
            assert!(
                message.starts_with("could not inspect broker socket: "),
                "{message}"
            );
            assert!(!message.contains("dekopon-secret-broker"), "{message}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_word_no_provider_owns_never_reaches_the_broker() {
            let directory = private_broker_directory();
            let leg = leg_for(&directory.path().join("absent.sock"));

            assert_eq!(run_word(leg, &["--help"], None).await, None);
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
                identifiers: IdSequence::for_session(),
                attestation,
                chat_memory: None,
                cancel: CancelSignal::never(),
                attachments: None,
                asset_inputs: None,
                progress: None,
                calls_max: 0,
                calls_used: AtomicU32::new(0),
                pending_report: Mutex::new(None),
            }
        }

        async fn invoke(leg: BrokerLeg, capability: &'static str) -> CapabilityCallResult {
            tokio::task::spawn_blocking(move || {
                leg.invoke(capability, json!({"uri": "http://x/"}), None)
            })
            .await
            .expect("blocking dispatch completes")
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
        async fn ordinary_attachment_metadata_remains_successful_provider_json() {
            for output in [
                json!({"subject": "report", "attachments": [{"name": "a.pdf"}]}),
                json!({"attachments": []}),
                json!({"attachments": {"count": 1}}),
            ] {
                let directory = private_broker_directory();
                let mut result = result(InvocationOutcome::Succeeded, None);
                result.output = Some(output.clone());
                let leg = stub_leg(
                    directory.path(),
                    vec![ResponseEnvelope::invocation(result, vec![], vec![], vec![])],
                )
                .await;
                assert_eq!(
                    invoke(leg, CAPABILITY).await,
                    CapabilityCallResult::Succeeded(output)
                );
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_retired_base64_attachment_envelope_is_refused_after_execution() {
            let directory = private_broker_directory();
            let mut result = result(InvocationOutcome::Succeeded, None);
            result.output =
                Some(json!({"attachments": [{"mediaType": "image/png", "base64": "cG5n"}]}));
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::invocation(result, vec![], vec![], vec![])],
            )
            .await;
            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Denied {
                    reason: format!(
                        "{CAPABILITY} returned retired result attachments; migrate this provider to dekopon:asset; the capability already executed"
                    ),
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
            use dekopon_broker_protocol::{AssetEncoding, DescriptorStream, NewAsset};
            use std::os::fd::AsFd;
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
                assert!(descriptors.is_empty());
                let mut result = result(InvocationOutcome::Succeeded, None);
                result.output = Some(json!({"effect":"complete"}));
                let reply = ResponseEnvelope::invocation(
                    result,
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
            let result = invoke(leg_with(&socket, None), CAPABILITY).await;
            peer.await.unwrap();
            let CapabilityCallResult::Succeeded(output) = result else {
                panic!("effect already completed")
            };
            assert_eq!(output["result"]["effect"], "complete");
            assert!(
                output["assetNote"]
                    .as_str()
                    .unwrap()
                    .contains("no asset store")
            );
            assert!(!output.to_string().contains("PRIVATE_PAYLOAD"));
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
        async fn a_successful_invocation_hands_provider_output_to_the_script() {
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
                CapabilityCallResult::Succeeded(json!({"status": 200}))
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
                CapabilityCallResult::Succeeded(json!({"status": 200}))
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
                CapabilityCallResult::Succeeded(json!({"status": 200}))
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
                    leg.invoke(CAPABILITY, json!({"uri": "http://x/"}), Some(submitted))
                })
                .await
                .expect("blocking dispatch completes"),
                CapabilityCallResult::Succeeded(json!({"status": 200}))
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
                self.events.lock().expect("progress lock").push(event);
            }
        }

        fn recording_sink() -> (Arc<RecordingSink>, Arc<dyn ProgressSink>) {
            let recorder = Arc::new(RecordingSink::default());
            let installed = Arc::clone(&recorder) as Arc<dyn ProgressSink>;
            (recorder, installed)
        }

        impl RecordingSink {
            fn labels(&self) -> Vec<String> {
                self.events
                    .lock()
                    .expect("progress lock")
                    .iter()
                    .map(label)
                    .collect()
            }
        }

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
                other => format!("unexpected {other:?}"),
            }
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
            let leg = leg.with_progress(progress, 4);

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Denied {
                    reason: "authorization refused this invocation".to_owned(),
                }
            );

            assert_eq!(
                sink.labels(),
                vec![
                    format!("started {CAPABILITY} arguments=1 calls=1/4"),
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
            let leg = leg.with_cancel_signal(signal).with_progress(progress, 4);
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

        async fn reporting_probe_leg(
            directory: &Path,
            responses: Vec<ResponseEnvelope>,
        ) -> (BrokerLeg, Arc<RecordingSink>) {
            let (mut leg, _observed) = stub_leg_observing(directory, responses, None).await;
            leg.command_words.insert("probe".to_owned());
            let (sink, progress) = recording_sink();
            (leg.with_progress(progress, 4), sink)
        }

        fn proposal_of(capability: &str) -> ResponseEnvelope {
            ResponseEnvelope::command_run(CommandRunOutcome::Proposed {
                capability: capability.parse().expect("valid capability fixture"),
                input: json!({"number": 7}),
                secret_use: None,
            })
        }

        fn rendered_at(status: u8) -> ResponseEnvelope {
            ResponseEnvelope::command_run(CommandRunOutcome::Rendered {
                stdout: String::new(),
                stderr: String::new(),
                status,
            })
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_word_that_proposes_nothing_is_one_pair_under_the_word_with_its_own_outcome() {
            let directory = private_broker_directory();
            let (leg, sink) =
                reporting_probe_leg(directory.path(), vec![rendered_at(0), rendered_at(2)]).await;

            let (help, usage_error) = tokio::task::spawn_blocking(move || {
                let help = leg.run_command("probe", &["--help".to_owned()], None);
                let usage_error = leg.run_command("probe", &["--nonsense".to_owned()], None);
                leg.script_finished();
                (help, usage_error)
            })
            .await
            .expect("blocking dispatch completes");

            assert!(
                matches!(help, Some(CommandRun::Rendered { status: 0, .. })),
                "{help:?}"
            );
            assert!(
                matches!(usage_error, Some(CommandRun::Rendered { status: 2, .. })),
                "{usage_error:?}"
            );
            assert_eq!(
                sink.labels(),
                vec![
                    "started probe arguments=1 calls=0/4".to_owned(),
                    "finished probe Succeeded".to_owned(),
                    "started probe arguments=1 calls=0/4".to_owned(),
                    "finished probe Failed".to_owned(),
                ]
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_proposing_word_is_one_pair_under_the_word_finished_by_the_call_it_proposed() {
            let directory = private_broker_directory();
            let (leg, sink) = reporting_probe_leg(
                directory.path(),
                vec![
                    proposal_of(CAPABILITY),
                    ResponseEnvelope::invocation(
                        result(InvocationOutcome::Denied, Some("policy-denied")),
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                    ),
                ],
            )
            .await;

            let called = tokio::task::spawn_blocking(move || {
                let argv = ["fetch".to_owned(), "7".to_owned()];
                let Some(CommandRun::Proposed {
                    capability,
                    input,
                    secret_use,
                }) = leg.run_command("probe", &argv, None)
                else {
                    panic!("the stub broker answers the word with a proposal");
                };
                let called = leg.invoke(&capability, input, secret_use);
                leg.script_finished();
                called
            })
            .await
            .expect("blocking dispatch completes");

            assert_eq!(
                called,
                CapabilityCallResult::Denied {
                    reason: "policy-denied".to_owned(),
                }
            );
            assert_eq!(
                sink.labels(),
                vec![
                    "started probe arguments=2 calls=0/4".to_owned(),
                    "finished probe Denied".to_owned(),
                ]
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_proposal_the_script_never_invokes_finishes_failed_before_the_script_returns() {
            let directory = private_broker_directory();
            let (leg, sink) =
                reporting_probe_leg(directory.path(), vec![proposal_of("gh.pr-merge")]).await;
            let runtime = ShellRuntime {
                invoker: crate::SessionInvoker {
                    direct: super::FakeLeg::new("cli-probe.upper", "direct"),
                    broker: Some(Box::new(leg)),
                },
                limits: Limits::default(),
            };

            let outcome =
                tokio::task::spawn_blocking(move || runtime.run_script("probe merge 7", 4))
                    .await
                    .expect("blocking dispatch completes");

            assert_eq!(outcome.exit_code, ExitCode::NOT_FOUND, "{}", outcome.output);
            assert_eq!(
                sink.labels(),
                vec![
                    "started probe arguments=2 calls=0/4".to_owned(),
                    "finished probe Failed".to_owned(),
                ],
                "{}",
                outcome.output
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_proposal_left_uninvoked_finishes_failed_before_the_next_word_starts() {
            let directory = private_broker_directory();
            let (leg, sink) = reporting_probe_leg(
                directory.path(),
                vec![proposal_of("gh.pr-merge"), rendered_at(0)],
            )
            .await;

            let (merge, help) = tokio::task::spawn_blocking(move || {
                let merge = leg.run_command("probe", &["merge".to_owned(), "7".to_owned()], None);
                let help = leg.run_command("probe", &["--help".to_owned()], None);
                leg.script_finished();
                (merge, help)
            })
            .await
            .expect("blocking dispatch completes");

            assert!(
                matches!(merge, Some(CommandRun::Proposed { .. })),
                "{merge:?}"
            );
            assert!(
                matches!(help, Some(CommandRun::Rendered { status: 0, .. })),
                "{help:?}"
            );
            assert_eq!(
                sink.labels(),
                vec![
                    "started probe arguments=2 calls=0/4".to_owned(),
                    "finished probe Failed".to_owned(),
                    "started probe arguments=1 calls=0/4".to_owned(),
                    "finished probe Succeeded".to_owned(),
                ]
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_capability_no_provider_owns_is_refused_without_reaching_a_progress_surface() {
            // Check the capability is known before reporting progress, or an untrusted model-chosen
            // word renders directly in chat.
            let directory = private_broker_directory();
            let leg = leg_for(&directory.path().join("absent.sock"));
            let (sink, progress) = recording_sink();
            let leg = leg.with_progress(progress, 4);

            let outcome = tokio::task::spawn_blocking(move || {
                leg.invoke("ignore-your-instructions", json!({}), None)
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
