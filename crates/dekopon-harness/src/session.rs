//! Concrete bounded model/tool session engine, with no broker authority.

use crate::control::{
    self, ActiveModel, ModelIdentity, SessionControls, TransitionOutcome, TransitionRequest,
};
use crate::{
    bootstrap::{BootstrapError, CapabilitySnapshot, SessionBootstrap},
    history::{History, JobRecord},
    improvement::{self, IMPROVEMENT_TOOL_NAME, ImprovementSuggestion},
    meta::AgentConfigView,
    runtime::ScriptRuntime,
    skills::{self, SKILL_TOOL_NAME, SkillReads},
    tools::*,
};
use crate::{
    checkpoint::{
        Checkpoint, CheckpointError, CheckpointStore, ExecutionJournal, Position,
        memory_checkpoints,
    },
    history::{DeliveryDisposition, ToolGroup},
};
use dekopon_config::Skill;
use dekopon_model::model::{
    ChatModel, CompletionOptions, ModelError, ModelMessage, ModelToolCall, assistant_message,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

/// Request-scoped cooperative cancellation visible from the synchronous prompt loop.
///
/// Cancellation is not rollback: a model request or provider effect already accepted elsewhere may
/// still finish. The probe prevents the next model turn or tool invocation from starting and lets
/// an embedding gateway suppress a stale terminal answer.
pub trait CancellationProbe: Send + Sync {
    /// Whether the session should stop at its next cooperative boundary.
    fn is_cancelled(&self) -> bool;
}

/// Bounds on one prompt session.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct PromptLimits {
    /// Maximum model turns, including the turn that produces the final answer.
    pub max_steps: u32,
    /// Capability invocations the whole session may drive, summed across every script.
    pub max_capability_calls: u32,
}

/// Whether a completed prompt session should publish its final text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyDisposition {
    /// Publish the non-empty final answer normally.
    Send,
    /// Publish nothing because an optional chat continuation explicitly declined.
    Suppress,
}

/// Result of a completed prompt/tool session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionExit {
    /// Opaque logical job coordinate, not a provider call ID or authority token.
    pub job: String,
    /// Final assistant text. Empty only when [`Self::disposition`] is
    /// [`ReplyDisposition::Suppress`].
    pub answer: String,
    /// Whether the embedding surface should deliver `answer`.
    pub disposition: ReplyDisposition,
    /// Number of model requests made.
    pub model_turns: u32,
    /// Number of scripts the model ran.
    pub script_calls: u32,
    /// Capability invocations those scripts drove.
    pub capability_invocations: u32,
    /// Improvement suggestions the model recorded, when the embedder offered the tool.
    ///
    /// Already written to telemetry by the time they arrive here; this copy is for an embedder
    /// that wants to show them to the operator directly, the way the one-shot runner prints them.
    pub suggestions: Vec<ImprovementSuggestion>,
}

/// Optional, request-scoped surfaces handed to the inner model loop.
#[derive(Clone, Copy)]
struct SessionExtensions<'a> {
    controls: Option<&'a SessionControls<'a>>,
    system: Option<&'a str>,
    context_policy: Option<&'a dyn crate::context::ContextPolicy>,
    capabilities: &'a CapabilitySnapshot,
    assets: Option<&'a dyn AssetSource>,
    image_generation: Option<ImageGeneration<'a>>,
    agent_config: Option<&'a AgentConfigView>,
    cancellation: Option<&'a dyn CancellationProbe>,
    optional_reply: bool,
    skills: &'a [Skill],
    improvement_suggestions: bool,
}

/// Concrete synchronous session driver, run by the host on its blocking executor.
///
/// The host keeps model clients, authenticated ingress, cancellation, and reply delivery. This
/// engine owns request-one context, tool dispatch and session-wide work bounds; it grants nothing.
pub struct SessionEngine<'a, M: ?Sized, R: ?Sized> {
    model: &'a M,
    runtime: &'a R,
    checkpoints: Arc<dyn CheckpointStore>,
}

/// Monotonic work already spent by this logical job, including failed attempts.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct SpentBudgets {
    pub model_calls: u32,
    pub script_calls: u32,
    pub capability_invocations: u32,
    pub asset_fetches: u32,
    pub control_attempts: u32,
}

/// Portable loop state. No client, credential, grant or opaque provider continuation is stored.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct SessionState {
    pub spent: SpentBudgets,
    pub agent_config_shown: bool,
    pub image_generation_attempted: bool,
    pub skill_reads: SkillReads,
    pub suggestions: Vec<ImprovementSuggestion>,
    pub current_tool: String,
    pub current_model: Option<ModelIdentity>,
    pub control_baseline: Option<ModelIdentity>,
    pub control_scope: Option<dekopon_broker_protocol::ControlScope>,
    pub control_fenced: bool,
    pub transitions: Vec<control::TransitionRecord>,
    pub accounting: crate::accounting::TokenTracker,
}

impl<'a, M: ChatModel + ?Sized, R: ScriptRuntime + ?Sized> SessionEngine<'a, M, R> {
    /// Borrows a selected model client and the request's scoped, unprivileged runtime.
    pub fn new(model: &'a M, runtime: &'a R) -> Self {
        Self {
            model,
            runtime,
            checkpoints: memory_checkpoints(),
        }
    }

    /// Supply bounded storage; every engine, including runner and replay, consumes checkpoints.
    pub fn with_checkpoint_store(mut self, store: Arc<dyn CheckpointStore>) -> Self {
        self.checkpoints = store;
        self
    }

    /// Runs one bounded session, recording its exchange even when inference or tool parsing fails.
    /// Bootstrap refusal and zero-step requests record nothing: neither reached inference.
    pub fn run(
        &self,
        inputs: SessionBootstrap<'_>,
        history: &mut History,
    ) -> Result<SessionExit, PromptError> {
        let SessionBootstrap {
            activity,
            scope,
            surface_epoch,
            resume,
            capabilities: prebuilt_capabilities,
            controls,
            context_policy,
            prompt,
            selected_model,
            system,
            limits,
            options,
            assets,
            image_generation,
            accounting,
            model_identity,
            agent_config,
            cancellation,
            optional_reply,
            skills,
            improvement_suggestions,
        } = inputs;
        let fallback = CompletionOptions::default();
        let options = options.unwrap_or(&fallback);
        if limits.max_steps == 0 {
            // Nothing is recorded here: a zero-step session builds no request, so the prompt never
            // reached a model and the conversation must not claim otherwise.
            return Err(PromptError::ZeroSteps);
        }

        if prompt.len() > 128 * 1024 || limits.max_steps > 128 {
            return Err(CheckpointError::Capacity.into());
        }
        // The host may hand over the snapshot it already built for this message from the same
        // scoped runtime; building it twice per message is the same bounded projection twice.
        let capabilities = match prebuilt_capabilities {
            Some(capabilities) => capabilities.clone(),
            None => self.runtime.capability_snapshot()?,
        };
        if let Some(controls) = controls
            && (surface_epoch != Some(controls.epoch())
                || resume.is_some_and(|job| job != controls.job()))
        {
            return Err(control::ControlError::Configuration.into());
        }
        let prepared = controls
            .map(|c| c.prepare(c.baseline()))
            .transpose()
            .map_err(control::ControlError::from)?;
        let identity = prepared.as_ref().map_or_else(
            || {
                model_identity.unwrap_or_else(|| {
                    let (backend, model) = self.model.model_identity();
                    ModelIdentity {
                        configured: None,
                        backend: backend.to_owned(),
                        model: if model == "unreported" {
                            selected_model
                        } else {
                            model
                        }
                        .to_owned(),
                        effort: options.effort(),
                    }
                })
            },
            |p| p.identity.clone(),
        );
        let mut active = ActiveModel {
            options: options.clone().with_effort(identity.effort),
            identity,
            prepared,
        };
        if let Some(prepared) = &active.prepared {
            prepared.client.validate_options(&active.options)?;
        } else {
            self.model.validate_options(&active.options)?;
        }
        let bootstrap = capabilities.prompt_block(&active.identity.model)?;

        // Order matters and is fixed here rather than left to callers: instructions first, then the
        // standing skills listing, then what the conversation remembers, then what the operator just
        // said. The listing sits with the instructions because it is agent-standing rather than
        // request-scoped, which keeps a route's cached prompt prefix stable across sessions.
        let mut messages = Vec::new();
        if let Some(system) = system {
            messages.push(ModelMessage::system(system));
        }
        if let Some(listing) = skills::prompt_block(skills) {
            messages.push(ModelMessage::system(listing));
        }
        messages.push(ModelMessage::system(bootstrap));
        if active.identity.configured.is_some()
            || active.identity.effort != dekopon_core::Effort::ProviderDefault
        {
            messages.push(model_identity_context(&active.identity));
        }
        if optional_reply {
            messages.push(ModelMessage::system(OPTIONAL_REPLY_INSTRUCTION));
        }
        let default_policy = crate::context::WindowContext;
        let surface = match surface_epoch {
            Some(epoch) => {
                crate::history::digest(format!("{}:{epoch}", capabilities.fingerprint()).as_bytes())
            }
            None => capabilities.fingerprint(),
        };
        let scope = scope.unwrap_or("direct");
        let checkpoint = match resume {
            Some(job) => {
                let mut saved = self.checkpoints.load(job)?;
                saved.validate_resume(scope, &surface)?;
                if saved.limits != limits
                    || saved.state.control_scope.as_ref() != controls.map(SessionControls::scope)
                    || saved.state.control_baseline != controls.map(|_| active.identity.clone())
                    || (controls.is_none()
                        && (saved.model != selected_model
                            || saved.effort != active.identity.effort.to_string()))
                {
                    return Err(CheckpointError::ScopeChanged.into());
                }
                // Fresh runtime has no binary assets or opaque continuation. Repeated-read pointers
                // cannot point at text an excerpt/trim omitted.
                saved.state.skill_reads = SkillReads::default();
                saved.state.agent_config_shown = false;
                saved.context_revision = saved
                    .context_revision
                    .checked_add(1)
                    .ok_or(CheckpointError::Capacity)?;
                messages.extend(
                    context_policy
                        .unwrap_or(&default_policy)
                        .select(&saved.history),
                );
                crate::context::replay_job(&saved.record, &mut messages);
                saved
            }
            None => {
                messages.extend(context_policy.unwrap_or(&default_policy).select(history));
                messages.push(ModelMessage::user(prompt));
                Checkpoint {
                    version: crate::checkpoint::CHECKPOINT_VERSION,
                    revision: 0,
                    position: Position::Ready,
                    scope: scope.to_owned(),
                    surface,
                    model: active.identity.model.clone(),
                    effort: active.identity.effort.to_string(),
                    context_revision: 0,
                    record: JobRecord::new(
                        controls.map_or_else(crate::checkpoint::opaque_id, |c| c.job().to_owned()),
                        prompt,
                    ),
                    history: history.checkpoint_seed(),
                    limits,
                    state: SessionState {
                        current_model: Some(active.identity.clone()),
                        control_baseline: controls.map(|_| active.identity.clone()),
                        control_scope: controls.map(|c| c.scope().clone()),
                        ..SessionState::default()
                    },
                    pending_execution: None,
                    finalized: false,
                }
            }
        };
        let mut checkpoint = checkpoint;
        if resume.is_none() {
            checkpoint.state.accounting.job = checkpoint.record.job.clone();
        }
        let activity = activity
            .map(|(p, labels)| p.bind(checkpoint.record.job.clone(), labels, &capabilities));
        let mut state = checkpoint.state.clone();
        let journal = ExecutionJournal::new(
            self.checkpoints.clone(),
            checkpoint,
            resume.is_none(),
            accounting,
        )?
        .with_cancellation(cancellation)
        .with_activity(activity);
        let job_span = journal.accounting.span();
        let _job_entered = job_span.enter();
        let mut result = self.run_session(
            &mut messages,
            limits,
            SessionExtensions {
                controls,
                system,
                context_policy,
                capabilities: &capabilities,
                assets,
                image_generation,
                agent_config,
                cancellation,
                optional_reply,
                skills,
                improvement_suggestions,
            },
            &mut state,
            &journal,
            &mut active,
        );
        journal.accounting.generation(
            match &result {
                Ok(_) => crate::accounting::CallOutcome::Succeeded,
                Err(PromptError::Cancelled) => crate::accounting::CallOutcome::Cancelled,
                Err(_) => crate::accounting::CallOutcome::Failed,
            },
            result
                .as_ref()
                .err()
                .map_or("completed", PromptError::telemetry_kind),
        );
        let persisted = journal.update(|c| {
            state.spent.capability_invocations = state
                .spent
                .capability_invocations
                .max(c.state.spent.capability_invocations);
            state.image_generation_attempted |= c.state.image_generation_attempted;
            if let Some(model) = &state.current_model {
                c.model = model.model.clone();
                c.effort = model.effort.to_string();
            }
            c.state = state;
            if let Some(group) = c.record.groups.last_mut() {
                group.capture_results(&messages);
            }
            if let Ok(outcome) = &result {
                if outcome.disposition == ReplyDisposition::Send {
                    c.record.generated = Some(outcome.answer.clone());
                } else {
                    c.record.delivery = DeliveryDisposition::Suppressed;
                }
            }
            if matches!(result, Err(PromptError::Cancelled)) {
                c.record.delivery = DeliveryDisposition::Cancelled;
            }
            c.position = Position::GenerationFinished;
        });
        // Failure/Stop/persistence errors never erase observations. A fenced store copy cannot be resumed.
        history.record(journal.snapshot().record);
        if let Err(source) = persisted {
            journal
                .accounting
                .generation(crate::accounting::CallOutcome::Failed, "checkpoint");
            result = Err(PromptError::Interrupted {
                source,
                checkpoint: Box::new(journal.snapshot()),
            });
        }
        result
    }

    fn run_session(
        &self,
        messages: &mut Vec<ModelMessage>,
        limits: PromptLimits,
        extensions: SessionExtensions<'_>,
        state: &mut SessionState,
        journal: &ExecutionJournal,
        active: &mut ActiveModel,
    ) -> Result<SessionExit, PromptError> {
        let runtime = self.runtime;
        let SessionExtensions {
            controls,
            system,
            context_policy,
            capabilities,
            assets,
            image_generation,
            agent_config,
            cancellation,
            optional_reply,
            skills,
            improvement_suggestions,
        } = extensions;
        if let Some(saved) = state.current_model.clone()
            && saved != active.identity
        {
            let outcome = control::transition(
                controls,
                TransitionRequest {
                    selection: saved.selection().ok_or(TransitionOutcome::Disabled),
                    refusal: None,
                    requesting_call: None,
                    assets_present: false,
                },
                active,
                state,
                journal,
                cancellation,
            )?;
            control::save_boundary(state, journal, messages)?;
            if outcome != TransitionOutcome::Applied {
                state.control_fenced = true;
                return Err(control::ControlError::Configuration.into());
            }
            rebuild_context(messages, journal, active, &extensions)?;
            control::save_boundary(state, journal, messages)?;
        }
        if journal.snapshot().record.generated.is_some() {
            let saved = journal.snapshot();
            return Ok(SessionExit {
                job: saved.record.job,
                answer: saved.record.generated.expect("checked generated"),
                disposition: ReplyDisposition::Send,
                model_turns: state.spent.model_calls,
                script_calls: state.spent.script_calls,
                capability_invocations: state.spent.capability_invocations,
                suggestions: state.suggestions.clone(),
            });
        }
        // `system` and `context_policy` remain in extensions for atomic context rebuilds.
        let _ = (system, context_policy);
        // Offered only when this conversation actually carries something. A tool that can only fail is
        // a tool a model will still call, and every unusable tool costs prompt tokens on every turn.
        let assets = assets.filter(|source| !source.is_empty());
        let mut base_tools = vec![script_tool(capabilities.command_words())];
        if agent_config.is_some() {
            base_tools.push(agent_config_tool());
        }
        if !skills.is_empty() {
            base_tools.push(skills::skill_tool());
        }
        if assets.is_some() {
            base_tools.push(asset_tool());
        }
        if image_generation.is_some() {
            base_tools.push(image_generation_tool());
        }
        if improvement_suggestions {
            base_tools.push(improvement::improvement_tool());
        }
        if optional_reply {
            base_tools.push(decline_reply_tool());
        }

        let session_span = tracing::info_span!(
            "prompt.session",
            prompt.max_steps = limits.max_steps,
            prompt.max_capability_calls = limits.max_capability_calls
        );
        let _session = session_span.enter();
        // Number of already exported messages; later transcript events carry only appended items.
        let mut transcribed = 0_usize;
        let mut opaque_bytes = 0_usize;

        for model_turns in state.spent.model_calls + 1..=limits.max_steps {
            check_cancelled(cancellation)?;
            check_freshness(runtime, journal)?;
            if journal.snapshot().record.has_unknown_work()
                || journal.snapshot().history.has_unknown_work()
            {
                return Err(CheckpointError::UnknownWork.into());
            }
            if crate::context::bound_live(messages)? {
                state.skill_reads = SkillReads::default();
                state.agent_config_shown = false;
                // Rebuild portable messages after any incompatible trim; opaque replay never survives.
                let snapshot = journal.snapshot();
                messages.retain(|m| m.role() == "system");
                crate::context::replay_job(&snapshot.record, messages);
                crate::context::bound_live(messages)?;
                transcribed = 0;
                opaque_bytes = 0;
                journal.update(|c| c.context_revision += 1)?;
            }
            state.spent.model_calls = model_turns;
            // Reserve the logical call before any checkpoint or transmission can fail.
            let call_sequence = journal.accounting.reserve(
                active.identity.clone(),
                crate::accounting::CallKind::Chat,
                model_turns,
            )?;
            journal.update(|c| {
                c.state = state.clone();
                c.position = Position::ModelPending;
                if let Some(group) = c.record.groups.last_mut() {
                    group.capture_results(messages);
                }
            })?;
            let recorder = crate::accounting::CallRecorder::reserved(journal, call_sequence);
            let model_span = recorder.span();
            let model_entered = model_span.enter();
            // Verbatim transcript rides the log stream rather than span attributes: a conversation is
            // unbounded text, span attributes are the wrong container for it, and the log stream is
            // what a backend indexes for full-text search. Both carry the same trace and span IDs, so
            // a log result still pivots to the turn it belongs to.
            //
            // Within an untrimmed context revision, only the first turn ships the whole list.
            // A rebuild resets the transcript cursor. Otherwise turn N's message vector contains
            // turn N-1's, so re-shipping it every turn would cost a session O(N^2) payload bytes to
            // repeat what this turn's `agent.model.answer`, `agent.tool.script`, and
            // `agent.tool.output` already said. Later turns log the messages appended since the
            // previous one, so the events of a session still concatenate back into the exact request.
            if dekopon_core::telemetry_payloads() {
                let scope = if transcribed == 0 { "full" } else { "delta" };
                tracing::info!(
                    target: "dekopon_harness::audit",
                    {
                        audit.event = "agent.model.prompt",
                        model.turn = model_turns,
                        transcript.version = 2_u32,
                        context.revision = journal.snapshot().context_revision,
                        job.id = %journal.snapshot().record.job,
                        transcript.scope = scope,
                        message.count = messages.len(),
                        messages = %transcript(&messages[transcribed..]),
                    },
                    "model turn prompt"
                );
                transcribed = messages.len();
            }
            let mut model_tools = base_tools.clone();
            if let Some(controls) = controls {
                model_tools.extend(
                    controls.tools(
                        &active.identity,
                        active
                            .prepared
                            .as_ref()
                            .expect("controls prepare baseline")
                            .client
                            .as_ref(),
                        state.spent.control_attempts,
                    ),
                );
            }
            let completion = match &active.prepared {
                Some(prepared) => prepared.client.complete_with(
                    messages,
                    &model_tools,
                    &active.options,
                    &recorder,
                ),
                None => {
                    self.model
                        .complete_with(messages, &model_tools, &active.options, &recorder)
                }
            };
            let cancelled = cancellation.is_some_and(CancellationProbe::is_cancelled);
            let call_outcome = if cancelled {
                crate::accounting::CallOutcome::Cancelled
            } else if completion.is_ok() {
                crate::accounting::CallOutcome::Succeeded
            } else {
                crate::accounting::CallOutcome::Failed
            };
            recorder.finish(
                call_outcome,
                if cancelled {
                    "cancelled"
                } else if completion.is_ok() {
                    "completed"
                } else {
                    "model-error"
                },
                completion
                    .as_ref()
                    .is_ok_and(|turn| turn.content.as_ref().is_some_and(|s| !s.trim().is_empty())),
            )?;
            state.accounting = journal.accounting.snapshot();
            let turn = completion?;
            journal.update(|c| {
                c.state = state.clone();
                c.position = Position::Tools;
            })?;
            // Usage is already retained. Fence before exporting or acting on generated content.
            check_freshness(runtime, journal)?;
            if dekopon_core::telemetry_payloads() {
                tracing::info!(
                    target: "dekopon_harness::audit",
                    {
                        audit.event = "agent.model.answer",
                        model.turn = model_turns,
                        answer = turn.content.as_deref().unwrap_or_default(),
                        tool_calls = %tool_calls_json(&turn.tool_calls),
                    },
                    "model turn answer"
                );
            }
            drop(model_entered);
            drop(recorder);
            drop(model_span);
            if turn.tool_calls.is_empty() {
                // Only text that would actually be sent is recorded as generated. Storing the raw
                // content here and rejecting whitespace-only content a few lines below left the
                // checkpoint claiming an answer the session then refused to deliver, so a resumed
                // job — and the conversation this turn is appended to — reported a blank answer.
                let generated = turn
                    .content
                    .clone()
                    .filter(|content| !content.trim().is_empty());
                journal.update(|c| c.record.generated = generated)?;
            }
            check_cancelled(cancellation)?;
            opaque_bytes = opaque_bytes
                .checked_add(
                    serde_json::to_vec(&turn.replay_items)
                        .expect("opaque items serialize")
                        .len(),
                )
                .ok_or(CheckpointError::Capacity)?;
            if opaque_bytes > crate::context::MAX_GROUP_BYTES {
                return Err(CheckpointError::Capacity.into());
            }
            messages.push(assistant_message(&turn));

            if turn.tool_calls.is_empty() {
                check_cancelled(cancellation)?;
                let answer = turn
                    .content
                    .filter(|content| !content.trim().is_empty())
                    .ok_or(PromptError::EmptyAnswer)?;
                return Ok(SessionExit {
                    job: journal.snapshot().record.job,
                    answer,
                    disposition: ReplyDisposition::Send,
                    model_turns,
                    script_calls: state.spent.script_calls,
                    capability_invocations: state.spent.capability_invocations,
                    suggestions: state.suggestions.clone(),
                });
            }
            if turn.tool_calls.len() > MAX_TOOL_CALLS_PER_TURN {
                tracing::error!(
                    target: "dekopon_harness::audit",
                    {
                        audit.event = "agent.tool.rejected",
                        model.turn = model_turns,
                        tool_call.count = turn.tool_calls.len(),
                        error.type = "too-many-tool-calls",
                    },
                    "model tool calls rejected"
                );
                return Err(PromptError::TooManyToolCalls {
                    actual: turn.tool_calls.len(),
                    maximum: MAX_TOOL_CALLS_PER_TURN,
                });
            }

            let mut ids = std::collections::BTreeSet::new();
            if turn
                .tool_calls
                .iter()
                .any(|c| c.id.trim().is_empty() || c.id.len() > 256 || !ids.insert(c.id.clone()))
            {
                return Err(PromptError::EmptyToolCallId);
            }
            let mut portable_calls = turn.tool_calls.clone();
            for call in &mut portable_calls {
                if call.function.name == IMAGE_GENERATION_TOOL_NAME {
                    call.function.arguments = "{}".to_owned();
                }
            }
            let oversized = serde_json::to_vec(&portable_calls)
                .expect("calls serialize")
                .len()
                > crate::context::MAX_GROUP_BYTES;
            if oversized {
                return Err(CheckpointError::Capacity.into());
            }
            journal.update(|c| {
                c.record.groups.push(ToolGroup {
                    call: call_sequence,
                    calls: portable_calls,
                    results: Vec::new(),
                    omitted: false,
                    provenance: None,
                })
            })?;

            let decline_requested = optional_reply
                && turn
                    .tool_calls
                    .iter()
                    .any(|call| call.function.name == DECLINE_REPLY_TOOL_NAME);
            if decline_requested {
                for call in turn
                    .tool_calls
                    .iter()
                    .filter(|call| control::is_control(call))
                {
                    control::transition(
                        controls,
                        TransitionRequest {
                            selection: control::parse(call, &active.identity),
                            refusal: Some(TransitionOutcome::BatchRefused),
                            requesting_call: Some(model_turns),
                            assets_present: assets.is_some(),
                        },
                        active,
                        state,
                        journal,
                        cancellation,
                    )?;
                }
                // A terminal decline does not need tool results, but malformed correlation IDs and
                // arguments are still malformed model output rather than a magic escape hatch.
                for (index, call) in turn.tool_calls.iter().enumerate() {
                    if call.id.trim().is_empty() {
                        reject_tool_call(model_turns, index + 1, "empty-tool-call-id");
                        return Err(PromptError::EmptyToolCallId);
                    }
                    if call.function.name == DECLINE_REPLY_TOOL_NAME {
                        decline_reply_argument(&call.function.name, &call.function.arguments)?;
                    }
                }
                if state.spent.capability_invocations == 0 {
                    check_cancelled(cancellation)?;
                    tracing::info!(
                        target: "dekopon_harness::audit",
                        {
                            audit.event = "agent.reply.declined",
                            model.turn = model_turns,
                        },
                        "optional chat reply declined"
                    );
                    return Ok(SessionExit {
                        job: journal.snapshot().record.job,
                        answer: String::new(),
                        disposition: ReplyDisposition::Suppress,
                        model_turns,
                        script_calls: state.spent.script_calls,
                        capability_invocations: state.spent.capability_invocations,
                        suggestions: state.suggestions.clone(),
                    });
                }

                // Once a capability ran, silence could conceal an external effect. If no model turn
                // remains, return a distinct error so the embedding surface can post a fixed warning
                // not to retry blindly. Otherwise answer every call in this turn without running any
                // of them, then require the model to report what the earlier work did.
                if model_turns == limits.max_steps {
                    return Err(PromptError::UnreportedCapabilityWork);
                }
                for call in &turn.tool_calls {
                    messages.push(ModelMessage::tool(
                        call.id.clone(),
                        DECLINE_AFTER_WORK_RESULT.to_owned(),
                    ));
                }
                continue;
            }

            if turn.tool_calls.iter().any(control::is_control) {
                // Preflight the ENTIRE batch: no script/meta tool can run beside a control,
                // including forged controls in direct/replay mode. Decline precedence is above.
                let mixed = turn.tool_calls.len() != 1;
                for call in &turn.tool_calls {
                    let outcome = if control::is_control(call) {
                        control::transition(
                            controls,
                            TransitionRequest {
                                selection: control::parse(call, &active.identity),
                                refusal: mixed.then_some(TransitionOutcome::BatchRefused),
                                requesting_call: Some(model_turns),
                                assets_present: assets.is_some(),
                            },
                            active,
                            state,
                            journal,
                            cancellation,
                        )?
                    } else {
                        TransitionOutcome::BatchRefused
                    };
                    messages.push(ModelMessage::tool(&call.id, outcome.result()));
                }
                control::save_boundary(state, journal, messages)?;
                if !mixed
                    && state
                        .transitions
                        .last()
                        .is_some_and(|r| r.outcome == TransitionOutcome::Applied)
                {
                    rebuild_context(messages, journal, active, &extensions)?;
                    transcribed = 0;
                    opaque_bytes = 0;
                    control::save_boundary(state, journal, messages)?;
                }
                continue;
            }

            for (tool_call_index, call) in turn.tool_calls.into_iter().enumerate() {
                check_cancelled(cancellation)?;
                state.current_tool = call.id.clone();
                journal.update(|c| {
                    c.state = state.clone();
                    if let Some(group) = c.record.groups.last_mut() {
                        group.capture_results(messages);
                    }
                })?;
                let tool_call_index = tool_call_index + 1;
                if call.id.trim().is_empty() {
                    reject_tool_call(model_turns, tool_call_index, "empty-tool-call-id");
                    return Err(PromptError::EmptyToolCallId);
                }
                if call.function.name == AGENT_CONFIG_TOOL_NAME
                    && let Some(config) = agent_config
                {
                    inspect_agent_config_into(
                        messages,
                        config,
                        &call,
                        model_turns,
                        tool_call_index,
                        &mut state.agent_config_shown,
                    )?;
                    continue;
                }
                if call.function.name == SKILL_TOOL_NAME && !skills.is_empty() {
                    skills::read_skill_into(
                        messages,
                        skills,
                        &mut state.skill_reads,
                        &call,
                        model_turns,
                        tool_call_index,
                    )?;
                    continue;
                }
                if call.function.name == IMPROVEMENT_TOOL_NAME && improvement_suggestions {
                    improvement::suggest_improvement_into(
                        messages,
                        &mut state.suggestions,
                        &call,
                        model_turns,
                        tool_call_index,
                    )?;
                    continue;
                }
                if call.function.name == ASSET_TOOL_NAME
                    && let Some(source) = assets
                {
                    if state.spent.asset_fetches >= 4 {
                        return Err(CheckpointError::Budget.into());
                    }
                    state.spent.asset_fetches += 1;
                    journal.update(|c| c.state = state.clone())?;
                    fetch_asset_into(messages, source, &call, model_turns, tool_call_index)?;
                    continue;
                }
                if call.function.name == IMAGE_GENERATION_TOOL_NAME
                    && let Some(generation) = image_generation
                {
                    journal.update(|c| {
                        c.state = state.clone();
                        c.state.image_generation_attempted = true;
                    })?;
                    generate_image_into(
                        messages,
                        generation,
                        &mut state.image_generation_attempted,
                        &call,
                        model_turns,
                        tool_call_index,
                        journal,
                    )?;
                    continue;
                }
                // The model-selected name is deliberately not copied into telemetry: it is untrusted
                // model output, and an operator reads it from the error on stderr instead.
                if call.function.name != SCRIPT_TOOL_NAME {
                    reject_tool_call(model_turns, tool_call_index, "unknown-tool");
                    return Err(PromptError::UnknownTool(call.function.name));
                }
                let script = match script_argument(&call.function.name, &call.function.arguments) {
                    Ok(script) => script,
                    Err(error) => {
                        reject_tool_call(model_turns, tool_call_index, error.telemetry_kind());
                        return Err(error);
                    }
                };

                // Whatever the session has already spent is unavailable to this script, so a model
                // cannot widen its own budget by splitting work across more tool calls.
                let remaining = limits
                    .max_capability_calls
                    .saturating_sub(state.spent.capability_invocations);
                // One span per unit of tool work. That unit is now a whole script rather than a single
                // capability call, so the per-capability detail the old loop recorded here lives in
                // the interpreter and is reported through the outcome attributes below.
                let span = tracing::info_span!(
                    "prompt.script",
                    model.turn = model_turns,
                    tool_call.index = tool_call_index,
                    script.max_capability_calls = remaining,
                    script.bytes = script.len()
                );
                let outcome = {
                    let _entered = span.enter();
                    if dekopon_core::telemetry_payloads() {
                        tracing::info!(
                            target: "dekopon_harness::audit",
                            {
                                audit.event = "agent.tool.script",
                                model.turn = model_turns,
                                tool_call.index = tool_call_index,
                                script = script.as_str(),
                            },
                            "agent tool script"
                        );
                    }
                    // `run_script` returns no `Result`: a failed script is an outcome the model reads
                    // and recovers from, so the `prompt.script` span always closes normally and
                    // reports the script's own exit code rather than a host error.
                    check_cancelled(cancellation)?;
                    let outcome = runtime.run_script_observed(&script, remaining, journal);
                    if dekopon_core::telemetry_payloads() {
                        tracing::info!(
                            target: "dekopon_harness::audit",
                            {
                                audit.event = "agent.tool.output",
                                model.turn = model_turns,
                                tool_call.index = tool_call_index,
                                output = outcome.output.as_str(),
                            },
                            "agent tool output"
                        );
                    }
                    outcome
                };
                state.spent.script_calls = state.spent.script_calls.saturating_add(1);
                state.spent.capability_invocations = if runtime.observes_executions() {
                    journal.snapshot().state.spent.capability_invocations
                } else {
                    state
                        .spent
                        .capability_invocations
                        .saturating_add(outcome.capability_calls)
                };
                messages.push(ModelMessage::tool(call.id, format_script_outcome(&outcome)));
                journal.update(|c| {
                    c.state = state.clone();
                    if let Some(group) = c.record.groups.last_mut() {
                        group.capture_results(messages);
                    }
                })?;
                if let Some(error) = journal.error() {
                    return Err(error.into());
                }
                if journal.snapshot().record.has_unknown_work() {
                    return Err(CheckpointError::UnknownWork.into());
                }
                check_cancelled(cancellation)?;
            }
        }

        Err(PromptError::MaxSteps {
            maximum: limits.max_steps,
        })
    }
}

fn check_freshness<R: ScriptRuntime + ?Sized>(
    runtime: &R,
    journal: &ExecutionJournal<'_>,
) -> Result<(), PromptError> {
    runtime.check_freshness().map_err(|error| {
        tracing::warn!(cause_type = "session-surface-fenced", cause = %error);
        journal.failure(CheckpointError::ScopeChanged);
        PromptError::Checkpoint(CheckpointError::ScopeChanged)
    })
}

fn model_identity_context(identity: &ModelIdentity) -> ModelMessage {
    ModelMessage::system(format!(
        "Host-selected inference identity (not authorization): {}",
        serde_json::to_string(identity).expect("bounded identity serializes")
    ))
}

fn rebuild_context(
    messages: &mut Vec<ModelMessage>,
    journal: &ExecutionJournal<'_>,
    active: &ActiveModel,
    extensions: &SessionExtensions<'_>,
) -> Result<(), PromptError> {
    let had_binary = messages.iter().any(|m| m.parts().is_some());
    let mut rebuilt = Vec::new();
    if let Some(system) = extensions.system {
        rebuilt.push(ModelMessage::system(system));
    }
    if let Some(listing) = skills::prompt_block(extensions.skills) {
        rebuilt.push(ModelMessage::system(listing));
    }
    rebuilt.push(ModelMessage::system(
        extensions
            .capabilities
            .prompt_block(&active.identity.model)?,
    ));
    rebuilt.push(model_identity_context(&active.identity));
    if extensions.optional_reply {
        rebuilt.push(ModelMessage::system(OPTIONAL_REPLY_INSTRUCTION));
    }
    let snapshot = journal.snapshot();
    let default_policy = crate::context::WindowContext;
    rebuilt.extend(
        extensions
            .context_policy
            .unwrap_or(&default_policy)
            .select(&snapshot.history),
    );
    crate::context::replay_job(&snapshot.record, &mut rebuilt);
    if had_binary {
        rebuilt.push(ModelMessage::user("[Request-local attachment bytes discarded during transition. Fetch again only if needed within the remaining budget.]"));
    }
    crate::context::bound_live(&mut rebuilt)?;
    *messages = rebuilt;
    Ok(())
}

pub(crate) fn check_cancelled(
    cancellation: Option<&dyn CancellationProbe>,
) -> Result<(), PromptError> {
    if cancellation.is_some_and(CancellationProbe::is_cancelled) {
        Err(PromptError::Cancelled)
    } else {
        Ok(())
    }
}

/// Failure to complete a prompt/tool session.
///
/// Every variant here is a broken *session*, not a failed script. A script that parses badly,
/// trips a budget, or calls a capability policy refuses is reported to the model through
/// [`format_script_outcome`] so it can recover.
#[derive(Debug, Error)]
pub enum PromptError {
    /// Mandatory ledger failure.
    #[error(transparent)]
    Accounting(#[from] dekopon_model::usage::AccountingError),
    /// A fenced or invalid configured model control cannot admit further inference.
    #[error(transparent)]
    Control(#[from] control::ControlError),
    /// Latest live observations survive persistence failure; never resume the store's older copy.
    #[error("session fenced: {source}; live observations retained, no automatic retry is safe")]
    Interrupted {
        source: CheckpointError,
        checkpoint: Box<Checkpoint>,
    },
    /// Checkpoint, evidence capacity, or unresolved-work fence halted the session.
    #[error(transparent)]
    Checkpoint(#[from] CheckpointError),
    /// The fresh capability surface or selected model identity was invalid or oversized.
    #[error(transparent)]
    Bootstrap(#[from] BootstrapError),
    /// The embedding caller stopped the session at a cooperative boundary.
    #[error("prompt session was cancelled")]
    Cancelled,
    /// A zero-length loop was requested.
    #[error("prompt max steps must be greater than zero")]
    ZeroSteps,
    /// A model request failed.
    #[error(transparent)]
    Model(#[from] ModelError),
    /// The model selected a tool that was not offered.
    #[error("model requested unknown or unavailable tool {0:?}")]
    UnknownTool(String),
    /// A model requested more tool calls in one turn than a plan ever needs.
    #[error("model returned {actual} tool calls in one turn; the maximum is {maximum}")]
    TooManyToolCalls {
        /// Model-requested call count.
        actual: usize,
        /// Fixed per-turn bound.
        maximum: usize,
    },
    /// A model supplied an empty tool-call correlation ID.
    #[error("model returned an empty tool-call ID")]
    EmptyToolCallId,
    /// Tool arguments were malformed JSON.
    #[error("model returned invalid JSON arguments for tool {tool:?}")]
    InvalidArguments {
        /// Prompt-visible tool name.
        tool: String,
        /// JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// Tool arguments were valid JSON but not an object.
    #[error("model arguments for tool {tool:?} must be a JSON object")]
    ArgumentsNotObject {
        /// Prompt-visible tool name.
        tool: String,
    },
    /// The agent-configuration tool received fields despite having no arguments.
    #[error("model arguments for tool {tool:?} must be an empty object")]
    AgentConfigArgumentsNotEmpty {
        /// Prompt-visible tool name.
        tool: String,
    },
    /// The optional-reply decline tool received fields despite having no arguments.
    #[error("model arguments for tool {tool:?} must be an empty object")]
    DeclineReplyArgumentsNotEmpty {
        /// Prompt-visible tool name.
        tool: String,
    },
    /// Tool arguments carried no script to run.
    #[error("model arguments for tool {tool:?} must include a string \"script\" field")]
    MissingScript {
        /// Prompt-visible tool name.
        tool: String,
    },
    /// Tool arguments carried no asset to fetch.
    #[error("model arguments for tool {tool:?} must include an integer \"id\" field")]
    MissingAssetId {
        /// Prompt-visible tool name.
        tool: String,
    },
    /// Image-generation arguments carried no non-empty prompt.
    #[error("model arguments for tool {tool:?} must include a non-empty string \"prompt\" field")]
    MissingImagePrompt {
        /// Prompt-visible tool name.
        tool: String,
    },
    /// Image-generation arguments contained fields outside the strict one-prompt schema.
    #[error("model arguments for tool {tool:?} contain unexpected fields")]
    UnexpectedImageArguments {
        /// Prompt-visible tool name.
        tool: String,
    },
    /// Skill-reading arguments carried no skill name.
    #[error("model arguments for tool {tool:?} must include a non-empty string \"name\" field")]
    MissingSkillName {
        /// Prompt-visible tool name.
        tool: String,
    },
    /// Skill-reading arguments contained fields outside the strict name-and-resource schema.
    #[error("model arguments for tool {tool:?} contain unexpected or mistyped fields")]
    UnexpectedSkillArguments {
        /// Prompt-visible tool name.
        tool: String,
    },
    /// Suggestion arguments were a JSON object but not the six-field shape the tool declares.
    #[error("model arguments for tool {tool:?} do not match the suggestion schema")]
    InvalidSuggestion {
        /// Prompt-visible tool name.
        tool: String,
        /// Decoder diagnostic.
        #[source]
        source: serde_json::Error,
    },
    /// The model-authored image prompt exceeded the fixed byte ceiling.
    #[error("image generation prompt is {actual} bytes; maximum is {maximum}")]
    ImagePromptTooLarge {
        /// Actual UTF-8 byte length.
        actual: usize,
        /// Fixed maximum UTF-8 byte length.
        maximum: usize,
    },
    /// Capability work ran, then the model tried to decline with no reporting turn left.
    #[error("model tried to suppress a reply after capability work with no reporting turn left")]
    UnreportedCapabilityWork,
    /// The model ended without text or a tool call.
    #[error("model returned neither tool calls nor a final answer")]
    EmptyAnswer,
    /// The model did not produce a final answer within the configured loop bound.
    #[error("model did not produce a final answer within {maximum} turns")]
    MaxSteps {
        /// Configured model-turn limit.
        maximum: u32,
    },
}

impl PromptError {
    /// Stable, low-cardinality failure category for telemetry.
    ///
    /// Several variants carry model-chosen text; the category returned here never does. Embedding
    /// binaries reuse it so a session failure is labeled identically wherever it is reported.
    #[must_use]
    pub fn telemetry_kind(&self) -> &'static str {
        match self {
            Self::Checkpoint(_) | Self::Interrupted { .. } => "checkpoint",
            Self::Accounting(_) => "accounting",
            Self::Control(_) => "model-control",
            Self::Bootstrap(_) => "invalid-bootstrap",
            Self::Cancelled => "cancelled",
            Self::ZeroSteps => "zero-steps",
            // A ledger refusal is permanent and this process caused it; `model` tells an
            // operator the endpoint misbehaved and that a retry is reasonable. Both are false.
            Self::Model(ModelError::Accounting(_)) => "accounting",
            Self::Model(_) => "model",
            Self::UnknownTool(_) => "unknown-tool",
            Self::TooManyToolCalls { .. } => "too-many-tool-calls",
            Self::EmptyToolCallId => "empty-tool-call-id",
            Self::InvalidArguments { .. } => "invalid-json-arguments",
            Self::ArgumentsNotObject { .. } => "arguments-not-object",
            Self::AgentConfigArgumentsNotEmpty { .. } => "agent-config-arguments-not-empty",
            Self::DeclineReplyArgumentsNotEmpty { .. } => "decline-reply-arguments-not-empty",
            Self::MissingScript { .. } => "missing-script",
            Self::MissingAssetId { .. } => "missing-asset-id",
            Self::MissingImagePrompt { .. } => "missing-image-prompt",
            Self::UnexpectedImageArguments { .. } => "unexpected-image-arguments",
            Self::MissingSkillName { .. } => "missing-skill-name",
            Self::UnexpectedSkillArguments { .. } => "unexpected-skill-arguments",
            Self::InvalidSuggestion { .. } => "invalid-suggestion",
            Self::ImagePromptTooLarge { .. } => "image-prompt-too-large",
            Self::UnreportedCapabilityWork => "unreported-capability-work",
            Self::EmptyAnswer => "empty-answer",
            Self::MaxSteps { .. } => "max-steps",
        }
    }
}

fn transcript(messages: &[ModelMessage]) -> String {
    serde_json::to_string(messages).expect("typed model messages serialize")
}
fn tool_calls_json(tool_calls: &[ModelToolCall]) -> String {
    serde_json::to_string(tool_calls).expect("typed tool calls serialize")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use dekopon_model::{
        image::{GeneratedImage, ImageGenerationError, ImageGenerator},
        model::{
            AssistantTurn, ChatModel, CompletionOptions, ModelError, ModelFunctionCall,
            ModelMessage, ModelTool, ModelToolCall, ModelUsage,
        },
    };
    use dekopon_shell::{CapabilityCallResult, CapabilityInvoker, ExitCode, ScriptOutcome};
    use serde_json::{Value, json};

    use crate::meta::{
        AgentConfigView, ConversationConfigView, EffectiveCapabilityView, SessionConfigView,
    };

    use super::{
        AGENT_CONFIG_ALREADY_SHOWN, AGENT_CONFIG_TOOL_NAME, ASSET_TOOL_NAME, AssetSource,
        CancellationProbe, DECLINE_REPLY_TOOL_NAME, FetchedAsset, GeneratedImageOutput, History,
        IMAGE_GENERATION_TOOL_NAME, IMPROVEMENT_TOOL_NAME, JobRecord, MAX_TEXTUAL_ASSET_BYTES,
        MAX_TOOL_CALLS_PER_TURN, PromptError, PromptLimits, ReplyDisposition,
        SCRIPT_TOOL_DESCRIPTION, SCRIPT_TOOL_NAME, SKILL_TOOL_NAME, ScriptRuntime,
        SessionBootstrap, SessionEngine, SessionExit, agent_config_tool, format_script_outcome,
        script_tool,
    };

    use crate::{
        bootstrap::{BootstrapError, CapabilitySnapshot},
        history::{DEFAULT_MAX_BYTES, DEFAULT_MAX_TURNS, HistoryLimits},
    };

    fn run_prompt<M, R>(
        model: &M,
        runtime: &R,
        prompt: &str,
        system: Option<&str>,
        limits: PromptLimits,
    ) -> Result<SessionExit, PromptError>
    where
        M: ChatModel + ?Sized,
        R: ScriptRuntime + ?Sized,
    {
        let mut history = History::default();
        run_prompt_with_history(model, runtime, prompt, system, limits, &mut history)
    }

    /// Runs one bounded prompt/tool session as the continuation of an earlier conversation.
    ///
    /// `history` is both the input and the output: the remembered exchanges are replayed ahead of
    /// `prompt`, and this session's own exchange is recorded into it before returning. It is an
    /// accumulator rather than a returned value on purpose. A session that fails still consumed the
    /// operator's message, and a signature returning `(SessionExit, History)` hands the history back
    /// only on the success path — every caller writing the natural `?` would silently drop the
    /// conversation exactly when a turn had gone wrong and the operator was about to retry. Borrowing
    /// the accumulator makes losing it impossible: whatever the caller does with the `Result`, the
    /// exchange is already recorded. See [`JobRecord::unanswered`] for what a failed turn
    /// leaves behind.
    ///
    /// `system` is supplied fresh on every call and is never remembered; [`JobRecord`] explains
    /// the request corruption that separation prevents. The upside is that editing an agent's
    /// instructions takes effect on the next message without rewriting a single stored conversation.
    /// The matching obligation is on the caller: pass the *same* `system` for every call of one
    /// conversation unless a change is intended. Instructions are hoisted out of the message list
    /// entirely on the ChatGPT path, so changing them — including changing between `None` and
    /// `Some`, since an absent system prompt is replaced by that backend's own default rather than by
    /// nothing — rewrites the front of every subsequent request and discards the provider's prompt
    /// cache for the conversation.
    fn run_prompt_with_history<M, R>(
        model: &M,
        runtime: &R,
        prompt: &str,
        system: Option<&str>,
        limits: PromptLimits,
        history: &mut History,
    ) -> Result<SessionExit, PromptError>
    where
        M: ChatModel + ?Sized,
        R: ScriptRuntime + ?Sized,
    {
        run_prompt_with_history_and_options(
            model,
            runtime,
            prompt,
            system,
            limits,
            history,
            &CompletionOptions::default(),
        )
    }

    /// The same conversation continuation, carrying request-scoped routing metadata to every model
    /// call this session makes.
    ///
    /// `options` is the [`CompletionOptions`] the loop hands to [`ChatModel::complete_with`], and it is
    /// deliberately a *request* input rather than session state: nothing in it changes what the model
    /// is asked, only how the provider routes the request that carries it. A caller passing
    /// [`CompletionOptions::default`] gets the byte-identical requests
    /// [`run_prompt_with_history`] has always produced, which is why that function is this one with a
    /// default rather than a separate implementation.
    ///
    /// Every turn of the session sends the same options, which is the point of a prompt cache key: the
    /// tool-calling turns within one session share the longest prefix of all, and they are exactly the
    /// requests a per-session key routes to one cache lane.
    ///
    /// A model that implements only [`ChatModel::complete`] still answers, because `complete_with` is a
    /// provided method that discards what it does not understand. The cost of that is a cache lookup,
    /// never an answer.
    fn run_prompt_with_history_and_options<M, R>(
        model: &M,
        runtime: &R,
        prompt: &str,
        system: Option<&str>,
        limits: PromptLimits,
        history: &mut History,
        options: &CompletionOptions,
    ) -> Result<SessionExit, PromptError>
    where
        M: ChatModel + ?Sized,
        R: ScriptRuntime + ?Sized,
    {
        run_prompt_session(
            model,
            runtime,
            SessionBootstrap::new(prompt, limits, "fixture-model")
                .with_system(system)
                .with_options(options),
            history,
        )
    }

    fn run_prompt_session<M: ChatModel + ?Sized, R: ScriptRuntime + ?Sized>(
        model: &M,
        runtime: &R,
        inputs: SessionBootstrap<'_>,
        history: &mut History,
    ) -> Result<SessionExit, PromptError> {
        SessionEngine::new(model, runtime).run(inputs, history)
    }

    /// A model whose turns are fixed in advance, recording what it was asked.
    ///
    /// `Mutex` rather than `RefCell`: the loop now runs on a blocking task, so every fixture it
    /// touches has to cross a thread boundary.
    ///
    /// The whole `messages` slice is captured rather than a filtered projection of it. History
    /// assertions are about ordering, role placement, and what is *absent* from a request, none of
    /// which survive a filter applied before the test sees the request.
    struct ScriptedModel {
        turns: Mutex<VecDeque<AssistantTurn>>,
        observed_tools: Mutex<Vec<Vec<ModelTool>>>,
        observed_messages: Mutex<Vec<Vec<ModelMessage>>>,
    }

    impl ScriptedModel {
        fn new(turns: impl IntoIterator<Item = AssistantTurn>) -> Self {
            Self {
                turns: Mutex::new(turns.into_iter().collect()),
                observed_tools: Mutex::new(Vec::new()),
                observed_messages: Mutex::new(Vec::new()),
            }
        }

        /// Messages the model saw on its first request of the session.
        fn first_request(&self) -> Vec<ModelMessage> {
            self.observed_messages
                .lock()
                .expect("message observations lock")
                .first()
                .cloned()
                .expect("the model was asked at least once")
                .into_iter()
                .filter(|message| {
                    !message
                        .content()
                        .is_some_and(crate::bootstrap::is_prompt_block)
                })
                .collect()
        }

        /// `(role, content)` pairs from the first request, the shape most assertions want.
        fn first_roles(&self) -> Vec<(&'static str, String)> {
            self.first_request()
                .iter()
                .filter(|message| {
                    !message
                        .content()
                        .is_some_and(crate::bootstrap::is_prompt_block)
                })
                .map(|message| {
                    (
                        message.role(),
                        message.content().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        }

        /// Every tool result the model was handed, across every request.
        fn tool_messages(&self) -> Vec<String> {
            self.observed_messages
                .lock()
                .expect("message observations lock")
                .iter()
                .flatten()
                .filter(|message| message.role() == "tool")
                .filter_map(|message| message.content().map(str::to_owned))
                .collect()
        }
    }

    impl ChatModel for ScriptedModel {
        fn complete(
            &self,
            messages: &[ModelMessage],
            tools: &[ModelTool],
            recorder: &dyn dekopon_model::usage::AttemptRecorder,
        ) -> Result<AssistantTurn, ModelError> {
            let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
            let result: Result<AssistantTurn, ModelError> = {
                self.observed_tools
                    .lock()
                    .expect("tool observations lock")
                    .push(tools.to_vec());
                self.observed_messages
                    .lock()
                    .expect("message observations lock")
                    .push(messages.to_vec());
                self.turns
                    .lock()
                    .expect("turn lock")
                    .pop_front()
                    .ok_or(ModelError::NoChoices)
            };
            if let Ok(turn) = &result
                && let Some(usage) = turn.usage
            {
                recorder.observe(
                    attempt,
                    dekopon_model::usage::UsageObservation {
                        usage,
                        invalid: [false; 5],
                    },
                )?;
            }
            result
        }
    }

    /// A runtime that records the scripts and ceilings it was handed.
    struct RecordingRuntime {
        scripts: Mutex<Vec<(String, u32)>>,
        capability_calls_per_script: u32,
    }

    impl RecordingRuntime {
        fn new(capability_calls_per_script: u32) -> Self {
            Self {
                scripts: Mutex::new(Vec::new()),
                capability_calls_per_script,
            }
        }
    }

    impl ScriptRuntime for RecordingRuntime {
        fn capability_snapshot(&self) -> Result<CapabilitySnapshot, BootstrapError> {
            Ok(CapabilitySnapshot::empty())
        }
        fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome {
            self.scripts
                .lock()
                .expect("script lock")
                .push((script.to_owned(), max_capability_calls));
            let capability_calls = self.capability_calls_per_script.min(max_capability_calls);
            ScriptOutcome {
                output: format!("ran {} bytes", script.len()),
                exit_code: ExitCode::SUCCESS,
                truncated: false,
                capability_calls,
                steps: 1,
            }
        }
    }

    fn script_call(id: &str, script: &str) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: id.to_owned(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: json!({ "script": script }).to_string(),
                },
            }],
            usage: None,
            replay_items: Vec::new(),
        }
    }

    fn answer(text: &str) -> AssistantTurn {
        AssistantTurn {
            content: Some(text.to_owned()),
            tool_calls: Vec::new(),
            usage: None,
            replay_items: Vec::new(),
        }
    }

    fn image_call(id: &str, prompt: &str) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: id.to_owned(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: IMAGE_GENERATION_TOOL_NAME.to_owned(),
                    arguments: json!({"prompt": prompt}).to_string(),
                },
            }],
            usage: None,
            replay_items: Vec::new(),
        }
    }

    fn decline_call(id: &str, arguments: Value) -> ModelToolCall {
        ModelToolCall {
            id: id.to_owned(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: DECLINE_REPLY_TOOL_NAME.to_owned(),
                arguments: arguments.to_string(),
            },
        }
    }

    fn decline(arguments: Value) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![decline_call("decline-call", arguments)],
            usage: None,
            replay_items: Vec::new(),
        }
    }

    struct FixedImageGenerator {
        calls: AtomicUsize,
    }

    impl ImageGenerator for FixedImageGenerator {
        fn generate(
            &self,
            _prompt: &str,
            recorder: &dyn dekopon_model::usage::AttemptRecorder,
        ) -> Result<GeneratedImage, ImageGenerationError> {
            recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
            bytes.extend_from_slice(b"secret-generated-pixels");
            GeneratedImage::from_png(bytes)
        }
    }

    struct CancellingImageGenerator(Arc<AtomicBool>);

    impl ImageGenerator for CancellingImageGenerator {
        fn generate(
            &self,
            _prompt: &str,
            recorder: &dyn dekopon_model::usage::AttemptRecorder,
        ) -> Result<GeneratedImage, ImageGenerationError> {
            recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
            self.0.store(true, Ordering::SeqCst);
            let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
            bytes.extend_from_slice(b"cancelled pixels");
            GeneratedImage::from_png(bytes)
        }
    }

    struct AtomicCancellation(Arc<AtomicBool>);

    impl CancellationProbe for AtomicCancellation {
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    struct FailingImageGenerator;

    impl ImageGenerator for FailingImageGenerator {
        fn generate(
            &self,
            _prompt: &str,
            recorder: &dyn dekopon_model::usage::AttemptRecorder,
        ) -> Result<GeneratedImage, ImageGenerationError> {
            recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
            Err(ImageGenerationError::Configuration(
                "provider diagnostic sentinel".to_owned(),
            ))
        }
    }

    fn limits(max_steps: u32, max_capability_calls: u32) -> PromptLimits {
        PromptLimits {
            max_steps,
            max_capability_calls,
        }
    }

    struct Cancelled;

    impl CancellationProbe for Cancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_pre_cancelled_session_never_reaches_the_model_or_history() {
        let model = ScriptedModel::new([answer("too late")]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();
        let error = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("stop", limits(2, 2), "fixture-model")
                .with_cancellation(&Cancelled),
            &mut history,
        )
        .expect_err("cancellation is a terminal session outcome");

        assert!(matches!(error, PromptError::Cancelled));
        assert!(
            model
                .observed_messages
                .lock()
                .expect("message observations lock")
                .is_empty(),
            "no model request starts after cancellation"
        );
        assert_eq!(
            history.len(),
            1,
            "the prompt loop records an unanswered turn"
        );
    }

    #[test]
    fn a_route_generator_yields_one_byte_free_image_output() {
        let model = ScriptedModel::new([
            image_call("image-1", "a tiny orange kitten"),
            // A second request cannot turn one configured generation into ambient unbounded cost.
            image_call("image-2", "another kitten"),
            answer("Here is your kitten."),
        ]);
        let runtime = RecordingRuntime::new(0);
        let generator = FixedImageGenerator {
            calls: AtomicUsize::new(0),
        };
        let output = GeneratedImageOutput::default();
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("draw me a kitty cat", limits(3, 1), "fixture-model")
                .with_image_generation(&generator, &output),
            &mut history,
        )
        .expect("the image tool is a recoverable model turn");

        assert_eq!(outcome.answer, "Here is your kitten.");
        assert_eq!(generator.calls.load(Ordering::SeqCst), 1);
        let image = output.take().expect("one generated output");
        assert_eq!(image.media_type(), "image/png");
        assert_eq!(output.take().map(|image| image.bytes().len()), None);
        let tool_results = model.tool_messages();
        assert!(
            tool_results
                .iter()
                .any(|result| result.contains("Generated one image"))
        );
        assert!(
            tool_results
                .iter()
                .any(|result| result.contains("one image-generation attempt"))
        );
        let transcript = serde_json::to_string(
            &*model
                .observed_messages
                .lock()
                .expect("message observations lock"),
        )
        .expect("transcript serializes");
        assert!(
            !transcript.contains("secret-generated-pixels"),
            "generated bytes entered model messages: {transcript}"
        );
        assert!(
            model
                .observed_tools
                .lock()
                .expect("tool observations lock")
                .iter()
                .all(|tools| tools
                    .iter()
                    .any(|tool| tool.name == IMAGE_GENERATION_TOOL_NAME))
        );
    }

    #[test]
    fn cancellation_during_generation_discards_the_accounted_image() {
        let model = ScriptedModel::new([image_call("image-1", "a kitten")]);
        let runtime = RecordingRuntime::new(0);
        let cancelled = Arc::new(AtomicBool::new(false));
        let generator = CancellingImageGenerator(Arc::clone(&cancelled));
        let probe = AtomicCancellation(cancelled);
        let output = GeneratedImageOutput::default();
        let mut history = History::default();

        let error = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("draw", limits(2, 1), "fixture-model")
                .with_image_generation(&generator, &output)
                .with_cancellation(&probe),
            &mut history,
        )
        .expect_err("cancellation wins after the billed generation request returns");

        assert!(matches!(error, PromptError::Cancelled));
        assert!(
            output.take().is_none(),
            "cancelled bytes must not leave the loop"
        );
        assert!(
            model.tool_messages().is_empty(),
            "no later model turn starts"
        );
    }

    #[test]
    fn image_provider_diagnostics_never_return_to_the_chat_model() {
        let model = ScriptedModel::new([
            image_call("image-1", "a kitten"),
            answer("I could not draw that."),
        ]);
        let runtime = RecordingRuntime::new(0);
        let output = GeneratedImageOutput::default();
        let mut history = History::default();

        run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("draw", limits(2, 1), "fixture-model")
                .with_image_generation(&FailingImageGenerator, &output),
            &mut history,
        )
        .expect("generation failure is a fixed tool outcome");

        assert!(output.take().is_none());
        let results = model.tool_messages();
        assert!(
            results
                .iter()
                .any(|result| result == "Image generation failed. Answer without an image.")
        );
        assert!(
            results
                .iter()
                .all(|result| !result.contains("provider diagnostic sentinel"))
        );
    }

    fn agent_config() -> AgentConfigView {
        AgentConfigView::new(
            "reviewer".to_owned(),
            "Reviews pull requests".to_owned(),
            Some("reasoning".to_owned()),
            Some("Be concise and skeptical.".to_owned()),
            SessionConfigView {
                max_steps: 8,
                max_capability_calls: 16,
                conversation: ConversationConfigView::OneShot,
            },
            vec![EffectiveCapabilityView {
                id: "gh.pull-request.read".to_owned(),
                provider: "gh".to_owned(),
                description: "Reads one pull request".to_owned(),
                effect: "read-only".to_owned(),
                risk: "Low".to_owned(),
                idempotency: "idempotent".to_owned(),
            }],
        )
    }

    fn agent_config_tool_call(id: &str, arguments: Value) -> ModelToolCall {
        ModelToolCall {
            id: id.to_owned(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: AGENT_CONFIG_TOOL_NAME.to_owned(),
                arguments: arguments.to_string(),
            },
        }
    }

    fn agent_config_call(arguments: Value) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![agent_config_tool_call("config-call", arguments)],
            usage: None,
            replay_items: Vec::new(),
        }
    }

    /// A conversation of `count` answered exchanges, every turn the same size.
    fn conversation(count: usize) -> Vec<JobRecord> {
        (1..=count)
            .map(|index| JobRecord::completed(format!("ask {index}"), format!("answer {index}")))
            .collect()
    }

    /// Asserts a replayed window is a request both backends accept.
    ///
    /// The two 400s this feature could produce are a `tool` result whose call was trimmed away and
    /// an assistant `tool_calls` nothing answered. Neither is checked by reading the loop: both are
    /// checked on the serialized message, because the serialized message is what a backend sees.
    fn assert_window_is_well_formed(history: &History) {
        let mut messages = Vec::new();
        history.replay_into(&mut messages);

        let mut pending = std::collections::BTreeSet::new();
        for message in &messages {
            let encoded = serde_json::to_value(message).expect("message serializes");
            assert!(
                !encoded
                    .as_object()
                    .expect("object")
                    .contains_key("replay_items")
            );
            if let Some(calls) = encoded.get("tool_calls").and_then(Value::as_array) {
                assert!(
                    pending.is_empty(),
                    "a batch cannot orphan a previous result"
                );
                for call in calls {
                    assert!(pending.insert(call["id"].as_str().expect("id").to_owned()));
                }
            } else if message.role() == "tool" {
                assert!(
                    pending.remove(
                        encoded["tool_call_id"]
                            .as_str()
                            .expect("result correlation")
                    )
                );
            } else {
                assert!(
                    pending.is_empty(),
                    "incomplete groups must render as summaries"
                );
            }
        }
        assert!(pending.is_empty());
        for turn in history.turns() {
            assert!(
                messages
                    .iter()
                    .any(|m| m.role() == "user" && m.content() == Some(turn.user()))
            );
            if let Some(answer) = turn.answer() {
                assert!(
                    messages
                        .iter()
                        .any(|m| m.role() == "assistant" && m.content() == Some(answer))
                );
            }
        }
    }

    #[test]
    fn token_tracker_sees_reported_and_unreported_successful_model_responses() {
        let mut first = script_call("call-1", "echo hello");
        let expected = ModelUsage {
            input_tokens: Some(41),
            output_tokens: Some(5),
            ..ModelUsage::default()
        };
        first.usage = Some(expected);
        let model = ScriptedModel::new([first, answer("done")]);
        let runtime = RecordingRuntime::new(0);
        let observer = crate::accounting::JobAccounting::default();
        let mut history = History::default();

        run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("run it", limits(3, 4), "fixture-model")
                .with_accounting(&observer),
            &mut history,
        )
        .expect("session succeeds");

        assert_eq!(
            observer
                .snapshot()
                .calls
                .iter()
                .map(|c| c.attempts[0].observation.map(|o| o.usage))
                .collect::<Vec<_>>(),
            vec![Some(expected), None]
        );
    }

    #[test]
    fn the_default_window_is_bounded_in_both_dimensions() {
        let limits = HistoryLimits::default();

        assert_eq!(limits.max_turns, DEFAULT_MAX_TURNS);
        assert_eq!(limits.max_bytes, DEFAULT_MAX_BYTES);
        assert!(History::default().is_empty());
        assert_eq!(History::default().limits(), limits);
    }

    #[test]
    fn a_seeded_conversation_reaches_the_model_ahead_of_the_new_prompt() {
        let model = ScriptedModel::new([answer("Two.")]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::from_turns(HistoryLimits::default(), conversation(2));

        run_prompt_with_history(
            &model,
            &runtime,
            "and now?",
            Some("Be terse."),
            limits(2, 32),
            &mut history,
        )
        .expect("prompt session succeeds");

        assert_eq!(
            model.first_roles(),
            vec![
                ("system", "Be terse.".to_owned()),
                ("user", "ask 1".to_owned()),
                ("assistant", "answer 1".to_owned()),
                (
                    "user",
                    "[Delivery disposition: Pending; generation is not transport acceptance.]"
                        .to_owned()
                ),
                ("user", "ask 2".to_owned()),
                ("assistant", "answer 2".to_owned()),
                (
                    "user",
                    "[Delivery disposition: Pending; generation is not transport acceptance.]"
                        .to_owned()
                ),
                ("user", "and now?".to_owned()),
            ]
        );
    }

    #[test]
    fn the_instructions_are_prepended_once_per_call_and_never_remembered() {
        // The failure this guards is silent rather than loud: the ChatGPT backend joins every
        // `system` message it is handed into one `instructions` string, so a conversation that
        // remembered the system prompt would send an agent its own instructions concatenated with
        // themselves, one extra copy per exchange, with no error from anywhere.
        let system = "You are Dekopon.";
        let mut history = History::default();

        for exchange in 1..=3 {
            let model = ScriptedModel::new([answer(&format!("answer {exchange}"))]);
            let runtime = RecordingRuntime::new(0);

            run_prompt_with_history(
                &model,
                &runtime,
                &format!("ask {exchange}"),
                Some(system),
                limits(2, 32),
                &mut history,
            )
            .expect("prompt session succeeds");

            let request = model.first_request();
            let instructions = request
                .iter()
                .filter(|message| message.role() == "system")
                .collect::<Vec<_>>();
            assert_eq!(
                instructions.len(),
                1,
                "exchange {exchange} sent {} system messages",
                instructions.len()
            );
            assert_eq!(instructions[0].content(), Some(system));
            assert_eq!(request[0].role(), "system");
        }

        assert_eq!(history.len(), 3);
        assert_window_is_well_formed(&history);
        let mut replayed = Vec::new();
        history.replay_into(&mut replayed);
        assert!(replayed.iter().all(|message| message.role() != "system"));
    }

    #[test]
    fn a_remembered_exchange_preserves_correlated_tool_traffic() {
        let model = ScriptedModel::new([
            script_call("call-1", "one"),
            script_call("call-2", "two"),
            answer("I ran two scripts."),
        ]);
        let runtime = RecordingRuntime::new(1);
        let mut history = History::default();

        run_prompt_with_history(
            &model,
            &runtime,
            "do the work",
            None,
            limits(8, 32),
            &mut history,
        )
        .expect("prompt session succeeds");

        // Portable groups keep bounded results and whole correlations, not opaque continuation.
        assert!(!model.tool_messages().is_empty());
        assert_eq!(history.len(), 1);
        assert_eq!(history.turns()[0].user(), "do the work");
        assert_eq!(history.turns()[0].answer(), Some("I ran two scripts."));
        assert_window_is_well_formed(&history);

        let next = ScriptedModel::new([answer("done")]);
        run_prompt_with_history(
            &next,
            &RecordingRuntime::new(0),
            "and again",
            None,
            limits(2, 32),
            &mut history,
        )
        .expect("prompt session succeeds");

        assert_eq!(
            next.first_roles().first(),
            Some(&("user", "do the work".to_owned()))
        );
        assert_eq!(
            next.first_roles().last(),
            Some(&("user", "and again".to_owned()))
        );
        assert_eq!(next.tool_messages().len(), 2);
    }

    #[test]
    fn every_cut_point_leaves_whole_exchanges() {
        let turns = conversation(6);
        let total_bytes = turns.iter().map(JobRecord::bytes).sum::<usize>();

        for max_turns in 0..=turns.len() {
            let history = History::from_turns(
                HistoryLimits {
                    max_turns,
                    max_bytes: usize::MAX,
                },
                turns.clone(),
            );

            assert_eq!(history.len(), max_turns);
            assert_window_is_well_formed(&history);
            // Trimming is oldest-first in whole exchanges, so survivors are always a suffix.
            assert_eq!(history.turns(), &turns[turns.len() - max_turns..]);
        }

        for max_bytes in 0..=total_bytes + 1 {
            let history = History::from_turns(
                HistoryLimits {
                    max_turns: usize::MAX,
                    max_bytes,
                },
                turns.clone(),
            );

            assert!(history.bytes() <= max_bytes);
            assert_window_is_well_formed(&history);
            assert_eq!(history.turns(), &turns[turns.len() - history.len()..]);
        }
    }

    #[test]
    fn the_turn_bound_and_the_byte_bound_each_trim_on_their_own() {
        let turns = conversation(4);

        let by_turns = History::from_turns(
            HistoryLimits {
                max_turns: 2,
                max_bytes: usize::MAX,
            },
            turns.clone(),
        );
        assert_eq!(by_turns.len(), 2);
        assert_eq!(by_turns.turns()[0].user(), "ask 3");

        let by_bytes = History::from_turns(
            HistoryLimits {
                max_turns: usize::MAX,
                max_bytes: turns[0].bytes(),
            },
            turns.clone(),
        );
        assert_eq!(by_bytes.len(), 1);
        assert_eq!(by_bytes.turns()[0].user(), "ask 4");
    }

    #[test]
    fn an_exchange_too_large_for_the_window_leaves_it_empty_rather_than_half_present() {
        let mut history = History::new(HistoryLimits {
            max_turns: 8,
            max_bytes: 4,
        });

        history.record(JobRecord::completed("a long question", "a long answer"));

        assert!(history.is_empty());
        assert_window_is_well_formed(&history);
    }

    #[test]
    fn a_running_conversation_trims_itself_as_it_grows() {
        let mut history = History::new(HistoryLimits {
            max_turns: 2,
            max_bytes: usize::MAX,
        });

        for exchange in 1..=4 {
            let model = ScriptedModel::new([answer(&format!("answer {exchange}"))]);
            run_prompt_with_history(
                &model,
                &RecordingRuntime::new(0),
                &format!("ask {exchange}"),
                None,
                limits(2, 32),
                &mut history,
            )
            .expect("prompt session succeeds");
        }

        assert_eq!(history.len(), 2);
        let model = ScriptedModel::new([answer("done")]);
        run_prompt_with_history(
            &model,
            &RecordingRuntime::new(0),
            "ask 5",
            None,
            limits(2, 32),
            &mut history,
        )
        .expect("prompt session succeeds");

        assert_eq!(
            model.first_roles(),
            vec![
                ("user", "ask 3".to_owned()),
                ("assistant", "answer 3".to_owned()),
                (
                    "user",
                    "[Delivery disposition: Pending; generation is not transport acceptance.]"
                        .to_owned()
                ),
                ("user", "ask 4".to_owned()),
                ("assistant", "answer 4".to_owned()),
                (
                    "user",
                    "[Delivery disposition: Pending; generation is not transport acceptance.]"
                        .to_owned()
                ),
                ("user", "ask 5".to_owned()),
            ]
        );
    }

    #[test]
    fn a_session_that_never_answers_still_remembers_what_it_was_asked() {
        let model = ScriptedModel::new([
            script_call("call-1", "echo one"),
            script_call("call-2", "echo two"),
        ]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        let error = run_prompt_with_history(
            &model,
            &runtime,
            "loop forever",
            None,
            limits(2, 32),
            &mut history,
        )
        .expect_err("an answerless session must terminate");

        assert!(matches!(error, PromptError::MaxSteps { maximum: 2 }));
        assert_eq!(history.len(), 1);
        assert_eq!(history.turns()[0].user(), "loop forever");
        assert_eq!(history.turns()[0].answer(), None);
        assert!(!history.turns()[0].is_answered());
        assert_window_is_well_formed(&history);

        // The retry knows what it is a retry of, and the abandoned attempt's tool traffic is gone.
        let retry = ScriptedModel::new([answer("sorry about that")]);
        run_prompt_with_history(
            &retry,
            &RecordingRuntime::new(0),
            "try again",
            None,
            limits(2, 32),
            &mut history,
        )
        .expect("prompt session succeeds");

        assert_eq!(
            retry.first_roles().first(),
            Some(&("user", "loop forever".to_owned()))
        );
        assert_eq!(
            retry.first_roles().last(),
            Some(&("user", "try again".to_owned()))
        );
        assert_eq!(retry.tool_messages().len(), 2);
    }

    #[test]
    fn a_broken_model_connection_still_remembers_what_it_was_asked() {
        let model = ScriptedModel::new([]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        let error = run_prompt_with_history(
            &model,
            &runtime,
            "ask something",
            None,
            limits(2, 32),
            &mut history,
        )
        .expect_err("a model failure ends the session");

        assert!(matches!(error, PromptError::Model(_)));
        assert_eq!(history.len(), 1);
        assert_eq!(history.turns()[0].user(), "ask something");
        assert!(!history.turns()[0].is_answered());
    }

    #[test]
    fn a_zero_step_session_records_nothing() {
        // A usage error, not a conversation event: no request was built, so nothing in the
        // conversation may claim the model was asked.
        let model = ScriptedModel::new([]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::from_turns(HistoryLimits::default(), conversation(1));

        let error = run_prompt_with_history(
            &model,
            &runtime,
            "nothing",
            None,
            limits(0, 32),
            &mut history,
        )
        .expect_err("a zero-step session is a usage error");

        assert!(matches!(error, PromptError::ZeroSteps));
        assert_eq!(history.len(), 1);
        assert_eq!(history.turns()[0].user(), "ask 1");
    }

    #[test]
    fn run_prompt_starts_every_session_from_an_empty_conversation() {
        for _ in 0..2 {
            let model = ScriptedModel::new([answer("done")]);
            let runtime = RecordingRuntime::new(0);

            run_prompt(
                &model,
                &runtime,
                "same question",
                Some("Be terse."),
                limits(2, 32),
            )
            .expect("prompt session succeeds");

            assert_eq!(
                model.first_roles(),
                vec![
                    ("system", "Be terse.".to_owned()),
                    ("user", "same question".to_owned()),
                ]
            );
        }
    }

    /// A model that answers once per request and records the options each request carried.
    ///
    /// Deliberately overrides `complete_with` and leaves `complete` panicking: every other double
    /// in this module implements only `complete`, which is what proves the provided-method default
    /// still works, so this one exists to prove the other half — that the loop really does take the
    /// `complete_with` path when it has options to pass.
    struct OptionsObserver {
        turns: Mutex<VecDeque<AssistantTurn>>,
        observed: Mutex<Vec<Option<String>>>,
    }

    impl ChatModel for OptionsObserver {
        fn complete(
            &self,
            _: &[ModelMessage],
            _: &[ModelTool],
            _: &dyn dekopon_model::usage::AttemptRecorder,
        ) -> Result<AssistantTurn, ModelError> {
            panic!("the loop must reach a model through complete_with");
        }

        fn complete_with(
            &self,
            _messages: &[ModelMessage],
            _tools: &[ModelTool],
            options: &CompletionOptions,
            recorder: &dyn dekopon_model::usage::AttemptRecorder,
        ) -> Result<AssistantTurn, ModelError> {
            let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
            let result: Result<AssistantTurn, ModelError> = {
                self.observed
                    .lock()
                    .expect("options lock")
                    .push(options.prompt_cache_key().map(str::to_owned));
                self.turns
                    .lock()
                    .expect("turn lock")
                    .pop_front()
                    .ok_or(ModelError::NoChoices)
            };
            if let Ok(turn) = &result
                && let Some(usage) = turn.usage
            {
                recorder.observe(
                    attempt,
                    dekopon_model::usage::UsageObservation {
                        usage,
                        invalid: [false; 5],
                    },
                )?;
            }
            result
        }
    }

    #[test]
    fn every_turn_of_a_session_carries_the_same_routing_metadata() {
        // The tool-calling turns within one session share the longest prefix in the whole feature —
        // each one repeats every message before it — so a key that reached only the first request
        // would miss the requests it helps most.
        let model = OptionsObserver {
            turns: Mutex::new(
                [
                    script_call("call-1", "echo one"),
                    script_call("call-2", "echo two"),
                    answer("done"),
                ]
                .into_iter()
                .collect(),
            ),
            observed: Mutex::new(Vec::new()),
        };
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        let outcome = run_prompt_with_history_and_options(
            &model,
            &runtime,
            "ask",
            Some("Be terse."),
            limits(4, 32),
            &mut history,
            &CompletionOptions::default().with_prompt_cache_key("lane-7"),
        )
        .expect("prompt session succeeds");

        assert_eq!(outcome.model_turns, 3);
        assert_eq!(
            *model.observed.lock().expect("options lock"),
            vec![
                Some("lane-7".to_owned()),
                Some("lane-7".to_owned()),
                Some("lane-7".to_owned())
            ]
        );
    }

    #[test]
    fn a_session_without_options_asks_exactly_what_it_always_asked() {
        // The additive half of the contract: a caller that supplies nothing must be indistinguishable
        // from the same caller before options existed, which is why the default carries no key
        // rather than an empty one.
        let observer = OptionsObserver {
            turns: Mutex::new([answer("done")].into_iter().collect()),
            observed: Mutex::new(Vec::new()),
        };
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        run_prompt_with_history(
            &observer,
            &runtime,
            "ask",
            Some("Be terse."),
            limits(2, 32),
            &mut history,
        )
        .expect("prompt session succeeds");

        assert_eq!(*observer.observed.lock().expect("options lock"), vec![None]);
    }

    /// A provider's command words reach the model, or it has no way to know they exist.
    ///
    /// `cap --list` enumerates capabilities, not the ergonomic words a provider layers over them,
    /// so a word absent from this description is a word the model will never type.
    #[test]
    fn provider_command_words_are_offered_to_the_model() {
        // Load order and a repeated word must not change the definition the model is sent.
        let tool = script_tool(&["gh".to_owned(), "fly".to_owned(), "gh".to_owned()]);
        assert!(
            tool.description.contains("command words: fly, gh."),
            "{}",
            tool.description
        );
        // A provider word is a program of its own, and its help page is the only place its
        // subcommands and flags are described; the model has to be told where to look.
        assert!(
            tool.description.contains("run `<word> --help`"),
            "{}",
            tool.description
        );
        assert_no_doubled_spaces(&tool.description);
    }

    /// This is the one string the project treats as engineered prompt text, and it ships verbatim
    /// to the model on every request. A run of spaces is a collapsed line continuation: junk
    /// tokens that read to a model as a typo, and which substring assertions cannot see.
    fn assert_no_doubled_spaces(description: &str) {
        assert!(
            !description.contains("  "),
            "the tool description contains a run of spaces: {description}"
        );
    }

    /// A session holding nothing, for scripts that must be refused before a command ever runs.
    struct NoCapabilities;

    impl CapabilityInvoker for NoCapabilities {
        fn granted(&self) -> Vec<String> {
            Vec::new()
        }

        fn invoke(
            &self,
            capability: &str,
            _input: Value,
            _secret_use: Option<dekopon_core::SecretUseProposal>,
        ) -> CapabilityCallResult {
            panic!("a refused construct must never reach {capability}");
        }
    }

    /// Returns the constructs the description still calls errors, as it writes their names.
    fn refusal_list() -> Vec<&'static str> {
        let listed = SCRIPT_TOOL_DESCRIPTION
            .split_once("fails loudly and by name: ")
            .expect("the description still names the constructs it refuses")
            .1
            .split_once(" are errors")
            .expect("the refusal list still ends at `are errors`")
            .0;
        listed
            .split(", ")
            .map(|name| name.strip_prefix("and ").unwrap_or(name))
            .collect()
    }

    /// The refusal list is the interpreter's API documentation, not a comment about it.
    ///
    /// No human writes these scripts, so a construct the description calls an error is one the
    /// model will never type — which is how `[[ ]]` and `set -e` stayed unreachable after #165
    /// implemented them. Pinning the list to the shell the way `dekopon-shell`'s builtin registry
    /// is pinned to its documented builtin list is what makes that drift a test failure: the names
    /// still listed must be exactly these, and each must be refused by the interpreter itself.
    #[test]
    fn every_construct_the_description_calls_an_error_is_refused_by_the_shell() {
        // Name as the description writes it, a script that reaches the construct, and the word its
        // refusal has to carry — a refusal naming the wrong feature sends a model to the wrong fix.
        let refused = [
            ("`eval`", "eval 'echo hi'", "eval"),
            ("backticks", "echo `echo hi`", "backtick"),
            ("subshells", "(echo hi)", "subshells"),
            ("`<<<`", "cat <<<\"hi\"", "here-string"),
            (
                "`&` backgrounding",
                "sleep 1 &\necho after",
                "backgrounding",
            ),
        ];

        assert_eq!(
            refusal_list(),
            refused.iter().map(|(name, ..)| *name).collect::<Vec<_>>()
        );

        for (name, script, expected) in refused {
            let outcome = dekopon_shell::run(script, &NoCapabilities);
            assert_eq!(outcome.exit_code, ExitCode::SYNTAX, "{name}: {outcome:?}");
            assert!(
                outcome.output.contains(expected),
                "{name}: {}",
                outcome.output
            );
        }
    }

    /// The other half of the same pin: what #165 built has to stay off the refusal list.
    ///
    /// Without this, dropping `[[ ]]` and `set -e` from the list could be undone — or the shell
    /// could stop supporting them — and the test above would still pass on the shortened list.
    #[test]
    fn conditionals_and_errexit_are_supported_rather_than_refused() {
        let listed = refusal_list();
        assert!(!listed.iter().any(|name| name.contains("[[")), "{listed:?}");
        assert!(
            !listed.iter().any(|name| name.contains("set -e")),
            "{listed:?}"
        );

        let conditional = dekopon_shell::run(
            "if [[ \"a\" == \"a\" ]]; then echo yes; fi",
            &NoCapabilities,
        );
        assert_eq!(conditional.exit_code, ExitCode::SUCCESS, "{conditional:?}");
        assert_eq!(conditional.output, "yes");

        let errexit = dekopon_shell::run("set -e\nnosuchcmd.here\necho after", &NoCapabilities);
        assert_eq!(errexit.exit_code, ExitCode::NOT_FOUND, "{errexit:?}");
        assert!(errexit.output.contains("`set -e` is on"), "{errexit:?}");
        assert!(!errexit.output.contains("after"), "{errexit:?}");
    }

    /// A session whose capabilities cover every outcome the description explains.
    struct OutcomeCapabilities;

    impl CapabilityInvoker for OutcomeCapabilities {
        fn granted(&self) -> Vec<String> {
            ["posts.get", "locked.door", "broken.thing"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        }

        fn describe(&self, _capability: &str) -> Option<dekopon_shell::CapabilityDescription> {
            None
        }

        fn invoke(
            &self,
            capability: &str,
            input: Value,
            _secret_use: Option<dekopon_core::SecretUseProposal>,
        ) -> CapabilityCallResult {
            match capability {
                "posts.get" => CapabilityCallResult::Succeeded(input),
                "locked.door" => CapabilityCallResult::Denied {
                    reason: "policy says no".to_owned(),
                },
                "broken.thing" => CapabilityCallResult::Failed {
                    error: "upstream boom".to_owned(),
                },
                _ => CapabilityCallResult::NotFound,
            }
        }
    }

    /// The exit codes, messages, and argument rules the description promises are the shell's.
    ///
    /// The "Reading the result" paragraph and the flag-typing sentence describe interpreter
    /// behaviour that no prose can keep true on its own; this pins each promise to the shell the
    /// way `refusal_list` pins the refused constructs, so a remapped code, a reworded message, or
    /// a changed typing rule fails here rather than misleading a model.
    #[test]
    fn every_outcome_the_description_explains_is_what_the_shell_produces() {
        for (code, phrase) in [
            (ExitCode::SUCCESS, "Exit 0 is success"),
            (ExitCode::FAILURE, "Exit 1 is a command that ran and failed"),
            (
                ExitCode::SYNTAX,
                "Exit 2 is a parse error, a refused construct, a usage error, or an exhausted budget",
            ),
            (ExitCode::TIMEOUT, "Exit 124 is the wall-clock deadline"),
            (
                ExitCode::DENIED,
                "Exit 126 means this session holds the capability but authorization refused this use",
            ),
            (
                ExitCode::NOT_FOUND,
                "Exit 127 (`command not found` or `capability not found`)",
            ),
        ] {
            assert!(SCRIPT_TOOL_DESCRIPTION.contains(phrase), "{phrase}");
            assert!(
                phrase.contains(&format!("Exit {} ", code.get())),
                "{phrase} must name exit code {}",
                code.get()
            );
        }

        let not_found = dekopon_shell::run("nosuch.capability --x 1", &OutcomeCapabilities);
        assert_eq!(not_found.exit_code, ExitCode::NOT_FOUND, "{not_found:?}");
        assert!(
            not_found.output.contains("command not found"),
            "{not_found:?}"
        );

        let denied = dekopon_shell::run("locked.door --knock", &OutcomeCapabilities);
        assert_eq!(denied.exit_code, ExitCode::DENIED, "{denied:?}");

        let failed = dekopon_shell::run("broken.thing", &OutcomeCapabilities);
        assert_eq!(failed.exit_code, ExitCode::FAILURE, "{failed:?}");
        assert!(
            failed
                .output
                .contains("broken.thing: failed: upstream boom"),
            "{failed:?}"
        );

        let usage = dekopon_shell::run("echo abc | grep '[0-9]'", &OutcomeCapabilities);
        assert_eq!(usage.exit_code, ExitCode::SYNTAX, "{usage:?}");

        // Item 2: numbers, booleans, and null typed; anything else a string; bare flag true;
        // repeats an array. The echo-like capability returns exactly what it was sent.
        let typed = dekopon_shell::run(
            "posts.get --post-id 7 --include-body --tag a --tag b --name 7x --gone null",
            &OutcomeCapabilities,
        );
        assert_eq!(typed.exit_code, ExitCode::SUCCESS, "{typed:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&typed.output).expect("the input is echoed as JSON"),
            json!({"postId": 7, "includeBody": true, "tag": ["a", "b"], "name": "7x", "gone": null})
        );
        let via_cap = dekopon_shell::run("cap posts.get --post-id 7", &OutcomeCapabilities);
        assert_eq!(via_cap.exit_code, ExitCode::SUCCESS, "{via_cap:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&via_cap.output).expect("cap echoes the same input"),
            json!({"postId": 7})
        );

        let structured = dekopon_shell::run(
            "jq 'fromjson | .n' <<'EOF'\n{\"n\": 3}\nEOF",
            &OutcomeCapabilities,
        );
        assert_eq!(structured.exit_code, ExitCode::SUCCESS, "{structured:?}");
        assert_eq!(structured.output.trim(), "3");

        let truncated = dekopon_shell::Interpreter::new(dekopon_shell::Limits {
            max_output_lines: 4,
            ..dekopon_shell::Limits::default()
        })
        .run(
            "for i in 1 2 3 4 5 6 7 8 9 10; do echo $i; done",
            &OutcomeCapabilities,
        );
        assert!(truncated.truncated, "{truncated:?}");
        assert!(
            truncated
                .output
                .contains("... Output truncated (10 total lines) ..."),
            "{truncated:?}"
        );
        assert!(truncated.output.starts_with("1\n"), "{truncated:?}");
        assert!(truncated.output.ends_with("10"), "{truncated:?}");
    }

    #[test]
    fn offers_exactly_one_scripting_tool() {
        let tool = script_tool(&[]);

        assert_eq!(tool.name, "bash");
        assert_eq!(tool.parameters["properties"]["script"]["type"], "string");
        assert_eq!(tool.parameters["required"], json!(["script"]));
        assert_eq!(tool.parameters["additionalProperties"], json!(false));
        // The description has to point at the interpreter's own self-disclosure, or a model has no
        // way to learn which capabilities this session can reach.
        assert!(tool.description.contains("cap --list"));
        assert!(tool.description.contains("cap --describe"));
        // A session with no provider command words reads exactly as it always did.
        assert!(
            !tool
                .description
                .contains("providers add these command words")
        );
        // ...and it must not invent a discovery command the interpreter does not implement. There
        // is no `help` builtin, so advertising one would spend a tool call on "command not found".
        assert!(tool.description.contains("There is no `help`"));
        assert_no_doubled_spaces(&tool.description);
    }

    #[test]
    fn agent_config_tool_promises_a_credential_free_effective_view() {
        let tool = agent_config_tool();

        assert_eq!(tool.name, AGENT_CONFIG_TOOL_NAME);
        assert_eq!(tool.parameters["properties"], json!({}));
        assert_eq!(tool.parameters["required"], json!([]));
        assert_eq!(tool.parameters["additionalProperties"], false);
        assert!(tool.description.contains("Markdown tables"));
        assert!(tool.description.contains("currently grants this sender"));
        assert!(
            tool.description
                .contains("credential values are intentionally omitted")
        );
    }

    #[test]
    fn agent_config_tool_returns_the_prompt_and_effective_grants_without_spending_authority() {
        let model = ScriptedModel::new([
            agent_config_call(json!({})),
            answer("Here is the configuration table."),
        ]);
        let runtime = RecordingRuntime::new(0);
        let config = agent_config();
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new(
                "what is your configuration?",
                limits(4, 32),
                "fixture-model",
            )
            .with_agent_config(&config),
            &mut history,
        )
        .expect("meta inspection succeeds");

        assert_eq!(outcome.answer, "Here is the configuration table.");
        assert_eq!(outcome.script_calls, 0);
        assert_eq!(outcome.capability_invocations, 0);
        assert!(runtime.scripts.lock().expect("script lock").is_empty());

        let observed = model.observed_tools.lock().expect("tool observations lock");
        assert_eq!(observed[0].len(), 2);
        assert_eq!(observed[0][0].name, SCRIPT_TOOL_NAME);
        assert_eq!(observed[0][1].name, AGENT_CONFIG_TOOL_NAME);
        drop(observed);

        let messages = model.tool_messages();
        assert_eq!(messages.len(), 1);
        let value: Value = serde_json::from_str(&messages[0]).expect("meta result is JSON");
        assert_eq!(value["agent"]["id"], "reviewer");
        assert_eq!(value["prompt"]["instructions"], "Be concise and skeptical.");
        assert_eq!(
            value["effectiveAuthorization"]["capabilities"][0]["id"],
            "gh.pull-request.read"
        );
        assert_eq!(value["security"]["credentialsIncluded"], false);
    }

    #[test]
    fn agent_config_can_be_inspected_repeatedly_within_a_turn() {
        let model = ScriptedModel::new([
            AssistantTurn {
                content: None,
                tool_calls: vec![
                    agent_config_tool_call("config-call-1", json!({})),
                    agent_config_tool_call("config-call-2", json!({})),
                ],
                usage: None,
                replay_items: Vec::new(),
            },
            answer("done"),
        ]);
        let runtime = RecordingRuntime::new(0);
        let config = agent_config();
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("inspect twice", limits(3, 32), "fixture-model")
                .with_agent_config(&config),
            &mut history,
        )
        .expect("repeated inspection succeeds");

        assert_eq!(outcome.script_calls, 0);
        assert_eq!(outcome.capability_invocations, 0);
        assert!(runtime.scripts.lock().expect("script lock").is_empty());

        // Repetition still succeeds — it is bounded by the loop's shared per-turn tool-call and
        // model-step limits and by nothing of its own.
        let messages = model.tool_messages();
        assert_eq!(messages.len(), 2);
        let first: Value =
            serde_json::from_str(&messages[0]).expect("the first configuration is JSON");
        assert_eq!(first["agent"]["id"], "reviewer");
        assert!(first.get("error").is_none());
        // What it does not do is append a second copy. Every tool result stays in the message
        // vector and is re-sent to the provider on every later turn, so a 128 KiB view repeated
        // ten times a turn is a session that pays for it twelve turns running.
        assert_eq!(messages[1], AGENT_CONFIG_ALREADY_SHOWN);
        assert!(messages[1].len() < messages[0].len() / 2);
    }

    #[test]
    fn agent_config_is_copied_once_per_session_across_turns() {
        let model = ScriptedModel::new([
            agent_config_call(json!({})),
            agent_config_call(json!({})),
            answer("done"),
        ]);
        let runtime = RecordingRuntime::new(0);
        let config = agent_config();
        let mut history = History::default();

        run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("inspect on two turns", limits(4, 32), "fixture-model")
                .with_agent_config(&config),
            &mut history,
        )
        .expect("repeated inspection succeeds");

        let messages = model
            .observed_messages
            .lock()
            .expect("message observations");
        let copies = messages
            .last()
            .expect("the model was asked at least once")
            .iter()
            .filter(|message| {
                message
                    .content()
                    .is_some_and(|content| content.contains("\"effectiveAuthorization\""))
            })
            .count();
        assert_eq!(
            copies, 1,
            "the final request carries one configuration copy"
        );
    }

    /// One conversation's attachments, fixed in advance and numbered from one.
    struct FixedAssets(Vec<FetchedAsset>);

    impl AssetSource for FixedAssets {
        fn fetch(&self, id: u64) -> Result<FetchedAsset, String> {
            usize::try_from(id)
                .ok()
                .filter(|index| *index >= 1)
                .and_then(|index| self.0.get(index - 1))
                .cloned()
                .ok_or_else(|| format!("Chat Asset #{id} is not part of this conversation."))
        }

        fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
    }

    fn text_asset(text: &str) -> FetchedAsset {
        FetchedAsset {
            name: "attachment.txt".to_owned(),
            mime: "text/plain".to_owned(),
            data: text.as_bytes().to_vec(),
        }
    }

    fn asset_call(id: u64) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: "asset-call".to_owned(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: ASSET_TOOL_NAME.to_owned(),
                    arguments: json!({ "id": id }).to_string(),
                },
            }],
            usage: None,
            replay_items: Vec::new(),
        }
    }

    #[test]
    fn a_textual_asset_within_the_bound_reaches_the_model_verbatim() {
        let model = ScriptedModel::new([asset_call(1), answer("It is a log line.")]);
        let runtime = RecordingRuntime::new(0);
        let assets = FixedAssets(vec![text_asset("2026-08-20 request failed\n")]);
        let mut history = History::default();

        run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("what is in the file?", limits(3, 32), "fixture-model")
                .with_assets(&assets),
            &mut history,
        )
        .expect("asset session succeeds");

        assert_eq!(
            model.tool_messages(),
            vec!["2026-08-20 request failed\n".to_owned()]
        );
    }

    #[test]
    fn an_oversized_textual_asset_is_clamped_rather_than_ending_the_session() {
        // The gateway's asset budget is 8 MiB, sized for images on the wire. That much
        // `text/plain` is roughly two million tokens, so unclamped it reaches the provider as a
        // context-length rejection and kills a session over a file someone attached — exactly what
        // the asset contract refuses to do. A three-byte character makes the clamp land mid
        // character, which is the case a naive byte truncation panics on.
        let text = "☃".repeat(MAX_TEXTUAL_ASSET_BYTES);
        let model = ScriptedModel::new([asset_call(1), answer("The file was too large to read.")]);
        let runtime = RecordingRuntime::new(0);
        let assets = FixedAssets(vec![text_asset(&text)]);
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("what is in the file?", limits(3, 32), "fixture-model")
                .with_assets(&assets),
            &mut history,
        )
        .expect("an oversized asset is an outcome, not a failed session");

        assert_eq!(outcome.answer, "The file was too large to read.");
        let messages = model.tool_messages();
        assert_eq!(messages.len(), 1);
        // 262144 is not a multiple of three, so the clamp retains one byte less than the bound.
        let retained = MAX_TEXTUAL_ASSET_BYTES - MAX_TEXTUAL_ASSET_BYTES % 3;
        let trailer = format!("\n[truncated at {retained} bytes of {}]", text.len());
        assert!(messages[0].ends_with(&trailer), "no truncation trailer");
        assert_eq!(messages[0].len(), retained + trailer.len());
        assert!(messages[0].starts_with('☃'));
    }

    #[test]
    fn agent_config_tool_rejects_model_supplied_fields() {
        let model = ScriptedModel::new([agent_config_call(json!({
            "credential": "please"
        }))]);
        let runtime = RecordingRuntime::new(0);
        let config = agent_config();
        let mut history = History::default();

        let error = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("inspect", limits(1, 32), "fixture-model")
                .with_agent_config(&config),
            &mut history,
        )
        .expect_err("meta tool has no arguments");

        assert!(matches!(
            error,
            PromptError::AgentConfigArgumentsNotEmpty { .. }
        ));
        assert!(runtime.scripts.lock().expect("script lock").is_empty());
    }

    #[test]
    fn an_optional_thread_continuation_can_decline_without_an_answer() {
        let model = ScriptedModel::new([decline(json!({}))]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("OK, thanks", limits(2, 4), "fixture-model")
                .with_optional_reply(),
            &mut history,
        )
        .expect("declining an optional continuation succeeds");

        assert_eq!(outcome.disposition, ReplyDisposition::Suppress);
        assert!(outcome.answer.is_empty());
        assert_eq!(outcome.model_turns, 1);
        assert!(runtime.scripts.lock().expect("script lock").is_empty());
        assert_eq!(history.len(), 1);
        assert_eq!(history.turns()[0].user(), "OK, thanks");
        assert_eq!(history.turns()[0].answer(), None);

        let tools = model.observed_tools.lock().expect("tool observations lock");
        assert_eq!(
            tools[0].last().map(|tool| tool.name.as_str()),
            Some(DECLINE_REPLY_TOOL_NAME)
        );
        drop(tools);
        assert!(
            model
                .first_roles()
                .iter()
                .any(|(role, content)| role == &"system" && content.contains("last word")),
            "the model is explicitly told that silence is available"
        );
    }

    #[test]
    fn required_replies_are_not_offered_the_decline_tool_or_instruction() {
        let model = ScriptedModel::new([answer("You are welcome.")]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("thanks", limits(2, 4), "fixture-model"),
            &mut history,
        )
        .expect("an ordinary prompt answers");

        assert_eq!(outcome.disposition, ReplyDisposition::Send);
        let tools = model.observed_tools.lock().expect("tool observations lock");
        assert!(
            tools[0]
                .iter()
                .all(|tool| tool.name != DECLINE_REPLY_TOOL_NAME)
        );
        assert!(
            model
                .first_roles()
                .iter()
                .all(|(_, content)| !content.contains("decline_chat_reply"))
        );
    }

    #[test]
    fn a_decline_requested_alongside_work_runs_nothing() {
        let model = ScriptedModel::new([AssistantTurn {
            content: None,
            tool_calls: vec![
                ModelToolCall {
                    id: "image-call".to_owned(),
                    kind: "function".to_owned(),
                    function: ModelFunctionCall {
                        name: IMAGE_GENERATION_TOOL_NAME.to_owned(),
                        arguments: json!({"prompt": "should not be generated"}).to_string(),
                    },
                },
                decline_call("decline-call", json!({})),
                ModelToolCall {
                    id: "script-call".to_owned(),
                    kind: "function".to_owned(),
                    function: ModelFunctionCall {
                        name: SCRIPT_TOOL_NAME.to_owned(),
                        arguments: json!({"script": "echo should-not-run"}).to_string(),
                    },
                },
            ],
            usage: None,
            replay_items: Vec::new(),
        }]);
        let runtime = RecordingRuntime::new(1);
        let generator = FixedImageGenerator {
            calls: AtomicUsize::new(0),
        };
        let image = GeneratedImageOutput::default();
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("conversation moved on", limits(2, 4), "fixture-model")
                .with_image_generation(&generator, &image)
                .with_optional_reply(),
            &mut history,
        )
        .expect("the no-reply decision is terminal");

        assert_eq!(outcome.disposition, ReplyDisposition::Suppress);
        assert!(runtime.scripts.lock().expect("script lock").is_empty());
        assert_eq!(generator.calls.load(Ordering::SeqCst), 0);
        assert!(image.take().is_none());
        assert_eq!(outcome.capability_invocations, 0);
        let tools = model.observed_tools.lock().expect("tool observations lock");
        assert!(
            tools[0]
                .iter()
                .any(|tool| tool.name == IMAGE_GENERATION_TOOL_NAME)
        );
        assert!(
            tools[0]
                .iter()
                .any(|tool| tool.name == DECLINE_REPLY_TOOL_NAME)
        );
    }

    #[test]
    fn capability_work_requires_a_reply_even_if_the_model_later_declines() {
        let model = ScriptedModel::new([
            script_call("script-call", "echo did-work"),
            decline(json!({})),
            answer("I completed the capability call."),
        ]);
        let runtime = RecordingRuntime::new(1);
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("maybe do this", limits(4, 4), "fixture-model")
                .with_optional_reply(),
            &mut history,
        )
        .expect("the model reports work instead of hiding it");

        assert_eq!(outcome.disposition, ReplyDisposition::Send);
        assert_eq!(outcome.answer, "I completed the capability call.");
        assert_eq!(outcome.capability_invocations, 1);
        assert!(
            model
                .tool_messages()
                .iter()
                .any(|message| message.contains("a concise reply describing what happened"))
        );
    }

    #[test]
    fn a_final_turn_decline_after_capability_work_is_a_distinct_unsafe_retry_warning() {
        let model = ScriptedModel::new([
            script_call("script-call", "echo did-work"),
            decline(json!({})),
        ]);
        let runtime = RecordingRuntime::new(1);
        let mut history = History::default();

        let error = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("maybe do this", limits(2, 4), "fixture-model")
                .with_optional_reply(),
            &mut history,
        )
        .expect_err("work cannot disappear behind a final-turn decline");

        assert!(matches!(error, PromptError::UnreportedCapabilityWork));
        assert_eq!(history.turns().last().and_then(JobRecord::answer), None);
    }

    #[test]
    fn the_decline_tool_rejects_model_supplied_fields() {
        let model = ScriptedModel::new([decline(json!({"message": "secret"}))]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        let error = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("optional", limits(1, 4), "fixture-model").with_optional_reply(),
            &mut history,
        )
        .expect_err("the decline tool has no model-controlled payload");

        assert!(matches!(
            error,
            PromptError::DeclineReplyArgumentsNotEmpty { .. }
        ));
    }

    #[test]
    fn runs_a_model_script_and_returns_the_final_answer() {
        let model = ScriptedModel::new([
            script_call("call-1", "echo.echo --message hi | jq -r .message"),
            answer("The capability echoed hi."),
        ]);
        let runtime = RecordingRuntime::new(1);

        let outcome = run_prompt(&model, &runtime, "say hi", None, limits(4, 32))
            .expect("prompt session succeeds");

        assert_eq!(outcome.answer, "The capability echoed hi.");
        assert_eq!(outcome.model_turns, 2);
        assert_eq!(outcome.script_calls, 1);
        assert_eq!(outcome.capability_invocations, 1);
        let scripts = runtime.scripts.lock().expect("script lock");
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].0, "echo.echo --message hi | jq -r .message");
    }

    #[test]
    fn exposes_one_tool_per_request_regardless_of_capability_count() {
        let model = ScriptedModel::new([answer("done")]);
        let runtime = RecordingRuntime::new(0);

        run_prompt(&model, &runtime, "do nothing", None, limits(2, 32)).expect("prompt succeeds");

        let observed = model.observed_tools.lock().expect("tool observations lock");
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].len(), 1);
        assert_eq!(observed[0][0].name, SCRIPT_TOOL_NAME);
    }

    #[test]
    fn returns_script_output_and_exit_code_to_the_model() {
        let model = ScriptedModel::new([script_call("call-1", "echo hi"), answer("done")]);
        let runtime = RecordingRuntime::new(0);

        run_prompt(&model, &runtime, "run something", None, limits(4, 32))
            .expect("prompt session succeeds");

        let messages = model.tool_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0], "ran 7 bytes\n[exit code: 0]");
    }

    #[test]
    fn spends_one_capability_budget_across_every_script_in_the_session() {
        // The interpreter's own ceiling bounds one script. Without this, a model widens its budget
        // simply by writing more scripts, and `max_steps` multiplies rather than bounds the work.
        let model = ScriptedModel::new([
            script_call("call-1", "one"),
            script_call("call-2", "two"),
            script_call("call-3", "three"),
            answer("done"),
        ]);
        let runtime = RecordingRuntime::new(4);

        let outcome = run_prompt(&model, &runtime, "spend it", None, limits(8, 10))
            .expect("prompt session succeeds");

        let scripts = runtime.scripts.lock().expect("script lock");
        let ceilings = scripts
            .iter()
            .map(|(_, ceiling)| *ceiling)
            .collect::<Vec<_>>();
        assert_eq!(ceilings, vec![10, 6, 2]);
        assert_eq!(outcome.capability_invocations, 10);
    }

    #[test]
    fn exhausted_capability_budget_leaves_later_scripts_with_nothing_to_spend() {
        let model = ScriptedModel::new([
            script_call("call-1", "one"),
            script_call("call-2", "two"),
            answer("done"),
        ]);
        let runtime = RecordingRuntime::new(8);

        let outcome = run_prompt(&model, &runtime, "spend it", None, limits(8, 3))
            .expect("prompt session succeeds");

        let scripts = runtime.scripts.lock().expect("script lock");
        assert_eq!(scripts[1].1, 0);
        assert_eq!(outcome.capability_invocations, 3);
    }

    #[test]
    fn rejects_model_selected_tools_that_were_not_offered() {
        let model = ScriptedModel::new([AssistantTurn {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: "call-1".to_owned(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: "echo_echo".to_owned(),
                    arguments: "{}".to_owned(),
                },
            }],
            usage: None,
            replay_items: Vec::new(),
        }]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "call the old tool", None, limits(1, 32))
            .expect_err("unknown tools must fail closed");

        assert!(matches!(error, PromptError::UnknownTool(_)));
        assert!(runtime.scripts.lock().expect("script lock").is_empty());
    }

    #[test]
    fn rejects_tool_calls_without_a_string_script_argument() {
        for arguments in [r#"{"command":"echo hi"}"#, r#"{"script":42}"#, "{}"] {
            let model = ScriptedModel::new([AssistantTurn {
                content: None,
                tool_calls: vec![ModelToolCall {
                    id: "call-1".to_owned(),
                    kind: "function".to_owned(),
                    function: ModelFunctionCall {
                        name: SCRIPT_TOOL_NAME.to_owned(),
                        arguments: arguments.to_owned(),
                    },
                }],
                usage: None,
                replay_items: Vec::new(),
            }]);
            let runtime = RecordingRuntime::new(0);

            let error = run_prompt(&model, &runtime, "malformed", None, limits(1, 32))
                .expect_err("a missing script must fail closed");

            assert!(
                matches!(error, PromptError::MissingScript { .. }),
                "{arguments}: {error}"
            );
            assert!(runtime.scripts.lock().expect("script lock").is_empty());
        }
    }

    #[test]
    fn accepts_ten_tool_calls_in_one_model_turn() {
        assert_eq!(MAX_TOOL_CALLS_PER_TURN, 10);
        let tool_calls = (0..MAX_TOOL_CALLS_PER_TURN)
            .map(|index| ModelToolCall {
                id: format!("call-{index}"),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: json!({ "script": "echo hi" }).to_string(),
                },
            })
            .collect();
        let model = ScriptedModel::new([
            AssistantTurn {
                content: None,
                tool_calls,
                usage: None,
                replay_items: Vec::new(),
            },
            answer("done"),
        ]);
        let runtime = RecordingRuntime::new(0);

        let outcome = run_prompt(&model, &runtime, "fan out", None, limits(2, 32))
            .expect("ten calls remain inside the per-turn bound");

        assert_eq!(
            outcome.script_calls,
            u32::try_from(MAX_TOOL_CALLS_PER_TURN).expect("tool-call bound fits u32")
        );
        assert_eq!(
            runtime.scripts.lock().expect("script lock").len(),
            MAX_TOOL_CALLS_PER_TURN
        );
    }

    #[test]
    fn rejects_eleven_tool_calls_in_one_model_turn() {
        let tool_calls = (0..=MAX_TOOL_CALLS_PER_TURN)
            .map(|index| ModelToolCall {
                id: format!("call-{index}"),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: json!({ "script": "echo hi" }).to_string(),
                },
            })
            .collect();
        let model = ScriptedModel::new([AssistantTurn {
            content: None,
            tool_calls,
            usage: None,
            replay_items: Vec::new(),
        }]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "fan out", None, limits(1, 32))
            .expect_err("eleven calls must exceed the per-turn bound");

        assert!(matches!(
            error,
            PromptError::TooManyToolCalls {
                actual: 11,
                maximum: 10
            }
        ));
        assert!(runtime.scripts.lock().expect("script lock").is_empty());
    }

    #[test]
    fn formats_an_empty_script_outcome_without_a_leading_blank_line() {
        let outcome = ScriptOutcome {
            output: String::new(),
            exit_code: ExitCode::NOT_FOUND,
            truncated: false,
            capability_calls: 0,
            steps: 1,
        };

        assert_eq!(format_script_outcome(&outcome), "[exit code: 127]");
    }

    /// A runtime whose capability dispatch is genuinely asynchronous underneath.
    ///
    /// This is the shape embedding binaries use in production: a synchronous [`ScriptRuntime`]
    /// bridging to an async broker round trip with `Handle::block_on`, which is correct only
    /// because the whole loop runs on a blocking task rather than a runtime worker thread.
    struct BlockingBridgeRuntime {
        handle: tokio::runtime::Handle,
        dispatched: Arc<Mutex<Vec<String>>>,
    }

    impl ScriptRuntime for BlockingBridgeRuntime {
        fn capability_snapshot(&self) -> Result<CapabilitySnapshot, BootstrapError> {
            Ok(CapabilitySnapshot::empty())
        }
        fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome {
            let dispatched = Arc::clone(&self.dispatched);
            let script = script.to_owned();
            let output = self.handle.block_on(async move {
                tokio::task::yield_now().await;
                dispatched
                    .lock()
                    .expect("dispatch lock")
                    .push(script.clone());
                format!("async runtime saw: {script}")
            });
            ScriptOutcome {
                output,
                exit_code: ExitCode::SUCCESS,
                truncated: false,
                capability_calls: 1.min(max_capability_calls),
                steps: 1,
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drives_the_loop_from_a_blocking_task_over_an_async_dispatch() {
        let dispatched = Arc::new(Mutex::new(Vec::new()));
        let handle = tokio::runtime::Handle::current();
        let recorded = Arc::clone(&dispatched);

        let outcome = tokio::task::spawn_blocking(move || {
            let model = ScriptedModel::new([
                script_call("call-1", "http.get --url https://example.test"),
                answer("fetched"),
            ]);
            let runtime = BlockingBridgeRuntime {
                handle,
                dispatched: recorded,
            };
            run_prompt(&model, &runtime, "fetch it", None, limits(4, 32))
        })
        .await
        .expect("blocking prompt task completes")
        .expect("prompt session succeeds");

        assert_eq!(outcome.answer, "fetched");
        assert_eq!(outcome.script_calls, 1);
        assert_eq!(outcome.capability_invocations, 1);
        assert_eq!(
            *dispatched.lock().expect("dispatch lock"),
            vec!["http.get --url https://example.test".to_owned()]
        );
    }

    // The companion assertion — that the interpreter's own spans nest under `prompt.script` across
    // this same bridge — lives in `dekopon-run/tests/prompt_tracing.rs` rather than here.
    // `tracing` caches callsite interest globally and once, so a `prompt.script` first reached by
    // one of the tests above (which install no subscriber) stays disabled for any later
    // thread-local one; that made the assertion depend on test ordering. Its own binary removes
    // the race.

    #[test]
    fn rejects_a_zero_step_session_before_contacting_the_model() {
        let model = ScriptedModel::new([]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "nothing", None, limits(0, 32))
            .expect_err("a zero-step session is a usage error");

        assert!(matches!(error, PromptError::ZeroSteps));
        assert!(
            model
                .observed_tools
                .lock()
                .expect("tool observations lock")
                .is_empty()
        );
    }

    #[test]
    fn stops_when_the_model_never_produces_an_answer() {
        let model = ScriptedModel::new([
            script_call("call-1", "echo one"),
            script_call("call-2", "echo two"),
        ]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "loop forever", None, limits(2, 32))
            .expect_err("an answerless session must terminate");

        assert!(matches!(error, PromptError::MaxSteps { maximum: 2 }));
    }

    #[test]
    fn tool_call_ids_must_correlate() {
        let model = ScriptedModel::new([AssistantTurn {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: "  ".to_owned(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: json!({ "script": "echo hi" }).to_string(),
                },
            }],
            usage: None,
            replay_items: Vec::new(),
        }]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "correlate", None, limits(1, 32))
            .expect_err("an uncorrelated tool call must fail closed");

        assert!(matches!(error, PromptError::EmptyToolCallId));
    }

    #[test]
    fn rejects_arguments_that_are_not_a_json_object() {
        let model = ScriptedModel::new([AssistantTurn {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: "call-1".to_owned(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: Value::String("echo hi".to_owned()).to_string(),
                },
            }],
            usage: None,
            replay_items: Vec::new(),
        }]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "malformed", None, limits(1, 32))
            .expect_err("non-object arguments must fail closed");

        assert!(matches!(error, PromptError::ArgumentsNotObject { .. }));
    }

    // -----------------------------------------------------------------------
    // Skills
    // -----------------------------------------------------------------------

    /// One loaded skill with one resource file, held with the directory it was read from.
    fn mounted_skill() -> (tempfile::TempDir, dekopon_config::Skill) {
        let root = tempfile::tempdir().expect("temporary directory");
        let directory = root.path().join("pull-request-review");
        std::fs::create_dir_all(directory.join("references")).expect("skill directory");
        std::fs::write(
            directory.join("SKILL.md"),
            "---\nname: pull-request-review\ndescription: Use when reviewing a pull request.\n---\nRead the diff before commenting.\n",
        )
        .expect("skill file");
        std::fs::write(
            directory.join("references/checklist.md"),
            "- every write has a capability\n",
        )
        .expect("resource");
        let skill = dekopon_config::load_skill(&directory).expect("fixture loads");
        (root, skill)
    }

    /// The tool results the model saw on its *last* request, in order.
    ///
    /// `tool_messages` flattens every request, so a result the loop appended on turn one is
    /// observed again on every later request; the last request carries each exactly once.
    fn last_tool_results(model: &ScriptedModel) -> Vec<String> {
        model
            .observed_messages
            .lock()
            .expect("message observations lock")
            .last()
            .expect("the model was asked at least once")
            .iter()
            .filter(|message| message.role() == "tool")
            .filter_map(|message| message.content().map(str::to_owned))
            .collect()
    }

    fn tool_call(id: &str, name: &str, arguments: Value) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: id.to_owned(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: name.to_owned(),
                    arguments: arguments.to_string(),
                },
            }],
            usage: None,
            replay_items: Vec::new(),
        }
    }

    #[test]
    fn mounted_skills_are_listed_by_summary_and_read_on_demand() {
        let (_root, skill) = mounted_skill();
        let skills = vec![skill];
        let model = ScriptedModel::new([
            tool_call(
                "read-1",
                SKILL_TOOL_NAME,
                json!({"name": "pull-request-review"}),
            ),
            tool_call(
                "read-2",
                SKILL_TOOL_NAME,
                json!({"name": "pull-request-review", "resource": "references/checklist.md"}),
            ),
            // A repeat costs a pointer rather than a second copy.
            tool_call(
                "read-3",
                SKILL_TOOL_NAME,
                json!({"name": "pull-request-review"}),
            ),
            answer("Reviewed."),
        ]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("review PR 7", limits(5, 2), "fixture-model")
                .with_system(Some("Be concise."))
                .with_skills(&skills),
            &mut history,
        )
        .expect("skill reads are recoverable model turns");

        assert_eq!(outcome.answer, "Reviewed.");
        // Instructions first, then the standing listing, then the prompt.
        let roles = model.first_roles();
        assert_eq!(roles[0], ("system", "Be concise.".to_owned()));
        assert_eq!(roles[1].0, "system");
        assert!(
            roles[1]
                .1
                .contains("- pull-request-review: Use when reviewing a pull request."),
            "{}",
            roles[1].1
        );
        assert!(
            !roles[1].1.contains("Read the diff before commenting"),
            "the body must not ride the listing: {}",
            roles[1].1
        );
        assert_eq!(roles[2], ("user", "review PR 7".to_owned()));
        let tools = model.observed_tools.lock().expect("tool observations lock");
        assert!(
            tools[0].iter().any(|tool| tool.name == SKILL_TOOL_NAME),
            "the read tool is offered when a skill is mounted"
        );
        drop(tools);

        let results = last_tool_results(&model);
        assert!(
            results[0].starts_with("# Skill: pull-request-review"),
            "{}",
            results[0]
        );
        assert!(
            results[0].contains("Read the diff before commenting."),
            "{}",
            results[0]
        );
        assert!(
            results[0].contains("references/checklist.md"),
            "{}",
            results[0]
        );
        assert_eq!(
            results[1],
            "# pull-request-review/references/checklist.md\n\n- every write has a capability\n"
        );
        assert!(
            results[2].contains("already in this conversation"),
            "{}",
            results[2]
        );
        assert!(
            runtime.scripts.lock().expect("script lock").is_empty(),
            "reading a skill runs no script and spends no capability budget"
        );
    }

    #[test]
    fn an_unknown_skill_or_resource_is_a_refusal_the_model_reads() {
        let (_root, skill) = mounted_skill();
        let skills = vec![skill];
        let model = ScriptedModel::new([
            tool_call("read-1", SKILL_TOOL_NAME, json!({"name": "release-notes"})),
            tool_call(
                "read-2",
                SKILL_TOOL_NAME,
                json!({"name": "pull-request-review", "resource": "scripts/none.sh"}),
            ),
            answer("Working without it."),
        ]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("review", limits(4, 2), "fixture-model").with_skills(&skills),
            &mut history,
        )
        .expect("a wrong name is recoverable");

        assert_eq!(outcome.answer, "Working without it.");
        let results = last_tool_results(&model);
        assert!(
            results[0].contains("Mounted skills: pull-request-review."),
            "{}",
            results[0]
        );
        assert!(
            results[1].contains("has no resource by that path"),
            "{}",
            results[1]
        );
        assert!(
            results[1].contains("references/checklist.md"),
            "{}",
            results[1]
        );
    }

    #[test]
    fn a_session_without_skills_offers_no_listing_and_no_tool() {
        let model = ScriptedModel::new([tool_call(
            "read-1",
            SKILL_TOOL_NAME,
            json!({"name": "pull-request-review"}),
        )]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        let error = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("review", limits(2, 2), "fixture-model")
                .with_system(Some("Be concise.")),
            &mut history,
        )
        .expect_err("a tool that was never offered is unknown");

        assert!(matches!(error, PromptError::UnknownTool(name) if name == SKILL_TOOL_NAME));
        let roles = model.first_roles();
        assert_eq!(roles.len(), 2, "no listing was added: {roles:?}");
        let tools = model.observed_tools.lock().expect("tool observations lock");
        assert!(tools[0].iter().all(|tool| tool.name != SKILL_TOOL_NAME));
    }

    #[test]
    fn malformed_skill_arguments_end_the_session_like_every_other_tool() {
        let (_root, skill) = mounted_skill();
        let skills = vec![skill];
        let model = ScriptedModel::new([tool_call(
            "read-1",
            SKILL_TOOL_NAME,
            json!({"name": "pull-request-review", "page": 2}),
        )]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        let error = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("review", limits(2, 2), "fixture-model").with_skills(&skills),
            &mut history,
        )
        .expect_err("an unexpected field is malformed model output");

        assert!(matches!(
            error,
            PromptError::UnexpectedSkillArguments { .. }
        ));
    }

    // -----------------------------------------------------------------------
    // Improvement suggestions
    // -----------------------------------------------------------------------

    fn suggestion(id: &str, target: &str) -> AssistantTurn {
        tool_call(
            id,
            IMPROVEMENT_TOOL_NAME,
            json!({
                "category": "capability",
                "target": target,
                "summary": "The capability was never granted.",
                "evidence": "exit code 127 on every attempt",
                "proposal": "Grant it to this agent.",
                "confidence": "high"
            }),
        )
    }

    #[test]
    fn suggestions_are_recorded_bounded_and_returned_with_the_outcome() {
        let model = ScriptedModel::new([
            suggestion("s-1", "gh.pull-request.read"),
            suggestion("s-2", "gh.pull-request.comment"),
            suggestion("s-3", "gh.issue.read"),
            // One past the bound: refused in a sentence, never an error.
            suggestion("s-4", "gh.issue.comment"),
            answer("Done."),
        ]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("do the thing", limits(6, 2), "fixture-model")
                .with_improvement_suggestions(),
            &mut history,
        )
        .expect("suggestions never fail a session");

        assert_eq!(outcome.answer, "Done.");
        assert_eq!(outcome.suggestions.len(), 3);
        assert_eq!(outcome.suggestions[0].target, "gh.pull-request.read");
        assert_eq!(
            outcome.suggestions[2].category,
            crate::improvement::ImprovementCategory::Capability
        );
        let results = last_tool_results(&model);
        assert!(
            results[0].contains("Recorded suggestion 1 of 3"),
            "{}",
            results[0]
        );
        assert!(
            results[2].contains("Recorded suggestion 3 of 3"),
            "{}",
            results[2]
        );
        assert!(results[3].contains("already recorded"), "{}", results[3]);
        let tools = model.observed_tools.lock().expect("tool observations lock");
        assert!(
            tools[0]
                .iter()
                .any(|tool| tool.name == IMPROVEMENT_TOOL_NAME)
        );
    }

    #[test]
    fn a_badly_formed_suggestion_is_refused_without_ending_the_session() {
        let model = ScriptedModel::new([
            tool_call(
                "s-1",
                IMPROVEMENT_TOOL_NAME,
                json!({
                    "category": "vibes",
                    "target": "x",
                    "summary": "s",
                    "evidence": "e",
                    "proposal": "p",
                    "confidence": "high"
                }),
            ),
            answer("Carrying on."),
        ]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionBootstrap::new("do the thing", limits(3, 2), "fixture-model")
                .with_improvement_suggestions(),
            &mut history,
        )
        .expect("a refused suggestion is a tool result");

        assert_eq!(outcome.answer, "Carrying on.");
        assert!(outcome.suggestions.is_empty());
        let results = last_tool_results(&model);
        assert!(
            results[0].contains("Suggestion not recorded"),
            "{}",
            results[0]
        );
        assert!(results[0].contains("`category`"), "{}", results[0]);
    }

    #[test]
    fn the_suggestion_tool_is_absent_unless_the_embedder_offers_it() {
        let model = ScriptedModel::new([suggestion("s-1", "gh.pull-request.read")]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "do the thing", None, limits(2, 2))
            .expect_err("a tool that was never offered is unknown");

        assert!(matches!(error, PromptError::UnknownTool(name) if name == IMPROVEMENT_TOOL_NAME));
        let tools = model.observed_tools.lock().expect("tool observations lock");
        assert!(
            tools[0]
                .iter()
                .all(|tool| tool.name != IMPROVEMENT_TOOL_NAME)
        );
        assert!(
            tools[0].iter().all(|tool| tool.name != SKILL_TOOL_NAME),
            "no skill tool either: the default session is exactly the pre-skills session"
        );
    }

    /// A ledger refusal that reached the model client is reported as `accounting`, never `model`.
    ///
    /// `model` tells an operator the endpoint misbehaved and that a retry is reasonable; a fenced
    /// job is permanent and this process caused it. The distinction lives in one arm above a
    /// catch-all, so nothing but this assertion keeps it there.
    #[test]
    fn a_fenced_ledger_is_an_accounting_failure_and_not_a_model_one() {
        let fenced = PromptError::from(ModelError::Accounting(
            dekopon_model::usage::AccountingError("job fenced"),
        ));
        assert_eq!(fenced.telemetry_kind(), "accounting", "{fenced}");
        assert!(
            fenced.to_string().contains("job fenced"),
            "the cause travels with it: {fenced}"
        );
        let transport = PromptError::from(ModelError::Request("connection reset".to_owned()));
        assert_eq!(transport.telemetry_kind(), "model");
    }
}
